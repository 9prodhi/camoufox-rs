//! Unix socket daemon with per-connection workers and per-instance locking.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::cli::instance::{InstanceManager, LaunchOptions};
use crate::cli::ipc::{DaemonRequest, DaemonResponse};

const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;

/// Remove only a socket. Also usable by the top-level signal shutdown path.
pub fn unlink_socket(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|e| e.to_string())
        }
        Ok(_) => Err("refusing to replace non-socket file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

struct SocketGuard {
    // Never unlink the lock file: waiters must always flock the same inode.
    _lock: File,
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = unlink_socket(&self.path);
    }
}

fn bind_socket(path: &Path) -> Result<(UnixListener, SocketGuard), String> {
    crate::cli::socket::check_socket_parent(path, true)?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|e| format!("failed to open daemon lock: {e}"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Err("daemon is already running".into());
        }
        return Err(format!("failed to lock daemon socket: {e}"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err("refusing to replace non-socket file".into());
            }
            if UnixStream::connect(path).is_ok() {
                return Err("daemon is already running".into());
            }
            unlink_socket(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to inspect socket: {e}")),
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("failed to bind socket: {e}"))?;
    let guard = SocketGuard {
        _lock: lock,
        path: path.to_owned(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("failed to secure socket: {e}"))?;
    Ok((listener, guard))
}

/// Top-level daemon entry point.
///
/// SIGTERM/SIGINT are blocked on every thread and consumed by a dedicated
/// `sigwait` thread, which sets `serve`'s stop flag. The serve loop then
/// joins in-flight workers, shuts every instance down, and unlinks the
/// socket, so a signalled daemon cleans up exactly like a `Shutdown` request.
pub fn run(socket_path: &Path, foreground: bool) -> Result<(), String> {
    // Block before spawning any threads so every thread inherits the mask.
    // (std::process::Command resets the mask for spawned browsers.)
    let signals = unsafe {
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGTERM);
        libc::sigaddset(&mut signals, libc::SIGINT);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
        if rc != 0 {
            return Err(format!(
                "failed to block shutdown signals: {}",
                std::io::Error::from_raw_os_error(rc)
            ));
        }
        signals
    };

    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("camoufox-signal-waiter".into())
        .spawn(move || {
            let mut signal = 0;
            let rc = unsafe { libc::sigwait(&signals, &mut signal) };
            if rc == 0 {
                log::info!("received signal {signal}; shutting down");
                signal_stop.store(true, Ordering::Release);
            } else {
                log::error!("sigwait failed: {}", std::io::Error::from_raw_os_error(rc));
            }
        })
        .map_err(|e| format!("failed to start signal waiter: {e}"))?;

    if foreground {
        eprintln!("camoufox daemon listening on {}", socket_path.display());
    }
    serve(socket_path, Arc::new(InstanceManager::new()), stop)
}

/// Bind securely, retaining the sibling flock until all browser work has stopped.
pub fn serve(
    path: &Path,
    manager: Arc<InstanceManager>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let (listener, _guard) = bind_socket(path)?;
    serve_on(listener, manager, stop)
}

/// Serve an already-bound listener in-process. The caller must serialize binding
/// (use `serve` for the full lock/check/remove/bind policy).
pub fn serve_on(
    listener: UnixListener,
    manager: Arc<InstanceManager>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let path = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .as_pathname()
        .map(Path::to_owned);
    let shutdown_waiters = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if !same_uid(&stream) {
                    continue;
                }
                let manager = Arc::clone(&manager);
                let stop = Arc::clone(&stop);
                let waiters = Arc::clone(&shutdown_waiters);
                workers.push(std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, &manager, &stop, &waiters) {
                        log::warn!("connection error: {e}");
                    }
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(format!("accept error: {e}")),
        }
        let mut i = 0;
        while i < workers.len() {
            if workers[i].is_finished() {
                let _ = workers.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
    };
    drop(listener);
    manager.begin_shutdown();
    manager.close_all_connections();
    for worker in workers {
        let _ = worker.join();
    }
    manager.shutdown_all();
    // Always answer waiting `shutdown` callers, even if unlinking fails.
    let unlinked = match path {
        Some(path) => unlink_socket(&path),
        None => Ok(()),
    };
    for mut writer in shutdown_waiters
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
    {
        let _ = serde_json::to_writer(&mut writer, &DaemonResponse::ok_empty());
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }
    unlinked?;
    result
}

#[cfg(target_os = "linux")]
fn same_uid(stream: &UnixStream) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) == 0
            && len as usize == std::mem::size_of::<libc::ucred>()
            && cred.uid == libc::getuid()
    }
}

