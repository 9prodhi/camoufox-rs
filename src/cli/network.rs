//! Page-owned network workers. Reader callbacks only enqueue events: protocol
//! calls must run on another thread so the reader can deliver their responses.

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::api::main_frame::SubscriptionGuard;
use crate::api::MainFrame;
use crate::protocol::client::Connection;
use crate::protocol::EventMessage;

use super::cassette::{self, Exchange, Matcher, Recorder};

/// Owns one page's recording or replay subscription and worker.
pub(crate) struct NetworkSession {
    recording: bool,
    sender: Arc<Mutex<Option<mpsc::Sender<EventMessage>>>>,
    guard: Option<SubscriptionGuard>,
    worker: Option<JoinHandle<Result<Value, String>>>,
}

impl NetworkSession {
    fn record(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        recorder: Recorder,
    ) -> Result<Self, String> {
        Self::spawn(page, connection, Mode::Record(recorder))
    }

    fn replay(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        replay: Replay,
    ) -> Result<Self, String> {
        let session = Self::spawn(Arc::clone(&page), connection, Mode::Replay(replay))?;
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
        let worker = std::thread::Builder::new()
            .name("camoufox-cassette".into())
            .spawn(move || run(page, mode, rx))
            .map_err(|e| format!("io error: starting network worker: {e}"))?;
        Ok(Self {
            recording,
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
    // Encoded lazily with a 16 MiB target; one larger body can exceed it.
    encoded: HashMap<String, String>,
    encoded_bytes: usize,
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
            encoded: HashMap::new(),
            encoded_bytes: 0,
            served: 0,
            missed: Vec::new(),
            missed_keys: HashMap::new(),
            missed_total: 0,
        })
    }

    fn body(&mut self, hash: &str) -> Result<&str, String> {
        // Revalidate even cache hits: mutation must never serve different data.
        let bytes = self.bodies[hash].read(hash)?;
        if !self.encoded.contains_key(hash) {
            let encoded = STANDARD.encode(bytes);
            const CACHE_BYTES: usize = 16 * 1024 * 1024;
            if self.encoded_bytes.saturating_add(encoded.len()) > CACHE_BYTES {
                self.encoded.clear();
                self.encoded_bytes = 0;
            }
            self.encoded_bytes += encoded.len();
            self.encoded.insert(hash.into(), encoded);
        }
        Ok(&self.encoded[hash])
    }

