//! Readiness detection for the Camoufox browser process.
//!
//! After spawning, Camoufox writes `"Juggler pipe initialized\n"` to stderr
//! once the Juggler engine is initialized and the pipe transport is ready to
//! accept commands (see PROTOCOL.md section 2 & 12).
//!
//! This module watches stderr for that sentinel string, with a configurable
//! timeout. If the process exits or the timeout expires before the sentinel
//! appears, an appropriate error is returned.

use crate::process::ProcessError;

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStderr};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The sentinel string that Camoufox writes to stderr when the Juggler pipe
/// transport is initialized and ready to accept commands.
///
/// Camoufox outputs `"Juggler pipe initialized"` (via its patched Juggler),
/// which differs from vanilla Playwright Firefox's `"Juggler listening to the pipe"`.
const READINESS_SENTINEL: &str = "Juggler pipe initialized";

/// Outcome of the stderr reader thread.
enum ReadResult {
    /// The sentinel string was found. The `String` contains all stderr output
    /// collected up to and including the sentinel line.
    Ready(String),
    /// The child's stderr stream reached EOF (process exited) before the
    /// sentinel was found. The `String` contains all collected stderr output.
    Eof(String),
    /// An I/O error occurred while reading stderr.
    Error(std::io::Error),
}

/// Wait for the Camoufox process to signal readiness on stderr.
///
/// Spawns a background thread that reads from the child's stderr line by line,
/// looking for the [`READINESS_SENTINEL`] substring. Uses a channel with
/// timeout to enforce the deadline.
///
/// # Arguments
///
/// * `child` - The spawned child process. Its `stderr` will be taken
///   (consumed) by this function. After this call, `child.stderr` is `None`.
/// * `timeout` - Maximum time to wait for the sentinel string.
///
/// # Returns
///
/// * `Ok(stderr_output)` - The sentinel was found. Returns all stderr output
///   collected up to that point (useful for logging/debugging).
/// * `Err(ProcessError::ExitedBeforeReady)` - The process exited before the
///   sentinel appeared.
/// * `Err(ProcessError::Timeout)` - The timeout expired. The child process
///   is still running; the caller should kill it.
/// * `Err(ProcessError::Io)` - An I/O error occurred reading stderr, or
///   stderr was not piped.
pub fn wait_for_ready(
    child: &mut Child,
    timeout: Duration,
) -> Result<String, ProcessError> {
    // Take ownership of stderr. If it was not piped, return an error.
    let stderr: ChildStderr = child.stderr.take().ok_or_else(|| {
        ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "child stderr is not piped (was it already taken?)",
        ))
    })?;

    let (tx, rx) = mpsc::channel::<ReadResult>();

    // Spawn a reader thread. This thread owns the stderr handle and reads
    // line by line until it finds the sentinel, hits EOF, or encounters an
    // I/O error.
    std::thread::Builder::new()
        .name("camoufox-stderr-reader".into())
        .spawn(move || {
            stderr_reader(stderr, tx);
        })
        .map_err(ProcessError::Io)?;

    // Wait for the reader thread to report, with timeout.
    let deadline = Instant::now() + timeout;
    let result = rx.recv_timeout(timeout);

    match result {
        Ok(ReadResult::Ready(output)) => Ok(output),

        Ok(ReadResult::Eof(output)) => {
            // Process stderr closed. Check if the process has exited.
            let code = child.try_wait().ok().flatten().map(|s| s.code()).flatten();
            Err(ProcessError::ExitedBeforeReady {
                code,
                stderr: output,
            })
        }

        Ok(ReadResult::Error(e)) => Err(ProcessError::Io(e)),

        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The reader thread may still be running (blocking on stderr read).
            // We report timeout; the caller is responsible for killing the
            // child, which will cause the reader thread to see EOF and exit.
            //
            // Try to collect any stderr output that may have been captured.
            // We cannot get it from the reader thread easily, so we report
            // what we know.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = remaining;

            // Give the reader thread a brief moment to send a partial result
            // in case it finished just after the timeout.
            let stderr_output = match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(ReadResult::Eof(s)) | Ok(ReadResult::Ready(s)) => s,
                _ => String::new(),
            };

            Err(ProcessError::Timeout {
                timeout,
                stderr: stderr_output,
            })
        }

        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // The reader thread panicked or was dropped without sending.
            // Check if the process exited.
            let code = child.try_wait().ok().flatten().map(|s| s.code()).flatten();
            Err(ProcessError::ExitedBeforeReady {
                code,
                stderr: String::new(),
            })
        }
    }
}

