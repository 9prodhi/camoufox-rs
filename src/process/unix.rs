//! Unix process spawning with fd 3/4 pipe transport.
//!
//! Creates two anonymous pipe pairs and uses `pre_exec` to `dup2` the child
//! ends onto the hardcoded file descriptors 3 and 4 that Juggler expects:
//!
//! - **Pipe A** (commands): parent writes `command_pipe` -> child reads fd 3
//! - **Pipe B** (responses): child writes fd 4 -> parent reads `response_pipe`
//!
//! # Safety
//!
//! The `pre_exec` closure runs between `fork()` and `exec()` in the child
//! process. Only async-signal-safe functions are permitted. We use `dup2`,
//! `close`, and `fcntl`, all of which are async-signal-safe per POSIX.

use crate::config::LaunchConfig;
use crate::process::{LaunchedProcess, ProcessError};

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

/// File descriptor the child reads commands from (parent -> child).
const CHILD_READ_FD: libc::c_int = 3;

/// File descriptor the child writes responses/events to (child -> parent).
const CHILD_WRITE_FD: libc::c_int = 4;

/// Create an anonymous pipe pair using `pipe2(O_CLOEXEC)`.
///
/// Returns `(read_fd, write_fd)`. Both fds have `O_CLOEXEC` set so they are
/// automatically closed on `exec()` unless explicitly `dup2`'d to a target fd
/// (which clears `O_CLOEXEC` on the target).
fn create_pipe() -> Result<(OwnedFd, OwnedFd), ProcessError> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe2` is a standard POSIX function. It writes two valid file
    // descriptors into the provided array on success. The array is stack-
    // allocated and correctly sized.
    #[cfg(not(target_vendor = "apple"))]
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(target_vendor = "apple")]
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(ProcessError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: pipe succeeded; each descriptor has exactly one owner.
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(target_vendor = "apple")]
    for fd in [&pair.0, &pair.1] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(ProcessError::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(pair)
}

/// Spawn a Camoufox process with the fd 3/4 pipe transport configured.
///
/// This function:
/// 1. Creates two pipe pairs (command pipe A, response pipe B).
/// 2. Uses `CommandExt::pre_exec` to `dup2` the child ends onto fd 3 and fd 4.
/// 3. Spawns the browser with the argument list from `config.build_args()`.
/// 4. Closes the child-side fds in the parent process.
/// 5. Returns the child process handle and the parent pipe endpoints.
///
/// The returned [`LaunchedProcess`] owns the parent-side pipe fds. Dropping
/// `command_pipe` closes the write end, which causes the child's reader thread
/// to see EOF and trigger a graceful shutdown (per PROTOCOL.md section 13).
///
/// # Errors
///
/// - `ProcessError::Io` if pipe creation fails.
/// - `ProcessError::SpawnFailed` if `Command::spawn` fails.
pub fn spawn(config: &LaunchConfig) -> Result<LaunchedProcess, ProcessError> {
    config.validate().map_err(std::io::Error::other)?;
    #[cfg(target_os = "linux")]
    {
        // PDEATHSIG tracks the spawning THREAD, not just the process. Daemon
        // connection threads are short-lived, so fork from one persistent thread.
        use std::sync::{mpsc, OnceLock};
        type Request = (
            LaunchConfig,
            mpsc::Sender<Result<LaunchedProcess, ProcessError>>,
        );
        static SPAWNER: OnceLock<Result<mpsc::Sender<Request>, String>> = OnceLock::new();
        let spawner = SPAWNER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Request>();
            std::thread::Builder::new()
                .name("camoufox-process-spawner".into())
                .spawn(move || {
                    for (config, reply) in rx {
                        if let Err(mpsc::SendError(Ok(mut launched))) =
                            reply.send(spawn_inner(&config))
                        {
                            let _ = super::lifecycle::force_kill(&mut launched.child);
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(tx)
        });
        let spawner = spawner
            .as_ref()
            .map_err(|e| ProcessError::Io(std::io::Error::other(e.clone())))?;
        let (tx, rx) = mpsc::channel();
        spawner
            .send((config.clone(), tx))
            .map_err(|e| ProcessError::Io(std::io::Error::other(e.to_string())))?;
        rx.recv()
            .map_err(|e| ProcessError::Io(std::io::Error::other(e.to_string())))?
    }
    #[cfg(not(target_os = "linux"))]
    spawn_inner(config)
}

fn spawn_inner(config: &LaunchConfig) -> Result<LaunchedProcess, ProcessError> {
    // --- Create pipe pairs ---
    //
    // Pipe A (commands):  parent writes to pipe_a_write,
    //                     child reads from pipe_a_read (dup2'd to fd 3)
    // Pipe B (responses): child writes to pipe_b_write (dup2'd to fd 4),
    //                     parent reads from pipe_b_read
    let (pipe_a_read, pipe_a_write) = create_pipe()?;
    let (pipe_b_read, pipe_b_write) = create_pipe()?;

    // Protect child endpoints from Command's stdio remapping, which occurs
    // BEFORE pre_exec and can overwrite sources at 0, 1 or 2.
    let protect_stdio = |fd: OwnedFd| -> Result<OwnedFd, ProcessError> {
        if fd.as_raw_fd() > 2 {
            return Ok(fd);
        }
        let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
        if moved == -1 {
            return Err(ProcessError::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: fcntl returned a fresh descriptor; fd drops the original.
        Ok(unsafe { OwnedFd::from_raw_fd(moved) })
    };
    let pipe_a_read = protect_stdio(pipe_a_read)?;
    let pipe_b_write = protect_stdio(pipe_b_write)?;

    // All endpoints remain owned across every fallible operation.
    let command_pipe = std::fs::File::from(pipe_a_write);
    let response_pipe = std::fs::File::from(pipe_b_read);

    // --- Build the command ---
    let args = config.build_args();
    let mut cmd = Command::new(&config.executable);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Set environment variables from config.
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    // --- pre_exec: dup2 child ends onto fd 3 and fd 4 ---
    //
    // SAFETY: This closure runs between fork() and exec() in the child
    // process address space. We only call dup2(), close(), and fcntl(),
    // all of which are async-signal-safe per POSIX.
    //
    // After dup2:
    // - fd 3 is a copy of pipe_a_read (child reads commands)
    // - fd 4 is a copy of pipe_b_write (child writes responses)
    //
    // The original pipe_a_read and pipe_b_write have O_CLOEXEC, so they are
    // automatically closed when exec() succeeds. We close them explicitly
    // here if they differ from the target fd (for cleanliness).
    //
    // The parent-side fds (pipe_a_write, pipe_b_read) also have O_CLOEXEC
    // and will be closed in the child on exec().
    let child_read = pipe_a_read.as_raw_fd();
    let child_write = pipe_b_write.as_raw_fd();
    #[cfg(target_os = "linux")]
    let parent_pid = unsafe { libc::getpid() };

    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close the race where the parent died before prctl.
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGKILL);
                }
            }
            // The daemon blocks termination signals for sigwait. Do not pass
            // that mask to the browser.
            let mut mask = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Move BOTH sources above the targets before overwriting either.
            let read = libc::fcntl(child_read, libc::F_DUPFD_CLOEXEC, 5);
            if read == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let write = libc::fcntl(child_write, libc::F_DUPFD_CLOEXEC, 5);
            if write == -1 {
                libc::close(read);
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(read, CHILD_READ_FD) == -1 || libc::dup2(write, CHILD_WRITE_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 clears CLOEXEC because both sources are above 4.
            libc::close(read);
            libc::close(write);

            Ok(())
        });
    }

    // Drop the child-side endpoints on success AND failed exec/pre_exec.
    let result = cmd.spawn();
    drop(pipe_a_read);
    drop(pipe_b_write);
    let child = result.map_err(ProcessError::SpawnFailed)?;

    Ok(LaunchedProcess {
        child,
        command_pipe,
        response_pipe,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    fn script(source: &str) -> (tempfile::TempDir, LaunchConfig) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("browser.sh");
        crate::test_script::write_script(&path, &format!("#!/bin/sh\n{source}\n"), 0o700);
        let config = LaunchConfig {
            executable: path,
            profile_dir: Some(dir.path().to_owned()),
            ..Default::default()
        };
        (dir, config)
    }

    #[test]
    fn create_pipe_returns_valid_fds() {
        let (r, w) = create_pipe().expect("pipe");
        assert_ne!(r.as_raw_fd(), w.as_raw_fd());
        let mut r = std::fs::File::from(r);
        let mut w = std::fs::File::from(w);
        w.write_all(b"hello pipe").expect("write");
        let mut buf = [0; 10];
        r.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hello pipe");
    }

    #[test]
    fn pipe_fds_have_cloexec() {
        let (r, w) = create_pipe().expect("pipe");
        for fd in [r, w] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn spawn_with_sh_creates_pipes() {
        let (_dir, config) = script("exit 0");
        let mut launched = spawn(&config).expect("spawn");
        assert!(launched.child.wait().expect("wait").success());
        assert_eq!(
            launched.command_pipe.write(b"test").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(launched.response_pipe.read(&mut [0; 1]).expect("read"), 0);
    }

    #[test]
    fn spawn_passes_env_vars() {
        let (_dir, mut config) = script("printf '%s' \"$CAMOUFOX_TEST_VAR\" >&4");
        config
            .env
            .insert("CAMOUFOX_TEST_VAR".into(), "test_value".into());
        let mut launched = spawn(&config).expect("spawn");
        let mut output = String::new();
        launched
            .response_pipe
            .read_to_string(&mut output)
            .expect("read");
        assert!(launched.child.wait().expect("wait").success());
        assert_eq!(output, "test_value");
    }

    #[test]
    fn spawn_fd_pipes_are_connected() {
        let (_dir, config) = script("read line <&3; echo \"$line\" >&4");
        let mut launched = spawn(&config).expect("spawn");
        launched
            .command_pipe
            .write_all(b"hello from parent\n")
            .expect("write");
        let mut line = String::new();
        BufReader::new(&mut launched.response_pipe)
            .read_line(&mut line)
            .expect("read");
        assert_eq!(line, "hello from parent\n");
        assert!(launched.child.wait().expect("wait").success());
    }

    #[test]
    fn closed_stdio_does_not_clobber_juggler_fds() {
        const MARKER: &str = "CAMOUFOX_CLOSED_STDIO_TEST_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let status = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "process::unix::tests::closed_stdio_does_not_clobber_juggler_fds",
                    "--test-threads=1",
                ])
                .env(MARKER, "1")
                .status()
                .expect("isolated test");
            assert!(status.success());
            return;
        }
        let (_dir, config) = script("read line <&3; echo \"$line\" >&4");
        unsafe {
            libc::close(0);
            libc::close(1);
            libc::close(2);
        }
        let mut launched = spawn(&config).expect("spawn with closed stdio");
        launched
            .command_pipe
            .write_all(b"closed stdio\n")
            .expect("write");
        let mut line = String::new();
        BufReader::new(&mut launched.response_pipe)
            .read_line(&mut line)
            .expect("read");
        assert_eq!(line, "closed stdio\n");
        assert!(launched.child.wait().expect("wait").success());
        std::process::exit(0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn browser_survives_launching_connection_thread_exit() {
        let (_dir, config) = script("read line <&3; echo \"$line\" >&4");
        let mut launched = std::thread::spawn(move || spawn(&config).expect("spawn"))
            .join()
            .expect("launch thread");
        // Joining guarantees that the requesting thread has exited.
        launched
            .command_pipe
            .write_all(b"still alive\n")
            .expect("write");
        let mut line = String::new();
        BufReader::new(&mut launched.response_pipe)
            .read_line(&mut line)
            .expect("read");
        assert_eq!(line, "still alive\n");
        assert!(launched.child.wait().expect("wait").success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_spawns_do_not_leak_fds() {
        // Isolate from the other concurrently running tests' fd activity.
        const MARKER: &str = "CAMOUFOX_FD_LEAK_TEST_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let status = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "process::unix::tests::failed_spawns_do_not_leak_fds",
                    "--test-threads=1",
                ])
                .env(MARKER, "1")
                .status()
                .expect("isolated test");
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().expect("profile");
        let config = LaunchConfig {
            executable: "/nonexistent/camoufox".into(),
            profile_dir: Some(dir.path().to_owned()),
            ..Default::default()
        };
        let count = || std::fs::read_dir("/proc/self/fd").expect("fds").count();
        let before = count();
        for _ in 0..10 {
            assert!(matches!(spawn(&config), Err(ProcessError::SpawnFailed(_))));
            assert_eq!(count(), before);
        }
    }
}
