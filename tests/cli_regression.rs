//! Regression tests pinning the agent-facing behaviour of the camoufox CLI.
//!
//! These tests assert the **current** output shapes so that later additive
//! changes (new fields, new subcommands, error-kind enrichment) are proven
//! not to break existing consumers.
//!
//! # Running
//!
//! Non-browser tests (always run):
//! ```sh
//! cargo test --all-features --test cli_regression
//! ```
//!
//! Browser-integration tests (require a Camoufox installation):
//! ```sh
//! cargo test --all-features --test cli_regression -- --ignored --test-threads=1
//! ```

#![cfg(feature = "cli")]

use std::io::Cursor;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use camoufox::cli::ipc::DaemonResponse;
use camoufox::cli::output::render;
use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Path to the `camoufox` binary under test, set at compile-time by Cargo.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_camoufox")
}

/// Assert that the JSON object `v` satisfies the error contract:
///  - `ok == false`
///  - `error` is exactly `expected_error`
///  - `data` is absent
///  - `kind` is present, required, and a string
///  - only `ok`, `error`, and `kind` keys are present
fn assert_error_shape(v: &Value, expected_error: &str) {
    assert_eq!(v["ok"], false, "ok must be false in error response");
    assert_eq!(
        v["error"],
        Value::String(expected_error.to_owned()),
        "error text must match exactly"
    );
    assert!(
        v.get("data").is_none(),
        "data must be absent in error response; got: {:?}",
        v.get("data")
    );
    for key in v.as_object().unwrap().keys() {
        assert!(
            matches!(key.as_str(), "ok" | "error" | "kind"),
            "unexpected key {key:?} in error response (only ok/error/kind are allowed)"
        );
    }
    let kind = v.get("kind").expect("kind is required in error response");
    assert!(kind.is_string(), "kind must be a string; got: {kind:?}");
}

// ---------------------------------------------------------------------------
// Plain-mode golden tests via `render`
// ---------------------------------------------------------------------------

#[test]
fn render_ping() {
    let data = json!({ "instance_count": 0 });
    assert_eq!(render(Some(&data)).stdout, "pong (0 instances)");

    let data = json!({ "instance_count": 3 });
    assert_eq!(render(Some(&data)).stdout, "pong (3 instances)");
}

#[test]
fn render_launch() {
    let data = json!({
        "instance_id": "00000001",
        "version": "Firefox/150.0",
        "pid": 42
    });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "00000001");
    assert_eq!(r.stderr, vec!["version: Firefox/150.0", "pid: 42"]);
}

#[test]
fn render_launch_no_version_no_pid() {
    // A launch response without optional fields still works.
    let data = json!({ "instance_id": "00000002" });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "00000002");
    assert!(r.stderr.is_empty());
}

#[test]
fn render_list_empty() {
    let data = json!({ "instances": [] });
    assert_eq!(render(Some(&data)).stdout, "no running instances");
}

