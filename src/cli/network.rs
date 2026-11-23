//! Page-owned network workers. Reader callbacks only enqueue events: protocol
//! calls must run on another thread so the reader can deliver their responses.

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::api::main_frame::SubscriptionGuard;
use crate::api::MainFrame;
use crate::protocol::client::Connection;
use crate::protocol::EventMessage;

use super::cassette::{self, Exchange, Matcher, Recorder};

/// Total body-drain budget for instance and page cleanup.
pub(crate) const FINALIZE_TIMEOUT_SECS: u64 = 15;
const MAX_REPLAY_MISSES: usize = 1000;

type Deadline = Arc<Mutex<Option<Instant>>>;

/// Owns one page's recording or replay subscription and worker.
pub(crate) struct NetworkSession {
    recording: bool,
    finalize_deadline: Deadline,
    sender: Arc<Mutex<Option<mpsc::Sender<EventMessage>>>>,
    guard: Option<SubscriptionGuard>,
    worker: Option<JoinHandle<Result<Value, String>>>,
}

impl NetworkSession {
    fn replay(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        replay: Replay,
        deadline: Deadline,
    ) -> Result<Self, String> {
        let session = Self::spawn(
            Arc::clone(&page),
            connection,
            Mode::Replay(replay),
            deadline,
        )?;
        if let Err(e) = page.set_request_interception(true) {
            let _ = page.set_request_interception(false);
            return Err(e.message);
        }
        Ok(session)
    }

    fn spawn(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        mode: Mode,
        finalize_deadline: Deadline,
    ) -> Result<Self, String> {
        let recording = matches!(mode, Mode::Record(_));
        let filters = match &mode {
            Mode::Record(recorder) => Some(recorder.filters.clone()),
            Mode::Replay(_) => None,
        };
        let (tx, rx) = mpsc::channel();
        let sender = Arc::new(Mutex::new(Some(tx)));
        let key = page.session().key().to_owned();
        let id = connection.on_event_any(&key, event_callback(filters, Arc::clone(&sender)));
        let guard = SubscriptionGuard::session(connection, &key, "*", id);
        let worker_deadline = Arc::clone(&finalize_deadline);
        let worker = std::thread::Builder::new()
            .name("camoufox-cassette".into())
            .spawn(move || run(page, mode, rx, worker_deadline))
            .map_err(|e| format!("io error: starting network worker: {e}"))?;
        Ok(Self {
            recording,
            finalize_deadline,
            sender,
            guard: Some(guard),
            worker: Some(worker),
        })
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.recording
    }

    pub(crate) fn stop(mut self) -> Result<Value, String> {
        self.join()
    }

    fn join(&mut self) -> Result<Value, String> {
        self.guard.take();
        // Serialize with callbacks already cloned by the reader. Dropping
        // the sender ends the stream after every queued event is drained.
        self.sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| "io error: network worker panicked".to_string())?,
            None => Ok(Value::Null),
        }
    }
}

impl Drop for NetworkSession {
    fn drop(&mut self) {
        if let Err(e) = self.join() {
            log::warn!("network cassette cleanup: {e}");
        }
    }
}

pub(super) struct Replay {
    matcher: Matcher,
    match_body: bool,
    bodies: cassette::Bodies,
    served: u64,
    missed: Vec<Value>,
    missed_keys: HashMap<(String, String), usize>,
    missed_total: u64,
}

impl Replay {
    pub fn load(dir: &std::path::Path, match_body: bool) -> Result<Self, String> {
        let (index, bodies) = cassette::load(dir)?;
        if index.exchanges.iter().any(Exchange::redirects) {
            return Err(
                "invalid argument: offline replay of redirect responses is unsupported by Juggler"
                    .into(),
            );
        }
        Ok(Self {
            matcher: Matcher::new(index.exchanges, match_body),
            match_body,
            bodies,
            served: 0,
            missed: Vec::new(),
            missed_keys: HashMap::new(),
            missed_total: 0,
        })
    }

    fn body(&self, hash: &str) -> Result<String, String> {
        // Read and revalidate on every fulfillment, including repeated matches.
        Ok(STANDARD.encode(self.bodies[hash].read(hash)?))
    }

    fn miss(&mut self, method: &str, url: &str) {
        self.missed_total += 1;
        let key = (method.to_owned(), url.to_owned());
        if let Some(&i) = self.missed_keys.get(&key) {
            let count = self.missed[i]["count"].as_u64().unwrap() + 1;
            self.missed[i]["count"] = json!(count);
        } else if self.missed.len() < MAX_REPLAY_MISSES {
            self.missed_keys.insert(key, self.missed.len());
            self.missed
                .push(json!({"method":method, "url":url, "count":1}));
        }
    }
}

