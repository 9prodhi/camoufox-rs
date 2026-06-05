//! Readiness detection for the Camoufox browser process.
//!
//! After spawning, Camoufox prints a readiness banner once the Juggler engine
//! is initialized and the fd 3/4 pipe transport is ready to accept commands
//! (see PROTOCOL.md section 2 & 12).
//!
//! The exact banner — and the stream it lands on — varies across builds:
//!
//! - Some Camoufox builds emit `"Juggler pipe initialized"` (patched message).
//! - Stock Playwright Firefox / recent Camoufox builds (e.g. 150.0.2-beta.25)
//!   emit `"Juggler listening to the pipe"` **on stdout**, not stderr.
//!
//! To be robust across builds, this module watches **both** stdout and stderr
//! and accepts **either** banner. It keeps draining both streams after the
//! banner is seen, so that a long-lived browser logging to stdout/stderr can
//! never fill its pipe buffer and deadlock.

use crate::process::ProcessError;

use std::io::{BufRead, BufReader, Read};
use std::process::Child;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Readiness banner used by older Camoufox builds (patched Juggler message).
///
/// Retained as the canonical constant referenced by tests; the full set of
/// accepted banners is [`READINESS_SENTINELS`].
const READINESS_SENTINEL: &str = "Juggler pipe initialized";

/// All readiness banners accepted across Camoufox / Playwright-Firefox builds.
///
/// `contains` matching is used, so a timestamped or prefixed line still
/// matches. The vanilla Playwright banner (`"Juggler listening to the pipe"`)
/// is emitted on **stdout**, which is why readiness watches both streams.
const READINESS_SENTINELS: &[&str] = &["Juggler listening to the pipe", READINESS_SENTINEL];

/// Returns true if `line` contains any accepted readiness banner.
fn is_ready_line(line: &str) -> bool {
    READINESS_SENTINELS.iter().any(|s| line.contains(s))
}

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

/// Wait for the Camoufox process to signal readiness on stdout or stderr.
///
/// Spawns one background reader thread per available output stream (stdout and
/// stderr). Each thread scans line by line for any banner in
/// [`READINESS_SENTINELS`]. The first thread to see a banner reports `Ready`;
/// whichever stream it appeared on no longer matters. Uses a channel with a
/// deadline to enforce the timeout.
///
/// After signalling, the reader threads keep draining their streams to EOF so
/// a long-lived browser cannot fill a pipe buffer and deadlock.
///
/// # Arguments
///
/// * `child` - The spawned child process. Its `stdout` and `stderr` are taken
///   (consumed) by this function. After this call, both are `None`.
/// * `timeout` - Maximum time to wait for a readiness banner.
///
/// # Returns
///
/// * `Ok(output)` - A banner was found. Returns the output collected from the
///   stream it appeared on, up to and including the banner line.
/// * `Err(ProcessError::ExitedBeforeReady)` - All watched streams reached EOF
///   (the process exited) before any banner appeared.
/// * `Err(ProcessError::Timeout)` - The timeout expired. The child process is
///   still running; the caller should kill it.
/// * `Err(ProcessError::Io)` - Neither stdout nor stderr was piped.
pub fn wait_for_ready(child: &mut Child, timeout: Duration) -> Result<String, ProcessError> {
    let (tx, rx) = mpsc::channel::<ReadResult>();

    // Spawn a reader per available stream. At least one must be piped.
    let mut readers = 0u32;

    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("camoufox-stdout-reader".into())
            .spawn(move || stream_reader("stdout", stdout, tx))
            .map_err(ProcessError::Io)?;
        readers += 1;
    }

    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("camoufox-stderr-reader".into())
            .spawn(move || stream_reader("stderr", stderr, tx))
            .map_err(ProcessError::Io)?;
        readers += 1;
    }

    // Drop the original sender so the channel disconnects once every reader
    // thread (each holding a clone) has finished.
    drop(tx);

    if readers == 0 {
        return Err(ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "neither child stdout nor stderr is piped (were they already taken?)",
        )));
    }

    let deadline = Instant::now() + timeout;
    // Track outstanding readers; only when ALL of them EOF without a banner do
    // we conclude the process exited before becoming ready.
    let mut active = readers;
    let mut last_output = String::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(ReadResult::Ready(output)) => return Ok(output),

            Ok(ReadResult::Eof(output)) => {
                if !output.is_empty() {
                    last_output = output;
                }
                active -= 1;
                if active == 0 {
                    let code = child.try_wait().ok().flatten().and_then(|s| s.code());
                    return Err(ProcessError::ExitedBeforeReady {
                        code,
                        stderr: last_output,
                    });
                }
            }

            Ok(ReadResult::Error(e)) => {
                // Treat a read error on one stream as that reader finishing;
                // keep waiting on the other stream if it is still alive.
                log::warn!("readiness stream read error: {e}");
                active -= 1;
                if active == 0 {
                    return Err(ProcessError::Io(e));
                }
            }

            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(ProcessError::Timeout {
                    timeout,
                    stderr: last_output,
                });
            }

            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // All readers dropped their senders without a banner.
                let code = child.try_wait().ok().flatten().and_then(|s| s.code());
                return Err(ProcessError::ExitedBeforeReady {
                    code,
                    stderr: last_output,
                });
            }
        }
    }
}

