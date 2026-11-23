# camoufox-rs

Pure Rust client for controlling Camoufox through the Firefox Juggler protocol.

This crate implements the full stack needed to automate a Camoufox browser process over the `-juggler-pipe` transport: process launch, null-delimited JSON framing, protocol request/response/event routing, and ergonomic `Browser` / `BrowserContext` / `MainFrame` wrappers.

## Current Scope

- Library crate with a synchronous API for Juggler domains (`Browser`, `MainFrame`, `Network`, `Runtime`, `Heap`)
- Optional CLI (`--features cli`) with a Unix socket daemon for multi-instance management, file uploads, persistent profiles (`launch --profile-dir`), external subcommand plugins (`camoufox-<name>` on `PATH`), and structured error `kind` classifications
- Page HTTP cassette recording and deterministic offline replay with `record start/stop` and `replay start/stop`
- Unix implementation (process + fd pipe model): Linux and macOS. Windows is not supported.
## Documentation

The project's documentation follows the [Diataxis](https://diataxis.fr/) framework:

| Category | Document | Description |
|---|---|---|
| **Tutorial** | [Getting Started](docs/tutorial-getting-started.md) | Hands-on step-by-step introduction from installation to scraping |
| **How-To** | [CLI Daemon & Management](docs/howto-cli-daemon.md) | Managing the daemon, multiple browser instances, and scraping flows |
| **How-To** | [Iframes and Shadow DOM](docs/howto-iframes-and-shadow-dom.md) | Navigating nested frames and piercing shadow DOM with `>>>` |
| **How-To** | [Downloads and Dialogs](docs/howto-downloads-and-dialogs.md) | Safe file downloads, the click-then-poll download pattern, and handling JS dialogs |
| **How-To** | [Library Usage Guide](docs/howto-library-usage.md) | Using `camoufox-rs` as a Rust library crate in your application |
| **Reference** | [CLI Reference](docs/reference-cli.md) | Exhaustive command reference for CLI subcommands |
| **Reference** | [Library API Reference](docs/reference-library-api.md) | Complete crate and module API reference |
| **Reference** | [Juggler Protocol Reference](docs/PROTOCOL.md) | Wire-level Juggler protocol specification |
| **Explanation** | [Architecture & Design](docs/explanation-architecture.md) | Pipe transport, security model, coordinate spaces, and process lifecycle |
| **Explanation** | [Protocol & Transport Internals](docs/UNDERSTANDING.md) | Deep-dive on Juggler framing, pipe IPC, and session multiplexing |

## Requirements

- Rust 1.85+ (`rust-version` in `Cargo.toml`)
- Unix-like OS for full functionality (process spawning + Unix sockets)
- Camoufox binary available on disk

The CLI daemon resolves the Camoufox binary in this order:

1. `--executable <path>` passed to `launch`
2. the `CAMOUFOX_BIN` environment variable
3. the install marked active in `~/.cache/camoufox/config.json` (`active_version`), as set by the Python `camoufox fetch` / `camoufox set` commands
4. the newest `~/.cache/camoufox/browsers/<repo>/<version>/camoufox`
5. `~/.cache/camoufox/camoufox`, the legacy flat layout

`~/.cache` means `$XDG_CACHE_HOME` when it is set to an absolute path, else
`$HOME/.cache` (`/root/.cache` when `HOME` is unset). Library users get the
same lookup from `camoufox::config::default_executable()`. On macOS, set
`CAMOUFOX_BIN` or pass `--executable`.

## Build

Library only:

```bash
cargo build
```

CLI binary:

```bash
cargo build --features cli --bin camoufox
```

## CLI Quick Start

The CLI uses a daemon process and newline-delimited JSON over a Unix domain socket.

Built-in commands: `serve`, `launch`, `dialog`, `list`, `stop`, `new-page`, `navigate`,
`evaluate`, `click`, `screenshot`, `shutdown`, `ping`, `cookies`, `text`, `html`, `links`,
`data`, `url`, `back`, `forward`, `reload`, `wait`, `cookie`, `header`, `fill`, `upload`,
`type`, `press`, `hover`, `select`, `scroll`, `tabs`, `close-tab`, `downloads`, `record`,
`replay`.

