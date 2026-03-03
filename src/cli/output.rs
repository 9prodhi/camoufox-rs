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
                let version = inst
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
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

        // Navigation response
        if let Some(nav_id) = data.get("navigation_id") {
            if nav_id.is_null() {
                println!("ok (same-document navigation)");
            } else if let Some(id) = nav_id.as_str() {
                println!("ok (navigation_id: {id})");
            } else {
                println!("ok");
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
