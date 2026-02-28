use std::collections::HashMap;

use crate::protocol::types::EventMessage;

/// Callback type for event handlers.
///
/// Handlers receive an immutable reference to the event message. They run
/// synchronously on the reader thread, so they should be cheap. For heavy
/// processing, handlers should forward the event to a channel.
pub type EventHandler = Box<dyn Fn(&EventMessage) + Send>;

/// Manages event subscriptions per session.
///
/// Events are keyed by `(session_key, method)`. The `session_key` is `""` for
/// the root (browser-level) session and a UUID string for page sessions.
///
/// There are three subscription levels:
/// - **Specific**: `(session_key, method)` — matches one event type on one session.
/// - **Session-wide**: `(session_key, "*")` — matches all events on one session.
/// - **Global**: catches every event regardless of session or method.
pub struct EventRouter {
    /// Map from `(session_key, method)` to a list of handlers.
    /// The wildcard method `"*"` matches all events on that session.
    handlers: HashMap<(String, String), Vec<EventHandler>>,

    /// Global catch-all handlers (for logging/debugging).
    global_handlers: Vec<EventHandler>,
}

impl EventRouter {
    /// Create a new, empty event router.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            global_handlers: Vec::new(),
        }
    }

    /// Subscribe to a specific event type on a session.
    ///
    /// - `session_key`: `""` for root session, UUID string for page sessions.
    /// - `method`: the event method name, e.g. `"Page.navigationStarted"`.
    /// - `handler`: callback invoked when a matching event is dispatched.
    pub fn on(&mut self, session_key: &str, method: &str, handler: EventHandler) {
        self.handlers
            .entry((session_key.to_owned(), method.to_owned()))
            .or_default()
            .push(handler);
    }

    /// Subscribe to ALL events on a session (wildcard).
    ///
    /// The handler fires for every event whose `session_id` matches, regardless
    /// of the event's `method` name.
    pub fn on_any(&mut self, session_key: &str, handler: EventHandler) {
        self.on(session_key, "*", handler);
    }

    /// Add a global event listener that fires for every single event.
    ///
    /// Global handlers run *after* session-specific and session-wildcard
    /// handlers. They are mainly useful for logging and debugging.
    pub fn on_global(&mut self, handler: EventHandler) {
        self.global_handlers.push(handler);
    }

    /// Dispatch an event to all matching handlers.
    ///
    /// The dispatch order is:
    /// 1. Exact-match handlers for `(session_key, method)`.
    /// 2. Session-wildcard handlers for `(session_key, "*")`.
    /// 3. Global handlers.
    ///
    /// Handlers that panic are not caught here; callers (the reader thread)
    /// should wrap dispatch in `catch_unwind` if resilience is needed.
    pub fn dispatch(&self, event: &EventMessage) {
        let session_key = session_key_from_event(event);

        // 1. Exact match: (session_key, method)
        if let Some(handlers) = self.handlers.get(&(session_key.clone(), event.method.clone())) {
            for handler in handlers {
                handler(event);
            }
        }

        // 2. Session wildcard: (session_key, "*")
        if let Some(handlers) = self.handlers.get(&(session_key, "*".to_owned())) {
            for handler in handlers {
                handler(event);
            }
        }

        // 3. Global catch-all handlers
        for handler in &self.global_handlers {
            handler(event);
        }
    }

    /// Remove all handlers associated with a session.
    ///
    /// Called when a session is disposed (page closed or detached). This
    /// removes both exact-match and wildcard subscriptions for the session.
    pub fn remove_session(&mut self, session_key: &str) {
        self.handlers
            .retain(|(key, _method), _handlers| key != session_key);
    }

    /// Returns the total number of handler registrations (all keys + globals).
    #[cfg(test)]
    fn handler_count(&self) -> usize {
        let specific: usize = self.handlers.values().map(|v| v.len()).sum();
        specific + self.global_handlers.len()
    }
}

