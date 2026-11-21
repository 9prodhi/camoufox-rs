//! Unix pipe transport for the Juggler protocol.
//!
//! Communicates with the Camoufox child process over two anonymous pipes:
//! - fd 3: parent writes commands to the browser
//! - fd 4: parent reads responses/events from the browser
//!
//! Messages are null-byte (`\0`) delimited UTF-8 JSON, handled by
//! [`NulJsonCodec`].

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};

use crate::codec::nul_json::NulJsonCodec;
use crate::protocol::types::RawMessage;
use crate::transport::errors::TransportError;
use crate::transport::{Transport, TransportReader, TransportWriter};

/// Default read buffer size (64KB), matching the write chunk size.
const READ_BUF_SIZE: usize = 65_536;

/// Pipe transport that communicates with Camoufox over fd 3 (write) and fd 4 (read).
///
/// # Usage
///
/// Typically created automatically by the process launcher after spawning
/// the browser. Can also be constructed manually for testing:
///
/// ```rust,no_run
/// use std::fs::File;
/// use camoufox::transport::pipe::PipeTransport;
///
/// // From raw file descriptors (e.g., after fork+exec):
/// let transport = unsafe { PipeTransport::from_raw_fds(3, 4) };
///
/// // Or from already-opened File handles:
/// # let writer = unsafe { File::from_raw_fd(3) };
/// # let reader = unsafe { File::from_raw_fd(4) };
/// # use std::os::unix::io::FromRawFd;
/// let transport = PipeTransport::new(writer, reader);
/// ```
pub struct PipeTransport {
    /// Buffered writer to fd 3 (parent writes commands to browser).
    writer: BufWriter<File>,
    /// Buffered reader from fd 4 (browser writes responses/events to parent).
    reader: BufReader<File>,
    /// Codec for null-byte delimited JSON framing.
    codec: NulJsonCodec,
    /// Whether the transport has been closed.
    closed: bool,
    /// Buffered messages from multi-message reads. When a single `read()`
    /// returns data containing multiple `\0`-delimited messages, extras are
    /// queued here and drained by subsequent `receive()` calls.
    pending_messages: VecDeque<RawMessage>,
}

impl PipeTransport {
    /// Create a pipe transport from raw file descriptors.
    ///
    /// - `write_fd`: The file descriptor the parent writes commands to (fd 3).
    /// - `read_fd`: The file descriptor the parent reads responses from (fd 4).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - Both file descriptors are valid and open.
    /// - The caller transfers ownership of both fds to this transport (they will
    ///   be closed when the transport is dropped).
    /// - No other code reads from `read_fd` or writes to `write_fd` concurrently.
    pub unsafe fn from_raw_fds(write_fd: RawFd, read_fd: RawFd) -> Self {
        let writer = File::from_raw_fd(write_fd);
        let reader = File::from_raw_fd(read_fd);
        Self::new(writer, reader)
    }

    /// Create a pipe transport from already-opened file handles.
    ///
    /// - `writer`: File handle for writing commands to the browser (fd 3).
    /// - `reader`: File handle for reading responses/events from the browser (fd 4).
    pub fn new(writer: File, reader: File) -> Self {
        Self {
            writer: BufWriter::new(writer),
            reader: BufReader::new(reader),
            codec: NulJsonCodec::new(),
            closed: false,
            pending_messages: VecDeque::new(),
        }
    }

    /// Drain the next pending message, if any were buffered from a prior read.
    pub fn drain_pending(&mut self) -> Option<RawMessage> {
        self.pending_messages.pop_front()
    }

    /// Split this transport into independent write and read halves.
    ///
    /// This enables true concurrent I/O: the writer thread can send on fd 3
    /// while the reader thread blocks on fd 4, with no lock contention.
    ///
    /// After splitting, the original `PipeTransport` is consumed.
    pub fn split(self) -> (PipeWriter, PipeReader) {
        (
            PipeWriter {
                writer: self.writer,
                codec: NulJsonCodec::new(),
                closed: self.closed,
            },
            PipeReader {
                reader: self.reader,
                codec: self.codec,
                closed: self.closed,
                pending_messages: self.pending_messages,
            },
        )
    }
}

