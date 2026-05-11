//! Integration tests against a real Camoufox browser.
//!
//! These tests are `#[ignore]`d by default because they require a Camoufox
//! binary at `/root/.cache/camoufox/camoufox`. Run with:
//!
//! ```sh
//! cargo test --test integration -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::time::Duration;

use camoufox::api::{Browser, BrowserContext, BrowserOptions, ContextOptions, Page};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

fn camoufox_bin() -> String {
    std::env::var("CAMOUFOX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        format!("{home}/.cache/camoufox/camoufox")
    })
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestBrowser {
    browser: Browser,
    child: Child,
    _profile_dir: tempfile::TempDir,
}

impl TestBrowser {
    /// Shut down the browser and wait for the child process to exit.
    /// Kills the process if it doesn't exit within 5 seconds.
    fn teardown(self) {
        let TestBrowser {
            browser,
            mut child,
            _profile_dir,
        } = self;
        let _ = browser.close();

        // Give the process a few seconds to exit gracefully.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return,
            }
        }
    }
}

fn setup() -> TestBrowser {
    let _ = env_logger::try_init();

    let profile_dir = tempfile::tempdir().expect("failed to create temp profile dir");

    let config = LaunchConfig {
        executable: PathBuf::from(camoufox_bin()),
        profile_dir: Some(profile_dir.path().to_owned()),
        headless: true,
        ..Default::default()
    };

    let mut launched = process::unix::spawn(&config).expect("failed to spawn camoufox");
    let _ = process::readiness::wait_for_ready(&mut launched.child, config.timeout)
        .expect("camoufox did not become ready");

    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let session = conn.root_session();
    let browser =
        Browser::connect(conn, session, BrowserOptions::default()).expect("bootstrap failed");

    TestBrowser {
        browser,
        child: launched.child,
        _profile_dir: profile_dir,
    }
}

