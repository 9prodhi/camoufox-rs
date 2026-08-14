//! Daemon: Unix socket listener + request dispatch.
//!
//! Listens on a Unix domain socket and dispatches incoming requests to
//! the `InstanceManager`. Uses thread-per-connection for simplicity.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::cli::instance::InstanceManager;
use crate::cli::ipc::{DaemonRequest, DaemonResponse};

/// Run the daemon, blocking forever (or until Shutdown).
pub fn run(socket_path: &std::path::Path, foreground: bool) -> Result<(), String> {
    // Ensure parent directory exists with owner-only permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create socket directory: {e}"))?;
        // Set directory to 0o700.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    // Remove stale socket.
    if socket_path.exists() {
        // Try connecting to see if a daemon is already running.
        if UnixStream::connect(socket_path).is_ok() {
            return Err("daemon is already running".into());
        }
        std::fs::remove_file(socket_path)
            .map_err(|e| format!("failed to remove stale socket: {e}"))?;
    }

    let listener =
        UnixListener::bind(socket_path).map_err(|e| format!("failed to bind socket: {e}"))?;

    // Set socket permissions to owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    }

    let manager = Arc::new(Mutex::new(InstanceManager::new()));

    if foreground {
        eprintln!("camoufox daemon listening on {}", socket_path.display());
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let mgr = Arc::clone(&manager);
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, &mgr) {
                        log::warn!("connection error: {e}");
                    }
                });
            }
            Err(e) => {
                log::warn!("accept error: {e}");
            }
        }
    }

    Ok(())
}

