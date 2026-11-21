# Tutorial: Getting Started with Camoufox in Rust

This tutorial takes you from zero to a working Camoufox scraper in Rust. You will launch a real browser, navigate to a live website, extract structured information, capture a screenshot of a fixed 800x600 clip, and cleanly shut down the process.

## What you will build

You will build a standalone Rust application that:
1. Spawns an anti-detect Camoufox browser over Unix pipes.
2. Connects to the Juggler protocol session.
3. Creates an isolated context and loads a web page.
4. Reads DOM text and extracts JavaScript properties.
5. Captures a screenshot to disk.
6. Cleanly terminates the browser without leaving orphaned processes.

Time to first working result: 3 steps.

## Prerequisites

- **Rust 1.85 or later** (`rustc --version`).
- **Linux or macOS** (Windows pipe transport is not supported).
- **Camoufox browser binary**:
  Placed at `$HOME/.cache/camoufox/camoufox` or pointed to by the `CAMOUFOX_BIN` environment variable.

Verify your Camoufox binary before starting:

```bash
${CAMOUFOX_BIN:-$HOME/.cache/camoufox/camoufox} --version
```

---

## Step 1: Set up your project and dependencies

Create a new binary crate and add `camoufox` to `Cargo.toml`:

```bash
cargo new camoufox-scraper
cd camoufox-scraper
```

Add dependencies to `Cargo.toml`:

```toml
[dependencies]
camoufox = { path = "../camoufox-rs" } # or git repository URL
tempfile = "3"
serde_json = "1"
```

Verify that the project compiles:

```bash
cargo check
```

---

## Step 2: Spawn and connect to Camoufox

Replace `src/main.rs` with the bootstrap skeleton:

```rust,no_run
use std::path::PathBuf;
use std::time::Duration;

use camoufox::api::{Browser, BrowserOptions, ContextOptions};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

fn resolve_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CAMOUFOX_BIN") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(format!("{home}/.cache/camoufox/camoufox"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = resolve_binary();
    if !executable.exists() {
        return Err(format!("Camoufox binary not found at {}", executable.display()).into());
    }

    // 1. Configure browser process launch
    let profile = tempfile::tempdir()?;
    let config = LaunchConfig {
        executable,
        profile_dir: Some(profile.path().to_owned()),
        headless: true,
        ..Default::default()
    };

    println!("[1/4] Spawning browser process...");
    let mut launched = process::unix::spawn(&config)?;

    // 2. Wait for readiness sentinel on stdout/stderr
    process::readiness::wait_for_ready(&mut launched.child, config.timeout)?;
    println!("[2/4] Browser is ready.");

    // 3. Initialize transport and protocol connection
    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let root_session = conn.root_session();

    // 4. Perform protocol bootstrap
    let browser = Browser::connect(conn, root_session, BrowserOptions::default())?;
    println!(
        "[3/4] Connected to Camoufox version: {}",
        browser.version().unwrap_or("unknown")
    );

    // 5. Clean close
    browser.close()?;
    let _ = launched.child.wait()?;
    println!("[4/4] Browser shut down cleanly.");

    Ok(())
}
```

Run this step:

```bash
cargo run
```

Expected output:
```text
[1/4] Spawning browser process...
[2/4] Browser is ready.
[3/4] Connected to Camoufox version: Firefox/135.0 (or similar)
[4/4] Browser shut down cleanly.
```

You now have a verified connection to Camoufox in under 3 steps.

---

## Step 3: Create an isolated context and navigate

Now let's load a web page. In Camoufox, pages belong to a `BrowserContext` (an isolated profile with its own cookies and storage). Calling `context.new_main_frame()` returns a `MainFrame` pinned to the top frame.

Update `src/main.rs`:

