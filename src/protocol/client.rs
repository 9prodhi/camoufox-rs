use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::protocol::errors::{DownloadInfo, ProtocolError, ProtocolErrorKind};
use crate::protocol::events::{EventHandler, EventRouter, HandlerId};
use crate::protocol::pending::PendingMap;
use crate::protocol::state::{ConnectionState, IdGenerator, SessionState};
use crate::protocol::types::{
    EventMessage, IncomingMessage, MessageId, Request, ResponseMessage, SessionId,
    BROWSER_CLOSE_MESSAGE_ID,
};
use crate::transport::Transport;

/// Default deadline applied to `Session::send` so no protocol call can hang
/// forever. Individual callers that need a tighter (or looser) bound should
/// use [`Session::send_with_timeout`] explicitly.
pub const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Internal shared state
// ---------------------------------------------------------------------------

/// Shared mutable state protected by a `Mutex`.
struct ConnectionInner {
    state: ConnectionState,
    id_gen: IdGenerator,
    /// Session map. Key is `""` for the root session, UUID for page sessions.
    sessions: HashMap<String, SessionInner>,
    /// Event subscription router.
    events: EventRouter,
    /// Active `Page.navigate` requests indexed by `frameId`.
    ///
    /// When a `Browser.downloadCreated` event arrives, this map lets the
    /// reader thread find the matching pending navigation and surface a
    /// [`ProtocolErrorKind::NavigationBecameDownload`] error to the
    /// blocked `Session::send` caller — without it, the renderer never
    /// sends a `Page.navigate` response (it diverted into a download flow)
    /// and the caller would park forever.
    pending_navs: HashMap<String, PendingNav>,
}

/// A `Page.navigate` request that is currently in flight, recorded by
/// `frameId` for cross-referencing with `Browser.downloadCreated`.
#[derive(Debug, Clone)]
struct PendingNav {
    session_key: String,
    request_id: MessageId,
}

/// Per-session state and pending request map.
struct SessionInner {
    state: SessionState,
    pending: PendingMap,
}

