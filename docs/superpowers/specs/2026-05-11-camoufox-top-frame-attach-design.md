# Top-frame attach fix — design spec

**Date:** 2026-05-11
**Status:** Approved for implementation planning
**Scope:** Bug fix + type-safety refactor in `src/api/` and `src/cli/instance.rs`

## Problem

When camoufox-rs creates a new page and the target site contains an early cross-origin iframe (e.g. `aax-eu.amazon-adsystem.com` on amazon.in), `evaluate` returns content from the iframe instead of the top page, and `location.href` reflects the iframe URL. The CLI is effectively unusable on any site with ad-pixel iframes or third-party widgets that attach faster than the main page.

## Root cause

Three accept-the-first-event spots in `src/cli/instance.rs` allow a non-page target to win a race against the real page target.

| Layer | Location | Race condition |
|---|---|---|
| 1 — primary | `instance.rs:66-85`, awaited at `:160` | `Browser.attachedToTarget` handler captures the first event with no filter on `targetInfo.type`; iframe/worker targets get accepted as "the page". |
| 2 — amplifier | `instance.rs:172-188` | `Page.frameAttached` handler captures the first frame as `main_frame_id` without checking `parentFrameId` is absent. |
| 3 — amplifier | `instance.rs:266-286` | Execution-context tracker filters on `auxData.name == ""` but not on `auxData.frameId == main_frame_id`; a sub-frame's main-world context can become the evaluate target. |

## Goal

Make it structurally impossible for page-scoped operations (`navigate`, `evaluate`, `screenshot`) to run against anything other than the top frame of the intended page. Authoritative protocol responses replace event races at every layer.

## Non-goals

- Sub-frame access (no `evaluate_in_frame(frame_id, …)` API).
- Changes to transport, codec, daemon socket protocol, or CLI command names/arguments.
- Real-site CI tests against external services (amazon.in, etc.).
- Retry/backoff loops on Layer-1 timeout.
- Deprecation period for the old API — caller list is one example file, updated in the same PR.

## Architecture

A new public type `MainFrame` replaces the existing `Page` struct as the single thing every page-scoped operation works against. Lives in `src/api/main_frame.rs` (replaces `src/api/page.rs`).

```rust
pub struct MainFrame {
    session: Session,                                 // page-scoped Juggler session
    target_id: String,                                // server-assigned, type == "page"
    frame_id: String,                                 // root frame from Page.getFrameTree
    execution_context_id: Arc<Mutex<Option<String>>>, // filtered by frame_id
}

impl MainFrame {
    pub async fn navigate(&self, url: &str) -> Result<()>;
    pub async fn evaluate(&self, expr: &str) -> Result<Value>;
    pub async fn screenshot(&self) -> Result<Vec<u8>>;
}
```

All four fields are populated from authoritative protocol responses, never from "first event seen". Once constructed, a `MainFrame` cannot be made to refer to a sub-frame.

CLI surface is unchanged: `camoufox new-page`, `navigate`, `evaluate`, `screenshot` keep their names, args, and wire formats. Only the internal Rust API renames: `BrowserContext::new_page()` → `BrowserContext::new_main_frame()`.

`Page` is deleted. `examples/web_browse.rs` is updated in the same PR.

## Components

| File | Action | What changes |
|---|---|---|
| `src/api/main_frame.rs` | new | `MainFrame` struct + `navigate`/`evaluate`/`screenshot`; encapsulates the post-attach `Page.getFrameTree` call that populates `frame_id` |
| `src/api/page.rs` | deleted | Replaced wholesale |
| `src/api/context.rs` | edit | `new_page()` renamed `new_main_frame() -> Result<MainFrame>`; internal flow described below |
| `src/lib.rs` | edit | Re-export `MainFrame`, remove `Page` |
| `src/cli/instance.rs` | edit | All three filter fixes; `ManagedPage` → `ManagedMainFrame`; event handlers gain `targetInfo.type == "page"` and `auxData.frameId == frame_id` predicates; diagnostic-rich timeout error |
| `src/cli/daemon.rs` | edit | Internal call sites updated; CLI command names unchanged on the wire |
| `examples/web_browse.rs` | edit | Updated to call new API |
| `tests/fixtures/main.html` | new | Top page; contains `MAIN_SENTINEL_8f3a2b1c` and an iframe referencing `iframe.html` on a different port |
| `tests/fixtures/iframe.html` | new | Cross-origin iframe content; contains `IFRAME_SENTINEL_4e9d7c0a` |
| `tests/fixtures/server.rs` | new | `tiny_http`-based dev test server binding two ephemeral ports on `127.0.0.1`; returns `(main_url, iframe_url)`; shuts down on Drop |
| `tests/integration.rs` | edit | Adds `navigate_main_frame_with_cross_origin_iframe`, `navigate_plain_page_still_works`; updates `navigate_and_evaluate` to the new API |

