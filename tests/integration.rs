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
