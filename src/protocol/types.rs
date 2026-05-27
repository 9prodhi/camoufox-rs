use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire-level message types (Section 1 of PROTOCOL.md)
// ---------------------------------------------------------------------------

/// Monotonically increasing message ID, globally unique across all sessions.
pub type MessageId = i64;

/// Session identifier. Root session = None (absent on wire). Page session = Some(uuid).
pub type SessionId = Option<String>;

/// Special message ID for Browser.close graceful shutdown.
pub const BROWSER_CLOSE_MESSAGE_ID: MessageId = -9999;

/// An outgoing request (Client → Browser).
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: MessageId,
    pub method: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    /// Root session: None (field absent). Page session: Some(uuid).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
}

/// An incoming message from the browser. Could be a response or an event.
/// We parse into this raw form first, then discriminate.
#[derive(Debug, Clone, Deserialize)]
pub struct RawMessage {
    #[serde(default)]
    pub id: Option<MessageId>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorData>,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

/// The error object inside an error response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorData {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Discriminated message type after parsing a RawMessage.
#[derive(Debug, Clone)]
pub enum IncomingMessage {
    /// A response to a prior request (has `id`).
    Response(ResponseMessage),
    /// An unsolicited event (has `method`, no `id`).
    Event(EventMessage),
}

/// A response to a previously sent request.
#[derive(Debug, Clone)]
pub struct ResponseMessage {
    pub id: MessageId,
    pub session_id: SessionId,
    pub result: Result<serde_json::Value, ErrorData>,
}

/// An unsolicited event from the browser.
#[derive(Debug, Clone)]
pub struct EventMessage {
    pub method: String,
    pub params: serde_json::Value,
    pub session_id: SessionId,
}

impl RawMessage {
    /// Discriminate: if `id` is present and non-zero → Response, else → Event.
    /// id=0 is never used (IDs start at 1). id=-9999 handled before this.
    pub fn classify(self) -> Option<IncomingMessage> {
        if let Some(id) = self.id {
            if id == 0 {
                // Truthy check: 0 is falsy in JS, treat as event
                return self.classify_as_event();
            }
            let result = if let Some(err) = self.error {
                Err(err)
            } else {
                Ok(self.result.unwrap_or(serde_json::Value::Null))
            };
            Some(IncomingMessage::Response(ResponseMessage {
                id,
                session_id: self.session_id,
                result,
            }))
        } else {
            self.classify_as_event()
        }
    }

    fn classify_as_event(self) -> Option<IncomingMessage> {
        let method = self.method?;
        Some(IncomingMessage::Event(EventMessage {
            method,
            params: self
                .params
                .unwrap_or(serde_json::Value::Object(Default::default())),
            session_id: self.session_id,
        }))
    }
}
