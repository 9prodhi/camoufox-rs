use std::collections::HashMap;
use std::sync::mpsc;

use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
use crate::protocol::types::{ErrorData, MessageId};

/// A pending request waiting for its response from the browser.
struct PendingRequest {
    /// The method name (e.g. "Page.navigate") — carried so rejection errors
    /// can report which method was in flight.
    method: String,
    /// One-shot sender: the caller holds the matching `Receiver`.
    sender: mpsc::Sender<Result<serde_json::Value, ProtocolError>>,
}

/// Tracks in-flight requests for a single session and correlates responses
/// back to callers by message ID.
///
/// Each session owns its own `PendingMap`. When a response arrives, the
/// dispatcher looks up the session, then resolves/rejects the matching entry.
///
/// # Concurrency
///
/// `PendingMap` itself is **not** `Sync` — it is designed to be protected by
/// an external `Mutex` (or held exclusively by a single dispatcher thread).
/// The cross-thread boundary is the `mpsc` channel: the `Receiver` lives on
/// the caller's thread.
pub struct PendingMap {
    requests: HashMap<MessageId, PendingRequest>,
}

impl PendingMap {
    /// Create an empty pending map.
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
        }
    }

    /// Register a new in-flight request.
    ///
    /// Returns the `Receiver` end of a one-shot channel. The caller blocks
    /// (or polls) this receiver to get the result when the response arrives.
    ///
    /// # Panics
    ///
    /// Debug-asserts that `id` is not already present (duplicate IDs are a
    /// programming error — the `IdGenerator` guarantees uniqueness).
    pub fn insert(
        &mut self,
        id: MessageId,
        method: String,
    ) -> mpsc::Receiver<Result<serde_json::Value, ProtocolError>> {
        let (tx, rx) = mpsc::channel();
        let prev = self
            .requests
            .insert(id, PendingRequest { method, sender: tx });
        debug_assert!(prev.is_none(), "duplicate pending request ID: {id}");
        rx
    }

    /// Resolve a pending request with a response from the browser.
    ///
    /// On success (`Ok(value)`), the caller receives the result value.
    /// On server error (`Err(ErrorData)`), the caller receives a
    /// `ProtocolError` with kind `Response`.
    ///
    /// Returns `true` if the ID was found and resolved, `false` if the ID
    /// was unknown. Per the protocol spec (Section 14, item 7), unknown
    /// response IDs are silently ignored.
    pub fn resolve(&mut self, id: MessageId, result: Result<serde_json::Value, ErrorData>) -> bool {
        let Some(pending) = self.requests.remove(&id) else {
            return false;
        };

        let outcome = match result {
            Ok(value) => Ok(value),
            Err(error_data) => Err(ProtocolError::response(&pending.method, error_data)),
        };

        // If the receiver has been dropped (caller gave up), we silently
        // discard. This is not an error condition.
        let _ = pending.sender.send(outcome);
        true
    }

    /// Remove a pending entry without sending anything to the receiver.
    ///
    /// Used by the timeout path: when `Session::send_with_timeout` gives up,
    /// it removes the slot so a late-arriving response with this id is
    /// silently dropped (the reader thread calls `resolve` which simply
    /// returns `false` for unknown ids).
    ///
    /// Returns `true` if the id was present, `false` otherwise.
    pub fn remove(&mut self, id: MessageId) -> bool {
        self.requests.remove(&id).is_some()
    }

    /// Resolve a specific pending request with a fully-formed `ProtocolError`.
    ///
    /// Used when the reader needs to surface an out-of-band error to a
    /// blocked caller — e.g. a `Browser.downloadCreated` event indicating
    /// the navigation was diverted into a download flow. Unlike
    /// [`resolve`](Self::resolve) (which only knows how to map server
    /// `ErrorData` to a `Response`-kind error), this lets the caller supply
    /// any error variant.
    ///
    /// Returns `true` if the ID was found and resolved, `false` if unknown.
    pub fn resolve_with_error(&mut self, id: MessageId, error: ProtocolError) -> bool {
        let Some(pending) = self.requests.remove(&id) else {
            return false;
        };
        let _ = pending.sender.send(Err(error));
        true
    }

    /// Reject ALL pending requests in this session.
    ///
    /// Called when:
    /// - A session is disposed (page closed) → `kind = Closed`
    /// - A page crashes → `kind = Crashed`
    /// - The connection drops → `kind = Closed`
    ///
    /// Each pending request receives a `ProtocolError` carrying the original
    /// method name, so callers can produce meaningful error messages.
    pub fn reject_all(&mut self, kind: ProtocolErrorKind) {
        // Drain all entries. `HashMap::drain` gives us owned key-value pairs.
        for (_id, pending) in self.requests.drain() {
            let error = match kind {
                ProtocolErrorKind::Closed => ProtocolError::closed(Some(pending.method)),
                ProtocolErrorKind::Crashed => ProtocolError::crashed(Some(pending.method)),
                _ => ProtocolError {
                    kind,
                    method: Some(pending.method),
                    message: format!("{kind:?}"),
                    data: None,
                    source: None,
                    download_info: None,
                },
            };
            let _ = pending.sender.send(Err(error));
        }
    }

    /// Number of in-flight (unresolved) requests.
    #[inline]
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    /// Returns `true` if there are no in-flight requests.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