External subcommands: executing `camoufox <name>` for any command not built-in searches for
`camoufox-<name>` in absolute `PATH` entries. Arguments, standard streams, and exit status
pass through. Put global flags before `<name>`; `CAMOUFOX_SOCKET` receives the absolute
socket path, and `CAMOUFOX_JSON` is set to `1` with `--json` or cleared otherwise.

Start daemon (run in a dedicated shell):

```bash
cargo run --features cli --bin camoufox -- serve
```

Launch an instance (`--profile-dir` specifies an optional persistent profile directory):

```bash
cargo run --features cli --bin camoufox -- launch
cargo run --features cli --bin camoufox -- launch --profile-dir /path/to/profile
```

For persistent browsing data, add `--profile-dir /absolute/path/to/profile` to
`launch`. The directory must be empty or contain `prefs.js` or `times.json`.
It is created with mode 0700 or has group/other permissions removed,
canonicalized, and kept after stop. The daemon confirms the canonical path in
launch success data and shows it in `list`. Concurrent launches using the same
canonical profile are refused within one daemon process. Do not point two daemons
at the same profile directory: Firefox does not support concurrent use and it can
corrupt the profile. Restart older daemons that
cannot confirm persistent profile support. The client stops the instance returned
by an older daemon before reporting the compatibility error; if cleanup fails,
the error includes the instance ID so you can stop it manually.

Create a page:

```bash
cargo run --features cli --bin camoufox -- new-page <instance_id>
```

Navigate and evaluate:

```bash
cargo run --features cli --bin camoufox -- navigate <instance_id> <page_id> https://example.com
cargo run --features cli --bin camoufox -- evaluate <instance_id> <page_id> "document.title"
```

`evaluate` runs in an isolated JavaScript world (Camoufox 158+): the DOM is shared with the
page, but globals defined by page scripts (`window.__NEXT_DATA__`, app state) are not visible.
To read them, add `--main-world`:

```bash
cargo run --features cli --bin camoufox -- evaluate --main-world <instance_id> <page_id> "window.__NEXT_DATA__.page"
```

`--main-world` injects a `<script>` into the page, so it accepts synchronous expressions only,
the page can observe it, and a strict Content-Security-Policy blocks it (reporting a hint to relaunch with `--bypass-csp`).
For pages with strict CSP that prohibit inline scripts without nonces, launch the browser instance with `--bypass-csp`:

```bash
cargo run --features cli --bin camoufox -- launch --bypass-csp
```

Note: `--bypass-csp` disables Content-Security-Policy at the browser context level and is detectable by pages. For nonce-protected pages, bridge v2 automatically borrows an existing script's nonce attribute from the DOM, allowing main-world evaluation to succeed without `--bypass-csp`.

Navigate with wait-until and status_code:

```bash
# Block until the load event fires (bounded by --timeout seconds):
cargo run --features cli --bin camoufox -- navigate <instance_id> <page_id> https://example.com \
    --wait-until load --timeout 30

# Or wait only until DOMContentLoaded:
cargo run --features cli --bin camoufox -- navigate <instance_id> <page_id> https://example.com \
    --wait-until domcontentloaded --timeout 15

# --wait-until accepts: load, domcontentloaded. Any other value is an error.
# navigate always succeeds even on 4xx/5xx responses.
# --json output includes status_code (final main-document HTTP status after following
# all redirects; null if uncapturable, e.g. about: pages or navigation errors):
#   { "ok": true, "data": { "navigation_id": "...", "status_code": 200 } }
```

Export the session cookie jar:

```bash
# Export all cookies for the specified instance (includes HttpOnly cookies):
cargo run --features cli --bin camoufox -- cookies <instance_id>

# --json returns full cookie objects (name, value, domain, path, httpOnly, secure, ...):
cargo run --features cli --bin camoufox -- --json cookies <instance_id>

# The exported jar can drive host-side fetches without in-session XHR:
#   curl --cookie "name=value" https://example.com/gated-endpoint
```

Set a cookie or an extra request header:

