//! Unix socket client for communicating with the daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::cli::ipc::{DaemonRequest, DaemonResponse};

/// Send a request to the daemon and return the response.
pub fn send_request(socket_path: &Path, request: &DaemonRequest) -> Result<DaemonResponse, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            "daemon is not running (start with: camoufox serve --foreground)".to_string()
        } else {
            format!("failed to connect to daemon: {e}")
        }
    })?;

    // Set a timeout so we don't hang forever.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let mut req_json = serde_json::to_string(request).map_err(|e| e.to_string())?;
    req_json.push('\n');
    writer
        .write_all(req_json.as_bytes())
        .map_err(|e| format!("failed to send request: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("failed to flush: {e}"))?;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("failed to read response: {e}"))?;

    if line.is_empty() {
        return Err("daemon closed connection without responding".into());
    }

    serde_json::from_str(&line).map_err(|e| format!("invalid response from daemon: {e}"))
}