impl Default for PendingMap {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insert_and_resolve_success() {
        let mut map = PendingMap::new();
        let rx = map.insert(1, "Page.navigate".into());

        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());

        let resolved = map.resolve(1, Ok(json!({"frameId": "main"})));
        assert!(resolved);
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());

        let result = rx.recv().unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({"frameId": "main"}));
    }

    #[test]
    fn insert_and_resolve_error() {
        let mut map = PendingMap::new();
        let rx = map.insert(2, "Page.evaluate".into());

        let error_data = ErrorData {
            message: "Evaluation failed".into(),
            data: Some("stack trace here".into()),
        };
        let resolved = map.resolve(2, Err(error_data));
        assert!(resolved);

        let result = rx.recv().unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ProtocolErrorKind::Response);
        assert_eq!(err.method.as_deref(), Some("Page.evaluate"));
        assert_eq!(err.message, "Evaluation failed");
        assert_eq!(err.data.as_deref(), Some("stack trace here"));
    }

    #[test]
    fn resolve_unknown_id_returns_false() {
        let mut map = PendingMap::new();

        // No requests registered — resolve should return false
        assert!(!map.resolve(999, Ok(json!({}))));
        assert!(map.is_empty());
    }

    #[test]
    fn resolve_unknown_id_with_existing_entries() {
        let mut map = PendingMap::new();
        let _rx = map.insert(1, "Browser.enable".into());

        // ID 2 doesn't exist
        assert!(!map.resolve(2, Ok(json!({}))));
        // ID 1 should still be pending
        assert_eq!(map.len(), 1);

        // Now resolve the real one
        assert!(map.resolve(1, Ok(json!({}))));
        assert!(map.is_empty());
    }

    #[test]
    fn reject_all_closed() {
        let mut map = PendingMap::new();
        let rx1 = map.insert(1, "Page.navigate".into());
        let rx2 = map.insert(2, "Page.evaluate".into());
        let rx3 = map.insert(3, "Runtime.callFunction".into());

        assert_eq!(map.len(), 3);

        map.reject_all(ProtocolErrorKind::Closed);
        assert!(map.is_empty());

        // All receivers should get Closed errors with their method names
        for (rx, expected_method) in [
            (rx1, "Page.navigate"),
            (rx2, "Page.evaluate"),
            (rx3, "Runtime.callFunction"),
        ] {
            let result = rx.recv().unwrap();
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.kind, ProtocolErrorKind::Closed);
            assert_eq!(err.method.as_deref(), Some(expected_method));
            assert_eq!(err.message, "Session closed");
        }
    }

    #[test]
    fn reject_all_crashed() {
        let mut map = PendingMap::new();
        let rx = map.insert(1, "Page.navigate".into());

        map.reject_all(ProtocolErrorKind::Crashed);
        assert!(map.is_empty());

        let result = rx.recv().unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ProtocolErrorKind::Crashed);
        assert_eq!(err.method.as_deref(), Some("Page.navigate"));
        assert_eq!(err.message, "Page crashed");
    }

    #[test]
    fn reject_all_empty_map_is_noop() {
        let mut map = PendingMap::new();
        map.reject_all(ProtocolErrorKind::Closed);
        assert!(map.is_empty());
    }

    #[test]
    fn resolve_after_receiver_dropped_does_not_panic() {
        let mut map = PendingMap::new();
        let rx = map.insert(1, "Page.navigate".into());

        // Drop the receiver before resolving — simulates caller timeout/cancel
        drop(rx);

        // Should not panic; result is silently discarded
        let resolved = map.resolve(1, Ok(json!({})));
        assert!(resolved);
        assert!(map.is_empty());
    }

    #[test]
    fn multiple_inserts_and_selective_resolve() {
        let mut map = PendingMap::new();
        let rx1 = map.insert(10, "Browser.enable".into());
        let rx2 = map.insert(20, "Page.navigate".into());
        let rx3 = map.insert(30, "Page.evaluate".into());

        assert_eq!(map.len(), 3);

        // Resolve middle one first (out of order — protocol allows this)
        assert!(map.resolve(20, Ok(json!({"navigationId": "nav1"}))));
        assert_eq!(map.len(), 2);
        assert_eq!(
            rx2.recv().unwrap().unwrap(),
            json!({"navigationId": "nav1"})
        );

        // Resolve first
        assert!(map.resolve(10, Ok(json!({}))));
        assert_eq!(map.len(), 1);
        assert_eq!(rx1.recv().unwrap().unwrap(), json!({}));

        // Resolve last
        assert!(map.resolve(30, Ok(json!({"result": 42}))));
        assert!(map.is_empty());
        assert_eq!(rx3.recv().unwrap().unwrap(), json!({"result": 42}));
    }

    #[test]
    fn reject_all_then_insert_works() {
        let mut map = PendingMap::new();
        let rx1 = map.insert(1, "Page.navigate".into());

        map.reject_all(ProtocolErrorKind::Closed);
        assert!(rx1.recv().unwrap().is_err());

        // Map should accept new entries after rejection
        let rx2 = map.insert(2, "Page.evaluate".into());
        assert_eq!(map.len(), 1);
        assert!(map.resolve(2, Ok(json!("result"))));
        assert_eq!(rx2.recv().unwrap().unwrap(), json!("result"));
    }

    #[test]
    fn remove_drops_entry_without_sending() {
        let mut map = PendingMap::new();
        let rx = map.insert(42, "Page.navigate".into());

        assert!(map.remove(42));
        assert!(map.is_empty());

        // A subsequent resolve for the same id is a no-op (returns false).
        // The receiver does not get any message; it disconnects when the
        // sender is dropped along with the PendingRequest above.
        assert!(!map.resolve(42, Ok(json!({}))));
        assert!(rx.recv().is_err());
    }

    #[test]
    fn remove_unknown_id_returns_false() {
        let mut map = PendingMap::new();
        assert!(!map.remove(999));
    }

    #[test]
    fn default_impl() {
        let map = PendingMap::default();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn resolve_with_error_delivers_full_protocol_error() {
        use crate::protocol::errors::DownloadInfo;

        let mut map = PendingMap::new();
        let rx = map.insert(1, "Page.navigate".into());

        let info = DownloadInfo {
            url: "https://example.com/file.pdf".into(),
            frame_id: Some("frame-1".into()),
            download_id: Some("dl-uuid-1".into()),
        };
        let err =
            ProtocolError::navigation_became_download(Some("Page.navigate".into()), info.clone());

        assert!(map.resolve_with_error(1, err));
        assert!(map.is_empty());

        let result = rx.recv().unwrap();
        let received_err = result.unwrap_err();
        assert_eq!(
            received_err.kind,
            ProtocolErrorKind::NavigationBecameDownload
        );
        assert_eq!(received_err.method.as_deref(), Some("Page.navigate"));
        assert_eq!(
            received_err.download_info.as_deref(),
            Some(&info),
            "boxed download_info should deref-equal the original"
        );
    }

    #[test]
    fn resolve_with_error_unknown_id_returns_false() {
        let mut map = PendingMap::new();
        let err = ProtocolError::closed(Some("Page.navigate".into()));
        assert!(!map.resolve_with_error(999, err));
    }
}
