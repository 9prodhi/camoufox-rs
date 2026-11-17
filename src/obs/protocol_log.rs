//! Protocol message logger.
//!
//! [`ProtocolLogger`] provides structured logging for all Juggler protocol
//! traffic at `TRACE` level using the [`log`] crate. This is invaluable
//! for debugging protocol interactions.
//!
//! # Log format
//!
//! - Outgoing commands: `SEND method params_preview`
//! - Incoming responses: `RECV id result_or_error_preview`
//! - Incoming events: `EVNT method [session_id]`
//!
//! Large payloads (over 512 bytes when serialized) are truncated with
//! `"..."` to avoid flooding log output.

use crate::protocol::types::ErrorData;

/// Maximum length of a JSON value preview in log messages.
const MAX_PREVIEW_LEN: usize = 512;

/// Protocol message logger.
///
/// All methods are stateless and take their arguments by reference.
/// The logger uses the [`log`] crate at `TRACE` level, so messages are
/// zero-cost when trace logging is not enabled.
///
/// # Integration
///
/// Call the appropriate method from the transport/connection layer:
///
/// - [`log_send`](ProtocolLogger::log_send) when sending a command.
/// - [`log_receive_response`](ProtocolLogger::log_receive_response) when
///   receiving a response.
/// - [`log_receive_event`](ProtocolLogger::log_receive_event) when
///   receiving an event.
pub struct ProtocolLogger;

impl ProtocolLogger {
    /// Log an outgoing command at TRACE level.
    ///
    /// # Arguments
    ///
    /// - `method`: The protocol method name (e.g., `"Browser.enable"`).
    /// - `params`: The command parameters as a JSON value.
    ///
    /// # Log output
    ///
    /// ```text
    /// SEND Browser.enable {"attachToDefaultContext":false}
    /// ```
    pub fn log_send(method: &str, params: &serde_json::Value) {
        if log::log_enabled!(log::Level::Trace) {
            let preview = truncate_json(params);
            log::trace!("SEND {} {}", method, preview);
        }
    }

    /// Log an incoming response at TRACE level.
    ///
    /// # Arguments
    ///
    /// - `id`: The message ID of the original request.
    /// - `result`: The response result (success or error).
    ///
    /// # Log output
    ///
    /// Success:
    /// ```text
    /// RECV id=1 OK {"userAgent":"Mozilla/5.0 ..."}
    /// ```
    ///
    /// Error:
    /// ```text
    /// RECV id=2 ERR "Unknown method"
    /// ```
    pub fn log_receive_response(id: i64, result: &Result<serde_json::Value, ErrorData>) {
        if log::log_enabled!(log::Level::Trace) {
            match result {
                Ok(value) => {
                    let preview = truncate_json(value);
                    log::trace!("RECV id={} OK {}", id, preview);
                }
                Err(error) => {
                    log::trace!("RECV id={} ERR {:?}", id, error.message);
                }
            }
        }
    }

    /// Log an incoming event at TRACE level.
    ///
    /// # Arguments
    ///
    /// - `method`: The event method name (e.g., `"Browser.attachedToTarget"`).
    /// - `session_id`: The session ID (root session = `None`).
    ///
    /// # Log output
    ///
    /// Root session event:
    /// ```text
    /// EVNT Browser.attachedToTarget [root]
    /// ```
    ///
    /// Page session event:
    /// ```text
    /// EVNT Page.navigationStarted [session=abc-123]
    /// ```
    pub fn log_receive_event(method: &str, session_id: &Option<String>) {
        if log::log_enabled!(log::Level::Trace) {
            let session_label = match session_id {
                Some(id) => format!("[session={}]", truncate_str(id, 36)),
                None => "[root]".to_owned(),
            };
            log::trace!("EVNT {} {}", method, session_label);
        }
    }

    /// Log a raw outgoing message at TRACE level.
    ///
    /// This is a lower-level alternative to [`log_send`](ProtocolLogger::log_send)
    /// that logs the full wire message.
    ///
    /// # Arguments
    ///
    /// - `id`: The message ID.
    /// - `method`: The protocol method name.
    /// - `session_id`: The session ID (root session = `None`).
    pub fn log_send_raw(id: i64, method: &str, session_id: &Option<String>) {
        if log::log_enabled!(log::Level::Trace) {
            let session_label = match session_id {
                Some(id) => format!(" session={}", truncate_str(id, 36)),
                None => String::new(),
            };
            log::trace!("SEND id={} {}{}", id, method, session_label);
        }
    }

    /// Log a connection lifecycle event at DEBUG level.
    ///
    /// # Arguments
    ///
    /// - `event`: Description of the lifecycle event (e.g., `"connected"`,
    ///   `"disconnected"`, `"session created"`).
    pub fn log_lifecycle(event: &str) {
        log::debug!("LIFECYCLE {}", event);
    }

