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
use std::time::Duration;

use camoufox::api::{Browser, BrowserOptions, ContextOptions};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

mod fixtures;

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

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    assert!(!main_frame.frame_id().is_empty());

    let nav_result = main_frame.navigate(
        "https://example.com",
        Default::default(),
        Duration::from_secs(30),
    );
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
    use std::time::Duration;

    let tb = setup();
    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    main_frame
        .navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(30),
        )
        .expect("navigate failed");

    let title = main_frame
        .evaluate("document.title", Duration::from_secs(15))
        .expect("evaluate failed");
    let title_str = title
        .pointer("/result/value")
        .or_else(|| title.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    assert!(
        title_str.to_lowercase().contains("example"),
        "title should contain 'example', got: {title_str:?}"
    );

    tb.teardown();
}

#[test]
#[ignore]
fn navigate_to_attachment_returns_navigation_became_download() {
    // REGRESSION TEST for the SCI sci-get-pdf wedge.
    //
    // When the renderer receives `Content-Disposition: attachment` it
    // diverts the response into a download flow without creating a
    // document, so `Page.navigate` never sends a response. Before the
    // download-detection patch this parked the caller forever; with the
    // patch the connection's reader thread catches `Browser.downloadCreated`
    // and resolves the pending navigate with
    // `ProtocolErrorKind::NavigationBecameDownload`.
    //
    // Run with:
    //   cargo test --test integration -- --ignored \
    //       navigate_to_attachment_returns_navigation_became_download \
    //       --test-threads=1
    use camoufox::protocol::errors::ProtocolErrorKind;
    use std::time::Instant;

    let server = fixtures::AttachmentServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    let start = Instant::now();
    let result = main_frame.navigate(&server.url, Default::default(), Duration::from_secs(30));
    let elapsed = start.elapsed();

    // Must surface as an error, not hang. We give a generous upper bound
    // (10s) — empirically the event arrives within a few hundred ms.
    assert!(
        elapsed < Duration::from_secs(10),
        "navigate should return promptly, took {elapsed:?}"
    );

    let err = result.expect_err("navigate to attachment URL must error");
    assert_eq!(
        err.kind,
        ProtocolErrorKind::NavigationBecameDownload,
        "expected NavigationBecameDownload, got {err:?}"
    );
    let info = err.download_info.as_ref().expect("download_info populated");
    assert!(info.url.contains("file.pdf"));

    tb.teardown();
}

#[test]
#[ignore]
fn cookies_command_returns_http_only_cookies() {
    // Regression test for G1: `Browser.getCookies` must return HttpOnly cookies
    // alongside ordinary cookies, with the httpOnly flag preserved.
    //
    // Setup:
    //  1. Start a CookieServer that sets one plain cookie + one HttpOnly cookie.
    //  2. Launch a browser, create a context, create a page, navigate to the
    //     cookie-setter URL.
    //  3. Call `BrowserContext::get_cookies()`.
    //  4. Assert both cookies are present, the HttpOnly one has httpOnly == true.
    //
    // Run with:
    //   cargo test --test integration -- --ignored \
    //       cookies_command_returns_http_only_cookies --test-threads=1
    use camoufox::api::context::Cookie;

    let server = fixtures::CookieServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    main_frame
        .navigate(&server.url, Default::default(), Duration::from_secs(30))
        .expect("navigate to cookie-setter failed");

    // Give the browser a moment to process Set-Cookie headers after navigation.
    // `Page.navigate` acks before the HTTP response is fully committed to the
    // cookie jar; a brief settle avoids a race on slow CI.
    std::thread::sleep(Duration::from_millis(500));

    let cookies: Vec<Cookie> = context.get_cookies().expect("get_cookies failed");

    // Categorise the cookies by name.
    let normal = cookies
        .iter()
        .find(|c| c.name == "normal_cookie")
        .expect("normal_cookie not found in jar");
    let http_only = cookies
        .iter()
        .find(|c| c.name == "http_only_cookie")
        .expect("http_only_cookie not found in jar — HttpOnly cookies must be returned");

    assert_eq!(normal.value, "hello", "normal_cookie value mismatch");
    assert!(
        !normal.http_only,
        "normal_cookie must have httpOnly == false"
    );

    assert_eq!(http_only.value, "secret", "http_only_cookie value mismatch");
    assert!(
        http_only.http_only,
        "http_only_cookie must have httpOnly == true"
    );

    tb.teardown();
}

