//! Record/replay failure paths against a scripted protocol peer. A real
//! browser cannot be made to fail these calls on demand.
use super::*;
use crate::api::browser::BrowserOptions;
use crate::protocol::types::{ErrorData, RawMessage};
use crate::transport::errors::TransportError;
use crate::transport::{Transport, TransportReader, TransportWriter};
use serde_json::Value;
use std::sync::mpsc;
use std::time::Instant;

type Reply = dyn Fn(&str, &Value) -> Result<Value, String> + Send;

struct Writer(mpsc::Sender<Value>, Arc<AtomicBool>);
impl TransportWriter for Writer {
    fn send(&mut self, message: &Value) -> Result<(), TransportError> {
        self.0
            .send(message.clone())
            .map_err(|_| TransportError::Closed)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        self.1.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn is_closed(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }
}
struct Reader(mpsc::Receiver<RawMessage>, Arc<AtomicBool>);
impl TransportReader for Reader {
    fn receive(&mut self) -> Result<RawMessage, TransportError> {
        loop {
            if self.1.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            match self.0.recv_timeout(Duration::from_millis(5)) {
                Ok(message) => return Ok(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(TransportError::Closed),
            }
        }
    }
    fn is_closed(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }
}
struct MockTransport(Writer, Reader);
impl Transport for MockTransport {
    fn into_split(self: Box<Self>) -> (Box<dyn TransportWriter>, Box<dyn TransportReader>) {
        (Box::new(self.0), Box::new(self.1))
    }
}

/// One instance `mock` with one page `p`, answered by `reply`.
struct Harness {
    manager: InstanceManager,
    events: mpsc::Sender<RawMessage>,
    sent: Arc<Mutex<Vec<Value>>>,
    closed: Arc<AtomicBool>,
    connection: Arc<Connection>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn new(reply: impl Fn(&str, &Value) -> Result<Value, String> + Send + 'static) -> Self {
        let reply: Box<Reply> = Box::new(reply);
        let (events, incoming) = mpsc::channel();
        let (outgoing, outgoing_rx) = mpsc::channel::<Value>();
        let closed = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let connection = Connection::new(Box::new(MockTransport(
            Writer(outgoing, closed.clone()),
            Reader(incoming, closed.clone()),
        )));
        let (stop, log, replies) = (closed.clone(), sent.clone(), events.clone());
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok(request) = outgoing_rx.recv_timeout(Duration::from_millis(2)) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                let method = request["method"].as_str().unwrap_or("");
                let (result, error) = match reply(method, &request["params"]) {
                    Ok(value) => (Some(value), None),
                    Err(message) => (
                        None,
                        Some(ErrorData {
                            message,
                            data: None,
                        }),
                    ),
                };
                let _ = replies.send(RawMessage {
                    id: request["id"].as_i64(),
                    method: None,
                    params: None,
                    result,
                    error,
                    session_id: request["sessionId"].as_str().map(str::to_owned),
                });
            }
        });
        let root = connection.root_session();
        let browser = Browser::connect(connection, root, BrowserOptions::default()).unwrap();
        let conn = browser.connection().clone();
        let context =
            crate::api::BrowserContext::new("ctx".into(), &conn.root_session(), conn.clone());
        let harness_event = |method: &str, params: Value, session: Option<&str>| {
            events
                .send(RawMessage {
                    id: None,
                    method: Some(method.into()),
                    params: Some(params),
                    result: None,
                    error: None,
                    session_id: session.map(str::to_owned),
                })
                .unwrap();
        };
        harness_event(
            "Browser.attachedToTarget",
            json!({"sessionId":"page", "targetInfo":{"type":"page","targetId":"target","browserContextId":"ctx"}}),
            None,
        );
        harness_event(
            "Page.frameAttached",
            json!({"frameId":"main"}),
            Some("page"),
        );
        let tracked = context
            .registry()
            .claim("target", Duration::from_secs(2))
            .unwrap();
        let main_frame =
            MainFrame::from_tracked(tracked, conn.clone(), context.registry().clone()).unwrap();
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let instance = Instance {
            instance_id: "mock".into(),
            browser,
            pid: child.id(),
            child,
            version: None,
            profile_dir: ProfileDir::Temp(tempfile::tempdir().unwrap()),
            profile_reservation: None,
            pages: HashMap::from([(
                "p".into(),
                ManagedMainFrame {
                    main_frame: Arc::new(main_frame),
                    network: None,
                    headers: Vec::new(),
                },
            )]),
            closed_pages: HashSet::new(),
            closed_pages_order: VecDeque::new(),
            page_counter: 1,
            close_timeout: Duration::from_millis(100),
            registry: context.registry().clone(),
        };
        let manager = InstanceManager::new();
        manager.instances.lock().unwrap().insert(
            "mock".into(),
            InstanceEntry {
                managed: Arc::new(Mutex::new(ManagedInstance {
                    instance: Some(instance),
                    context,
                    _download_guards: Vec::new(),
                })),
                closer: conn.clone(),
                downloads: crate::cli::downloads::DownloadLog::new("ctx".into(), None),
            },
        );
        Self {
            manager,
            events,
            sent,
            closed,
            connection: conn,
            worker: Some(worker),
        }
    }

    fn page_event(&self, method: &str, params: Value) {
        self.events
            .send(RawMessage {
                id: None,
                method: Some(method.into()),
                params: Some(params),
                result: None,
                error: None,
                session_id: Some("page".into()),
            })
            .unwrap();
    }

    /// Emit a complete HTTP exchange for `id` (request, response, finished).
    fn exchange(&self, id: &str) {
        let url = format!("https://example.com/{id}");
        self.page_event(
            "Network.requestWillBeSent",
            json!({"requestId": id, "url": url, "method": "GET"}),
        );
        self.page_event(
            "Network.responseReceived",
            json!({"requestId": id, "status": 200, "headers": [{"name":"Content-Type","value":"text/plain"}]}),
        );
        self.page_event("Network.requestFinished", json!({"requestId": id}));
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == method)
            .map(|r| r["params"].clone())
            .collect()
    }

    /// Wait until the peer has received `count` requests for `method`.
    fn wait_for(&self, method: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.requests(method).len() < count {
            assert!(Instant::now() < deadline, "{method} x{count} never sent");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.connection.close();
        self.worker.take().unwrap().join().unwrap();
    }
}

