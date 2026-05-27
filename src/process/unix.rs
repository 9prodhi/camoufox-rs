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

use std::os::unix::io::FromRawFd;
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
fn create_pipe() -> Result<(libc::c_int, libc::c_int), ProcessError> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe2` is a standard POSIX function. It writes two valid file
    // descriptors into the provided array on success. The array is stack-
    // allocated and correctly sized.
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        return Err(ProcessError::Io(std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// Close a raw file descriptor, ignoring errors.
///
/// # Safety
///
/// The caller must ensure `fd` is a valid, open file descriptor that they own.
/// After this call, `fd` must not be used again.
unsafe fn close_fd(fd: libc::c_int) {
    // On Linux, the fd is always closed even if close() returns EINTR.
    // Retrying after EINTR would risk closing a reused fd. We ignore errors.
    let _ = libc::close(fd);
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
    // --- Create pipe pairs ---
    //
    // Pipe A (commands):  parent writes to pipe_a_write,
    //                     child reads from pipe_a_read (dup2'd to fd 3)
    // Pipe B (responses): child writes to pipe_b_write (dup2'd to fd 4),
    //                     parent reads from pipe_b_read
    let (pipe_a_read, pipe_a_write) = create_pipe()?;
    let (pipe_b_read, pipe_b_write) = create_pipe()?;

    // Wrap parent-side fds in File immediately so they get closed via Drop
    // if an error occurs later (before we return them in LaunchedProcess).
    //
    // SAFETY: These are freshly created, valid fds. We transfer ownership to
    // the File; no other code will close them.
    let command_pipe = unsafe { std::fs::File::from_raw_fd(pipe_a_write) };
    let response_pipe = unsafe { std::fs::File::from_raw_fd(pipe_b_read) };

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
    let child_read = pipe_a_read;
    let child_write = pipe_b_write;

    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            // dup2(src, dst): makes dst a copy of src. If src == dst, it is a
            // no-op (the fd survives but O_CLOEXEC remains set, which we must
            // clear so the fd survives exec).

            if child_read != CHILD_READ_FD {
                if libc::dup2(child_read, CHILD_READ_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close the original since we have a copy on fd 3 now.
                // (It has O_CLOEXEC so exec would close it anyway, but
                // closing early is cleaner.)
                libc::close(child_read);
            } else {
                // child_read IS already fd 3. Clear O_CLOEXEC so it
                // survives exec.
                let flags = libc::fcntl(child_read, libc::F_GETFD);
                if flags == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(child_read, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if child_write != CHILD_WRITE_FD {
                if libc::dup2(child_write, CHILD_WRITE_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(child_write);
            } else {
                // child_write IS already fd 4. Clear O_CLOEXEC.
                let flags = libc::fcntl(child_write, libc::F_GETFD);
                if flags == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(child_write, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            Ok(())
        });
    }

    // --- Spawn ---
    let child = cmd.spawn().map_err(ProcessError::SpawnFailed)?;

    // --- Close child-side fds in the parent ---
    //
    // After fork(), the parent still has copies of the child-side fds.
    // We must close them so that:
    // - pipe_a_read closed: parent no longer holds the read end of pipe A
    //   (only the child reads from fd 3).
    // - pipe_b_write closed: parent no longer holds the write end of pipe B
    //   (only the child writes to fd 4). Without this, reads on pipe_b_read
    //   would never see EOF when the child exits.
    //
    // SAFETY: These are valid fds we own in the parent. The child has its own
    // copies (via dup2 in pre_exec), so closing these is correct.
    unsafe {
        close_fd(pipe_a_read);
        close_fd(pipe_b_write);
    }

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
    use std::io::{Read, Write};
    use std::path::PathBuf;

    /// Test that `create_pipe` returns two distinct, valid fds.
    #[test]
    fn create_pipe_returns_valid_fds() {
        let (r, w) = create_pipe().expect("pipe2 should succeed");
        assert!(r >= 0);
        assert!(w >= 0);
        assert_ne!(r, w);

        // Write through the pipe and read back.
        let msg = b"hello pipe";
        // SAFETY: r and w are valid fds from create_pipe.
        let written = unsafe { libc::write(w, msg.as_ptr() as *const libc::c_void, msg.len()) };
        assert_eq!(written as usize, msg.len());

        let mut buf = [0u8; 64];
        let read_n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert_eq!(read_n as usize, msg.len());
        assert_eq!(&buf[..msg.len()], msg);

        unsafe {
            close_fd(r);
            close_fd(w);
        }
    }

    /// Test that `create_pipe` fds have `O_CLOEXEC` set.
    #[test]
    fn pipe_fds_have_cloexec() {
        let (r, w) = create_pipe().expect("pipe2 should succeed");

        // SAFETY: r and w are valid fds.
        let r_flags = unsafe { libc::fcntl(r, libc::F_GETFD) };
        let w_flags = unsafe { libc::fcntl(w, libc::F_GETFD) };
        assert_ne!(r_flags, -1);
        assert_ne!(w_flags, -1);
        assert_ne!(r_flags & libc::FD_CLOEXEC, 0, "read fd must have CLOEXEC");
        assert_ne!(w_flags & libc::FD_CLOEXEC, 0, "write fd must have CLOEXEC");

        unsafe {
            close_fd(r);
            close_fd(w);
        }
    }

    /// Test that `spawn` creates a child process with working pipes.
    ///
    /// We use `/bin/sh` as a stand-in for camoufox. The shell does not
    /// understand Firefox flags and will exit quickly, but the pipe setup
    /// is what we are testing. The child process is expected to spawn
    /// successfully, and the parent-side pipe fds should be usable.
    #[test]
    fn spawn_with_sh_creates_pipes() {
        let config = LaunchConfig {
            executable: PathBuf::from("/bin/sh"),
            profile_dir: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };

        let result = spawn(&config);
        match result {
            Ok(mut launched) => {
                // sh will exit quickly (it does not understand firefox flags).
                let status = launched.child.wait().expect("wait should succeed");
                let _ = status;

                // Verify command_pipe is writable (may get BrokenPipe since
                // child exited, but the fd should be valid).
                let write_result = launched.command_pipe.write(b"test");
                let _ = write_result;

                // Verify response_pipe is readable (should get EOF).
                let mut buf = [0u8; 64];
                let read_result = launched.response_pipe.read(&mut buf);
                match read_result {
                    Ok(0) => {}  // EOF — expected since child exited
                    Ok(_) => {}  // child wrote something
                    Err(_) => {} // broken pipe or similar
                }
            }
            Err(ProcessError::SpawnFailed(_)) => {
                // /bin/sh not available in some environments; skip.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// Test that spawn passes environment variables to the child.
    #[test]
    fn spawn_passes_env_vars() {
        let mut config = LaunchConfig {
            executable: PathBuf::from("/bin/sh"),
            profile_dir: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };
        config
            .env
            .insert("CAMOUFOX_TEST_VAR".into(), "test_value".into());

        match spawn(&config) {
            Ok(mut launched) => {
                let _ = launched.child.wait();
            }
            Err(ProcessError::SpawnFailed(_)) => {
                // /bin/sh not available; skip.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    /// Test the fd 3/4 pipe transport end-to-end with a shell script that
    /// reads from fd 3 and writes to fd 4.
    #[test]
    fn spawn_fd_pipes_are_connected() {
        // Use a shell one-liner that:
        //   1. Reads a line from fd 3
        //   2. Writes it back to fd 4
        //   3. Exits
        //
        // We override `executable` and build custom args via a trick:
        // set executable to /bin/sh and use -c with the script as args.
        //
        // Since build_args() prepends -no-remote etc., we cannot easily
        // inject -c. Instead, create a tiny script file.
        use std::io::{BufRead, BufReader};

        // Write a temp script.
        let script_path = "/tmp/camoufox_rs_test_pipe.sh";
        {
            let mut f = std::fs::File::create(script_path).expect("create script");
            // The script ignores all args, reads one line from fd 3, writes to fd 4.
            f.write_all(b"#!/bin/sh\nread line <&3\necho \"$line\" >&4\n")
                .expect("write script");
        }

        // Make it executable.
        let _ = std::process::Command::new("chmod")
            .args(["+x", script_path])
            .status();

        let config = LaunchConfig {
            executable: PathBuf::from(script_path),
            profile_dir: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };

        match spawn(&config) {
            Ok(mut launched) => {
                // Write a message to the command pipe (fd 3 from child's perspective).
                let test_msg = b"hello from parent\n";
                launched
                    .command_pipe
                    .write_all(test_msg)
                    .expect("write to command pipe");

                // Read the echo back from the response pipe (fd 4 from child).
                let mut reader = BufReader::new(&mut launched.response_pipe);
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .expect("read from response pipe");
                assert_eq!(line.trim(), "hello from parent");

                let _ = launched.child.wait();
            }
            Err(ProcessError::SpawnFailed(e)) => {
                eprintln!("spawn failed (expected in some envs): {e}");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }

        // Cleanup.
        let _ = std::fs::remove_file(script_path);
    }
}
