# Camoufox top-frame attach fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop camoufox-rs from operating against cross-origin ad-pixel iframes (e.g. `aax-eu.amazon-adsystem.com` on amazon.in) by replacing three event-race accept-the-first-event spots in `src/cli/instance.rs` with authoritative protocol responses, and consolidating page-scoped state into a new `MainFrame` type that replaces the public `Page`.

**Architecture:** A new public `MainFrame` (in `src/api/main_frame.rs`) holds `{ session, target_id, frame_id, execution_context_id }` — all required, all populated from synchronous protocol responses. `BrowserContext::new_main_frame()` performs the full setup: send `Browser.newPage` → wait for `Browser.attachedToTarget` filtered on `targetInfo.type == "page"` (Layer 1 fix) → call `Page.getFrameTree` to get the authoritative main frame id (Layer 2 fix) → subscribe `Runtime.executionContextCreated` filtered on `auxData.frameId == frame_id` (Layer 3 fix). `Instance::create_page` shrinks from ~250 lines of event-handling to a single call. CLI wire format is unchanged.

**Tech Stack:** Rust 1.70+, sync code (no async runtime), `serde_json`, `mpsc::channel` for event passing, `tiny_http` (new dev-dep) for the test fixture server.

**Spec:** `docs/superpowers/specs/2026-05-11-camoufox-top-frame-attach-design.md`

---

## File map

| Action | Path | Responsibility |
|---|---|---|
| new | `src/api/main_frame.rs` | `MainFrame` type and all `Page.*` / `Runtime.*` / `Network.*` / `Heap.*` methods. Replaces today's `Page`. |
| delete | `src/api/page.rs` | Replaced wholesale by `main_frame.rs` (the bulk of method bodies is moved, not rewritten). |
| edit | `src/api/mod.rs` | Re-export `MainFrame` instead of `Page`. Keep the option/event-param types re-exported. |
| edit | `src/api/context.rs` | Replace `new_page() -> Result<Page>` with `new_main_frame() -> Result<MainFrame>`; the new method does the full setup sequence with all three filter fixes. |
| edit | `src/cli/instance.rs` | `ManagedPage` → `ManagedMainFrame` (smaller — only holds `MainFrame` + label). `Instance::create_page` shrinks dramatically. `Instance::evaluate/navigate/screenshot` delegate to `MainFrame` methods. |
| edit | `tests/integration.rs` | Update existing tests to use the new `MainFrame` API; remove the buggy `setup_page()` helper. |
| edit | `examples/web_browse.rs` | Update to use `MainFrame` API. |
| edit | `Cargo.toml` | Add `tiny_http` as dev-dependency. |
| new | `tests/fixtures/main.html` | Top page with `MAIN_SENTINEL_8f3a2b1c` and an iframe to port B. |
| new | `tests/fixtures/iframe.html` | Cross-origin iframe content with `IFRAME_SENTINEL_4e9d7c0a`. |
| new | `tests/fixtures/mod.rs` | `tiny_http`-based two-port fixture server (`FixtureServer` with Drop-based shutdown). |

---

## Task 1: Verify `Page.getFrameTree` is available in this Camoufox build

**Goal:** Don't delete the Layer-2 event-listening code (which we know works) until we've confirmed the replacement (`Page.getFrameTree`) works in this build. Read-only probe; no source code is modified in this task.

**Files:**
- Read: `docs/PROTOCOL.md` (search for `getFrameTree`)
- Run: a one-shot Rust integration test

- [ ] **Step 1: Check PROTOCOL.md for `getFrameTree`**

Run:
```bash
grep -n -i "getFrameTree\|frameTree" docs/PROTOCOL.md docs/UNDERSTANDING.md
```

Expected: at least one hit showing `Page.getFrameTree` is documented. Note the file:line for reference in later tasks.

- [ ] **Step 2: Write a probe test that calls `Page.getFrameTree` against a freshly attached page session**

Add this at the end of `tests/integration.rs`:

```rust
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
```

- [ ] **Step 3: Run the probe test**

Run:
```bash
cargo test --test integration probe_page_get_frame_tree_available -- --ignored --test-threads=1
```

Expected: PASS. If FAIL, two outcomes:
- If the error mentions "method not found" / "unknown method" — `getFrameTree` is unavailable. STOP and switch to the hybrid fallback documented in the spec (Layer 2 keeps `Page.frameAttached` listening but filters on `parentFrameId.is_none()`). Adjust Task 4 accordingly and continue.
- If the assertion `getFrameTree main frame id should equal …` fails — the response shape differs from the spec assumption. Inspect the actual JSON, adjust the JSON path in Task 4's code, and re-run.

- [ ] **Step 4: Commit (probe only — design unchanged)**

```bash
git add tests/integration.rs
git commit --no-gpg-sign -m "test: probe Page.getFrameTree availability (temporary)"
```

---

## Task 2: Add HTTP-fixture infrastructure (dev-dep + server module + HTML files)

**Goal:** Stand up a two-port local HTTP test server so the regression test in Task 3 has a deterministic cross-origin-iframe site to navigate against. No source code under `src/` changes.

**Files:**
- Modify: `Cargo.toml` (add `tiny_http` to `[dev-dependencies]`)
- Create: `tests/fixtures/main.html`
- Create: `tests/fixtures/iframe.html`
- Create: `tests/fixtures/mod.rs`

- [ ] **Step 1: Add `tiny_http` to dev-dependencies**

Edit `Cargo.toml`. Locate the `[dev-dependencies]` block (lines 20-23):

```toml
[dev-dependencies]
env_logger = "0.11"
libc = "0.2.182"
tempfile = "3"
```

Replace with:

```toml
[dev-dependencies]
env_logger = "0.11"
libc = "0.2.182"
tempfile = "3"
tiny_http = "0.12"
```

- [ ] **Step 2: Create `tests/fixtures/main.html`**

Write file `tests/fixtures/main.html` (the `__IFRAME_URL__` placeholder is substituted at serve time by `tests/fixtures/mod.rs` — do not interpret it as a template here):

```html
<!doctype html>
<html>
<head><title>Main</title></head>
<body data-testid="main">
MAIN_SENTINEL_8f3a2b1c
<iframe src="__IFRAME_URL__" width="200" height="200"></iframe>
</body>
</html>
```

- [ ] **Step 3: Create `tests/fixtures/iframe.html`**

Write file `tests/fixtures/iframe.html`:

