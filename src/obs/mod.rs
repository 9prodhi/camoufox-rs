//! Observability and protocol logging.
//!
//! This module provides structured logging for Juggler protocol messages
//! using the [`log`] crate. All protocol traffic (commands, responses, and
//! events) can be logged at `TRACE` level for debugging.
//!
//! # Usage
//!
//! Enable trace logging by setting the `RUST_LOG` environment variable:
//!
//! ```text
//! RUST_LOG=camoufox=trace cargo run
//! ```
//!
//! Or configure a specific filter for protocol messages only:
//!
//! ```text
//! RUST_LOG=camoufox::obs=trace cargo run
//! ```

pub mod protocol_log;

pub use protocol_log::ProtocolLogger;
