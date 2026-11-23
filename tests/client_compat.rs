//! Client compatibility with older daemon response shapes, without a browser.
#![cfg(feature = "cli")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn old_daemon(response: &str, args: &[&str], socket_from_env: bool) -> (Output, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let response = format!("{response}\n");
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        stream.write_all(response.as_bytes()).unwrap();
        serde_json::from_str(&line).unwrap()
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_camoufox"));
    command.env_remove("CAMOUFOX_SOCKET");
    if socket_from_env {
        command.env("CAMOUFOX_SOCKET", &socket);
    } else {
        command.arg("--socket").arg(&socket);
    }
    let output = command.args(args).output().unwrap();
    (output, worker.join().unwrap())
}

const OLD_LAUNCH: &str =
    r#"{"ok":true,"data":{"instance_id":"00000001","version":"test","pid":123}}"#;
const PROFILE_UNSUPPORTED: &str =
    "daemon does not support --profile-dir; restart it: camoufox shutdown && camoufox serve";

#[test]
fn persistent_launch_rejects_old_daemon_in_both_output_modes() {
    for json in [false, true] {
        let mut args = vec!["launch", "--profile-dir", "/absolute/profile"];
        if json {
            args.insert(0, "--json");
        }
        let (output, request) = old_daemon(OLD_LAUNCH, &args, false);
        assert_eq!(request["params"]["profile_dir"], "/absolute/profile");
        assert_eq!(output.status.code(), Some(1));
        if json {
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                response,
                serde_json::json!({"ok": false, "error": PROFILE_UNSUPPORTED, "kind": "daemon"})
            );
            assert!(output.stderr.is_empty());
        } else {
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                format!("error: {PROFILE_UNSUPPORTED}\n")
            );
        }
    }
}

#[test]
fn temporary_launch_accepts_old_daemon() {
    let (output, _) = old_daemon(OLD_LAUNCH, &["--json", "launch"], false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["data"]["instance_id"],
        "00000001"
    );
}

#[test]
fn persistent_launch_accepts_confirmed_profile_and_preserves_daemon_errors() {
    for (response, code) in [
        (
            r#"{"ok":true,"data":{"instance_id":"00000001","profile_dir":"/canonical/profile"}}"#,
            0,
        ),
        (
            r#"{"ok":false,"error":"invalid argument: profile directory already in use"}"#,
            1,
        ),
    ] {
        let (output, _) = old_daemon(
            response,
            &["--json", "launch", "--profile-dir", "/absolute/profile"],
            false,
        );
        assert_eq!(output.status.code(), Some(code));
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let expected: serde_json::Value = serde_json::from_str(response).unwrap();
        assert_eq!(actual["ok"], expected["ok"]);
        assert_eq!(actual["data"], expected["data"]);
        assert_eq!(actual["error"], expected["error"]);
    }
}

#[test]
fn errors_from_old_daemon_get_kinds_without_changing_messages() {
    for (message, kind) in [
        ("page p1 was closed", "page_closed"),
        ("selector not found: #missing", "selector_not_found"),
    ] {
        let response = serde_json::json!({"ok":false,"error":message}).to_string();
        let (output, _) = old_daemon(&response, &["--json", "ping"], false);
        assert_eq!(output.status.code(), Some(1));
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            actual,
            serde_json::json!({"ok":false,"error":message,"kind":kind})
        );
    }
}

#[test]
fn daemon_provided_unknown_kind_is_preserved() {
    let response = r#"{"ok":false,"error":"page p1 was closed","kind":"future_kind"}"#;
    let (output, _) = old_daemon(response, &["--json", "ping"], false);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::from_str::<serde_json::Value>(response).unwrap()
    );
}

#[test]
fn client_honours_socket_environment_default() {
    let (output, request) = old_daemon(r#"{"ok":true}"#, &["--json", "ping"], true);
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert_eq!(request["method"], "Ping");
}
