//! Tests for external subcommand dispatch (git/cargo style).

#![cfg(feature = "cli")]

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

fn camoufox_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_camoufox"))
}

const PINNED_NONEXISTENT_STDERR: &str = "\
error: unrecognized subcommand 'nonexistent'

Usage: camoufox [OPTIONS] <COMMAND>

For more information, try '--help'.
";

const PINNED_PATH_SEPARATOR_STDERR: &str = "\
error: unrecognized subcommand 'some/path'

Usage: camoufox [OPTIONS] <COMMAND>

For more information, try '--help'.
";

#[test]
fn external_dispatch_runs_script_with_args_env_and_exit_code() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let script_path = temp_dir.path().join("camoufox-hello");

    let script_content = "#!/bin/sh\n\
        echo \"args: $@\"\n\
        echo \"socket: $CAMOUFOX_SOCKET\"\n\
        exit 7\n";
    fs::write(&script_path, script_content).expect("write script");

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("set permissions");
    }

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = temp_dir.path().as_os_str().to_os_string();
    new_path.push(":");
    new_path.push(&original_path);

    let output = Command::new(camoufox_bin())
        .env("PATH", &new_path)
        .arg("hello")
        .arg("foo")
        .arg("bar")
        .arg("--flag")
        .arg("baz")
        .output()
        .expect("run camoufox");

    assert_eq!(output.status.code(), Some(7), "exit code must be 7");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("args: foo bar --flag baz"),
        "stdout must contain passed arguments, got:\n{stdout}"
    );
    assert!(
        stdout.contains("socket: "),
        "stdout must contain CAMOUFOX_SOCKET, got:\n{stdout}"
    );
}

#[test]
fn external_dispatch_respects_custom_socket_flag() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let script_path = temp_dir.path().join("camoufox-hello");

    let script_content = "#!/bin/sh\n\
        echo \"socket: $CAMOUFOX_SOCKET\"\n\
        exit 7\n";
    fs::write(&script_path, script_content).expect("write script");

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("set permissions");
    }

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = temp_dir.path().as_os_str().to_os_string();
    new_path.push(":");
    new_path.push(&original_path);

    let custom_socket = "/tmp/test-camoufox-custom.sock";
    let output = Command::new(camoufox_bin())
        .env("PATH", &new_path)
        .arg("--socket")
        .arg(custom_socket)
        .arg("hello")
        .output()
        .expect("run camoufox");

    assert_eq!(output.status.code(), Some(7), "exit code must be 7");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("socket: {custom_socket}")),
        "stdout must contain custom socket, got:\n{stdout}"
    );
}

#[test]
fn external_dispatch_unknown_name_matches_pinned_clap_error() {
    let output = Command::new(camoufox_bin())
        .arg("nonexistent")
        .output()
        .expect("run camoufox");

    assert_eq!(output.status.code(), Some(2), "exit code must be 2");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert_eq!(
        stderr, PINNED_NONEXISTENT_STDERR,
        "stderr must be byte-identical to pinned clap error"
    );
}

#[test]
fn external_dispatch_path_separator_rejected_as_unrecognized() {
    let output = Command::new(camoufox_bin())
        .arg("some/path")
        .output()
        .expect("run camoufox");

    assert_eq!(output.status.code(), Some(2), "exit code must be 2");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert_eq!(
        stderr, PINNED_PATH_SEPARATOR_STDERR,
        "stderr must be byte-identical to pinned clap error"
    );
}

#[test]
fn external_dispatch_builtin_names_unaffected() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    // Place a shadowing binary on PATH with the same name as a built-in command.
    let script_path = temp_dir.path().join("camoufox-list");

    let script_content = "#!/bin/sh\n\
        echo \"SHADOWED_EXTERNAL_CALLED\"\n\
        exit 42\n";
    fs::write(&script_path, script_content).expect("write script");

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("set permissions");
    }

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = temp_dir.path().as_os_str().to_os_string();
    new_path.push(":");
    new_path.push(&original_path);

    // Run `camoufox list --help`. If the built-in is invoked, it prints help.
    // If external dispatch took over, it would output SHADOWED_EXTERNAL_CALLED and exit 42.
    let output = Command::new(camoufox_bin())
        .env("PATH", &new_path)
        .arg("list")
        .arg("--help")
        .output()
        .expect("run camoufox");

    assert_eq!(
        output.status.code(),
        Some(0),
        "exit code must be 0 for --help"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("SHADOWED_EXTERNAL_CALLED"),
        "built-in command must not invoke external binary"
    );
    assert!(
        stdout.contains("List all running browser instances"),
        "built-in help must be printed"
    );
}