    fn miss(&mut self, method: &str, url: &str) {
        self.missed_total += 1;
        let key = (method.to_owned(), url.to_owned());
        if let Some(&i) = self.missed_keys.get(&key) {
            let count = self.missed[i]["count"].as_u64().unwrap() + 1;
            self.missed[i]["count"] = json!(count);
        } else if self.missed.len() < 1000 {
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
    request_body_hash(params, true)
}

fn request_body_hash(params: &Value, normalize: bool) -> Result<Option<String>, String> {
    let body = match params.get("postData") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(body)) => body,
        Some(_) => {
            return Err("io error: invalid network request body: expected base64 string".into())
        }
    };
    STANDARD
        .decode(body)
        .map(|raw| {
            Some(cassette::sha256(&if normalize {
                normalize_multipart(params, raw)
            } else {
                raw
            }))
        })
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

fn run(
    page: Arc<MainFrame>,
    mut mode: Mode,
    rx: mpsc::Receiver<EventMessage>,
) -> Result<Value, String> {
    let mut pending: HashMap<String, Exchange> = HashMap::new();
    let mut next_seq = 0;
    let mut error = None;
    while let Ok(event) = rx.recv() {
        let p = &event.params;
        let Some(id) = p["requestId"].as_str() else {
            continue;
        };
        match &mut mode {
            Mode::Replay(replay) => {
                if event.method != "Network.requestWillBeSent" || p["isIntercepted"] != true {
                    continue;
                }
                let method = p["method"].as_str().unwrap_or("");
                let url = p["url"].as_str().unwrap_or("");
                let body = if replay.match_body {
                    request_body(p)
                } else {
                    Ok(None)
                };
                let matched = body.ok().and_then(|body| {
                    replay
                        .matcher
                        .take(method, url, body.as_deref())
                        .or_else(|| {
                            // Pre-normalization v1 cassettes stored raw multipart hashes.
                            if replay.match_body && multipart_boundary(p).is_some() {
                                let legacy = request_body_hash(p, false).ok()?;
                                replay.matcher.take(method, url, legacy.as_deref())
                            } else {
                                None
                            }
                        })
                });
                if let Some(exchange) = matched {
                    let body = match replay.body(&exchange.body_sha256) {
                        Ok(body) => body,
                        Err(e) => {
                            page.abort_intercepted_request(id, "blockedbyclient");
                            error.get_or_insert(e);
                            continue;
                        }
                    };
                    let headers = cassette::replay_headers(&exchange.headers, exchange.body_len);
                    let borrowed: Vec<_> = headers
                        .iter()
                        .map(|(n, v)| (n.as_str(), v.as_str()))
                        .collect();
                    page.fulfill_intercepted_request(id, exchange.status, "", &borrowed, body);
                    replay.served += 1;
                } else {
                    page.abort_intercepted_request(id, "blockedbyclient");
                    replay.miss(method, url);
                }
            }
            Mode::Record(recorder) => {
                // Recording never enables interception or changes requests/cache.
                // All frame traffic shares this page session in Juggler.
                match event.method.as_str() {
                    "Network.requestWillBeSent" => {
                        let url = p["url"].as_str().unwrap_or("");
                        let body = match request_body(p) {
                            Ok(body) => body,
                            Err(e) => {
                                error.get_or_insert(e);
                                continue;
                            }
                        };
                        pending.insert(
                            id.into(),
                            Exchange {
                                seq: next_seq,
                                method: p["method"].as_str().unwrap_or("").into(),
                                url: url.into(),
                                request_body_sha256: body,
                                status: 0,
                                headers: Vec::new(),
                                body_sha256: String::new(),
                                body_len: 0,
                            },
                        );
                        next_seq += 1;
                    }
                    "Network.responseReceived" => {
                        if let Some(exchange) = pending.get_mut(id) {
                            exchange.status = p["status"].as_u64().unwrap_or(0) as u16;
                            let headers = p["headers"]
                                .as_array()
                                .map(|h| {
                                    h.iter()
                                        .filter_map(|h| {
                                            Some((
                                                h["name"].as_str()?.into(),
                                                h["value"].as_str()?.into(),
                                            ))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            exchange.headers = cassette::scrub_headers(headers);
                        }
                    }
                    "Network.requestFinished" => {
                        if let Some(mut exchange) = pending.remove(id) {
                            if exchange.status == 0 {
                                continue;
                            }
                            let response = page.get_response_body(id).or_else(|e| {
                                // Juggler removes redirected requests before body reads.
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
                                    continue;
                                }
                                Err(e) => {
                                    recorder.skip(
                                        &exchange,
                                        format!(
                                            "reading response body (status {}): {}",
                                            exchange.status, e.message
                                        ),
                                    );
                                    continue;
                                }
                            };
                            exchange.body_sha256 = cassette::sha256(&body);
                            exchange.body_len = body.len() as u64;
                            if let Err(e) = recorder.store(exchange, &body) {
                                error.get_or_insert(e);
                            }
                        }
                    }
                    "Network.requestFailed" => {
                        pending.remove(id);
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    match mode {
        Mode::Record(recorder) => recorder.finish(),
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
    fn replay_encodes_bodies_lazily_and_reuses_cached_encoding() {
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
        let mut replay = Replay::load(&dir, false).unwrap();
        assert!(replay.encoded.is_empty());
        let first = replay.body(&hash).unwrap().as_ptr();
        assert_eq!(replay.body(&hash).unwrap(), "aGVsbG8=");
        assert_eq!(first, replay.body(&hash).unwrap().as_ptr());
        // A cached encoding must still reject a file changed after its first use.
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
    session: Option<NetworkSession>,
    last_record_stop: Option<Result<Value, String>>,
    last_replay_stop: Option<Result<Value, String>>,
    closed: bool,
}

impl NetworkState {
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
    ) -> Result<Value, String> {
        self.idle()?;
        let recorder = Recorder::new(dir, urls)?;
        self.session = Some(NetworkSession::record(page, connection, recorder)?);
        self.last_record_stop = None;
        Ok(json!({"recording":true, "dir":dir}))
    }

    pub fn replay(
        &mut self,
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        dir: &std::path::Path,
        match_body: bool,
    ) -> Result<Value, String> {
        self.idle()?;
        let replay = Replay::load(dir, match_body)?;
        self.session = Some(NetworkSession::replay(page, connection, replay)?);
        self.last_replay_stop = None;
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

    pub fn close(&mut self) {
        self.closed = true;
        if let Some(session) = self.session.take() {
            if let Err(e) = session.stop() {
                log::warn!("network cassette cleanup: {e}");
            }
        }
    }
}

/// Reach active page operations without acquiring the per-instance mutex.
#[derive(Default)]
pub(crate) struct NetworkSessions {
    closed: bool,
    pages: Vec<std::sync::Weak<Mutex<NetworkState>>>,
}

impl NetworkSessions {
    pub fn register(&mut self, state: &Arc<Mutex<NetworkState>>) -> Result<(), String> {
        if self.closed {
            return Err("invalid argument: instance network sessions were closed".into());
        }
        self.pages.retain(|p| p.strong_count() > 0);
        if !self.pages.iter().any(|p| p.ptr_eq(&Arc::downgrade(state))) {
            self.pages.push(Arc::downgrade(state));
        }
        Ok(())
    }

    pub fn close_all(registry: &Mutex<Self>) {
        // Keep this separate network-registry mutex through the drain: two
        // concurrent cleanup paths must both wait before either closes the
        // connection. No per-instance mutex is held here.
        let mut registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.closed = true;
        for page in std::mem::take(&mut registry.pages)
            .into_iter()
            .filter_map(|p| p.upgrade())
        {
            page.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close();
        }
    }
}