// ---------------------------------------------------------------------------
// PipeWriter — write half (fd 3)
// ---------------------------------------------------------------------------

/// Write half of the pipe transport (owns fd 3).
///
/// Created by [`PipeTransport::split()`].
pub struct PipeWriter {
    writer: BufWriter<File>,
    codec: NulJsonCodec,
    closed: bool,
}

impl TransportWriter for PipeWriter {
    fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let chunks = self.codec.encode_chunks(message);
        for chunk in &chunks {
            self.writer.write_all(chunk).map_err(|e| {
                self.closed = true;
                TransportError::Io(e)
            })?;
        }
        self.writer.flush().map_err(|e| {
            self.closed = true;
            TransportError::Io(e)
        })?;
        Ok(())
    }

    fn close(&mut self) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if let Ok(devnull) = File::open("/dev/null") {
            let old_writer = std::mem::replace(&mut self.writer, BufWriter::new(devnull));
            drop(old_writer);
        }
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }
}

// ---------------------------------------------------------------------------
// PipeReader — read half (fd 4)
// ---------------------------------------------------------------------------

/// Read half of the pipe transport (owns fd 4).
///
/// Created by [`PipeTransport::split()`].
pub struct PipeReader {
    reader: BufReader<File>,
    codec: NulJsonCodec,
    closed: bool,
    pending_messages: VecDeque<RawMessage>,
}

impl PipeReader {
    /// Drain the next pending message, if any were buffered from a prior read.
    pub fn drain_pending(&mut self) -> Option<RawMessage> {
        self.pending_messages.pop_front()
    }

    /// Mark the reader as closed.
    pub fn close(&mut self) {
        self.closed = true;
    }
}

impl TransportReader for PipeReader {
    fn receive(&mut self) -> Result<RawMessage, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        if let Some(msg) = self.pending_messages.pop_front() {
            return Ok(msg);
        }
        let mut buf = [0u8; READ_BUF_SIZE];
        loop {
            let n = self.reader.read(&mut buf).map_err(|e| {
                self.closed = true;
                TransportError::Io(e)
            })?;
            if n == 0 {
                self.closed = true;
                return Err(TransportError::Closed);
            }
            let results = self.codec.feed(&buf[..n]);
            for result in results {
                match result {
                    Ok(value) => match serde_json::from_value::<RawMessage>(value) {
                        Ok(msg) => {
                            self.pending_messages.push_back(msg);
                        }
                        Err(e) => {
                            log::warn!("PipeReader: schema-mismatched JSON frame, skipping: {e}");
                        }
                    },
                    Err(e) => {
                        log::warn!("PipeReader: invalid JSON frame, skipping: {e}");
                    }
                }
            }
            if let Some(msg) = self.pending_messages.pop_front() {
                return Ok(msg);
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.closed
    }
}

impl PipeTransport {
    /// Whether the transport is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Transport for PipeTransport {
    fn into_split(self: Box<Self>) -> (Box<dyn TransportWriter>, Box<dyn TransportReader>) {
        let (writer, reader) = (*self).split();
        (Box::new(writer), Box::new(reader))
    }
}

impl std::fmt::Debug for PipeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeTransport")
            .field("closed", &self.closed)
            .field("pending_messages", &self.pending_messages.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::IntoRawFd;
    use std::os::unix::net::UnixStream;

    /// Create a pair of connected streams as `File` handles.
    ///
    /// Uses `UnixStream::pair()` and converts each end to a `File` via
    /// `IntoRawFd` + `FromRawFd`, transferring ownership without needing
    /// the `libc` crate for `dup()`.
    fn socket_pair_as_files() -> (File, File) {
        let (a, b) = UnixStream::pair().expect("failed to create UnixStream pair");
        let file_a = unsafe { File::from_raw_fd(a.into_raw_fd()) };
        let file_b = unsafe { File::from_raw_fd(b.into_raw_fd()) };
        (file_a, file_b)
    }

    /// Create a `PipeTransport` connected to mock pipe ends.
    ///
    /// Returns `(transport, browser_write_end, browser_read_end)` where:
    /// - `browser_write_end`: write here to simulate browser sending responses/events
    /// - `browser_read_end`: read here to see what the transport sent to the browser
    fn make_test_transport() -> (PipeTransport, File, File) {
        // For the transport's reader (fd 4 equivalent):
        // browser_write -> transport reads.
        let (browser_write, transport_read) = socket_pair_as_files();

        // For the transport's writer (fd 3 equivalent):
        // transport writes -> browser_read.
        let (transport_write, browser_read) = socket_pair_as_files();

        let transport = PipeTransport::new(transport_write, transport_read);
        (transport, browser_write, browser_read)
    }

    #[test]
    fn send_and_verify() {
        let (transport, _browser_write, mut browser_read) = make_test_transport();
        let (mut writer, _reader) = transport.split();

        let msg = serde_json::json!({
            "id": 1,
            "method": "Browser.enable",
            "params": {}
        });

        writer.send(&msg).unwrap();

        // Read what the transport wrote.
        let mut buf = vec![0u8; 4096];
        let n = browser_read.read(&mut buf).unwrap();
        let data = &buf[..n];

        // Should end with \0.
        assert_eq!(*data.last().unwrap(), 0u8);

        // Parse the JSON (without trailing \0).
        let parsed: serde_json::Value = serde_json::from_slice(&data[..n - 1]).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "Browser.enable");
    }

