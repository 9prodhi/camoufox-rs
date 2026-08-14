//! Local two-port HTTP fixture server for integration tests.
//!
//! Bind a `FixtureServer` on `127.0.0.1` to serve `main.html` and
//! `iframe.html` on two different ports. Different ports on the same host
//! count as different origins, which forces Camoufox onto the OOPIF code
//! path — the same path Amazon's ad-pixel iframe uses in production.
//!
//! The main page is served with a 50 ms delay so the iframe target wins the
//! `Browser.attachedToTarget` race; this is the exact race condition the
//! Layer-1 fix needs to handle.

use std::io::Cursor;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tiny_http::{Header, Response, Server};

const MAIN_HTML_TEMPLATE: &str = include_str!("main.html");
const IFRAME_HTML: &str = include_str!("iframe.html");
const MAIN_DELAY: Duration = Duration::from_millis(50);

pub struct FixtureServer {
    /// `http://127.0.0.1:<main_port>/`
    pub main_url: String,
    /// `http://127.0.0.1:<iframe_port>/`
    #[allow(dead_code)]
    pub iframe_url: String,
    _main_server: Arc<Server>,
    _iframe_server: Arc<Server>,
}

impl FixtureServer {
    /// Start both servers on ephemeral ports.
    pub fn start() -> Self {
        let main_server = Arc::new(Server::http("127.0.0.1:0").expect("bind main server"));
        let iframe_server = Arc::new(Server::http("127.0.0.1:0").expect("bind iframe server"));

        let main_port = main_server.server_addr().to_ip().unwrap().port();
        let iframe_port = iframe_server.server_addr().to_ip().unwrap().port();

        let main_url = format!("http://127.0.0.1:{main_port}/");
        let iframe_url = format!("http://127.0.0.1:{iframe_port}/");

        // Main page: 50 ms delay, then HTML with the iframe URL substituted.
        let main_html = MAIN_HTML_TEMPLATE.replace("__IFRAME_URL__", &iframe_url);
        let main_server_clone = Arc::clone(&main_server);
        thread::spawn(move || {
            for req in main_server_clone.incoming_requests() {
                thread::sleep(MAIN_DELAY);
                let body = main_html.clone();
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(body.into_bytes()),
                    None,
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        // Iframe page: served immediately, no delay.
        let iframe_server_clone = Arc::clone(&iframe_server);
        thread::spawn(move || {
            for req in iframe_server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(IFRAME_HTML.as_bytes().to_vec()),
                    None,
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        FixtureServer {
            main_url,
            iframe_url,
            _main_server: main_server,
            _iframe_server: iframe_server,
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        // tiny_http's Server stops accepting new connections when the Arc
        // is dropped; the worker threads exit on their next iteration.
        // No explicit shutdown call needed.
    }
}

/// A single-port HTTP server that returns a tiny PDF with
/// `Content-Disposition: attachment` — the SCI-style endpoint that
/// triggers the download-detection path.
pub struct AttachmentServer {
    pub url: String,
    _server: Arc<Server>,
}

impl AttachmentServer {
    /// Start the server on an ephemeral port. Every request to `/file.pdf`
    /// (or any path) gets back a minimal, valid PDF with
    /// `Content-Disposition: attachment` set.
    #[allow(dead_code)]
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind attachment server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/file.pdf");

        // Smallest plausible PDF body. Camoufox never needs to render it —
        // the renderer routes it into the download flow before parsing.
        const PDF_BYTES: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n%%EOF\n";

        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![
                        Header::from_bytes(&b"Content-Type"[..], &b"application/pdf"[..]).unwrap(),
                        Header::from_bytes(
                            &b"Content-Disposition"[..],
                            &b"attachment; filename=\"file.pdf\""[..],
                        )
                        .unwrap(),
                    ],
                    Cursor::new(PDF_BYTES.to_vec()),
                    Some(PDF_BYTES.len()),
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        AttachmentServer {
            url,
            _server: server,
        }
    }
}

impl Drop for AttachmentServer {
    fn drop(&mut self) {}
}

/// A single-port HTTP server that sets one normal cookie and one `HttpOnly`
/// cookie via `Set-Cookie` response headers, then returns a minimal HTML page.
///
/// This fixture is used by the `cookies_command_returns_http_only_cookies`
/// integration test to verify that `Browser.getCookies` surfaces HttpOnly
/// cookies alongside ordinary cookies.
pub struct CookieServer {
    /// URL of the page that sets the cookies.
    pub url: String,
    _server: Arc<Server>,
}

impl CookieServer {
    /// Start the server on an ephemeral port. Every request receives a 200
    /// response with two `Set-Cookie` headers: one plain cookie and one
    /// `HttpOnly` cookie.
    #[allow(dead_code)]
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind cookie server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        const HTML: &[u8] = b"<html><body>cookie-setter</body></html>";

        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![
                        Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap(),
                        // Normal cookie (readable by JS).
                        Header::from_bytes(&b"Set-Cookie"[..], &b"normal_cookie=hello; Path=/"[..])
                            .unwrap(),
                        // HttpOnly cookie (not readable by JS, but returned by getCookies).
                        Header::from_bytes(
                            &b"Set-Cookie"[..],
                            &b"http_only_cookie=secret; Path=/; HttpOnly"[..],
                        )
                        .unwrap(),
                    ],
                    Cursor::new(HTML.to_vec()),
                    Some(HTML.len()),
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        CookieServer {
            url,
            _server: server,
        }
    }
}

impl Drop for CookieServer {
    fn drop(&mut self) {}
}

/// A single-port HTTP server that serves a page with a **deferred resource**.
///
/// The page HTML contains a `<script src="/slow.js" defer></script>` where
/// `/slow.js` is served after a 200 ms delay. The `load` event therefore fires
/// only AFTER `slow.js` has been delivered (browsers wait for all deferred
/// scripts before firing `load`). The slow script inserts a DOM marker element:
///
/// ```html
/// <div id="load-marker">loaded</div>
/// ```
///
/// Integration tests can navigate to `LifecycleServer::url` with
/// `--wait-until=load` and then assert `document.getElementById('load-marker')`
/// is non-null — proving the caller did not return until after `load`.
pub struct LifecycleServer {
    /// URL of the page that requires a deferred resource before `load`.
    pub url: String,
    _server: Arc<Server>,
}

impl LifecycleServer {
    /// Start on an ephemeral port.
    #[allow(dead_code)]
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind lifecycle server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        // The main page references /slow.js as a defer script.
        // On load, slow.js creates a div#load-marker.
        let main_html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
  <title>Lifecycle Test</title>
  <script src="http://127.0.0.1:{port}/slow.js" defer></script>
</head>
<body>
  <div id="before-load">before load</div>
</body>
</html>"#
        );

        // slow.js inserts the marker that proves load has fired.
        const SLOW_JS: &str = r#"
var d = document.createElement('div');
d.id = 'load-marker';
d.textContent = 'loaded';
document.body.appendChild(d);
"#;

        let main_html_bytes = main_html.into_bytes();
        let slow_js_bytes = SLOW_JS.as_bytes().to_vec();

        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let path = req.url().to_owned();
                if path.contains("slow.js") {
                    // Delay to ensure load fires AFTER navigation ack.
                    thread::sleep(Duration::from_millis(200));
                    let resp = Response::new(
                        200.into(),
                        vec![Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/javascript"[..],
                        )
                        .unwrap()],
                        Cursor::new(slow_js_bytes.clone()),
                        Some(slow_js_bytes.len()),
                        None,
                    );
                    let _ = req.respond(resp);
                } else {
                    // Main page: served immediately.
                    let resp = Response::new(
                        200.into(),
                        vec![Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"text/html; charset=utf-8"[..],
                        )
                        .unwrap()],
                        Cursor::new(main_html_bytes.clone()),
                        Some(main_html_bytes.len()),
                        None,
                    );
                    let _ = req.respond(resp);
                }
            }
        });

        LifecycleServer {
            url,
            _server: server,
        }
    }
}

