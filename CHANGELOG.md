# Changelog

All notable changes to camoufox-rs are documented here.

## [Unreleased]

### Added

- **Download log and file saving (`downloads` command and `--downloads-dir`)**:
  - `launch --downloads-dir <path>` configures a validated absolute directory where downloads
    are saved (defaults to `cancel` behavior when omitted, preventing uninspected files on disk).
  - Saved files are written to `<downloads_dir>/<uuid>`, verified on disk against Camoufox 158;
    paths are never derived from untrusted `suggestedFileName`.
  - `downloads <instance_id> [--wait <secs>]` views or waits for download lifecycle events via
    a context-scoped `DownloadLog`. Condvar waiting runs outside the per-instance mutex so it
    never blocks concurrent commands on that instance.
  - `click` and `click_selector` responses include recent download events (`"downloads": [...]`),
    enabling the two-step agent pattern `click <inst> <page> <sel> && downloads <inst> --wait 10`.

- **Browsing command surface (20 new CLI subcommands)**: the CLI grew from 12 primitives to a
  full browsing/scraping surface. Every new command is a thin CLI/IPC wrapper over the existing
  `api` layer — no new protocol plumbing.

  - *Reading*: `text` (page `innerText`, `--selector` to scope), `html` (`outerHTML` of a
    selector, or the whole document), `links` (every `<a href>` as `text → href`, hrefs
    resolved to absolute URLs), `data` (structured metadata split into `--og` /`--jsonld` /
    `--meta` groups; no flag returns all three), `url` (current URL + title).
  - *Navigation / waiting*: `wait --selector <css> [--timeout N]` (polls until the element
    exists, reports the elapsed time), `reload`, `back`, `forward`.
  - *Cookies / headers*: `cookie <name>=<value>` (sets one cookie via `Browser.setCookies`;
    binds to the page's current URL unless `--url` / `--domain` is given), `header
    'Name: value'` (adds an extra request header via `Network.setExtraHTTPHeaders`; headers
    accumulate per page across calls).
  - *Interaction*: `click <css>` (resolve → scroll into view → trusted click at the centre),
    `fill <css> <value>`, `type <text>`, `press <key>`, `hover <css>`, `select <css> <value>`
    (matches by option value, label, or visible text), `scroll [css]`.
  - *Tabs*: `tabs` (page id + URL + title per open page), `close-tab`.

  Selectors and values are passed to the page as JSON-encoded string literals, so quotes and
  backslashes cannot break out of the generated expression. Covered by the
  `reading_commands_extract_text_html_links_and_metadata` and
  `interaction_commands_fill_press_and_click_by_selector` integration tests.

- **`click` accepts a CSS selector**: `click <instance_id> <page_id> <selector>` is the new
  selector form. The original `click <instance_id> <page_id> <x> <y>` coordinate form is
  unchanged — two trailing numeric arguments still mean coordinates.

- **`screenshot --selector <css>` and `--clip x,y,width,height`**: crop a screenshot to one
  element (scrolled into view first) or to an explicit region. `--json` echoes the resolved
  `clip` rectangle.

- **`click <instance_id> <page_id> <x> <y>` command**: dispatches a trusted left-click
  (`mousemove` → `mousedown` → `mouseup`) at viewport coordinates via the Juggler
  `Page.dispatchMouseEvent` call. Because the events originate from the browser rather than
  from JavaScript, the page sees `event.isTrusted === true`, so the click drives cross-origin
  and closed-shadow widgets (e.g. Cloudflare Turnstile) that synthetic JS
  `click()` / `dispatchEvent` cannot reach. `--json` returns
  `{ "clicked": true, "x": <x>, "y": <y> }`. Covered by the
  `click_command_dispatches_trusted_event_at_coordinates` integration test, which asserts the
  dispatched event arrives with `isTrusted === true` at the requested coordinates.

- **G1 — `cookies <instance_id>` command**: exports the full in-session cookie jar via the
  Juggler `Browser.getCookies` call. Includes `HttpOnly` cookies (which are inaccessible from
  JavaScript). `--json` returns full cookie objects (name, value, domain, path, httpOnly,
  secure, sameSite, expires, session, size). The exported jar can be passed directly to
  host-side HTTP clients (e.g. `curl --cookie`) to fetch session-gated content without
  requiring in-session XHR round-trips.

- **G3 — `navigate --wait-until load|domcontentloaded`**: the `navigate` command now accepts
  a `--wait-until` flag that blocks until the specified browser lifecycle event fires, bounded
  by `--timeout` seconds. Accepted values: `load` (fires after all resources on the page have
  loaded) and `domcontentloaded` (fires once the HTML is parsed, before sub-resources). Any
  other value is rejected with an error at dispatch time.

- **G4 — `navigate` reports `status_code`**: `--json` output now includes `status_code`, the
  final main-document HTTP status code after following all redirects. `status_code` is `null`
  when the status is uncapturable (e.g. `about:` pages, navigation that errors before a
  network response is received). Navigate never fails on 4xx/5xx responses — the result is
  always `"ok": true` and the caller inspects `status_code` to decide how to proceed.

- **`evaluate --main-world` / `MainFrame::evaluate_main_world`.** On Camoufox 158, `evaluate`
  runs in an isolated world: the DOM is shared, but globals set by page scripts are invisible.
  The new API runs a synchronous expression in the page's main world through an injected
  `<script>` bridge (expression passed only as a JSON string literal) and returns the plain
  value. The page can observe it, and a strict CSP blocks it.
- Protocol tracing: `RUST_LOG=camoufox::protocol=trace` logs every SEND/RECV message with
  bounded previews (no cost when disabled).
- `ProtocolError::{new, closed_with, response_msg, timeout_msg}` constructors;
  `Connection::off_event_global`; `NulJsonCodec::with_max_frame`; `process::ChildGuard`.

### Fixed

- **Undefined behaviour in the connection.** `SplitTransport` handed `&mut PipeTransport`
  to the reader and writer threads at the same time (both wrote `closed`, and `close()` could
  free the writer mid-send). Replaced with an owned reader/writer split.
- **One bad handler or frame could kill the connection.** Event handlers ran under the
  connection mutex, so a panic poisoned it and a re-entrant handler deadlocked the reader.
  Handlers now run outside the lock inside `catch_unwind`. A single undecodable frame (e.g. a
  lone UTF-16 surrogate from `console.log`) used to close the whole connection; it is now
  skipped.
- **Handler leaks.** Each `new_main_frame` permanently registered four event handlers that
  ran on every later event, and one buffered without bound after an attach timeout. All are
  now RAII-scoped; `MainFrame` removes its listeners on drop.
- **60s hangs on closed or crashed tabs.** `Browser.detachedFromTarget` and `Page.crashed`
  now fail in-flight calls immediately. When the browser exits, event waiters are released.
- **Downloads landed in `~/Downloads`.** Download cancellation was only applied to the
  default context. Every new context now cancels downloads unless configured otherwise.
- **`evaluate` ignored its timeout** for the RPC itself (it used the 60s default) and reported
  timeouts as `Closed`. The CLI `evaluate` reported page exceptions as success and printed a
  raw `{"result":{}}` for `undefined`.
- **Orphaned browsers and leaked fds.** A failed launch left the browser running with its
  profile deleted underneath it. Failed spawns leaked two fds each. Killing the daemon left
  browsers behind. Now: kill-on-drop guard, `OwnedFd` pipes, `PR_SET_PDEATHSIG` (Linux), and
  SIGTERM/SIGINT run a clean shutdown. `stop` honours `close_timeout` and never reports
  success without reaping.
- **Launch stalled for the full timeout** if the browser printed a non-UTF-8 byte before its
  readiness banner. Output is now decoded lossily and drained for the browser's lifetime.
- **macOS build.** `pipe2` does not exist on Apple targets; the crate failed to compile there.
- **Daemon serialized everything.** One `launch` blocked every other client for up to 180s.
  The daemon now uses per-instance locks and starts browsers without holding any lock. The
  client's read timeout follows the request's own timeout (it was a fixed 120s).
- **`cargo test` did not compile without `--features cli`.**
- `cookies` printed its count on stdout (now stderr). `screenshot -o <relative>` wrote
  relative to the daemon's cwd. `LaunchConfig::validate` was never called, so a missing
  profile panicked. `--juggler-*` args with `--` prefixes slipped past validation.
  `obs::protocol_log` panicked on multi-byte UTF-8 at the truncation point.
- **Downloads were reported as successful navigations on Camoufox 158.** The browser acks
  `Page.navigate` before `Browser.downloadCreated` arrives, so `navigate` returned `Ok(200)`.
  It now watches for the download event itself, including during `--wait-until`, and returns
  `NavigationBecameDownload`.
- **`navigate` intermittently reported a redirect's 3xx instead of the final status.** A local
  302 -> 200 chain takes ~500ms on Camoufox 158, which straddled the fixed 500ms status cap
  (the redirect integration test failed ~50% of runs). `navigate` now waits up to 2s while a
  redirect hop is in flight, tolerates out-of-order events, and reports `None` rather than a
  stale 3xx if the chain never completes.
- `Browser::new_context` silently used the default context if the browser returned no
  `browserContextId`. The codec buffered without limit; frames are now capped at 256 MiB.

- **`press Enter` never submitted forms.** Juggler's `Page.dispatchKeyEvent` routes a keydown
  through `commitCompositionWith(text, …)` whenever `text` is present *and differs from*
  `key` — an IME composition commit, which the page observes as `event.key === "Process"`.
  Sending `Enter` with `text: "\r"` therefore delivered a composition, not a keypress. The CLI
  now omits `text` entirely and lets Gecko derive the character, which is correct for both
  named keys and printable characters.
- **`launch` advertised `Camoufox/<version>` in the User-Agent on Camoufox 150+.** Launched
  without a fingerprint config, 152.0.4-beta.30 sends `Camoufox/152.0.4-beta.30` as the
  product token both in the HTTP `User-Agent` header and in `navigator.userAgent`, where
  135.0.1-beta.24 sent `Firefox/135.0`. On Linux, `launch` now reads the major version from the
  `application.ini` next to the browser executable and pins `navigator.userAgent` to
  `Mozilla/5.0 (X11; Linux x86_64; rv:<major>.0) Gecko/20100101 Firefox/<major>.0` via
  `CAMOU_CONFIG`, which also fixes the header. Nothing is set when a `CAMOU_CONFIG*` variable is
  already exported or the version can't be read.

### Security

- **Any request containing the text `Shutdown` stopped the daemon.** Shutdown was detected
  with a substring match on the raw request, so `type I P Shutdown` killed every browser.
  It is now matched on the parsed request.
- **Socket directory hijack.** When `XDG_RUNTIME_DIR` is unset, another local user could
  pre-create `/tmp/camoufox-<uid>`, swap in their own socket, and read `fill` values, cookies,
  and headers. The runtime directory must now be a real, owner-only directory owned by the
  current user. Linux also checks the peer uid on every connection.
- **The stale-socket cleanup deleted any file** at `--socket` and chmodded its parent. It now
  only replaces sockets. A `<socket>.lock` flock prevents two daemons racing for one path.
- **Screenshots were world-readable** at a shared `/tmp/screenshot-p1.png`. They now default
  to the private runtime directory, are named per instance and page, and use mode `0600`.
- IPC requests are capped at 16 MiB with a 30s read timeout.

### Changed

- **BREAKING:** `protocol::events::EventHandler` is `Arc<dyn Fn(&EventMessage) + Send + Sync>`
  (was `Box<dyn Fn + Send>`), and `Connection::on_event_global` returns a `HandlerId`.
- **BREAKING:** removed the unused `compat` module, `CodecError::Incomplete`, and
  `TransportError::ProcessExited`.
- **BREAKING (CLI):** removed the no-op `serve --foreground` flag. The plugin skill reads the
  CLI path from `CAMOUFOX_CLI`; `CAMOUFOX_BIN` still means the browser executable.
- `rust-version` is now 1.85. The previous 1.70 was not buildable: clap 4.5 needs 1.74 and
  tempfile's getrandom needs 1.85.
- **`screenshot` with no crop options now captures the current viewport.** It previously
  always clipped to `(0, 0, innerWidth, innerHeight)`; because Juggler's clip is in *document*
  coordinates, a scrolled page was captured from the top of the document rather than from
  what was on screen. The default clip origin is now `(window.scrollX, window.scrollY)`.

### Motivation

These three gaps (G1, G3, G4) were identified while driving a session-gated portal that
serves documents only behind cookies set by authenticated page navigations. Extracting those
cookies and replaying them host-side (G1) eliminates the need for in-browser XHR workarounds
and unblocks robust, resumable fetching. Deterministic load-event waiting (G3) and
redirect-aware status inspection (G4) provide the scaffolding needed to drive a multi-step
login → search → download flow reliably.