#[test]
fn render_list_one_instance() {
    let data = json!({
        "instances": [
            {
                "instance_id": "00000001",
                "pid": 1234,
                "version": "Firefox/150.0",
                "pages": ["p1", "p2"]
            }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(
        r.stdout,
        "00000001  pid=1234  version=Firefox/150.0  pages=[p1,p2]"
    );
    assert!(r.stderr.is_empty());
}

#[test]
fn render_new_page() {
    let data = json!({ "page_id": "p1" });
    assert_eq!(render(Some(&data)).stdout, "p1");
}

#[test]
fn render_navigate_with_nav_id_and_status() {
    let data = json!({ "navigation_id": "nav-13", "status_code": 200 });
    assert_eq!(
        render(Some(&data)).stdout,
        "ok (navigation_id: nav-13) status=200"
    );
}

#[test]
fn render_navigate_without_status_code() {
    let data = json!({ "navigation_id": "nav-7" });
    assert_eq!(render(Some(&data)).stdout, "ok (navigation_id: nav-7)");
}

#[test]
fn render_navigate_same_document_null_nav_id() {
    let data = json!({ "navigation_id": null });
    assert_eq!(render(Some(&data)).stdout, "ok (same-document navigation)");
}

#[test]
fn render_navigate_same_document_null_nav_id_with_status() {
    let data = json!({ "navigation_id": null, "status_code": 200 });
    assert_eq!(
        render(Some(&data)).stdout,
        "ok (same-document navigation) status=200"
    );
}

#[test]
fn render_text() {
    let data = json!({ "text": "Hello\nWorld" });
    assert_eq!(render(Some(&data)).stdout, "Hello\nWorld");
}

#[test]
fn render_links_with_text_and_bare() {
    let data = json!({
        "links": [
            { "text": "One", "href": "https://example.com/one" },
            { "text": "", "href": "https://example.com/two" }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(
        r.stdout,
        "One → https://example.com/one\nhttps://example.com/two"
    );
    assert_eq!(r.stderr, vec!["2 link(s)"]);
}

#[test]
fn render_links_empty() {
    let data = json!({ "links": [] });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "");
    assert_eq!(r.stderr, vec!["0 link(s)"]);
}

#[test]
fn render_fill() {
    // `fill` returns a JSON fallback (no special key recognised by render).
    let data = json!({
        "filled": true,
        "selector": "#q",
        "tag": "input",
        "value": "hello"
    });
    // render falls through to pretty-print since none of the special keys match.
    let r = render(Some(&data));
    assert!(r.stdout.contains("\"filled\""));
    assert!(r.stdout.contains("\"selector\""));
    assert!(r.stdout.contains("\"tag\""));
    assert!(r.stdout.contains("\"value\""));
}

#[test]
fn render_click_basic() {
    // A coordinate click with no dialog, no downloads.
    let data = json!({ "clicked": true, "x": 100, "y": 200 });
    let r = render(Some(&data));
    assert!(r.stdout.contains("\"clicked\""));
    assert!(r.stdout.contains("\"x\""));
    assert!(r.stdout.contains("\"y\""));
}

#[test]
fn render_click_with_dialog() {
    let data = json!({
        "clicked": true,
        "x": 10,
        "y": 20,
        "dialog": { "kind": "alert", "message": "hello" },
        "dialog_action": "accept"
    });
    // dialog_action is checked first → "dialog accepted: alert \"hello\""
    assert_eq!(
        render(Some(&data)).stdout,
        r#"dialog accepted: alert "hello""#
    );
}

#[test]
fn render_click_with_downloads() {
    // A click that produced a download — `clicked` key present alongside `downloads`.
    let data = json!({
        "clicked": true,
        "selector": "#dl",
        "x": 50,
        "y": 60,
        "downloads": [
            {
                "state": "completed",
                "page": "p1",
                "url": "http://x/file.pdf",
                "path": "/tmp/dl/uuid1"
            }
        ]
    });
    // `clicked` present → `downloads` branch skipped → falls to JSON pretty-print.
    let r = render(Some(&data));
    assert!(r.stdout.contains("\"clicked\""));
    assert!(r.stdout.contains("\"downloads\""));
}

#[test]
fn render_select() {
    let data = json!({
        "selected": true,
        "selector": "#sel",
        "option_value": "b",
        "option_text": "Beta"
    });
    // `selected` key: no special branch → falls to JSON pretty-print.
    let r = render(Some(&data));
    assert!(r.stdout.contains("\"selected\""));
    assert!(r.stdout.contains("\"option_value\""));
}

#[test]
fn render_evaluate_string() {
    let data = json!({ "result": "hello world" });
    // String results are printed unquoted.
    assert_eq!(render(Some(&data)).stdout, "hello world");
}

#[test]
fn render_evaluate_non_string_number() {
    let data = json!({ "result": 42 });
    assert_eq!(render(Some(&data)).stdout, "42");
}

#[test]
fn render_evaluate_non_string_bool() {
    let data = json!({ "result": true });
    assert_eq!(render(Some(&data)).stdout, "true");
}

#[test]
fn render_evaluate_non_string_null() {
    let data = json!({ "result": null });
    assert_eq!(render(Some(&data)).stdout, "null");
}

#[test]
fn render_evaluate_non_string_object() {
    let data = json!({ "result": {"n": 1} });
    assert_eq!(render(Some(&data)).stdout, "{\"n\":1}");
}

#[test]
fn render_screenshot() {
    let data = json!({ "path": "/tmp/shot.png", "bytes": 30447 });
    assert_eq!(render(Some(&data)).stdout, "/tmp/shot.png (30447 bytes)");
}

#[test]
fn render_cookies_non_empty() {
    let data = json!({
        "cookies": [
            { "name": "session", "value": "abc" },
            { "name": "PHPSESSID", "value": "secret" }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "session=abc\nPHPSESSID=secret");
    assert_eq!(r.stderr, vec!["2 cookie(s)"]);
}

#[test]
fn render_cookies_empty() {
    let data = json!({ "cookies": [] });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "");
    assert_eq!(r.stderr, vec!["0 cookie(s)"]);
}

#[test]
fn render_tabs_non_empty() {
    let data = json!({
        "tabs": [
            { "page_id": "p1", "url": "https://example.com/", "title": "Example Domain" },
            { "page_id": "p2", "url": "https://other.example/", "title": "Other" }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(
        r.stdout,
        "p1  https://example.com/  Example Domain\np2  https://other.example/  Other"
    );
}

#[test]
fn render_tabs_empty() {
    let data = json!({ "tabs": [] });
    assert_eq!(render(Some(&data)).stdout, "no open pages");
}

#[test]
fn render_downloads_with_path() {
    let data = json!({
        "downloads": [
            {
                "state": "completed",
                "page": "p1",
                "url": "http://x/file.pdf",
                "path": "/tmp/dl/uuid1"
            }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(
        r.stdout,
        "completed  p1  http://x/file.pdf  -> /tmp/dl/uuid1"
    );
}

#[test]
fn render_downloads_canceled_no_path() {
    let data = json!({
        "downloads": [
            {
                "state": "canceled",
                "page": "p1",
                "url": "http://x/file.pdf"
            }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(r.stdout, "canceled  p1  http://x/file.pdf");
}

#[test]
fn render_downloads_error() {
    let data = json!({
        "downloads": [
            {
                "state": "failed",
                "page": "p1",
                "url": "http://x/file.pdf",
                "error": "network error"
            }
        ]
    });
    let r = render(Some(&data));
    assert_eq!(
        r.stdout,
        "failed  p1  http://x/file.pdf  (error: network error)"
    );
}

#[test]
fn render_downloads_empty() {
    let data = json!({ "downloads": [] });
    assert_eq!(render(Some(&data)).stdout, "no downloads");
}

#[test]
fn render_reload() {
    // `reload` returns `{"reloaded": true}` — no special branch → JSON pretty-print.
    let data = json!({ "reloaded": true });
    let r = render(Some(&data));
    assert!(r.stdout.contains("\"reloaded\""));
    assert!(r.stdout.contains("true"));
}

#[test]
fn render_back_navigated() {
    let data = json!({ "navigated": true, "direction": "back" });
    assert_eq!(render(Some(&data)).stdout, "ok (back)");
}

#[test]
fn render_back_no_history() {
    let data = json!({ "navigated": false, "direction": "back" });
    assert_eq!(render(Some(&data)).stdout, "no back history entry");
}

#[test]
fn render_forward_navigated() {
    let data = json!({ "navigated": true, "direction": "forward" });
    assert_eq!(render(Some(&data)).stdout, "ok (forward)");
}

#[test]
fn render_forward_no_history() {
    let data = json!({ "navigated": false, "direction": "forward" });
    assert_eq!(render(Some(&data)).stdout, "no forward history entry");
}

#[test]
fn render_no_data() {
    // Commands like `stop` or `shutdown` return no data.
    assert_eq!(render(None).stdout, "ok");
}

// ---------------------------------------------------------------------------
// JSON golden tests for DaemonResponse shapes
// ---------------------------------------------------------------------------

#[test]
fn json_ok_with_data_shape() {
    let resp = DaemonResponse::ok(json!({ "instance_count": 2 }));
    let v: Value = serde_json::to_value(&resp).expect("serialize ok response");

    assert_eq!(v["ok"], true, "ok must be true");
    assert_eq!(v["data"]["instance_count"], 2);
    assert!(
        v.get("error").is_none(),
        "error must be absent in success response"
    );
}

#[test]
fn json_ok_empty_shape() {
    let resp = DaemonResponse::ok_empty();
    let v: Value = serde_json::to_value(&resp).expect("serialize ok_empty response");

    assert_eq!(v["ok"], true);
    assert!(v.get("error").is_none());
    assert!(v.get("data").is_none());
}

#[test]
fn json_err_shape_instance_not_found() {
    let msg = "instance 00000001 not found";
    let resp = DaemonResponse::err(msg);
    let v: Value = serde_json::to_value(&resp).expect("serialize error response");
    assert_error_shape(&v, msg);
    assert_eq!(v["kind"], "instance_not_found");
}

#[test]
fn json_err_shape_page_not_found() {
    let msg = "page p99 not found";
    let resp = DaemonResponse::err(msg);
    let v: Value = serde_json::to_value(&resp).expect("serialize error response");
    assert_error_shape(&v, msg);
    assert_eq!(v["kind"], "page_not_found");
}

#[test]
fn json_err_shape_selector_not_found() {
    let msg = "selector not found: #x";
    let resp = DaemonResponse::err(msg);
    let v: Value = serde_json::to_value(&resp).expect("serialize error response");
    assert_error_shape(&v, msg);
    assert_eq!(v["kind"], "selector_not_found");
}

#[test]
fn json_err_shape_timeout() {
    let msg = "timed out after 2s waiting for selector: #x";
    let resp = DaemonResponse::err(msg);
    let v: Value = serde_json::to_value(&resp).expect("serialize error response");
    assert_error_shape(&v, msg);
    assert_eq!(v["kind"], "timeout");
}

#[test]
fn json_err_shape_io() {
    let msg = "io error: connection refused";
    let resp = DaemonResponse::err(msg);
    let v: Value = serde_json::to_value(&resp).expect("serialize error response");
    assert_error_shape(&v, msg);
    assert_eq!(v["kind"], "io");
}

#[test]
fn json_err_shape_kind_is_required_string() {
    // Error responses require a string `kind` field.
    let resp = DaemonResponse::err("some error");
    let v: Value = serde_json::to_value(&resp).expect("serialize");
    assert_error_shape(&v, "some error");
    assert_eq!(v["kind"], "other");
}

#[test]
fn json_err_data_is_absent_not_null() {
    // `data` must be missing (skip_serializing_if), not serialized as null.
    let resp = DaemonResponse::err("any error");
    let raw = serde_json::to_string(&resp).expect("serialize");
    assert!(
        !raw.contains("\"data\""),
        "data key must be absent in error JSON; got: {raw}"
    );
}

#[test]
fn json_ok_error_is_absent_not_null() {
    let resp = DaemonResponse::ok(json!({"ok": true}));
    let raw = serde_json::to_string(&resp).expect("serialize");
    assert!(
        !raw.contains("\"error\""),
        "error key must be absent in success JSON; got: {raw}"
    );
}

// ---------------------------------------------------------------------------
// Binary CLI golden tests (no browser needed)
// ---------------------------------------------------------------------------

#[test]
fn help_contains_all_builtin_subcommands() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("run camoufox --help");

    // `--help` exits 0.
    assert_eq!(out.status.code(), Some(0), "--help must exit 0");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let commands_section = stdout
        .split("Commands:\n")
        .nth(1)
        .expect("help text must contain Commands: section");
    let commands_text = commands_section
        .split("\n\n")
        .next()
        .unwrap_or(commands_section);

    let found_subcommands: std::collections::BTreeSet<String> = commands_text
        .lines()
        .filter_map(|line| {
            let token = line.split_whitespace().next()?;
            Some(token.to_string())
        })
        .collect();

    let expected_subcommands: std::collections::BTreeSet<String> = [
        "serve",
        "record",
        "replay",
        "launch",
        "dialog",
        "list",
        "stop",
        "new-page",
        "navigate",
        "evaluate",
        "click",
        "screenshot",
        "shutdown",
        "ping",
        "cookies",
        "text",
        "html",
        "links",
        "data",
        "url",
        "back",
        "forward",
        "reload",
        "wait",
        "cookie",
        "header",
        "fill",
        "upload",
        "type",
        "press",
        "hover",
        "select",
        "scroll",
        "tabs",
        "close-tab",
        "downloads",
        "help",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert_eq!(found_subcommands, expected_subcommands);
}

#[test]
fn mistyped_subcommand_exits_2_with_error_on_stderr() {
    let out = Command::new(bin())
        .arg("navgate")
        .output()
        .expect("run camoufox navgate");

    assert_eq!(
        out.status.code(),
        Some(2),
        "mistyped subcommand must exit with code 2"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Pin the exact current error text from clap.
    assert!(
        stderr.contains("unrecognized subcommand 'navgate'"),
        "stderr must contain the unrecognized-subcommand message; got:\n{stderr}"
    );
    // Stdout must be empty for a parse error.
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty for a parse error"
    );
}

#[test]
fn mistyped_subcommand_stderr_exact_golden() {
    // Pin the complete stderr so any clap version bump or message change is caught.
    let out = Command::new(bin())
        .arg("navgate")
        .output()
        .expect("run camoufox navgate");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // The binary name in the usage line may vary; normalise it.
    let normalised = stderr.replace(bin(), "camoufox");

    let expected = "error: unrecognized subcommand 'navgate'\n\n  tip: some similar subcommands exist: 'evaluate', 'navigate'\n\nUsage: camoufox [OPTIONS] <COMMAND>\n\nFor more information, try '--help'.\n";

    assert_eq!(
        normalised, expected,
        "stderr golden mismatch for mistyped subcommand"
    );
}

#[test]
fn ping_nonexistent_socket_parent_exits_1_stderr() {
    // The socket parent directory does not exist — the client checks this before
    // even trying to connect.
    let out = Command::new(bin())
        .args(["ping", "--socket", "/nonexistent/no.sock"])
        .output()
        .expect("run camoufox ping --socket /nonexistent/no.sock");

    assert_eq!(
        out.status.code(),
        Some(1),
        "exit code must be 1 for connection error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.trim(),
        "error: socket parent directory does not exist: /nonexistent",
        "stderr golden mismatch for missing socket parent"
    );
    assert!(out.stdout.is_empty(), "stdout must be empty");
}

#[test]
fn ping_nonexistent_socket_parent_json_shape() {
    let out = Command::new(bin())
        .args(["--json", "ping", "--socket", "/nonexistent/no.sock"])
        .output()
        .expect("run camoufox --json ping --socket /nonexistent/no.sock");

    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    assert_error_shape(&v, "socket parent directory does not exist: /nonexistent");
    assert_eq!(v["kind"], "daemon");
}

#[test]
fn ping_daemon_not_running_exits_1_stderr() {
    // Socket parent exists (temp dir) but no daemon is listening.
    let tmp = tempfile::tempdir().expect("create tempdir");
    // tmp is 0o700 by default on most systems; ensure it.
    std::fs::set_permissions(
        tmp.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .ok();
    let sock = tmp.path().join("d.sock");

    let out = Command::new(bin())
        .args(["ping", "--socket", sock.to_str().unwrap()])
        .output()
        .expect("run camoufox ping --socket <missing>");

    assert_eq!(
        out.status.code(),
        Some(1),
        "exit code must be 1 when daemon is not running"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.trim(),
        "error: daemon is not running (start with: camoufox serve)",
        "stderr golden mismatch for daemon-not-running"
    );
}

#[test]
fn ping_daemon_not_running_json_shape() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    std::fs::set_permissions(
        tmp.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .ok();
    let sock = tmp.path().join("d.sock");

    let out = Command::new(bin())
        .args(["--json", "ping", "--socket", sock.to_str().unwrap()])
        .output()
        .expect("run camoufox --json ping --socket <missing>");

    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    assert_error_shape(&v, "daemon is not running (start with: camoufox serve)");
    assert_eq!(v["kind"], "daemon");
}

/// Run `camoufox <args>` from a working directory that is deleted first.
fn run_in_removed_cwd(args: &[&str]) -> std::process::Output {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cwd = tmp.path().join("gone");
    std::fs::create_dir(&cwd).unwrap();
    Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cd "$1" && rmdir "$1" && shift && exec "$@""#)
        .arg("sh")
        .arg(&cwd)
        .arg(bin())
        .args(args)
        .output()
        .expect("run camoufox in a removed cwd")
}

#[test]
fn upload_relative_path_with_removed_cwd_is_io_error() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let sock = tmp.path().join("d.sock");
    let sock = sock.to_str().unwrap();
    let upload = ["upload", "i", "p1", "#file", "fixture.pdf"];

    let out = run_in_removed_cwd(&[&["--socket", sock][..], &upload].concat());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: io error: resolve upload path: "),
        "{stderr}"
    );

    let out = run_in_removed_cwd(&[&["--json", "--socket", sock][..], &upload].concat());
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .starts_with("io error: resolve upload path: "));
    assert_eq!(v["kind"], "io");

    // Absolute paths never consult the cwd and reach the (absent) daemon.
    let out = run_in_removed_cwd(&[
        "--json",
        "--socket",
        sock,
        "upload",
        "i",
        "p1",
        "#file",
        "/fixture.pdf",
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    assert_error_shape(&v, "daemon is not running (start with: camoufox serve)");
}

// ---------------------------------------------------------------------------
// Browser integration goldens (ignored; require Camoufox)
// ---------------------------------------------------------------------------

/// A minimal HTTP server for browser tests that serves a small fixture page.
struct RegressionFixtureServer {
    pub url: String,
    _server: Arc<Server>,
}

impl RegressionFixtureServer {
    fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind regression fixture server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        const PAGE: &[u8] = br#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Regression Fixture</title></head>
<body>
  <h1>Regression Fixture</h1>
  <p id="p">Hello from fixture.</p>
  <input id="inp" type="text">
  <select id="sel">
    <option value="a">Alpha</option>
    <option value="b">Beta</option>
  </select>
  <button id="btn" onclick="document.getElementById('p').textContent='clicked'">Click me</button>
  <div id="log"></div>
  <script>
    function log(m) { document.getElementById('log').textContent += m + '\n'; }
    document.getElementById('sel').addEventListener('change', function(e) {
      log('change:' + e.target.value);
    });
  </script>
</body>
</html>"#;

        let worker = Arc::clone(&server);
        thread::spawn(move || {
            for req in worker.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(PAGE.to_vec()),
                    Some(PAGE.len()),
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        RegressionFixtureServer {
            url,
            _server: server,
        }
    }
}

/// Extract `-profile <path>` from `/proc/<pid>/cmdline` on Linux.
fn extract_profile_dir_from_cmdline(pid: u64) -> Option<std::path::PathBuf> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter_map(|s| String::from_utf8(s.to_vec()).ok())
        .collect();
    let pos = args.iter().position(|a| a == "-profile")?;
    args.get(pos + 1).map(std::path::PathBuf::from)
}

/// A drop guard that sends `shutdown` to the daemon via the binary when dropped.
struct DaemonGuard {
    sock: std::path::PathBuf,
    child: std::process::Child,
    log_path: std::path::PathBuf,
}

impl DaemonGuard {
    /// Start `camoufox serve` and wait until it is ready.
    fn start(rt_dir: &std::path::Path) -> Self {
        let sock = rt_dir.join("camoufox").join("daemon.sock");
        let log_path = rt_dir.join("daemon_stderr.log");
        let log_file = std::fs::File::create(&log_path).expect("create daemon log file");
        let child = Command::new(bin())
            .arg("serve")
            .env("XDG_RUNTIME_DIR", rt_dir)
            .env("RUST_LOG", "camoufox::protocol=trace")
            .stdout(Stdio::null())
            .stderr(log_file)
            .spawn()
            .expect("spawn camoufox serve");

        // Poll until the socket appears (up to 10 s).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if sock.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(sock.exists(), "daemon socket did not appear within 10s");

        DaemonGuard {
            sock,
            child,
            log_path,
        }
    }

    /// Read the captured daemon stderr log content.
    fn log_content(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    /// Run the CLI binary against this daemon.
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(bin())
            .arg("--socket")
            .arg(&self.sock)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run camoufox {:?}: {e}", args))
    }

    /// Run the CLI binary in JSON mode against this daemon.
    fn run_json(&self, args: &[&str]) -> Value {
        let mut full_args = vec!["--json"];
        full_args.extend_from_slice(args);
        let out = self.run(&full_args);
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("JSON parse failed for {:?}: {e}", args))
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // Best-effort shutdown; ignore errors (daemon may already be gone).
        let _ = Command::new(bin())
            .arg("--socket")
            .arg(&self.sock)
            .arg("shutdown")
            .output();
        // Give the process up to 3 s to exit, then kill.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

#[test]
#[ignore]
fn browser_integration_full_session() {
    // End-to-end browser session: start `camoufox serve`, drive commands via the CLI binary,
    // assert exact plain-text stdout/stderr and JSON shapes.
    let rt_dir = tempfile::Builder::new()
        .prefix("camoufox-rt-")
        .tempdir()
        .expect("create temp rt dir");
    // Secure the runtime dir so the daemon accepts it.
    std::fs::set_permissions(
        rt_dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod rt dir");

    // Create the camoufox sub-dir the daemon expects (XDG_RUNTIME_DIR/camoufox/).
    std::fs::create_dir_all(rt_dir.path().join("camoufox")).expect("mkdir camoufox");
    std::fs::set_permissions(
        rt_dir.path().join("camoufox"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod camoufox dir");

    let server = RegressionFixtureServer::start();
    let daemon = DaemonGuard::start(rt_dir.path());

    // -----------------------------------------------------------------------
    // 1. launch (default temp profile — no --profile-dir flag)
    // -----------------------------------------------------------------------
    let launch_out = daemon.run(&["launch"]);
    assert_eq!(
        launch_out.status.code(),
        Some(0),
        "launch must exit 0; stderr: {}",
        String::from_utf8_lossy(&launch_out.stderr)
    );
    let launch_stdout = String::from_utf8_lossy(&launch_out.stdout);
    let instance_id = launch_stdout.trim().to_string();
    assert!(
        !instance_id.is_empty(),
        "launch must print instance_id on stdout"
    );
    let launch_stderr = String::from_utf8_lossy(&launch_out.stderr);
    assert!(
        launch_stderr.contains("version: Firefox/"),
        "launch stderr must report browser version; got: {launch_stderr}"
    );
    assert!(
        launch_stderr.contains("pid: "),
        "launch stderr must report browser pid; got: {launch_stderr}"
    );

    // Extract PID to find temporary profile directory via /proc/<pid>/cmdline.
    let first_pid: u64 = launch_stderr
        .lines()
        .find_map(|line| line.strip_prefix("pid: ")?.trim().parse().ok())
        .expect("launch stderr must report numeric pid");
    #[cfg(target_os = "linux")]
    let first_profile_dir = extract_profile_dir_from_cmdline(first_pid)
        .expect("temporary profile directory must be extractable from cmdline on linux");
    #[cfg(not(target_os = "linux"))]
    let first_profile_dir = extract_profile_dir_from_cmdline(first_pid);

    #[cfg(target_os = "linux")]
    assert!(
        first_profile_dir.exists(),
        "temporary profile directory {first_profile_dir:?} must exist while instance is running"
    );
    #[cfg(not(target_os = "linux"))]
    if let Some(ref pdir) = first_profile_dir {
        assert!(
            pdir.exists(),
            "temporary profile directory {pdir:?} must exist while instance is running"
        );
    }

    // JSON golden for launch.
    let launch_json = daemon.run_json(&["launch"]);
    assert_eq!(launch_json["ok"], true);
    assert!(launch_json["data"]["instance_id"].is_string());
    assert!(launch_json["data"]["version"].is_string());
    assert!(launch_json["data"]["pid"].is_number());
    assert!(launch_json.get("error").is_none());

    // Stop second instance and assert its temporary profile directory is cleaned up.
    let second_id = launch_json["data"]["instance_id"]
        .as_str()
        .unwrap()
        .to_string();
    let second_pid = launch_json["data"]["pid"].as_u64().unwrap();
    #[cfg(target_os = "linux")]
    let second_profile_dir = extract_profile_dir_from_cmdline(second_pid)
        .expect("second temporary profile directory must be extractable from cmdline on linux");
    #[cfg(not(target_os = "linux"))]
    let second_profile_dir = extract_profile_dir_from_cmdline(second_pid);

    #[cfg(target_os = "linux")]
    assert!(
        second_profile_dir.exists(),
        "second instance profile directory {second_profile_dir:?} must exist while running"
    );
    #[cfg(not(target_os = "linux"))]
    if let Some(ref pdir) = second_profile_dir {
        assert!(
            pdir.exists(),
            "second instance profile directory {pdir:?} must exist while running"
        );
    }

    let stop_second = daemon.run(&["stop", &second_id]);
    assert_eq!(
        stop_second.status.code(),
        Some(0),
        "stop of second instance must succeed"
    );
    assert_eq!(String::from_utf8_lossy(&stop_second.stdout).trim(), "ok");
    assert!(stop_second.stderr.is_empty());

    #[cfg(target_os = "linux")]
    assert!(
        !second_profile_dir.exists(),
        "second instance temporary profile dir {second_profile_dir:?} must no longer exist after stop"
    );
    #[cfg(not(target_os = "linux"))]
    if let Some(ref pdir) = second_profile_dir {
        assert!(
            !pdir.exists(),
            "second instance temporary profile dir {pdir:?} must no longer exist after stop"
        );
    }

    // -----------------------------------------------------------------------
    // 2. new-page
    // -----------------------------------------------------------------------
    let np_out = daemon.run(&["new-page", &instance_id]);
    assert_eq!(np_out.status.code(), Some(0), "new-page must exit 0");
    let np_stdout = String::from_utf8_lossy(&np_out.stdout);
    let page_id = np_stdout.trim().to_string();
    assert!(!page_id.is_empty(), "new-page must print page_id on stdout");
    assert!(np_out.stderr.is_empty(), "new-page stderr must be empty");

    // JSON golden for new-page.
    let np_json = daemon.run_json(&["new-page", &instance_id]);
    assert_eq!(np_json["ok"], true);
    assert!(np_json["data"]["page_id"].is_string());

    // -----------------------------------------------------------------------
    // 3. navigate to local fixture (plain + JSON)
    // -----------------------------------------------------------------------
    let nav_out = daemon.run(&["navigate", &instance_id, &page_id, &server.url]);
    assert_eq!(nav_out.status.code(), Some(0), "navigate must exit 0");
    let nav_stdout = String::from_utf8_lossy(&nav_out.stdout);
    assert!(
        nav_stdout.contains("ok (navigation_id:") && nav_stdout.contains("status=200"),
        "navigate stdout must be 'ok (navigation_id: ...) status=200'; got: {nav_stdout}"
    );
    assert!(nav_out.stderr.is_empty(), "navigate stderr must be empty");

    let nav_json = daemon.run_json(&["navigate", &instance_id, &page_id, &server.url]);
    assert_eq!(nav_json["ok"], true);
    assert!(
        nav_json["data"].get("navigation_id").is_some(),
        "navigate JSON must include navigation_id"
    );
    assert_eq!(nav_json["data"]["status_code"], 200);

    // -----------------------------------------------------------------------
    // 4. text
    // -----------------------------------------------------------------------
    let text_out = daemon.run(&["text", &instance_id, &page_id]);
    assert_eq!(text_out.status.code(), Some(0), "text must exit 0");
    let text_str = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        text_str.contains("Regression Fixture"),
        "text must contain page content; got: {text_str}"
    );
    assert!(
        text_str.contains("Hello from fixture."),
        "text must contain fixture paragraph; got: {text_str}"
    );
    assert!(!text_str.contains('<'), "text must not contain HTML markup");
    assert!(text_out.stderr.is_empty(), "text stderr must be empty");

    let text_json = daemon.run_json(&["text", &instance_id, &page_id]);
    assert_eq!(text_json["ok"], true);
    assert_eq!(text_json["data"]["text"], text_str.trim_end());

    // -----------------------------------------------------------------------
    // 5. fill
    // -----------------------------------------------------------------------
    let fill_out = daemon.run(&["fill", &instance_id, &page_id, "#inp", "test value"]);
    assert_eq!(fill_out.status.code(), Some(0), "fill must exit 0");
    let fill_stdout = String::from_utf8_lossy(&fill_out.stdout);
    let fill_parsed: Value =
        serde_json::from_str(&fill_stdout).expect("fill plain output must be JSON pretty-print");
    assert_eq!(fill_parsed["filled"], true);
    assert_eq!(fill_parsed["selector"], "#inp");
    assert_eq!(fill_parsed["tag"], "input");
    assert_eq!(fill_parsed["value"], "test value");
    assert!(fill_out.stderr.is_empty(), "fill stderr must be empty");

    let fill_json = daemon.run_json(&["fill", &instance_id, &page_id, "#inp", "test value"]);
    assert_eq!(fill_json["ok"], true);
    assert_eq!(fill_json["data"]["filled"], true);
    assert_eq!(fill_json["data"]["selector"], "#inp");
    assert_eq!(fill_json["data"]["tag"], "input");
    assert_eq!(fill_json["data"]["value"], "test value");

    // -----------------------------------------------------------------------
    // 6. select
    // -----------------------------------------------------------------------
    let sel_out = daemon.run(&["select", &instance_id, &page_id, "#sel", "b"]);
    assert_eq!(sel_out.status.code(), Some(0), "select must exit 0");
    let sel_stdout = String::from_utf8_lossy(&sel_out.stdout);
    let sel_parsed: Value =
        serde_json::from_str(&sel_stdout).expect("select plain output must be JSON pretty-print");
    assert_eq!(sel_parsed["selected"], true);
    assert_eq!(sel_parsed["selector"], "#sel");
    assert_eq!(sel_parsed["option_value"], "b");
    assert_eq!(sel_parsed["option_text"], "Beta");
    assert!(sel_out.stderr.is_empty(), "select stderr must be empty");

    let sel_json = daemon.run_json(&["select", &instance_id, &page_id, "#sel", "b"]);
    assert_eq!(sel_json["ok"], true);
    assert_eq!(sel_json["data"]["selected"], true);
    assert_eq!(sel_json["data"]["selector"], "#sel");
    assert_eq!(sel_json["data"]["option_value"], "b");
    assert_eq!(sel_json["data"]["option_text"], "Beta");

    // -----------------------------------------------------------------------
    // 7. click (by selector)
    // -----------------------------------------------------------------------
    let click_out = daemon.run(&["click", &instance_id, &page_id, "#btn"]);
    assert_eq!(click_out.status.code(), Some(0), "click must exit 0");
    let click_stdout = String::from_utf8_lossy(&click_out.stdout);
    let click_parsed: Value =
        serde_json::from_str(&click_stdout).expect("click plain output must be JSON pretty-print");
    assert_eq!(click_parsed["clicked"], true);
    assert_eq!(click_parsed["selector"], "#btn");
    assert!(click_parsed["x"].is_number());
    assert!(click_parsed["y"].is_number());
    assert!(click_out.stderr.is_empty(), "click stderr must be empty");

    let click_json = daemon.run_json(&["click", &instance_id, &page_id, "#btn"]);
    assert_eq!(click_json["ok"], true);
    assert_eq!(click_json["data"]["clicked"], true);
    assert_eq!(click_json["data"]["selector"], "#btn");
    assert!(click_json["data"]["x"].is_number());
    assert!(click_json["data"]["y"].is_number());

    // -----------------------------------------------------------------------
    // 8. evaluate — string result
    // -----------------------------------------------------------------------
    let eval_out = daemon.run(&["evaluate", &instance_id, &page_id, "document.title"]);
    assert_eq!(eval_out.status.code(), Some(0), "evaluate must exit 0");
    let eval_str = String::from_utf8_lossy(&eval_out.stdout);
    assert_eq!(
        eval_str.trim(),
        "Regression Fixture",
        "evaluate string result must be unquoted"
    );
    assert!(eval_out.stderr.is_empty(), "evaluate stderr must be empty");

    let eval_json = daemon.run_json(&["evaluate", &instance_id, &page_id, "document.title"]);
    assert_eq!(eval_json["ok"], true);
    assert_eq!(eval_json["data"]["result"], "Regression Fixture");

    // -----------------------------------------------------------------------
    // 9. evaluate — thrown Error
    // -----------------------------------------------------------------------
    let err_out = daemon.run(&[
        "evaluate",
        &instance_id,
        &page_id,
        "(() => { throw new Error('boom'); })()",
    ]);
    assert_eq!(err_out.status.code(), Some(1), "evaluate error must exit 1");
    assert!(
        err_out.stdout.is_empty(),
        "stdout must be empty on evaluate error"
    );
    let err_stderr = String::from_utf8_lossy(&err_out.stderr);
    assert!(
        err_stderr.starts_with("error: "),
        "evaluate error must write to stderr starting with 'error: '; got: {err_stderr}"
    );
    assert!(
        err_stderr.contains("boom"),
        "evaluate error stderr must reference the error; got: {err_stderr}"
    );

    let err_json = daemon.run_json(&[
        "evaluate",
        &instance_id,
        &page_id,
        "(() => { throw new Error('boom'); })()",
    ]);
    assert_eq!(err_json["ok"], false);
    let error_text = err_json["error"].as_str().unwrap_or("");
    assert_error_shape(&err_json, error_text);
    assert!(error_text.contains("boom"));

    // -----------------------------------------------------------------------
    // 10. Stop the instance and verify it disappears from list and profile dir is removed.
    // -----------------------------------------------------------------------
    let stop_out = daemon.run(&["stop", &instance_id]);
    assert_eq!(stop_out.status.code(), Some(0), "stop must exit 0");
    assert_eq!(String::from_utf8_lossy(&stop_out.stdout).trim(), "ok");
    assert!(stop_out.stderr.is_empty(), "stop stderr must be empty");

    let list_json = daemon.run_json(&["list"]);
    assert_eq!(list_json["ok"], true);
    let instances = list_json["data"]["instances"].as_array().unwrap();
    assert!(
        instances
            .iter()
            .all(|i| i["instance_id"].as_str() != Some(&instance_id)),
        "stopped instance must no longer appear in list"
    );

    #[cfg(target_os = "linux")]
    assert!(
        !first_profile_dir.exists(),
        "first instance temporary profile dir {first_profile_dir:?} must no longer exist after stop"
    );
    #[cfg(not(target_os = "linux"))]
    if let Some(ref pdir) = first_profile_dir {
        assert!(
            !pdir.exists(),
            "first instance temporary profile dir {pdir:?} must no longer exist after stop"
        );
    }

    // -----------------------------------------------------------------------
    // 11. Interception-off-by-default in active session
    // -----------------------------------------------------------------------
    let log = daemon.log_content();
    assert!(
        log.contains("TRACE camoufox::protocol] SEND"),
        "protocol logger must have logged outgoing messages; log was:\n{log}"
    );
    assert!(
        !log.contains("setRequestInterception"),
        "interception must be off by default; found setRequestInterception in log:\n{log}"
    );
}

#[test]
#[ignore]
fn browser_integration_evaluate_error_json_superset_rule() {
    // Verifies that a thrown Error from `evaluate` produces an error response
    // satisfying the superset rule: ok=false, error=<text>, data absent,
    // kind required string.
    let rt_dir = tempfile::Builder::new()
        .prefix("camoufox-rt2-")
        .tempdir()
        .expect("create temp rt dir");
    std::fs::set_permissions(
        rt_dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
    std::fs::create_dir_all(rt_dir.path().join("camoufox")).expect("mkdir camoufox");
    std::fs::set_permissions(
        rt_dir.path().join("camoufox"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod camoufox");

    let daemon = DaemonGuard::start(rt_dir.path());

    let launch_out = daemon.run_json(&["launch"]);
    assert_eq!(launch_out["ok"], true);
    let instance_id = launch_out["data"]["instance_id"]
        .as_str()
        .unwrap()
        .to_string();
    let np_out = daemon.run_json(&["new-page", &instance_id]);
    let page_id = np_out["data"]["page_id"].as_str().unwrap().to_string();

    let err_json = daemon.run_json(&[
        "evaluate",
        &instance_id,
        &page_id,
        "(() => { throw new Error('type-test'); })()",
    ]);
    assert_eq!(err_json["ok"], false);
    assert!(err_json.get("data").is_none());
    let error_text = err_json["error"]
        .as_str()
        .expect("error text must be string");
    assert!(
        error_text.contains("type-test"),
        "error text must reference the thrown error; got: {error_text}"
    );
    assert_eq!(err_json["kind"], "other");
    assert_error_shape(&err_json, error_text);

    let _ = daemon.run(&["stop", &instance_id]);
}

// ---------------------------------------------------------------------------
// Interception-off-by-default
// ---------------------------------------------------------------------------

/// Interception-off-by-default verification:
/// Protocol messages are logged via `src/obs/protocol_log.rs` at TRACE level
/// under target `camoufox::protocol`. In integration tests, starting `camoufox serve`
/// with RUST_LOG=camoufox::protocol=trace routes protocol SEND/RECV logs to stderr.
/// `browser_integration_interception_off_by_default` and `browser_integration_full_session`
/// assert that no `setRequestInterception` command is sent during launch and navigation.
#[test]
#[ignore]
fn browser_integration_interception_off_by_default() {
    let rt_dir = tempfile::Builder::new()
        .prefix("camoufox-rt-obs-")
        .tempdir()
        .expect("create temp rt dir");
    std::fs::set_permissions(
        rt_dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
    std::fs::create_dir_all(rt_dir.path().join("camoufox")).expect("mkdir camoufox");
    std::fs::set_permissions(
        rt_dir.path().join("camoufox"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod camoufox");

    let server = RegressionFixtureServer::start();
    let daemon = DaemonGuard::start(rt_dir.path());

    let launch_json = daemon.run_json(&["launch"]);
    assert_eq!(launch_json["ok"], true);
    let instance_id = launch_json["data"]["instance_id"]
        .as_str()
        .unwrap()
        .to_string();

    let np_json = daemon.run_json(&["new-page", &instance_id]);
    assert_eq!(np_json["ok"], true);
    let page_id = np_json["data"]["page_id"].as_str().unwrap().to_string();

    let nav_out = daemon.run(&["navigate", &instance_id, &page_id, &server.url]);
    assert_eq!(nav_out.status.code(), Some(0));

    let log = daemon.log_content();
    assert!(
        log.contains("TRACE camoufox::protocol] SEND"),
        "protocol trace logger must have captured outgoing messages"
    );
    assert!(
        !log.contains("setRequestInterception"),
        "interception must be off by default (found setRequestInterception in protocol log)"
    );

    let _ = daemon.run(&["stop", &instance_id]);
}

/// `camoufox launch --profile-dir` through the real binary and daemon: the
/// flag is forwarded, `list` reports the canonical path in both output modes,
/// aliases collide, and the directory outlives the instance.
#[test]
#[ignore]
fn browser_cli_launch_profile_dir_is_listed_and_survives_stop() {
    use std::os::unix::fs::PermissionsExt;
    let rt_dir = tempfile::Builder::new()
        .prefix("camoufox-rt-")
        .tempdir()
        .expect("create temp rt dir");
    std::fs::set_permissions(rt_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(rt_dir.path().join("camoufox")).unwrap();
    std::fs::set_permissions(
        rt_dir.path().join("camoufox"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let daemon = DaemonGuard::start(rt_dir.path());

    let profiles = tempfile::tempdir().unwrap();
    let profile = profiles.path().join("nested").join("profile");
    let profile_arg = profile.to_str().unwrap();

    // The daemon (not the client) enforces absolute paths.
    let relative = daemon.run_json(&["launch", "--profile-dir", "relative/profile"]);
    assert_error_shape(
        &relative,
        "invalid argument: --profile-dir must be an absolute path",
    );
    assert_eq!(relative["kind"], "invalid_argument");

    let launched = daemon.run_json(&["launch", "--profile-dir", profile_arg]);
    assert_eq!(launched["ok"], true, "{launched}");
    let instance = launched["data"]["instance_id"].as_str().unwrap().to_owned();
    let canonical = profile.canonicalize().unwrap();
    let canonical = canonical.to_str().unwrap();
    assert_eq!(
        std::fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
        0o700
    );

    let listed = daemon.run_json(&["list"]);
    assert_eq!(listed["data"]["instances"][0]["profile_dir"], canonical);
    let plain = daemon.run(&["list"]);
    assert!(plain.status.success());
    let stdout = String::from_utf8_lossy(&plain.stdout);
    assert!(
        stdout.starts_with(&instance)
            && stdout
                .trim_end()
                .ends_with(&format!("  profile_dir={canonical}")),
        "{stdout}"
    );

    // A symlink alias resolves to the same directory and is refused.
    let alias = profiles.path().join("alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    let second = daemon.run_json(&["launch", "--profile-dir", alias.to_str().unwrap()]);
    assert_error_shape(
        &second,
        &format!("invalid argument: profile directory already in use by instance {instance}"),
    );
    assert_eq!(second["kind"], "invalid_argument");

    let stopped = daemon.run(&["stop", &instance]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(daemon.run_json(&["list"])["data"]["instances"], json!([]));
    assert!(
        profile.join("prefs.js").is_file(),
        "profile must survive stop"
    );
}