```html
<!doctype html>
<html>
<head><title>Iframe</title></head>
<body>
IFRAME_SENTINEL_4e9d7c0a
</body>
</html>
```

- [ ] **Step 4: Create `tests/fixtures/mod.rs` (two-port HTTP server)**

Write file `tests/fixtures/mod.rs`. The server binds two ephemeral ports on `127.0.0.1` (different ports = different origin from the browser's perspective = OOPIF path), serves the iframe page immediately, and serves the main page after a 50 ms delay so the iframe target reliably wins the attach race we're testing against:

```rust
//! Local two-port HTTP fixture server for integration tests.
//!
//! Bind a `FixtureServer` on `127.0.0.1` to serve `main.html` and
//! `iframe.html` on two different ports. Different ports on the same host
//! count as different origins, which forces Camoufox onto the OOPIF code
//! path — the same path Amazon's ad-pixel iframe uses in production.
//!
//! The main page is served with a 50 ms delay so the iframe target wins the
//! `Browser.attachedToTarget` race; this is the exact race condition the
//! Layer-1 fix needs to handle.

use std::io::Cursor;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tiny_http::{Header, Response, Server};

const MAIN_HTML_TEMPLATE: &str = include_str!("main.html");
const IFRAME_HTML: &str = include_str!("iframe.html");
const MAIN_DELAY: Duration = Duration::from_millis(50);

pub struct FixtureServer {
    /// `http://127.0.0.1:<main_port>/`
    pub main_url: String,
    /// `http://127.0.0.1:<iframe_port>/`
    pub iframe_url: String,
    _main_server: Arc<Server>,
    _iframe_server: Arc<Server>,
}

