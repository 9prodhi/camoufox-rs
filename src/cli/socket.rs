//! Socket path resolution for the daemon.
//!
//! Uses `$XDG_RUNTIME_DIR/camoufox/daemon.sock` if available,
//! otherwise falls back to `/tmp/camoufox-$UID/daemon.sock`.

use std::path::PathBuf;

/// Resolve the daemon socket path.
///
/// If `override_path` is `Some`, uses that directly. Otherwise:
/// 1. `$XDG_RUNTIME_DIR/camoufox/daemon.sock`
/// 2. `/tmp/camoufox-<uid>/daemon.sock`
pub fn socket_path(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        return PathBuf::from(p);
    }

    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let mut p = PathBuf::from(xdg);
        p.push("camoufox");
        p.push("daemon.sock");
        return p;
    }

    let uid = unsafe { libc::getuid() };
    let mut p = PathBuf::from(format!("/tmp/camoufox-{uid}"));
    p.push("daemon.sock");
    p
}
