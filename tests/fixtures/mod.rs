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
        let main_server =
            Arc::new(Server::http("127.0.0.1:0").expect("bind main server"));
        let iframe_server =
            Arc::new(Server::http("127.0.0.1:0").expect("bind iframe server"));

        let main_port = main_server.server_addr().to_ip().unwrap().port();
        let iframe_port = iframe_server.server_addr().to_ip().unwrap().port();

        let main_url = format!("http://127.0.0.1:{main_port}/");
        let iframe_url = format!("http://127.0.0.1:{iframe_port}/");

        // Main page: 50 ms delay, then HTML with the iframe URL substituted.
        let main_html =
            MAIN_HTML_TEMPLATE.replace("__IFRAME_URL__", &iframe_url);
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
