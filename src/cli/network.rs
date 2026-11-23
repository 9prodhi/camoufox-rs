//! Page-owned network workers. Reader callbacks only enqueue events: protocol
//! calls must run on another thread so the reader can deliver their responses.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use crate::api::main_frame::SubscriptionGuard;
use crate::api::MainFrame;
use crate::protocol::client::Connection;
use crate::protocol::EventMessage;

use super::cassette::{self, Exchange, Matcher, Recorder};

enum Message {
    Event(EventMessage),
    Stop,
}

/// Owns one page's recording or replay subscription and worker.
pub(crate) struct NetworkSession {
    recording: bool,
    sender: Arc<Mutex<Option<mpsc::Sender<Message>>>>,
    guard: Option<SubscriptionGuard>,
    worker: Option<JoinHandle<Result<Value, String>>>,
}

impl NetworkSession {
    pub(crate) fn record(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        dir: &Path,
        filters: &[String],
    ) -> Result<Self, String> {
        let recorder = Recorder::new(dir, filters)?;
        Self::spawn(page, connection, Mode::Record(recorder))
    }

    pub(crate) fn replay(
        page: Arc<MainFrame>,
        connection: Arc<Connection>,
        dir: &Path,
        match_body: bool,
    ) -> Result<Self, String> {
        // Validate and preload every body before changing browser state.
        let (index, bodies) = cassette::load(dir)?;
        // Juggler deliberately bypasses interception after redirects. Refuse
        // these before enabling replay rather than allowing a network escape.
        if index.exchanges.iter().any(Exchange::redirects) {
            return Err(
                "invalid argument: offline replay of redirect responses is unsupported by Juggler"
                    .into(),
            );
        }
        let session = Self::spawn(
            Arc::clone(&page),
            connection,
            Mode::Replay(Replay {
                matcher: Matcher::new(index.exchanges, match_body),
                bodies,
                served: 0,
                missed: Vec::new(),
            }),
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
    ) -> Result<Self, String> {
        let recording = matches!(mode, Mode::Record(_));
        let (tx, rx) = mpsc::channel();
        let sender = Arc::new(Mutex::new(Some(tx)));
        let events = Arc::clone(&sender);
        let key = page.session().key().to_owned();
        let id = connection.on_event_any(
            &key,
            Arc::new(move |event| {
                if !event.method.starts_with("Network.") {
                    return;
                }
                if let Some(sender) = events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    let _ = sender.send(Message::Event(event.clone()));
                }
            }),
        );
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
        // Serialize with callbacks already cloned by the reader. Nothing can
        // enqueue after Stop, even if unsubscription races with dispatch.
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = sender.send(Message::Stop);
        }
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

struct Replay {
    matcher: Matcher,
    bodies: HashMap<String, Vec<u8>>,
    served: u64,
    missed: Vec<Value>,
}

enum Mode {
    Record(Recorder),
    Replay(Replay),
}

fn request_body(params: &Value) -> Result<Option<String>, String> {
    params
        .get("postData")
        .and_then(Value::as_str)
        .map(|body| {
            STANDARD
                .decode(body)
                .map(|raw| cassette::sha256(&raw))
                .map_err(|e| format!("io error: invalid network request body: {e}"))
        })
        .transpose()
}

fn run(page: Arc<MainFrame>, mut mode: Mode, rx: mpsc::Receiver<Message>) -> Result<Value, String> {
    let mut pending: HashMap<String, Exchange> = HashMap::new();
    let mut next_seq = 0;
    let mut error = None;
    while let Ok(Message::Event(event)) = rx.recv() {
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
                let matched = request_body(p)
                    .ok()
                    .and_then(|body| replay.matcher.take(method, url, body.as_deref()));
                if let Some(exchange) = matched {
                    let body = &replay.bodies[&exchange.body_sha256];
                    let headers = cassette::replay_headers(&exchange.headers, exchange.body_len);
                    let borrowed: Vec<_> = headers
                        .iter()
                        .map(|(n, v)| (n.as_str(), v.as_str()))
                        .collect();
                    page.fulfill_intercepted_request(
                        id,
                        exchange.status,
                        "",
                        &borrowed,
                        &STANDARD.encode(body),
                    );
                    replay.served += 1;
                } else {
                    page.abort_intercepted_request(id, "blockedbyclient");
                    replay.missed.push(json!({"method": method, "url": url}));
                }
            }
            Mode::Record(recorder) => {
                // Recording never enables interception or changes requests/cache.
                // All frame traffic shares this page session in Juggler.
                match event.method.as_str() {
                    "Network.requestWillBeSent" => {
                        let url = p["url"].as_str().unwrap_or("");
                        if !url.starts_with("http://") && !url.starts_with("https://") {
                            continue;
                        }
                        if !recorder.matches(url) {
                            continue;
                        }
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
                            let result = page
                                .get_response_body(id)
                                .or_else(|e| {
                                    // Juggler removes redirected requests before body reads
                                    // (PROTOCOL.md, Redirect Handling). Browsers discard
                                    // these bodies, so replay only needs status + Location.
                                    if exchange.redirects() {
                                        Ok((Vec::new(), false))
                                    } else {
                                        Err(e)
                                    }
                                })
                                .map_err(|e| {
                                    format!(
                                        "io error: reading response body for {} (status {}): {}",
                                        exchange.url, exchange.status, e.message
                                    )
                                })
                                .and_then(|(body, evicted)| {
                                    if evicted {
                                        return Err(
                                            "io error: response body was evicted during recording"
                                                .into(),
                                        );
                                    }
                                    exchange.body_sha256 = cassette::sha256(&body);
                                    exchange.body_len = body.len() as u64;
                                    recorder.store(exchange, &body)
                                });
                            if let Err(e) = result {
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
        Mode::Replay(replay) => Ok(json!({"served": replay.served, "missed": replay.missed})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
