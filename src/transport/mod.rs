pub mod errors;
pub mod pipe;

use crate::protocol::types::RawMessage;
use errors::TransportError;

/// A bidirectional transport for sending/receiving JSON messages.
///
/// The transport handles framing (null-byte delimited) internally.
/// Implementations must be `Send` to allow use across threads.
pub trait Transport: Send {
    /// Send a JSON message to the browser.
    /// The implementation serializes + frames with `\0`.
    fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError>;

    /// Receive the next complete JSON message from the browser.
    /// Blocks until a full `\0`-delimited message is available.
    fn receive(&mut self) -> Result<RawMessage, TransportError>;

    /// Close the transport. After this, send/receive will return `Closed`.
    fn close(&mut self) -> Result<(), TransportError>;

    /// Whether the transport is closed.
    fn is_closed(&self) -> bool;
}

/// Write half of a split transport.
///
/// Used by the writer thread to send outgoing messages independently
/// of the reader thread. For the pipe transport, this owns fd 3.
pub trait TransportWriter: Send {
    /// Send a JSON message to the browser.
    fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError>;

    /// Close the write end of the transport.
    fn close(&mut self) -> Result<(), TransportError>;

    /// Whether the writer is closed.
    fn is_closed(&self) -> bool;
}

/// Read half of a split transport.
///
/// Used by the reader thread to receive incoming messages independently
/// of the writer thread. For the pipe transport, this owns fd 4.
pub trait TransportReader: Send {
    /// Receive the next complete JSON message from the browser.
    /// Blocks until a full message is available or the transport is closed.
    fn receive(&mut self) -> Result<RawMessage, TransportError>;

    /// Whether the reader is closed.
    fn is_closed(&self) -> bool;
}