impl SessionInner {
    fn new() -> Self {
        Self {
            state: SessionState::Active,
            pending: PendingMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Split transport
// ---------------------------------------------------------------------------

/// Enables concurrent read and write access to a `Transport`.
///
/// The `Transport` trait uses `&mut self` for both `send()` and `receive()`.
/// For pipe-based transports, reads and writes go to separate file
/// descriptors (fd 3 write, fd 4 read) and can safely happen concurrently.
///
/// `SplitTransport` uses `UnsafeCell` to allow the reader thread to call
/// `receive()` while caller threads call `send()` (serialized by a Mutex).
///
/// # Safety
///
/// Sound when `send()` and `receive()` operate on independent internal state,
/// which is true for pipe transports (separate fds) and for the channel-based
/// mock transport in tests (separate channels).
struct SplitTransport {
    inner: std::cell::UnsafeCell<Box<dyn Transport>>,
}

unsafe impl Send for SplitTransport {}
unsafe impl Sync for SplitTransport {}

impl SplitTransport {
    fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            inner: std::cell::UnsafeCell::new(transport),
        }
    }

    /// Read the next message. Only called by the reader thread.
    fn receive(
        &self,
    ) -> Result<crate::protocol::types::RawMessage, crate::transport::errors::TransportError> {
        unsafe { &mut *self.inner.get() }.receive()
    }

    /// Send a message. Caller must serialize access (via Mutex).
    fn send(
        &self,
        message: &serde_json::Value,
    ) -> Result<(), crate::transport::errors::TransportError> {
        unsafe { &mut *self.inner.get() }.send(message)
    }

    /// Close the transport.
    fn close(&self) -> Result<(), crate::transport::errors::TransportError> {
        unsafe { &mut *self.inner.get() }.close()
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A connection to the Juggler protocol endpoint.
///
/// Manages the root session and page sessions. Spawns a single reader thread
/// that processes incoming messages (responses and events). Outgoing messages
/// are sent directly from the caller's thread, serialized by a `Mutex`.
///
/// # Architecture
///
/// - **Reader thread**: loops on `transport.receive()`, classifies each
///   incoming message, resolves pending requests, and dispatches events.
/// - **Write path**: caller threads hold the `write_lock` Mutex, serialize
///   the request to JSON, and call `transport.send()` directly.
///
/// There is no dedicated writer thread. This avoids deadlock issues with
/// channel-based designs and is simpler overall.
pub struct Connection {
    /// Thread-safe shared state.
    inner: Arc<Mutex<ConnectionInner>>,
    /// Handle to the reader thread.
    reader_handle: Option<thread::JoinHandle<()>>,
    /// Shared transport (concurrent read/write via SplitTransport).
    transport: Arc<SplitTransport>,
    /// Mutex to serialize write operations from multiple caller threads.
    write_lock: Arc<Mutex<()>>,
}

impl Connection {
    /// Create a new connection from a transport.
    ///
    /// Spawns the reader thread. The root session (key `""`) is
    /// automatically created.
    pub fn new(transport: Box<dyn Transport>) -> Self {
        let inner = Arc::new(Mutex::new(ConnectionInner {
            state: ConnectionState::Connected,
            id_gen: IdGenerator::new(),
            sessions: HashMap::new(),
            events: EventRouter::new(),
            pending_navs: HashMap::new(),
        }));

        // Create root session
        {
            let mut guard = inner.lock().unwrap();
            guard.sessions.insert(String::new(), SessionInner::new());
        }

        let transport = Arc::new(SplitTransport::new(transport));
        let write_lock = Arc::new(Mutex::new(()));

        // Spawn reader thread
        let inner_r = Arc::clone(&inner);
        let transport_r = Arc::clone(&transport);
        let reader_handle = thread::Builder::new()
            .name("juggler-reader".into())
            .spawn(move || {
                reader_thread(transport_r, inner_r);
            })
            .expect("failed to spawn reader thread");

        Connection {
            inner,
            reader_handle: Some(reader_handle),
            transport,
            write_lock,
        }
    }

    /// Get a handle to the root session for `Browser.*` commands.
    pub fn root_session(&self) -> Session {
        Session {
            session_id: None,
            session_key: String::new(),
            inner: Arc::clone(&self.inner),
            transport: Arc::clone(&self.transport),
            write_lock: Arc::clone(&self.write_lock),
        }
    }

    /// Create a page session.
    ///
    /// Call when `Browser.attachedToTarget` event arrives with the
    /// server-assigned `sessionId`.
    pub fn create_session(&self, session_id: String) -> Session {
        let session_key = session_id.clone();
        {
            let mut guard = self.inner.lock().unwrap();
            guard
                .sessions
                .entry(session_key.clone())
                .or_insert_with(SessionInner::new);
        }
        Session {
            session_id: Some(session_id),
            session_key,
            inner: Arc::clone(&self.inner),
            transport: Arc::clone(&self.transport),
            write_lock: Arc::clone(&self.write_lock),
        }
    }

    /// Dispose a page session.
    ///
    /// Called when `Browser.detachedFromTarget` arrives. Rejects all pending
    /// requests and removes event handlers.
    pub fn dispose_session(&self, session_id: &str) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut session) = guard.sessions.remove(session_id) {
            session.pending.reject_all(ProtocolErrorKind::Closed);
        }
        guard.events.remove_session(session_id);
    }

    /// Subscribe to events on a specific session.
    ///
    /// Returns a [`HandlerId`] that can be passed to [`off_event`](Self::off_event)
    /// to deregister exactly this handler. Callers that subscribe for the
    /// duration of a single operation MUST call `off_event` when done, or the
    /// closure leaks in the router (it stays in the handler Vec forever).
    pub fn on_event(&self, session_key: &str, method: &str, handler: EventHandler) -> HandlerId {
        let mut guard = self.inner.lock().unwrap();
        guard.events.on(session_key, method, handler)
    }

    /// Deregister a handler previously registered with [`on_event`](Self::on_event).
    ///
    /// No-op if the handler was already removed (e.g. by session disposal).
    pub fn off_event(&self, session_key: &str, method: &str, id: HandlerId) {
        let mut guard = self.inner.lock().unwrap();
        guard.events.off(session_key, method, id);
    }

    /// Subscribe to all events on a specific session.
    ///
    /// Returns a [`HandlerId`]; deregister with `off_event(session_key, "*", id)`.
    pub fn on_event_any(&self, session_key: &str, handler: EventHandler) -> HandlerId {
        let mut guard = self.inner.lock().unwrap();
        guard.events.on_any(session_key, handler)
    }

    /// Subscribe to all events globally (for logging/debugging).
    pub fn on_event_global(&self, handler: EventHandler) {
        let mut guard = self.inner.lock().unwrap();
        guard.events.on_global(handler);
    }

    /// Test-only: number of handlers registered for one `(session_key, method)`.
    /// Used by leak-regression guards (e.g. transient lifecycle waits).
    #[cfg(test)]
    pub fn event_handler_count_for(&self, session_key: &str, method: &str) -> usize {
        let guard = self.inner.lock().unwrap();
        guard.events.handler_count_for(session_key, method)
    }

    /// Close the connection gracefully.
    ///
    /// Sends `Browser.close` with id=-9999 directly via the transport
    /// (bypassing session send). After this, call `wait_closed()`.
    pub fn close(&self) -> Result<(), ProtocolError> {
        {
            let mut guard = self.inner.lock().unwrap();
            if guard.state.is_closed() || guard.state == ConnectionState::Closing {
                return Ok(());
            }
            guard.state = ConnectionState::Closing;
        }

        let request = Request {
            id: BROWSER_CLOSE_MESSAGE_ID,
            method: "Browser.close".to_owned(),
            params: serde_json::Value::Object(Default::default()),
            session_id: None,
        };
        let value = serde_json::to_value(&request).map_err(|e| ProtocolError {
            kind: ProtocolErrorKind::Transport,
            method: Some("Browser.close".to_owned()),
            message: format!("failed to serialize Browser.close: {e}"),
            data: None,
            source: Some(Box::new(e)),
            download_info: None,
        })?;

        let _lock = self.write_lock.lock().unwrap();
        self.transport
            .send(&value)
            .map_err(ProtocolError::transport)?;

        Ok(())
    }

    /// Wait for the connection to fully close.
    ///
    /// Force-closes the transport, rejects all pending requests, and joins
    /// the reader thread.
    pub fn wait_closed(&mut self) {
        let _ = self.transport.close();
        close_all_sessions(&self.inner);
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Force-close the transport so the reader thread exits.
        let _ = self.transport.close();
        // Join the reader thread to ensure clean shutdown. This is safe
        // because close() above guarantees the reader will exit promptly
        // (real transports unblock read on fd close; MockTransport uses
        // recv_timeout to check the closed flag).
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Session handle
// ---------------------------------------------------------------------------

/// A handle to a specific session (root or page).
///
/// Cheap to clone (all state is behind `Arc`).
///
/// # Root session
/// - `session_id`: `None` (absent from wire), `session_key`: `""`
///
/// # Page session
/// - `session_id`: `Some(uuid)`, `session_key`: `uuid`
#[derive(Clone)]
pub struct Session {
    session_id: SessionId,
    session_key: String,
    inner: Arc<Mutex<ConnectionInner>>,
    transport: Arc<SplitTransport>,
    write_lock: Arc<Mutex<()>>,
}

impl Session {
    /// Send a protocol method and wait for the response.
    ///
    /// Applies a default deadline of [`DEFAULT_SEND_TIMEOUT`] so no protocol
    /// call can hang forever — even if a renderer never responds (e.g. a
    /// navigation that became a download). For an explicit deadline (e.g.
    /// from `--timeout`), call [`send_with_timeout`](Self::send_with_timeout).
    ///
    /// # Pre-send checks
    ///
    /// If the session is disposed/crashed or the connection is closed,
    /// returns an error immediately without sending.
    pub fn send(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ProtocolError> {
        self.send_with_timeout(method, params, DEFAULT_SEND_TIMEOUT)
    }

    /// Send a protocol method and wait for the response with an explicit
    /// deadline.
    ///
    /// On timeout, the pending slot is removed from the session map before
    /// returning, so a late-arriving response with this id is silently
    /// dropped by the reader thread and the slot is not leaked.
    ///
    /// # Pre-send checks
    ///
    /// If the session is disposed/crashed or the connection is closed,
    /// returns an error immediately without sending.
    pub fn send_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, ProtocolError> {
        let (rx, id, value) = {
            let mut guard = self.inner.lock().unwrap();

            // Pre-send check: connection state
            if guard.state.is_closed() {
                return Err(ProtocolError::closed(Some(method.to_owned())));
            }

            // Pre-send check: session state
            let session = guard
                .sessions
                .get(&self.session_key)
                .ok_or_else(|| ProtocolError::closed(Some(method.to_owned())))?;

            match session.state {
                SessionState::Disposed => {
                    return Err(ProtocolError::closed(Some(method.to_owned())));
                }
                SessionState::Crashed => {
                    return Err(ProtocolError::crashed(Some(method.to_owned())));
                }
                SessionState::Active => {}
            }

            // Allocate ID and register pending
            let id = guard.id_gen.next();
            let session = guard.sessions.get_mut(&self.session_key).unwrap();
            let rx = session.pending.insert(id, method.to_owned());

            // Build and serialize the request
            let request = Request {
                id,
                method: method.to_owned(),
                params,
                session_id: self.session_id.clone(),
            };
            let value = match serde_json::to_value(&request) {
                Ok(v) => v,
                Err(e) => {
                    session.pending.resolve(id, Ok(serde_json::Value::Null));
                    return Err(ProtocolError {
                        kind: ProtocolErrorKind::Transport,
                        method: Some(request.method),
                        message: format!("failed to serialize request: {e}"),
                        data: None,
                        source: Some(Box::new(e)),
                        download_info: None,
                    });
                }
            };

            (rx, id, value)
        };
        // Inner lock released. Now send through the transport with write serialization.
        {
            let _lock = self.write_lock.lock().unwrap();
            if let Err(e) = self.transport.send(&value) {
                // Send failed — drop the pending slot so it does not leak.
                let mut guard = self.inner.lock().unwrap();
                if let Some(session) = guard.sessions.get_mut(&self.session_key) {
                    session.pending.remove(id);
                }
                return Err(ProtocolError::transport(e));
            }
        }

        // Block waiting for the response, bounded by the deadline.
        match rx.recv_timeout(deadline) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                // Free the pending slot so a late response is silently
                // discarded by the reader thread (resolve returns false for
                // unknown ids) and the slot is not leaked.
                let mut guard = self.inner.lock().unwrap();
                if let Some(session) = guard.sessions.get_mut(&self.session_key) {
                    session.pending.remove(id);
                }
                Err(ProtocolError::timeout(method, deadline))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(ProtocolError::closed(Some(method.to_owned())))
            }
        }
    }

    /// Send a `Page.navigate` (or any frame-scoped navigation) with download
    /// detection. Equivalent to calling
    /// [`send_navigate_with_timeout`](Self::send_navigate_with_timeout) with
    /// [`DEFAULT_SEND_TIMEOUT`].
    pub fn send_navigate(
        &self,
        method: &str,
        params: serde_json::Value,
        frame_id: &str,
    ) -> Result<serde_json::Value, ProtocolError> {
        self.send_navigate_with_timeout(method, params, frame_id, DEFAULT_SEND_TIMEOUT)
    }

    /// Send a `Page.navigate` (or any frame-scoped navigation) with both
    /// download detection AND a hard deadline.
    ///
    /// While the request is in flight, the connection records
    /// `(frame_id → request_id)` so that an incoming
    /// `Browser.downloadCreated` event for the same frame can resolve the
    /// pending request with [`ProtocolErrorKind::NavigationBecameDownload`]
    /// instead of letting the caller block forever (the browser never sends
    /// a `Page.navigate` response when it diverts the navigation into a
    /// download flow).
    ///
    /// If `deadline` elapses before either a normal response or a download
    /// event arrives, the pending slot is removed and the call returns
    /// [`ProtocolErrorKind::Timeout`].
    ///
    /// The pending-nav entry is always cleaned up on return — whether the
    /// response was a normal success, an error, a timeout, a close, or a
    /// download-detected resolution.
    pub fn send_navigate_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        frame_id: &str,
        deadline: Duration,
    ) -> Result<serde_json::Value, ProtocolError> {
        // Send-with-registration. We allocate the message ID, register the
        // pending nav under our frame_id, then run the rest of `send`'s
        // logic inline. We can't piggy-back on `send` because we need the
        // allocated ID *before* the request goes out.
        let (rx, value, id) = {
            let mut guard = self.inner.lock().unwrap();

            if guard.state.is_closed() {
                return Err(ProtocolError::closed(Some(method.to_owned())));
            }

            let session = guard
                .sessions
                .get(&self.session_key)
                .ok_or_else(|| ProtocolError::closed(Some(method.to_owned())))?;

            match session.state {
                SessionState::Disposed => {
                    return Err(ProtocolError::closed(Some(method.to_owned())));
                }
                SessionState::Crashed => {
                    return Err(ProtocolError::crashed(Some(method.to_owned())));
                }
                SessionState::Active => {}
            }

            let id = guard.id_gen.next();
            let session = guard.sessions.get_mut(&self.session_key).unwrap();
            let rx = session.pending.insert(id, method.to_owned());

            // Register the pending nav. If a prior nav for the same frame is
            // still tracked, log and overwrite — the older entry's response
            // path will simply not get a download-translated error, but the
            // pending slot itself is independent and will resolve normally.
            if let Some(prev) = guard.pending_navs.get(frame_id) {
                log::debug!(
                    "register_pending_nav: overwriting prior nav for frame {} (was id={})",
                    frame_id,
                    prev.request_id,
                );
            }
            guard.pending_navs.insert(
                frame_id.to_owned(),
                PendingNav {
                    session_key: self.session_key.clone(),
                    request_id: id,
                },
            );

            let request = Request {
                id,
                method: method.to_owned(),
                params,
                session_id: self.session_id.clone(),
            };
            let value = match serde_json::to_value(&request) {
                Ok(v) => v,
                Err(e) => {
                    let session = guard.sessions.get_mut(&self.session_key).unwrap();
                    session.pending.resolve(id, Ok(serde_json::Value::Null));
                    // Drop the pending-nav entry too.
                    if let Some(p) = guard.pending_navs.get(frame_id) {
                        if p.request_id == id {
                            guard.pending_navs.remove(frame_id);
                        }
                    }
                    return Err(ProtocolError {
                        kind: ProtocolErrorKind::Transport,
                        method: Some(request.method),
                        message: format!("failed to serialize request: {e}"),
                        data: None,
                        source: Some(Box::new(e)),
                        download_info: None,
                    });
                }
            };

            (rx, value, id)
        };

        {
            let _lock = self.write_lock.lock().unwrap();
            if let Err(e) = self.transport.send(&value) {
                // Clean up the pending-nav entry on send failure.
                let mut guard = self.inner.lock().unwrap();
                if let Some(p) = guard.pending_navs.get(frame_id) {
                    if p.request_id == id {
                        guard.pending_navs.remove(frame_id);
                    }
                }
                return Err(ProtocolError::transport(e));
            }
        }

        // Block on the response, bounded by the deadline. The reader thread
        // may resolve us with a `NavigationBecameDownload` error if it sees
        // a matching download event before the (never-coming) navigate
        // response.
        let outcome = match rx.recv_timeout(deadline) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                // Free the pending slot so a late response is silently
                // discarded by the reader thread.
                let mut guard = self.inner.lock().unwrap();
                if let Some(session) = guard.sessions.get_mut(&self.session_key) {
                    session.pending.remove(id);
                }
                Err(ProtocolError::timeout(method, deadline))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(ProtocolError::closed(Some(method.to_owned())))
            }
        };