    /// Log a warning about a protocol anomaly.
    ///
    /// Used for non-fatal issues like unknown session IDs, unmatched
    /// response IDs, etc.
    ///
    /// # Arguments
    ///
    /// - `message`: Description of the anomaly.
    pub fn log_anomaly(message: &str) {
        log::warn!("PROTOCOL {}", message);
    }

    /// Log an outgoing JSON message at TRACE level with zero allocation overhead
    /// when trace logging is disabled.
    ///
    /// Checks `log_enabled!(target: "camoufox::protocol", log::Level::Trace)`
    /// first, returning immediately without serializing or formatting if trace
    /// logging is disabled.
    ///
    /// # Intended call sites
    ///
    /// To be called from client/transport write paths immediately before sending
    /// an outgoing JSON command payload over the underlying transport / WebSocket:
    /// - `src/protocol/client.rs`: `Connection::send_raw` or `Session::send_with_timeout`
    pub fn log_outgoing(value: &serde_json::Value) {
        if !log::log_enabled!(target: "camoufox::protocol", log::Level::Trace) {
            return;
        }
        let preview = truncate_json(value);
        log::trace!(target: "camoufox::protocol", "SEND {}", preview);
    }

    /// Log an incoming raw text frame at TRACE level with zero allocation overhead
    /// when trace logging is disabled.
    ///
    /// Checks `log_enabled!(target: "camoufox::protocol", log::Level::Trace)`
    /// first, returning immediately without copying or formatting if trace
    /// logging is disabled.
    ///
    /// # Intended call sites
    ///
    /// To be called from the reader thread / transport pump immediately upon
    /// receiving a raw text frame before JSON parsing:
    /// - `src/protocol/client.rs`: inside `reader_thread` or transport read loop
    pub fn log_incoming(raw: &str) {
        if !log::log_enabled!(target: "camoufox::protocol", log::Level::Trace) {
            return;
        }
        if raw.len() > MAX_PREVIEW_LEN {
            log::trace!(
                target: "camoufox::protocol",
                "RECV {}...",
                truncate_str(raw, MAX_PREVIEW_LEN)
            );
        } else {
            log::trace!(target: "camoufox::protocol", "RECV {}", raw);
        }
    }
}

/// Log an outgoing JSON message at TRACE level if trace logging is enabled.
///
/// See [`ProtocolLogger::log_outgoing`] for details and intended call sites.
#[inline]
pub fn log_outgoing(value: &serde_json::Value) {
    ProtocolLogger::log_outgoing(value);
}

/// Log an incoming raw text frame at TRACE level if trace logging is enabled.
///
/// See [`ProtocolLogger::log_incoming`] for details and intended call sites.
#[inline]
pub fn log_incoming(raw: &str) {
    ProtocolLogger::log_incoming(raw);
}

/// Truncate a JSON value to a human-readable preview string.
///
/// If the serialized form exceeds [`MAX_PREVIEW_LEN`], it is truncated
/// at a UTF-8 character boundary and `"..."` is appended.
fn truncate_json(value: &serde_json::Value) -> String {
    match serde_json::to_string(value) {
        Ok(s) => {
            if s.len() > MAX_PREVIEW_LEN {
                format!("{}...", truncate_str(&s, MAX_PREVIEW_LEN))
            } else {
                s
            }
        }
        Err(_) => "<serialization error>".to_owned(),
    }
}