impl Drop for LifecycleServer {
    fn drop(&mut self) {}
}

/// A single-port HTTP server with two routes for testing status-code capture:
///
/// - `GET /200` → HTTP 200 with a minimal HTML body.
/// - `GET /404` → HTTP 404 with a minimal HTML body.
/// - `GET /redirect` → HTTP 302 to `/200` (exercises redirect-chain status).
/// - Any other path → HTTP 200 (treated as the "base" URL).
///
/// Used by the G4 integration tests
/// (`navigate_reports_main_document_status_code`,
/// `navigate_reports_final_status_after_redirect`).
pub struct StatusServer {
    /// Base URL: `http://127.0.0.1:<port>/`
    #[allow(dead_code)]
    pub base_url: String,
    /// URL that returns 200.
    pub url_200: String,
    /// URL that returns 404.
    pub url_404: String,
    /// URL that returns 302 → `/200` (redirect chain).
    pub url_redirect: String,
    _server: Arc<Server>,
}

impl StatusServer {
    /// Start on an ephemeral port.
    #[allow(dead_code)]
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind status server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}/");
        let url_200 = format!("http://127.0.0.1:{port}/200");
        let url_404 = format!("http://127.0.0.1:{port}/404");
        let url_redirect = format!("http://127.0.0.1:{port}/redirect");

        const HTML_200: &[u8] = b"<html><body>200 OK</body></html>";
        const HTML_404: &[u8] = b"<html><body>404 Not Found</body></html>";

        let redirect_target = format!("http://127.0.0.1:{port}/200");
        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let path = req.url().to_owned();
                if path.contains("/redirect") {
                    // 302 Found → /200. The browser follows it, so the final
                    // main-document status must be 200, not 302.
                    let resp = Response::new(
                        302.into(),
                        vec![
                            Header::from_bytes(&b"Location"[..], redirect_target.as_bytes())
                                .unwrap(),
                            Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap(),
                        ],
                        Cursor::new(Vec::new()),
                        Some(0),
                        None,
                    );
                    let _ = req.respond(resp);
                } else if path.contains("/404") {
                    let resp = Response::new(
                        404.into(),
                        vec![Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap()],
                        Cursor::new(HTML_404.to_vec()),
                        Some(HTML_404.len()),
                        None,
                    );
                    let _ = req.respond(resp);
                } else {
                    let resp = Response::new(
                        200.into(),
                        vec![Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap()],
                        Cursor::new(HTML_200.to_vec()),
                        Some(HTML_200.len()),
                        None,
                    );
                    let _ = req.respond(resp);
                }
            }
        });

        StatusServer {
            base_url,
            url_200,
            url_404,
            url_redirect,
            _server: server,
        }
    }
}