```bash
# Bind the cookie to the page's current URL (or pass --url / --domain explicitly):
cargo run --features cli --bin camoufox -- cookie <instance_id> <page_id> 'session=abc123'
cargo run --features cli --bin camoufox -- cookie <instance_id> <page_id> 'tracker=xyz' \
    --domain example.com --path / --secure

# Extra request headers accumulate per page across calls:
cargo run --features cli --bin camoufox -- header <instance_id> <page_id> 'Accept-Language: fr-FR'
```

Read the page (no hand-written extraction JS required):

```bash
# Rendered text, whole page or scoped to a selector:
cargo run --features cli --bin camoufox -- text <instance_id> <page_id>
cargo run --features cli --bin camoufox -- text <instance_id> <page_id> --selector 'article'

# outerHTML of one element, or the whole document when --selector is omitted:
cargo run --features cli --bin camoufox -- html <instance_id> <page_id> --selector h1

# Every <a href> as `text → href`, with hrefs resolved to absolute URLs:
cargo run --features cli --bin camoufox -- links <instance_id> <page_id>

# Structured metadata; no flag returns og + jsonld + meta together:
cargo run --features cli --bin camoufox -- data <instance_id> <page_id> --og

# Current URL (stdout) and title (stderr):
cargo run --features cli --bin camoufox -- url <instance_id> <page_id>
```

Wait for content to appear:

```bash
# Polls document.querySelector until it matches, or --timeout seconds elapse:
cargo run --features cli --bin camoufox -- wait <instance_id> <page_id> \
    --selector '#results' --timeout 15
```