/// Truncate a string to at most `max_len` bytes, ending at a valid UTF-8 character boundary.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() > max_len {
        let mut cut = max_len;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        &s[..cut]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_json_short() {
        let val = json!({"key": "value"});
        let result = truncate_json(&val);
        assert!(result.len() < MAX_PREVIEW_LEN);
        assert!(!result.ends_with("..."));
    }

    #[test]
    fn truncate_json_long() {
        let long_string = "x".repeat(1000);
        let val = json!({"data": long_string});
        let result = truncate_json(&val);
        assert!(result.len() <= MAX_PREVIEW_LEN + 3); // +3 for "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_json_null() {
        let result = truncate_json(&json!(null));
        assert_eq!(result, "null");
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_char_boundary_emoji() {
        // "🦀" is 4 bytes: [0xF0, 0x9F, 0xA6, 0x80]
        let s = "ab🦀cd"; // "ab" = 2 bytes, "🦀" = 4 bytes (indices 2..6), "cd" = 2 bytes (indices 6..8)
        assert_eq!(truncate_str(s, 2), "ab");
        // Slicing at index 3, 4, or 5 falls inside the emoji.
        // It must safely walk back to index 2 ("ab") without panicking.
        assert_eq!(truncate_str(s, 3), "ab");
        assert_eq!(truncate_str(s, 4), "ab");
        assert_eq!(truncate_str(s, 5), "ab");
        // At index 6, the full emoji is included.
        assert_eq!(truncate_str(s, 6), "ab🦀");
        assert_eq!(truncate_str(s, 7), "ab🦀c");
        assert_eq!(truncate_str(s, 8), "ab🦀cd");
    }

    #[test]
    fn truncate_str_char_boundary_cjk() {
        // '中' is 3 bytes: [0xE4, 0xB8, 0xAD]
        // '文' is 3 bytes: [0xE6, 0x96, 0x87]
        let s = "ab中文"; // "ab" = 2 bytes, '中' = 3 bytes (indices 2..5), '文' = 3 bytes (indices 5..8)
        assert_eq!(truncate_str(s, 2), "ab");
        // Slicing at index 3 or 4 falls inside '中'.
        assert_eq!(truncate_str(s, 3), "ab");
        assert_eq!(truncate_str(s, 4), "ab");
        // At index 5, '中' is complete.
        assert_eq!(truncate_str(s, 5), "ab中");
        // Slicing at index 6 or 7 falls inside '文'.
        assert_eq!(truncate_str(s, 6), "ab中");
        assert_eq!(truncate_str(s, 7), "ab中");
        // At index 8, both characters are complete.
        assert_eq!(truncate_str(s, 8), "ab中文");
    }

    #[test]
    fn truncate_json_char_boundary_emoji_at_max_preview_len() {
        // JSON serialization format: {"data":"<payload>"}
        // Prefix is `{"data":"` which is 9 bytes.
        // We want an emoji (4 bytes) to straddle the 512-byte boundary.
        // If we put 501 'a's, 9 + 501 = 510 bytes before the emoji.
        // The emoji occupies bytes 510..514.
        // The 512-byte cut falls right at byte 512 (middle of the emoji).
        let filler = "a".repeat(501);
        let s = format!("{}🦀extra_text_here", filler);
        let val = json!({ "data": s });
        let serialized = serde_json::to_string(&val).unwrap();
        assert!(serialized.len() > MAX_PREVIEW_LEN);
        // Byte 512 is not a char boundary in `serialized`
        assert!(!serialized.is_char_boundary(MAX_PREVIEW_LEN));

        // This must not panic and must end with "..."
        let preview = truncate_json(&val);
        assert!(preview.ends_with("..."));
        // The truncated content before "..." must be valid UTF-8 and end before the emoji
        let content = &preview[..preview.len() - 3];
        assert!(content.ends_with('a'));
    }

    #[test]
    fn truncate_json_char_boundary_cjk_at_max_preview_len() {
        // Prefix `{"data":"` is 9 bytes.
        // 502 'a's -> 9 + 502 = 511 bytes before CJK char '中' (3 bytes: 511..514).
        // Slicing at 512 cuts inside '中'.
        let filler = "a".repeat(502);
        let s = format!("{}中文_extra_text", filler);
        let val = json!({ "data": s });
        let serialized = serde_json::to_string(&val).unwrap();
        assert!(serialized.len() > MAX_PREVIEW_LEN);
        assert!(!serialized.is_char_boundary(MAX_PREVIEW_LEN));

        // This must not panic
        let preview = truncate_json(&val);
        assert!(preview.ends_with("..."));
        let content = &preview[..preview.len() - 3];
        assert!(content.ends_with('a'));
    }

    // Note: We don't test the actual log output since it requires
    // configuring a logger. The log_* methods are tested implicitly
    // through integration tests.
    #[test]
    fn log_methods_do_not_panic() {
        // These should not panic even without a logger configured.
        ProtocolLogger::log_send("Browser.enable", &json!({}));
        ProtocolLogger::log_receive_response(1, &Ok(json!({"userAgent": "test"})));
        ProtocolLogger::log_receive_response(
            2,
            &Err(ErrorData {
                message: "test error".into(),
                data: None,
            }),
        );
        ProtocolLogger::log_receive_event("Browser.attachedToTarget", &None);
        ProtocolLogger::log_receive_event("Page.navigationStarted", &Some("session-123".into()));
        ProtocolLogger::log_send_raw(1, "Browser.enable", &None);
        ProtocolLogger::log_send_raw(2, "Page.navigate", &Some("session-abc".into()));
        ProtocolLogger::log_lifecycle("connected");
        ProtocolLogger::log_anomaly("unknown session ID");

        // Test hot-path logging functions
        let large_emoji_val = json!({ "data": format!("{}🦀", "x".repeat(600)) });
        ProtocolLogger::log_outgoing(&large_emoji_val);
        log_outgoing(&large_emoji_val);

        let large_cjk_raw = format!("{}中文", "y".repeat(600));
        ProtocolLogger::log_incoming(&large_cjk_raw);
        log_incoming(&large_cjk_raw);
    }
}
