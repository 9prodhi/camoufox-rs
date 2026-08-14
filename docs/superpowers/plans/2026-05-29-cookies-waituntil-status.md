# camoufox-rs: cookie export + navigate lifecycle + HTTP status — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Fresh subagent per task; spec-then-quality review between tasks. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add three capabilities to the camoufox-rs CLI that unblock robust host-side fetching of session-gated content (the G1/G3/G4 gaps from a session-gated portal spike): export the cookie jar incl. HttpOnly cookies; let `navigate` wait for a page lifecycle event; surface the main-document HTTP status in the `navigate` result.

**Architecture:** Mirror the existing download-detection fix pattern (commits `b1f5c43`/`fca5698`): thread a new option/result from CLI → daemon IPC → `InstanceManager`/`Instance` → `api` layer → `protocol` layer; subscribe to a Juggler event in the protocol/api layer and resolve it to the blocked caller. All three changes are **purely additive** — a new `cookies` subcommand, an opt-in `--wait-until` flag, and a new `status_code` field on the navigate result — so existing callers keep working.

**Tech Stack:** Rust; clap CLI + Unix-socket daemon (`--features cli`); Juggler protocol over a fd pipe. Unit tests via the in-process `MockTransport`/`SilentMockTransport` pattern; integration tests `#[ignore]`d, run with `cargo test --test integration -- --ignored --test-threads=1` against a live camoufox at `~/.cache/camoufox/camoufox` (or `$CAMOUFOX_BIN`).

**Source of requirements:** gaps G1, G3, G4 identified during a session-gated portal spike, and the recon report grounding this plan.

**Scope decisions (read before implementing):**
- **`networkidle` is OUT of scope.** The Juggler protocol exposes no networkidle primitive (`Page.eventFired` gives only `load` and `DOMContentLoaded`). `--wait-until` supports `load` and `domcontentloaded` only; any other value returns a clear error. networkidle (client-side request-tracking) is a documented follow-up.
- **Additive only.** Do not change the existing success shape of `navigate` (`{navigation_id}`) beyond ADDING `status_code`. Do not make `navigate` fail on 4xx/5xx — surface the status, let the caller decide. `--wait-until` absent ⇒ current behavior unchanged.
- **Preserve the download-detection behavior** (`NavigationBecameDownload`) — these changes layer on top of it.

---

## File map (anchors from recon; verify before editing)

| Layer | File | Role for this work |
|---|---|---|
| CLI args | `src/cli/commands.rs` | `Command` enum (add `Cookies`; add `--wait-until` to `Navigate`) |
| CLI IPC | `src/cli/ipc.rs` | `DaemonRequest`/`DaemonResponse` (add `Cookies`; add `wait_until` to `Navigate`; `status_code` in response data) |
| CLI bin | `src/bin/camoufox.rs` | map `Command` → `DaemonRequest` |
| CLI daemon | `src/cli/daemon.rs` | `dispatch()` arms |
| CLI routing | `src/cli/instance.rs` | `InstanceManager`/`Instance` methods (`cookies`, navigate threading) |
| CLI output | `src/cli/output.rs` | `print_response()` human + `--json` |
| API | `src/api/main_frame.rs` | `navigate()` (wait-until, status); add a reusable page-event-wait helper |
| API | `src/api/browser.rs` / `context.rs` | root-session access for `Browser.getCookies`; context_id |
| Protocol | `src/protocol/client.rs` | `Session::send*`, `on_event`, reader thread; mock transport for tests |
| Protocol | `src/protocol/events.rs` | event router |
| Docs/tests | `docs/PROTOCOL.md`, `tests/integration.rs`, `tests/fixtures/mod.rs` | protocol reference; integration tests + fixtures |

Implementers: the download-detection commits `b1f5c43` and `fca5698` are the canonical template — `git show` them before starting.

---

## Task 1 — Branch + green baseline (controller-run; no subagent)

- [ ] Feature branch `feat/cookies-waituntil-status` created off `main` (controller does this before dispatching).
- [ ] Baseline confirmed: `cargo test --lib --features cli` (166 pass), `cargo clippy --features cli --all-targets` (0 issues), live camoufox present.
- [ ] Confirm the integration harness runs in this env: `cargo test --test integration -- --ignored --test-threads=1 --exact navigate_basic` (or any one existing ignored test) passes. If the harness can't reach a live browser, STOP and surface it.

---

## Task 2 — G1: `cookies` command (export jar incl. HttpOnly)

**What:** A new CLI subcommand `camoufox cookies <instance_id>` that returns all cookies for the instance's browser context — including HttpOnly — via the Juggler `Browser.getCookies` root-session RPC. `--json` emits the full cookie objects; human mode prints `name=value` (one per line) plus a count.