Interact with the page (`click`, `hover`, `press`, `type`, and the typing phase of `fill` dispatch trusted browser-level input events; `select`, `scroll`, and `fill`'s clear step use JavaScript):

```bash
# Click by CSS selector: resolves the element, scrolls it into view, clicks its centre:
cargo run --features cli --bin camoufox -- click <instance_id> <page_id> '#submit'

# Or dispatch a trusted left-click at raw viewport coordinates (x, y). Because the event
# originates from the browser (not JavaScript), the page sees isTrusted === true, so it
# can drive widgets like Cloudflare Turnstile that reject synthetic click events:
cargo run --features cli --bin camoufox -- click <instance_id> <page_id> 200 300

# Fill a field, then submit with a real Enter keypress:
cargo run --features cli --bin camoufox -- fill <instance_id> <page_id> 'input[name=q]' camoufox
cargo run --features cli --bin camoufox -- press <instance_id> <page_id> Enter

# Set a file input's files (also works with hidden inputs):
cargo run --features cli --bin camoufox -- upload <instance_id> <page_id> 'input[type=file]' ./fixture.pdf

# Also available: type <text>, hover <selector>, select <selector> <value>, scroll [selector]
```

Upload replaces the current selection with one or more regular files. Relative
paths resolve against the client's working directory and are validated before
page access. Multiple files require the input's `multiple` attribute. Plain output
lists basenames; JSON returns `uploaded`, `selector`, and `files`.
Upload rejects directory-picker (`webkitdirectory`) inputs with kind
`invalid_argument`. If an `input` or `change` handler opens a JavaScript dialog,
successful JSON upload data may include `"dialog": {...}`; resolve it with
`camoufox dialog <instance_id> <page_id> accept` or `dismiss`.

### Iframe and Shadow-DOM Scoping

After upgrading, restart any running daemon: `camoufox shutdown`, then
`camoufox serve`. An older daemon ignores `--frame`, even when the client is new.

Scope commands to iframes with `--frame <selector>` (repeatable for nested iframes):

```bash
# Scope text extraction to an iframe:
cargo run --features cli --bin camoufox -- text <instance_id> <page_id> --frame '#ad-frame'

# Scope click to a nested iframe (accumulating frame coordinate offsets):
cargo run --features cli --bin camoufox -- click <instance_id> <page_id> '#btn' \
    --frame '#outer-frame' --frame '#inner-frame'

# Supported across 12 commands: text, html, links, click, fill, upload, type, press, hover, select, wait, evaluate.
```

Clicks and hovers scroll ancestor frames and the target into view before measuring
coordinates. Axis-aligned positive CSS scaling, borders, and padding are supported,
including nested frames; rotated or skewed iframes are unsupported.

`wait` follows late-created/replaced iframes and late-attached open shadow roots.
Closed roots cannot be pierced in Camoufox's isolated world; an absent root cannot
reliably be distinguished from a closed one. Other commands report that the host
has no open shadow root. `type` and `press` also accept `--timeout` (seconds),
including the client socket budget. With `--frame`, they focus each ancestor
iframe, then the target window, and dispatch input only if a field is **currently
focused** there. If the active element is absent, `body`, or `documentElement`,
they fail with `--frame <sel>: nothing is focused inside the frame; click or fill
an element inside it first`.

There is no remembered focus target and no focus-listener or blur-hook
instrumentation. After interacting with the top-level page,
**click or fill inside the iframe again** before using `type --frame` or
`press --frame`. Give the **full** `--frame` chain down to the frame that holds
the field (for a field in a nested iframe, pass both the outer and the inner
`--frame`): the chain is focused before the check, so a partial chain fails and
moves focus out of the inner frame. Plain `type` and `press` still type into
whatever is focused.

Pierce open shadow-DOM boundaries using the ` >>> ` combinator in any selector.
Operators inside CSS quotes, brackets, parentheses, or escapes remain ordinary CSS.
All matching hosts are searched depth-first in document order:

```bash
# Click a button inside an open shadow root:
cargo run --features cli --bin camoufox -- click <instance_id> <page_id> 'custom-element >>> button.action'

# Deeply nested shadow roots:
cargo run --features cli --bin camoufox -- text <instance_id> <page_id> --selector 'host-a >>> host-b >>> span.label'
```

Take a screenshot:

```bash
# Current viewport:
cargo run --features cli --bin camoufox -- screenshot <instance_id> <page_id> --format png -o /tmp/example.png

# Cropped to one element (scrolled into view first), or to an explicit region:
cargo run --features cli --bin camoufox -- screenshot <instance_id> <page_id> --selector 'article' -o /tmp/article.png
cargo run --features cli --bin camoufox -- screenshot <instance_id> <page_id> --clip 0,0,800,600 -o /tmp/region.png
```

Manage pages as tabs:

```bash
cargo run --features cli --bin camoufox -- tabs <instance_id>
cargo run --features cli --bin camoufox -- close-tab <instance_id> <page_id>
```

Tabs and popups opened by the site (e.g. via `<a target="_blank">` or `window.open()`) are automatically tracked and appear in `tabs` on subsequent commands, complete with an `"opener"` field pointing to the parent page. Closed pages are removed from `tabs`, and most commands targeting them report that the page was closed. `record stop` and `replay stop` can retrieve retained results with `repeated: true` and `page_closed: true` for up to the last 64 closed pages per instance; retained failures remain errors.

Manage downloads:

```bash
# Launch with an absolute download directory (defaults to cancel if omitted):
cargo run --features cli --bin camoufox -- launch --downloads-dir /path/to/downloads

# Click a download link, then wait/poll for download lifecycle events:
cargo run --features cli --bin camoufox -- click <instance_id> <page_id> '#download-link'
cargo run --features cli --bin camoufox -- downloads <instance_id> --wait 10
# Note: --wait wakes on the next event advance; poll until state is completed, failed, or canceled.
# Click responses also automatically carry a "downloads" list if triggered during the click.

# Inspect the download log for an instance:
cargo run --features cli --bin camoufox -- downloads <instance_id>
```

Inspect and stop:

```bash
cargo run --features cli --bin camoufox -- list
cargo run --features cli --bin camoufox -- stop <instance_id>
cargo run --features cli --bin camoufox -- shutdown
```

### Network cassettes

Record before navigating, then stop to publish the cassette:

```bash
camoufox record start <instance_id> <page_id> --dir /absolute/cassette
camoufox navigate <instance_id> <page_id> https://example.com --wait-until load
camoufox record stop <instance_id> <page_id>
camoufox replay start <instance_id> <page_id> --dir /absolute/cassette
camoufox navigate <instance_id> <page_id> https://example.com --wait-until load
camoufox replay stop <instance_id> <page_id>
```

Recording requires an absent or empty absolute directory with owner-only access.
Repeat `--url <regex>` on `record start` to filter URLs. Replay matches method and
exact URL, optionally request-body hashes with `--match-body`; misses are aborted
and reported at stop. Repeated matches consume responses in order, then reuse the
last response. Replay rejects redirect cassettes. Only one recording or replay
can run per page, including its subframes; popups need separate sessions and
service-worker responses are outside replay interception.

Cassettes store `index.json` and decoded bodies under `bodies/<sha256>`.
Credential headers are scrubbed, but URLs and bodies may contain secrets. Use
trusted cassettes because replayed HTML and scripts use the origin's real storage.
See [record and replay](docs/reference-cli.md#record) for format and cleanup details.

Built-in client commands support JSON output (`--json`). On failure, JSON error responses include a `kind` field identifying the error category (e.g. `selector_not_found`, `timeout`, `dialog_open`, `invalid_argument`, `page_not_found`, `instance_not_found`, `io`, `daemon`, etc.):

```bash
cargo run --features cli --bin camoufox -- --json list
```

Client-side argument validation exits 2 with kind `invalid_argument`; local I/O
failures, such as resolving an upload or screenshot path, exit 1 with kind `io`.
Other execution and daemon errors exit 1; success exits 0. Argument parsing
errors use clap's text diagnostics and exit 2, even with `--json`. Success responses
omit `kind`; the client backfills missing kinds from older daemon messages and
preserves unknown kinds, which consumers should treat as `other`. External
subcommands control their own output and exit status.

Socket resolution:

- `--socket <path>` to override
- else `CAMOUFOX_SOCKET` when set
- else `$XDG_RUNTIME_DIR/camoufox/daemon.sock`
- else `/tmp/camoufox-<uid>/daemon.sock`

Daemon security and lifecycle:

- The default runtime directory is created `0700`. The daemon and the client both refuse to
  use it unless it is a real directory (not a symlink), owned by the current user, with no
  group/other permissions. On Linux the daemon also rejects connections from other uids
  (`SO_PEERCRED`). An explicit `--socket` is used as given, and the daemon never chmods its
  parent.
- The daemon only ever replaces a stale *socket* at the path, never another kind of file.
  It holds an exclusive lock on `<socket>.lock` for its lifetime, so a second `serve`
  on the same path fails with `daemon is already running`.
- `shutdown`, SIGTERM and SIGINT stop every browser, wait for in-flight requests, and
  remove the socket. On Linux, browsers are spawned with `PR_SET_PDEATHSIG`, so they die
  with the daemon even on SIGKILL (temporary profiles may remain in that case).
- Screenshots without `-o` are written to the runtime directory as
  `screenshot-<instance>-<page>.<ext>` with mode `0600`. A relative `-o` path is resolved
  against the client's working directory.
- Requests for different instances run in parallel. Most commands on the same instance
  are serialized; download waits and cassette validation, disk I/O, and draining run
  outside the instance lock. `launch` holds no lock while the browser starts.

## JavaScript dialogs

Dialogs use manual handling by default. When a page opens an alert, confirm,
prompt, or beforeunload dialog, respond with `camoufox dialog <instance> <page> accept`
or `camoufox dialog <instance> <page> dismiss`. For a prompt, supply the answer
with `--text`. Launch with `--dialogs accept` or `--dialogs dismiss` to handle
dialogs automatically; `--dialogs manual` restores the default.

## Library Bootstrap Example

The low-level lifecycle is:

1. Build `LaunchConfig`
2. Spawn process (`process::unix::spawn`)
3. Wait readiness sentinel on stdout or stderr
4. Build `PipeTransport`
5. Build `Connection` + root session
6. `Browser::connect(...)`

```rust,no_run
use std::path::PathBuf;

use camoufox::api::{Browser, BrowserOptions, ContextOptions};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile_dir = std::env::temp_dir().join("camoufox-rs-profile");
    std::fs::create_dir_all(&profile_dir)?;

    let config = LaunchConfig {
        executable: PathBuf::from("/root/.cache/camoufox/camoufox"),
        profile_dir: Some(profile_dir),
        headless: true,
        ..Default::default()
    };

    let mut launched = process::unix::spawn(&config)?;
    process::readiness::wait_for_ready(&mut launched.child, config.timeout)?;

    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let root = conn.root_session();

    let browser = Browser::connect(conn, root, BrowserOptions::default())?;
    let _context = browser.new_context(ContextOptions::default())?;

    browser.close()?;
    Ok(())
}
```

### Pages via `MainFrame`

For persistent storage, connect with `BrowserOptions::attach_to_default_context`
set to `true`, then call `browser.default_context(ContextOptions::default())`
instead of `new_context`. The default context uses the on-disk `LaunchConfig::profile_dir`
and has an empty context ID. Its `close()` only drops the handle; call
`Browser::close()` to close pages and flush storage. Keep the profile directory
between launches. The CLI's `launch --profile-dir` handles this setup automatically.

`BrowserContext::new_main_frame()` returns a fully wired `MainFrame`: a page
handle that is **structurally pinned to the top frame**. The call blocks until
the page target, top frame, and main-world execution context are all resolved
from authoritative protocol responses, so there is no manual session or
execution-context wiring to do.

This is the fix for the cross-origin-iframe attach bug: on sites that embed an
early out-of-process iframe (e.g. an ad pixel), the old handle could bind to the
iframe instead of the page: `evaluate` then ran in the wrong document.
`new_main_frame()` applies three filters so it can only ever resolve to the real
top frame:

- **Layer 1**: accept only `Browser.attachedToTarget` events where `targetInfo.type == "page"`
- **Layer 2**: accept only the top frame's `Page.frameAttached` (empty `parentFrameId`)
- **Layer 3**: accept only the main-world `Runtime.executionContextCreated` whose `auxData.frameId` matches the top frame

```rust,no_run
# fn doc_test(browser: &camoufox::api::Browser) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::ContextOptions;
use std::time::Duration;

// ...continuing from the bootstrap example, after `Browser::connect`:
let context = browser.new_context(ContextOptions::default())?;

// One call: no manual attachedToTarget / frameAttached / executionContext wiring:
let main_frame = context.new_main_frame()?;

// navigate(url, NavigateOptions, timeout) -> NavigateOutcome (nav_id and status_code)
let outcome: camoufox::api::main_frame::NavigateOutcome = main_frame.navigate("https://example.com", Default::default(), Duration::from_secs(30))?;
println!("status: {:?}", outcome.status_code);

// evaluate(expr, timeout) runs in the isolated world and returns the raw
// Runtime.evaluate envelope: read result.value and check exceptionDetails.
let raw: serde_json::Value = main_frame.evaluate("document.title", Duration::from_secs(15))?;
let title = raw["result"]["value"].as_str().unwrap_or_default();
println!("title: {title}");

// evaluate_main_world runs a synchronous expression in the page's own world
// and returns the plain JSON value (no envelope)
let app_state: serde_json::Value = main_frame.evaluate_main_world("window.location.href", Duration::from_secs(15))?;
println!("state: {app_state}");
# Ok(())
# }
```

A complete, runnable version is in [`examples/web_browse.rs`](examples/web_browse.rs):

```bash
cargo run --example web_browse -- --url https://example.com
```

End-to-end wiring is also exercised in `tests/integration.rs` and `src/cli/instance.rs`.

For file inputs, `MainFrame::set_file_input_files(frame_id, object_id, files)`
uses a 60-second timeout; `set_file_input_files_with_timeout` accepts an explicit
`Duration`. Both take a file-input object handle and dispatch `input` and `change`.
The CLI's `upload` adds CSS selection, frame/shadow scoping, and path validation.

### Reliability

- Every protocol request is bounded: `Session::send` enforces a default deadline
  (60s), and commands like `navigate` / `evaluate` / `wait` accept an explicit
  `timeout` (CLI: `--timeout <seconds>`), so a stuck call can no longer hang the
  daemon.
- Navigations that the browser diverts into a download (e.g. a
  `Content-Disposition: attachment` URL) are detected via `Browser.downloadCreated`
  and surfaced promptly as a `NavigationBecameDownload` error instead of blocking
  forever waiting for a navigation response that never arrives. Every context
  created by `Browser::new_context` cancels downloads unless
  `ContextOptions::download_options` says otherwise, so nothing is written to disk.
- Event handlers run outside the connection lock and inside `catch_unwind`: a
  panicking handler is logged instead of killing the connection, and a handler may
  call back into the connection. An undecodable frame from the browser is logged and
  skipped.
- When a page closes (`Browser.detachedFromTarget`) or crashes (`Page.crashed`),
  in-flight calls on it fail immediately with `Closed` / `Crashed`. If the browser
  goes away entirely, every pending call and event waiter is released.
- `MainFrame` removes its event listeners when dropped, so opening and closing tabs in
  a long-running process does not accumulate handlers.
- A failed launch (readiness timeout, bootstrap error) kills and reaps the browser.

## Architecture

Core layers (top to bottom):

- `api/`: high-level `Browser`, `BrowserContext`, `MainFrame`
- `protocol/`: request IDs, pending map, session state, event router, reader thread
- `transport/`: transport traits + Unix pipe transport
- `codec/`: null-byte-delimited JSON framing (`NulJsonCodec`)
- `process/`: spawn/readiness/lifecycle around Camoufox child process
- `cli/` (feature-gated): daemon + command dispatch over Unix socket
- `obs/`: protocol logging helpers

## Testing

Unit tests (`--all-features` is needed to build and test the CLI; plain
`cargo test` covers only the library):

```bash
cargo test --all-features
```

Integration tests against a real Camoufox binary are ignored by default:

```bash
cargo test --all-features --test integration -- --ignored --test-threads=1
```

Integration tests resolve the binary the same way as the daemon (see
[Requirements](#requirements)), so a `camoufox fetch` install is found
automatically. Override per run with:

```bash
CAMOUFOX_BIN="/path/to/camoufox" cargo test --all-features --test integration -- --ignored --test-threads=1
```

## Observability

Use `log` + `env_logger` filters to inspect protocol behavior:

```bash
RUST_LOG=camoufox::protocol=trace cargo test
```

Protocol traffic is traced at `camoufox::protocol=trace` with bounded payload previews.

## Known Limitations

- Windows pipe transport is not implemented (`src/transport/pipe/windows.rs` hard errors at compile time)
- API is synchronous/blocking today (no async runtime integration)
- Sub-frames are reached through `MainFrame` (`frames()`, `has_frame()`, `resolve_child_frame()`, `evaluate_in_frame()`)
  and the CLI's `--frame`; there is no separate frame object type
- Closed shadow roots cannot be pierced (` >>> ` reports them); rotated or skewed iframes are
  not supported for pointer coordinates (translation and scale are)
- `type --frame` / `press --frame` only type into a field that is currently focused inside the
  frame; after touching the top-level page, click or fill inside the frame again
- Instance/page IDs do not survive daemon restart; persistent profiles and cassettes remain on disk
- Most requests to the *same* instance are serialized: a long-polling command (e.g.
  `wait --timeout 60`) on one page blocks other commands for that instance until it returns.
  Other instances are unaffected. Download waits and cassette validation, disk I/O,
  and draining release the instance lock.
- `close-tab`, `stop`, and `shutdown` finalize active cassettes. Tab cleanup has a
  15-second budget after acquiring its network-operation lock; stop and shutdown
  share 15 seconds across an instance's pages. `close-tab` and `stop` report
  finalization failures in success data as `cassette_errors` and warn on stderr
  without changing exit code 0. Shutdown logs failures and replies after cleanup
  and socket removal. Explicit cassette stop commands allow 300 seconds to drain.
- `back` / `forward` are wired to `Page.goBack` / `Page.goForward` but are inert against the
  Camoufox builds tested here: pages created via `Browser.newPage` expose no session history
  (`history.length === 0`), so both always report "no history entry". Re-`navigate` instead.

## License

MIT (see crate metadata in `Cargo.toml`).
