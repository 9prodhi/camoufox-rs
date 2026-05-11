//! Null-byte delimited JSON codec for the Juggler wire format.
//!
//! Messages on the fd 3/4 pipe are UTF-8 JSON objects terminated by a single
//! `\0` byte.  Multiple messages can arrive in a single `read()` call, and a
//! single message may span several reads.
//!
//! See PROTOCOL.md section 1 (Wire Format) and section 2 (Transport) for the
//! authoritative specification.

use super::errors::CodecError;

/// Maximum bytes per `write()` syscall, matching the Firefox-side reader's
/// expectations (64 KiB = 1 << 16).
const DEFAULT_WRITE_CHUNK_SIZE: usize = 65_536;

/// Null-byte delimited JSON codec.
///
/// Accumulates partial reads in an internal buffer and yields fully-framed
/// JSON messages when a `\0` delimiter is encountered.
#[derive(Debug)]
pub struct NulJsonCodec {
    /// Accumulation buffer for bytes that have not yet been delimited.
    buf: Vec<u8>,
    /// Byte offset up to which we have already scanned for `\0`.
    /// Only newly appended bytes need scanning (Firefox-side optimization).
    scan_pos: usize,
    /// Maximum bytes per chunk when encoding for `write()`.
    write_chunk_size: usize,
}