        // Always clear the pending-nav entry, but only if it still points at
        // *our* request (a later navigation for the same frame may have
        // overwritten it).
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(p) = guard.pending_navs.get(frame_id) {
                if p.request_id == id {
                    guard.pending_navs.remove(frame_id);
                }
            }
        }

        outcome
    }

    /// Send a protocol method, swallowing errors (fire-and-forget).
    ///
    /// Matches the `sendMayFail` pattern from Playwright. Inherits the
    /// default timeout from [`Session::send`].
    pub fn send_may_fail(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.send_may_fail_with_timeout(method, params, DEFAULT_SEND_TIMEOUT)
    }

    /// Send a protocol method, swallowing errors, with an explicit deadline.
    pub fn send_may_fail_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Option<serde_json::Value> {
        match self.send_with_timeout(method, params, deadline) {
            Ok(result) => Some(result),
            Err(e) => {
                log::debug!("sendMayFail({method}): {e}");
                None
            }
        }
    }

    /// Mark this session as crashed.
    ///
    /// Rejects all pending requests with `Crashed`. Future sends fail
    /// immediately.
    pub fn mark_crashed(&self) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(session) = guard.sessions.get_mut(&self.session_key) {
            session.state = SessionState::Crashed;
            session.pending.reject_all(ProtocolErrorKind::Crashed);
        }
    }

    /// Returns the session key (`""` for root, UUID for page).
    pub fn key(&self) -> &str {
        &self.session_key
    }

    /// Returns the session ID for wire format (`None` for root).
    pub fn id(&self) -> &SessionId {
        &self.session_id
    }
}

