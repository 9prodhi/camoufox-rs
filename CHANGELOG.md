# Changelog

All notable changes to camoufox-rs are documented here.

## [Unreleased]

### Added

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

### Fixed

- **`press Enter` never submitted forms.** Juggler's `Page.dispatchKeyEvent` routes a keydown
  through `commitCompositionWith(text, …)` whenever `text` is present *and differs from*
  `key` — an IME composition commit, which the page observes as `event.key === "Process"`.
  Sending `Enter` with `text: "\r"` therefore delivered a composition, not a keypress. The CLI
  now omits `text` entirely and lets Gecko derive the character, which is correct for both
  named keys and printable characters.

### Changed

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