**Internal data flow split:** `ManagedPage { page, session_id, execution_context_id }` becomes `ManagedMainFrame { main_frame, page_label }`. All frame/session/context state lives inside `MainFrame`; the manager only adds the CLI-facing `"p1"`/`"p2"` label.

Unchanged: transport (`src/transport/`), codec (`src/codec/`), daemon socket wire format, `InstanceManager` outer shape.

## Data flow — new `new_main_frame()` sequence

1. Send the existing "create page" RPC (already in `context.rs`) on the root session.
2. Listen for `Browser.attachedToTarget` events on the root session, filtered:
   - `targetInfo.type == "page"` → accept; capture `targetId` + `sessionId`; stop listening.
   - Otherwise → record `SkippedAttach { kind, url }` in a bounded `Vec` (cap 16) for the diagnostic timeout error; keep listening.
   - **Layer 1 fix.**
3. Construct a page-scoped `Session` from `sessionId`.
4. Synchronously send `Page.getFrameTree` on the new session. Response's `frameTree.frame.id` is the main frame by definition. Capture as `frame_id`.
   - **Layer 2 fix — replaces the deleted `frame_rx` channel and `set_main_frame_id` wait at `instance.rs:172-188`.**
5. Subscribe `Runtime.executionContextCreated` events on the new session, filtered:
   - `auxData.frameId == frame_id` AND `auxData.name == ""` (main world) → store id in `Arc<Mutex<Option<String>>>`.
   - Otherwise → drop.
   - **Layer 3 fix.**
6. Return `MainFrame { session, target_id, frame_id, execution_context_id }`.

### `MainFrame::navigate(url)`
Send `Page.navigate` with `frameId == self.frame_id` on `self.session`. Wait for `Page.loadEventFired` (or `Page.frameNavigated` with matching `frameId`). Same waiting logic as today, now correctly scoped.

### `MainFrame::evaluate(expr)`
Read `execution_context_id`; if `None`, wait up to `EXEC_CTX_TIMEOUT`. Send `Runtime.evaluate` with the id. On `executionContextDestroyed` error (SPA reload mid-call), retry up to 5× — preserved from current behavior at `instance.rs:392-460`.

### `MainFrame::screenshot()`
Send `Page.screenshot` on `self.session`. Session is page-scoped; no frame parameter needed.

### What this kills

- `attach_rx.recv_timeout` at `instance.rs:160` (Layer 1 race) — replaced by filtered listening in step 2.
- `frame_rx` channel and `set_main_frame_id` wait at `instance.rs:172-188` (Layer 2 race) — replaced by `getFrameTree` in step 4.
- Unfiltered execution-context capture at `instance.rs:266-286` (Layer 3 race) — replaced by frame-id-filtered capture in step 5.

## Error handling

All errors propagate as `Result<_, String>` through the existing daemon → CLI plumbing. No silent fallbacks.

| Failure | Behavior | Error message shape |
|---|---|---|
| Layer 1 timeout — no `type=="page"` attach within `EVENT_TIMEOUT` | Error; include skipped attaches | `"timeout waiting for type=='page' attach after Ns; saw 2 skipped: [{type:'iframe', url:'https://aax-eu...'}, {type:'worker', url:'...'}]"` |
| Layer 2 — `Page.getFrameTree` RPC fails or unsupported | Error immediately | `"Page.getFrameTree failed: <protocol error>"` — surfaces Juggler-compat risk loudly |
| Layer 3 timeout — no main-frame main-world execution context within `EXEC_CTX_TIMEOUT` | Error; include sibling-frame contexts seen | `"timeout waiting for execution context for frame <frame_id>; saw 1 context for frame <other_id>"` |
| `evaluate` retry exhaustion | Same as today | `"executionContextDestroyed after 5 retries"` (unchanged) |
| `navigate` load-event timeout | Same as today | Unchanged |

