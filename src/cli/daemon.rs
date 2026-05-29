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
                Ok(nav_id) => DaemonResponse::ok(json!({ "navigation_id": nav_id })),
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

        DaemonRequest::Screenshot {
            instance_id,
            page_id,
            format,
            quality,
            path,
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
                timeout,
            ) {
                Ok((bytes, out_path)) => DaemonResponse::ok(json!({
                    "bytes": bytes.len(),
                    "path": out_path,
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
    }
}
