//! Executable fixtures whose writable descriptors stay outside the test process.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Write in a child so concurrent forks cannot inherit a writable script fd.
/// Waiting for the writer also guarantees its descriptors close before execution.
pub(crate) fn write_script(path: &Path, contents: &str, mode: u32) {
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            "printf '%s' \"$1\" > \"$2\"",
            "write-script",
            contents,
        ])
        .arg(path)
        .output()
        .expect("run script writer");
    assert!(
        output.status.success(),
        "script writer failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("set script permissions");
}
