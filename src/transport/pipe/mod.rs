//! Pipe transport for the Juggler protocol.
//!
//! Uses fd 3 (write commands to browser) and fd 4 (read responses/events from
//! browser) on Unix. On Windows, handles come from `PW_PIPE_READ` /
//! `PW_PIPE_WRITE` environment variables (not yet implemented).
//!
//! # Platform support
//!
//! - **Unix**: Fully implemented via [`unix::PipeTransport`].
//! - **Windows**: Stub that emits a compile-time error.

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{PipeReader, PipeTransport, PipeWriter};

#[cfg(windows)]
pub use windows::PipeTransport;
