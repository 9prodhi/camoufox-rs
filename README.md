# camoufox-rs

Pure Rust client for controlling Camoufox through the Firefox Juggler protocol.

This crate implements the full stack needed to automate a Camoufox browser process over the `-juggler-pipe` transport: process launch, null-delimited JSON framing, protocol request/response/event routing, and ergonomic `Browser` / `BrowserContext` / `Page` wrappers.

## Current Scope

- Library crate with a synchronous API for Juggler domains (`Browser`, `Page`, `Network`, `Runtime`, `Heap`)
- Optional CLI (`--features cli`) with a Unix socket daemon for multi-instance management
- Unix-first implementation (Linux/macOS style process + fd pipe model)
- Protocol reference docs in-repo:
  - `docs/PROTOCOL.md`
  - `docs/UNDERSTANDING.md`

## Requirements

- Rust 1.70+ (see `Cargo.toml`)
- Unix-like OS for full functionality (process spawning + Unix sockets)
- Camoufox binary available on disk

By default, the CLI daemon launches:

- `/root/.cache/camoufox/camoufox`

You can override that per launch with `--executable`.

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

### Important: Page Session Wiring

`BrowserContext::new_page()` returns a `Page` handle with no attached session yet. You must wire:

- `Browser.attachedToTarget` -> get `sessionId`
- `Connection::create_session(session_id)` -> `page.set_session(...)`
- `Page.frameAttached` -> `page.set_main_frame_id(...)`
- `Runtime.executionContextCreated/Destroyed` tracking for stable `Runtime.evaluate`

See working end-to-end wiring in:

- `tests/integration.rs`
- `src/cli/instance.rs`

## Architecture

Core layers (top to bottom):

- `api/`: high-level `Browser`, `BrowserContext`, `Page`
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

Current integration tests assume binary path:

- `/root/.cache/camoufox/camoufox`

## Observability

Use `log` + `env_logger` filters to inspect protocol behavior:

```bash
RUST_LOG=camoufox=trace cargo test
```

`obs::ProtocolLogger` formats command, response, and event traces with bounded payload previews.

## Known Limitations

- Windows pipe transport is not implemented (`src/transport/pipe/windows.rs` hard errors at compile time)
- API is synchronous/blocking today (no async runtime integration)
- Library users currently handle page session and execution-context wiring manually
- CLI daemon uses in-memory instance state only

## License

MIT (see crate metadata in `Cargo.toml`).
