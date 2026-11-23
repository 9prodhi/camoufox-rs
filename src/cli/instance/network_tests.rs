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
        Self::build(reply, None)
    }

    fn ignoring_response_bodies() -> Self {
        Self::build(|_, _| Ok(json!({})), Some("Network.getResponseBody"))
    }

    fn build(
        reply: impl Fn(&str, &Value) -> Result<Value, String> + Send + 'static,
        ignore: Option<&'static str>,
    ) -> Self {
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
                if ignore == Some(method) {
                    continue;
                }
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
                    network: Default::default(),
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
                networks: Default::default(),
                downloads: crate::cli::downloads::DownloadLog::new("ctx".into(), None),
            },
        );
        manager.instances.lock().unwrap()["mock"]
            .networks
            .lock()
            .unwrap()
            .set_finalize_budget(Duration::from_millis(200));
        Self {
            manager,
            events,
            sent,
            closed,
            connection: conn,
            worker: Some(worker),
        }
    }

    fn add_other_page(&self) {
        self.events.send(RawMessage {
            id: None,
            method: Some("Browser.attachedToTarget".into()),
            params: Some(json!({"sessionId":"other", "targetInfo":{"type":"page","targetId":"other-target","browserContextId":"ctx"}})),
            result: None, error: None, session_id: None,
        }).unwrap();
        self.events
            .send(RawMessage {
                id: None,
                method: Some("Page.frameAttached".into()),
                params: Some(json!({"frameId":"other-main"})),
                result: None,
                error: None,
                session_id: Some("other".into()),
            })
            .unwrap();
        self.manager
            .with_instance("mock", |inst, context| {
                let tracked = context
                    .registry()
                    .claim("other-target", Duration::from_secs(2))
                    .unwrap();
                let page = MainFrame::from_tracked(
                    tracked,
                    self.connection.clone(),
                    context.registry().clone(),
                )
                .unwrap();
                inst.pages.insert(
                    "p2".into(),
                    ManagedMainFrame {
                        main_frame: Arc::new(page),
                        network: Default::default(),
                        headers: Vec::new(),
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    fn assert_other_page_works(&self) {
        self.manager
            .with_instance("mock", |inst, _| {
                inst.pages["p2"]
                    .main_frame
                    .session()
                    .send("Test.otherPage", json!({}))
                    .unwrap();
                Ok(())
            })
            .unwrap();
        assert!(!self.closed.load(Ordering::SeqCst));
        assert!(self.requests("Browser.close").is_empty());
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
        serde_json::to_vec(&json!({"format":crate::cli::cassette::FORMAT,"version":crate::cli::cassette::VERSION,"exchanges":[]})).unwrap(),
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
        "disable failed"
    );
    assert!(manager
        .replay_start("mock", "p", cassette.path(), false)
        .is_ok());
}

#[test]
fn record_unreadable_bodies_are_skipped_and_index_is_published() {
    let harness = Harness::new(|method, params| match method {
        "Network.getResponseBody" if params["requestId"] == "unreadable" => {
            Err("no resource".into())
        }
        "Network.getResponseBody" if params["requestId"] == "evicted" => {
            Ok(json!({"base64body":"", "evicted":true}))
        }
        "Network.getResponseBody" => Ok(json!({"base64body":"aGVsbG8=", "evicted":false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    for id in ["unreadable", "evicted", "good"] {
        harness.exchange(id);
    }
    harness.wait_for("Network.getResponseBody", 3);
    let result = harness.manager.record_stop("mock", "p").unwrap();
    assert_eq!(result["exchanges"], 1);
    let skipped = result["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2);
    assert_eq!(skipped[0]["method"], "GET");
    assert_eq!(skipped[0]["url"], "https://example.com/unreadable");
    assert!(skipped[0]["reason"]
        .as_str()
        .unwrap()
        .contains("no resource"));
    assert!(skipped[1]["reason"].as_str().unwrap().contains("evicted"));
    let (index, _) = crate::cli::cassette::load(&dir).unwrap();
    assert_eq!(index.exchanges.len(), 1);
    assert_eq!(index.exchanges[0].url, "https://example.com/good");
}

#[test]
fn record_body_read_failure_is_skipped_without_losing_later_exchanges() {
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
    let result = manager.record_stop("mock", "p").unwrap();
    assert_eq!(result["exchanges"], 1);
    assert_eq!(
        result["skipped"],
        json!([{
            "method": "GET",
            "url": "https://example.com/r1",
            "reason": "reading response body (status 200): no resource"
        }])
    );
    assert!(dir
        .join("bodies")
        .join(crate::cli::cassette::sha256(b"hello"))
        .is_file());
    let (index, _) = crate::cli::cassette::load(&dir).unwrap();
    assert_eq!(index.exchanges.len(), 1);
    assert_eq!(index.exchanges[0].url, "https://example.com/r2");
    let mut repeated = result;
    repeated["repeated"] = json!(true);
    assert_eq!(manager.record_stop("mock", "p").unwrap(), repeated);
}

#[test]
fn record_evicted_body_is_skipped_without_storing_body() {
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
    let result = manager.record_stop("mock", "p").unwrap();
    assert_eq!(result["exchanges"], 0);
    assert_eq!(
        result["skipped"],
        json!([{
            "method": "GET",
            "url": "https://example.com/r1",
            "reason": "response body was evicted during recording"
        }])
    );
    assert_eq!(std::fs::read_dir(dir.join("bodies")).unwrap().count(), 0);
    let (index, _) = crate::cli::cassette::load(&dir).unwrap();
    assert!(index.exchanges.is_empty());
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

#[test]
fn network_stops_repeat_the_last_result_and_work_with_dialogs() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.page_event(
        "Page.dialogOpened",
        json!({"dialogId":"d", "type":"alert", "message":"hello", "defaultValue":""}),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if harness
            .manager
            .with_instance("mock", |inst, _| {
                Ok(inst.pages["p"].main_frame.pending_dialog().is_some())
            })
            .unwrap()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let mut result = harness.manager.record_stop("mock", "p").unwrap();
    result["repeated"] = json!(true);
    assert_eq!(harness.manager.record_stop("mock", "p").unwrap(), result);
    harness.page_event("Page.dialogClosed", json!({"dialogId":"d"}));
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness
        .manager
        .with_instance("mock", |inst, _| {
            Ok(inst.pages["p"].main_frame.pending_dialog().is_some())
        })
        .unwrap()
    {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    harness
        .manager
        .replay_start("mock", "p", &dir, false)
        .unwrap();
    harness.page_event(
        "Page.dialogOpened",
        json!({"dialogId":"e", "type":"alert", "message":"hello", "defaultValue":""}),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !harness
        .manager
        .with_instance("mock", |inst, _| {
            Ok(inst.pages["p"].main_frame.pending_dialog().is_some())
        })
        .unwrap()
    {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let mut result = harness.manager.replay_stop("mock", "p").unwrap();
    result["repeated"] = json!(true);
    assert_eq!(harness.manager.replay_stop("mock", "p").unwrap(), result);
}

#[test]
fn network_cleanup_drains_recording_before_stop_and_shutdown_close_connection() {
    for shutdown in [false, true] {
        let (release, wait) = mpsc::channel();
        let harness = Harness::new(move |method, _| {
            if method == "Network.getResponseBody" {
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(json!({"base64body":"aGVsbG8=", "evicted":false}))
            } else {
                Ok(json!({}))
            }
        });
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        harness
            .manager
            .record_start("mock", "p", &dir, &[])
            .unwrap();
        harness.exchange("good");
        harness.wait_for("Network.getResponseBody", 1);
        std::thread::scope(|scope| {
            let stopped = scope.spawn(|| {
                if shutdown {
                    harness.manager.shutdown_all();
                } else {
                    harness.manager.stop("mock").unwrap();
                }
            });
            // The peer can answer only after cleanup starts; closing its
            // connection first loses this completed response.
            let deadline = Instant::now() + Duration::from_secs(2);
            while harness.connection.event_handler_count_for("page", "*") != 0
                && !harness.closed.load(Ordering::SeqCst)
            {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let closed_early = harness.closed.load(Ordering::SeqCst);
            release.send(()).unwrap();
            stopped.join().unwrap();
            assert!(
                !closed_early,
                "connection closed before the recording drained"
            );
        });
        let (index, _) = crate::cli::cassette::load(&dir).unwrap();
        assert_eq!(index.exchanges.len(), 1, "shutdown={shutdown}");
    }
}

#[test]
fn replay_without_body_matching_does_not_decode_post_data() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body":"aGVsbG8=", "evicted":false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("good");
    harness.wait_for("Network.getResponseBody", 1);
    harness.manager.record_stop("mock", "p").unwrap();
    harness
        .manager
        .replay_start("mock", "p", &dir, false)
        .unwrap();
    harness.page_event("Network.requestWillBeSent", json!({"requestId":"replay", "url":"https://example.com/good", "method":"GET", "isIntercepted":true, "postData":"!"}));
    harness.wait_for("Network.fulfillInterceptedRequest", 1);
    assert_eq!(
        harness.manager.replay_stop("mock", "p").unwrap()["served"],
        1
    );
}

#[test]
fn replay_reads_bodies_at_fulfillment_and_fails_closed_on_changes() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body":"aGVsbG8=", "evicted":false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("good");
    harness.wait_for("Network.getResponseBody", 1);
    harness.manager.record_stop("mock", "p").unwrap();
    harness
        .manager
        .replay_start("mock", "p", &dir, false)
        .unwrap();
    std::fs::write(
        dir.join("bodies")
            .join(crate::cli::cassette::sha256(b"hello")),
        b"wrong",
    )
    .unwrap();
    harness.page_event("Network.requestWillBeSent", json!({"requestId":"replay", "url":"https://example.com/good", "method":"GET", "isIntercepted":true}));
    harness.wait_for("Network.abortInterceptedRequest", 1);
    assert!(harness
        .requests("Network.fulfillInterceptedRequest")
        .is_empty());
    assert!(harness
        .manager
        .replay_stop("mock", "p")
        .unwrap_err()
        .starts_with("io error:"));
}

#[test]
fn replay_misses_are_counted_deduplicated_and_bounded() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let dir = empty_cassette();
    harness
        .manager
        .replay_start("mock", "p", dir.path(), false)
        .unwrap();
    for i in 0..1003 {
        harness.page_event("Network.requestWillBeSent", json!({"requestId":i.to_string(), "url":format!("https://example.com/{}", i % 1001), "method":"GET", "isIntercepted":true}));
    }
    harness.wait_for("Network.abortInterceptedRequest", 1003);
    let result = harness.manager.replay_stop("mock", "p").unwrap();
    assert_eq!(result["missed_total"], 1003);
    let misses = result["missed"].as_array().unwrap();
    assert_eq!(misses.len(), 1000);
    assert_eq!(
        misses[0],
        json!({"method":"GET", "url":"https://example.com/0", "count":2})
    );
    assert_eq!(misses[1]["count"], 2);
    assert_eq!(misses[999]["count"], 1);
}

#[test]
fn multipart_boundary_normalization_matches_record_and_replay() {
    use base64::Engine;
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body":"aGVsbG8=", "evicted":false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    let request = |boundary: &str, intercepted: bool| {
        json!({
            "requestId":boundary, "url":"https://example.com/form", "method":"POST", "isIntercepted":intercepted,
            "headers":[{"name":"cOnTeNt-TyPe", "value":format!("multipart/form-data; boundary=\"{boundary}\"")}],
            "postData":base64::engine::general_purpose::STANDARD.encode(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n--{boundary}--\r\n"))
        })
    };
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.page_event(
        "Network.requestWillBeSent",
        request("first-boundary", false),
    );
    harness.page_event(
        "Network.responseReceived",
        json!({"requestId":"first-boundary", "status":200, "headers":[]}),
    );
    harness.page_event(
        "Network.requestFinished",
        json!({"requestId":"first-boundary"}),
    );
    harness.wait_for("Network.getResponseBody", 1);
    harness.manager.record_stop("mock", "p").unwrap();
    harness
        .manager
        .replay_start("mock", "p", &dir, true)
        .unwrap();
    for boundary in ["second-boundary", "third-boundary"] {
        harness.page_event("Network.requestWillBeSent", request(boundary, true));
    }
    harness.wait_for("Network.fulfillInterceptedRequest", 2);
    let mut different = request("third-boundary", true);
    different["postData"] = json!(base64::engine::general_purpose::STANDARD.encode("--third-boundary\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\ndifferent\r\n--third-boundary--\r\n"));
    harness.page_event("Network.requestWillBeSent", different);
    harness.wait_for("Network.abortInterceptedRequest", 1);
    let result = harness.manager.replay_stop("mock", "p").unwrap();
    assert_eq!(result["served"], 2);
    assert_eq!(result["missed_total"], 1);
}

#[test]
fn recording_join_does_not_hold_instance_lock() {
    let (release, wait) = mpsc::channel();
    let harness = Harness::new(move |method, _| {
        if method == "Network.getResponseBody" {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(json!({"base64body":"aGVsbG8=", "evicted":false}))
        } else {
            Ok(json!({}))
        }
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("good");
    harness.wait_for("Network.getResponseBody", 1);
    let entry = harness.manager.instances.lock().unwrap()["mock"]
        .managed
        .clone();
    std::thread::scope(|scope| {
        let stop = scope.spawn(|| harness.manager.record_stop("mock", "p"));
        // Wait until stop reaches the drain, then a page-independent operation
        // on the same instance must still acquire the instance lock.
        let deadline = Instant::now() + Duration::from_secs(2);
        while harness.connection.event_handler_count_for("page", "*") != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let acquired = entry.try_lock().is_ok();
        release.send(()).unwrap();
        stop.join().unwrap().unwrap();
        assert!(acquired, "record stop held the instance lock while joining");
    });
}

#[test]
fn replay_validation_rejects_fifo_index_without_holding_instance_lock() {
    use std::os::unix::ffi::OsStrExt;
    let harness = Harness::new(|_, _| Ok(json!({})));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.json");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let entry = harness.manager.instances.lock().unwrap()["mock"]
        .managed
        .clone();
    let state = entry.lock().unwrap().instance.as_ref().unwrap().pages["p"]
        .network
        .clone();
    let operation = state.lock().unwrap();
    let owners = Arc::strong_count(&state);
    std::thread::scope(|scope| {
        let start = scope.spawn(|| harness.manager.replay_start("mock", "p", dir.path(), false));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&state) == owners {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // The operation waits for page state only after releasing the instance.
        while entry.try_lock().is_err() {
            assert!(
                Instant::now() < deadline,
                "replay validation held the instance lock"
            );
            std::thread::yield_now();
        }
        drop(operation);
        assert!(start
            .join()
            .unwrap()
            .unwrap_err()
            .contains("not a regular file"));
    });
}

#[test]
fn closed_page_cleanup_does_not_join_under_instance_lock() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    let entry = harness.manager.instances.lock().unwrap()["mock"]
        .managed
        .clone();
    let state = entry.lock().unwrap().instance.as_ref().unwrap().pages["p"]
        .network
        .clone();
    // A concurrent network operation owns the page mutex until released.
    let operation = state.lock().unwrap();
    harness.connection.dispose_session("page");
    std::thread::scope(|scope| {
        let cleanup = scope.spawn(|| {
            harness
                .manager
                .with_instance("mock", |inst, _| Ok(inst.page_ids()))
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut unlocked = false;
        while Instant::now() < deadline {
            if let Ok(guard) = entry.try_lock() {
                if guard.instance.as_ref().unwrap().pages.is_empty() {
                    unlocked = true;
                    break;
                }
            }
            std::thread::yield_now();
        }
        drop(operation);
        assert!(cleanup.join().unwrap().unwrap().is_empty());
        assert!(
            unlocked,
            "closed-page cleanup held the instance lock while joining"
        );
    });
    assert!(dir.join("index.json").is_file());
}

#[test]
fn record_filters_discard_non_http_and_excluded_requests() {
    let harness = Harness::new(|method, _| match method {
        "Network.getResponseBody" => Ok(json!({"base64body":"aGVsbG8=", "evicted":false})),
        _ => Ok(json!({})),
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &["/keep$".into()])
        .unwrap();
    for url in [
        "data:text/plain,keep",
        "file:///keep",
        "https://example.com/excluded",
        "https://example.com/keep",
    ] {
        harness.page_event(
            "Network.requestWillBeSent",
            json!({"requestId":url, "url":url, "method":"GET"}),
        );
        harness.page_event(
            "Network.responseReceived",
            json!({"requestId":url, "status":200, "headers":[]}),
        );
        harness.page_event("Network.requestFinished", json!({"requestId":url}));
    }
    harness.wait_for("Network.getResponseBody", 1);
    assert_eq!(
        harness.manager.record_stop("mock", "p").unwrap()["exchanges"],
        1
    );
    assert_eq!(
        harness.requests("Network.getResponseBody"),
        [json!({"requestId":"https://example.com/keep"})]
    );
}

#[test]
fn daemon_shutdown_drains_recording_before_closing_connections() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let (release, wait) = mpsc::channel();
    let mut harness = Harness::new(move |method, _| {
        if method == "Network.getResponseBody" {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(json!({"base64body":"aGVsbG8=", "evicted":false}))
        } else {
            Ok(json!({}))
        }
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("good");
    harness.wait_for("Network.getResponseBody", 1);
    let manager = Arc::new(std::mem::replace(
        &mut harness.manager,
        InstanceManager::new(),
    ));
    let path = parent.path().join("daemon.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server_manager = manager.clone();
    let server = std::thread::spawn(move || {
        crate::cli::daemon::serve_on(listener, server_manager, Arc::new(AtomicBool::new(false)))
    });
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client.write_all(b"{\"method\":\"Shutdown\"}\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness.connection.event_handler_count_for("page", "*") != 0
        && !harness.closed.load(Ordering::SeqCst)
    {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let closed_early = harness.closed.load(Ordering::SeqCst);
    release.send(()).unwrap();
    let mut response = String::new();
    BufReader::new(client).read_line(&mut response).unwrap();
    server.join().unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["ok"],
        true
    );
    assert!(
        !closed_early,
        "daemon closed the connection before draining recording"
    );
    assert_eq!(
        crate::cli::cassette::load(&dir).unwrap().0.exchanges.len(),
        1
    );
}

#[test]
fn concurrent_instance_stop_and_daemon_cleanup_wait_for_the_same_drain() {
    let (release, wait) = mpsc::channel();
    let harness = Harness::new(move |method, _| {
        if method == "Network.getResponseBody" {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(json!({"base64body":"aGVsbG8=", "evicted":false}))
        } else {
            Ok(json!({}))
        }
    });
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("good");
    harness.wait_for("Network.getResponseBody", 1);
    std::thread::scope(|scope| {
        let daemon_cleanup = scope.spawn(|| harness.manager.close_all_connections());
        let deadline = Instant::now() + Duration::from_secs(2);
        while harness.connection.event_handler_count_for("page", "*") != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let (done, finished) = mpsc::channel();
        let manager = &harness.manager;
        let stop = scope.spawn(move || {
            manager.stop("mock").unwrap();
            done.send(()).unwrap();
        });
        let closed_early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
        release.send(()).unwrap();
        daemon_cleanup.join().unwrap();
        stop.join().unwrap();
        assert!(
            !closed_early,
            "concurrent cleanup bypassed a drain already in progress"
        );
    });
    assert_eq!(
        crate::cli::cassette::load(&dir).unwrap().0.exchanges.len(),
        1
    );
}

#[test]
fn failed_network_start_clears_previous_stop_result() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.manager.record_stop("mock", "p").unwrap();
    assert!(harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .is_err());
    assert_eq!(
        harness.manager.record_stop("mock", "p").unwrap_err(),
        "invalid argument: recording is not active for this page"
    );
    harness
        .manager
        .replay_start("mock", "p", &dir, false)
        .unwrap();
    harness.manager.replay_stop("mock", "p").unwrap();
    let invalid = parent.path().join("nonempty");
    std::fs::create_dir(&invalid).unwrap();
    std::fs::write(invalid.join("keep"), "keep").unwrap();
    assert!(harness
        .manager
        .replay_start("mock", "p", &invalid, false)
        .is_err());
    assert_eq!(
        harness.manager.replay_stop("mock", "p").unwrap_err(),
        "invalid argument: replay is not active for this page"
    );
}

#[test]
fn network_finalize_deadline_bounds_cleanup_and_publishes_index() {
    let started = Instant::now();
    for action in ["close-tab", "stop", "shutdown"] {
        let harness = Harness::ignoring_response_bodies();
        harness.add_other_page();
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        harness
            .manager
            .record_start("mock", "p", &dir, &[])
            .unwrap();
        if action == "close-tab" {
            harness
                .manager
                .record_start("mock", "p2", &parent.path().join("other"), &[])
                .unwrap();
        }
        let (state, page) = harness
            .manager
            .with_instance("mock", |inst, _| {
                let page = &inst.pages["p"];
                Ok((page.network.clone(), page.main_frame.clone()))
            })
            .unwrap();
        harness.exchange("first");
        harness.wait_for("Network.getResponseBody", 1);
        harness.exchange("second");
        let (sent, received) = mpsc::channel();
        harness.connection.on_event(
            "page",
            "Test.queued",
            Arc::new(move |_| {
                let _ = sent.send(());
            }),
        );
        harness.page_event("Test.queued", json!({}));
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::scope(|scope| {
            let (done, finished) = mpsc::channel();
            let manager = &harness.manager;
            let worker = scope.spawn(move || {
                match action {
                    "stop" => manager.stop("mock").unwrap(),
                    "shutdown" => manager.shutdown_all(),
                    _ => {
                        manager.close_tab("mock", "p").unwrap();
                    }
                }
                done.send(()).unwrap();
            });
            let bounded = finished.recv_timeout(Duration::from_secs(1)).is_ok();
            if !bounded {
                let _ = harness.connection.close();
            }
            worker.join().unwrap();
            assert!(bounded, "{action} exceeded its total finalize budget");
        });
        let (index, _) = crate::cli::cassette::load(&dir).unwrap();
        assert!(index.exchanges.is_empty(), "{action}");
        let result = state.lock().unwrap().stop(true, &page).unwrap();
        assert_eq!(result["skipped"].as_array().unwrap().len(), 2, "{result}");
        for skip in result["skipped"].as_array().unwrap() {
            assert_eq!(skip["reason"], "finalize deadline exceeded");
        }
        if action == "close-tab" {
            harness.assert_other_page_works();
            assert_eq!(
                harness.manager.record_stop("mock", "p2").unwrap()["exchanges"],
                0
            );
            assert!(parent.path().join("other/index.json").is_file());
        } else {
            assert!(harness.closed.load(Ordering::SeqCst));
        }
        assert_eq!(harness.requests("Network.getResponseBody").len(), 1);
    }
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn network_finalize_deadline_marks_all_pages_before_draining() {
    let harness = Harness::ignoring_response_bodies();
    harness
        .manager
        .with_instance("mock", |inst, _| {
            let main_frame = inst.pages["p"].main_frame.clone();
            inst.pages.insert(
                "p2".into(),
                ManagedMainFrame {
                    main_frame,
                    network: Default::default(),
                    headers: Vec::new(),
                },
            );
            Ok(())
        })
        .unwrap();
    let parent = tempfile::tempdir().unwrap();
    for page in ["p", "p2"] {
        harness
            .manager
            .record_start("mock", page, &parent.path().join(page), &[])
            .unwrap();
    }
    let states = harness
        .manager
        .with_instance("mock", |inst, _| {
            Ok(["p", "p2"].map(|id| {
                (
                    inst.pages[id].network.clone(),
                    inst.pages[id].main_frame.clone(),
                )
            }))
        })
        .unwrap();
    harness.exchange("first");
    harness.wait_for("Network.getResponseBody", 1);
    let (sent, received) = mpsc::channel();
    harness.connection.on_event(
        "page",
        "Test.queued",
        Arc::new(move |_| {
            let _ = sent.send(());
        }),
    );
    harness.page_event("Test.queued", json!({}));
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| harness.manager.record_stop("mock", "p"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while harness.connection.event_handler_count_for("page", "*") == 2 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        harness.manager.stop("mock").unwrap();
        let result = stopping.join().unwrap().unwrap();
        assert_eq!(result["skipped"][0]["reason"], "finalize deadline exceeded");
    });
    for (state, page) in states {
        let result = state.lock().unwrap().stop(true, &page).unwrap();
        assert_eq!(
            result["skipped"][0]["reason"], "finalize deadline exceeded",
            "{result}"
        );
    }
}

#[test]
fn failed_start_with_dialog_clears_previous_stop_result() {
    for recording in [true, false] {
        let harness = Harness::new(|_, _| Ok(json!({})));
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        harness
            .manager
            .record_start("mock", "p", &dir, &[])
            .unwrap();
        harness.manager.record_stop("mock", "p").unwrap();
        if !recording {
            harness
                .manager
                .replay_start("mock", "p", &dir, false)
                .unwrap();
            harness.manager.replay_stop("mock", "p").unwrap();
        }
        harness.page_event(
            "Page.dialogOpened",
            json!({"dialogId":"d", "type":"alert", "message":"hello", "defaultValue":""}),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !harness
            .manager
            .with_instance("mock", |inst, _| {
                Ok(inst.pages["p"].main_frame.pending_dialog().is_some())
            })
            .unwrap()
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let error = if recording {
            harness
                .manager
                .record_start("mock", "p", &parent.path().join("next"), &[])
                .unwrap_err()
        } else {
            harness
                .manager
                .replay_start("mock", "p", &dir, false)
                .unwrap_err()
        };
        assert_eq!(kind(&error).as_deref(), Some("dialog_open"));
        let stopped = if recording {
            harness.manager.record_stop("mock", "p")
        } else {
            harness.manager.replay_stop("mock", "p")
        };
        let name = if recording { "recording" } else { "replay" };
        assert_eq!(
            stopped.unwrap_err(),
            format!("invalid argument: {name} is not active for this page")
        );
    }
}

#[test]
fn closed_page_stop_returns_finalized_record_and_replay_results() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.manager.record_stop("mock", "p").unwrap();
    harness
        .manager
        .replay_start("mock", "p", &dir, false)
        .unwrap();
    harness.manager.close_tab("mock", "p").unwrap();
    let recorded = harness.manager.record_stop("mock", "p").unwrap();
    assert_eq!(recorded["exchanges"], 0);
    assert_eq!(recorded["repeated"], true);
    assert_eq!(recorded["page_closed"], true);
    let replayed = harness.manager.replay_stop("mock", "p").unwrap();
    assert_eq!(replayed["served"], 0);
    assert_eq!(replayed["repeated"], true);
    assert_eq!(replayed["page_closed"], true);
    assert_eq!(
        harness.manager.record_stop("mock", "never").unwrap_err(),
        "page never not found"
    );
}

#[test]
fn popup_self_close_preserves_cassette_and_other_pages() {
    let harness = Harness::ignoring_response_bodies();
    harness.add_other_page();
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("popup-response");
    harness.wait_for("Network.getResponseBody", 1);
    let entry = harness.manager.instances.lock().unwrap()["mock"]
        .managed
        .clone();
    let state = entry.lock().unwrap().instance.as_ref().unwrap().pages["p"]
        .network
        .clone();
    let operation = state.lock().unwrap();
    // The page detaches itself while another network operation owns its lock.
    harness.connection.dispose_session("page");
    std::thread::scope(|scope| {
        let cleanup = scope.spawn(|| {
            harness
                .manager
                .with_instance("mock", |inst, _| Ok(inst.page_ids()))
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(entry) = entry.try_lock() {
                if !entry.instance.as_ref().unwrap().pages.contains_key("p") {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "popup was not removed");
            std::thread::yield_now();
        }
        // More than the injected budget must not close other pages while waiting.
        std::thread::sleep(Duration::from_millis(400));
        let closed_early = harness.closed.load(Ordering::SeqCst);
        drop(operation);
        assert_eq!(cleanup.join().unwrap().unwrap(), ["p2"]);
        assert!(
            !closed_early,
            "popup cleanup closed the shared connection while waiting for its network lock"
        );
    });
    harness.assert_other_page_works();
    assert!(dir.join("index.json").is_file());
    let result = harness.manager.record_stop("mock", "p").unwrap();
    assert_eq!(result["page_closed"], true);
    assert_eq!(result["repeated"], true);
}

#[test]
fn cassette_failure_is_reported_by_close_tab_and_stop() {
    for (action, failure) in ["close-tab", "stop"]
        .into_iter()
        .flat_map(|action| ["index", "body", "replay"].map(|failure| (action, failure)))
    {
        let harness = Harness::new(|method, _| {
            Ok(if method == "Network.getResponseBody" {
                json!({"base64body":"aGVsbG8=", "evicted":false})
            } else {
                json!({})
            })
        });
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cassette");
        harness
            .manager
            .record_start("mock", "p", &dir, &[])
            .unwrap();
        match failure {
            "index" => {
                // A directory at the destination makes the atomic rename fail.
                std::fs::create_dir(dir.join("index.json")).unwrap();
            }
            "body" => {
                std::fs::remove_dir(dir.join("bodies")).unwrap();
                std::fs::write(dir.join("bodies"), b"not a directory").unwrap();
                harness.exchange("good");
                harness.wait_for("Network.getResponseBody", 1);
            }
            _ => {
                harness.exchange("good");
                harness.wait_for("Network.getResponseBody", 1);
                harness.manager.record_stop("mock", "p").unwrap();
                harness
                    .manager
                    .replay_start("mock", "p", &dir, false)
                    .unwrap();
                std::fs::write(
                    dir.join("bodies")
                        .join(crate::cli::cassette::sha256(b"hello")),
                    b"wrong",
                )
                .unwrap();
                harness.page_event("Network.requestWillBeSent", json!({"requestId":"changed", "method":"GET", "url":"https://example.com/good", "isIntercepted":true}));
                harness.wait_for("Network.abortInterceptedRequest", 1);
            }
        }
        let data = if action == "close-tab" {
            harness.manager.close_tab_with_result("mock", "p").unwrap()
        } else {
            harness.manager.stop_with_result("mock").unwrap()
        };
        let errors = data["cassette_errors"]
            .as_array()
            .expect("cassette_errors missing");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["page_id"], "p");
        assert_eq!(
            errors[0]["mode"],
            if failure == "replay" {
                "replay"
            } else {
                "record"
            }
        );
        let error = errors[0]["error"].as_str().unwrap();
        assert!(error.starts_with("io error:"), "{error}");
        let rendered = crate::cli::output::render(Some(&data));
        assert_eq!(
            rendered.stderr,
            [format!("warning: cassette for p not published: {error}")]
        );
        if action == "close-tab" {
            let stopped = if failure == "replay" {
                harness.manager.replay_stop("mock", "p")
            } else {
                harness.manager.record_stop("mock", "p")
            };
            assert_eq!(stopped.unwrap_err(), error);
        }
    }
}

#[test]
fn closed_page_results_keep_only_the_last_64_pages() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    for i in 0..66 {
        let id = format!("closed{i}");
        harness
            .manager
            .with_instance("mock", |inst, _| {
                inst.pages.insert(
                    id.clone(),
                    ManagedMainFrame {
                        main_frame: inst.pages["p"].main_frame.clone(),
                        network: Default::default(),
                        headers: Vec::new(),
                    },
                );
                Ok(())
            })
            .unwrap();
        harness
            .manager
            .record_start("mock", &id, &parent.path().join(&id), &[])
            .unwrap();
        harness.manager.close_tab("mock", &id).unwrap();
    }
    for i in 0..2 {
        assert_eq!(
            harness
                .manager
                .record_stop("mock", &format!("closed{i}"))
                .unwrap_err(),
            format!("page closed{i} was closed")
        );
    }
    for i in 2..66 {
        let result = harness
            .manager
            .record_stop("mock", &format!("closed{i}"))
            .unwrap();
        assert_eq!(result["repeated"], true);
        assert_eq!(result["page_closed"], true);
    }
}

#[test]
fn page_finalize_budget_starts_after_the_network_lock_is_acquired() {
    let harness = Harness::ignoring_response_bodies();
    harness.add_other_page();
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("cassette");
    harness
        .manager
        .record_start("mock", "p", &dir, &[])
        .unwrap();
    harness.exchange("blocked");
    harness.wait_for("Network.getResponseBody", 1);
    let state = harness
        .manager
        .with_instance("mock", |inst, _| Ok(inst.pages["p"].network.clone()))
        .unwrap();
    let operation = state.lock().unwrap();
    let owners = Arc::strong_count(&state);
    std::thread::scope(|scope| {
        let (done, finished) = mpsc::channel();
        let manager = &harness.manager;
        let cleanup = scope.spawn(move || {
            let result = manager.close_tab("mock", "p");
            done.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&state) == owners {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(400));
        let closed_early = harness.closed.load(Ordering::SeqCst);
        let started = Instant::now();
        drop(operation);
        let result = finished.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            let _ = harness.connection.close();
        }
        cleanup.join().unwrap();
        assert!(
            !closed_early,
            "waiting for the network lock closed the connection"
        );
        result.expect("cleanup exceeded its budget").unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "lock wait consumed the finalize budget"
        );
    });
    harness.assert_other_page_works();
    assert!(dir.join("index.json").is_file());
}

#[test]
fn page_replay_finalize_deadline_bounds_protocol_reads() {
    let harness = Harness::build(
        |_, _| Ok(json!({})),
        Some("Network.abortInterceptedRequest"),
    );
    harness.add_other_page();
    let dir = empty_cassette();
    harness
        .manager
        .replay_start("mock", "p", dir.path(), false)
        .unwrap();
    harness.page_event("Network.requestWillBeSent", json!({"requestId":"miss", "url":"https://example.com/miss", "method":"GET", "isIntercepted":true}));
    harness.wait_for("Network.abortInterceptedRequest", 1);
    std::thread::scope(|scope| {
        let (done, finished) = mpsc::channel();
        let manager = &harness.manager;
        let cleanup = scope.spawn(move || done.send(manager.close_tab("mock", "p")).unwrap());
        let result = finished.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            let _ = harness.connection.close();
        }
        cleanup.join().unwrap();
        result.expect("replay cleanup exceeded its budget").unwrap();
    });
    harness.assert_other_page_works();
    let result = harness.manager.replay_stop("mock", "p").unwrap();
    assert_eq!(result["page_closed"], true);
}

#[test]
fn closing_pages_without_sessions_evicts_old_stop_results() {
    let harness = Harness::new(|_, _| Ok(json!({})));
    let parent = tempfile::tempdir().unwrap();
    harness
        .manager
        .record_start("mock", "p", &parent.path().join("cassette"), &[])
        .unwrap();
    let frame = harness
        .manager
        .with_instance("mock", |inst, _| Ok(inst.pages["p"].main_frame.clone()))
        .unwrap();
    harness.manager.close_tab("mock", "p").unwrap();
    for i in 0..64 {
        let id = format!("empty{i}");
        harness
            .manager
            .with_instance("mock", |inst, _| {
                inst.pages.insert(
                    id.clone(),
                    ManagedMainFrame {
                        main_frame: frame.clone(),
                        network: Default::default(),
                        headers: Vec::new(),
                    },
                );
                Ok(())
            })
            .unwrap();
        harness.manager.close_tab("mock", &id).unwrap();
    }
    assert_eq!(
        harness.manager.record_stop("mock", "p").unwrap_err(),
        "page p was closed"
    );
}