#[cfg(not(target_os = "linux"))]
fn same_uid(_: &UnixStream) -> bool {
    true
}

fn handle_connection(
    stream: UnixStream,
    manager: &InstanceManager,
    stop: &AtomicBool,
    waiters: &Mutex<Vec<UnixStream>>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;
    let mut line = String::new();
    let response = match (&mut reader).take(MAX_REQUEST_BYTES).read_line(&mut line) {
        Ok(0) => return Ok(()),
        Ok(_) if !line.ends_with('\n') => {
            DaemonResponse::err("request line too large or missing newline")
        }
        Ok(_) => match serde_json::from_str::<DaemonRequest>(&line) {
            Ok(request) => {
                if matches!(request, DaemonRequest::Shutdown) {
                    waiters
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(writer);
                    manager.begin_shutdown();
                    stop.store(true, Ordering::Release);
                    return Ok(());
                }
                if stop.load(Ordering::Acquire) {
                    DaemonResponse::err("daemon shutting down")
                } else {
                    dispatch_guarded(request, manager)
                }
            }
            Err(e) => DaemonResponse::err(format!("invalid request: {e}")),
        },
        Err(e) => return Err(format!("read error: {e}")),
    };
    serde_json::to_writer(&mut writer, &response).map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

fn respond<T: Serialize>(result: Result<T, String>) -> DaemonResponse {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => DaemonResponse::ok(value),
            Err(e) => DaemonResponse::err(e.to_string()),
        },
        Err(e) => DaemonResponse::err(e),
    }
}

fn dispatch_guarded(request: DaemonRequest, mgr: &InstanceManager) -> DaemonResponse {
    if mgr.is_closing() && !matches!(request, DaemonRequest::Shutdown) {
        return DaemonResponse::err("daemon shutting down");
    }
    dispatch(request, mgr)
}

