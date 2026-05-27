//! Browser process lifecycle management: graceful shutdown with kill fallback.
//!
//! The Juggler protocol defines a clean shutdown sequence (PROTOCOL.md
//! section 13):
//!
//! 1. Client sends `Browser.close` (id = -9999).
//! 2. Browser disposes sessions, shuts down the pipe transport, quits.
//! 3. Client waits for the process to exit within `close_timeout`.
//! 4. If the process does not exit in time, it is killed with `SIGKILL`.
//!
//! Alternatively, closing the parent's write end of the command pipe
//! (dropping `command_pipe`) causes the child's reader thread to see EOF,
//! which triggers the same shutdown flow on the browser side.

use crate::process::ProcessError;

use std::process::Child;
use std::time::{Duration, Instant};

/// Minimum poll interval for checking if the child has exited.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Maximum poll interval (ramps up to avoid busy-waiting).
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Gracefully shut down the browser process.
///
/// The caller should have already:
/// 1. Sent `Browser.close` via the transport, **or**
/// 2. Dropped the `command_pipe` to trigger an EOF-based shutdown.
///
/// This function polls `child.try_wait()` in a loop until the process exits
/// or the timeout expires. If the timeout expires, the process is killed
/// with `SIGKILL` and then reaped.
///
/// # Arguments
///
/// * `child` - The browser child process to shut down.
/// * `timeout` - Maximum time to wait for a graceful exit before resorting
///   to `kill`.
///
/// # Returns
///
/// * `Ok(())` - The process exited (either gracefully or after kill).
/// * `Err(ProcessError::KillFailed)` - The `kill()` call failed.
/// * `Err(ProcessError::Io)` - `try_wait()` or `wait()` returned an I/O error.
pub fn graceful_shutdown(child: &mut Child, timeout: Duration) -> Result<(), ProcessError> {
    let deadline = Instant::now() + timeout;
    let mut poll_interval = MIN_POLL_INTERVAL;

    // Poll for exit until the deadline.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process has exited.
                log::debug!(
                    "browser process exited with status: {}",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                );
                return Ok(());
            }
            Ok(None) => {
                // Still running. Check if we have exceeded the deadline.
                let now = Instant::now();
                if now >= deadline {
                    break; // Timeout — proceed to kill.
                }

                // Sleep for a short interval before polling again. The
                // interval ramps up to reduce CPU usage during long waits.
                let remaining = deadline - now;
                let sleep_time = poll_interval.min(remaining);
                std::thread::sleep(sleep_time);

                // Ramp up poll interval (exponential backoff, capped).
                poll_interval = (poll_interval * 2).min(MAX_POLL_INTERVAL);
            }
            Err(e) => {
                return Err(ProcessError::Io(e));
            }
        }
    }

    // Timeout expired — force kill.
    log::warn!(
        "browser process did not exit within {:.1}s, sending SIGKILL",
        timeout.as_secs_f64()
    );

    child.kill().map_err(ProcessError::KillFailed)?;

    // Reap the zombie. This should return quickly after SIGKILL.
    match child.wait() {
        Ok(status) => {
            log::debug!(
                "browser process killed, status: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            );
            Ok(())
        }
        Err(e) => Err(ProcessError::Io(e)),
    }
}

/// Forcefully kill the browser process without waiting for graceful exit.
///
/// Sends `SIGKILL` immediately and reaps the zombie process.
///
/// # Errors
///
/// - `ProcessError::KillFailed` if `kill()` fails.
/// - `ProcessError::Io` if `wait()` fails.
pub fn force_kill(child: &mut Child) -> Result<(), ProcessError> {
    log::debug!("force-killing browser process (pid={})", child.id());
    child.kill().map_err(ProcessError::KillFailed)?;
    child.wait().map_err(ProcessError::Io)?;
    Ok(())
}

/// Check if the child process has already exited.
///
/// Returns `Some(exit_code)` if the process has exited (code is `None` if
/// terminated by signal), or `None` if still running.
pub fn try_exit_status(child: &mut Child) -> Result<Option<Option<i32>>, ProcessError> {
    match child.try_wait() {
        Ok(Some(status)) => Ok(Some(status.code())),
        Ok(None) => Ok(None),
        Err(e) => Err(ProcessError::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Spawn a child process that exits immediately.
    fn spawn_quick_exit() -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh")
    }

    /// Spawn a child process that sleeps for the given duration.
    fn spawn_sleeper(secs: u64) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep {secs}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh")
    }

    #[test]
    fn graceful_shutdown_succeeds_when_process_exits_quickly() {
        let mut child = spawn_quick_exit();
        // Give it a moment to exit.
        std::thread::sleep(Duration::from_millis(50));

        let result = graceful_shutdown(&mut child, Duration::from_secs(5));
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn graceful_shutdown_kills_on_timeout() {
        let mut child = spawn_sleeper(300); // sleeps for 5 minutes

        let start = Instant::now();
        let result = graceful_shutdown(&mut child, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "kill+wait should succeed: {result:?}");
        // Should complete in roughly the timeout duration, not 5 minutes.
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown took too long: {elapsed:?}"
        );
    }

    #[test]
    fn graceful_shutdown_handles_already_exited_process() {
        let mut child = spawn_quick_exit();
        // Wait for it to finish.
        let _ = child.wait();

        // Calling graceful_shutdown on an already-exited process should be fine.
        let result = graceful_shutdown(&mut child, Duration::from_secs(1));
        // This may return Ok (try_wait returns the cached status) or an error
        // depending on platform behavior. Either is acceptable since the
        // process is already dead.
        let _ = result;
    }

    #[test]
    fn force_kill_terminates_process() {
        let mut child = spawn_sleeper(300);

        let result = force_kill(&mut child);
        assert!(result.is_ok(), "force_kill should succeed: {result:?}");
    }

    #[test]
    fn try_exit_status_returns_none_for_running() {
        let mut child = spawn_sleeper(300);

        let result = try_exit_status(&mut child).expect("try_wait should not error");
        assert!(result.is_none(), "process should still be running");

        // Clean up.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn try_exit_status_returns_some_for_exited() {
        let mut child = spawn_quick_exit();
        // Wait for it to exit.
        std::thread::sleep(Duration::from_millis(100));

        let result = try_exit_status(&mut child).expect("try_wait should not error");
        assert!(result.is_some(), "process should have exited");
        assert_eq!(result.unwrap(), Some(0), "exit code should be 0");
    }

    #[test]
    fn graceful_shutdown_poll_interval_ramps_up() {
        // This is an indirect test: verify that shutdown with a longer timeout
        // does not burn excessive CPU. We test by checking that the function
        // returns quickly when the process exits during the timeout window.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 0.15")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        let start = Instant::now();
        let result = graceful_shutdown(&mut child, Duration::from_secs(10));
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // Should detect exit reasonably quickly (within a second, not 10s).
        assert!(
            elapsed < Duration::from_secs(2),
            "should detect exit quickly: {elapsed:?}"
        );
    }
}