```rust,no_run
use std::path::PathBuf;
use std::time::Duration;

use camoufox::api::{Browser, BrowserOptions, ContextOptions, NavigateOptions, ScreenshotOptions, Rect};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

fn resolve_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CAMOUFOX_BIN") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(format!("{home}/.cache/camoufox/camoufox"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = resolve_binary();
    let profile = tempfile::tempdir()?;
    let config = LaunchConfig {
        executable,
        profile_dir: Some(profile.path().to_owned()),
        headless: true,
        ..Default::default()
    };

    let mut launched = process::unix::spawn(&config)?;
    process::readiness::wait_for_ready(&mut launched.child, config.timeout)?;

    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let root = conn.root_session();
    let browser = Browser::connect(conn, root, BrowserOptions::default())?;

    // Create an isolated context
    let context = browser.new_context(ContextOptions::default())?;

    // Create a new main frame handle
    let page = context.new_main_frame()?;

    // Navigate and wait for DOMContentLoaded
    println!("Navigating to https://example.com...");
    let nav_options = NavigateOptions {
        referer: None,
        wait_until: Some("domcontentloaded".into()),
    };
    let outcome: camoufox::api::main_frame::NavigateOutcome = page.navigate("https://example.com", nav_options, Duration::from_secs(30))?;
    println!("Loaded with HTTP status: {:?}", outcome.status_code);

    // Evaluate JavaScript in the isolated world
    let title_raw: serde_json::Value = page.evaluate("document.title", Duration::from_secs(5))?;
    let title = title_raw["result"]["value"].as_str().unwrap_or_default();
    println!("Page title: {title}");

    let heading_raw: serde_json::Value = page.evaluate("document.querySelector('h1').innerText", Duration::from_secs(5))?;
    let heading = heading_raw["result"]["value"].as_str().unwrap_or_default();
    println!("Heading: {heading}");

    // Capture screenshot
    let shot_opts = ScreenshotOptions {
        mime_type: "image/png".into(),
        clip: Rect { x: 0.0, y: 0.0, width: 800.0, height: 600.0 },
        quality: None,
        omit_device_scale_factor: None,
    };
    let png_bytes = page.screenshot(shot_opts)?;
    std::fs::write("example.png", png_bytes)?;
    println!("Saved screenshot to example.png");

    browser.close()?;
    let _ = launched.child.wait()?;
    Ok(())
}
```

Run the application:

```bash
cargo run
```

Expected output:
```text
Navigating to https://example.com...
Loaded with HTTP status: Some(200)
Page title: Example Domain
Heading: Example Domain
Saved screenshot to example.png
```

Verify that `example.png` exists in your project directory:

```bash
ls -lh example.png
```

---

## Step 4: Evaluate scripts in the main world

Camoufox 158+ separates JavaScript execution into worlds:
- `page.evaluate()` runs in an **isolated world**: DOM nodes are accessible, but globals set by site scripts (such as `window.__NEXT_DATA__`) are hidden.
- `page.evaluate_main_world()` runs in the **page's main world**: site globals are readable.

Let's test this distinction by injecting a page global and reading it back:

```rust,no_run
# fn doc_test(page: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
# use std::time::Duration;
    // Set a global variable via a DOM script element
    let _: serde_json::Value = page.evaluate(
        "const s = document.createElement('script'); s.textContent = 'window.mySecret = 42;'; document.head.appendChild(s);",
        Duration::from_secs(5)
    )?;

    // Isolated world cannot see window.mySecret:
    let isolated_raw: serde_json::Value = page.evaluate("window.mySecret", Duration::from_secs(5))?;
    let isolated_val = &isolated_raw["result"]["value"];
    println!("Isolated world raw value: {isolated_val:?}"); // Null / undefined

    // Main world reads window.mySecret directly:
    let main_val: serde_json::Value = page.evaluate_main_world("window.mySecret", Duration::from_secs(5))?;
    println!("Main world sees: {main_val}"); // 42
# Ok(())
# }
```

Note: If a target site uses strict Content-Security-Policy (CSP) that disallows inline scripts, launch with `bypass_csp: true` in `ContextOptions` to allow main-world evaluation.

---

## What you built

You created a complete, self-contained Camoufox automation client:
- Process lifecycle management without leaks or orphan processes.
- Juggler transport framing over raw Unix pipes.
- Navigation with lifecycle synchronization (`wait_until`).
- JavaScript execution across isolated and main worlds.
- Clipped 800x600 region screenshot capture.

## Next steps

- Read the task guides for CLI and advanced features:
  - [How to use the CLI daemon](howto-cli-daemon.md)
  - [How to handle iframes and shadow DOM](howto-iframes-and-shadow-dom.md)
  - [How to manage downloads and JavaScript dialogs](howto-downloads-and-dialogs.md)
  - [How to use Camoufox as a library](howto-library-usage.md)
- Explore the API details:
  - [CLI Reference](reference-cli.md)
  - [Rust Library API Reference](reference-library-api.md)
  - [Architecture and Design Rationale](explanation-architecture.md)