    #[test]
    fn receive_single_message() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Simulate browser sending a response.
        let response = b"{\"id\":1,\"result\":{}}\0";
        browser_write.write_all(response).unwrap();
        browser_write.flush().unwrap();

        let msg = reader.receive().unwrap();
        assert_eq!(msg.id, Some(1));
        assert!(msg.result.is_some());
    }

    #[test]
    fn receive_multiple_messages_in_one_read() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Simulate browser sending two messages in one write.
        let responses = b"{\"id\":1,\"result\":{}}\0{\"id\":2,\"result\":{}}\0";
        browser_write.write_all(responses).unwrap();
        browser_write.flush().unwrap();

        let msg1 = reader.receive().unwrap();
        assert_eq!(msg1.id, Some(1));

        let msg2 = reader.receive().unwrap();
        assert_eq!(msg2.id, Some(2));
    }

    #[test]
    fn receive_event_message() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        let event = b"{\"method\":\"Page.loadFinished\",\"params\":{\"frameId\":\"main\"},\"sessionId\":\"s1\"}\0";
        browser_write.write_all(event).unwrap();
        browser_write.flush().unwrap();

        let msg = reader.receive().unwrap();
        assert!(msg.id.is_none());
        assert_eq!(msg.method.as_deref(), Some("Page.loadFinished"));
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn receive_eof_returns_closed() {
        let (transport, browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Close the browser's write end -- transport should see EOF.
        drop(browser_write);

        let result = reader.receive();
        assert!(result.is_err());
        match result.unwrap_err() {
            TransportError::Closed => {} // expected
            other => panic!("expected Closed, got: {other}"),
        }
        assert!(reader.is_closed());
    }

    #[test]
    fn send_after_close_returns_closed() {
        let (transport, _browser_write, _browser_read) = make_test_transport();
        let (mut writer, _reader) = transport.split();

        writer.close().unwrap();

        let msg = serde_json::json!({"id": 1, "method": "Browser.enable"});
        let result = writer.send(&msg);
        assert!(result.is_err());
        match result.unwrap_err() {
            TransportError::Closed => {} // expected
            other => panic!("expected Closed, got: {other}"),
        }
        assert!(writer.is_closed());
    }

    #[test]
    fn receive_after_close_returns_closed() {
        let (transport, _browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        reader.close();

        let result = reader.receive();
        assert!(result.is_err());
        match result.unwrap_err() {
            TransportError::Closed => {} // expected
            other => panic!("expected Closed, got: {other}"),
        }
        assert!(reader.is_closed());
    }

    #[test]
    fn close_is_idempotent() {
        let (transport, _browser_write, _browser_read) = make_test_transport();
        let (mut writer, _reader) = transport.split();

        writer.close().unwrap();
        writer.close().unwrap(); // second close should be a no-op
        assert!(writer.is_closed());
    }

    #[test]
    fn receive_partial_then_complete() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Send a partial message (no \0 yet).
        browser_write.write_all(b"{\"id\":").unwrap();
        browser_write.flush().unwrap();

        // Use a separate thread to complete the message after a small delay,
        // since receive() blocks.
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            browser_write.write_all(b"42,\"result\":{}}\0").unwrap();
            browser_write.flush().unwrap();
            browser_write // return ownership so it isn't dropped prematurely
        });

        let msg = reader.receive().unwrap();
        assert_eq!(msg.id, Some(42));

        let _browser_write = handle.join().unwrap();
    }

    #[test]
    fn debug_impl() {
        let (transport, _bw, _br) = make_test_transport();
        let debug_str = format!("{:?}", transport);
        assert!(debug_str.contains("PipeTransport"));
        assert!(debug_str.contains("closed: false"));
    }

    #[test]
    fn send_receive_roundtrip() {
        // Full send-receive cycle simulating parent <-> child communication.
        let (cmd_write, cmd_read) = socket_pair_as_files();
        let (resp_write, resp_read) = socket_pair_as_files();

        // Transport: writes commands via cmd_write, reads responses via resp_read.
        let transport = PipeTransport::new(cmd_write, resp_read);
        let (mut writer, mut reader) = transport.split();

        // "Browser" side uses cmd_read and resp_write.
        let mut browser_cmd_reader = cmd_read;
        let mut browser_resp_writer = resp_write;

        // Send a command through the transport.
        let cmd = serde_json::json!({
            "id": 7,
            "method": "Browser.getInfo",
            "params": {}
        });
        writer.send(&cmd).unwrap();

        // "Browser" reads the command.
        let mut buf = vec![0u8; 4096];
        let n = browser_cmd_reader.read(&mut buf).unwrap();
        assert!(n > 0);

        // Verify the command contains expected JSON.
        let nul_pos = buf[..n].iter().position(|&b| b == 0).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf[..nul_pos]).unwrap();
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["method"], "Browser.getInfo");

        // "Browser" sends a response.
        let resp = b"{\"id\":7,\"result\":{\"version\":\"1.0\"}}\0";
        browser_resp_writer.write_all(resp).unwrap();
        browser_resp_writer.flush().unwrap();

        // Transport receives the response.
        let msg = reader.receive().unwrap();
        assert_eq!(msg.id, Some(7));
        assert!(msg.result.is_some());
        let result = msg.result.unwrap();
        assert_eq!(result["version"], "1.0");
    }

    #[test]
    fn empty_messages_are_skipped() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Send: msg1 \0 \0 msg2 \0 (empty segment between msg1 and msg2).
        let data = b"{\"id\":1,\"result\":{}}\0\0{\"id\":2,\"result\":{}}\0";
        browser_write.write_all(data).unwrap();
        browser_write.flush().unwrap();

        let msg1 = reader.receive().unwrap();
        assert_eq!(msg1.id, Some(1));

        let msg2 = reader.receive().unwrap();
        assert_eq!(msg2.id, Some(2));
    }

    #[test]
    fn bad_frame_in_middle_of_batch_is_skipped() {
        let (transport, mut browser_write, _browser_read) = make_test_transport();
        let (_writer, mut reader) = transport.split();

        // Batch: 1) valid, 2) invalid JSON, 3) schema-mismatched JSON, 4) valid
        let batch = b"{\"id\":100,\"result\":{\"step\":1}}\0{\"broken json\0\"schema mismatched string\"\0{\"id\":200,\"result\":{\"step\":2}}\0";
        browser_write.write_all(batch).unwrap();
        browser_write.flush().unwrap();

        let msg1 = reader.receive().unwrap();
        assert_eq!(msg1.id, Some(100));
        assert_eq!(msg1.result.unwrap()["step"], 1);

        let msg2 = reader.receive().unwrap();
        assert_eq!(msg2.id, Some(200));
        assert_eq!(msg2.result.unwrap()["step"], 2);
    }

    #[test]
    fn is_closed_initially_false() {
        let (transport, _bw, _br) = make_test_transport();
        assert!(!transport.is_closed());
    }

    #[test]
    fn transport_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PipeTransport>();
    }
}
