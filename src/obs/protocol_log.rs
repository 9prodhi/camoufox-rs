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
}

/// Truncate a JSON value to a human-readable preview string.
///
/// If the serialized form exceeds [`MAX_PREVIEW_LEN`], it is truncated
/// and `"..."` is appended.
fn truncate_json(value: &serde_json::Value) -> String {
    match serde_json::to_string(value) {
        Ok(s) => {
            if s.len() > MAX_PREVIEW_LEN {
                format!("{}...", &s[..MAX_PREVIEW_LEN])
            } else {
                s
            }
        }
        Err(_) => "<serialization error>".to_owned(),
    }
}

/// Truncate a string to a maximum length.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() > max_len {
        &s[..max_len]
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
    }
}
