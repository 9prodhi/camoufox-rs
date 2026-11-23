#![cfg(feature = "cli")]
//! Offline cassette tests against an auto-detected Camoufox browser.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use camoufox::cli::instance::InstanceManager;
use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

const COMPRESSED: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 75, 206, 207, 45, 40, 74, 45, 46, 78, 77, 81, 0, 82, 5, 249,
    121, 197, 169, 0, 177, 255, 50, 111, 19, 0, 0, 0,
];

const TIMEOUT: Duration = Duration::from_secs(15);

fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

struct Fixture {
    origin: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Fixture {
    fn start() -> Self {
        let server = Server::http("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", server.server_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_requests = Arc::clone(&requests);
        let worker = std::thread::spawn(move || {
            let mut counter = 0;
            while !worker_stop.load(Ordering::SeqCst) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                else {
                    continue;
                };
                worker_requests
                    .lock()
                    .unwrap()
                    .push(request.url().to_owned());
                let mut input = Vec::new();
                request.as_reader().read_to_end(&mut input).unwrap();
                let mut extra_headers = Vec::new();
                let (body, content_type, status) = match request.url() {
                    "/" => (b"<!doctype html><body><p>fixture text</p><iframe src='/frame'></iframe><script src='/app.js'></script></body>".to_vec(), "text/html", 200),
                    "/app.js" => (br#"(async () => {
                        const values = [];
                        const tracked = (url, options) => { document.body.dataset.step = url; return fetch(url, options); };
                        values.push(await (await tracked('/data')).json());
                        values.push(await (await tracked('/compressed')).text());
                        values.push((await tracked('/empty')).status);
                        values.push((await tracked('/head', {method:'HEAD'})).status);
                        for (let i = 0; i < 2; i++) values.push(await (await tracked('/counter')).text());
                        for (const body of ['alpha', 'beta']) values.push(await (await tracked('/echo', {method:'POST', body})).text());
                        values.push(Array.from(new Uint8Array(await (await tracked('/binary')).arrayBuffer())));
                        document.body.dataset.result = JSON.stringify(values);
                        document.querySelector('p').textContent += ': loaded';
                        document.body.dataset.ready = 'yes';
                    })().catch(e => document.body.dataset.error = String(e));"#.to_vec(), "application/javascript", 200),
                    "/frame" => (b"<!doctype html><body>frame text<script src='/frame.js'></script>".to_vec(), "text/html", 200),
                    "/frame.js" => (b"document.body.dataset.ready='yes'".to_vec(), "application/javascript", 200),
                    "/data" => (br#"{"value":"fetch result"}"#.to_vec(), "application/json", 200),
                    "/redirect" => {
                        extra_headers.push(Header::from_bytes("Location", "/data").unwrap());
                        (Vec::new(), "text/plain", 302)
                    },
                    "/compressed" => {
                        extra_headers.push(Header::from_bytes("Content-Encoding", "gzip").unwrap());
                        (COMPRESSED.to_vec(), "text/plain", 200)
                    },
                    "/empty" => (Vec::new(), "text/plain", 204),
                    "/head" => (Vec::new(), "text/plain", 200),
                    "/counter" => { counter += 1; (counter.to_string().into_bytes(), "text/plain", 200) },
                    "/echo" => (input, "text/plain", 200),
                    "/large" => (vec![b'x'; 11 * 1024 * 1024], "application/octet-stream", 200),
                    "/binary" => (vec![0, 128, 255, 42], "application/octet-stream", 200),
                    _ => (b"missing".to_vec(), "text/plain", 404),
                };
                let mut response = Response::from_data(body)
                    .with_status_code(status)
                    .with_header(Header::from_bytes("Content-Type", content_type).unwrap())
                    .with_header(Header::from_bytes("Set-Cookie", "private=value").unwrap())
                    .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
                for header in extra_headers {
                    response.add_header(header);
                }
                let _ = request.respond(response);
            }
        });
        Self {
            origin,
            stop,
            worker: Some(worker),
            requests,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn launch() -> (InstanceManager, String, String) {
    let mut manager = InstanceManager::new();
    let (instance, _, _) = manager.launch(Some(true), None).unwrap();
    let page = manager.new_page(&instance).unwrap();
    (manager, instance, page)
}

fn eval(manager: &InstanceManager, instance: &str, page: &str, js: &str) -> Value {
    manager.evaluate(instance, page, js, TIMEOUT).unwrap()
}

fn rendered(manager: &InstanceManager, instance: &str, page: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = eval(manager, instance, page, "({ready:document.body?.dataset.ready, error:document.body?.dataset.error, step:document.body?.dataset.step, text:document.querySelector('p')?.textContent, result:document.body?.dataset.result, frame:document.querySelector('iframe')?.contentDocument?.body?.dataset.ready})");
        if result["ready"] == "yes" && result["frame"] == "yes" {
            return result;
        }
        assert!(result["error"].is_null(), "{result}");
        assert!(Instant::now() < deadline, "page did not render: {result}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore]
fn cassette_offline_roundtrip_misses_body_matching_and_occurrences() {
    cassette_offline_roundtrip(false);
}

#[test]
#[ignore]
fn cassette_offline_roundtrip_in_default_context() {
    cassette_offline_roundtrip(true);
}

#[allow(clippy::field_reassign_with_default)]
fn cassette_offline_roundtrip(persistent: bool) {
    let fixture = Fixture::start();
    let url = format!("{}/", fixture.origin);
    let dir = private_dir();
    let profile = private_dir();
    let manager = InstanceManager::new();
    let mut options = camoufox::cli::instance::LaunchOptions::default();
    options.headless = Some(true);
    options.profile_dir = persistent.then(|| profile.path().display().to_string());
    let (instance, _, _) = manager.launch_with(&options).unwrap();
    let page = manager.new_page(&instance).unwrap();
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    assert!(manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap_err()
        .starts_with("invalid argument:"));
    assert!(manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err()
        .starts_with("invalid argument:"));
    assert!(manager
        .replay_stop(&instance, &page)
        .unwrap_err()
        .starts_with("invalid argument:"));
    manager
        .navigate(&instance, &page, &url, TIMEOUT, Some("load"))
        .unwrap();
    let before = rendered(&manager, &instance, &page);
    let recorded = manager.record_stop(&instance, &page).unwrap();
    assert!(recorded["exchanges"].as_u64().unwrap() >= 11, "{recorded}");
    let mut repeated = recorded.clone();
    repeated["repeated"] = json!(true);
    assert_eq!(manager.record_stop(&instance, &page).unwrap(), repeated);
    let index: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
    for exchange in index["exchanges"].as_array().unwrap() {
        assert!(exchange["headers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| !h[0].as_str().unwrap().eq_ignore_ascii_case("Set-Cookie")));
    }
    let compressed = index["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["url"].as_str().unwrap().ends_with("/compressed"))
        .unwrap();
    assert_eq!(
        std::fs::read(
            dir.path()
                .join("bodies")
                .join(compressed["body_sha256"].as_str().unwrap())
        )
        .unwrap(),
        b"compressed response"
    );
    drop(fixture);
    let replay = manager.new_page(&instance).unwrap();
    manager
        .replay_start(&instance, &replay, dir.path(), true)
        .unwrap();
    assert!(manager
        .replay_start(&instance, &replay, dir.path(), true)
        .unwrap_err()
        .starts_with("invalid argument:"));
    assert!(manager
        .record_stop(&instance, &replay)
        .unwrap_err()
        .starts_with("invalid argument:"));
    manager
        .navigate(&instance, &replay, &url, TIMEOUT, Some("load"))
        .unwrap();
    assert_eq!(rendered(&manager, &instance, &replay), before);
    assert_eq!(
        eval(
            &manager,
            &instance,
            &replay,
            "fetch('/counter').then(r=>r.text())"
        ),
        "2"
    );
    assert_eq!(
        eval(
            &manager,
            &instance,
            &replay,
            "fetch('/echo',{method:'POST',body:'beta'}).then(r=>r.text())"
        ),
        "beta"
    );
    assert_eq!(
        eval(
            &manager,
            &instance,
            &replay,
            "fetch('/echo',{method:'POST',body:'unknown'}).then(()=>false,()=>true)"
        ),
        true
    );
    assert_eq!(
        eval(
            &manager,
            &instance,
            &replay,
            "fetch('/unrecorded').then(()=>false,()=>true)"
        ),
        true
    );
    let stopped = manager.replay_stop(&instance, &replay).unwrap();
    assert!(stopped["served"].as_u64().unwrap() >= 13);
    let misses = stopped["missed"].as_array().unwrap();
    assert!(
        misses.contains(&json!({"method":"GET","url":format!("{}unrecorded", url),"count":1})),
        "{stopped}"
    );
    assert!(
        misses.contains(&json!({"method":"POST","url":format!("{}echo", url),"count":1})),
        "{stopped}"
    );
    let mut repeated = stopped.clone();
    repeated["repeated"] = json!(true);
    assert_eq!(manager.replay_stop(&instance, &replay).unwrap(), repeated);
    manager.stop(&instance).unwrap();
}

#[test]
#[ignore]
fn cassette_url_filter_and_active_page_cleanup() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let (manager, instance, page) = launch();
    manager
        .record_start(&instance, &page, dir.path(), &["/data$".into()])
        .unwrap();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    rendered(&manager, &instance, &page);
    assert_eq!(
        manager.record_stop(&instance, &page).unwrap()["exchanges"],
        1
    );
    let index: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
    assert!(index["exchanges"][0]["url"]
        .as_str()
        .unwrap()
        .ends_with("/data"));
    manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap();
    manager.close_tab(&instance, &page).unwrap();
    let page = manager.new_page(&instance).unwrap();
    let active = private_dir();
    manager
        .record_start(&instance, &page, active.path(), &[])
        .unwrap();
    manager.close_tab(&instance, &page).unwrap();
    assert!(active.path().join("index.json").is_file());
    let page = manager.new_page(&instance).unwrap();
    let recording = private_dir();
    manager
        .record_start(&instance, &page, recording.path(), &[])
        .unwrap();
    let replay = manager.new_page(&instance).unwrap();
    manager
        .replay_start(&instance, &replay, dir.path(), false)
        .unwrap();
    manager.stop(&instance).unwrap();
    assert!(recording.path().join("index.json").is_file());
}

#[test]
#[ignore]
fn cassette_invalid_inputs_leave_page_usable() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let (manager, instance, page) = launch();
    assert!(manager
        .record_start(&instance, &page, std::path::Path::new("relative"), &[])
        .unwrap_err()
        .starts_with("invalid argument:"));
    assert!(manager
        .replay_start(&instance, &page, std::path::Path::new("relative"), false)
        .unwrap_err()
        .starts_with("invalid argument:"));
    std::fs::write(dir.path().join("keep"), "keep").unwrap();
    assert!(manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap_err()
        .contains("non-empty"));
    assert!(manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err()
        .starts_with("invalid argument:"));
    let hash = "0".repeat(64);
    let mut index = json!({"format":"camoufox-cassette", "version":2, "exchanges":[{
        "seq":0, "method":"GET", "url":format!("{}/data", fixture.origin), "request_body_sha256":null,
        "status":200, "headers":[], "body_sha256":hash, "body_len":0
    }]});
    std::fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    assert!(manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err()
        .contains("version"));
    index["version"] = json!(1);
    std::fs::write(
        dir.path().join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    assert!(manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err()
        .contains("body"));
    std::fs::create_dir(dir.path().join("bodies")).unwrap();
    std::fs::write(dir.path().join("bodies").join(hash), b"wrong").unwrap();
    assert!(manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err()
        .contains("sha256"));
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    rendered(&manager, &instance, &page);
    manager.stop(&instance).unwrap();
}

struct CliDaemon {
    _runtime: tempfile::TempDir,
    socket: String,
    child: std::process::Child,
}

impl CliDaemon {
    fn new() -> Self {
        let runtime = private_dir();
        let socket = runtime
            .path()
            .join("daemon.sock")
            .to_str()
            .unwrap()
            .to_owned();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_camoufox"))
            .args(["--socket", &socket, "serve"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self {
            _runtime: runtime,
            socket,
            child,
        };
        let deadline = Instant::now() + TIMEOUT;
        while !std::path::Path::new(&daemon.socket).exists() {
            assert!(Instant::now() < deadline, "daemon socket did not appear");
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon
    }

    fn run(&self, args: &[&str], success: bool) -> Value {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_camoufox"))
            .args(["--socket", &self.socket, "--json"])
            .args(args)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "invalid CLI output: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(value["ok"], success, "{value}");
        assert_eq!(output.status.success(), success);
        value
    }
}

impl Drop for CliDaemon {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_camoufox"))
            .args(["--socket", &self.socket, "shutdown"])
            .output();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore]
fn cassette_cli_daemon_commands_and_stop_restores_network() {
    let fixture = Fixture::start();
    let daemon = CliDaemon::new();
    let dir = private_dir();
    let dir_arg = dir.path().to_str().unwrap();
    let launched = daemon.run(&["launch"], true);
    let instance = launched["data"]["instance_id"].as_str().unwrap();
    let created = daemon.run(&["new-page", instance], true);
    let page = created["data"]["page_id"].as_str().unwrap();
    assert_eq!(
        daemon.run(
            &[
                "record", "start", instance, page, "--dir", dir_arg, "--url", "/data$", "--url",
                "/echo$"
            ],
            true
        )["data"],
        json!({"recording":true, "dir": dir_arg})
    );
    let error = daemon.run(
        &["replay", "start", instance, page, "--dir", dir_arg],
        false,
    );
    assert!(error["error"]
        .as_str()
        .unwrap()
        .starts_with("invalid argument:"));
    daemon.run(
        &[
            "navigate",
            instance,
            page,
            &format!("{}/", fixture.origin),
            "--wait-until",
            "load",
        ],
        true,
    );
    daemon.run(
        &["wait", instance, page, "--selector", "body[data-ready=yes]"],
        true,
    );
    let recorded = daemon.run(&["record", "stop", instance, page], true);
    assert_eq!(
        recorded["data"],
        json!({"dir":dir_arg, "exchanges":3, "bodies":3,"skipped":[]})
    );
    daemon.run(
        &[
            "replay",
            "start",
            instance,
            page,
            "--dir",
            dir_arg,
            "--match-body",
        ],
        true,
    );
    let fetched = daemon.run(
        &[
            "evaluate",
            instance,
            page,
            "fetch('/data').then(r=>r.json())",
        ],
        true,
    );
    assert_eq!(fetched["data"]["result"], json!({"value":"fetch result"}));
    daemon.run(
        &["evaluate", instance, page, "fetch('/miss').catch(()=>null)"],
        true,
    );
    let stopped = daemon.run(&["replay", "stop", instance, page], true);
    assert_eq!(
        stopped["data"],
        json!({"served":1, "missed_total":1, "missed":[{"method":"GET", "url":format!("{}/miss", fixture.origin), "count":1}]})
    );
    let fetched = daemon.run(
        &[
            "evaluate",
            instance,
            page,
            "fetch('/unrecorded').then(r=>r.status)",
        ],
        true,
    );
    assert_eq!(fetched["data"]["result"], 404);
    daemon.run(
        &["record", "start", instance, page, "--dir", dir_arg],
        false,
    );
    daemon.run(&["stop", instance], true);
}

#[test]
#[ignore]
fn cassette_redirect_recording_and_offline_validation() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    rendered(&manager, &instance, &page);
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            "fetch('/redirect').then(r=>r.json())"
        ),
        json!({"value":"fetch result"})
    );
    assert_eq!(
        manager.record_stop(&instance, &page).unwrap()["exchanges"],
        2
    );
    let index: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(index["exchanges"][0]["status"], 302);
    assert_eq!(index["exchanges"][0]["body_len"], 0);
    let error = manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap_err();
    assert!(
        error.starts_with("invalid argument:") && error.contains("redirect"),
        "{error}"
    );
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            "fetch('/unrecorded').then(r=>r.status)"
        ),
        404
    );
    manager.stop(&instance).unwrap();
}

#[test]
#[ignore]
fn cassette_stop_drains_requests_before_disabling_interception() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    rendered(&manager, &instance, &page);
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    manager.record_stop(&instance, &page).unwrap();
    manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap();
    // Wait for replay answers, not elapsed time: the stop assertions must
    // cover requests accepted by the worker even on a fast browser run.
    let answered = manager
        .evaluate(
            &instance,
            &page,
            r#"(() => {
            let ready;
            const intercepted = new Promise(resolve => { ready = resolve; });
            let blocked = 0;
            window.batch = Object.fromEntries(Array.from({length:400}, (_, i) => {
                const path = '/batch?i=' + i;
                const request = fetch(path).then(
                    () => ({path, blocked:false}),
                    () => {
                        if (++blocked === 16) ready(blocked);
                        return {path, blocked:true};
                    }
                );
                return [path, request];
            }));
            return intercepted;
        })()"#,
            TIMEOUT,
        )
        .expect("replay did not reject the first 16 requests");
    assert_eq!(answered, 16);
    let stopped = manager.replay_stop(&instance, &page).unwrap();
    let misses = stopped["missed"].as_array().unwrap();
    assert!(
        misses.len() >= 16,
        "accepted requests were not reported: {stopped}"
    );
    // Await every accepted request before checking the origin log. Requests
    // outside the stop boundary may use the network and are not replay misses.
    let paths: Vec<_> = misses
        .iter()
        .map(|miss| {
            miss["url"]
                .as_str()
                .unwrap()
                .strip_prefix(&fixture.origin)
                .unwrap()
        })
        .collect();
    let expression = format!(
        "Promise.all({}.map(path => window.batch[path]))",
        json!(paths)
    );
    let completed = manager
        .evaluate(&instance, &page, &expression, TIMEOUT)
        .expect("accepted requests did not settle after replay stop");
    let completed = completed.as_array().unwrap();
    assert_eq!(completed.len(), misses.len());
    let requests = fixture.requests.lock().unwrap().clone();
    for miss in misses {
        let path = miss["url"]
            .as_str()
            .unwrap()
            .strip_prefix(&fixture.origin)
            .unwrap();
        assert!(
            completed
                .iter()
                .any(|result| result["path"] == path && result["blocked"] == true),
            "accepted replay miss was not rejected: {path}"
        );
        assert!(
            !requests.iter().any(|request| request == path),
            "replay miss reached the live server: {path}"
        );
    }
    manager.stop(&instance).unwrap();
}

#[test]
#[ignore]
fn cassette_skips_failed_and_non_http_requests_and_replay_restarts() {
    let fixture = Fixture::start();
    let dir = private_dir();
    // A closed, non-restricted local port makes the request fail at connect.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/unreachable", listener.local_addr().unwrap())
    };
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    rendered(&manager, &instance, &page);
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            &format!(
                "(async () => [
                (await (await fetch('/data')).json()).value,
                await fetch('{closed}').then(() => 'reached', () => 'failed'),
                await (await fetch('data:text/plain,inline')).text(),
            ])()"
            )
        ),
        json!(["fetch result", "failed", "inline"])
    );
    let recorded = manager.record_stop(&instance, &page).unwrap();
    assert_eq!(recorded["exchanges"], 1, "{recorded}");
    let index: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(
        index["exchanges"][0]["url"],
        format!("{}/data", fixture.origin)
    );

    // Start/stop replay twice on one page: interception must toggle cleanly.
    drop(fixture);
    for _ in 0..2 {
        manager
            .replay_start(&instance, &page, dir.path(), false)
            .unwrap();
        assert_eq!(
            eval(
                &manager,
                &instance,
                &page,
                "fetch('/data').then(r => r.json()).then(v => v.value)"
            ),
            "fetch result"
        );
        assert_eq!(
            eval(
                &manager,
                &instance,
                &page,
                &format!("fetch('{closed}').then(() => 'reached', () => 'blocked')")
            ),
            "blocked"
        );
        let stopped = manager.replay_stop(&instance, &page).unwrap();
        assert_eq!(
            stopped,
            json!({"served":1, "missed_total":1, "missed":[{"method":"GET", "url":closed, "count":1}]})
        );
    }
    manager.stop(&instance).unwrap();
}