/// Full page setup: create context, create page, wire session, wait for frame.
///
/// Returns `(context, page, session_id)` with the page fully wired (session
/// attached, main frame ID set from `Page.frameAttached` event).
fn setup_page(browser: &Browser) -> (BrowserContext, Page, String) {
    let conn = browser.connection();

    // 1. Register handlers BEFORE creating the page to avoid missing events.

    // Listen for attachedToTarget on the root session.
    let (attach_tx, attach_rx) = mpsc::channel();
    conn.on_event(
        "",
        "Browser.attachedToTarget",
        Box::new(move |event| {
            let session_id = event
                .params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let target_id = event
                .params
                .get("targetInfo")
                .and_then(|v| v.get("targetId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let _ = attach_tx.send((session_id, target_id));
        }),
    );

    // Listen globally for Page.frameAttached to get the real frameId.
    // The frameId format in Camoufox is "mainframe-<browserId>", NOT the targetId.
    let (frame_tx, frame_rx) = mpsc::channel::<(String, String)>();
    conn.on_event_global(Box::new(move |event| {
        if event.method == "Page.frameAttached" {
            let frame_id = event
                .params
                .get("frameId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let sid = event
                .session_id
                .as_deref()
                .unwrap_or("")
                .to_owned();
            let _ = frame_tx.send((sid, frame_id));
        }
    }));

    // 2. Create context and page.
    let context = browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");

    let mut page = context.new_page().expect("failed to create page");

    // 3. Wait for attachedToTarget.
    let (session_id, target_id) = attach_rx
        .recv_timeout(EVENT_TIMEOUT)
        .expect("timeout waiting for Browser.attachedToTarget");

    assert!(!session_id.is_empty(), "session_id should not be empty");
    assert!(!target_id.is_empty(), "target_id should not be empty");
    assert_eq!(
        page.target_id(),
        target_id,
        "target_id should match the page"
    );

    // 4. Create the page session and wire it to the page.
    let page_session = conn.create_session(session_id.clone());
    page.set_session(page_session);

    // 5. Wait for Page.frameAttached to get the real main frame ID.
    let main_frame_id = {
        let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
        let mut found = None;
        while std::time::Instant::now() < deadline {
            match frame_rx.recv_timeout(Duration::from_secs(2)) {
                Ok((sid, fid)) if sid == session_id && !fid.is_empty() => {
                    found = Some(fid);
                    break;
                }
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        found.expect("did not receive Page.frameAttached for our page session")
    };

    page.set_main_frame_id(main_frame_id);

    (context, page, session_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn spawn_and_bootstrap() {
    let tb = setup();

    // Verify browser info was populated during bootstrap.
    let version = tb.browser.version().expect("version should be Some");
    assert!(
        version.contains("Firefox"),
        "version should contain 'Firefox', got: {version}"
    );

    let ua = tb.browser.user_agent().expect("user_agent should be Some");
    assert!(!ua.is_empty(), "user_agent should not be empty");

    tb.teardown();
}

#[test]
#[ignore]
fn create_context_and_page() {
    let tb = setup();

    let (_context, page, session_id) = setup_page(&tb.browser);

    // The page should have a session wired and be usable.
    assert!(!session_id.is_empty());
    assert!(page.main_frame_id().is_some());

    // Navigate to verify the session works (use a data URI to avoid network).
    let nav_result = page.navigate("https://example.com", Default::default());
    assert!(
        nav_result.is_ok(),
        "navigate failed: {:?}",
        nav_result.err()
    );

    tb.teardown();
}

#[test]
#[ignore]
fn navigate_and_evaluate() {
    let tb = setup();
    let conn = tb.browser.connection().clone();

    // Track execution context lifecycle globally. During page init, contexts
    // are created and destroyed (transient about:blank navigations). We need
    // the LAST surviving context for our page session.
    let (ctx_tx, ctx_rx) = mpsc::channel::<(String, String, bool)>(); // (sid, ctx_id, created)
    let ctx_tx2 = ctx_tx.clone();
    conn.on_event_global(Box::new(move |event| {
        if event.method == "Runtime.executionContextCreated" {
            let ctx_id = event
                .params
                .get("executionContextId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let sid = event
                .session_id
                .as_deref()
                .unwrap_or("")
                .to_owned();
            let _ = ctx_tx.send((sid, ctx_id, true));
        } else if event.method == "Runtime.executionContextDestroyed" {
            let ctx_id = event
                .params
                .get("executionContextId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let sid = event
                .session_id
                .as_deref()
                .unwrap_or("")
                .to_owned();
            let _ = ctx_tx2.send((sid, ctx_id, false));
        }
    }));

    // Also wait for Page.ready to know when transient navigations are done.
    let (ready_tx, ready_rx) = mpsc::channel::<String>();
    conn.on_event_global(Box::new(move |event| {
        if event.method == "Page.ready" {
            let sid = event
                .session_id
                .as_deref()
                .unwrap_or("")
                .to_owned();
            let _ = ready_tx.send(sid);
        }
    }));

    let (_context, page, session_id) = setup_page(&tb.browser);

    // Wait for Page.ready on our session (signals transient navs are done).
    {
        let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
        loop {
            match ready_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(sid) if sid == session_id => break,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        panic!("timeout waiting for Page.ready");
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("channel disconnected waiting for Page.ready")
                }
            }
        }
    }

    // Now collect execution context events. Track creates/destroys to find
    // the surviving context.
    let exec_ctx_id = {
        use std::collections::HashSet;
        let mut alive = HashSet::new();

        // Drain all already-received events.
        loop {
            match ctx_rx.try_recv() {
                Ok((sid, ctx_id, created)) if sid == session_id && !ctx_id.is_empty() => {
                    if created {
                        alive.insert(ctx_id);
                    } else {
                        alive.remove(&ctx_id);
                    }
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        // If we found a surviving context, use it; otherwise wait briefly.
        if alive.is_empty() {
            let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
            while std::time::Instant::now() < deadline {
                match ctx_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok((sid, ctx_id, created))
                        if sid == session_id && !ctx_id.is_empty() && created =>
                    {
                        alive.insert(ctx_id);
                        break;
                    }
                    Ok((sid, ctx_id, false)) if sid == session_id => {
                        alive.remove(&ctx_id);
                    }
                    Ok(_) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }

        alive
            .into_iter()
            .next()
            .expect("no surviving execution context for our page session")
    };

    // Evaluate a simple expression.
    let result = page
        .evaluate("1 + 1", &exec_ctx_id)
        .expect("evaluate failed");

    // The result shape is {"result": {"value": 2}}. Extract the inner value.
    let value = result
        .get("result")
        .and_then(|r| r.get("value"))
        .or_else(|| result.get("value"))
        .cloned()
        .unwrap_or(result.clone());
    assert_eq!(
        value,
        serde_json::json!(2),
        "expected 1+1=2, got: {result}"
    );

    tb.teardown();
}

#[test]
#[ignore]
fn probe_page_get_frame_tree_available() {
    // PROBE: confirms Page.getFrameTree exists in this Camoufox build before
    // the design depends on it. Remove this test in the cleanup task at the
    // end of the plan.
    let tb = setup();
    let (_context, page, _session_id) = setup_page(&tb.browser);

    // Use the page's session to call Page.getFrameTree directly via the
    // raw protocol. We don't have a wrapper for it yet — that's the point.
    let conn = tb.browser.connection();
    let page_session = conn.create_session(_session_id);
    let result = page_session
        .send("Page.getFrameTree", serde_json::json!({}))
        .expect("Page.getFrameTree should succeed");

    let frame_id = result
        .pointer("/frameTree/frame/frameId")
        .or_else(|| result.pointer("/frameTree/frame/id"))
        .and_then(|v| v.as_str())
        .expect("frameTree.frame should have a frameId");

    assert!(!frame_id.is_empty(), "main frame id should be non-empty");

    // The frame id from getFrameTree must match the one we got from
    // Page.frameAttached during setup — that's the whole point of the fix.
    assert_eq!(
        Some(frame_id),
        page.main_frame_id(),
        "getFrameTree main frame id should equal the frameAttached main frame id"
    );

    tb.teardown();
}
