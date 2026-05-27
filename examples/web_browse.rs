//! Complete web browsing example using Camoufox.
//!
//! Demonstrates: launch, page setup via `BrowserContext::new_main_frame`,
//! navigation, JS evaluation, screenshot.
//!
//! Usage:
//!   cargo run --example web_browse
//!   cargo run --example web_browse -- --url https://scholar.google.com

use std::path::PathBuf;
use std::time::{Duration, Instant};

use camoufox::api::{Browser, BrowserOptions, ContextOptions, Rect, ScreenshotOptions};
use camoufox::config::LaunchConfig;
use camoufox::process;
use camoufox::protocol::client::Connection;
use camoufox::transport::pipe::PipeTransport;

const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const EVALUATE_TIMEOUT: Duration = Duration::from_secs(15);

fn camoufox_binary() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    for path in [
        PathBuf::from(format!("{home}/.cache/camoufox/camoufox")),
        PathBuf::from("/root/.cache/camoufox/camoufox"),
    ] {
        if path.exists() {
            return path;
        }
    }
    panic!("Camoufox binary not found in HOME/.cache/camoufox or /root/.cache/camoufox.");
}

fn extract_value(result: &serde_json::Value) -> serde_json::Value {
    result
        .get("result")
        .and_then(|r| r.get("value"))
        .or_else(|| result.get("value"))
        .cloned()
        .unwrap_or(result.clone())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::try_init();

    let url = std::env::args()
        .skip_while(|a| a != "--url")
        .nth(1)
        .unwrap_or_else(|| "https://example.com".to_string());

    let binary = camoufox_binary();
    println!("[*] Using Camoufox binary: {}", binary.display());

    // 1. Launch browser.
    let profile_dir = tempfile::tempdir()?;
    let config = LaunchConfig {
        executable: binary,
        profile_dir: Some(profile_dir.path().to_owned()),
        headless: true,
        ..Default::default()
    };

    println!("[*] Spawning Camoufox (headless)...");
    let mut launched = process::unix::spawn(&config)?;
    let pid = launched.child.id();
    println!("[*] Process spawned (PID: {pid})");

    println!("[*] Waiting for browser readiness...");
    let _ = process::readiness::wait_for_ready(&mut launched.child, config.timeout)?;
    println!("[+] Browser ready!");

    // 2. Connect.
    let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
    let conn = Connection::new(Box::new(transport));
    let root = conn.root_session();
    let browser = Browser::connect(conn, root, BrowserOptions::default())?;

    println!(
        "[+] Connected — version: {}",
        browser.version().unwrap_or("unknown")
    );
    println!(
        "[+] User-Agent: {}",
        browser.user_agent().unwrap_or("unknown")
    );

    // 3. Create a context and a fully-wired MainFrame in one call.
    //
    // `new_main_frame` applies all three filter fixes (Layer 1: targetInfo.type
    // filter on attachedToTarget; Layer 2: parentFrameId filter on
    // Page.frameAttached; Layer 3: auxData.frameId filter on
    // executionContextCreated), so the returned handle is structurally pinned
    // to the page's top frame.
    println!("[*] Creating page (via new_main_frame)...");
    let context = browser.new_context(ContextOptions::default())?;
    let main_frame = context.new_main_frame()?;
    println!("[+] MainFrame ready (target={})", main_frame.target_id());

    // 4. Navigate.
    println!("[*] Navigating to {url}...");
    let nav_id = main_frame.navigate(&url, Default::default(), PAGE_LOAD_TIMEOUT)?;
    println!("[*] Navigation started (id: {nav_id:?})");

    // 5. Wait for the page to settle on the target URL and reach readyState=complete.
    // MainFrame::evaluate internally polls the cached execution context with a
    // timeout, so we don't need any manual mpsc plumbing here.
    println!("[*] Waiting for page load...");
    let deadline = Instant::now() + PAGE_LOAD_TIMEOUT;
    loop {
        let loc = main_frame
            .evaluate("window.location.href", EVALUATE_TIMEOUT)
            .ok()
            .map(|r| extract_value(&r))
            .and_then(|v| v.as_str().map(|s| s.to_owned()))
            .unwrap_or_default();

        if loc != "about:blank" && !loc.is_empty() {
            let state = main_frame
                .evaluate("document.readyState", EVALUATE_TIMEOUT)
                .ok()
                .map(|r| extract_value(&r))
                .and_then(|v| v.as_str().map(|s| s.to_owned()))
                .unwrap_or_default();
            if state == "complete" {
                break;
            }
        }

        if Instant::now() >= deadline {
            eprintln!("[!] Timed out waiting for page load, proceeding with current state");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("[+] Page loaded!");

    // 6. Extract page info via JS.
    let title = extract_value(&main_frame.evaluate("document.title", EVALUATE_TIMEOUT)?);
    println!("[+] Page title: {title}");

    let current_url =
        extract_value(&main_frame.evaluate("window.location.href", EVALUATE_TIMEOUT)?);
    println!("[+] Current URL: {current_url}");

    let text = extract_value(&main_frame.evaluate(
        "document.body ? document.body.innerText.substring(0, 1000) : '(no body)'",
        EVALUATE_TIMEOUT,
    )?);
    println!("\n--- Page Content (first 1000 chars) ---");
    println!("{text}");
    println!("--- End ---\n");

    // 7. Screenshot.
    let dims = extract_value(
        &main_frame.evaluate("[window.innerWidth, window.innerHeight]", EVALUATE_TIMEOUT)?,
    );
    let width = dims.get(0).and_then(|v| v.as_f64()).unwrap_or(1280.0);
    let height = dims.get(1).and_then(|v| v.as_f64()).unwrap_or(720.0);

    let screenshot_path = "/tmp/camoufox-screenshot.png";
    let options = ScreenshotOptions {
        mime_type: "image/png".to_string(),
        clip: Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        quality: None,
        omit_device_scale_factor: None,
    };

    let bytes = main_frame.screenshot(options)?;
    std::fs::write(screenshot_path, &bytes)?;
    println!(
        "[+] Screenshot saved to {screenshot_path} ({} bytes)",
        bytes.len()
    );

    // 8. Clean shutdown.
    println!("[*] Closing browser...");
    browser.close()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match launched.child.try_wait() {
            Ok(Some(status)) => {
                println!("[+] Browser exited ({status})");
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = launched.child.kill();
                    let _ = launched.child.wait();
                    println!("[!] Browser killed (timeout)");
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                println!("[!] Wait error: {e}");
                break;
            }
        }
    }

    println!("[+] Done!");
    Ok(())
}