/// Background function that reads one stream line by line, looking for any
/// readiness banner.
///
/// Sends at most one decisive [`ReadResult`] on the channel: `Ready` when a
/// banner is seen, otherwise `Eof`/`Error` when the stream ends. After a
/// `Ready` signal it keeps reading to EOF (discarding the data) so the browser
/// never blocks on a full pipe buffer.
fn stream_reader<R: Read>(label: &'static str, stream: R, tx: mpsc::Sender<ReadResult>) {
    let reader = BufReader::new(stream);
    let mut collected = String::new();
    let mut signalled = false;

    for line_result in reader.lines() {
        match line_result {
            Ok(line) => {
                log::debug!("browser {label}: {line}");
                if !signalled {
                    if is_ready_line(&line) {
                        collected.push_str(&line);
                        collected.push('\n');
                        let _ = tx.send(ReadResult::Ready(std::mem::take(&mut collected)));
                        signalled = true;
                        // Keep draining the stream to EOF below.
                    } else {
                        collected.push_str(&line);
                        collected.push('\n');
                    }
                }
            }
            Err(e) => {
                log::warn!("{label} read error: {e}");
                if !signalled {
                    let _ = tx.send(ReadResult::Error(e));
                }
                return;
            }
        }
    }

    // EOF reached — the child closed this stream (likely exited).
    if !signalled {
        let _ = tx.send(ReadResult::Eof(collected));
    }
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
    fn is_ready_line_matches_all_known_banners() {
        assert!(is_ready_line("Juggler pipe initialized"));
        assert!(is_ready_line("Juggler listening to the pipe"));
        assert!(is_ready_line("[ts] Juggler listening to the pipe (extra)"));
        assert!(!is_ready_line("some unrelated startup noise"));
    }

    /// Regression: Camoufox 150.0.2-beta.25 prints the vanilla Playwright
    /// banner `"Juggler listening to the pipe"` on **stdout**, not stderr.
    /// Readiness must detect it on stdout even when stderr stays silent.
    #[test]
    fn detects_vanilla_banner_on_stdout() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            // Banner on stdout; unrelated noise on stderr; then linger briefly.
            .arg("echo 'noise' >&2; echo 'Juggler listening to the pipe'; sleep 2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let result = wait_for_ready(&mut child, Duration::from_secs(5));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
        assert!(result.unwrap().contains("Juggler listening to the pipe"));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// With both streams piped, an EOF on one stream must not be mistaken for
    /// process exit while the other stream is still open and about to emit the
    /// banner.
    #[test]
    fn one_stream_eof_does_not_abort_other() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            // stderr closes immediately (subshell exits); stdout emits the
            // banner only after a short delay.
            .arg("( echo 'early' >&2 ) ; sleep 1 ; echo 'Juggler pipe initialized'")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

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