impl Default for EventRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the session key string from an event message.
///
/// Root session events have `session_id: None` → key `""`.
/// Page session events have `session_id: Some(uuid)` → key `uuid`.
fn session_key_from_event(event: &EventMessage) -> String {
    match &event.session_id {
        Some(id) => id.clone(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn make_event(method: &str, session_id: Option<&str>) -> EventMessage {
        EventMessage {
            method: method.to_owned(),
            params: json!({}),
            session_id: session_id.map(|s| s.to_owned()),
        }
    }

    #[test]
    fn exact_match_fires() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        router.on("", "Browser.attachedToTarget", Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        router.dispatch(&make_event("Browser.attachedToTarget", None));
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Different method should not fire
        router.dispatch(&make_event("Browser.detachedFromTarget", None));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_wildcard_fires_for_all_methods() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        router.on_any("session-1", Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        router.dispatch(&make_event("Page.navigationStarted", Some("session-1")));
        router.dispatch(&make_event("Page.dialogOpened", Some("session-1")));
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Different session should not fire
        router.dispatch(&make_event("Page.navigationStarted", Some("session-2")));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn global_handler_fires_for_everything() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        router.on_global(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        router.dispatch(&make_event("Browser.attachedToTarget", None));
        router.dispatch(&make_event("Page.navigationStarted", Some("s1")));
        router.dispatch(&make_event("Runtime.console", Some("s2")));
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn dispatch_order_exact_then_wildcard_then_global() {
        let mut router = EventRouter::new();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        let o1 = order.clone();
        router.on("", "Browser.attachedToTarget", Box::new(move |_| {
            o1.lock().unwrap().push("exact");
        }));

        let o2 = order.clone();
        router.on_any("", Box::new(move |_| {
            o2.lock().unwrap().push("wildcard");
        }));

        let o3 = order.clone();
        router.on_global(Box::new(move |_| {
            o3.lock().unwrap().push("global");
        }));

        router.dispatch(&make_event("Browser.attachedToTarget", None));
        let result = order.lock().unwrap().clone();
        assert_eq!(result, vec!["exact", "wildcard", "global"]);
    }

    #[test]
    fn remove_session_cleans_up_all_handlers() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = count.clone();
        let c2 = count.clone();

        router.on("s1", "Page.navigationStarted", Box::new(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        }));
        router.on_any("s1", Box::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        }));

        // Should have 2 handler entries for s1
        assert_eq!(router.handler_count(), 2);

        router.remove_session("s1");
        assert_eq!(router.handler_count(), 0);

        // Dispatch should not fire anything
        router.dispatch(&make_event("Page.navigationStarted", Some("s1")));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn remove_session_does_not_affect_other_sessions() {
        let mut router = EventRouter::new();
        let count_s1 = Arc::new(AtomicUsize::new(0));
        let count_s2 = Arc::new(AtomicUsize::new(0));
        let c1 = count_s1.clone();
        let c2 = count_s2.clone();

        router.on("s1", "Page.load", Box::new(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        }));
        router.on("s2", "Page.load", Box::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        }));

        router.remove_session("s1");

        router.dispatch(&make_event("Page.load", Some("s1")));
        router.dispatch(&make_event("Page.load", Some("s2")));

        assert_eq!(count_s1.load(Ordering::SeqCst), 0);
        assert_eq!(count_s2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remove_session_does_not_affect_globals() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        router.on_global(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        router.remove_session("s1");

        router.dispatch(&make_event("Page.load", Some("s1")));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn multiple_handlers_on_same_key() {
        let mut router = EventRouter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = count.clone();
        let c2 = count.clone();

        router.on("", "Browser.attachedToTarget", Box::new(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
        }));
        router.on("", "Browser.attachedToTarget", Box::new(move |_| {
            c2.fetch_add(10, Ordering::SeqCst);
        }));

        router.dispatch(&make_event("Browser.attachedToTarget", None));
        assert_eq!(count.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn handler_receives_event_params() {
        let mut router = EventRouter::new();
        let received = Arc::new(std::sync::Mutex::new(None));
        let r = received.clone();
        router.on("", "Browser.attachedToTarget", Box::new(move |event| {
            *r.lock().unwrap() = Some(event.params.clone());
        }));

        let event = EventMessage {
            method: "Browser.attachedToTarget".to_owned(),
            params: json!({"sessionId": "abc", "targetInfo": {}}),
            session_id: None,
        };
        router.dispatch(&event);

        let params = received.lock().unwrap().take().unwrap();
        assert_eq!(params["sessionId"], "abc");
    }

    #[test]
    fn empty_router_dispatch_is_noop() {
        let router = EventRouter::new();
        // Should not panic
        router.dispatch(&make_event("Browser.attachedToTarget", None));
    }

    #[test]
    fn default_impl() {
        let router = EventRouter::default();
        assert_eq!(router.handler_count(), 0);
    }
}
