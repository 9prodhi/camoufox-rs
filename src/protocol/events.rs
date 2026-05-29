use std::collections::HashMap;

use crate::protocol::types::EventMessage;

/// Callback type for event handlers.
///
/// Handlers receive an immutable reference to the event message. They run
/// synchronously on the reader thread, so they should be cheap. For heavy
/// processing, handlers should forward the event to a channel.
pub type EventHandler = Box<dyn Fn(&EventMessage) + Send>;

/// A stable identifier for a registered handler, returned by
/// [`EventRouter::on`] and accepted by [`EventRouter::off`].
///
/// IDs are unique across the whole router (monotonic from a single counter),
/// so an `(session_key, method, id)` triple unambiguously identifies one
/// registration even if the same callback shape is registered repeatedly.
pub type HandlerId = u64;

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
    /// Map from `(session_key, method)` to a list of `(id, handler)` pairs.
    /// The wildcard method `"*"` matches all events on that session.
    handlers: HashMap<(String, String), Vec<(HandlerId, EventHandler)>>,

    /// Global catch-all handlers (for logging/debugging).
    global_handlers: Vec<EventHandler>,

    /// Monotonic counter for assigning [`HandlerId`]s.
    next_id: HandlerId,
}

impl EventRouter {
    /// Create a new, empty event router.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            global_handlers: Vec::new(),
            next_id: 0,
        }
    }

    /// Subscribe to a specific event type on a session.
    ///
    /// - `session_key`: `""` for root session, UUID string for page sessions.
    /// - `method`: the event method name, e.g. `"Page.navigationStarted"`.
    /// - `handler`: callback invoked when a matching event is dispatched.
    ///
    /// Returns a [`HandlerId`] that can be passed to [`off`](Self::off) to
    /// deregister exactly this handler. Callers that subscribe for the
    /// duration of a single operation (e.g. waiting for one lifecycle event)
    /// MUST call `off` when done to avoid leaking the closure.
    pub fn on(&mut self, session_key: &str, method: &str, handler: EventHandler) -> HandlerId {
        let id = self.next_id;
        self.next_id += 1;
        self.handlers
            .entry((session_key.to_owned(), method.to_owned()))
            .or_default()
            .push((id, handler));
        id
    }

    /// Deregister a handler previously registered via [`on`](Self::on).
    ///
    /// Removes the `(session_key, method)` handler whose id matches `id`. If no
    /// such handler exists (already removed, or the session was disposed), this
    /// is a no-op. The `(session_key, method)` map entry is pruned when its last
    /// handler is removed so empty Vecs do not accumulate.
    pub fn off(&mut self, session_key: &str, method: &str, id: HandlerId) {
        let key = (session_key.to_owned(), method.to_owned());
        if let Some(handlers) = self.handlers.get_mut(&key) {
            handlers.retain(|(hid, _)| *hid != id);
            if handlers.is_empty() {
                self.handlers.remove(&key);
            }
        }
    }

    /// Subscribe to ALL events on a session (wildcard).
    ///
    /// The handler fires for every event whose `session_id` matches, regardless
    /// of the event's `method` name. Returns the [`HandlerId`]; deregister with
    /// `off(session_key, "*", id)`.
    pub fn on_any(&mut self, session_key: &str, handler: EventHandler) -> HandlerId {
        self.on(session_key, "*", handler)
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
        if let Some(handlers) = self
            .handlers
            .get(&(session_key.clone(), event.method.clone()))
        {
            for (_id, handler) in handlers {
                handler(event);
            }
        }

        // 2. Session wildcard: (session_key, "*")
        if let Some(handlers) = self.handlers.get(&(session_key, "*".to_owned())) {
            for (_id, handler) in handlers {
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

    /// Returns the number of handlers registered for one `(session_key, method)`
    /// key. Test-only; used by leak-regression guards.
    #[cfg(test)]
    pub fn handler_count_for(&self, session_key: &str, method: &str) -> usize {
        self.handlers
            .get(&(session_key.to_owned(), method.to_owned()))
            .map(|v| v.len())
            .unwrap_or(0)
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
        router.on(
            "",
            "Browser.attachedToTarget",
            Box::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

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
        router.on_any(
            "session-1",
            Box::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

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
        router.on(
            "",
            "Browser.attachedToTarget",
            Box::new(move |_| {
                o1.lock().unwrap().push("exact");
            }),
        );

        let o2 = order.clone();
        router.on_any(
            "",
            Box::new(move |_| {
                o2.lock().unwrap().push("wildcard");
            }),
        );

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

        router.on(
            "s1",
            "Page.navigationStarted",
            Box::new(move |_| {
                c1.fetch_add(1, Ordering::SeqCst);
            }),
        );
        router.on_any(
            "s1",
            Box::new(move |_| {
                c2.fetch_add(1, Ordering::SeqCst);
            }),
        );

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

        router.on(
            "s1",
            "Page.load",
            Box::new(move |_| {
                c1.fetch_add(1, Ordering::SeqCst);
            }),
        );
        router.on(
            "s2",
            "Page.load",
            Box::new(move |_| {
                c2.fetch_add(1, Ordering::SeqCst);
            }),
        );

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

        router.on(
            "",
            "Browser.attachedToTarget",
            Box::new(move |_| {
                c1.fetch_add(1, Ordering::SeqCst);
            }),
        );
        router.on(
            "",
            "Browser.attachedToTarget",
            Box::new(move |_| {
                c2.fetch_add(10, Ordering::SeqCst);
            }),
        );

        router.dispatch(&make_event("Browser.attachedToTarget", None));
        assert_eq!(count.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn handler_receives_event_params() {
        let mut router = EventRouter::new();
        let received = Arc::new(std::sync::Mutex::new(None));
        let r = received.clone();
        router.on(
            "",
            "Browser.attachedToTarget",
            Box::new(move |event| {
                *r.lock().unwrap() = Some(event.params.clone());
            }),
        );

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

    // -----------------------------------------------------------------------
    // Deregistration (off) tests — leak-prevention foundation for wait_for_*.
    // -----------------------------------------------------------------------

    #[test]
    fn on_returns_unique_ids() {
        let mut router = EventRouter::new();
        let id1 = router.on("s1", "Page.eventFired", Box::new(|_| {}));
        let id2 = router.on("s1", "Page.eventFired", Box::new(|_| {}));
        let id3 = router.on("s2", "Page.eventFired", Box::new(|_| {}));
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn off_removes_only_the_matching_handler() {
        let mut router = EventRouter::new();
        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));
        let ca = count_a.clone();
        let cb = count_b.clone();

        let id_a = router.on(
            "s1",
            "Page.eventFired",
            Box::new(move |_| {
                ca.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let _id_b = router.on(
            "s1",
            "Page.eventFired",
            Box::new(move |_| {
                cb.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 2);

        // Remove only handler A.
        router.off("s1", "Page.eventFired", id_a);
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 1);

        router.dispatch(&make_event("Page.eventFired", Some("s1")));
        assert_eq!(count_a.load(Ordering::SeqCst), 0, "A must not fire");
        assert_eq!(count_b.load(Ordering::SeqCst), 1, "B must still fire");
    }

    #[test]
    fn off_prunes_empty_key_entry() {
        let mut router = EventRouter::new();
        let id = router.on("s1", "Page.eventFired", Box::new(|_| {}));
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 1);

        router.off("s1", "Page.eventFired", id);
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 0);
        // The whole map entry should be gone (handler_count counts all Vecs).
        assert_eq!(router.handler_count(), 0);
    }

    #[test]
    fn off_unknown_id_is_noop() {
        let mut router = EventRouter::new();
        let _id = router.on("s1", "Page.eventFired", Box::new(|_| {}));
        // Removing an id that was never issued does nothing.
        router.off("s1", "Page.eventFired", 99999);
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 1);
        // Removing on an unknown key is also a no-op.
        router.off("nonexistent", "Page.eventFired", 0);
        assert_eq!(router.handler_count_for("s1", "Page.eventFired"), 1);
    }

    #[test]
    fn repeated_on_off_does_not_grow_handler_count() {
        // Leak-regression guard: register + deregister N times on the same
        // (session_key, method) must leave the count at 0 — proving the
        // wait_for_lifecycle pattern does not accumulate dead closures.
        let mut router = EventRouter::new();
        for _ in 0..5 {
            let id = router.on("page-A", "Page.eventFired", Box::new(|_| {}));
            assert_eq!(router.handler_count_for("page-A", "Page.eventFired"), 1);
            router.off("page-A", "Page.eventFired", id);
            assert_eq!(router.handler_count_for("page-A", "Page.eventFired"), 0);
        }
        assert_eq!(router.handler_count(), 0);
    }
}