fn event_callback(
    filters: Option<Vec<regex::Regex>>,
    events: Arc<Mutex<Option<mpsc::Sender<EventMessage>>>>,
) -> crate::protocol::events::EventHandler {
    // Track accepted IDs so excluded response/finish payloads never enter
    // the queue, even if they do not carry a URL of their own.
    let accepted = Mutex::new(HashSet::<String>::new());
    Arc::new(move |event| {
        if let Some(filters) = &filters {
            if !accept_record_event(event, filters, &accepted) {
                return;
            }
        } else if event.method != "Network.requestWillBeSent"
            || event.params["isIntercepted"] != true
        {
            return;
        }
        if let Some(sender) = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = sender.send(event.clone());
        }
    })
}

fn accept_record_event(
    event: &EventMessage,
    filters: &[regex::Regex],
    accepted: &Mutex<HashSet<String>>,
) -> bool {
    let Some(id) = event.params["requestId"].as_str() else {
        return false;
    };
    let mut accepted = accepted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match event.method.as_str() {
        "Network.requestWillBeSent" => {
            let url = event.params["url"].as_str().unwrap_or("");
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || !(filters.is_empty() || filters.iter().any(|r| r.is_match(url)))
            {
                accepted.remove(id);
                return false;
            }
            accepted.insert(id.to_owned());
            true
        }
        "Network.responseReceived" => accepted.contains(id),
        "Network.requestFinished" | "Network.requestFailed" => accepted.remove(id),
        _ => false,
    }
}

enum Mode {
    Record(Recorder),
    Replay(Replay),
}

fn request_body(params: &Value) -> Result<Option<String>, String> {
    let body = match params.get("postData") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(body)) => body,
        Some(_) => {
            return Err("io error: invalid network request body: expected base64 string".into())
        }
    };
    STANDARD
        .decode(body)
        .map(|raw| Some(cassette::sha256(&normalize_multipart(params, raw))))
        .map_err(|e| format!("io error: invalid network request body: {e}"))
}

fn multipart_boundary(params: &Value) -> Option<&str> {
    let content_type = params["headers"].as_array().and_then(|headers| {
        headers.iter().find_map(|h| {
            h["name"]
                .as_str()
                .filter(|n| n.eq_ignore_ascii_case("content-type"))?;
            h["value"].as_str()
        })
    });
    let content_type = content_type?;
    let mut parts = content_type.split(';');
    if !parts
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    parts
        .find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("boundary")
                .then(|| value.trim().trim_matches('"'))
        })
        .filter(|s| !s.is_empty())
}

fn normalize_multipart(params: &Value, raw: Vec<u8>) -> Vec<u8> {
    let Some(boundary) = multipart_boundary(params) else {
        return raw;
    };
    let boundary = boundary.as_bytes();
    let mut normalized = Vec::with_capacity(raw.len());
    let mut remaining = raw.as_slice();
    while let Some(pos) = remaining
        .windows(boundary.len())
        .position(|w| w == boundary)
    {
        normalized.extend_from_slice(&remaining[..pos]);
        normalized.extend_from_slice(b"CAMOUFOX_MULTIPART_BOUNDARY");
        remaining = &remaining[pos + boundary.len()..];
    }
    normalized.extend_from_slice(remaining);
    normalized
}

fn handle_replay_event(
    page: &MainFrame,
    replay: &mut Replay,
    event: &EventMessage,
) -> Result<(), String> {
    let p = &event.params;
    let Some(id) = p["requestId"].as_str() else {
        return Ok(());
    };
    if event.method != "Network.requestWillBeSent" || p["isIntercepted"] != true {
        return Ok(());
    }
    let method = p["method"].as_str().unwrap_or("");
    let url = p["url"].as_str().unwrap_or("");
    let body = if replay.match_body {
        request_body(p)
    } else {
        Ok(None)
    };
    let matched = body
        .ok()
        .and_then(|body| replay.matcher.take(method, url, body.as_deref()));
    if let Some(exchange) = matched {
        let body = match replay.body(&exchange.body_sha256) {
            Ok(body) => body,
            Err(e) => {
                page.abort_intercepted_request(id, "blockedbyclient");
                return Err(e);
            }
        };
        let headers = cassette::replay_headers(&exchange.headers, exchange.body_len);
        let borrowed: Vec<_> = headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        page.fulfill_intercepted_request(id, exchange.status, "", &borrowed, &body);
        replay.served += 1;
    } else {
        page.abort_intercepted_request(id, "blockedbyclient");
        replay.miss(method, url);
    }
    Ok(())
}

