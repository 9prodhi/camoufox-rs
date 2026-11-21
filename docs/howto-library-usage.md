# How to Use Camoufox as a Rust Library

This guide shows how to integrate `camoufox` directly into your Rust application without running the CLI daemon.

---

## 1. Add Dependency to Cargo.toml

Because `camoufox` is currently in pre-release development and not yet published to crates.io, reference it by path or repository URL:

```toml
[dependencies]
camoufox = { path = "../camoufox-rs" } # or git = "https://github.com/9prodhi/camoufox-rs"
tempfile = "3"
serde_json = "1"
```

If you only use the library core, default features are sufficient. The `cli` feature is only required if building CLI daemon components.

---

## 2. Complete End-to-End Example

The following example spawns a headless Camoufox browser process over Unix pipes, creates an isolated browser context, opens a page, navigates to a site, evaluates JavaScript, and shuts down cleanly:

```rust,no_run
use camoufox::api::browser::{Browser, BrowserOptions};
use camoufox::api::context::ContextOptions;
use camoufox::api::main_frame::{NavigateOptions, NavigateOutcome};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Configure the browser launch with a temporary profile directory
    let temp_dir = tempfile::tempdir()?;
    let mut config = LaunchConfig::default();
    config.executable = PathBuf::from(std::env::var("CAMOUFOX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap();
        format!("{home}/.cache/camoufox/camoufox")
    }));
    config.profile_dir = Some(temp_dir.path().to_owned());
    config.headless = true;

    // 2. Spawn the browser process with pipe transport (fd 3/4)
    let mut launched = process::unix::spawn(&config)?;
    process::readiness::wait_for_ready(&mut launched.child, config.timeout)?;

    // 3. Connect the API client
    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let root_session = conn.root_session();
    let browser = Browser::connect(conn, root_session, BrowserOptions::default())?;

    // 4. Create an isolated browser context (profile)
    let context = browser.new_context(ContextOptions::default())?;

    // 5. Open a new page/tab (MainFrame pinned to top frame)
    let page = context.new_main_frame()?;

    // 6. Navigate to a target URL
    let nav_opts = NavigateOptions::default();
    let outcome: NavigateOutcome = page.navigate("https://example.com", nav_opts, Duration::from_secs(30))?;
    println!("Navigation complete, HTTP status: {:?}", outcome.status_code);

    // 7. Evaluate JavaScript in the isolated execution world
    let raw_title: serde_json::Value = page.evaluate("document.title", Duration::from_secs(5))?;
    let title = raw_title["result"]["value"].as_str().unwrap_or_default();
    println!("Page title: {title}");

    // 8. Graceful shutdown
    browser.close()?;
    let _ = launched.child.wait();

    Ok(())
}
```

---

## 3. Interacting with Elements

`MainFrame` provides direct DOM interaction methods and raw keyboard and pointer event dispatching.

### DOM Evaluation and Text Insertion

```rust,no_run
# fn doc_test(page: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::main_frame::KeyEventParams;
use std::time::Duration;

let timeout = Duration::from_secs(10);

// Evaluate DOM action in page
let _: serde_json::Value = page.evaluate(
    "document.querySelector('button#submit')?.click()",
    timeout,
)?;

// Insert text into currently focused element
page.insert_text("password123")?;

// Dispatch raw key event
let key_event = KeyEventParams {
    r#type: "keydown".to_string(),
    key_code: 13,
    code: "Enter".to_string(),
    key: "Enter".to_string(),
    repeat: false,
    location: 0,
    text: None,
};
page.dispatch_key_event(key_event)?;
# Ok(())
# }
```

---

## 4. Working with Iframes and Shadow DOM

To resolve child frames inside a page:

```rust,no_run
# fn doc_test(page: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
use std::time::Duration;

let timeout = Duration::from_secs(10);

// Resolve child iframe ID
let top_id = page.frame_id();
let child_id = page.resolve_child_frame(&top_id, "iframe#payment-frame", timeout)?;

// Evaluate expression in child frame
let raw_val: serde_json::Value = page.evaluate_in_frame(&child_id, "document.title", timeout)?;
let val = raw_val["result"]["value"].as_str().unwrap_or_default();
println!("Frame title: {val}");
# Ok(())
# }
```

For detailed explanations of iframe coordinate transformation and focus management, see [How to Work with Iframes and Shadow DOM](howto-iframes-and-shadow-dom.md).

---

## 5. Execution Worlds and CSP Bypass

Camoufox supports two distinct execution contexts for running JavaScript:

1. **Isolated World (`page.evaluate`)**:
   - Runs in a clean JavaScript sandbox with its own global namespace.
   - Page scripts cannot detect or modify globals injected by your evaluation.
   - Recommended for web scraping and test assertions.

2. **Main World (`page.evaluate_main_world`)**:
   - Runs in the page's actual window execution context.
   - Can inspect and modify window variables defined by page scripts.
   - If the site enforces strict Content Security Policy (`script-src 'self'`), enable `bypass_csp` on your context options:

```rust,no_run
# fn doc_test() {
use camoufox::api::context::ContextOptions;

let mut opts = ContextOptions::default();
opts.bypass_csp = true;
# }
```

---

## 6. Cookies and Custom Headers

### Managing Cookies

Cookies can be retrieved or injected through `BrowserContext`:

```rust,no_run
# fn doc_test(context: &camoufox::api::context::BrowserContext) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::context::CookieOptions;

// Read all cookies in the context
let cookies = context.get_cookies()?;
for c in cookies {
    println!("{}: {}", c.name, c.value);
}

// Set a custom session cookie
let new_cookie = CookieOptions {
    name: "session_id".into(),
    value: "xyz123".into(),
    url: None,
    domain: Some(".example.com".into()),
    path: Some("/".into()),
    secure: None,
    http_only: None,
    same_site: None,
    expires: None,
};
context.set_cookies(&[new_cookie])?;

Ok(())
# }
```

### Setting Custom HTTP Headers

Custom headers can be set per page via `set_extra_http_headers`:

```rust,no_run
# fn doc_test(page: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
page.set_extra_http_headers(&[
    ("Authorization", "Bearer secret_token"),
    ("X-Custom-Header", "ScraperBot/1.0"),
])?;
# Ok(())
# }
```