**Why:** lets a host-side HTTP client replay a camoufox session to fetch session-gated bodies (e.g. `PHPSESSID`-style session-cookie cases) — retiring the in-session XHR workaround.

**Files:** `src/cli/commands.rs`, `src/cli/ipc.rs`, `src/bin/camoufox.rs`, `src/cli/daemon.rs`, `src/cli/instance.rs`, `src/cli/output.rs`; possibly a thin accessor in `src/api/browser.rs`. Tests: unit in the touched modules + `tests/integration.rs` (+ `tests/fixtures/mod.rs` if a cookie-setting fixture is needed).

**Protocol:** `Browser.getCookies` with `{ "browserContextId": <ctx_id> }` on the **root session** returns `{ cookies: [ { name, value, domain, path, httpOnly, secure, ... } ] }` (PROTOCOL.md §6 / Cookie type). The `BrowserContext.context_id` is reachable from `InstanceManager` (it holds the contexts map). No event subscription needed — synchronous RPC.

**TDD / acceptance:**
- [ ] **Unit (mock transport):** a test that drives the `cookies` path with a `MockTransport` scripted to answer `Browser.getCookies` with a payload containing an `httpOnly: true` cookie; assert the returned/serialized data includes that cookie with `httpOnly` preserved. (Follow the `MockTransport` pattern in `src/protocol/client.rs` tests.)
- [ ] **Unit:** `DaemonRequest::Cookies` / `DaemonResponse` serde round-trip; `print_response` formats the cookies block in both `--json` and human mode (assert HttpOnly cookies are NOT dropped).
- [ ] **Integration (`#[ignore]`):** launch a real browser, navigate to a page that sets a cookie (extend `tests/fixtures/mod.rs` with a `CookieServer` that sets one normal + one `HttpOnly` cookie via `Set-Cookie`), call the cookies path, assert BOTH cookies come back and the HttpOnly flag is present. Run with `cargo test --test integration -- --ignored --test-threads=1`.
- [ ] `cargo clippy --features cli --all-targets` clean; `cargo fmt`.
- [ ] Commit.

**Done when:** `camoufox --json cookies <inst>` returns a cookie array including HttpOnly cookies, with unit + ignored-integration coverage, clippy/fmt clean.

---

## Task 3 — G3: `navigate --wait-until=load|domcontentloaded`

**What:** Add an opt-in `--wait-until <state>` flag to `navigate`. When set, after the `Page.navigate` RPC is acked, block until the matching `Page.eventFired` (`name == "load"` or `name == "DOMContentLoaded"`) fires on the page session, bounded by the existing `--timeout`. Absent flag ⇒ current behavior (return after ack). Unsupported value (incl. `networkidle`) ⇒ a clear `Err`/error response naming the supported set.

**Why:** removes the "sleep and pray" pattern; ensures the DOM/scripts/cookies are settled before extracting URLs or exporting cookies (SPA cold-load and jQuery-not-yet-loaded symptoms).

**Files:** `src/cli/commands.rs` (flag), `src/cli/ipc.rs` (`wait_until: Option<String>`), `src/bin/camoufox.rs`, `src/cli/daemon.rs`, `src/cli/instance.rs`, `src/api/main_frame.rs` (the wait logic + a **reusable `wait_for_page_event(name, timeout)` helper** — Task 4 will reuse it). Tests: unit + integration.

**Protocol:** `Page.eventFired { frameId, name }` on the **page session** (PROTOCOL.md). Subscribe via `connection.on_event(session_key, "Page.eventFired", handler)`; the handler pushes onto an `mpsc` channel the caller `recv_timeout`s. Register the listener and then check — `load` fires after navigate ack, so registering right after the ack is safe; deregister/clean up after. Map `domcontentloaded` (CLI) → `DOMContentLoaded` (protocol).

**TDD / acceptance:**
- [ ] **Unit (mock transport):** script a `MockTransport` to ack `Page.navigate` then emit a `Page.eventFired{name:"load"}` event; assert `navigate(..., wait_until=load)` returns only after the event. Add a case where the event never arrives ⇒ returns a `Timeout` error within the bound and leaves the session usable (mirror the `SilentMockTransport` timeout tests).
- [ ] **Unit:** `--wait-until=networkidle` (and any unknown value) ⇒ clear error naming `load`/`domcontentloaded`; serde round-trip of the new IPC field; default (no flag) path unchanged.
- [ ] **Integration (`#[ignore]`):** `navigate <inst> <page> <fixture-url> --wait-until load` against a fixture page whose `load` fires after a deferred resource; assert it blocks until load (e.g. a DOM marker only present post-load is readable immediately after navigate returns).
- [ ] clippy/fmt clean; commit.

**Done when:** `--wait-until=load|domcontentloaded` blocks correctly with timeout safety, unsupported values error clearly, default behavior unchanged, unit + ignored-integration coverage.