#[test]
#[ignore]
fn navigate_main_frame_with_cross_origin_iframe() {
    // REGRESSION TEST for the cross-origin-iframe attach bug.
    //
    // When a page contains a fast cross-origin iframe (such as Amazon's
    // aax-eu.amazon-adsystem.com ad-pixel), the iframe's main-world
    // execution context arrives shortly after the top frame's, and the old
    // code's unfiltered Runtime.executionContextCreated handler would
    // overwrite the cached context with the iframe's. Subsequent
    // `evaluate` then ran in the iframe.
    //
    // What this test exercises:
    //   - Layer 3 fix (auxData.frameId filter on
    //     Runtime.executionContextCreated): YES, end-to-end.
    //   - Layer 1 fix (targetInfo.type == "page" on attachedToTarget):
    //     NOT end-to-end. At new_main_frame() time only the top page target
    //     exists; the iframe target appears later via navigation.
    //   - Layer 2 fix (parentFrameId.is_empty() on Page.frameAttached):
    //     NOT end-to-end. At new_main_frame() time only the top frame
    //     exists; iframe frames attach later.
    //
    // Layers 1 and 2 are exercised by inspection of the filter predicates
    // in src/api/context.rs.
    use std::time::Duration;

    let server = fixtures::FixtureServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    main_frame
        .navigate(
            &server.main_url,
            Default::default(),
            Duration::from_secs(30),
        )
        .expect("navigate failed");

    let body = main_frame
        .evaluate("document.body.innerText", Duration::from_secs(15))
        .expect("evaluate body failed");
    let location = main_frame
        .evaluate("location.href", Duration::from_secs(15))
        .expect("evaluate location.href failed");

    let body_str = body
        .pointer("/result/value")
        .or_else(|| body.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let loc_str = location
        .pointer("/result/value")
        .or_else(|| location.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    tb.teardown();

    assert!(
        body_str.contains("MAIN_SENTINEL_8f3a2b1c"),
        "evaluate should run in main frame; body was: {body_str:?}"
    );
    assert!(
        !body_str.contains("IFRAME_SENTINEL_4e9d7c0a"),
        "evaluate must NOT run in iframe; body was: {body_str:?}"
    );
    assert_eq!(
        loc_str, server.main_url,
        "location.href should be the main page, not the iframe"
    );
}

#[test]
#[ignore]
fn navigate_wait_until_load_blocks_until_dom_marker_present() {
    // G3 integration test: `navigate ... --wait-until load` must block until
    // the `load` event fires. The fixture page loads a /slow.js (delayed
    // 200 ms) which inserts `div#load-marker` into the DOM. We assert that
    // `div#load-marker` is non-null immediately after navigate returns,
    // proving the wait was genuine rather than just acking the navigate RPC.
    //
    // Run with:
    //   cargo test --test integration -- --ignored \
    //       navigate_wait_until_load_blocks_until_dom_marker_present \
    //       --test-threads=1

    use camoufox::api::main_frame::NavigateOptions;

    let server = fixtures::LifecycleServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    // Navigate with wait_until=load; this must block until slow.js is
    // delivered and the DOM marker is present.
    main_frame
        .navigate(
            &server.url,
            NavigateOptions {
                wait_until: Some("load".to_owned()),
                ..Default::default()
            },
            Duration::from_secs(30),
        )
        .expect("navigate with wait_until=load failed");

    // Immediately after navigate returns, the DOM marker must exist.
    // If navigate returned before load, slow.js would not yet have run
    // and this evaluate would return null.
    let result = main_frame
        .evaluate(
            "document.getElementById('load-marker') ? 'present' : 'absent'",
            Duration::from_secs(10),
        )
        .expect("evaluate failed");

    let marker = result
        .pointer("/result/value")
        .or_else(|| result.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("absent");

    assert_eq!(
        marker, "present",
        "div#load-marker must be present immediately after navigate (wait_until=load) returns. \
         Got: {marker:?}. If 'absent', navigate returned before the load event fired."
    );

    tb.teardown();
}

#[test]
#[ignore]
fn navigate_reports_main_document_status_code() {
    // G4 integration test: `navigate` must surface the main-document HTTP
    // status in `outcome.status_code` additively, WITHOUT failing on 4xx.
    //
    // Setup:
    //   1. Start a StatusServer with /200 (200 OK) and /404 (404 Not Found).
    //   2. Navigate to /404 → assert status_code == Some(404) AND navigate Ok.
    //   3. Navigate to /200 → assert status_code == Some(200).
    //
    // Run with:
    //   cargo test --test integration -- --ignored \
    //       navigate_reports_main_document_status_code --test-threads=1

    let server = fixtures::StatusServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    // --- 404 path ---
    let outcome_404 = main_frame
        .navigate(&server.url_404, Default::default(), Duration::from_secs(30))
        .expect("navigate to /404 must return Ok (not error on 4xx)");

    assert_eq!(
        outcome_404.status_code,
        Some(404),
        "status_code must be Some(404) for a 404 response; got {:?}",
        outcome_404.status_code
    );

    // --- 200 path ---
    let outcome_200 = main_frame
        .navigate(&server.url_200, Default::default(), Duration::from_secs(30))
        .expect("navigate to /200 must return Ok");

    assert_eq!(
        outcome_200.status_code,
        Some(200),
        "status_code must be Some(200) for a 200 response; got {:?}",
        outcome_200.status_code
    );

    tb.teardown();
}

#[test]
#[ignore]
fn navigate_reports_final_status_after_redirect() {
    // G4 redirect integration test: navigating to a URL that 302-redirects to
    // a 200 page must report the FINAL status (200), NOT the redirect hop
    // (302). This is a real-world cross-host redirect (`old.example → new.example`, 301 → 200).
    //
    // Run with:
    //   cargo test --test integration -- --ignored \
    //       navigate_reports_final_status_after_redirect --test-threads=1

    let server = fixtures::StatusServer::start();
    let tb = setup();

    let context = tb
        .browser
        .new_context(ContextOptions::default())
        .expect("failed to create context");
    let main_frame = context
        .new_main_frame()
        .expect("failed to create main frame");

    let outcome = main_frame
        .navigate(
            &server.url_redirect,
            Default::default(),
            Duration::from_secs(30),
        )
        .expect("navigate through redirect must return Ok");

    assert_eq!(
        outcome.status_code,
        Some(200),
        "status_code must be the FINAL hop (200), not the redirect (302); got {:?}",
        outcome.status_code
    );

    tb.teardown();
}

#[test]
#[ignore]
#[cfg(feature = "cli")]
fn click_command_dispatches_trusted_event_at_coordinates() {
    // Regression test for the trusted-click feature (the daemon `click` command).
    //
    // `click` dispatches Page.dispatchMouseEvent (a real mousemove → mousedown
    // → mouseup) instead of a synthetic JS .click() for one reason: the browser
    // reports the resulting event as `isTrusted == true`. Untrusted events
    // cannot drive protected widgets like Cloudflare Turnstile, so if this ever
    // regresses to an untrusted event the feature is silently gutted while
    // still appearing to "click".
    //
    // This exercises the exact daemon code path added by the click patch:
    //   InstanceManager::click → Instance::click → three
    //   MainFrame::dispatch_mouse_event calls (mousemove/mousedown/mouseup).
    //
    // The fixture page arms a capture-phase click listener that records the
    // last click into window.__click_result. We assert the listener fired with
    // isTrusted == true at the dispatched coordinates.
    //
    // Run with (requires the `cli` feature — the `click` command only exists
    // under it):
    //   cargo test --test integration --features cli -- --ignored \
    //       click_command_dispatches_trusted_event_at_coordinates \
    //       --test-threads=1
    use camoufox::cli::instance::InstanceManager;

    // Coordinates well inside the default headless viewport.
    const CLICK_X: i32 = 200;
    const CLICK_Y: i32 = 300;

    let server = fixtures::ClickServer::start();

    let mut manager = InstanceManager::new();
    let (instance_id, _version, _pid) = manager
        .launch(Some(true), Some(&camoufox_bin()))
        .expect("launch failed");
    let page_id = manager.new_page(&instance_id).expect("new_page failed");

    // Block until `load` so the inline listener is guaranteed armed.
    manager
        .navigate(
            &instance_id,
            &page_id,
            &server.url,
            Duration::from_secs(30),
            Some("load"),
        )
        .expect("navigate to click fixture failed");

    // Dispatch the trusted click via the exact daemon path under test.
    manager
        .click(&instance_id, &page_id, CLICK_X, CLICK_Y)
        .expect("click failed");

    // Read back the recorded event field by field, operating on the returned
    // serde_json::Value via its inherent accessors — matching this file's
    // evaluate-result convention (no serde_json import needed).
    let trusted = manager
        .evaluate(
            &instance_id,
            &page_id,
            "window.__click_result && window.__click_result.trusted",
            Duration::from_secs(10),
        )
        .expect("evaluate trusted failed");
    let client_x = manager
        .evaluate(
            &instance_id,
            &page_id,
            "window.__click_result ? window.__click_result.x : -1",
            Duration::from_secs(10),
        )
        .expect("evaluate clientX failed");
    let client_y = manager
        .evaluate(
            &instance_id,
            &page_id,
            "window.__click_result ? window.__click_result.y : -1",
            Duration::from_secs(10),
        )
        .expect("evaluate clientY failed");

    // Stop the browser before asserting so a failed assertion can't leak it.
    let _ = manager.stop(&instance_id);

    // The load-bearing assertion: the event must be TRUSTED. `None` means the
    // listener never fired (no click registered); `Some(false)` means the event
    // was synthetic/untrusted and would be rejected by bot-screen widgets.
    assert_eq!(
        trusted.as_bool(),
        Some(true),
        "click must dispatch a TRUSTED event (isTrusted == true); got {trusted:?}. \
         null = click never registered; false = untrusted (cannot drive Turnstile)."
    );
    assert_eq!(
        client_x.as_i64(),
        Some(CLICK_X as i64),
        "event clientX must equal the dispatched x ({CLICK_X}); got {client_x:?}"
    );
    assert_eq!(
        client_y.as_i64(),
        Some(CLICK_Y as i64),
        "event clientY must equal the dispatched y ({CLICK_Y}); got {client_y:?}"
    );
}

#[test]
#[ignore]
#[cfg(feature = "cli")]
fn reading_commands_extract_text_html_links_and_metadata() {
    // Covers the Tier-1 reading commands added to the daemon:
    //   InstanceManager::{text, html, links, data, url}
    //     → Instance::eval_checked → MainFrame::evaluate
    //
    // These exist so an agent never has to hand-write extraction JS. The
    // load-bearing properties are: `text` returns rendered text (not markup),
    // `html` returns the element's outerHTML, `links` resolves hrefs to
    // absolute URLs, and `data` splits metadata into og/jsonld/meta groups.
    //
    // Run with:
    //   cargo test --test integration --features cli -- --ignored \
    //       reading_commands_extract_text_html_links_and_metadata \
    //       --test-threads=1
    use camoufox::cli::instance::InstanceManager;

    let server = fixtures::BrowseServer::start();

    let mut manager = InstanceManager::new();
    let (instance_id, _version, _pid) = manager
        .launch(Some(true), Some(&camoufox_bin()))
        .expect("launch failed");
    let page_id = manager.new_page(&instance_id).expect("new_page failed");
    manager
        .navigate(
            &instance_id,
            &page_id,
            &server.url,
            Duration::from_secs(30),
            Some("load"),
        )
        .expect("navigate to browse fixture failed");

    let timeout = Duration::from_secs(10);
    let full_text = manager.text(&instance_id, &page_id, None, timeout);
    let scoped_text = manager.text(&instance_id, &page_id, Some("#body-copy"), timeout);
    let missing_text = manager.text(&instance_id, &page_id, Some("#nope"), timeout);
    let scoped_html = manager.html(&instance_id, &page_id, Some("#heading"), timeout);
    let links = manager.links(&instance_id, &page_id, None, timeout);
    let og = manager.data(&instance_id, &page_id, true, false, false, timeout);
    let all = manager.data(&instance_id, &page_id, false, false, false, timeout);
    let url = manager.url(&instance_id, &page_id, timeout);

    // Stop the browser before asserting so a failed assertion can't leak it.
    let _ = manager.stop(&instance_id);

    let full_text = full_text.expect("text failed");
    assert!(
        full_text.contains("Browse Fixture") && full_text.contains("Readable body copy."),
        "text must return rendered page text; got {full_text:?}"
    );
    assert!(
        !full_text.contains('<'),
        "text must not contain markup; got {full_text:?}"
    );

    assert_eq!(
        scoped_text.expect("scoped text failed"),
        "Readable body copy.",
        "--selector scopes extraction to that element"
    );
    let err = missing_text.expect_err("a missing selector must be an error, not empty output");
    assert!(err.contains("selector not found"), "got: {err}");

    assert_eq!(
        scoped_html.expect("scoped html failed"),
        "<h1 id=\"heading\">Browse Fixture</h1>",
        "html --selector returns outerHTML"
    );

    let links = links.expect("links failed");
    let links = links.as_array().expect("links is an array");
    assert_eq!(links.len(), 2, "both anchors collected: {links:?}");
    assert_eq!(links[0]["text"], "One");
    assert_eq!(
        links[0]["href"], "https://example.invalid/one",
        "hrefs are resolved to absolute URLs"
    );

    let og = og.expect("data --og failed");
    assert_eq!(og["og"]["og:title"], "Fixture OG Title");
    assert_eq!(og["og"]["og:image"], "https://example.invalid/og.png");
    assert!(
        og.get("jsonld").is_none() && og.get("meta").is_none(),
        "--og returns only the og group; got {og:?}"
    );

    let all = all.expect("data (no flags) failed");
    assert_eq!(
        all["jsonld"][0]["@type"], "WebPage",
        "JSON-LD blocks are parsed, not returned as raw strings"
    );
    assert_eq!(all["meta"]["description"], "fixture description");
    assert!(all.get("og").is_some(), "no flags means all three groups");

    let (page_url, title) = url.expect("url failed");
    assert_eq!(page_url, server.url);
    assert_eq!(title, "Browse Fixture");
}

#[test]
#[ignore]
#[cfg(feature = "cli")]
fn interaction_commands_fill_press_and_click_by_selector() {
    // Covers the Tier-4 interaction commands:
    //   InstanceManager::{fill, press, click_selector, select_option, hover}
    //
    // REGRESSION GUARD for the `press` keydown path: Juggler routes a keydown
    // through `commitCompositionWith` whenever `text` is present and differs
    // from `key`, so sending Enter with text "\r" arrives at the page as
    // `key === "Process"` and never submits a form. `key_descriptor` therefore
    // omits `text` entirely. If that regresses, the submit assertion below
    // fails while every other key still appears to "work".
    //
    // Run with:
    //   cargo test --test integration --features cli -- --ignored \
    //       interaction_commands_fill_press_and_click_by_selector \
    //       --test-threads=1
    use camoufox::cli::instance::InstanceManager;

    let server = fixtures::BrowseServer::start();

    let mut manager = InstanceManager::new();
    let (instance_id, _version, _pid) = manager
        .launch(Some(true), Some(&camoufox_bin()))
        .expect("launch failed");
    let page_id = manager.new_page(&instance_id).expect("new_page failed");
    manager
        .navigate(
            &instance_id,
            &page_id,
            &server.url,
            Duration::from_secs(30),
            Some("load"),
        )
        .expect("navigate to browse fixture failed");

    let timeout = Duration::from_secs(10);

    let filled = manager.fill(&instance_id, &page_id, "#q", "hello world", timeout);
    let input_value = manager.evaluate(
        &instance_id,
        &page_id,
        "document.getElementById('q').value",
        timeout,
    );
    // Enter must submit the form — the composition-commit regression guard.
    let pressed = manager.press(&instance_id, &page_id, "Enter");
    let clicked = manager.click_selector(&instance_id, &page_id, "#go", timeout);
    let selected = manager.select_option(&instance_id, &page_id, "#sel", "Beta", timeout);
    let hovered = manager.hover(&instance_id, &page_id, "#heading", timeout);
    let missing = manager.click_selector(&instance_id, &page_id, "#nope", timeout);
    let log = manager.text(&instance_id, &page_id, Some("#log"), timeout);

    let _ = manager.stop(&instance_id);

    assert_eq!(
        filled.expect("fill failed"),
        "input",
        "fill reports the tag"
    );
    assert_eq!(
        input_value.expect("evaluate failed").as_str(),
        Some("hello world"),
        "fill types the value into the element"
    );
    pressed.expect("press Enter failed");
    let (x, y) = clicked.expect("click by selector failed");
    assert!(x > 0 && y > 0, "click resolved a real box, got ({x}, {y})");
    let selected = selected.expect("select failed");
    assert_eq!(selected["value"], "b", "select matches by visible text too");
    hovered.expect("hover failed");
    let err = missing.expect_err("clicking a missing selector must error");
    assert!(err.contains("selector not found"), "got: {err}");

    let log = log.expect("reading the log failed");
    assert!(
        log.contains("submit:hello world"),
        "press Enter must submit the form — if this line is missing, the keydown \
         was delivered as an IME composition commit (key === \"Process\"). Log was: {log:?}"
    );
    assert!(
        log.contains("click:go"),
        "click by selector must fire the button's handler. Log was: {log:?}"
    );
    assert!(
        log.contains("change:b"),
        "select must dispatch a change event. Log was: {log:?}"
    );
}

#[test]
#[ignore]
#[cfg(feature = "cli")]
fn screenshot_default_clip_uses_scroll_offset() {
    // REGRESSION GUARD for the screenshot default-clip behavior change.
    //
    // `screenshot` with no --selector/--clip previously always clipped to
    // (0, 0, innerWidth, innerHeight) in *document* coordinates, so a scrolled
    // page was captured from the top of the document rather than what was on
    // screen. The default clip origin is now (window.scrollX, window.scrollY).
    //
    // This drives the exact path: InstanceManager::screenshot →
    // Instance::screenshot → the (None, None) viewport branch. We make the page
    // tall, scroll to a known offset, take a default screenshot, and assert the
    // RESOLVED clip rect's origin equals the scroll offset (not 0). No pixel
    // inspection needed — the returned Rect is the load-bearing evidence.
    //
    // Run with:
    //   cargo test --test integration --features cli -- --ignored \
    //       screenshot_default_clip_uses_scroll_offset --test-threads=1
    use camoufox::cli::instance::InstanceManager;

    let server = fixtures::BrowseServer::start();

    let mut manager = InstanceManager::new();
    let (instance_id, _version, _pid) = manager
        .launch(Some(true), Some(&camoufox_bin()))
        .expect("launch failed");
    let page_id = manager.new_page(&instance_id).expect("new_page failed");
    manager
        .navigate(
            &instance_id,
            &page_id,
            &server.url,
            Duration::from_secs(30),
            Some("load"),
        )
        .expect("navigate to browse fixture failed");

    let timeout = Duration::from_secs(10);

    // Make the document scrollable and scroll to a known offset.
    let scroll_y = manager
        .evaluate(
            &instance_id,
            &page_id,
            "document.body.style.height = '5000px'; window.scrollTo(0, 600); window.scrollY",
            timeout,
        )
        .expect("scroll setup failed");

    let mut out_path = std::env::temp_dir();
    out_path.push("camoufox-scroll-clip-test.png");
    let shot = manager.screenshot(
        &instance_id,
        &page_id,
        Some("png"),
        None,
        Some(out_path.to_str().unwrap()),
        None, // no selector
        None, // no explicit clip → viewport default
        timeout,
    );

    let _ = manager.stop(&instance_id);
    let _ = std::fs::remove_file(&out_path);

    assert_eq!(
        scroll_y.as_f64(),
        Some(600.0),
        "precondition: page scrolled to y=600; got {scroll_y:?}"
    );
    let (_bytes, _path, rect) = shot.expect("screenshot failed");
    assert!(
        (rect.y - 600.0).abs() < 1.0,
        "default screenshot clip origin must follow the scroll offset (y≈600), \
         NOT the document top (y=0). Got clip.y={}. A y of 0 means the pre-fix \
         behavior regressed.",
        rect.y
    );
}

#[test]
#[ignore]
#[cfg(feature = "cli")]
fn wait_fails_fast_on_invalid_selector() {
    // REGRESSION GUARD for wait_for_selector's permanent-vs-transient error
    // handling. An invalid CSS selector makes document.querySelector throw a
    // SyntaxError (a page-script error) — permanent. wait must fail fast with
    // the real error instead of retrying until the whole --timeout elapses
    // (which also holds the global daemon lock the entire time).
    //
    // Run with:
    //   cargo test --test integration --features cli -- --ignored \
    //       wait_fails_fast_on_invalid_selector --test-threads=1
    use camoufox::cli::instance::InstanceManager;

    let server = fixtures::BrowseServer::start();

    let mut manager = InstanceManager::new();
    let (instance_id, _version, _pid) = manager
        .launch(Some(true), Some(&camoufox_bin()))
        .expect("launch failed");
    let page_id = manager.new_page(&instance_id).expect("new_page failed");
    manager
        .navigate(
            &instance_id,
            &page_id,
            &server.url,
            Duration::from_secs(30),
            Some("load"),
        )
        .expect("navigate to browse fixture failed");

    // A generous timeout: if wait erroneously retries a permanent error, it
    // burns all 30s. Fail-fast must return in well under a second.
    let started = std::time::Instant::now();
    let result = manager.wait_for_selector(
        &instance_id,
        &page_id,
        ":::not-a-selector",
        Duration::from_secs(30),
    );
    let elapsed = started.elapsed();

    let _ = manager.stop(&instance_id);

    let err = result.expect_err("an invalid selector must be an error, not a match");
    assert!(
        err.contains("invalid selector"),
        "error must name the bad selector, not report a generic timeout; got: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "wait must fail fast on a permanent (invalid-selector) error, not burn the \
         full 30s timeout. Took {elapsed:?}."
    );
}