impl Default for NulJsonCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl NulJsonCodec {
    /// Create a new codec with default settings (64 KiB write chunk size).
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            scan_pos: 0,
            write_chunk_size: DEFAULT_WRITE_CHUNK_SIZE,
        }
    }

    /// Create a codec with a custom write-chunk size.
    ///
    /// # Panics
    ///
    /// Panics if `size` is zero.
    pub fn with_write_chunk_size(size: usize) -> Self {
        assert!(size > 0, "write_chunk_size must be positive");
        Self {
            buf: Vec::new(),
            scan_pos: 0,
            write_chunk_size: size,
        }
    }

    /// Feed raw bytes from a `read()` call into the codec.
    ///
    /// Returns a `Vec` of results, one per complete message found between `\0`
    /// delimiters.  Each result is independently either a parsed
    /// [`serde_json::Value`] or a [`CodecError`], so one malformed message
    /// does not prevent subsequent well-formed messages in the same chunk from
    /// being decoded.
    ///
    /// Empty segments (consecutive `\0\0` bytes) are silently ignored, matching
    /// the Firefox-side behaviour.
    ///
    /// Partial messages (no trailing `\0` yet) are buffered internally and will
    /// be yielded once the delimiter arrives in a future `feed()` call.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Result<serde_json::Value, CodecError>> {
        if data.is_empty() {
            return Vec::new();
        }

        let prev_len = self.buf.len();
        self.buf.extend_from_slice(data);

        let mut results: Vec<Result<serde_json::Value, CodecError>> = Vec::new();
        let mut start = 0;
        // Begin scanning from where we left off last time (only examine new bytes
        // that could contain a \0), but we need to scan from `start` if we've
        // already consumed some segments in this call.
        let mut search_from = self.scan_pos.min(prev_len);

        while let Some(rel_pos) = self.buf[search_from..].iter().position(|&b| b == 0) {
            let nul_pos = search_from + rel_pos;

            if nul_pos > start {
                // Non-empty segment: attempt decode.
                let segment = &self.buf[start..nul_pos];
                results.push(Self::decode_segment(segment));
            }
            // else: empty segment (\0\0) — silently skip.

            start = nul_pos + 1;
            search_from = start;
        }

        // Compact: remove all consumed bytes, keep the remaining partial.
        if start > 0 {
            self.buf.drain(..start);
        }
        // All current buffer bytes have been scanned for \0.
        self.scan_pos = self.buf.len();

        results
    }

    /// Decode a single segment (bytes between two `\0` delimiters) into a
    /// JSON value.
    fn decode_segment(segment: &[u8]) -> Result<serde_json::Value, CodecError> {
        // Validate UTF-8 first to give a specific error variant.
        // We need an owned String to get FromUtf8Error (which CodecError wraps).
        let text = String::from_utf8(segment.to_vec())?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        Ok(value)
    }

    /// Encode a JSON value into the wire format: UTF-8 JSON bytes followed by
    /// a `\0` delimiter.
    ///
    /// Returns the full encoded payload as a single `Vec<u8>`.  Use
    /// [`encode_chunks`](Self::encode_chunks) to split the result into
    /// write-sized pieces.
    pub fn encode(value: &serde_json::Value) -> Vec<u8> {
        // serde_json::to_vec on a Value is infallible (no I/O, no custom
        // serializer errors). unwrap is safe.
        let mut bytes = serde_json::to_vec(value).expect("serde_json::to_vec cannot fail on Value");
        bytes.push(0);
        bytes
    }

    /// Encode a JSON value and split the result into chunks suitable for
    /// individual `write()` syscalls.
    ///
    /// Each chunk is at most `write_chunk_size` bytes (default 64 KiB).
    /// The trailing `\0` delimiter is included in the payload and naturally
    /// ends up in the final chunk.
    pub fn encode_chunks(&self, value: &serde_json::Value) -> Vec<Vec<u8>> {
        let payload = Self::encode(value);
        if payload.len() <= self.write_chunk_size {
            return vec![payload];
        }

        payload
            .chunks(self.write_chunk_size)
            .map(|c| c.to_vec())
            .collect()
    }

    /// Returns `true` if the internal buffer holds un-consumed bytes (a
    /// partial message is pending).
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Clear the internal buffer, discarding any partial message data.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.scan_pos = 0;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Helpers -----------------------------------------------------------

    /// Convenience: encode a JSON value to wire bytes (with trailing \0).
    fn wire(v: &serde_json::Value) -> Vec<u8> {
        NulJsonCodec::encode(v)
    }

    /// Unwrap all results in a feed output, panicking on any error.
    fn unwrap_all(results: Vec<Result<serde_json::Value, CodecError>>) -> Vec<serde_json::Value> {
        results.into_iter().map(|r| r.unwrap()).collect()
    }

    // -- Single message ---------------------------------------------------

    #[test]
    fn feed_single_complete_message() {
        let mut codec = NulJsonCodec::new();
        let msg = json!({"id": 1, "method": "Browser.enable", "params": {}});
        let results = unwrap_all(codec.feed(&wire(&msg)));

        assert_eq!(results.len(), 1);
        assert_eq!(results[0], msg);
        assert!(!codec.has_partial());
    }

    // -- Multiple messages in one feed ------------------------------------

    #[test]
    fn feed_multiple_messages_in_one_chunk() {
        let mut codec = NulJsonCodec::new();
        let m1 = json!({"id": 1, "result": {}});
        let m2 = json!({"method": "Page.loaded", "params": {}, "sessionId": "s1"});

        let mut data = wire(&m1);
        data.extend_from_slice(&wire(&m2));

        let results = unwrap_all(codec.feed(&data));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], m1);
        assert_eq!(results[1], m2);
        assert!(!codec.has_partial());
    }

    // -- Partial message across feeds -------------------------------------

    #[test]
    fn feed_partial_then_rest() {
        let mut codec = NulJsonCodec::new();
        let msg = json!({"id": 42, "result": {"value": "hello"}});
        let full = wire(&msg);

        // Split in the middle.
        let mid = full.len() / 2;
        let (part1, part2) = full.split_at(mid);

        // First feed: no complete message yet.
        let r1 = codec.feed(part1);
        assert!(r1.is_empty());
        assert!(codec.has_partial());

        // Second feed: completes the message.
        let r2 = unwrap_all(codec.feed(part2));
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0], msg);
        assert!(!codec.has_partial());
    }

    // -- Empty messages (consecutive \0 bytes) ----------------------------

    #[test]
    fn feed_empty_messages_ignored() {
        let mut codec = NulJsonCodec::new();
        let m1 = json!({"id": 1});
        let m2 = json!({"id": 2});

        // \0\0 between two real messages.
        let mut data = wire(&m1);
        data.push(0); // extra \0 -> empty segment
        data.push(0); // another extra \0 -> empty segment
        data.extend_from_slice(&wire(&m2));

        let results = unwrap_all(codec.feed(&data));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], m1);
        assert_eq!(results[1], m2);
    }

    #[test]
    fn feed_only_empty_messages() {
        let mut codec = NulJsonCodec::new();
        let results = codec.feed(b"\0\0\0");
        assert!(results.is_empty());
        assert!(!codec.has_partial());
    }

    // -- Large message chunking -------------------------------------------

    #[test]
    fn encode_chunks_small_message_single_chunk() {
        let codec = NulJsonCodec::new();
        let msg = json!({"id": 1});
        let chunks = codec.encode_chunks(&msg);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], wire(&msg));
    }

    #[test]
    fn encode_chunks_large_message() {
        // Use a tiny chunk size to force splitting.
        let codec = NulJsonCodec::with_write_chunk_size(16);
        let msg = json!({"data": "abcdefghijklmnopqrstuvwxyz0123456789"});
        let chunks = codec.encode_chunks(&msg);

        // Must produce more than one chunk.
        assert!(
            chunks.len() > 1,
            "expected multiple chunks, got {}",
            chunks.len()
        );

        // Every chunk must be <= chunk_size.
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.len() <= 16,
                "chunk {i} has length {}, exceeds 16",
                chunk.len()
            );
        }

        // Reassembled payload must equal the full encoding.
        let reassembled: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(reassembled, wire(&msg));
    }

    #[test]
    fn encode_chunks_round_trip() {
        // Encode in chunks, then feed all chunks to a decoder.
        let codec = NulJsonCodec::with_write_chunk_size(10);
        let msg = json!({"method": "Browser.getInfo", "id": 99, "params": {}});
        let chunks = codec.encode_chunks(&msg);

        let mut decoder = NulJsonCodec::new();
        let mut all_results = Vec::new();
        for chunk in &chunks {
            all_results.extend(decoder.feed(chunk));
        }

        assert_eq!(all_results.len(), 1);
        assert_eq!(all_results[0].as_ref().unwrap(), &msg);
    }

    // -- Encode basic correctness -----------------------------------------

    #[test]
    fn encode_produces_nul_terminated_json() {
        let msg = json!({"id": 1});
        let encoded = NulJsonCodec::encode(&msg);

        // Must end with \0.
        assert_eq!(*encoded.last().unwrap(), 0u8);

        // Without the \0, it must be valid JSON.
        let json_part = &encoded[..encoded.len() - 1];
        let parsed: serde_json::Value = serde_json::from_slice(json_part).unwrap();
        assert_eq!(parsed, msg);
    }

    // -- Error cases ------------------------------------------------------

    #[test]
    fn feed_invalid_json_returns_error() {
        let mut codec = NulJsonCodec::new();
        let data = b"not valid json\0";
        let results = codec.feed(data);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        match results[0].as_ref().unwrap_err() {
            CodecError::InvalidJson(_) => {}
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn feed_invalid_utf8_returns_error() {
        let mut codec = NulJsonCodec::new();
        // 0xFF 0xFE are not valid UTF-8.
        let data: &[u8] = &[0xFF, 0xFE, 0x00];
        let results = codec.feed(data);
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        match results[0].as_ref().unwrap_err() {
            CodecError::InvalidUtf8(_) => {}
            other => panic!("expected InvalidUtf8, got {other:?}"),
        }
    }

    #[test]
    fn feed_bad_message_does_not_block_subsequent_good_ones() {
        let mut codec = NulJsonCodec::new();
        let good = json!({"id": 7, "result": {}});

        let mut data = Vec::new();
        data.extend_from_slice(b"{{bad json\0");
        data.extend_from_slice(&wire(&good));

        let results = codec.feed(&data);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_err()); // bad message
        assert_eq!(results[1].as_ref().unwrap(), &good); // good message still decoded
    }

    // -- Empty feed -------------------------------------------------------

    #[test]
    fn feed_empty_data_returns_nothing() {
        let mut codec = NulJsonCodec::new();
        let results = codec.feed(b"");
        assert!(results.is_empty());
    }

    // -- reset ------------------------------------------------------------

    #[test]
    fn reset_clears_partial() {
        let mut codec = NulJsonCodec::new();
        codec.feed(b"{\"id\":");
        assert!(codec.has_partial());
        codec.reset();
        assert!(!codec.has_partial());
    }

    // -- Interleaved partial + complete -----------------------------------

    #[test]
    fn feed_mixed_partial_and_complete() {
        let mut codec = NulJsonCodec::new();
        let m1 = json!({"id": 1});
        let m2 = json!({"id": 2});

        // Provide: complete m1 + partial m2 (no trailing \0).
        let mut data = wire(&m1);
        let m2_wire = wire(&m2);
        // Append m2 without its trailing \0.
        data.extend_from_slice(&m2_wire[..m2_wire.len() - 1]);

        let results = unwrap_all(codec.feed(&data));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], m1);
        assert!(codec.has_partial());

        // Now finish m2.
        let results2 = unwrap_all(codec.feed(b"\0"));
        assert_eq!(results2.len(), 1);
        assert_eq!(results2[0], m2);
        assert!(!codec.has_partial());
    }

    // -- Multiple feeds accumulate ----------------------------------------

    #[test]
    fn multiple_tiny_feeds_accumulate() {
        let mut codec = NulJsonCodec::new();

        // Feed in tiny increments.
        assert!(codec.feed(b"{").is_empty());
        assert!(codec.feed(b"\"id").is_empty());
        assert!(codec.feed(b"\":1").is_empty());
        assert!(codec.feed(b",\"re").is_empty());
        assert!(codec.feed(b"sult").is_empty());
        assert!(codec.feed(b"\":{}").is_empty());
        assert!(codec.feed(b"}").is_empty());

        // Final \0 completes the message.
        let results = unwrap_all(codec.feed(b"\0"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], json!({"id": 1, "result": {}}));
    }

    // -- Default write chunk size -----------------------------------------

    #[test]
    fn default_chunk_size_is_64k() {
        let codec = NulJsonCodec::new();
        assert_eq!(codec.write_chunk_size, 65_536);
    }

    // -- Event message round-trip -----------------------------------------

    #[test]
    fn event_message_round_trip() {
        let mut codec = NulJsonCodec::new();
        let event = json!({"method": "Page.loadFinished", "params": {}, "sessionId": "abc"});
        let results = unwrap_all(codec.feed(&wire(&event)));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], event);
    }

    // -- Response with error round-trip -----------------------------------

    #[test]
    fn error_response_round_trip() {
        let mut codec = NulJsonCodec::new();
        let err_resp = json!({
            "id": 5,
            "sessionId": "s1",
            "error": {"message": "No such method", "data": "stack trace"}
        });
        let results = unwrap_all(codec.feed(&wire(&err_resp)));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], err_resp);
    }
}
