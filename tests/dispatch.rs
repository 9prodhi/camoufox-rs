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

    write_script(temp_dir.path(), "camoufox-hello", "#!/bin/sh\necho \"args: $@\"\necho \"socket: $CAMOUFOX_SOCKET\"\necho \"json: $CAMOUFOX_JSON\"\nexit 7\n", 0o755);

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = temp_dir.path().as_os_str().to_os_string();
    new_path.push(":");
    new_path.push(&original_path);

    let output = Command::new(camoufox_bin())
        .env("PATH", &new_path)
        .env("XDG_RUNTIME_DIR", temp_dir.path())
        .env_remove("CAMOUFOX_SOCKET")
        .env_remove("CAMOUFOX_JSON")
        .arg("--json")
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
    assert_eq!(
        stdout,
        format!(
            "args: foo bar --flag baz\nsocket: {}/camoufox/daemon.sock\njson: 1\n",
            temp_dir.path().display()
        )
    );
}

#[test]
fn external_dispatch_respects_custom_socket_flag() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    write_script(
        temp_dir.path(),
        "camoufox-hello",
        "#!/bin/sh\necho \"socket: $CAMOUFOX_SOCKET\"\nexit 7\n",
        0o755,
    );

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = temp_dir.path().as_os_str().to_os_string();
    new_path.push(":");
    new_path.push(&original_path);

    let custom_socket = "/tmp/test-camoufox-custom.sock";
    let output = Command::new(camoufox_bin())
        .env("PATH", &new_path)
        .env("CAMOUFOX_SOCKET", "/unused/environment.sock")
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
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("executed");
    for name in ["some/path", "../cmd"] {
        let script = format!("camoufox-{name}");
        fs::create_dir_all(temp.path().join(&script).parent().unwrap()).unwrap();
        write_script(temp.path(), &script,
            "#!/bin/sh\necho SEPARATOR_EXECUTED\nprintf ran > \"$CAMOUFOX_DISPATCH_MARKER\"\nexit 42\n", 0o755);
        let output = Command::new(camoufox_bin())
            .env("PATH", temp.path())
            .env("CAMOUFOX_DISPATCH_MARKER", &marker)
            .arg(name)
            .output()
            .expect("run camoufox");
        assert_eq!(output.status.code(), Some(2), "exit code must be 2");
        assert!(
            output.stdout.is_empty(),
            "separator executable must not run"
        );
        assert!(
            !marker.exists(),
            "separator executable must not create its marker"
        );
        let expected = PINNED_PATH_SEPARATOR_STDERR.replace("some/path", name);
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
    }
}

#[test]
fn external_dispatch_builtin_names_unaffected() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    // Place a shadowing binary on PATH with the same name as a built-in command.
    write_script(
        temp_dir.path(),
        "camoufox-list",
        "#!/bin/sh\necho SHADOWED_EXTERNAL_CALLED\nexit 42\n",
        0o755,
    );

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

/// PATH order selects the first executable, including executable symlink targets.
#[cfg(unix)]
#[test]
fn external_dispatch_first_executable_on_path_wins_and_symlinks_resolve() {
    let first = tempfile::tempdir().expect("first dir");
    let second = tempfile::tempdir().expect("second dir");
    let target = write_script(
        first.path(),
        "real-tool",
        "#!/bin/sh\necho \"first $0\"\n",
        0o755,
    );
    std::os::unix::fs::symlink(&target, first.path().join("camoufox-tool")).unwrap();
    write_script(
        second.path(),
        "camoufox-tool",
        "#!/bin/sh\necho second\n",
        0o755,
    );
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();
    let output = Command::new(camoufox_bin())
        .env("PATH", &path)
        .arg("tool")
        .output()
        .expect("run camoufox");
    assert_eq!(output.status.code(), Some(0));
    // The child is run by the PATH entry's own path, not the link target.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("first {}\n", first.path().join("camoufox-tool").display())
    );

    // Reversed order picks the other binary.
    let path = std::env::join_paths([second.path(), first.path()]).unwrap();
    let output = Command::new(camoufox_bin())
        .env("PATH", &path)
        .arg("tool")
        .output()
        .expect("run camoufox");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "second\n");

    // A dangling symlink is skipped like a missing entry.
    fs::remove_file(&target).unwrap();
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();
    let output = Command::new(camoufox_bin())
        .env("PATH", &path)
        .arg("tool")
        .output()
        .expect("run camoufox");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "second\n");
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

#[test]
fn external_dispatch_resolves_relative_socket_against_client_cwd() {
    let temp = tempfile::tempdir().unwrap();
    write_script(
        temp.path(),
        "camoufox-tool",
        "#!/bin/sh\nprintf '%s' \"$CAMOUFOX_SOCKET\"\n",
        0o755,
    );
    let output = Command::new(camoufox_bin())
        .current_dir(temp.path())
        .env("PATH", temp.path())
        .args(["--socket", "relative.sock", "tool"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        temp.path()
            .join("relative.sock")
            .as_os_str()
            .as_encoded_bytes()
    );
}

#[test]
fn external_dispatch_skips_relative_and_empty_path_entries() {
    let temp = tempfile::tempdir().unwrap();
    let absolute = temp.path().join("absolute");
    let relative = temp.path().join("relative");
    fs::create_dir(&absolute).unwrap();
    fs::create_dir(&relative).unwrap();
    write_script(
        temp.path(),
        "camoufox-tool",
        "#!/bin/sh\necho unsafe-current\n",
        0o755,
    );
    write_script(
        &relative,
        "camoufox-tool",
        "#!/bin/sh\necho unsafe-relative\n",
        0o755,
    );
    write_script(
        &absolute,
        "camoufox-tool",
        "#!/bin/sh\necho absolute\n",
        0o755,
    );
    for prefix in ["", ".", "relative"] {
        let path = std::env::join_paths([std::path::Path::new(prefix), &absolute]).unwrap();
        let output = Command::new(camoufox_bin())
            .current_dir(temp.path())
            .env("PATH", path)
            .arg("tool")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, b"absolute\n");
    }
}

#[test]
fn external_dispatch_preserves_non_utf8_socket_path() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"runtime-\xff".to_vec()));
    write_script(
        temp.path(),
        "camoufox-tool",
        "#!/bin/sh\nprintf '%s' \"$CAMOUFOX_SOCKET\"\n",
        0o755,
    );
    let output = Command::new(camoufox_bin())
        .env("PATH", temp.path())
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("CAMOUFOX_SOCKET")
        .arg("tool")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        runtime
            .join("camoufox/daemon.sock")
            .as_os_str()
            .as_encoded_bytes()
    );
}