impl FixtureServer {
    /// Start both servers on ephemeral ports.
    pub fn start() -> Self {
        let main_server =
            Arc::new(Server::http("127.0.0.1:0").expect("bind main server"));
        let iframe_server =
            Arc::new(Server::http("127.0.0.1:0").expect("bind iframe server"));

        let main_port = main_server.server_addr().to_ip().unwrap().port();
        let iframe_port = iframe_server.server_addr().to_ip().unwrap().port();

        let main_url = format!("http://127.0.0.1:{main_port}/");
        let iframe_url = format!("http://127.0.0.1:{iframe_port}/");

        // Main page: 50 ms delay, then HTML with the iframe URL substituted.
        let main_html =
            MAIN_HTML_TEMPLATE.replace("__IFRAME_URL__", &iframe_url);
        let main_server_clone = Arc::clone(&main_server);
        thread::spawn(move || {
            for req in main_server_clone.incoming_requests() {
                thread::sleep(MAIN_DELAY);
                let body = main_html.clone();
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(body.into_bytes()),
                    None,
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        // Iframe page: served immediately, no delay.
        let iframe_server_clone = Arc::clone(&iframe_server);
        thread::spawn(move || {
            for req in iframe_server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(IFRAME_HTML.as_bytes().to_vec()),
                    None,
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        FixtureServer {
            main_url,
            iframe_url,
            _main_server: main_server,
            _iframe_server: iframe_server,
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        // tiny_http's Server stops accepting new connections when the Arc
        // is dropped; the worker threads exit on their next iteration.
        // No explicit shutdown call needed.
    }
}
```

- [ ] **Step 5: Verify it compiles**

Run:
```bash
cargo build --tests
```

Expected: PASS — no compile errors. (No tests use `FixtureServer` yet; `tests/fixtures/mod.rs` won't actually be linked until a test references it, which happens in Task 3.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml tests/fixtures/
git commit --no-gpg-sign -m "test: add two-port HTTP fixture server for OOPIF integration tests"
```

---

## Task 3: Write the failing regression test (RED)

**Goal:** Capture the bug with a test that fails against today's code. This proves we are testing the right thing before we fix it. The test uses today's `Page` API; in Task 8 it'll be migrated to `MainFrame` once that exists.

**Files:**
- Modify: `tests/integration.rs` (add new test + a `mod fixtures;` declaration)

- [ ] **Step 1: Add `mod fixtures;` at the top of `tests/integration.rs`**

Open `tests/integration.rs`. After the existing module-level docstring (around line 8, after the ``` closing the docstring), insert:

```rust
mod fixtures;
```

- [ ] **Step 2: Add the regression test at the bottom of `tests/integration.rs`**

Append:

```rust
#[test]
#[ignore]
fn navigate_main_frame_with_cross_origin_iframe() {
    // REGRESSION TEST for the cross-origin-iframe attach bug.
    //
    // When a page contains a fast cross-origin iframe (such as Amazon's
    // aax-eu.amazon-adsystem.com ad-pixel), the iframe target races ahead
    // of the main page target in `Browser.attachedToTarget` events. Today,
    // `setup_page` accepts the first event and the resulting Page ends up
    // operating against the iframe, not the top frame.
    //
    // This test should FAIL on current code and PASS after the Layer 1/2/3
    // fixes land. It uses Page (today's API); Task 8 of the plan migrates
    // it to MainFrame.
    use std::sync::mpsc;
    use std::time::Duration;

    let server = fixtures::FixtureServer::start();
    let tb = setup();
    let (_context, mut page, session_id) = setup_page(&tb.browser);

    // Set up execution-context tracking the same way navigate_and_evaluate
    // does — see lines ~240-300 of this file. Duplicating it here is fine
    // because Task 8 deletes both blocks.
    let conn = tb.browser.connection().clone();
    let exec_ctx = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let exec_ctx_clone = std::sync::Arc::clone(&exec_ctx);
    let sid_clone = session_id.clone();
    conn.on_event_global(Box::new(move |event| {
        if event.session_id.as_deref().unwrap_or("") != sid_clone {
            return;
        }
        if event.method == "Runtime.executionContextCreated" {
            let is_main_world = event
                .params
                .get("auxData")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str())
                .map(|n| n.is_empty())
                .unwrap_or(true);
            if is_main_world {
                if let Some(ctx_id) = event
                    .params
                    .get("executionContextId")
                    .and_then(|v| v.as_str())
                {
                    *exec_ctx_clone.lock().unwrap() = Some(ctx_id.to_owned());
                }
            }
        }
    }));

    // Navigate to the main page (which embeds the cross-origin iframe).
    page.navigate(&server.main_url, Default::default())
        .expect("navigate failed");

    // Poll for an execution context (up to 10 s).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let ctx_id = loop {
        if let Some(c) = exec_ctx.lock().unwrap().clone() {
            break c;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for execution context");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let body_text = page
        .evaluate("document.body.innerText", &ctx_id)
        .expect("evaluate failed");
    let location = page
        .evaluate("location.href", &ctx_id)
        .expect("evaluate location.href failed");

    let body_str = body_text
        .pointer("/result/value")
        .or_else(|| body_text.get("value"))
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
```

- [ ] **Step 3: Run the test against current code — expect RED**

Run:
```bash
cargo test --test integration navigate_main_frame_with_cross_origin_iframe -- --ignored --test-threads=1 --nocapture
```

Expected: FAIL. The body assertion should fail because today's code accepts the iframe target. (If the test PASSES, the bug isn't reproduced — likely the iframe-page race is timing-sensitive on this machine. Increase `MAIN_DELAY` in `tests/fixtures/mod.rs` from `50` to `150` ms and re-run. If it still passes, that's a useful signal — investigate whether the bug is real on this exact Camoufox build before continuing.)

- [ ] **Step 4: Commit the failing test**

```bash
git add tests/integration.rs
git commit --no-gpg-sign -m "test: failing regression test for cross-origin iframe attach bug"
```

---

## Task 4: Create `src/api/main_frame.rs` with `MainFrame` type

**Goal:** Introduce the new type. We copy the body of `Page` from `src/api/page.rs` and adjust the struct shape: `session` becomes non-optional, `main_frame_id` is renamed `frame_id` and is non-optional, and a new `execution_context_id: Arc<Mutex<Option<String>>>` field is added. We add a `new_for_test` constructor used only by the new sequence in Task 5. The old `Page` is left intact in `page.rs` until Task 9.

**Files:**
- Create: `src/api/main_frame.rs`
- Modify: `src/api/mod.rs` (add `pub mod main_frame;` plus re-export)

- [ ] **Step 1: Create `src/api/main_frame.rs`**

Write the file. Below is the full content. The bulk of the impl methods are copied verbatim from `src/api/page.rs` lines 247-1131 (the Page domain / Runtime domain / Network domain / Heap domain methods); only the struct, constructor, `session()`/`frame_id()` accessors, `evaluate`, `navigate`, and `screenshot` change.

Because the file is long (~1200 lines), copy `src/api/page.rs` to `src/api/main_frame.rs` first, then apply the diffs below. Run:

```bash
cp src/api/page.rs src/api/main_frame.rs
```

Then in `src/api/main_frame.rs` apply these changes:

**Change 1 — module doc + imports (replace lines 1-13):**

```rust
//! Main-frame domain wrapper.
//!
//! [`MainFrame`] wraps a page-scoped Juggler session pinned to the top frame
//! of a page. Every field is populated from authoritative protocol responses
//! at construction time, so a `MainFrame` cannot refer to a sub-frame.
//!
//! Created via
//! [`BrowserContext::new_main_frame`](crate::api::context::BrowserContext::new_main_frame).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::api::browser::Session;
use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
```

**Change 2 — replace the `Page` struct, its impl header, and the constructor / session / frame_id accessors (replaces lines 160-245 of the original page.rs):**

```rust
// ---------------------------------------------------------------------------
// MainFrame
// ---------------------------------------------------------------------------

/// A page handle pinned to the top frame.
///
/// Every field is populated from authoritative protocol responses
/// (`Browser.attachedToTarget` filtered to `type == "page"`,
/// `Page.getFrameTree`, `Runtime.executionContextCreated` filtered to
/// `auxData.frameId == frame_id`) — never from "first event seen". A
/// `MainFrame` cannot be constructed referring to a sub-frame.
pub struct MainFrame {
    /// Page-scoped Juggler session.
    session: Session,
    /// Server-assigned target ID; the target has `type == "page"`.
    target_id: String,
    /// Top frame ID, taken from `Page.getFrameTree`'s root.
    frame_id: String,
    /// Latest known main-world execution context ID for this frame.
    /// Updated by a `Runtime.executionContextCreated` listener registered
    /// in `BrowserContext::new_main_frame`, filtered on `auxData.frameId`.
    execution_context_id: Arc<Mutex<Option<String>>>,
}

impl MainFrame {
    /// Internal constructor used by `BrowserContext::new_main_frame`.
    pub(crate) fn new(
        session: Session,
        target_id: String,
        frame_id: String,
        execution_context_id: Arc<Mutex<Option<String>>>,
    ) -> Self {
        MainFrame {
            session,
            target_id,
            frame_id,
            execution_context_id,
        }
    }

    /// Returns the target ID for this page.
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Returns the top frame ID.
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }

    /// Shared handle to the cached execution context id. Updated by the
    /// listener registered in `BrowserContext::new_main_frame`.
    pub(crate) fn execution_context_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.execution_context_id)
    }

    fn session(&self) -> &Session {
        &self.session
    }
```

**Change 3 — change `navigate`'s frame-id source (replace lines 259-287 of the original):**

```rust
    /// Navigate to a URL.
    ///
    /// Always navigates the top frame (no `frame_id` override). Returns the
    /// navigation ID for cross-document navigations, `None` for same-document.
    pub fn navigate(
        &self,
        url: &str,
        options: NavigateOptions,
    ) -> Result<Option<String>, ProtocolError> {
        let mut params = json!({
            "url": url,
            "frameId": self.frame_id,
        });
        if let Some(ref referer) = options.referer {
            params["referer"] = json!(referer);
        }

        let result = self.session().send("Page.navigate", params)?;
        let nav_id = result
            .get("navigationId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        Ok(nav_id)
    }
```

(`NavigateOptions::frame_id` is now ignored. We can leave the field in place for now — Task 9 cleans it up.)

**Change 4 — `evaluate` reads exec context from the cache, with retry. Search the file for `pub fn evaluate(` and replace the entire method (originally lines 769-782 of `page.rs` — the two-line wrapper that took an `execution_context_id` argument) with this fuller version:**

```rust
    /// Evaluate a JavaScript expression in the top-frame main world.
    ///
    /// Polls the cached execution context (up to `timeout`); if `evaluate`
    /// fails with a "context destroyed" error (SPA navigation), retries up
    /// to 5 times after waiting for a fresh context.
    pub fn evaluate(
        &self,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, ProtocolError> {
        const MAX_RETRIES: u32 = 5;
        let deadline = Instant::now() + timeout;
        let mut bad_ctx: Option<String> = None;

        for attempt in 0..=MAX_RETRIES {
            if Instant::now() >= deadline {
                break;
            }

            // Acquire a usable execution context, skipping any known-bad one.
            let exec_ctx = loop {
                let cur = self.execution_context_id.lock().unwrap().clone();
                match cur {
                    Some(c) if bad_ctx.as_ref() != Some(&c) => break c,
                    _ => {
                        *self.execution_context_id.lock().unwrap() = None;
                        if Instant::now() >= deadline {
                            return Err(ProtocolError {
                                kind: ProtocolErrorKind::Closed,
                                method: Some("Runtime.evaluate".into()),
                                message: "timed out waiting for execution context".into(),
                                data: None,
                                source: None,
                            });
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            };

            match self.session().send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "executionContextId": &exec_ctx,
                }),
            ) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = format!("{e}");
                    let is_ctx_err = msg.contains("execution context")
                        || msg.contains("Failed to find");
                    if attempt < MAX_RETRIES && is_ctx_err {
                        bad_ctx = Some(exec_ctx);
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(ProtocolError {
            kind: ProtocolErrorKind::Closed,
            method: Some("Runtime.evaluate".into()),
            message: format!("evaluate failed after {MAX_RETRIES} retries"),
            data: None,
            source: None,
        })
    }
```

(The original `evaluate(&self, expression, execution_context_id)` taking an explicit context is **deleted** — `MainFrame` always uses its own cached context. The retry semantics that previously lived in `Instance::evaluate` move here.)

**Change 5 — find the standalone `impl std::fmt::Debug for Page` block (originally at lines 1134-1143 of `page.rs`, located just before the `decode_base64` helper) and replace it with:**

```rust
impl std::fmt::Debug for MainFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainFrame")
            .field("target_id", &self.target_id)
            .field("frame_id", &self.frame_id)
            .finish_non_exhaustive()
    }
}
```

**Change 6 — sweep for leftover `Page` references.** After Changes 2-5, run `grep -n '\bPage\b' src/api/main_frame.rs`. Expected matches are limited to protocol-string mentions inside `json!(...)` (e.g. `"Page.navigate"`, `"Page.screenshot"`) which must stay. If any Rust symbol references to `Page` remain (e.g. an `impl Page` block opener Change 2 didn't cover, or a function signature mentioning `Page`), rename them to `MainFrame`. The bottom-of-file `decode_base64` helper and `#[cfg(test)] mod tests` block stay verbatim.

- [ ] **Step 2: Wire the new module in `src/api/mod.rs`**

Edit `src/api/mod.rs`. Replace lines 22-31:

```rust
pub mod browser;
pub mod context;
pub mod page;

pub use browser::{Browser, BrowserOptions, ProxyConfig};
pub use context::{BrowserContext, ContextOptions, Cookie, CookieOptions, Geolocation, Viewport};
pub use page::{
    KeyEventParams, MouseEventParams, NavigateOptions, Page, Rect, ScreenshotOptions,
    WheelEventParams,
};
```

With (note `page` is still declared — we delete it in Task 9 only after migrating callers):

```rust
pub mod browser;
pub mod context;
pub mod main_frame;
pub mod page;

pub use browser::{Browser, BrowserOptions, ProxyConfig};
pub use context::{BrowserContext, ContextOptions, Cookie, CookieOptions, Geolocation, Viewport};
pub use main_frame::MainFrame;
pub use page::{
    KeyEventParams, MouseEventParams, NavigateOptions, Page, Rect, ScreenshotOptions,
    WheelEventParams,
};
```

- [ ] **Step 3: Verify the crate still builds (`Page` still exists; `MainFrame` is unused)**

Run:
```bash
cargo build --all-targets
```

Expected: PASS. There will be "unused" warnings for `MainFrame` methods — these go away in Task 5. Treat any compile *error* as a stop-and-fix; treat warnings as fine.

- [ ] **Step 4: Commit**

```bash
git add src/api/main_frame.rs src/api/mod.rs
git commit --no-gpg-sign -m "feat(api): add MainFrame type alongside Page"
```

---

## Task 5: Implement `BrowserContext::new_main_frame` with all three filter fixes

**Goal:** This is the core fix. New method on `BrowserContext` that performs the full setup with Layers 1, 2, and 3 fixed: filter `attachedToTarget` by `targetInfo.type == "page"`, call `Page.getFrameTree` for the authoritative main frame id, and filter `executionContextCreated` by both `auxData.frameId` and `auxData.name == ""`.

**Files:**
- Modify: `src/api/context.rs`

- [ ] **Step 1: Update imports in `src/api/context.rs`**

Edit `src/api/context.rs`. Replace the top of the imports block (lines 9-16):

```rust
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::browser::{Connection, ProxyConfig, Session};
use crate::api::page::Page;
use crate::protocol::errors::ProtocolError;
```

With:

```rust
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::browser::{Connection, ProxyConfig, Session};
use crate::api::main_frame::MainFrame;
use crate::api::page::Page;
use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
```

- [ ] **Step 2: Add the `new_main_frame` method to `impl BrowserContext`**

Insert this method **after** the existing `new_page` method (after `src/api/context.rs:565`):

```rust
    /// Timeout for waiting on `Browser.attachedToTarget` (Layer 1) and on
    /// `Page.getFrameTree`'s response.
    const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

    /// A skipped (non-page) attach event recorded for the diagnostic timeout
    /// error. Bounded to `MAX_SKIPPED_ATTACHES` entries.
    fn empty_skipped() -> Vec<(String, Option<String>)> {
        Vec::with_capacity(16)
    }

    /// Create a new page and return a fully-wired [`MainFrame`].
    ///
    /// Sends `Browser.newPage`, waits for a `Browser.attachedToTarget` event
    /// **filtered to `targetInfo.type == "page"`** (Layer 1 fix), calls
    /// `Page.getFrameTree` to get the authoritative main frame ID (Layer 2
    /// fix), and subscribes a `Runtime.executionContextCreated` listener
    /// **filtered to `auxData.frameId == frame_id`** (Layer 3 fix). All
    /// three filters together make it structurally impossible for the
    /// returned `MainFrame` to refer to a sub-frame.
    ///
    /// # Errors
    ///
    /// - `"timeout waiting for type=='page' attach …"` — `EVENT_TIMEOUT`
    ///   elapsed and no matching `Browser.attachedToTarget` was seen. The
    ///   error message lists any skipped attaches (up to 16) for triage.
    /// - `"Page.getFrameTree failed: …"` — Juggler rejected the request.
    ///   Indicates either a protocol mismatch or a missing method in this
    ///   Camoufox build.
    pub fn new_main_frame(&self) -> Result<MainFrame, ProtocolError> {
        const MAX_SKIPPED_ATTACHES: usize = 16;
        let conn = &self.connection;

        // === Layer 1 fix: only signal on type == "page" attaches. ===
        //
        // Channel carries (session_id, target_id). Listener also writes any
        // skipped attaches into a shared Vec for the timeout diagnostic.
        let (attach_tx, attach_rx) = mpsc::channel();
        let skipped: Arc<Mutex<Vec<(String, Option<String>)>>> =
            Arc::new(Mutex::new(Self::empty_skipped()));
        let skipped_clone = Arc::clone(&skipped);
        conn.on_event(
            "",
            "Browser.attachedToTarget",
            Box::new(move |event| {
                let ti = event.params.get("targetInfo");
                let t_type = ti
                    .and_then(|v| v.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let t_url = ti
                    .and_then(|v| v.get("url"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());

                if t_type == "page" {
                    let session_id = event
                        .params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let target_id = ti
                        .and_then(|v| v.get("targetId"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let _ = attach_tx.send((session_id, target_id));
                } else {
                    let mut s = skipped_clone.lock().unwrap();
                    if s.len() < MAX_SKIPPED_ATTACHES {
                        s.push((t_type.to_owned(), t_url));
                    }
                }
            }),
        );

        // === Send Browser.newPage. ===
        let result = self.session().send(
            "Browser.newPage",
            json!({ "browserContextId": self.context_id }),
        )?;
        let expected_target_id = result
            .get("targetId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        // === Wait for the page-typed attachedToTarget. ===
        let deadline = Instant::now() + Self::ATTACH_TIMEOUT;
        let (session_id, _target_id) = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let snapshot = skipped.lock().unwrap().clone();
                let summary: Vec<String> = snapshot
                    .iter()
                    .map(|(k, u)| match u {
                        Some(u) => format!("{{type:'{k}', url:'{u}'}}"),
                        None => format!("{{type:'{k}'}}"),
                    })
                    .collect();
                let msg = format!(
                    "timeout waiting for type=='page' attach after {}s; saw {} skipped: [{}]",
                    Self::ATTACH_TIMEOUT.as_secs(),
                    snapshot.len(),
                    summary.join(", "),
                );
                return Err(ProtocolError {
                    kind: ProtocolErrorKind::Closed,
                    method: Some("Browser.attachedToTarget".into()),
                    message: msg,
                    data: None,
                    source: None,
                });
            }
            match attach_rx.recv_timeout(remaining) {
                Ok((sid, tid))
                    if !sid.is_empty()
                        && (expected_target_id.is_empty() || tid == expected_target_id) =>
                {
                    break (sid, tid)
                }
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProtocolError {
                        kind: ProtocolErrorKind::Closed,
                        method: Some("Browser.attachedToTarget".into()),
                        message: "attach channel disconnected".into(),
                        data: None,
                        source: None,
                    });
                }
            }
        };

        // === Build the page session. ===
        let page_session = conn.create_session(session_id.clone());

        // === Layer 2 fix: ask the protocol for the main frame id. ===
        let tree = page_session
            .send("Page.getFrameTree", json!({}))
            .map_err(|e| ProtocolError {
                kind: e.kind,
                method: Some("Page.getFrameTree".into()),
                message: format!("Page.getFrameTree failed: {}", e.message),
                data: e.data,
                source: e.source,
            })?;

        // Camoufox/Juggler returns the root frame's id at either
        // `/frameTree/frame/frameId` or `/frameTree/frame/id`. If the build
        // diverges further, this is the single place to adjust.
        let frame_id = tree
            .pointer("/frameTree/frame/frameId")
            .or_else(|| tree.pointer("/frameTree/frame/id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ProtocolError {
                kind: ProtocolErrorKind::Response,
                method: Some("Page.getFrameTree".into()),
                message: "frameTree.frame.id missing or empty".into(),
                data: None,
                source: None,
            })?
            .to_owned();

        // === Layer 3 fix: subscribe with frame_id + main-world filter. ===
        let exec_ctx: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let exec_ctx_clone = Arc::clone(&exec_ctx);
        let sid_for_listener = session_id.clone();
        let frame_id_for_listener = frame_id.clone();
        conn.on_event_global(Box::new(move |event| {
            if event.session_id.as_deref().unwrap_or("") != sid_for_listener {
                return;
            }
            if event.method != "Runtime.executionContextCreated" {
                return;
            }
            let aux = event.params.get("auxData");
            let event_frame_id = aux
                .and_then(|a| a.get("frameId"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_main_world = aux
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str())
                .map(|n| n.is_empty())
                .unwrap_or(true);
            if event_frame_id != frame_id_for_listener || !is_main_world {
                return;
            }
            if let Some(ctx_id) = event
                .params
                .get("executionContextId")
                .and_then(|v| v.as_str())
            {
                *exec_ctx_clone.lock().unwrap() = Some(ctx_id.to_owned());
            }
        }));

        Ok(MainFrame::new(
            page_session,
            expected_target_id,
            frame_id,
            exec_ctx,
        ))
    }
```

- [ ] **Step 3: Verify the crate still builds**

Run:
```bash
cargo build --all-targets
```

Expected: PASS. The `Page` API still exists alongside `MainFrame`; no callers are switched yet.

- [ ] **Step 4: Commit**

```bash
git add src/api/context.rs
git commit --no-gpg-sign -m "feat(api): BrowserContext::new_main_frame with Layer 1/2/3 fixes"
```

---

## Task 6: Switch `Instance::create_page` to use `new_main_frame`

**Goal:** Replace the 250-line event-listening block in `Instance::create_page` with a single call to `context.new_main_frame()`. `ManagedPage` becomes `ManagedMainFrame`.

**Files:**
- Modify: `src/cli/instance.rs`

- [ ] **Step 1: Update imports and the `ManagedPage` struct**

Edit `src/cli/instance.rs`. Replace lines 7-34:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::api::{Browser, BrowserOptions, ContextOptions, Page, Rect, ScreenshotOptions};
use crate::config::LaunchConfig;
use crate::protocol::client::Connection;
use crate::transport::pipe::PipeTransport;

const DEFAULT_EXECUTABLE: &str = "/root/.cache/camoufox/camoufox";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// ManagedPage
// ---------------------------------------------------------------------------

/// A page with persistent execution context tracking.
pub struct ManagedPage {
    pub page: Page,
    pub session_id: String,
    /// The current execution context ID, updated by global event handlers.
    pub execution_context_id: Arc<Mutex<Option<String>>>,
}
```

With:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use serde_json::json;

use crate::api::{Browser, BrowserOptions, ContextOptions, MainFrame, Rect, ScreenshotOptions};
use crate::config::LaunchConfig;
use crate::protocol::client::Connection;
use crate::transport::pipe::PipeTransport;

const DEFAULT_EXECUTABLE: &str = "/root/.cache/camoufox/camoufox";

// ---------------------------------------------------------------------------
// ManagedMainFrame
// ---------------------------------------------------------------------------

/// A `MainFrame` plus the CLI-facing page label (e.g. `"p1"`).
pub struct ManagedMainFrame {
    pub main_frame: MainFrame,
}
```

(Note: `mpsc`, `Arc`, `Mutex`, and `EVENT_TIMEOUT` are no longer used here — they all moved into `BrowserContext::new_main_frame`.)

- [ ] **Step 2: Update the `Instance` struct field**

Replace `pages: HashMap<String, ManagedPage>` (line 47) with:

```rust
    pages: HashMap<String, ManagedMainFrame>,
```

- [ ] **Step 3: Replace `Instance::create_page` body**

Replace the entire body of `Instance::create_page` (lines 56-311) with:

```rust
    /// Create a new page in this instance's context, fully wired with
    /// session, top frame id, and execution context tracking.
    pub fn create_page(
        &mut self,
        context: &crate::api::BrowserContext,
    ) -> Result<String, String> {
        let main_frame = context
            .new_main_frame()
            .map_err(|e| format!("failed to create page: {e}"))?;

        self.page_counter += 1;
        let page_id = format!("p{}", self.page_counter);
        self.pages
            .insert(page_id.clone(), ManagedMainFrame { main_frame });
        Ok(page_id)
    }
```

- [ ] **Step 4: Delete `Instance::wait_for_exec_context`**

Delete lines 313-331 (the `fn wait_for_exec_context` method). Its logic now lives inside `MainFrame::evaluate`.

- [ ] **Step 5: Verify the crate still builds (`Instance::evaluate` etc. still broken — fixed in Task 7)**

Run:
```bash
cargo check --all-targets
```

Expected: COMPILE ERRORS — `Instance::navigate`, `Instance::evaluate`, `Instance::screenshot` still reference removed types (`Page`, `ManagedPage.page`, `wait_for_exec_context`). That's expected; Task 7 fixes them. Do NOT commit yet.

---

## Task 7: Switch `Instance::navigate / evaluate / screenshot` to `MainFrame` methods

**Goal:** Make the three remaining methods delegate to `MainFrame`'s methods. Most of the retry/wait logic moves into `MainFrame::evaluate` (already done in Task 4), so these become thin wrappers.

**Files:**
- Modify: `src/cli/instance.rs` (lines 333-530)

- [ ] **Step 1: Replace `Instance::navigate`**

Replace `Instance::navigate` (current lines 333-384 in the original — line numbers shift after Task 6) with:

```rust
    /// Navigate a page to a URL.
    ///
    /// Clears the cached execution context so the next `evaluate` waits for
    /// the post-navigation context; the wait happens inside `MainFrame::evaluate`.
    pub fn navigate(
        &self,
        page_id: &str,
        url: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // Force `evaluate` to wait for a fresh post-navigation context.
        *mp.main_frame.execution_context_handle().lock().unwrap() = None;

        mp.main_frame
            .navigate(url, Default::default())
            .map_err(|e| format!("navigate failed: {e}"))
    }
```

- [ ] **Step 2: Replace `Instance::evaluate`**

Replace `Instance::evaluate` (current lines 386-460 in the original) with:

```rust
    /// Evaluate JavaScript on a page.
    pub fn evaluate(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        let result = mp
            .main_frame
            .evaluate(expression, timeout)
            .map_err(|e| format!("evaluate failed: {e}"))?;

        // Unwrap `{result: {value: …}}` to just the value, matching today's
        // CLI output shape.
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .or_else(|| result.get("value"))
            .cloned()
            .unwrap_or(result);
        Ok(value)
    }
```

- [ ] **Step 3: Replace `Instance::screenshot`**

Replace `Instance::screenshot` (current lines 462-530 in the original) with:

```rust
    /// Take a screenshot of a page.
    pub fn screenshot(
        &self,
        page_id: &str,
        format: Option<&str>,
        quality: Option<u32>,
        path: Option<&str>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, String), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // Get viewport dimensions via evaluate (which itself waits for the
        // execution context if necessary).
        let dims = mp
            .main_frame
            .evaluate("[window.innerWidth, window.innerHeight]", timeout)
            .map_err(|e| format!("failed to get viewport dimensions: {e}"))?;

        let (width, height) = {
            let arr = dims
                .get("result")
                .and_then(|r| r.get("value"))
                .or_else(|| dims.get("value"))
                .unwrap_or(&dims);
            let w = arr.get(0).and_then(|v| v.as_f64()).unwrap_or(1280.0);
            let h = arr.get(1).and_then(|v| v.as_f64()).unwrap_or(720.0);
            (w, h)
        };

        let mime = match format {
            Some("jpeg") | Some("jpg") => "image/jpeg",
            _ => "image/png",
        };

        let options = ScreenshotOptions {
            mime_type: mime.to_string(),
            clip: Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            quality,
            omit_device_scale_factor: None,
        };

        let bytes = mp
            .main_frame
            .screenshot(options)
            .map_err(|e| format!("screenshot failed: {e}"))?;

        let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
        let out_path = match path {
            Some(p) => p.to_string(),
            None => format!("/tmp/screenshot-{page_id}.{ext}"),
        };

        std::fs::write(&out_path, &bytes)
            .map_err(|e| format!("failed to write screenshot: {e}"))?;

        Ok((bytes, out_path))
    }
```

- [ ] **Step 4: Build**

Run:
```bash
cargo build --all-targets
```

Expected: PASS. There may be unused-import or unused-variable warnings — those are fine.

- [ ] **Step 5: Run the existing unit tests**

Run:
```bash
cargo test --lib
```

Expected: PASS. These don't touch the browser; they just check protocol-internal helpers.

- [ ] **Step 6: Commit (without running integration tests yet — Task 8 does that)**

```bash
git add src/cli/instance.rs
git commit --no-gpg-sign -m "refactor(cli): route Instance through MainFrame methods"
```

---

## Task 8: Migrate the regression test to the `MainFrame` API and confirm GREEN

**Goal:** The regression test from Task 3 currently uses `Page` + manual exec-context wiring. Now that `MainFrame` exists and the fix is in, rewrite it to use the new API and confirm it passes.

**Files:**
- Modify: `tests/integration.rs`

- [ ] **Step 1: Replace the `navigate_main_frame_with_cross_origin_iframe` test body**

Replace the test added in Task 3 with this version (drops the manual `on_event_global` wiring entirely — `MainFrame` handles it):

```rust
#[test]
#[ignore]
fn navigate_main_frame_with_cross_origin_iframe() {
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
        .navigate(&server.main_url, Default::default())
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
```

- [ ] **Step 2: Update the imports at the top of `tests/integration.rs`**

Replace the existing `use camoufox::api::{...};` line (line 15):

```rust
use camoufox::api::{Browser, BrowserContext, BrowserOptions, ContextOptions, Page};
```

With:

```rust
use camoufox::api::{Browser, BrowserOptions, ContextOptions, MainFrame, Page};
```

(We keep `Page` for now because the OTHER existing tests still use it; Task 9 removes that import.)

- [ ] **Step 3: Build**

```bash
cargo build --tests
```

Expected: PASS.

- [ ] **Step 4: Run the regression test — confirm GREEN**

```bash
cargo test --test integration navigate_main_frame_with_cross_origin_iframe -- --ignored --test-threads=1 --nocapture
```

Expected: **PASS.** This is the moment the bug is fixed.

If it fails, debug:
- If body still contains `IFRAME_SENTINEL` — Layer 1 filter isn't matching. Add a `eprintln!` in the `Browser.attachedToTarget` closure in `context.rs` to log `targetInfo.type` values seen.
- If `evaluate` times out — Layer 3 filter rejected all candidate contexts. `eprintln!` the `event_frame_id` vs `frame_id_for_listener` to see the mismatch.
- If `Page.getFrameTree` errored — the response shape differs from the spec. Adjust the JSON path in Task 5 step 2 (search for `pointer("/frameTree/frame/frameId")`).

- [ ] **Step 5: Commit**

```bash
git add tests/integration.rs
git commit --no-gpg-sign -m "test: migrate iframe regression test to MainFrame API"
```

---

## Task 9: Delete `Page` and migrate remaining callers

**Goal:** Remove the now-unused `Page` type, the buggy `setup_page` test helper, and update `examples/web_browse.rs`. The probe test from Task 1 is also removed.

**Files:**
- Delete: `src/api/page.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/api/context.rs` (remove `use crate::api::page::Page;` and the old `new_page` method)
- Modify: `tests/integration.rs` (remove `setup_page`, probe test, port other tests)
- Modify: `examples/web_browse.rs`

- [ ] **Step 1: Move re-exported helper types from `page.rs` to `main_frame.rs`**

The types `NavigateOptions`, `ScreenshotOptions`, `Rect`, `KeyEventParams`, `MouseEventParams`, `WheelEventParams`, `TapEventParams`, `EmulatedMedia`, `ContentQuad`, `Point` were defined in `src/api/page.rs:14-158`. They already exist (verbatim) at the top of `src/api/main_frame.rs` because we did `cp` in Task 4. Confirm by running:

```bash
grep -nE "^pub struct (NavigateOptions|ScreenshotOptions|Rect|KeyEventParams|MouseEventParams|WheelEventParams|TapEventParams|EmulatedMedia|ContentQuad|Point)" src/api/main_frame.rs
```

Expected: 10 hits. If any are missing, copy them from `src/api/page.rs` to `src/api/main_frame.rs` before continuing.

- [ ] **Step 2: Update `src/api/mod.rs`**

Replace the module declarations and re-exports (current content):

```rust
pub mod browser;
pub mod context;
pub mod main_frame;
pub mod page;

pub use browser::{Browser, BrowserOptions, ProxyConfig};
pub use context::{BrowserContext, ContextOptions, Cookie, CookieOptions, Geolocation, Viewport};
pub use main_frame::MainFrame;
pub use page::{
    KeyEventParams, MouseEventParams, NavigateOptions, Page, Rect, ScreenshotOptions,
    WheelEventParams,
};
```

With:

```rust
pub mod browser;
pub mod context;
pub mod main_frame;

pub use browser::{Browser, BrowserOptions, ProxyConfig};
pub use context::{BrowserContext, ContextOptions, Cookie, CookieOptions, Geolocation, Viewport};
pub use main_frame::{
    KeyEventParams, MainFrame, MouseEventParams, NavigateOptions, Rect, ScreenshotOptions,
    WheelEventParams,
};
```

- [ ] **Step 3: Delete the old `new_page` method and `Page` import from `src/api/context.rs`**

In `src/api/context.rs`:
1. Remove `use crate::api::page::Page;` from the imports.
2. Delete the `pub fn new_page(&self) -> Result<Page, ProtocolError>` method (currently at lines 552-565).

- [ ] **Step 4: Delete `src/api/page.rs`**

```bash
git rm src/api/page.rs
```

- [ ] **Step 5: Remove `NavigateOptions::frame_id`**

`MainFrame::navigate` ignores `options.frame_id` (top frame is fixed). Remove the field from `NavigateOptions` in `src/api/main_frame.rs` to avoid silent-misuse — find the struct and delete the field. The struct should become:

```rust
#[derive(Debug, Clone, Default)]
pub struct NavigateOptions {
    /// HTTP referer header to send with the navigation request.
    pub referer: Option<String>,
}
```

- [ ] **Step 6: Update `examples/web_browse.rs`**

Replace lines 121-149 (the `new_page` → `set_session` → wait-for-frame block; line numbers from current file). The exact lines to replace:

```rust
    let mut page = context.new_page().expect("failed to create page");
```

…down through…

```rust
    page.set_main_frame_id(main_frame_id);
```

With:

```rust
    let page = context
        .new_main_frame()
        .expect("failed to create main frame");
```

Then anywhere else in the file that references `page.target_id()`, `page.main_frame_id()`, `page.navigate(...)`, `page.evaluate(..., &exec_ctx)`, etc., adapt:
- `page.main_frame_id()` returning `Option<&str>` → `Some(page.frame_id())` (now returns `&str` directly).
- `page.evaluate(expr, &exec_ctx_id)` → `page.evaluate(expr, Duration::from_secs(30))`.
- Delete any manual `Runtime.executionContextCreated` listener and any `set_session` / `set_main_frame_id` calls.

Run:
```bash
cargo build --example web_browse
```

Expected: PASS.

- [ ] **Step 7: Update existing integration tests in `tests/integration.rs`**

Three tests use the old API: `spawn_and_bootstrap` (no page work — no change needed), `create_context_and_page`, and `navigate_and_evaluate`. Also delete `setup_page` and the probe test.

(a) Delete the entire `fn setup_page(browser: &Browser) -> (BrowserContext, Page, String)` helper (lines 97-190 of the original file).

(b) Delete the probe test `probe_page_get_frame_tree_available` added in Task 1.

(c) Replace `create_context_and_page` (lines 214-234) with:

```rust
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

    let nav_result =
        main_frame.navigate("https://example.com", Default::default());
    assert!(
        nav_result.is_ok(),
        "navigate failed: {:?}",
        nav_result.err()
    );

    tb.teardown();
}
```

(d) Replace `navigate_and_evaluate` (lines 236-end of the test, originally lines 236-379) with:

```rust
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
        .navigate("https://example.com", Default::default())
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
```

(e) Update imports at the top of `tests/integration.rs`. The file should now import only:

```rust
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use camoufox::api::{Browser, BrowserOptions, ContextOptions};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;
```

(`mpsc`, `BrowserContext`, `Page` no longer used here.)

- [ ] **Step 8: Build everything**

```bash
cargo build --all-targets
```

Expected: PASS, no errors. Warnings about unused imports are fine — fix any that show up by deleting the imports.

- [ ] **Step 9: Run the full unit-test suite**

```bash
cargo test --lib
```

Expected: PASS.

- [ ] **Step 10: Run the full integration suite (requires Camoufox binary)**

```bash
cargo test --test integration -- --ignored --test-threads=1
```

Expected: ALL PASS, including `navigate_main_frame_with_cross_origin_iframe`, `create_context_and_page`, `navigate_and_evaluate`, `spawn_and_bootstrap`.

If `navigate_and_evaluate` fails because example.com is unreachable in this environment, swap the URL for `&server.main_url` from a `FixtureServer::start()` at the top of the test — but only as a fallback; the goal is to verify the API works against the real internet first.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit --no-gpg-sign -m "refactor: delete Page; finalize MainFrame as the public page API"
```

---

## Task 10: Final cleanup, lint, doc check

**Goal:** Make sure nothing rotted. No new behavior in this task — purely hygiene.

**Files:** any with warnings.

- [ ] **Step 1: Run clippy**

```bash
cargo clippy --all-targets -- -D warnings
```

Expected: PASS (zero warnings treated as errors). Fix any that surface. Common issues to expect:
- Unused imports in `instance.rs` (`mpsc`, `Arc`, `Mutex`) — delete.
- Unused-variable on `_timeout` in `Instance::navigate` — that's already prefixed with `_`, so clippy stays quiet.
- The `empty_skipped` helper in `context.rs` may trigger `clippy::needless_return` or similar — adjust as flagged.

- [ ] **Step 2: Check that `cargo doc` still builds**

```bash
cargo doc --no-deps
```

Expected: PASS. Any broken intra-doc links to `Page` (e.g. in `BrowserContext`'s doc comments) need to be retargeted to `MainFrame`.

- [ ] **Step 3: Re-run the full integration suite once more (paranoia)**

```bash
cargo test --test integration -- --ignored --test-threads=1
```

Expected: ALL PASS.

- [ ] **Step 4: Final commit**

```bash
git add -A
git commit --no-gpg-sign -m "chore: clippy and doc cleanup after MainFrame refactor"
```

---

## Verification checklist (end-to-end)

After Task 10:
- [ ] `cargo build --all-targets` passes
- [ ] `cargo test --lib` passes
- [ ] `cargo test --test integration -- --ignored --test-threads=1` passes (all 4 tests)
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo doc --no-deps` passes
- [ ] `src/api/page.rs` no longer exists
- [ ] No file references the symbol `Page` (other than `webPage`, `setPage`, etc. — match-case `\bPage\b`):
  ```bash
  grep -rn "\bPage\b" src/ tests/ examples/ | grep -v "homepage\|webpage\|MainPage" | wc -l
  ```
  should be 0 (or trivially small — e.g. `Page.getFrameTree` is a protocol string, which is fine).
- [ ] `git log --oneline` shows ~10 small commits, one per task plus the original spec.

---

## Known spec deferrals

The spec's Error-handling table calls for a Layer-3 timeout diagnostic that lists "sibling-frame contexts seen" (e.g. `"saw 1 context for frame <other_id>"`). The plan ships a simpler error message (`"timed out waiting for execution context"`). The richer diagnostic would require the Layer-3 listener to also stash contexts it *rejects* (different `frameId`) in a side buffer for the timeout path. That's ~15 lines of extra plumbing; it adds debuggability but does not affect correctness or the regression test. **Follow-up:** if Layer 3 ever silently filters out something we wanted, add the sibling-context cache then.

## Risks / fallbacks (carry-over from spec)

If Task 1's probe shows `Page.getFrameTree` is unavailable (the spec's documented worst case), do not delete the Layer 2 event-listening code in Task 6. Instead, in Task 5 step 2, replace the `Page.getFrameTree` block with a `Page.frameAttached` listener filtered on `parentFrameId.is_none()`:

```rust
// Hybrid fallback: pick the first frameAttached with no parent.
let (frame_tx, frame_rx) = mpsc::channel::<String>();
let sid_for_filter = session_id.clone();
conn.on_event_global(Box::new(move |event| {
    if event.method != "Page.frameAttached" {
        return;
    }
    if event.session_id.as_deref().unwrap_or("") != sid_for_filter {
        return;
    }
    let has_parent = event
        .params
        .get("parentFrameId")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if has_parent {
        return;
    }
    if let Some(fid) = event.params.get("frameId").and_then(|v| v.as_str()) {
        let _ = frame_tx.send(fid.to_owned());
    }
}));
let frame_id = frame_rx
    .recv_timeout(Self::ATTACH_TIMEOUT)
    .map_err(|_| ProtocolError {
        kind: ProtocolErrorKind::Closed,
        method: Some("Page.frameAttached".into()),
        message: "timeout waiting for main frame (no parent) attach".into(),
        data: None,
        source: None,
    })?;
```

The MainFrame shape and the Layer 1 / Layer 3 fixes are unchanged in the fallback path.
