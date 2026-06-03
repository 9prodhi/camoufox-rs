# camoufox-rs

Pure Rust client for controlling Camoufox through the Firefox Juggler protocol.

This crate implements the full stack needed to automate a Camoufox browser process over the `-juggler-pipe` transport: process launch, null-delimited JSON framing, protocol request/response/event routing, and ergonomic `Browser` / `BrowserContext` / `MainFrame` wrappers.

## Current Scope

- Library crate with a synchronous API for Juggler domains (`Browser`, `MainFrame`, `Network`, `Runtime`, `Heap`)
- Optional CLI (`--features cli`) with a Unix socket daemon for multi-instance management
- Unix-first implementation (Linux/macOS style process + fd pipe model)
- Protocol reference docs in-repo:
  - `docs/PROTOCOL.md`
  - `docs/UNDERSTANDING.md`

## Requirements

- Rust 1.70+ (see `Cargo.toml`)
- Unix-like OS for full functionality (process spawning + Unix sockets)
- Camoufox binary available on disk

The CLI daemon resolves the Camoufox binary in this order:

1. `--executable <path>` passed to `launch`
2. the `CAMOUFOX_BIN` environment variable
3. `$HOME/.cache/camoufox/camoufox` (falling back to `/root/.cache/camoufox/camoufox` when `HOME` is unset)

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

Start daemon (run in a dedicated shell):

```bash
cargo run --features cli --bin camoufox -- serve --foreground
```

Launch an instance:

```bash
cargo run --features cli --bin camoufox -- launch
```

Create a page:

```bash
cargo run --features cli --bin camoufox -- new-page <instance_id>
```

Navigate and evaluate:

```bash
cargo run --features cli --bin camoufox -- navigate <instance_id> <page_id> https://example.com
cargo run --features cli --bin camoufox -- evaluate <instance_id> <page_id> "document.title"
```

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
# Export all cookies for all instances (includes HttpOnly cookies):
cargo run --features cli --bin camoufox -- cookies <instance_id>

# --json returns full cookie objects (name, value, domain, path, httpOnly, secure, ...):
cargo run --features cli --bin camoufox -- --json cookies <instance_id>

# The exported jar can drive host-side fetches without in-session XHR:
#   curl --cookie "name=value" https://example.com/gated-endpoint
```

Take a screenshot:

```bash
cargo run --features cli --bin camoufox -- screenshot <instance_id> <page_id> --format png -o /tmp/example.png
```

Inspect and stop:

```bash
cargo run --features cli --bin camoufox -- list
cargo run --features cli --bin camoufox -- stop <instance_id>
cargo run --features cli --bin camoufox -- shutdown
```

JSON output mode is available for all commands:

```bash
cargo run --features cli --bin camoufox -- --json list
```

Socket resolution:

- `--socket <path>` to override
- else `$XDG_RUNTIME_DIR/camoufox/daemon.sock`
- else `/tmp/camoufox-<uid>/daemon.sock`

## Library Bootstrap Example

The low-level lifecycle is:

1. Build `LaunchConfig`
2. Spawn process (`process::unix::spawn`)
3. Wait readiness sentinel on stderr
4. Build `PipeTransport`
5. Build `Connection` + root session
6. `Browser::connect(...)`

```rust
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

`BrowserContext::new_main_frame()` returns a fully wired `MainFrame` — a page
handle that is **structurally pinned to the top frame**. The call blocks until
the page target, top frame, and main-world execution context are all resolved
from authoritative protocol responses, so there is no manual session or
execution-context wiring to do.

This is the fix for the cross-origin-iframe attach bug: on sites that embed an
early out-of-process iframe (e.g. an ad pixel), the old handle could bind to the
iframe instead of the page — `evaluate` then ran in the wrong document.
`new_main_frame()` applies three filters so it can only ever resolve to the real
top frame:

- **Layer 1** — accept only `Browser.attachedToTarget` events where `targetInfo.type == "page"`
- **Layer 2** — accept only the top frame's `Page.frameAttached` (empty `parentFrameId`)
- **Layer 3** — accept only the main-world `Runtime.executionContextCreated` whose `auxData.frameId` matches the top frame

```rust
use std::time::Duration;

// ...continuing from the bootstrap example, after `Browser::connect`:
let context = browser.new_context(ContextOptions::default())?;

// One call — no manual attachedToTarget / frameAttached / executionContext wiring:
let main_frame = context.new_main_frame()?;

// navigate(url, NavigateOptions, timeout) -> NavigateOutcome { nav_id, status_code }.
// status_code is the final main-document HTTP status after following redirects.
let outcome = main_frame.navigate("https://example.com", Default::default(), Duration::from_secs(30))?;
println!("status: {:?}", outcome.status_code);

// evaluate(expr, timeout) — the cached execution context is maintained internally,
// so no execution-context id is threaded through.
let title = main_frame.evaluate("document.title", Duration::from_secs(15))?;
println!("title: {title}");
```

A complete, runnable version is in [`examples/web_browse.rs`](examples/web_browse.rs):

```bash
cargo run --example web_browse -- --url https://example.com
```

End-to-end wiring is also exercised in `tests/integration.rs` and `src/cli/instance.rs`.

### Reliability

- Every protocol request is bounded — `Client::send` enforces a default 60s
  deadline, and `navigate` / `evaluate` / `screenshot` accept an explicit
  `timeout` (CLI: `--timeout <seconds>`), so a stuck call can no longer hang the
  daemon.
- Navigations that the browser diverts into a download (e.g. a
  `Content-Disposition: attachment` URL) are detected via `Browser.downloadCreated`
  and surfaced promptly as a `NavigationBecameDownload` error instead of blocking
  forever waiting for a navigation response that never arrives.

## Architecture

Core layers (top to bottom):

- `api/`: high-level `Browser`, `BrowserContext`, `MainFrame`
- `protocol/`: request IDs, pending map, session state, event router, reader thread
- `transport/`: transport traits + Unix pipe transport
- `codec/`: null-byte-delimited JSON framing (`NulJsonCodec`)
- `process/`: spawn/readiness/lifecycle around Camoufox child process
- `cli/` (feature-gated): daemon + command dispatch over Unix socket
- `compat/`: Camoufox detection/version capability checks
- `obs/`: protocol logging helpers

## Testing

Unit tests:

```bash
cargo test
```

Integration tests against a real Camoufox binary are ignored by default:

```bash
cargo test --test integration -- --ignored --test-threads=1
```

Integration tests resolve the binary the same way as the daemon: `CAMOUFOX_BIN`,
else `$HOME/.cache/camoufox/camoufox`. Override per run with:

```bash
CAMOUFOX_BIN=/path/to/camoufox cargo test --test integration -- --ignored --test-threads=1
```

## Observability

Use `log` + `env_logger` filters to inspect protocol behavior:

```bash
RUST_LOG=camoufox=trace cargo test
```

`obs::ProtocolLogger` formats command, response, and event traces with bounded payload previews.

## Known Limitations

- Windows pipe transport is not implemented (`src/transport/pipe/windows.rs` hard errors at compile time)
- API is synchronous/blocking today (no async runtime integration)
- `MainFrame` is top-frame only; there is no public API for operating on sub-frames
- CLI daemon uses in-memory instance state only

## License

MIT (see crate metadata in `Cargo.toml`).