// ---------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------

fn reader_thread(transport: Arc<SplitTransport>, inner: Arc<Mutex<ConnectionInner>>) {
    loop {
        let raw = transport.receive();

        let raw_message = match raw {
            Ok(msg) => msg,
            Err(e) => {
                log::debug!("reader thread: transport error: {e}");
                close_all_sessions(&inner);
                return;
            }
        };

        let incoming = match raw_message.classify() {
            Some(msg) => msg,
            None => {
                log::debug!("reader thread: unclassifiable message, skipping");
                continue;
            }
        };

        match incoming {
            IncomingMessage::Response(response) => handle_response(&inner, response),
            IncomingMessage::Event(event) => handle_event(&inner, event),
        }
    }
}

// ---------------------------------------------------------------------------
// Message handlers
// ---------------------------------------------------------------------------

fn handle_response(inner: &Arc<Mutex<ConnectionInner>>, response: ResponseMessage) {
    if response.id == BROWSER_CLOSE_MESSAGE_ID {
        log::debug!(
            "reader thread: discarding Browser.close response (id={BROWSER_CLOSE_MESSAGE_ID})"
        );
        return;
    }

    let mut guard = inner.lock().unwrap();
    let session_key = response.session_id.as_deref().unwrap_or("");

    if let Some(session) = guard.sessions.get_mut(session_key) {
        session.pending.resolve(response.id, response.result);
    } else {
        log::debug!(
            "reader thread: response for unknown session {:?}, dropping",
            response.session_id
        );
    }
}

fn handle_event(inner: &Arc<Mutex<ConnectionInner>>, event: EventMessage) {
    // Intercept Browser.downloadCreated first: if it matches a pending
    // navigation, resolve that pending request with a structured error so
    // the blocked send_navigate caller unparks. We do this *before*
    // dispatching to user event handlers so the resolution races nothing.
    if event.method == "Browser.downloadCreated" {
        handle_download_created(inner, &event);
    }

    let guard = inner.lock().unwrap();
    guard.events.dispatch(&event);
}

