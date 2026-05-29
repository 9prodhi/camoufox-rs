//! Output formatting for CLI responses.

use crate::cli::ipc::DaemonResponse;

/// Print a daemon response in the appropriate format.
pub fn print_response(response: &DaemonResponse, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".into())
        );
        return;
    }

    if !response.ok {
        eprintln!(
            "error: {}",
            response.error.as_deref().unwrap_or("unknown error")
        );
        return;
    }

    // Human-readable output based on the data shape.
    if let Some(data) = &response.data {
        // Launch response
        if let Some(id) = data.get("instance_id").and_then(|v| v.as_str()) {
            println!("{id}");
            if let Some(version) = data.get("version").and_then(|v| v.as_str()) {
                eprintln!("version: {version}");
            }
            if let Some(pid) = data.get("pid").and_then(|v| v.as_u64()) {
                eprintln!("pid: {pid}");
            }
            return;
        }

        // NewPage response
        if let Some(page_id) = data.get("page_id").and_then(|v| v.as_str()) {
            println!("{page_id}");
            return;
        }

        // List response
        if let Some(instances) = data.get("instances").and_then(|v| v.as_array()) {
            if instances.is_empty() {
                println!("no running instances");
                return;
            }
            for inst in instances {
                let id = inst
                    .get("instance_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let pid = inst
                    .get("pid")
                    .and_then(|v| v.as_u64())
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into());
                let version = inst.get("version").and_then(|v| v.as_str()).unwrap_or("?");
                let pages = inst
                    .get("pages")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                println!("{id}  pid={pid}  version={version}  pages=[{pages}]");
            }
            return;
        }

        // Evaluate response
        if let Some(result) = data.get("result") {
            match result {
                serde_json::Value::String(s) => println!("{s}"),
                other => println!("{other}"),
            }
            return;
        }

        // Screenshot response
        if let Some(path) = data.get("path").and_then(|v| v.as_str()) {
            let bytes = data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("{path} ({bytes} bytes)");
            return;
        }

        // Ping response
        if let Some(count) = data.get("instance_count").and_then(|v| v.as_u64()) {
            println!("pong ({count} instances)");
            return;
        }

        // Cookies response
        if let Some(cookies) = data.get("cookies").and_then(|v| v.as_array()) {
            for cookie in cookies {
                let name = cookie.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let value = cookie.get("value").and_then(|v| v.as_str()).unwrap_or("");
                println!("{name}={value}");
            }
            println!("{} cookie(s)", cookies.len());
            return;
        }

        // Navigation response
        if let Some(nav_id) = data.get("navigation_id") {
            let status_str = data
                .get("status_code")
                .and_then(|v| v.as_u64())
                .map(|s| format!(" status={s}"))
                .unwrap_or_default();
            if nav_id.is_null() {
                println!("ok (same-document navigation){status_str}");
            } else if let Some(id) = nav_id.as_str() {
                println!("ok (navigation_id: {id}){status_str}");
            } else {
                println!("ok{status_str}");
            }
            return;
        }

        // Fallback: print the data as JSON.
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("ok");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ipc::DaemonResponse;
    use serde_json::json;

    fn capture_stdout<F: FnOnce()>(f: F) -> String {
        // We don't capture stdout here (would need an extra crate); the
        // invariant under test is that `print_response` runs to completion
        // without panicking. Actually invoke the closure so the real
        // `print_response` code path is exercised.
        f();
        String::new()
    }

    /// `print_response` in JSON mode emits the full response including cookies
    /// with httpOnly preserved — verified by checking `to_string_pretty` output.
    #[test]
    fn json_mode_includes_http_only_cookies() {
        let resp = DaemonResponse::ok(json!({
            "cookies": [
                {
                    "name": "session",
                    "value": "abc",
                    "httpOnly": false
                },
                {
                    "name": "PHPSESSID",
                    "value": "secret",
                    "httpOnly": true
                }
            ]
        }));
        let serialized = serde_json::to_string_pretty(&resp).expect("serialize");
        // Both cookies appear in JSON output.
        assert!(serialized.contains("\"session\""), "session cookie present");
        assert!(
            serialized.contains("\"PHPSESSID\""),
            "PHPSESSID cookie present"
        );
        // httpOnly:true cookie is not silently dropped.
        assert!(
            serialized.contains("\"httpOnly\": true"),
            "httpOnly:true preserved in JSON output"
        );
    }

    /// `print_response` in human mode does not panic for a cookies payload
    /// containing an HttpOnly cookie.
    #[test]
    fn human_mode_cookies_does_not_panic() {
        let resp = DaemonResponse::ok(json!({
            "cookies": [
                {
                    "name": "session",
                    "value": "abc",
                    "httpOnly": false
                },
                {
                    "name": "PHPSESSID",
                    "value": "secret",
                    "httpOnly": true
                }
            ]
        }));
        // Calling print_response should complete without panicking.
        // (We cannot easily capture stdout in std tests, but non-panic is
        //  the key invariant here.)
        capture_stdout(|| print_response(&resp, false));
    }

    /// `print_response` in JSON mode for an error response does not panic.
    #[test]
    fn json_mode_error_response_does_not_panic() {
        let resp = DaemonResponse::err("instance not found");
        capture_stdout(|| print_response(&resp, true));
    }
}