fn deadline_exceeded(deadline: &Mutex<Option<Instant>>) -> bool {
    deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some_and(|deadline| Instant::now() >= deadline)
}

#[derive(Default)]
struct RecordEvents {
    pending: HashMap<String, Exchange>,
    next_seq: u64,
}

fn handle_record_event(
    page: &MainFrame,
    recorder: &mut Recorder,
    events: &mut RecordEvents,
    event: &EventMessage,
    deadline: &Mutex<Option<Instant>>,
) -> Result<(), String> {
    let p = &event.params;
    let Some(id) = p["requestId"].as_str() else {
        return Ok(());
    };
    // Recording never enables interception or changes requests/cache.
    // All frame traffic shares this page session in Juggler.
    match event.method.as_str() {
        "Network.requestWillBeSent" => {
            let body = request_body(p)?;
            events.pending.insert(
                id.into(),
                Exchange {
                    seq: events.next_seq,
                    method: p["method"].as_str().unwrap_or("").into(),
                    url: p["url"].as_str().unwrap_or("").into(),
                    request_body_sha256: body,
                    status: 0,
                    headers: Vec::new(),
                    body_sha256: String::new(),
                    body_len: 0,
                },
            );
            events.next_seq += 1;
        }
        "Network.responseReceived" => {
            if let Some(exchange) = events.pending.get_mut(id) {
                exchange.status = p["status"].as_u64().unwrap_or(0) as u16;
                let headers = p["headers"]
                    .as_array()
                    .map(|headers| {
                        headers
                            .iter()
                            .filter_map(|h| {
                                Some((h["name"].as_str()?.into(), h["value"].as_str()?.into()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                exchange.headers = cassette::scrub_headers(headers);
            }
        }
        "Network.requestFinished" => {
            if let Some(mut exchange) = events.pending.remove(id) {
                if exchange.status == 0 {
                    return Ok(());
                }
                if deadline_exceeded(deadline) {
                    recorder.skip(&exchange, "finalize deadline exceeded".into());
                    return Ok(());
                }
                let response = page.get_response_body(id);
                // The watchdog closes the connection to interrupt an in-flight
                // read; queued body reads then skip without protocol calls.
                if deadline_exceeded(deadline) {
                    recorder.skip(&exchange, "finalize deadline exceeded".into());
                    return Ok(());
                }
                let response = response.or_else(|e| {
                    if exchange.redirects() {
                        Ok((Vec::new(), false))
                    } else {
                        Err(e)
                    }
                });
                let body = match response {
                    Ok((body, false)) => body,
                    Ok((_, true)) => {
                        recorder.skip(
                            &exchange,
                            "response body was evicted during recording".into(),
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        recorder.skip(
                            &exchange,
                            format!(
                                "reading response body (status {}): {}",
                                exchange.status, e.message
                            ),
                        );
                        return Ok(());
                    }
                };
                exchange.body_sha256 = cassette::sha256(&body);
                exchange.body_len = body.len() as u64;
                recorder.store(exchange, &body)?;
            }
        }
        "Network.requestFailed" => {
            events.pending.remove(id);
        }
        _ => {}
    }
    Ok(())
}

fn run(
    page: Arc<MainFrame>,
    mut mode: Mode,
    rx: mpsc::Receiver<EventMessage>,
    deadline: Arc<Mutex<Option<Instant>>>,
) -> Result<Value, String> {
    let mut events = RecordEvents::default();
    let mut error = None;
    while let Ok(event) = rx.recv() {
        let result = match &mut mode {
            Mode::Replay(replay) => handle_replay_event(&page, replay, &event),
            Mode::Record(recorder) => {
                handle_record_event(&page, recorder, &mut events, &event, &deadline)
            }
        };
        if let Err(e) = result {
            error.get_or_insert(e);
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    match mode {
        Mode::Record(mut recorder) => {
            if deadline_exceeded(&deadline) {
                let mut pending: Vec<_> = events.pending.into_values().collect();
                pending.sort_by_key(|e| e.seq);
                for exchange in pending {
                    recorder.skip(&exchange, "finalize deadline exceeded".into());
                }
            }
            recorder.finish()
        }
        Mode::Replay(replay) => Ok(
            json!({"served": replay.served, "missed": replay.missed, "missed_total": replay.missed_total}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_callback_filters_before_enqueueing_events() {
        let (sender, receiver) = mpsc::channel();
        let callback = event_callback(
            Some(vec![regex::Regex::new("/keep$").unwrap()]),
            Arc::new(Mutex::new(Some(sender))),
        );
        for url in [
            "data:text/plain,keep",
            "https://example.com/excluded",
            "https://example.com/keep",
        ] {
            for method in [
                "Network.requestWillBeSent",
                "Network.responseReceived",
                "Network.requestFinished",
                "Network.responseReceived",
            ] {
                callback(&EventMessage {
                    method: method.into(),
                    params: json!({"requestId":url, "url":url}),
                    session_id: Some("page".into()),
                });
            }
        }
        let queued: Vec<_> = receiver.try_iter().collect();
        assert_eq!(queued.len(), 3);
        assert!(queued
            .iter()
            .all(|e| e.params["requestId"] == "https://example.com/keep"));
        assert_eq!(queued[2].method, "Network.requestFinished");
    }

    #[test]
    fn replay_encodes_on_demand_and_revalidates_repeated_reads() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        let mut recorder = Recorder::new(&dir, &[]).unwrap();
        let hash = cassette::sha256(b"hello");
        recorder
            .store(
                Exchange {
                    seq: 0,
                    method: "GET".into(),
                    url: "https://example.com/".into(),
                    request_body_sha256: None,
                    status: 200,
                    headers: Vec::new(),
                    body_sha256: hash.clone(),
                    body_len: 5,
                },
                b"hello",
            )
            .unwrap();
        recorder.finish().unwrap();
        let replay = Replay::load(&dir, false).unwrap();
        assert_eq!(replay.body(&hash).unwrap(), "aGVsbG8=");
        // Repeated reads must reject a file changed after its first use.
        std::fs::write(dir.join("bodies").join(&hash), b"wrong").unwrap();
        assert!(replay.body(&hash).unwrap_err().starts_with("io error:"));
    }

    #[test]
    fn request_hash_uses_raw_binary_and_distinguishes_absent_and_empty() {
        assert_eq!(request_body(&json!({})).unwrap(), None);
        assert_eq!(
            request_body(&json!({"postData":""})).unwrap(),
            Some(cassette::sha256(b""))
        );
        assert_eq!(
            request_body(&json!({"postData":"AID/"})).unwrap(),
            Some(cassette::sha256(&[0, 128, 255]))
        );
        assert!(request_body(&json!({"postData":"!"})).is_err());
        for malformed in [json!(3), json!([]), json!({}), json!(true)] {
            assert!(request_body(&json!({"postData":malformed})).is_err());
        }
        assert_eq!(request_body(&json!({"postData":null})).unwrap(), None);
    }
}

/// Per-page network operations serialize independently of browser commands.
#[derive(Default)]
pub(crate) struct NetworkState {
    finalize_deadline: Deadline,
    session: Option<NetworkSession>,
    last_record_stop: Option<Result<Value, String>>,
    last_replay_stop: Option<Result<Value, String>>,
    closed: bool,
}

impl NetworkState {
    /// Share the deadline handle registered before acquiring page state.
    pub fn set_finalize_deadline(&mut self, deadline: Deadline) {
        self.finalize_deadline = deadline;
    }

    fn idle(&self) -> Result<(), String> {
        if self.closed {
            return Err("invalid argument: page network session was closed".into());
        }
        if self.session.is_some() {
            return Err(
                "invalid argument: record or replay is already active for this page".into(),
            );
        }
        Ok(())
    }

    pub fn record(
        &mut self,
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        dir: &std::path::Path,
        urls: &[String],
        validation: Result<(), String>,
    ) -> Result<Value, String> {
        self.last_record_stop = None;
        validation?;
        self.idle()?;
        let recorder = Recorder::new(dir, urls)?;
        self.session = Some(NetworkSession::spawn(
            page,
            connection,
            Mode::Record(recorder),
            Arc::clone(&self.finalize_deadline),
        )?);
        Ok(json!({"recording":true, "dir":dir}))
    }

    pub fn replay(
        &mut self,
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        dir: &std::path::Path,
        match_body: bool,
        validation: Result<(), String>,
    ) -> Result<Value, String> {
        self.last_replay_stop = None;
        validation?;
        self.idle()?;
        let replay = Replay::load(dir, match_body)?;
        self.session = Some(NetworkSession::replay(
            page,
            connection,
            replay,
            Arc::clone(&self.finalize_deadline),
        )?);
        Ok(json!({"replaying":true, "dir":dir}))
    }

    pub fn stop(&mut self, recording: bool, page: &MainFrame) -> Result<Value, String> {
        let last = if recording {
            &mut self.last_record_stop
        } else {
            &mut self.last_replay_stop
        };
        if self.session.is_none() {
            if let Some(result) = last {
                return result.clone().map(|mut value| {
                    value["repeated"] = json!(true);
                    value
                });
            }
        }
        let name = if recording { "recording" } else { "replay" };
        if self
            .session
            .as_ref()
            .is_none_or(|s| s.is_recording() != recording)
        {
            return Err(format!(
                "invalid argument: {name} is not active for this page"
            ));
        }
        let result = self.session.take().unwrap().stop();
        let result = if recording {
            result
        } else {
            // Drain accepted requests before disabling interception, which
            // would otherwise resume them against the live network.
            page.set_request_interception(false)
                .map_err(|e| e.message)
                .and(result)
        };
        *last = Some(result.clone());
        result
    }

    fn close(&mut self, deadline: Instant) {
        self.closed = true;
        if let Some(session) = self.session.take() {
            set_deadline(&session.finalize_deadline, deadline);
            let recording = session.recording;
            let result = session.stop();
            if let Err(e) = &result {
                log::warn!("network cassette cleanup: {e}");
            }
            if recording {
                self.last_record_stop = Some(result);
            } else {
                self.last_replay_stop = Some(result);
            }
        }
    }
}

struct NetworkPage {
    state: std::sync::Weak<Mutex<NetworkState>>,
    deadline: Deadline,
}

/// Reach active page operations without acquiring the per-instance mutex.
#[derive(Default)]
pub(crate) struct NetworkSessions {
    closed: bool,
    pages: Vec<NetworkPage>,
}

impl NetworkSessions {
    pub fn register(&mut self, state: &Arc<Mutex<NetworkState>>) -> Result<Deadline, String> {
        if self.closed {
            return Err("invalid argument: instance network sessions were closed".into());
        }
        self.pages.retain(|p| p.state.strong_count() > 0);
        if let Some(page) = self
            .pages
            .iter()
            .find(|p| p.state.ptr_eq(&Arc::downgrade(state)))
        {
            return Ok(Arc::clone(&page.deadline));
        }
        let deadline = Arc::new(Mutex::new(None));
        self.pages.push(NetworkPage {
            state: Arc::downgrade(state),
            deadline: Arc::clone(&deadline),
        });
        Ok(deadline)
    }

    pub fn close_all(registry: &Mutex<Self>, connection: &Arc<Connection>) {
        let finalize = FinalizeDeadline::new(connection);
        // Keep this separate network-registry mutex through the drain: two
        // concurrent cleanup paths must both wait before either closes the
        // connection. No per-instance mutex is held here.
        let mut registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.closed = true;
        let pages = std::mem::take(&mut registry.pages);
        // Mark every worker before joining any of them. A concurrent record
        // stop can own the page mutex while its worker is blocked reading.
        for page in &pages {
            set_deadline(&page.deadline, finalize.deadline);
        }
        for page in pages.into_iter().filter_map(|p| p.state.upgrade()) {
            close_network_state_before(&page, finalize.deadline);
        }
    }

    pub fn close_page(
        registry: &Mutex<Self>,
        state: &Arc<Mutex<NetworkState>>,
        connection: &Arc<Connection>,
    ) {
        let finalize = FinalizeDeadline::new(connection);
        {
            let registry = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(page) = registry
                .pages
                .iter()
                .find(|p| p.state.ptr_eq(&Arc::downgrade(state)))
            {
                set_deadline(&page.deadline, finalize.deadline);
            }
        }
        close_network_state_before(state, finalize.deadline);
    }
}

fn set_deadline(deadline: &Mutex<Option<Instant>>, end: Instant) {
    let mut deadline = deadline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *deadline = Some(deadline.map_or(end, |current| current.min(end)));
}

/// Close page network state while recovering poisoned cleanup locks.
pub(crate) fn close_network_state(state: &Mutex<NetworkState>, connection: &Arc<Connection>) {
    let finalize = FinalizeDeadline::new(connection);
    close_network_state_before(state, finalize.deadline);
}

fn close_network_state_before(state: &Mutex<NetworkState>, deadline: Instant) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .close(deadline);
}

/// One watchdog covers the whole drain, including all queued body reads.
struct FinalizeDeadline {
    deadline: Instant,
    cancel: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl FinalizeDeadline {
    fn new(connection: &Arc<Connection>) -> Self {
        let deadline = Instant::now() + Duration::from_secs(FINALIZE_TIMEOUT_SECS);
        let (cancel, wait) = mpsc::channel();
        let connection = Arc::clone(connection);
        let worker = std::thread::spawn(move || {
            if wait
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err()
            {
                let _ = connection.close();
            }
        });
        Self {
            deadline,
            cancel,
            worker: Some(worker),
        }
    }
}

impl Drop for FinalizeDeadline {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        let _ = self.worker.take().unwrap().join();
    }
}