fn dispatch(request: DaemonRequest, mgr: &InstanceManager) -> DaemonResponse {
    // Derive the duration once; instance-scoped locking lives in the manager.
    let timeout = Duration::from_secs(request.timeout_secs());
    match request {
        DaemonRequest::Ping => DaemonResponse::ok(json!({"instance_count": mgr.instance_count()})),
        DaemonRequest::List => DaemonResponse::ok(json!({"instances": mgr.list()})),
        DaemonRequest::Shutdown => DaemonResponse::ok_empty(),
        DaemonRequest::Launch { headless, executable } => respond(mgr.launch_with(&LaunchOptions { headless, executable }).map(|(instance_id, version, pid)| json!({"instance_id": instance_id, "version": version, "pid": pid}))),
        DaemonRequest::Stop { instance_id } => respond(mgr.stop(&instance_id)),
        DaemonRequest::NewPage { instance_id } => respond(mgr.new_page(&instance_id).map(|page_id| json!({"page_id": page_id}))),
        DaemonRequest::Navigate { instance_id, page_id, url, wait_until, .. } => respond(mgr.navigate(&instance_id, &page_id, &url, timeout, wait_until.as_deref()).map(|o| json!({"navigation_id": o.nav_id, "status_code": o.status_code}))),
        DaemonRequest::Evaluate { instance_id, page_id, expression, main_world, .. } => {
            let result = if main_world {
                mgr.evaluate_main_world(&instance_id, &page_id, &expression, timeout)
            } else {
                mgr.evaluate(&instance_id, &page_id, &expression, timeout)
            };
            respond(result.map(|result| json!({"result": result})))
        },
        DaemonRequest::Click { instance_id, page_id, x, y } => respond(mgr.click(&instance_id, &page_id, x, y).map(|()| json!({"clicked": true, "x": x, "y": y}))),
        DaemonRequest::ClickSelector { instance_id, page_id, selector, .. } => respond(mgr.click_selector(&instance_id, &page_id, &selector, timeout).map(|(x,y)| json!({"clicked": true, "selector": selector, "x": x, "y": y}))),
        DaemonRequest::Screenshot { instance_id, page_id, format, quality, path, selector, clip, .. } => respond(mgr.screenshot(&instance_id, &page_id, format.as_deref(), quality, path.as_deref(), selector.as_deref(), clip, timeout).map(|(bytes, path, rect)| json!({"bytes": bytes.len(), "path": path, "clip": {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height}}))),
        DaemonRequest::Cookies { instance_id } => respond(mgr.cookies(&instance_id).map(|cookies| json!({"cookies": cookies}))),
        DaemonRequest::Text { instance_id, page_id, selector, .. } => respond(mgr.text(&instance_id, &page_id, selector.as_deref(), timeout).map(|text| json!({"text": text}))),
        DaemonRequest::Html { instance_id, page_id, selector, .. } => respond(mgr.html(&instance_id, &page_id, selector.as_deref(), timeout).map(|html| json!({"html": html}))),
        DaemonRequest::Links { instance_id, page_id, selector, .. } => respond(mgr.links(&instance_id, &page_id, selector.as_deref(), timeout).map(|links| json!({"links": links}))),
        DaemonRequest::Data { instance_id, page_id, og, jsonld, meta, .. } => respond(mgr.data(&instance_id, &page_id, og, jsonld, meta, timeout)),
        DaemonRequest::Url { instance_id, page_id, .. } => respond(mgr.url(&instance_id, &page_id, timeout).map(|(url,title)| json!({"url": url, "title": title}))),
        DaemonRequest::Back { instance_id, page_id } => respond(mgr.go_back(&instance_id, &page_id).map(|navigated| json!({"navigated": navigated, "direction": "back"}))),
        DaemonRequest::Forward { instance_id, page_id } => respond(mgr.go_forward(&instance_id, &page_id).map(|navigated| json!({"navigated": navigated, "direction": "forward"}))),
        DaemonRequest::Reload { instance_id, page_id } => respond(mgr.reload(&instance_id, &page_id).map(|()| json!({"reloaded": true}))),
        DaemonRequest::Wait { instance_id, page_id, selector, .. } => respond(mgr.wait_for_selector(&instance_id, &page_id, &selector, timeout).map(|waited_ms| json!({"found": true, "selector": selector, "waited_ms": waited_ms}))),
        DaemonRequest::SetCookie { instance_id, page_id, name, value, url, domain, path, secure, http_only, .. } => respond(mgr.set_cookie(&instance_id, &page_id, &name, &value, url.as_deref(), domain.as_deref(), path.as_deref(), secure, http_only, timeout)),
        DaemonRequest::SetHeader { instance_id, page_id, name, value } => respond(mgr.set_header(&instance_id, &page_id, &name, &value).map(|headers| { let headers: Vec<_> = headers.iter().map(|(n,v)| json!({"name": n, "value": v})).collect(); json!({"header_set": name, "headers": headers}) })),
        DaemonRequest::Fill { instance_id, page_id, selector, value, .. } => respond(mgr.fill(&instance_id, &page_id, &selector, &value, timeout).map(|tag| json!({"filled": true, "selector": selector, "tag": tag, "value": value}))),
        DaemonRequest::Type { instance_id, page_id, text } => respond(mgr.insert_text(&instance_id, &page_id, &text).map(|()| json!({"typed": text}))),
        DaemonRequest::Press { instance_id, page_id, key } => respond(mgr.press(&instance_id, &page_id, &key).map(|()| json!({"pressed": key}))),
        DaemonRequest::Hover { instance_id, page_id, selector, .. } => respond(mgr.hover(&instance_id, &page_id, &selector, timeout).map(|(x,y)| json!({"hovered": true, "selector": selector, "x": x, "y": y}))),
        DaemonRequest::Select { instance_id, page_id, selector, value, .. } => respond(mgr.select_option(&instance_id, &page_id, &selector, &value, timeout).map(|r| json!({"selected": true, "selector": selector, "option_value": r.get("value"), "option_text": r.get("text")}))),
        DaemonRequest::Scroll { instance_id, page_id, selector, .. } => respond(mgr.scroll(&instance_id, &page_id, selector.as_deref(), timeout)),
        DaemonRequest::Tabs { instance_id, .. } => respond(mgr.tabs(&instance_id, timeout).map(|tabs| json!({"tabs": tabs}))),
        DaemonRequest::CloseTab { instance_id, page_id } => respond(mgr.close_tab(&instance_id, &page_id).map(|()| json!({"closed_page": page_id}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::client::send_request;

    struct TestDaemon {
        _dir: tempfile::TempDir,
        path: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<(), String>>>,
    }

    impl TestDaemon {
        fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("d.sock");
            let (listener, guard) = bind_socket(&path).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let _guard = guard;
                serve_on(listener, Arc::new(InstanceManager::new()), flag)
            });
            Self {
                _dir: dir,
                path,
                stop,
                thread: Some(thread),
            }
        }
        fn request(&self, req: DaemonRequest) -> DaemonResponse {
            send_request(&self.path, &req).unwrap()
        }
        fn join(&mut self) {
            self.thread.take().unwrap().join().unwrap().unwrap();
        }
    }
    impl Drop for TestDaemon {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn ping_round_trip() {
        let daemon = TestDaemon::start();
        let response = daemon.request(DaemonRequest::Ping);
        assert!(response.ok);
        assert_eq!(response.data.unwrap()["instance_count"], 0);
    }

    #[test]
    fn shutdown_text_does_not_stop_daemon() {
        let daemon = TestDaemon::start();
        assert!(
            !daemon
                .request(DaemonRequest::Type {
                    instance_id: "missing".into(),
                    page_id: "p1".into(),
                    text: "Shutdown".into(),
                })
                .ok
        );
        assert!(daemon.request(DaemonRequest::Ping).ok);
        assert!(!daemon.stop.load(Ordering::Acquire));
    }

    #[test]
    fn requests_during_shutdown_are_refused() {
        let manager = InstanceManager::new();
        manager.begin_shutdown();
        let response = dispatch_guarded(DaemonRequest::List, &manager);
        assert!(!response.ok);
        assert!(format!("{response:?}").contains("daemon shutting down"));
    }

    #[test]
    fn shutdown_stops_and_unlinks() {
        let mut daemon = TestDaemon::start();
        assert!(daemon.request(DaemonRequest::Shutdown).ok);
        assert!(!daemon.path.exists(), "socket must be gone before reply");
        daemon.join();
        assert!(!daemon.path.exists());
    }

    #[test]
    fn refuses_regular_file_without_chmodding_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "precious notes").unwrap();
        let error = serve(
            &path,
            Arc::new(InstanceManager::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.contains("refusing to replace non-socket file"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "precious notes");
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn second_serve_fails_while_first_is_alive() {
        let daemon = TestDaemon::start();
        let error = serve(
            &daemon.path,
            Arc::new(InstanceManager::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.contains("already running"), "{error}");
        assert!(daemon.request(DaemonRequest::Ping).ok);
    }

    #[test]
    fn oversized_line_is_rejected_and_daemon_survives() {
        let daemon = TestDaemon::start();
        let mut stream = UnixStream::connect(&daemon.path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(&vec![b'x'; MAX_REQUEST_BYTES as usize])
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: DaemonResponse = serde_json::from_str(&line).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("too large"));
        assert!(daemon.request(DaemonRequest::Ping).ok);
    }

    #[test]
    fn idle_connection_does_not_block_ping() {
        // Holding an unrelated client connection open must not serialize Ping.
        let daemon = TestDaemon::start();
        let mut idle = UnixStream::connect(&daemon.path).unwrap();
        idle.write_all(b"{").unwrap();
        assert!(daemon.request(DaemonRequest::Ping).ok);
        drop(idle);
    }
}