fn empty_cassette() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.json"),
        r#"{"format":"camoufox-cassette","version":1,"exchanges":[]}"#,
    )
    .unwrap();
    dir
}

fn kind(error: &str) -> Option<String> {
    crate::cli::ipc::DaemonResponse::err(error).kind
}

#[test]
fn replay_start_interception_failure_rolls_back_and_leaves_page_idle() {
    let harness = Harness::new(|method, params| {
        if method == "Network.setRequestInterception" && params["enabled"] == true {
            Err("interception unavailable".into())
        } else {
            Ok(json!({}))
        }
    });
    let cassette = empty_cassette();
    let manager = &harness.manager;
    let error = manager
        .replay_start("mock", "p", cassette.path(), false)
        .unwrap_err();
    assert_eq!(error, "interception unavailable");
    assert_eq!(kind(&error).as_deref(), Some("other"));
    // The failed enable is followed by a best-effort disable.
    let toggles: Vec<_> = harness
        .requests("Network.setRequestInterception")
        .into_iter()
        .map(|p| p["enabled"].clone())
        .collect();
    assert_eq!(toggles, [json!(true), json!(false)]);
    // No session was installed: replay is not active and can be retried.
    assert_eq!(
        manager.replay_stop("mock", "p").unwrap_err(),
        "invalid argument: replay is not active for this page"
    );
    assert_eq!(
        manager
            .replay_start("mock", "p", cassette.path(), false)
            .unwrap_err(),
        "interception unavailable"
    );
    // The page can still record.
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    manager.record_start("mock", "p", &dir, &[]).unwrap();
    assert_eq!(manager.record_stop("mock", "p").unwrap()["exchanges"], 0);
}

#[test]
fn replay_stop_disable_failure_is_reported_and_releases_the_page() {
    let harness = Harness::new(|method, params| {
        if method == "Network.setRequestInterception" && params["enabled"] == false {
            Err("disable failed".into())
        } else {
            Ok(json!({}))
        }
    });
    let cassette = empty_cassette();
    let manager = &harness.manager;
    assert_eq!(
        manager
            .replay_start("mock", "p", cassette.path(), false)
            .unwrap(),
        json!({"replaying": true, "dir": cassette.path()})
    );
    let error = manager.replay_stop("mock", "p").unwrap_err();
    assert_eq!(error, "disable failed");
    // The session was consumed before disabling, so the page is not stuck
    // in a half-stopped replay.
    assert_eq!(
        manager.replay_stop("mock", "p").unwrap_err(),
        "invalid argument: replay is not active for this page"
    );
    assert!(manager
        .replay_start("mock", "p", cassette.path(), false)
        .is_ok());
}

