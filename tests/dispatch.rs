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
fn external_dispatch_near_miss_preserves_clap_suggestions() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(camoufox_bin())
        .env("PATH", temp_dir.path())
        .arg("navgate")
        .output()
        .expect("run camoufox");

    assert_eq!(output.status.code(), Some(2), "exit code must be 2");
    assert!(output.stdout.is_empty(), "stdout must be empty");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        stderr.contains("\n\n  tip: some similar subcommands exist: 'evaluate', 'navigate'\n\n"),
        "stderr must contain clap's suggestion line and surrounding blank lines, got:\n{stderr}"
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

/// Write `contents` to `dir/name` with the given Unix mode.
#[cfg(unix)]
fn write_script(dir: &std::path::Path, name: &str, contents: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set permissions");
    path
}

#[cfg(unix)]
#[test]
fn external_dispatch_signal_exit_maps_to_128_plus_signal() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    write_script(
        temp_dir.path(),
        "camoufox-signal",
        "#!/bin/sh\nkill -TERM $$\nsleep 5\n",
        0o755,
    );
    let output = Command::new(camoufox_bin())
        .env("PATH", temp_dir.path())
        .arg("signal")
        .output()
        .expect("run camoufox");
    assert_eq!(
        output.status.code(),
        Some(128 + 15),
        "a child killed by SIGTERM must exit with 128 + 15"
    );
}

#[cfg(unix)]
#[test]
fn external_dispatch_skips_non_executable_entries_and_missing_path() {
    let first = tempfile::tempdir().expect("first dir");
    let second = tempfile::tempdir().expect("second dir");
    let third = tempfile::tempdir().expect("third dir");
    // Earliest PATH entry: present but not executable, so it is skipped.
    write_script(
        first.path(),
        "camoufox-tool",
        "#!/bin/sh\necho non-executable\nexit 3\n",
        0o644,
    );
    // A directory with the binary name is skipped as well.
    fs::create_dir(second.path().join("camoufox-tool")).expect("create dir");
    write_script(
        third.path(),
        "camoufox-tool",
        "#!/bin/sh\necho executable\nexit 4\n",
        0o755,
    );
    let path = std::env::join_paths([first.path(), second.path(), third.path()]).unwrap();
    let output = Command::new(camoufox_bin())
        .env("PATH", &path)
        .arg("tool")
        .output()
        .expect("run camoufox");
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "executable\n");

    // With only the non-executable entry, or with no PATH at all, the
    // name is unrecognized.
    for path in [Some(first.path().as_os_str()), None] {
        let mut command = Command::new(camoufox_bin());
        match path {
            Some(path) => command.env("PATH", path),
            None => command.env_remove("PATH"),
        };
        let output = command.arg("tool").output().expect("run camoufox");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("error: unrecognized subcommand 'tool'"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn external_dispatch_exec_failure_exits_1_with_message() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    write_script(
        temp_dir.path(),
        "camoufox-broken",
        "#!/nonexistent/interpreter\n",
        0o755,
    );
    let output = Command::new(camoufox_bin())
        .env("PATH", temp_dir.path())
        .arg("broken")
        .output()
        .expect("run camoufox");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error: failed to execute external subcommand: "),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn external_dispatch_inherits_stdin_and_passes_args_verbatim() {
    use std::io::Write;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    write_script(
        temp_dir.path(),
        "camoufox-echo",
        "#!/bin/sh\nfor arg in \"$@\"; do printf '[%s]' \"$arg\"; done\nprintf '\\n'\ncat\n",
        0o755,
    );
    let mut child = Command::new(camoufox_bin())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", temp_dir.path().display()),
        )
        .args(["echo", "a b", "", "--json", "--socket", "x"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("run camoufox");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from stdin\n")
        .unwrap();
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[a b][][--json][--socket][x]\nfrom stdin\n"
    );
}
