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
