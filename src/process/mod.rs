//! Camoufox process management: spawning, readiness detection, and lifecycle.
//!
//! This module handles the entire lifecycle of a Camoufox child process:
//!
//! 1. **Spawning** ([`unix::spawn`]): Launch the browser with fd 3/4 pipe
//!    transport configured via `pre_exec` + `dup2`.
//! 2. **Readiness** ([`readiness::wait_for_ready`]): Watch both stdout and
//!    stderr for a Juggler readiness banner (the stream and exact wording
//!    vary across Camoufox builds).
//! 3. **Lifecycle** ([`lifecycle::graceful_shutdown`]): Gracefully shut down
//!    the browser, falling back to `kill` on timeout.
//!
//! # Platform support
//!
//! This module is Unix-only (`#[cfg(unix)]` in `lib.rs`).

pub mod lifecycle;
pub mod readiness;
pub mod unix;

use std::fmt;
use std::process::Child;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from process management.
#[derive(Debug)]
pub enum ProcessError {
    /// Failed to spawn the child process.
    SpawnFailed(std::io::Error),

    /// The process exited before the readiness signal was detected.
    ///
    /// `code` is the exit code (if available), and `stderr` contains any
    /// output captured from the child's stderr stream before exit.
    ExitedBeforeReady { code: Option<i32>, stderr: String },

    /// The readiness timeout expired before the sentinel string appeared.
    ///
    /// `timeout` is the configured duration and `stderr` contains whatever
    /// was captured from stderr up to that point.
    Timeout { timeout: Duration, stderr: String },

    /// Failed to send a kill signal to the process.
    KillFailed(std::io::Error),

    /// Generic I/O error (pipe creation, dup2, etc.).
    Io(std::io::Error),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(e) => write!(f, "failed to spawn browser process: {e}"),
            Self::ExitedBeforeReady { code, stderr } => {
                write!(f, "browser process exited before becoming ready")?;
                if let Some(c) = code {
                    write!(f, " (exit code {c})")?;
                }
                if !stderr.is_empty() {
                    write!(f, "\nstderr:\n{stderr}")?;
                }
                Ok(())
            }
            Self::Timeout { timeout, stderr } => {
                write!(
                    f,
                    "browser did not become ready within {:.1}s",
                    timeout.as_secs_f64()
                )?;
                if !stderr.is_empty() {
                    write!(f, "\nstderr:\n{stderr}")?;
                }
                Ok(())
            }
            Self::KillFailed(e) => write!(f, "failed to kill browser process: {e}"),
            Self::Io(e) => write!(f, "process I/O error: {e}"),
        }
    }
}

impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnFailed(e) | Self::KillFailed(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ProcessError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// LaunchedProcess
// ---------------------------------------------------------------------------

/// The result of successfully spawning a Camoufox child process.
///
/// Contains the child process handle and the two pipe endpoints that form the
/// Juggler transport:
///
/// - `command_pipe`: Parent writes commands here; child reads from fd 3.
/// - `response_pipe`: Child writes responses/events to fd 4; parent reads here.
pub struct LaunchedProcess {
    /// The spawned child process.
    pub child: Child,

    /// Write end of the command pipe (parent writes -> child fd 3 reads).
    ///
    /// This is wrapped in a `std::fs::File` created from the raw fd via
    /// `FromRawFd`. The caller takes ownership and is responsible for
    /// closing it (dropping triggers close).
    pub command_pipe: std::fs::File,

    /// Read end of the response pipe (child fd 4 writes -> parent reads).
    ///
    /// Same ownership semantics as `command_pipe`.
    pub response_pipe: std::fs::File,
}

impl fmt::Debug for LaunchedProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchedProcess")
            .field("child_pid", &self.child.id())
            .field("command_pipe", &self.command_pipe)
            .field("response_pipe", &self.response_pipe)
            .finish()
    }
}