/// Whether `pid` has exited (gone or a zombie awaiting its new parent).
fn process_gone(pid: u64) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
    }
}

#[test]
#[ignore]
fn cassette_daemon_killed_mid_record_leaves_unpublished_cassette() {
    let fixture = Fixture::start();
    let parent = private_dir();
    let dir = parent.path().join("cassette");
    let dir_arg = dir.to_str().unwrap();
    let mut daemon = CliDaemon::new();
    let launched = daemon.run(&["launch"], true);
    let instance = launched["data"]["instance_id"].as_str().unwrap().to_owned();
    let browser_pid = launched["data"]["pid"].as_u64().unwrap();
    let page = daemon.run(&["new-page", &instance], true)["data"]["page_id"]
        .as_str()
        .unwrap()
        .to_owned();
    daemon.run(
        &["record", "start", &instance, &page, "--dir", dir_arg],
        true,
    );
    daemon.run(
        &[
            "navigate",
            &instance,
            &page,
            &format!("{}/", fixture.origin),
            "--wait-until",
            "load",
        ],
        true,
    );
    daemon.run(
        &[
            "wait",
            &instance,
            &page,
            "--selector",
            "body[data-ready=yes]",
        ],
        true,
    );
    // Bodies are written as responses finish, so some exist before the kill.
    let deadline = Instant::now() + TIMEOUT;
    while std::fs::read_dir(dir.join("bodies"))
        .unwrap()
        .next()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "no completed response body before daemon kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();

    // Without the daemon, the cassette is never published.
    assert!(!dir.join("index.json").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(leftovers, ["bodies"]);
    // The browser exits once its protocol pipe closes.
    let deadline = Instant::now() + TIMEOUT;
    while !process_gone(browser_pid) {
        assert!(
            Instant::now() < deadline,
            "browser {browser_pid} outlived its daemon"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // A fresh daemon refuses to replay or overwrite the partial cassette.
    let daemon = CliDaemon::new();
    let instance = daemon.run(&["launch"], true)["data"]["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let page = daemon.run(&["new-page", &instance], true)["data"]["page_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let replay = daemon.run(
        &["replay", "start", &instance, &page, "--dir", dir_arg],
        false,
    );
    assert_eq!(replay["kind"], "invalid_argument", "{replay}");
    assert!(replay["error"]
        .as_str()
        .unwrap()
        .starts_with("invalid argument: invalid cassette: "));
    let record = daemon.run(
        &["record", "start", &instance, &page, "--dir", dir_arg],
        false,
    );
    assert_eq!(
        record["error"],
        "invalid argument: refusing non-empty cassette directory"
    );
    // The page is unaffected and replay is not active.
    daemon.run(&["replay", "stop", &instance, &page], false);
    daemon.run(&["stop", &instance], true);
}

#[test]
#[ignore]
fn cassette_large_response_is_skipped_and_instance_stop_publishes_recording() {
    let fixture = Fixture::start();
    let parent = private_dir();
    let dir = parent.path().join("cassette");
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/frame", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    manager
        .record_start(&instance, &page, &dir, &["/(large|data)$".into()])
        .unwrap();
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            "fetch('/large').then(r=>r.arrayBuffer()).then(b=>b.byteLength)"
        ),
        11 * 1024 * 1024
    );
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            "fetch('/data').then(r=>r.json()).then(v=>v.value)"
        ),
        "fetch result"
    );
    let result = manager.record_stop(&instance, &page).unwrap();
    assert_eq!(result["exchanges"], 1, "{result}");
    assert_eq!(
        result["skipped"][0]["url"],
        format!("{}/large", fixture.origin)
    );
    assert!(result["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("evicted"));
    let final_dir = parent.path().join("cleanup");
    manager
        .record_start(&instance, &page, &final_dir, &["/data$".into()])
        .unwrap();
    eval(
        &manager,
        &instance,
        &page,
        "fetch('/data').then(r=>r.json())",
    );
    manager.stop(&instance).unwrap();
    let index: Value =
        serde_json::from_slice(&std::fs::read(final_dir.join("index.json")).unwrap()).unwrap();
    assert_eq!(index["exchanges"].as_array().unwrap().len(), 1);
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/frame", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    drop(fixture);
    manager
        .replay_start(&instance, &page, &final_dir, false)
        .unwrap();
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            "fetch('/data').then(r=>r.json()).then(v=>v.value)"
        ),
        "fetch result"
    );
    let result = manager.replay_stop(&instance, &page).unwrap();
    assert_eq!(result["served"], 1);
    assert_eq!(result["missed_total"], 0);
    manager.stop(&instance).unwrap();
}