/// Match an incoming `Browser.downloadCreated` event against the pending-nav
/// map and resolve any matching pending request with a
/// [`ProtocolErrorKind::NavigationBecameDownload`] error.
fn handle_download_created(inner: &Arc<Mutex<ConnectionInner>>, event: &EventMessage) {
    let frame_id = event
        .params
        .get("frameId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let url = event
        .params
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_default();
    let download_id = event
        .params
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    let Some(frame_id) = frame_id else {
        log::warn!(
            "Browser.downloadCreated: no frameId in params, cannot match to a pending navigation"
        );
        return;
    };

    let mut guard = inner.lock().unwrap();
    let Some(pending) = guard.pending_navs.remove(&frame_id) else {
        log::warn!(
            "Browser.downloadCreated for frame {} has no matching pending navigation (download_id={:?})",
            frame_id,
            download_id,
        );
        return;
    };

    let info = DownloadInfo {
        url,
        frame_id: Some(frame_id),
        download_id,
    };
    let err = ProtocolError::navigation_became_download(Some("Page.navigate".to_owned()), info);

    let Some(session) = guard.sessions.get_mut(&pending.session_key) else {
        log::debug!(
            "downloadCreated resolution: session {} disappeared",
            pending.session_key
        );
        return;
    };

    // Reject the matching pending request slot. We can't go through
    // `PendingMap::resolve` (which only takes `Result<Value, ErrorData>`),
    // so we inject the error directly via reject_one — but PendingMap has no
    // such method. Instead we remove-and-send via a tiny helper here.
    session.pending.resolve_with_error(pending.request_id, err);
}

fn close_all_sessions(inner: &Arc<Mutex<ConnectionInner>>) {
    let mut guard = inner.lock().unwrap();
    guard.state = ConnectionState::Closed;
    for (_key, session) in guard.sessions.iter_mut() {
        session.state = SessionState::Disposed;
        session.pending.reject_all(ProtocolErrorKind::Closed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{MessageId, RawMessage};
    use crate::transport::errors::TransportError;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // Mock transport
    // -----------------------------------------------------------------------

    /// Mock transport using independent channels for read and write.
    /// `receive()` blocks on `incoming_rx.recv()`.
    /// `send()` pushes to `outgoing_tx`.
    /// Both are independent, satisfying SplitTransport's safety invariant.
    struct MockTransport {
        incoming_rx: mpsc::Receiver<RawMessage>,
        outgoing_tx: mpsc::Sender<serde_json::Value>,
        closed: Arc<AtomicBool>,
    }

    impl MockTransport {
        fn new() -> (
            Self,
            mpsc::Sender<RawMessage>,
            mpsc::Receiver<serde_json::Value>,
            Arc<AtomicBool>,
        ) {
            let (in_tx, in_rx) = mpsc::channel();
            let (out_tx, out_rx) = mpsc::channel();
            let closed = Arc::new(AtomicBool::new(false));
            (
                Self {
                    incoming_rx: in_rx,
                    outgoing_tx: out_tx,
                    closed: Arc::clone(&closed),
                },
                in_tx,
                out_rx,
                closed,
            )
        }
    }

    impl Transport for MockTransport {
        fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            self.outgoing_tx
                .send(message.clone())
                .map_err(|_| TransportError::Closed)
        }

        fn receive(&mut self) -> Result<RawMessage, TransportError> {
            // Use recv_timeout so that transport.close() (which sets the
            // closed flag) can unblock this loop promptly. Without this,
            // Connection::drop would hang waiting for the reader thread
            // when tests run in parallel and don't explicitly call teardown.
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(TransportError::Closed);
                }
                match self
                    .incoming_rx
                    .recv_timeout(std::time::Duration::from_millis(10))
                {
                    Ok(msg) => return Ok(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(TransportError::Closed)
                    }
                }
            }
        }

        fn close(&mut self) -> Result<(), TransportError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    struct TestHarness {
        conn: Connection,
        in_tx: mpsc::Sender<RawMessage>,
        out_rx: mpsc::Receiver<serde_json::Value>,
        #[allow(dead_code)]
        closed: Arc<AtomicBool>,
    }

    fn setup() -> TestHarness {
        let (transport, in_tx, out_rx, closed) = MockTransport::new();
        let conn = Connection::new(Box::new(transport));
        TestHarness {
            conn,
            in_tx,
            out_rx,
            closed,
        }
    }

    fn success_response(id: MessageId, session_id: Option<&str>) -> RawMessage {
        RawMessage {
            id: Some(id),
            method: None,
            params: None,
            result: Some(json!({"ok": true})),
            error: None,
            session_id: session_id.map(String::from),
        }
    }

    fn error_response(id: MessageId, session_id: Option<&str>, msg: &str) -> RawMessage {
        RawMessage {
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(crate::protocol::types::ErrorData {
                message: msg.to_owned(),
                data: None,
            }),
            session_id: session_id.map(String::from),
        }
    }

    fn event_message(method: &str, session_id: Option<&str>) -> RawMessage {
        RawMessage {
            id: None,
            method: Some(method.to_owned()),
            params: Some(json!({"data": "test"})),
            result: None,
            error: None,
            session_id: session_id.map(String::from),
        }
    }

    fn recv_out(rx: &mpsc::Receiver<serde_json::Value>) -> serde_json::Value {
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("timed out waiting for outgoing message")
    }

    /// Clean teardown: drop in_tx to unblock reader, then wait.
    fn teardown(mut h: TestHarness) {
        drop(h.in_tx);
        h.conn.wait_closed();
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn root_session_send_and_receive() {
        let h = setup();
        let session = h.conn.root_session();

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            assert_eq!(sent["method"], "Browser.enable");
            assert!(sent.get("sessionId").is_none());
            in_tx.send(success_response(id, None)).unwrap();
        });

        let result = session.send("Browser.enable", json!({}));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({"ok": true}));

        responder.join().unwrap();
    }

    #[test]
    fn page_session_send_and_receive() {
        let h = setup();
        let page = h.conn.create_session("page-uuid-1".to_owned());

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            assert_eq!(sent["method"], "Page.navigate");
            assert_eq!(sent["sessionId"], "page-uuid-1");
            in_tx
                .send(success_response(id, Some("page-uuid-1")))
                .unwrap();
        });

        let result = page.send("Page.navigate", json!({"url": "https://example.com"}));
        assert!(result.is_ok());

        responder.join().unwrap();
    }

    #[test]
    fn send_returns_protocol_error_on_server_error() {
        let h = setup();
        let session = h.conn.root_session();

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            in_tx
                .send(error_response(id, None, "Method not found"))
                .unwrap();
        });

        let result = session.send("Browser.nonexistent", json!({}));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ProtocolErrorKind::Response);
        assert!(err.message.contains("Method not found"));

        responder.join().unwrap();
    }

    #[test]
    fn event_dispatch_to_handlers() {
        let h = setup();

        let received = Arc::new(AtomicUsize::new(0));
        let r = received.clone();
        h.conn.on_event(
            "",
            "Browser.attachedToTarget",
            Box::new(move |_| {
                r.fetch_add(1, Ordering::SeqCst);
            }),
        );

        h.in_tx
            .send(event_message("Browser.attachedToTarget", None))
            .unwrap();

        thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 1);

        teardown(h);
    }

    #[test]
    fn dispose_session_rejects_pending() {
        let h = setup();
        let page = h.conn.create_session("page-1".to_owned());

        let page2 = page.clone();
        let handle = thread::spawn(move || page2.send("Page.navigate", json!({})));

        let _sent = recv_out(&h.out_rx);

        h.conn.dispose_session("page-1");

        let result = handle.join().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Closed);

        teardown(h);
    }

    #[test]
    fn mark_crashed_rejects_pending_and_future_sends() {
        let h = setup();
        let page = h.conn.create_session("page-1".to_owned());

        let page2 = page.clone();
        let handle = thread::spawn(move || page2.send("Page.evaluate", json!({})));

        let _sent = recv_out(&h.out_rx);

        page.mark_crashed();

        let result = handle.join().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Crashed);

        let result = page.send("Page.navigate", json!({}));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Crashed);

        teardown(h);
    }

    #[test]
    fn close_sends_browser_close() {
        let h = setup();

        h.conn.close().unwrap();

        let sent = recv_out(&h.out_rx);
        assert_eq!(sent["id"], BROWSER_CLOSE_MESSAGE_ID);
        assert_eq!(sent["method"], "Browser.close");

        teardown(h);
    }

    #[test]
    fn browser_close_response_is_silently_discarded() {
        let h = setup();

        h.in_tx
            .send(success_response(BROWSER_CLOSE_MESSAGE_ID, None))
            .unwrap();

        thread::sleep(std::time::Duration::from_millis(100));

        {
            let guard = h.conn.inner.lock().unwrap();
            assert_eq!(guard.state, ConnectionState::Connected);
        }

        teardown(h);
    }

    #[test]
    fn transport_error_closes_all_sessions() {
        let mut h = setup();
        let root = h.conn.root_session();
        let page = h.conn.create_session("page-1".to_owned());

        let root2 = root.clone();
        let h1 = thread::spawn(move || root2.send("Browser.getInfo", json!({})));
        let page2 = page.clone();
        let h2 = thread::spawn(move || page2.send("Page.navigate", json!({})));

        let _s1 = recv_out(&h.out_rx);
        let _s2 = recv_out(&h.out_rx);

        // Drop the incoming channel -- simulates transport EOF.
        drop(h.in_tx);

        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert!(r1.is_err());
        assert!(r2.is_err());
        assert_eq!(r1.unwrap_err().kind, ProtocolErrorKind::Closed);
        assert_eq!(r2.unwrap_err().kind, ProtocolErrorKind::Closed);

        h.conn.wait_closed();
    }

    #[test]
    fn send_on_closed_connection_returns_error() {
        let mut h = setup();
        let root = h.conn.root_session();

        drop(h.in_tx);
        thread::sleep(std::time::Duration::from_millis(100));

        let result = root.send("Browser.enable", json!({}));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Closed);

        h.conn.wait_closed();
    }

    #[test]
    fn send_may_fail_returns_none_on_error() {
        let mut h = setup();
        let root = h.conn.root_session();

        drop(h.in_tx);
        thread::sleep(std::time::Duration::from_millis(100));

        let result = root.send_may_fail("Browser.enable", json!({}));
        assert!(result.is_none());

        h.conn.wait_closed();
    }

    #[test]
    fn id_generator_is_monotonic() {
        let gen = IdGenerator::new();
        assert_eq!(gen.next(), 1);
        assert_eq!(gen.next(), 2);
        assert_eq!(gen.next(), 3);
    }

    #[test]
    fn multiple_sends_get_different_ids() {
        let h = setup();
        let session = h.conn.root_session();

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let s1 = recv_out(&h.out_rx);
            let s2 = recv_out(&h.out_rx);
            let id1 = s1["id"].as_i64().unwrap();
            let id2 = s2["id"].as_i64().unwrap();
            assert_ne!(id1, id2);
            assert!(id1 > 0);
            assert!(id2 > 0);
            in_tx.send(success_response(id1, None)).unwrap();
            in_tx.send(success_response(id2, None)).unwrap();
        });

        let s2 = session.clone();
        let h1 = thread::spawn(move || session.send("Browser.enable", json!({})));
        let h2 = thread::spawn(move || s2.send("Browser.getInfo", json!({})));

        assert!(h1.join().unwrap().is_ok());
        assert!(h2.join().unwrap().is_ok());
        responder.join().unwrap();
    }

    #[test]
    fn unknown_session_response_is_silently_dropped() {
        let h = setup();

        h.in_tx
            .send(success_response(1, Some("nonexistent-session")))
            .unwrap();

        thread::sleep(std::time::Duration::from_millis(100));

        {
            let guard = h.conn.inner.lock().unwrap();
            assert_eq!(guard.state, ConnectionState::Connected);
        }

        teardown(h);
    }

    #[test]
    fn close_is_idempotent() {
        let h = setup();

        assert!(h.conn.close().is_ok());
        assert!(h.conn.close().is_ok());

        teardown(h);
    }

    #[test]
    fn session_key_and_id() {
        let h = setup();

        let root = h.conn.root_session();
        assert_eq!(root.key(), "");
        assert_eq!(root.id(), &None);

        let page = h.conn.create_session("uuid-123".to_owned());
        assert_eq!(page.key(), "uuid-123");
        assert_eq!(page.id(), &Some("uuid-123".to_owned()));

        teardown(h);
    }

    // -----------------------------------------------------------------------
    // Download-detection tests (fix/download-detection)
    // -----------------------------------------------------------------------
    //
    // These tests cover the path that fires when a `Page.navigate` is
    // diverted into a download flow by the renderer. The browser sends
    // `Browser.downloadCreated` instead of a `Page.navigate` response, and
    // the reader thread must unblock the waiting send_navigate caller with
    // a NavigationBecameDownload error.

    fn download_created_event(
        frame_id: &str,
        url: &str,
        uuid: &str,
        session_id: Option<&str>,
    ) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Browser.downloadCreated".into()),
            params: Some(json!({
                "uuid": uuid,
                "frameId": frame_id,
                "url": url,
                "suggestedFileName": "file.pdf",
                "pageTargetId": "target-1",
            })),
            result: None,
            error: None,
            session_id: session_id.map(String::from),
        }
    }

    #[test]
    fn download_created_resolves_matching_pending_nav() {
        let h = setup();
        let page = h.conn.create_session("page-A".to_owned());

        // Spawn the navigate call in a thread; it must block waiting for a
        // response that will never come (we simulate that by NOT sending a
        // matching Page.navigate response from the responder).
        let page2 = page.clone();
        let handle = thread::spawn(move || {
            page2.send_navigate(
                "Page.navigate",
                json!({"url": "https://example.com/file.pdf", "frameId": "frame-1"}),
                "frame-1",
            )
        });

        // Consume the outgoing Page.navigate so the reader thread can see
        // an in-flight request id, then inject a downloadCreated event for
        // the same frame.
        let sent = recv_out(&h.out_rx);
        assert_eq!(sent["method"], "Page.navigate");

        h.in_tx
            .send(download_created_event(
                "frame-1",
                "https://example.com/file.pdf",
                "dl-uuid-1",
                None,
            ))
            .unwrap();

        let result = handle.join().unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ProtocolErrorKind::NavigationBecameDownload);
        let info = err
            .download_info
            .expect("download_info populated for NavigationBecameDownload");
        assert_eq!(info.url, "https://example.com/file.pdf");
        assert_eq!(info.frame_id.as_deref(), Some("frame-1"));
        assert_eq!(info.download_id.as_deref(), Some("dl-uuid-1"));

        teardown(h);
    }

    #[test]
    fn download_created_with_no_pending_nav_is_ignored() {
        // No pending navigation: the reader thread should log a warning
        // and otherwise leave the connection untouched. No panics, no
        // spurious resolution. We just verify the connection stays usable.
        let h = setup();

        h.in_tx
            .send(download_created_event(
                "frame-orphan",
                "https://example.com/orphan.pdf",
                "dl-uuid-orphan",
                None,
            ))
            .unwrap();

        // Give the reader a moment to process.
        thread::sleep(std::time::Duration::from_millis(100));

        // Connection should still be Connected and usable for a normal send.
        let session = h.conn.root_session();
        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            in_tx.send(success_response(id, None)).unwrap();
        });

        let result = session.send("Browser.enable", json!({}));
        assert!(result.is_ok());

        responder.join().unwrap();
    }

    #[test]
    fn download_created_for_other_frame_does_not_resolve_pending_nav() {
        // Two pending navigations on different frames: a downloadCreated
        // event for frame A must only resolve frame A's nav, never B's.
        let h = setup();
        let page_a = h.conn.create_session("page-A".to_owned());
        let page_b = h.conn.create_session("page-B".to_owned());

        // Start nav on frame-A
        let pa = page_a.clone();
        let handle_a = thread::spawn(move || {
            pa.send_navigate(
                "Page.navigate",
                json!({"url": "https://a.example/", "frameId": "frame-A"}),
                "frame-A",
            )
        });
        let sent_a = recv_out(&h.out_rx);
        let id_a = sent_a["id"].as_i64().unwrap();
        assert_eq!(sent_a["sessionId"], "page-A");

        // Start nav on frame-B
        let pb = page_b.clone();
        let handle_b = thread::spawn(move || {
            pb.send_navigate(
                "Page.navigate",
                json!({"url": "https://b.example/", "frameId": "frame-B"}),
                "frame-B",
            )
        });
        let sent_b = recv_out(&h.out_rx);
        let id_b = sent_b["id"].as_i64().unwrap();
        assert_eq!(sent_b["sessionId"], "page-B");
        assert_ne!(id_a, id_b);

        // Fire downloadCreated for frame-A only.
        h.in_tx
            .send(download_created_event(
                "frame-A",
                "https://a.example/file",
                "dl-A",
                Some("page-A"),
            ))
            .unwrap();

        // Frame-A's navigate must finish with NavigationBecameDownload.
        let result_a = handle_a.join().unwrap();
        let err_a = result_a.expect_err("frame-A nav must error");
        assert_eq!(err_a.kind, ProtocolErrorKind::NavigationBecameDownload);
        assert_eq!(
            err_a.download_info.as_ref().unwrap().frame_id.as_deref(),
            Some("frame-A")
        );

        // Frame-B's navigate must still be in flight. Resolve it normally
        // and confirm it succeeded — that proves the downloadCreated did
        // not leak into it.
        h.in_tx
            .send(success_response(id_b, Some("page-B")))
            .unwrap();
        let result_b = handle_b.join().unwrap();
        assert!(
            result_b.is_ok(),
            "frame-B nav must succeed, got {result_b:?}"
        );

        teardown(h);
    }

    #[test]
    fn successful_navigate_clears_pending_nav_entry() {
        // After a normal navigate response, the pending-nav entry should be
        // cleared so a later downloadCreated for the same frame is treated
        // as an orphan (logged, ignored) rather than poisoning a future
        // nav.
        let h = setup();
        let page = h.conn.create_session("page-1".to_owned());

        let p = page.clone();
        let handle = thread::spawn(move || {
            p.send_navigate(
                "Page.navigate",
                json!({"url": "https://ok.example/", "frameId": "frame-1"}),
                "frame-1",
            )
        });
        let sent = recv_out(&h.out_rx);
        let id = sent["id"].as_i64().unwrap();
        h.in_tx.send(success_response(id, Some("page-1"))).unwrap();
        let result = handle.join().unwrap();
        assert!(result.is_ok());

        // pending_navs should now be empty.
        {
            let guard = h.conn.inner.lock().unwrap();
            assert!(
                guard.pending_navs.is_empty(),
                "pending_navs should be empty after success"
            );
        }

        teardown(h);
    }

    #[test]
    fn global_event_handler_receives_all_events() {
        let h = setup();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        h.conn.on_event_global(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));

        let _page = h.conn.create_session("p1".to_owned());

        h.in_tx
            .send(event_message("Browser.attachedToTarget", None))
            .unwrap();
        h.in_tx
            .send(event_message("Page.navigationStarted", Some("p1")))
            .unwrap();

        thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(count.load(Ordering::SeqCst), 2);

        teardown(h);
    }

    // -----------------------------------------------------------------------
    // Timeout tests (regression: --timeout was previously dropped, and
    // Client::send blocked on bare rx.recv()).
    // -----------------------------------------------------------------------

    /// `send_with_timeout` returns a `Timeout` error when no response arrives
    /// within the deadline. The pending slot is freed (verified indirectly
    /// by the next test).
    #[test]
    fn send_with_timeout_times_out_when_no_response() {
        let h = setup();
        let session = h.conn.root_session();

        let start = std::time::Instant::now();
        let result = session.send_with_timeout(
            "Browser.enable",
            json!({}),
            std::time::Duration::from_millis(150),
        );
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout, got {:?}", result.ok());
        let err = result.unwrap_err();
        assert_eq!(err.kind, ProtocolErrorKind::Timeout);
        assert_eq!(err.method.as_deref(), Some("Browser.enable"));
        // Bound liberally: must be ≥ deadline, but not absurdly long.
        assert!(
            elapsed >= std::time::Duration::from_millis(140),
            "returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "returned too late: {elapsed:?}"
        );

        // Drain the outgoing request so the channel doesn't back up; we
        // don't inspect it here.
        let _ = h.out_rx.recv_timeout(std::time::Duration::from_secs(1));

        teardown(h);
    }

    /// After a timeout, the pending slot is freed: a subsequent `send` on
    /// the same session works normally (the next id has no leaked predecessor).
    #[test]
    fn timeout_frees_pending_slot_and_session_remains_usable() {
        let mut h = setup();
        let session = h.conn.root_session();

        // First send: times out.
        let result = session.send_with_timeout(
            "Browser.enable",
            json!({}),
            std::time::Duration::from_millis(100),
        );
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Timeout);

        // Drain the first request (id=1).
        let first_sent = h
            .out_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("first request was not actually sent");
        let first_id = first_sent["id"].as_i64().unwrap();

        // Pending map should now be empty.
        {
            let guard = h.conn.inner.lock().unwrap();
            let root = guard.sessions.get("").expect("root session must exist");
            assert!(
                root.pending.is_empty(),
                "pending slot leaked after timeout (len={})",
                root.pending.len()
            );
        }

        // Second send on the same session must succeed.
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx; // moved into the responder
        let responder = thread::spawn(move || {
            let sent = recv_out(&out_rx);
            let id = sent["id"].as_i64().unwrap();
            in_tx.send(success_response(id, None)).unwrap();
        });

        let result = session.send_with_timeout(
            "Browser.getInfo",
            json!({}),
            std::time::Duration::from_secs(5),
        );
        assert!(
            result.is_ok(),
            "second send after timeout failed: {:?}",
            result.err()
        );
        responder.join().unwrap();

        // The two requests should have had distinct ids (no reuse of slot).
        // We can't fetch the second id easily here, but verifying the slot
        // was empty between the two sends (above) is sufficient.
        let _ = first_id;

        drop(h.in_tx);
        h.conn.wait_closed();
    }

    /// A response that arrives AFTER the timeout fires for that id is
    /// silently discarded (the slot was removed by the timeout path) and
    /// does not panic or corrupt session state.
    #[test]
    fn late_response_after_timeout_is_discarded() {
        let h = setup();
        let session = h.conn.root_session();

        // Time out the request.
        let result = session.send_with_timeout(
            "Page.navigate",
            json!({}),
            std::time::Duration::from_millis(100),
        );
        assert_eq!(result.unwrap_err().kind, ProtocolErrorKind::Timeout);

        // Read the outgoing message to learn the id.
        let sent = h
            .out_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("request was not sent");
        let id = sent["id"].as_i64().unwrap();

        // Now deliver the "late" response. The reader thread should
        // handle this without panicking — it falls through resolve()
        // returning false for unknown ids.
        h.in_tx.send(success_response(id, None)).unwrap();

        // Give the reader thread a moment to process.
        thread::sleep(std::time::Duration::from_millis(100));

        // Connection still healthy.
        {
            let guard = h.conn.inner.lock().unwrap();
            assert_eq!(guard.state, ConnectionState::Connected);
            let root = guard.sessions.get("").expect("root session must exist");
            assert!(root.pending.is_empty());
        }

        teardown(h);
    }

    /// `send` (the default-timeout entry point) also enforces a bound: with
    /// the constant overridden to a tiny value via send_with_timeout, the
    /// same plumbing is exercised. This guards the regression where
    /// `Client::send` used `rx.recv()` with no deadline at all.
    #[test]
    fn send_default_timeout_constant_is_finite() {
        // Foundational safety guarantee: DEFAULT_SEND_TIMEOUT is bounded.
        assert!(DEFAULT_SEND_TIMEOUT < std::time::Duration::from_secs(600));
        assert!(DEFAULT_SEND_TIMEOUT >= std::time::Duration::from_secs(10));
    }

    // -----------------------------------------------------------------------
    // Browser.getCookies mock-transport tests (G1: cookie export)
    // -----------------------------------------------------------------------

    /// Build a `Browser.getCookies` response RawMessage containing one
    /// regular cookie and one HttpOnly cookie.
    fn get_cookies_response(id: i64) -> RawMessage {
        RawMessage {
            id: Some(id),
            method: None,
            params: None,
            result: Some(json!({
                "cookies": [
                    {
                        "name": "session",
                        "value": "abc123",
                        "domain": "example.com",
                        "path": "/",
                        "expires": -1.0,
                        "size": 13,
                        "httpOnly": false,
                        "secure": false,
                        "session": true,
                        "sameSite": "None"
                    },
                    {
                        "name": "PHPSESSID",
                        "value": "secret",
                        "domain": "example.com",
                        "path": "/",
                        "expires": -1.0,
                        "size": 13,
                        "httpOnly": true,
                        "secure": true,
                        "session": true,
                        "sameSite": "Strict"
                    }
                ]
            })),
            error: None,
            session_id: None,
        }
    }

    /// `Browser.getCookies` round-trip via mock transport: both cookies are
    /// returned; the HttpOnly flag is preserved on the HttpOnly cookie.
    #[test]
    fn get_cookies_round_trip_includes_http_only_flag() {
        let h = setup();
        let session = h.conn.root_session();

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            // Wait for the getCookies request, echo back a two-cookie payload.
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            assert_eq!(sent["method"], "Browser.getCookies");
            assert!(sent.get("sessionId").is_none(), "must use root session");
            in_tx.send(get_cookies_response(id)).unwrap();
        });

        let result = session
            .send(
                "Browser.getCookies",
                json!({ "browserContextId": "ctx-test-1" }),
            )
            .expect("Browser.getCookies should succeed");

        responder.join().unwrap();

        let cookies = result
            .get("cookies")
            .and_then(|v| v.as_array())
            .expect("result must have 'cookies' array");

        assert_eq!(
            cookies.len(),
            2,
            "expected 2 cookies, got {}",
            cookies.len()
        );

        // First cookie: non-HttpOnly.
        let c0 = &cookies[0];
        assert_eq!(c0["name"], "session");
        assert_eq!(c0["httpOnly"], false);

        // Second cookie: HttpOnly flag must be preserved as `true`.
        let c1 = &cookies[1];
        assert_eq!(c1["name"], "PHPSESSID");
        assert_eq!(
            c1["httpOnly"], true,
            "HttpOnly flag must be true on the HttpOnly cookie"
        );
        assert_eq!(c1["secure"], true);
    }

    /// `Browser.getCookies` with an empty cookies array returns an empty vec
    /// without errors.
    #[test]
    fn get_cookies_empty_response_ok() {
        let h = setup();
        let session = h.conn.root_session();

        let in_tx = h.in_tx.clone();
        let responder = thread::spawn(move || {
            let sent = recv_out(&h.out_rx);
            let id = sent["id"].as_i64().unwrap();
            in_tx
                .send(RawMessage {
                    id: Some(id),
                    method: None,
                    params: None,
                    result: Some(json!({ "cookies": [] })),
                    error: None,
                    session_id: None,
                })
                .unwrap();
        });

        let result = session
            .send(
                "Browser.getCookies",
                json!({ "browserContextId": "ctx-empty" }),
            )
            .expect("Browser.getCookies empty should succeed");

        responder.join().unwrap();

        let cookies = result
            .get("cookies")
            .and_then(|v| v.as_array())
            .expect("result must have 'cookies' array");
        assert!(cookies.is_empty(), "expected empty cookie array");
    }
}
