//! Secure per-user runtime directory and daemon socket paths.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Runtime directory shared by the default socket and screenshots.
pub fn runtime_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        PathBuf::from(xdg).join("camoufox")
    } else {
        PathBuf::from(format!("/tmp/camoufox-{}", unsafe { libc::getuid() }))
    }
}

pub fn socket_path(override_path: Option<&str>) -> PathBuf {
    override_path
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join("daemon.sock"))
}

/// Refuse symlinks, foreign ownership, and any group/other access.
pub fn check_runtime_dir(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect runtime directory {}: {e}", path.display()))?;
    if !meta.file_type().is_dir()
        || meta.uid() != unsafe { libc::getuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err(format!(
            "insecure runtime directory {} (expected a real, owner-only directory owned by this uid)",
            path.display()
        ));
    }
    Ok(())
}

pub fn ensure_runtime_dir() -> Result<PathBuf, String> {
    let path = runtime_dir();
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("failed to create runtime directory: {e}")),
    }
    check_runtime_dir(&path)?;
    Ok(path)
}

/// Explicit socket paths don't impose the runtime-directory permission policy.
pub fn check_socket_parent(path: &Path, create: bool) -> Result<(), String> {
    if path == socket_path(None) {
        if create {
            ensure_runtime_dir()?;
        } else {
            check_runtime_dir(&runtime_dir())?;
        }
    } else {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent.is_dir() {
            return Err(format!(
                "socket parent directory does not exist: {}",
                parent.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn refuses_public_and_symlinked_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runtime");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_runtime_dir(&dir).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_runtime_dir(&dir).is_ok());
        let link = tmp.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(check_runtime_dir(&link).is_err());
    }
}