impl Drop for StatusServer {
    fn drop(&mut self) {}
}

/// A single-port HTTP server that serves a page with a full-viewport
/// `#target` div and a capture-phase click listener pre-armed in the page.
/// The listener records the most recent click into `window.__click_result`
/// as `{ trusted, x, y, tag, id }`.
///
/// Used by the trusted-click integration test
/// (`click_command_dispatches_trusted_event_at_coordinates`) to prove the
/// `click` command dispatches a genuinely trusted event (`isTrusted == true`)
/// at the requested viewport coordinates — the property that lets it drive
/// bot-screen widgets like Cloudflare Turnstile, which reject synthetic
/// (untrusted) events. Gated on `cli` because its only consumer is the
/// cli-feature test.
#[cfg(feature = "cli")]
pub struct ClickServer {
    /// URL of the click-target page.
    pub url: String,
    _server: Arc<Server>,
}

#[cfg(feature = "cli")]
impl ClickServer {
    /// Start on an ephemeral port.
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind click server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        const PAGE_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Click Test</title>
<style>
  html, body { margin: 0; padding: 0; }
  /* Full-viewport target so a click anywhere inside the viewport lands here. */
  #target { position: fixed; inset: 0; }
</style>
</head>
<body>
<div id="target">click target</div>
<script>
  // Record the most recent click so the test can assert isTrusted + coords.
  window.__click_result = null;
  document.addEventListener('click', function (e) {
    window.__click_result = {
      trusted: e.isTrusted,
      x: e.clientX,
      y: e.clientY,
      tag: e.target.tagName,
      id: e.target.id
    };
  }, true);
</script>
</body>
</html>"#;

        let body = PAGE_HTML.as_bytes().to_vec();
        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        ClickServer {
            url,
            _server: server,
        }
    }
}

#[cfg(feature = "cli")]
impl Drop for ClickServer {
    fn drop(&mut self) {}
}

// ---------------------------------------------------------------------------
// BrowseServer — fixture for the reading + interaction CLI commands
// ---------------------------------------------------------------------------

/// Serves one page carrying everything the reading and interaction commands
/// need to assert against: Open Graph tags, a JSON-LD block, named meta tags,
/// links, a form with an input/button/select, and a JS log the test reads back.
#[cfg(feature = "cli")]
pub struct BrowseServer {
    /// URL of the browse fixture page.
    pub url: String,
    _server: Arc<Server>,
}

#[cfg(feature = "cli")]
impl BrowseServer {
    /// Start on an ephemeral port.
    pub fn start() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind browse server"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        const PAGE_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Browse Fixture</title>
<meta name="description" content="fixture description">
<meta property="og:title" content="Fixture OG Title">
<meta property="og:image" content="https://example.invalid/og.png">
<script type="application/ld+json">{"@context":"https://schema.org","@type":"WebPage","name":"Fixture"}</script>
</head>
<body>
<h1 id="heading">Browse Fixture</h1>
<p id="body-copy">Readable body copy.</p>
<a href="https://example.invalid/one">One</a>
<a href="https://example.invalid/two">Two</a>
<form id="f" onsubmit="log('submit:' + document.getElementById('q').value); return false;">
  <input id="q" name="q">
  <button id="go" type="button" onclick="log('click:go')">Go</button>
</form>
<select id="sel" onchange="log('change:' + document.getElementById('sel').value)">
  <option value="a">Alpha</option>
  <option value="b">Beta</option>
</select>
<div id="log"></div>
<script>
  function log(m) { document.getElementById('log').textContent += m + '\n'; }
</script>
</body>
</html>"#;

        let body = PAGE_HTML.as_bytes().to_vec();
        let server_clone = Arc::clone(&server);
        thread::spawn(move || {
            for req in server_clone.incoming_requests() {
                let resp = Response::new(
                    200.into(),
                    vec![Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/html; charset=utf-8"[..],
                    )
                    .unwrap()],
                    Cursor::new(body.clone()),
                    Some(body.len()),
                    None,
                );
                let _ = req.respond(resp);
            }
        });

        BrowseServer {
            url,
            _server: server,
        }
    }
}

#[cfg(feature = "cli")]
impl Drop for BrowseServer {
    fn drop(&mut self) {}
}