#[test]
fn record_body_read_failure_fails_stop_without_publishing_index() {
    let harness = Harness::new(|method, params| match method {
        "Network.getResponseBody" if params["requestId"] == "r1" => Err("no resource".into()),
        // Base64 for "hello".
        "Network.getResponseBody" => Ok(json!({"base64body": "aGVsbG8=", "evicted": false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let manager = &harness.manager;
    manager.record_start("mock", "p", &dir, &[]).unwrap();
    harness.exchange("r1");
    harness.exchange("r2");
    harness.wait_for("Network.getResponseBody", 2);
    let error = manager.record_stop("mock", "p").unwrap_err();
    assert_eq!(
        error,
        "io error: reading response body for https://example.com/r1 (status 200): no resource"
    );
    assert_eq!(kind(&error).as_deref(), Some("io"));
    assert!(!dir.join("index.json").exists());
    // Later exchanges are still stored, but no index references them.
    assert!(dir
        .join("bodies")
        .join(crate::cli::cassette::sha256(b"hello"))
        .is_file());
    assert!(crate::cli::cassette::load(&dir)
        .unwrap_err()
        .starts_with("invalid argument: invalid cassette: "));
    assert_eq!(
        manager.record_stop("mock", "p").unwrap_err(),
        "invalid argument: recording is not active for this page"
    );
}

#[test]
fn record_evicted_body_fails_stop_without_publishing_index() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body": "", "evicted": true})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let manager = &harness.manager;
    manager.record_start("mock", "p", &dir, &[]).unwrap();
    harness.exchange("r1");
    harness.wait_for("Network.getResponseBody", 1);
    let error = manager.record_stop("mock", "p").unwrap_err();
    assert_eq!(
        error,
        "io error: response body was evicted during recording"
    );
    assert_eq!(kind(&error).as_deref(), Some("io"));
    assert!(!dir.join("index.json").exists());
    assert_eq!(std::fs::read_dir(dir.join("bodies")).unwrap().count(), 0);
}

#[test]
fn record_body_write_failure_fails_stop_without_publishing_index() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body": "aGVsbG8=", "evicted": false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let manager = &harness.manager;
    manager.record_start("mock", "p", &dir, &[]).unwrap();
    std::fs::remove_dir(dir.join("bodies")).unwrap();
    std::fs::write(dir.join("bodies"), b"not a directory").unwrap();
    harness.exchange("r1");
    harness.wait_for("Network.getResponseBody", 1);
    let error = manager.record_stop("mock", "p").unwrap_err();
    assert!(
        error.starts_with("io error: writing cassette body: "),
        "{error}"
    );
    assert!(!dir.join("index.json").exists());
}

#[test]
fn record_malformed_request_body_fails_stop_and_skips_exchange() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body": "aGVsbG8=", "evicted": false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let manager = &harness.manager;
    manager.record_start("mock", "p", &dir, &[]).unwrap();
    harness.page_event(
        "Network.requestWillBeSent",
        json!({"requestId": "bad", "url": "https://example.com/post", "method": "POST", "postData": "!"}),
    );
    harness.page_event(
        "Network.responseReceived",
        json!({"requestId": "bad", "status": 200, "headers": []}),
    );
    harness.page_event("Network.requestFinished", json!({"requestId": "bad"}));
    // A well-formed exchange afterwards proves the worker kept running.
    harness.exchange("r1");
    harness.wait_for("Network.getResponseBody", 1);
    let error = manager.record_stop("mock", "p").unwrap_err();
    assert!(
        error.starts_with("io error: invalid network request body: "),
        "{error}"
    );
    assert_eq!(kind(&error).as_deref(), Some("io"));
    // Only the well-formed exchange read a body.
    let reads = harness.requests("Network.getResponseBody");
    assert_eq!(reads, [json!({"requestId": "r1"})]);
    assert!(!dir.join("index.json").exists());
}

#[test]
fn cassette_commands_on_missing_page_report_page_not_found() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let cassette = empty_cassette();
    let manager = &harness.manager;
    for result in [
        manager.record_start("mock", "p9", &dir, &[]),
        manager.record_stop("mock", "p9"),
        manager.replay_start("mock", "p9", cassette.path(), false),
        manager.replay_stop("mock", "p9"),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error, "page p9 not found");
        assert_eq!(kind(&error).as_deref(), Some("page_not_found"));
    }
    assert!(!dir.exists(), "a missing page must not reserve a cassette");
    assert!(harness
        .requests("Network.setRequestInterception")
        .is_empty());
    // Stop commands on an existing page with nothing active.
    assert_eq!(
        manager.record_stop("mock", "p").unwrap_err(),
        "invalid argument: recording is not active for this page"
    );
    assert_eq!(
        manager.replay_stop("mock", "p").unwrap_err(),
        "invalid argument: replay is not active for this page"
    );
}