/// Background function that reads stderr line by line and looks for the
/// readiness sentinel.
///
/// Sends exactly one [`ReadResult`] on the channel before returning.
fn stderr_reader(stderr: ChildStderr, tx: mpsc::Sender<ReadResult>) {
    let reader = BufReader::new(stderr);
    let mut collected = String::new();

    for line_result in reader.lines() {
        match line_result {
            Ok(line) => {
                log::debug!("browser stderr: {}", line);
                collected.push_str(&line);
                collected.push('\n');

                if line.contains(READINESS_SENTINEL) {
                    let _ = tx.send(ReadResult::Ready(collected));
                    return;
                }
            }
            Err(e) => {
                // I/O error reading stderr (unlikely but possible).
                log::warn!("stderr read error: {e}");
                let _ = tx.send(ReadResult::Error(e));
                return;
            }
        }
    }

    // EOF reached — the child closed its stderr (likely exited).
    let _ = tx.send(ReadResult::Eof(collected));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Helper: spawn a child process that writes the given text to stderr
    /// and then exits.
    fn spawn_echo_stderr(text: &str) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("echo '{}' >&2", text))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh")
    }

    /// Helper: spawn a child process that writes to stderr after a delay.
    fn spawn_delayed_stderr(text: &str, delay_secs: f64) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep {} && echo '{}' >&2", delay_secs, text))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /bin/sh")
    }

    #[test]
    fn detects_readiness_sentinel() {
        let mut child = spawn_echo_stderr(READINESS_SENTINEL);
        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
        let output = result.unwrap();
        assert!(
            output.contains(READINESS_SENTINEL),
            "output should contain sentinel: {output:?}"
        );
        let _ = child.wait();
    }

    #[test]
    fn detects_sentinel_among_other_output() {
        let text = format!(
            "some startup noise\nmore noise\n{}\nand more after",
            READINESS_SENTINEL
        );
        // Use printf to handle newlines properly.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '{}\\n' >&2", text.replace('\n', "\\n")))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("some startup noise"));
        assert!(output.contains(READINESS_SENTINEL));
        let _ = child.wait();
    }

    #[test]
    fn returns_exited_before_ready_when_no_sentinel() {
        // Process that writes something else and exits.
        let mut child = spawn_echo_stderr("no sentinel here");
        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(
            matches!(result, Err(ProcessError::ExitedBeforeReady { .. })),
            "expected ExitedBeforeReady, got: {result:?}"
        );

        if let Err(ProcessError::ExitedBeforeReady { stderr, .. }) = result {
            assert!(
                stderr.contains("no sentinel here"),
                "stderr should contain the output: {stderr:?}"
            );
        }
        let _ = child.wait();
    }

    #[test]
    fn returns_timeout_when_process_hangs() {
        // Process that sleeps for a long time without writing the sentinel.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("echo 'starting...' >&2 && sleep 60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let result = wait_for_ready(&mut child, Duration::from_millis(200));
        assert!(
            matches!(result, Err(ProcessError::Timeout { .. })),
            "expected Timeout, got: {result:?}"
        );

        // Clean up: kill the hanging process.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn returns_error_when_stderr_not_piped() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()) // Not piped!
            .spawn()
            .expect("spawn");

        let result = wait_for_ready(&mut child, Duration::from_secs(1));
        assert!(
            matches!(result, Err(ProcessError::Io(_))),
            "expected Io error, got: {result:?}"
        );
        let _ = child.wait();
    }

    #[test]
    fn returns_error_when_stderr_already_taken() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        // Take stderr manually first.
        let _stolen = child.stderr.take();

        let result = wait_for_ready(&mut child, Duration::from_secs(1));
        assert!(
            matches!(result, Err(ProcessError::Io(_))),
            "expected Io error for missing stderr, got: {result:?}"
        );
        let _ = child.wait();
    }

    #[test]
    fn sentinel_with_prefix_is_detected() {
        // Firefox may prefix the sentinel with other text on the same line.
        // Our detection uses `contains`, so partial matches work.
        let text = format!("[timestamp] {}", READINESS_SENTINEL);
        let mut child = spawn_echo_stderr(&text);
        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(result.is_ok());
        let _ = child.wait();
    }

    #[test]
    fn delayed_sentinel_is_detected_within_timeout() {
        let mut child = spawn_delayed_stderr(READINESS_SENTINEL, 0.1);
        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
        let _ = child.wait();
    }

    #[test]
    fn empty_stderr_returns_exited_before_ready() {
        // Process that immediately exits without writing anything to stderr.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(
            matches!(result, Err(ProcessError::ExitedBeforeReady { .. })),
            "expected ExitedBeforeReady, got: {result:?}"
        );
        let _ = child.wait();
    }
}
