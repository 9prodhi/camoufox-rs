//! Output formatting for CLI responses.

use serde_json::Value;

use crate::cli::ipc::DaemonResponse;

/// The human-readable rendering of a response: what goes to stdout, and the
/// supplementary lines that go to stderr (so stdout stays pipe-friendly).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Primary output. Printed to stdout, one `println!`.
    pub stdout: String,
    /// Supplementary context lines. Printed to stderr, one per line.
    pub stderr: Vec<String>,
}

impl Rendered {
    fn out(stdout: impl Into<String>) -> Self {
        Rendered {
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.stderr.push(note.into());
        self
    }
}

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

    let rendered = render(response.data.as_ref());
    println!("{}", rendered.stdout);
    for line in rendered.stderr {
        eprintln!("{line}");
    }
}

/// Render a successful response's `data` payload into human-readable text.
///
/// Dispatch is by *data shape*: each command's payload carries a key no other
/// command uses, and the more specific keys are tested first. Kept pure so the
/// exact rendering can be asserted in tests.
pub fn render(data: Option<&Value>) -> Rendered {
    let Some(data) = data else {
        return Rendered::out("ok");
    };

    // -----------------------------------------------------------------------
    // Reading / interaction commands.
    //
    // Checked before the older, more generic shapes below: `data --og` also
    // carries `url`/`title`, and a `select` result also carries a `text`.
    // -----------------------------------------------------------------------

    // Structured metadata (`data --og/--jsonld/--meta`).
    if data.get("og").is_some() || data.get("jsonld").is_some() || data.get("meta").is_some() {
        return Rendered::out(pretty(data));
    }

    // `text`
    if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
        return Rendered::out(text);
    }

    // `html`
    if let Some(html) = data.get("html").and_then(|v| v.as_str()) {
        return Rendered::out(html);
    }

    // `links` — one `text → href` per line, count on stderr.
    if let Some(links) = data.get("links").and_then(|v| v.as_array()) {
        let lines: Vec<String> = links
            .iter()
            .map(|link| {
                let text = link.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let href = link.get("href").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    href.to_string()
                } else {
                    format!("{text} → {href}")
                }
            })
            .collect();
        return Rendered::out(lines.join("\n")).with_note(format!("{} link(s)", links.len()));
    }

    // `tabs`
    if let Some(tabs) = data.get("tabs").and_then(|v| v.as_array()) {
        if tabs.is_empty() {
            return Rendered::out("no open pages");
        }
        let lines: Vec<String> = tabs
            .iter()
            .map(|tab| {
                let page_id = tab.get("page_id").and_then(|v| v.as_str()).unwrap_or("?");
                let url = tab.get("url").and_then(|v| v.as_str()).unwrap_or("-");
                let title = tab.get("title").and_then(|v| v.as_str()).unwrap_or("");
                format!("{page_id}  {url}  {title}")
            })
            .collect();
        return Rendered::out(lines.join("\n"));
    }

    // `url` — the URL on stdout, the title as a stderr note.
    if let (Some(url), Some(title)) = (
        data.get("url").and_then(|v| v.as_str()),
        data.get("title").and_then(|v| v.as_str()),
    ) {
        return Rendered::out(url).with_note(format!("title: {title}"));
    }

    // `back` / `forward`
    if let Some(direction) = data.get("direction").and_then(|v| v.as_str()) {
        let navigated = data
            .get("navigated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        return Rendered::out(if navigated {
            format!("ok ({direction})")
        } else {
            format!("no {direction} history entry")
        });
    }

    // `wait`
    if let Some(waited) = data.get("waited_ms").and_then(|v| v.as_u64()) {
        let selector = data.get("selector").and_then(|v| v.as_str()).unwrap_or("");
        return Rendered::out(format!("found {selector} after {waited}ms"));
    }

    // -----------------------------------------------------------------------
    // Session / lifecycle commands.
    // -----------------------------------------------------------------------

    // Launch response
    if let Some(id) = data.get("instance_id").and_then(|v| v.as_str()) {
        let mut rendered = Rendered::out(id);
        if let Some(version) = data.get("version").and_then(|v| v.as_str()) {
            rendered = rendered.with_note(format!("version: {version}"));
        }
        if let Some(pid) = data.get("pid").and_then(|v| v.as_u64()) {
            rendered = rendered.with_note(format!("pid: {pid}"));
        }
        return rendered;
    }

    // NewPage response
    if let Some(page_id) = data.get("page_id").and_then(|v| v.as_str()) {
        return Rendered::out(page_id);
    }

    // List response
    if let Some(instances) = data.get("instances").and_then(|v| v.as_array()) {
        if instances.is_empty() {
            return Rendered::out("no running instances");
        }
        let lines: Vec<String> = instances
            .iter()
            .map(|inst| {
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
                format!("{id}  pid={pid}  version={version}  pages=[{pages}]")
            })
            .collect();
        return Rendered::out(lines.join("\n"));
    }

    // Evaluate response
    if let Some(result) = data.get("result") {
        return Rendered::out(match result {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }

    // Screenshot response
    if let Some(path) = data.get("path").and_then(|v| v.as_str()) {
        let bytes = data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        return Rendered::out(format!("{path} ({bytes} bytes)"));
    }

    // Ping response
    if let Some(count) = data.get("instance_count").and_then(|v| v.as_u64()) {
        return Rendered::out(format!("pong ({count} instances)"));
    }

    // Cookies response
    if let Some(cookies) = data.get("cookies").and_then(|v| v.as_array()) {
        let mut lines: Vec<String> = cookies
            .iter()
            .map(|cookie| {
                let name = cookie.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let value = cookie.get("value").and_then(|v| v.as_str()).unwrap_or("");
                format!("{name}={value}")
            })
            .collect();
        lines.push(format!("{} cookie(s)", cookies.len()));
        return Rendered::out(lines.join("\n"));
    }

    // Navigation response
    if let Some(nav_id) = data.get("navigation_id") {
        let status_str = data
            .get("status_code")
            .and_then(|v| v.as_u64())
            .map(|s| format!(" status={s}"))
            .unwrap_or_default();
        return Rendered::out(if nav_id.is_null() {
            format!("ok (same-document navigation){status_str}")
        } else if let Some(id) = nav_id.as_str() {
            format!("ok (navigation_id: {id}){status_str}")
        } else {
            format!("ok{status_str}")
        });
    }

    // Fallback: print the data as JSON.
    Rendered::out(pretty(data))
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ipc::DaemonResponse;
    use serde_json::json;

    /// `print_response` in JSON mode emits the full response including cookies
    /// with httpOnly preserved — verified by checking `to_string_pretty` output.
    #[test]
    fn json_mode_includes_http_only_cookies() {
        let resp = DaemonResponse::ok(json!({
            "cookies": [
                { "name": "session", "value": "abc", "httpOnly": false },
                { "name": "PHPSESSID", "value": "secret", "httpOnly": true }
            ]
        }));
        let serialized = serde_json::to_string_pretty(&resp).expect("serialize");
        assert!(serialized.contains("\"session\""), "session cookie present");
        assert!(
            serialized.contains("\"PHPSESSID\""),
            "PHPSESSID cookie present"
        );
        assert!(
            serialized.contains("\"httpOnly\": true"),
            "httpOnly:true preserved in JSON output"
        );
    }

    /// Human mode lists every cookie (HttpOnly included) plus a count line.
    #[test]
    fn human_mode_renders_all_cookies_with_count() {
        let data = json!({
            "cookies": [
                { "name": "session", "value": "abc", "httpOnly": false },
                { "name": "PHPSESSID", "value": "secret", "httpOnly": true }
            ]
        });
        let rendered = render(Some(&data));
        assert_eq!(
            rendered.stdout,
            "session=abc\nPHPSESSID=secret\n2 cookie(s)"
        );
    }

    /// `print_response` in JSON mode for an error response does not panic.
    #[test]
    fn json_mode_error_response_does_not_panic() {
        let resp = DaemonResponse::err("instance not found");
        print_response(&resp, true);
    }

    /// A `text` payload renders as the bare text — no quoting, no JSON.
    #[test]
    fn text_renders_bare() {
        let data = json!({ "text": "Example Domain\n\nHello" });
        assert_eq!(render(Some(&data)).stdout, "Example Domain\n\nHello");
    }

    /// Links render as `text → href`, one per line, with the count on stderr
    /// so stdout stays pipe-friendly.
    #[test]
    fn links_render_as_text_arrow_href() {
        let data = json!({ "links": [
            { "text": "One", "href": "https://example.com/one" },
            { "text": "", "href": "https://example.com/two" }
        ]});
        let rendered = render(Some(&data));
        assert_eq!(
            rendered.stdout,
            "One → https://example.com/one\nhttps://example.com/two"
        );
        assert_eq!(rendered.stderr, vec!["2 link(s)".to_string()]);
    }

    /// `url` puts the URL on stdout and the title on stderr.
    #[test]
    fn url_renders_url_on_stdout_title_on_stderr() {
        let data = json!({ "url": "https://example.com/", "title": "Example Domain" });
        let rendered = render(Some(&data));
        assert_eq!(rendered.stdout, "https://example.com/");
        assert_eq!(rendered.stderr, vec!["title: Example Domain".to_string()]);
    }

    /// A `data --og` payload also carries `url` and `title`, but must render as
    /// the full JSON object — not be mistaken for a `url` response.
    #[test]
    fn structured_data_is_not_rendered_as_url() {
        let data = json!({
            "og": { "og:title": "T" },
            "title": "T",
            "url": "https://example.com/"
        });
        let rendered = render(Some(&data));
        assert!(
            rendered.stdout.contains("\"og:title\""),
            "og block rendered: {}",
            rendered.stdout
        );
        assert!(rendered.stderr.is_empty(), "no title note for `data`");
    }

    /// A `select` result carries an option label, which must not be mistaken
    /// for a page-`text` payload.
    #[test]
    fn select_result_is_not_rendered_as_page_text() {
        let data = json!({
            "selected": true,
            "selector": "#sel",
            "option_value": "c",
            "option_text": "Gamma"
        });
        let rendered = render(Some(&data));
        assert!(
            rendered.stdout.contains("\"selected\""),
            "select renders as JSON: {}",
            rendered.stdout
        );
    }

    /// `back`/`forward` report whether a history entry existed.
    #[test]
    fn back_renders_history_outcome() {
        let moved = json!({ "navigated": true, "direction": "back" });
        assert_eq!(render(Some(&moved)).stdout, "ok (back)");
        let stuck = json!({ "navigated": false, "direction": "forward" });
        assert_eq!(render(Some(&stuck)).stdout, "no forward history entry");
    }

    /// `wait` reports how long it waited.
    #[test]
    fn wait_renders_elapsed() {
        let data = json!({ "found": true, "selector": "#late", "waited_ms": 1061 });
        assert_eq!(render(Some(&data)).stdout, "found #late after 1061ms");
    }

    /// `tabs` renders one line per page; an empty list says so.
    #[test]
    fn tabs_render_one_line_per_page() {
        let data = json!({ "tabs": [
            { "page_id": "p1", "url": "https://example.com/", "title": "Example Domain" }
        ]});
        assert_eq!(
            render(Some(&data)).stdout,
            "p1  https://example.com/  Example Domain"
        );
        assert_eq!(render(Some(&json!({ "tabs": [] }))).stdout, "no open pages");
    }

    /// Pre-existing shapes keep rendering exactly as before.
    #[test]
    fn existing_shapes_are_unchanged() {
        assert_eq!(render(None).stdout, "ok");
        assert_eq!(
            render(Some(&json!({ "instance_count": 2 }))).stdout,
            "pong (2 instances)"
        );
        assert_eq!(render(Some(&json!({ "page_id": "p1" }))).stdout, "p1");
        assert_eq!(
            render(Some(&json!({ "result": "hello" }))).stdout,
            "hello",
            "string evaluate results print unquoted"
        );
        assert_eq!(render(Some(&json!({ "result": 42 }))).stdout, "42");
        assert_eq!(
            render(Some(&json!({ "path": "/tmp/a.png", "bytes": 30447 }))).stdout,
            "/tmp/a.png (30447 bytes)"
        );
        assert_eq!(
            render(Some(
                &json!({ "navigation_id": "nav-13", "status_code": 200 })
            ))
            .stdout,
            "ok (navigation_id: nav-13) status=200"
        );
        assert_eq!(
            render(Some(&json!({ "navigation_id": null }))).stdout,
            "ok (same-document navigation)"
        );

        let launch = render(Some(
            &json!({ "instance_id": "00000001", "version": "Firefox/150", "pid": 42 }),
        ));
        assert_eq!(launch.stdout, "00000001");
        assert_eq!(launch.stderr, vec!["version: Firefox/150", "pid: 42"]);

        assert_eq!(
            render(Some(&json!({ "instances": [] }))).stdout,
            "no running instances"
        );
    }
}