A bounded `SkippedAttach { kind: String, url: Option<String> }` (cap 16 entries) lives next to the listener in `src/cli/instance.rs`. Transport / pipe-close errors propagate unchanged.

`MainFrame` is never partially constructed — either all four fields are real or `new_main_frame()` returns an error. This invariant is what makes the compile-time guarantee load-bearing.

## Testing

One local-fixture integration test, no unit tests.

### Fixture

`tests/fixtures/main.html`:
```html
<body data-testid="main">MAIN_SENTINEL_8f3a2b1c
  <iframe src="http://127.0.0.1:{IFRAME_PORT}/iframe.html"></iframe>
</body>
```

`tests/fixtures/iframe.html`:
```html
<body>IFRAME_SENTINEL_4e9d7c0a</body>
```

Sentinels are distinct, non-overlapping fixed strings.

### Test server

`tests/fixtures/server.rs` — small helper, binds two ephemeral ports on `127.0.0.1` (different ports = different origin = OOPIF path). Returns `(main_url, iframe_url)`; the iframe URL is substituted into `main.html` at serve time. Drop shuts both down. Uses `tiny_http`.

### Test cases (in `tests/integration.rs`)

| Test | Asserts |
|---|---|
| `navigate_main_frame_with_cross_origin_iframe` | After navigating to `main_url`, `evaluate('document.body.innerText')` contains `MAIN_SENTINEL_...` and does **not** contain `IFRAME_SENTINEL_...`. `evaluate('location.href')` equals `main_url`. **Regression test for the Amazon bug.** |
| `navigate_plain_page_still_works` | Smoke: navigate to a no-iframe page; evaluate returns the sentinel. Guards happy path against the refactor. |

`tests/integration.rs::navigate_and_evaluate` (existing, line 238) is updated to the new API. `examples/web_browse.rs` is updated in the same PR.

CI gating matches existing integration tests — same Camoufox-binary requirement, same env-var pattern, no new gating.

## Open verification step (handled in the implementation plan)

Before deleting the Layer-2 event-based code, confirm `Page.getFrameTree` is available in this Camoufox build. The implementation plan's first task is a small write-only probe: send `Page.getFrameTree` on a freshly attached page session via the existing transport and assert a non-error response with a populated `frameTree.frame.id`. The probe is removed before the PR lands.

If the probe shows `getFrameTree` is unavailable, the design falls back to a hybrid for Layer 2 only: keep a `Page.frameAttached` listener, but filter it on `parentFrameId.is_none()` instead of taking the first event. The `MainFrame` shape, Layer 1 fix, and Layer 3 fix are unaffected.

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| `Page.getFrameTree` missing in Camoufox | High if it happens, low likelihood (present in upstream Juggler) | Verification step above; documented fallback to Approach 3 hybrid for Layer 2 |
| Filter on `targetInfo.type == "page"` discards a target type we actually want (some Juggler-specific variant) | Low | Diagnostic error lists skipped attaches; one user-visible failure makes the misclassification obvious |
| `execution_context_id` arrives slowly on a sluggish first page load, causing first `evaluate` to time out | Low | `EXEC_CTX_TIMEOUT` retained from today's code, not tightened |
| The fixture iframe doesn't reliably win the race against the main frame's events on every Camoufox build, making the regression test a no-op | Medium | If observed, swap data-URL iframe with a deliberately-delayed main-page response (`Content-Type` after a 50ms sleep on port A); the test server is local and trivially tunable |

## Out of scope (explicit)

- Sub-frame inspection API.
- Daemon protocol / wire format changes.
- Real-site CI tests.
- Unit tests of filter predicates in isolation.
- Library-level deprecation period (only example uses the public `Page` type today).