---

## Task 4 — G4: surface main-document HTTP status in `navigate`

**What:** Additively include the main-document HTTP `status_code` in the `navigate` result (e.g. `{ navigation_id, status_code }`). Capture it from `Network.responseReceived` for the navigation's main-document request. `navigate` still succeeds on 4xx/5xx — it just reports the status. If the status genuinely cannot be captured, return `status_code: null` (never fail navigate for lack of status).

**Why:** today a 403/404 (e.g. an expired JWT-signed URL) returns `ok`; callers (incl. a corpus harness) need the status to detect failures.

**Files:** `src/api/main_frame.rs` (capture logic; reuse Task 3's event-wait/collect helper), `src/cli/instance.rs` (thread status out), `src/cli/ipc.rs` (`status_code: Option<u16>` in navigate response data), `src/cli/daemon.rs`, `src/cli/output.rs` (print status). Tests: unit + integration.

**Protocol & ordering (the tricky part — read recon §6/risk 2):** `Network.responseReceived { requestId, status, ... }` and `Network.requestWillBeSent { requestId, navigationId, cause }`. To get the **main-document** status: subscribe to these on the page session **before** issuing `Page.navigate` (the response arrives async, possibly before/around the ack), correlate the main-document request (`cause == "document"` and/or `frameId == self.frame_id`, and/or `navigationId` matching the value `Page.navigate` returns), and capture its `status`. **Verify early** that `Network.responseReceived` events flow without an explicit enable (existing code uses `Network.getResponseBody` without enabling). If they do not flow in this build, STOP and report — do not silently ship a status that's always null.

**TDD / acceptance:**
- [ ] **Unit (mock transport):** script `Network.requestWillBeSent{cause:"document", navigationId:X}` then `Network.responseReceived{requestId, status:404}` around a `Page.navigate` that returns `navigationId:X`; assert `navigate` reports `status_code == 404` while still returning `Ok`. Add a case where no response event arrives ⇒ `status_code == None`, navigate still `Ok`.
- [ ] **Unit:** navigate response serde includes `status_code`; `print_response` shows it; absence of the field for legacy callers doesn't break (additive).
- [ ] **Integration (`#[ignore]`):** navigate to a fixture URL that returns **404** (extend `tests/fixtures/mod.rs` with a `StatusServer`, or reuse `AttachmentServer`'s server with a 404 route); assert `status_code == 404` and navigate did not error. Also assert a normal 200 page reports `200`.
- [ ] **Risk gate:** if early runtime check shows `Network.responseReceived` does not flow, report BLOCKED with findings rather than shipping a no-op.
- [ ] clippy/fmt clean; commit.

**Done when:** `navigate` reports the main-document status (200/403/404) additively without failing on non-2xx, with the always-on assumption verified, unit + ignored-integration coverage.

---

## Task 5 — Live end-to-end verification + docs

**What:** Prove the host-fetch unlock end-to-end and document the new surface. (May run in the controller / a verification subagent; the live browsing parts go through the camoufox CLI per project rule.)

- [ ] **G1 unlock proof:** in one camoufox session, navigate to a site that sets an HttpOnly session cookie, `cookies <inst>` to export it, then a **host-side `curl` carrying that cookie** fetches a session-gated resource successfully — demonstrating the in-session-XHR workaround is no longer required. (Use a benign cookie-gated test endpoint, e.g. httpbin `/cookies/set` → `/cookies`.)
- [ ] **G3/G4 smoke:** `navigate --wait-until load` on a real page returns post-load; `navigate` to a known 404 reports `status_code:404` without erroring.
- [ ] **Docs:** update `docs/PROTOCOL.md` notes if needed; add the three commands/flags to `README.md`; add a CHANGELOG/commit note. Cross-reference gaps G1/G3/G4 as the motivation.
- [ ] Commit.

**Done when:** the cookie-export → host-fetch path is demonstrated working, and the new CLI surface is documented.

---

## Self-review (run after drafting; fix inline)
- **Coverage:** G1 → Task 2; G3 → Task 3; G4 → Task 4; unlock proof → Task 5. networkidle explicitly deferred (documented).
- **Additivity:** new `cookies` command (no existing surface touched); `--wait-until` opt-in (default unchanged); `status_code` added to navigate data (existing `navigation_id` preserved). No breaking change to the corpus runbook/sub-agents.
- **Consistency:** Task 3 introduces `wait_for_page_event`/event-collect helper; Task 4 reuses it — same `on_event` + `mpsc` + `recv_timeout` pattern, mirroring the download-detection template.
- **Risk:** the one runtime unknown (Network events always-on, G4) is gated in Task 4 with an explicit STOP-and-report rather than a silent no-op.