#[test]
#[ignore]
fn cassette_multipart_form_matches_new_random_boundaries() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let (manager, instance, page) = launch();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/frame", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    let submit = "(() => { const body = new FormData(); body.append('field', 'value'); body.append('file', new Blob(['fixture content']), 'fixture.txt'); return fetch('/echo', {method:'POST', body}).then(r=>r.text()); })()";
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    let recorded = eval(&manager, &instance, &page, submit);
    manager.record_stop(&instance, &page).unwrap();
    drop(fixture);
    manager
        .replay_start(&instance, &page, dir.path(), true)
        .unwrap();
    for _ in 0..2 {
        assert_eq!(eval(&manager, &instance, &page, submit), recorded);
    }
    let different = submit.replace("'field', 'value'", "'field', 'different'");
    assert_eq!(
        eval(
            &manager,
            &instance,
            &page,
            &format!("({different}).then(()=>false,()=>true)")
        ),
        true
    );
    let result = manager.replay_stop(&instance, &page).unwrap();
    assert_eq!(result["served"], 2);
    assert_eq!(result["missed_total"], 1);
    manager.stop(&instance).unwrap();
}

#[test]
#[ignore]
#[allow(clippy::field_reassign_with_default)]
fn cassette_replay_suppresses_persistent_response_headers() {
    let fixture = Fixture::start();
    let dir = private_dir();
    let profile = private_dir();
    let manager = InstanceManager::new();
    let mut options = camoufox::cli::instance::LaunchOptions::default();
    options.headless = Some(true);
    options.profile_dir = Some(profile.path().display().to_string());
    let (instance, _, _) = manager.launch_with(&options).unwrap();
    let page = manager.new_page(&instance).unwrap();
    manager
        .navigate(
            &instance,
            &page,
            &format!("{}/frame", fixture.origin),
            TIMEOUT,
            Some("load"),
        )
        .unwrap();
    manager
        .record_start(&instance, &page, dir.path(), &[])
        .unwrap();
    eval(
        &manager,
        &instance,
        &page,
        "fetch('/data').then(r=>r.text())",
    );
    manager.record_stop(&instance, &page).unwrap();
    let index_path = dir.path().join("index.json");
    let mut index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    index["exchanges"][0]["headers"] = json!([
        ["Cache-Control", "public, max-age=31536000"],
        ["Expires", "Wed, 01 Jan 2042 00:00:00 GMT"],
        ["Pragma", "cache"],
        ["Strict-Transport-Security", "max-age=31536000"],
        ["Alt-Svc", "h2=\"example.com:443\""],
        ["Service-Worker-Allowed", "/"],
        ["Clear-Site-Data", "\"storage\", \"cookies\""],
        ["Set-Cookie", "replayed=secret"],
        ["Content-Type", "application/json"]
    ]);
    std::fs::write(index_path, index.to_string()).unwrap();
    eval(
        &manager,
        &instance,
        &page,
        "localStorage.setItem('keep','value'); document.cookie='keep=value'; true",
    );
    manager
        .replay_start(&instance, &page, dir.path(), false)
        .unwrap();
    let headers = eval(
        &manager,
        &instance,
        &page,
        "fetch('/data').then(async r=>{await r.text(); return Object.fromEntries(r.headers);})",
    );
    assert_eq!(headers["cache-control"], "no-store");
    for name in [
        "expires",
        "pragma",
        "strict-transport-security",
        "alt-svc",
        "service-worker-allowed",
        "clear-site-data",
        "set-cookie",
    ] {
        assert!(headers[name].is_null(), "{name}: {headers}");
    }
    assert_eq!(
        eval(&manager, &instance, &page, "localStorage.getItem('keep')"),
        "value"
    );
    let cookies = eval(&manager, &instance, &page, "document.cookie");
    let cookies = cookies.as_str().unwrap();
    assert!(cookies.contains("keep=value"));
    assert!(!cookies.contains("replayed="), "{cookies}");
    let cookies = manager.cookies(&instance).unwrap();
    assert!(cookies
        .iter()
        .any(|cookie| cookie["name"] == "keep" && cookie["value"] == "value"));
    assert!(!cookies.iter().any(|cookie| cookie["name"] == "replayed"));
    manager.replay_stop(&instance, &page).unwrap();
    // A live response after stop must reach the server, not replay's disk cache.
    let before = fixture
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|p| *p == "/data")
        .count();
    eval(
        &manager,
        &instance,
        &page,
        "fetch('/data').then(r=>r.text())",
    );
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == "/data")
            .count(),
        before + 1
    );
    manager.stop(&instance).unwrap();
}