fn handle_connection(
    stream: UnixStream,
    manager: &Arc<Mutex<InstanceManager>>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;

    if line.is_empty() {
        return Ok(());
    }

    let request: DaemonRequest =
        serde_json::from_str(&line).map_err(|e| format!("invalid request: {e}"))?;

    let response = dispatch(request, manager);

    let mut resp_json =
        serde_json::to_string(&response).map_err(|e| format!("serialize error: {e}"))?;
    resp_json.push('\n');
    writer
        .write_all(resp_json.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    // If this was a Shutdown, exit the process after responding.
    if response.ok && line.contains("\"Shutdown\"") {
        // Give a moment for the response to be sent.
        std::thread::sleep(std::time::Duration::from_millis(50));
        manager.lock().unwrap().shutdown_all();
        std::process::exit(0);
    }

    Ok(())
}

fn dispatch(request: DaemonRequest, manager: &Arc<Mutex<InstanceManager>>) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => {
            let mgr = manager.lock().unwrap();
            DaemonResponse::ok(json!({
                "instance_count": mgr.instance_count(),
            }))
        }

        DaemonRequest::Launch {
            headless,
            executable,
        } => {
            let mut mgr = manager.lock().unwrap();
            match mgr.launch(headless, executable.as_deref()) {
                Ok((instance_id, version, pid)) => DaemonResponse::ok(json!({
                    "instance_id": instance_id,
                    "version": version,
                    "pid": pid,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::List => {
            let mgr = manager.lock().unwrap();
            DaemonResponse::ok(json!({
                "instances": mgr.list(),
            }))
        }

        DaemonRequest::Stop { instance_id } => {
            let mut mgr = manager.lock().unwrap();
            match mgr.stop(&instance_id) {
                Ok(()) => DaemonResponse::ok_empty(),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::NewPage { instance_id } => {
            let mut mgr = manager.lock().unwrap();
            match mgr.new_page(&instance_id) {
                Ok(page_id) => DaemonResponse::ok(json!({ "page_id": page_id })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Navigate {
            instance_id,
            page_id,
            url,
            timeout_secs,
            wait_until,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.navigate(&instance_id, &page_id, &url, timeout, wait_until.as_deref()) {
                Ok(outcome) => DaemonResponse::ok(json!({
                    "navigation_id": outcome.nav_id,
                    "status_code": outcome.status_code,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Evaluate {
            instance_id,
            page_id,
            expression,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.evaluate(&instance_id, &page_id, &expression, timeout) {
                Ok(result) => DaemonResponse::ok(json!({ "result": result })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Click {
            instance_id,
            page_id,
            x,
            y,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.click(&instance_id, &page_id, x, y) {
                Ok(()) => DaemonResponse::ok(json!({ "clicked": true, "x": x, "y": y })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::ClickSelector {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.click_selector(&instance_id, &page_id, &selector, timeout) {
                Ok((x, y)) => DaemonResponse::ok(json!({
                    "clicked": true, "selector": selector, "x": x, "y": y,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Screenshot {
            instance_id,
            page_id,
            format,
            quality,
            path,
            selector,
            clip,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.screenshot(
                &instance_id,
                &page_id,
                format.as_deref(),
                quality,
                path.as_deref(),
                selector.as_deref(),
                clip,
                timeout,
            ) {
                Ok((bytes, out_path, rect)) => DaemonResponse::ok(json!({
                    "bytes": bytes.len(),
                    "path": out_path,
                    "clip": {
                        "x": rect.x, "y": rect.y,
                        "width": rect.width, "height": rect.height,
                    },
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Shutdown => {
            // Respond OK; the caller (handle_connection) handles the actual shutdown.
            DaemonResponse::ok_empty()
        }

        DaemonRequest::Cookies { instance_id } => {
            let mgr = manager.lock().unwrap();
            match mgr.cookies(&instance_id) {
                Ok(cookies) => DaemonResponse::ok(json!({ "cookies": cookies })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        // -------------------------------------------------------------------
        // Reading
        // -------------------------------------------------------------------
        DaemonRequest::Text {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.text(&instance_id, &page_id, selector.as_deref(), timeout) {
                Ok(text) => DaemonResponse::ok(json!({ "text": text })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Html {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.html(&instance_id, &page_id, selector.as_deref(), timeout) {
                Ok(html) => DaemonResponse::ok(json!({ "html": html })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Links {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.links(&instance_id, &page_id, selector.as_deref(), timeout) {
                Ok(links) => DaemonResponse::ok(json!({ "links": links })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Data {
            instance_id,
            page_id,
            og,
            jsonld,
            meta,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.data(&instance_id, &page_id, og, jsonld, meta, timeout) {
                Ok(data) => DaemonResponse::ok(data),
                Err(e) => DaemonResponse::err(e),
            }
        }

        // -------------------------------------------------------------------
        // Navigation / waiting
        // -------------------------------------------------------------------
        DaemonRequest::Url {
            instance_id,
            page_id,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.url(&instance_id, &page_id, timeout) {
                Ok((url, title)) => DaemonResponse::ok(json!({ "url": url, "title": title })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Back {
            instance_id,
            page_id,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.go_back(&instance_id, &page_id) {
                Ok(navigated) => {
                    DaemonResponse::ok(json!({ "navigated": navigated, "direction": "back" }))
                }
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Forward {
            instance_id,
            page_id,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.go_forward(&instance_id, &page_id) {
                Ok(navigated) => {
                    DaemonResponse::ok(json!({ "navigated": navigated, "direction": "forward" }))
                }
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Reload {
            instance_id,
            page_id,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.reload(&instance_id, &page_id) {
                Ok(()) => DaemonResponse::ok(json!({ "reloaded": true })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Wait {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            // NOTE: intentionally holds the manager lock for the whole poll —
            // matches every other handler; the daemon serialises page work.
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.wait_for_selector(&instance_id, &page_id, &selector, timeout) {
                Ok(waited_ms) => DaemonResponse::ok(json!({
                    "found": true, "selector": selector, "waited_ms": waited_ms,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        // -------------------------------------------------------------------
        // Cookies / headers
        // -------------------------------------------------------------------
        DaemonRequest::SetCookie {
            instance_id,
            page_id,
            name,
            value,
            url,
            domain,
            path,
            secure,
            http_only,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.set_cookie(
                &instance_id,
                &page_id,
                &name,
                &value,
                url.as_deref(),
                domain.as_deref(),
                path.as_deref(),
                secure,
                http_only,
                timeout,
            ) {
                Ok(data) => DaemonResponse::ok(data),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::SetHeader {
            instance_id,
            page_id,
            name,
            value,
        } => {
            let mut mgr = manager.lock().unwrap();
            match mgr.set_header(&instance_id, &page_id, &name, &value) {
                Ok(headers) => {
                    let list: Vec<serde_json::Value> = headers
                        .iter()
                        .map(|(n, v)| json!({ "name": n, "value": v }))
                        .collect();
                    DaemonResponse::ok(json!({ "header_set": name, "headers": list }))
                }
                Err(e) => DaemonResponse::err(e),
            }
        }

        // -------------------------------------------------------------------
        // Interaction
        // -------------------------------------------------------------------
        DaemonRequest::Fill {
            instance_id,
            page_id,
            selector,
            value,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.fill(&instance_id, &page_id, &selector, &value, timeout) {
                Ok(tag) => DaemonResponse::ok(json!({
                    "filled": true, "selector": selector, "tag": tag, "value": value,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Type {
            instance_id,
            page_id,
            text,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.insert_text(&instance_id, &page_id, &text) {
                Ok(()) => DaemonResponse::ok(json!({ "typed": text })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Press {
            instance_id,
            page_id,
            key,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.press(&instance_id, &page_id, &key) {
                Ok(()) => DaemonResponse::ok(json!({ "pressed": key })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Hover {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.hover(&instance_id, &page_id, &selector, timeout) {
                Ok((x, y)) => DaemonResponse::ok(json!({
                    "hovered": true, "selector": selector, "x": x, "y": y,
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Select {
            instance_id,
            page_id,
            selector,
            value,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.select_option(&instance_id, &page_id, &selector, &value, timeout) {
                Ok(result) => DaemonResponse::ok(json!({
                    "selected": true,
                    "selector": selector,
                    "option_value": result.get("value"),
                    "option_text": result.get("text"),
                })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::Scroll {
            instance_id,
            page_id,
            selector,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            let timeout = std::time::Duration::from_secs(timeout_secs);
            match mgr.scroll(&instance_id, &page_id, selector.as_deref(), timeout) {
                Ok(result) => DaemonResponse::ok(result),
                Err(e) => DaemonResponse::err(e),
            }
        }

        // -------------------------------------------------------------------
        // Tabs
        // -------------------------------------------------------------------
        DaemonRequest::Tabs {
            instance_id,
            timeout_secs,
        } => {
            let mgr = manager.lock().unwrap();
            match mgr.tabs(&instance_id, std::time::Duration::from_secs(timeout_secs)) {
                Ok(tabs) => DaemonResponse::ok(json!({ "tabs": tabs })),
                Err(e) => DaemonResponse::err(e),
            }
        }

        DaemonRequest::CloseTab {
            instance_id,
            page_id,
        } => {
            let mut mgr = manager.lock().unwrap();
            match mgr.close_tab(&instance_id, &page_id) {
                Ok(()) => DaemonResponse::ok(json!({ "closed_page": page_id })),
                Err(e) => DaemonResponse::err(e),
            }
        }
    }
}
