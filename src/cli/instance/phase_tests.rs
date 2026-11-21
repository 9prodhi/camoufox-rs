//! Phase-specific deadline tests. Only the named phase stalls; all other
//! protocol replies are immediate. The 300ms budget has 3x assertion headroom,
//! while a reverted fresh/hardcoded budget receives a reply after 2.7s.
use super::*;
use crate::api::browser::BrowserOptions;
use crate::protocol::client::Connection;
use crate::protocol::types::RawMessage;
use crate::transport::errors::TransportError;
use crate::transport::{Transport, TransportReader, TransportWriter};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Hop,
    Describe,
    ParentFocus,
    TargetFocus,
    FillEval,
    TypeDispatch,
    KeyDown,
    KeyUp,
    FillDispatch,
    Dispose,
    ClickMove,
    ClickDown,
    ClickUp,
    HoverMove,
    ChildContextWait,
}

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

struct Harness {
    instance: Instance,
    closed: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    stalled: Arc<AtomicBool>,
}
impl Harness {
    fn new(phase: Phase) -> Self {
        let (incoming, incoming_rx) = mpsc::channel();
        let (outgoing, outgoing_rx) = mpsc::channel::<Value>();
        let closed = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let connection = Connection::new(Box::new(MockTransport(
            Writer(outgoing, closed.clone()),
            Reader(incoming_rx, closed.clone()),
        )));
        let stop = closed.clone();
        let seen = stalled.clone();
        let replies = incoming.clone();
        let worker = std::thread::spawn(move || {
            let mut pending = Vec::<(Instant, RawMessage)>::new();
            while !stop.load(Ordering::SeqCst) {
                let now = Instant::now();
                let mut i = 0;
                while i < pending.len() {
                    if pending[i].0 <= now {
                        let (_, reply) = pending.swap_remove(i);
                        let _ = replies.send(reply);
                    } else {
                        i += 1;
                    }
                }
                let Ok(sent) = outgoing_rx.recv_timeout(Duration::from_millis(2)) else {
                    continue;
                };
                let method = sent["method"].as_str().unwrap();
                let expression = sent["params"]["expression"].as_str().unwrap_or("");
                let hop = method == "Runtime.evaluate" && sent["params"]["returnByValue"] == false;
                let parent_focus =
                    method == "Runtime.evaluate" && expression.contains("iframe disappeared");
                let target_focus =
                    method == "Runtime.evaluate" && expression.contains("window.focus()");
                let fill_eval =
                    method == "Runtime.evaluate" && expression.contains("el.tagName.toLowerCase()");
                let stall = match phase {
                    Phase::Hop => hop,
                    Phase::Describe => method == "Page.describeNode",
                    Phase::ParentFocus => parent_focus,
                    Phase::TargetFocus => target_focus,
                    Phase::FillEval => fill_eval,
                    Phase::TypeDispatch | Phase::FillDispatch => method == "Page.insertText",
                    Phase::Dispose => method == "Runtime.disposeObject",
                    Phase::ClickMove | Phase::HoverMove => {
                        method == "Page.dispatchMouseEvent" && sent["params"]["type"] == "mousemove"
                    }
                    Phase::ClickDown => {
                        method == "Page.dispatchMouseEvent" && sent["params"]["type"] == "mousedown"
                    }
                    Phase::ClickUp => {
                        method == "Page.dispatchMouseEvent" && sent["params"]["type"] == "mouseup"
                    }
                    Phase::ChildContextWait => false,
                    Phase::KeyDown => {
                        method == "Page.dispatchKeyEvent" && sent["params"]["type"] == "keydown"
                    }
                    Phase::KeyUp => {
                        method == "Page.dispatchKeyEvent" && sent["params"]["type"] == "keyup"
                    }
                };
                if phase == Phase::ChildContextWait && method == "Runtime.disposeObject" {
                    seen.store(true, Ordering::SeqCst);
                }
                let value = if hop {
                    json!({"result":{"objectId":"iframe-object"}})
                } else if method == "Page.describeNode" {
                    json!({"contentFrameId":"child"})
                } else if target_focus {
                    json!({"result":{"value":true}})
                } else if fill_eval {
                    json!({"result":{"value":"input"}})
                } else if expression.contains("const sx =") {
                    json!({"result":{"value":[0,0,1,1]}})
                } else if expression.contains("window.scrollX, window.scrollY") {
                    json!({"result":{"value":[10,10,16,16,0,0]}})
                } else if expression.contains("return true;") {
                    json!({"result":{"value":true}})
                } else if method == "Runtime.evaluate" {
                    json!({"result":{"value":"text"}})
                } else {
                    json!({})
                };
                let reply = RawMessage {
                    id: sent["id"].as_i64(),
                    method: None,
                    params: None,
                    result: Some(value),
                    error: None,
                    session_id: sent["sessionId"].as_str().map(str::to_owned),
                };
                if stall {
                    seen.store(true, Ordering::SeqCst);
                    pending.push((Instant::now() + Duration::from_millis(2700), reply));
                } else {
                    let _ = replies.send(reply);
                }
            }
        });
        let session = connection.root_session();
        let browser = Browser::connect(connection, session, BrowserOptions::default()).unwrap();
        let connection = browser.connection().clone();
        let registry = crate::api::page_registry::PageRegistry::install(
            &connection,
            "test",
            DialogPolicy::Manual,
        );
        let event = |method: &str, params: Value, session: Option<&str>| {
            incoming
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
        event(
            "Browser.attachedToTarget",
            json!({"sessionId":"page", "targetInfo":{"type":"page","targetId":"target","browserContextId":"test"}}),
            None,
        );
        for (frame, parent) in [("main", None), ("child", Some("main"))] {
            event(
                "Page.frameAttached",
                json!({"frameId":frame,"parentFrameId":parent}),
                Some("page"),
            );
            if phase != Phase::ChildContextWait || frame != "child" {
                event(
                    "Runtime.executionContextCreated",
                    json!({"executionContextId":format!("ctx-{frame}"),"auxData":{"frameId":frame,"name":""}}),
                    Some("page"),
                );
            }
        }
        let tracked = registry.claim("target", Duration::from_secs(2)).unwrap();
        let main_frame = MainFrame::from_tracked(tracked, connection, registry.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while main_frame.frame_ctx("child").is_none()
            || (phase != Phase::ChildContextWait
                && main_frame
                    .frame_ctx("child")
                    .and_then(|ctx| ctx.current())
                    .is_none())
        {
            assert!(
                Instant::now() < deadline,
                "mock child context was not installed"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let instance = Instance {
            instance_id: "mock".into(),
            browser,
            pid: child.id(),
            child,
            version: None,
            _profile_dir: tempfile::tempdir().unwrap(),
            pages: HashMap::from([(
                "p".into(),
                ManagedMainFrame {
                    main_frame,
                    headers: Vec::new(),
                },
            )]),
            closed_pages: HashSet::new(),
            closed_pages_order: VecDeque::new(),
            page_counter: 1,
            close_timeout: Duration::from_millis(100),
            registry,
        };
        Self {
            instance,
            closed,
            worker: Some(worker),
            stalled,
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.instance.browser.connection().close();
        self.worker.take().unwrap().join().unwrap();
    }
}

fn check(phase: Phase) {
    let harness = Harness::new(phase);
    let frames = ["#frame".into()];
    let budget = Duration::from_millis(300);
    let started = Instant::now();
    let result = match phase {
        Phase::Hop | Phase::Describe | Phase::Dispose => harness
            .instance
            .text("p", None, &frames, budget)
            .map(|_| ()),
        Phase::FillEval | Phase::FillDispatch => harness
            .instance
            .fill("p", "#input", "value", &frames, budget)
            .map(|_| ()),
        Phase::KeyDown | Phase::KeyUp => harness
            .instance
            .press("p", "Enter", &frames, budget)
            .map(|_| ()),
        Phase::ClickMove | Phase::ClickDown | Phase::ClickUp => harness
            .instance
            .click_selector("p", "#button", &frames, budget)
            .map(|_| ()),
        Phase::HoverMove => harness
            .instance
            .hover("p", "#button", &frames, budget)
            .map(|_| ()),
        _ => harness.instance.insert_text("p", "value", &frames, budget),
    };
    assert!(
        harness.stalled.load(Ordering::SeqCst),
        "{phase:?} was not exercised"
    );
    assert!(
        started.elapsed() < budget * 3,
        "{phase:?} exceeded 3x budget: {:?}: {result:?}",
        started.elapsed()
    );
    assert!(
        result.is_err(),
        "{phase:?}: stalled operation should time out"
    );
}

#[test]
fn budget_hop_is_observable() {
    check(Phase::Hop);
}
#[test]
fn budget_describe_is_observable() {
    check(Phase::Describe);
}
#[test]
fn budget_parent_focus_is_observable() {
    check(Phase::ParentFocus);
}
#[test]
fn budget_target_focus_is_observable() {
    check(Phase::TargetFocus);
}
#[test]
fn budget_fill_eval_is_observable() {
    check(Phase::FillEval);
}
#[test]
fn budget_type_dispatch_is_observable() {
    check(Phase::TypeDispatch);
}
#[test]
fn budget_keydown_dispatch_is_observable() {
    check(Phase::KeyDown);
}
#[test]
fn budget_keyup_dispatch_is_observable() {
    check(Phase::KeyUp);
}

#[test]
fn budget_fill_dispatch_is_observable() {
    check(Phase::FillDispatch);
}
#[test]
fn budget_dispose_is_observable() {
    check(Phase::Dispose);
}
#[test]
fn budget_click_move_is_observable() {
    check(Phase::ClickMove);
}
#[test]
fn budget_click_down_is_observable() {
    check(Phase::ClickDown);
}
#[test]
fn budget_click_up_is_observable() {
    check(Phase::ClickUp);
}
#[test]
fn budget_hover_dispatch_is_observable() {
    check(Phase::HoverMove);
}

#[test]
fn child_context_wait_returns_closed_promptly() {
    let harness = Harness::new(Phase::ChildContextWait);
    std::thread::scope(|scope| {
        let frame = &harness.instance.pages["p"].main_frame;
        let resolve =
            scope.spawn(|| frame.resolve_child_frame("main", "#frame", Duration::from_secs(3)));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !harness.stalled.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "context wait setup did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let started = Instant::now();
        harness.instance.browser.connection().close().unwrap();
        let error = resolve.join().unwrap().unwrap_err();
        assert_eq!(error.kind, ProtocolErrorKind::Closed, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    });
}

#[test]
fn press_errors_keep_plain_and_scoped_operation_prefixes() {
    for (phase, step) in [(Phase::KeyDown, "keydown"), (Phase::KeyUp, "keyup")] {
        let harness = Harness::new(phase);
        for frames in [vec![], vec!["#frame".into()]] {
            let error = harness
                .instance
                .press("p", "Enter", &frames, Duration::from_millis(300))
                .unwrap_err();
            let scope = if frames.is_empty() {
                ""
            } else {
                "--frame #frame: "
            };
            assert!(
                error.starts_with(&format!(
                    "{scope}press ({step}) failed: Request 'Page.dispatchKeyEvent' timed out"
                )),
                "{error}"
            );
            assert_eq!(
                error.matches("--frame #frame:").count(),
                usize::from(!frames.is_empty()),
                "{error}"
            );
        }
    }
}
