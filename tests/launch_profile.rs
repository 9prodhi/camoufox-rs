//! Persistent profile confirmation through the daemon protocol.
#![cfg(feature = "cli")]

use camoufox::cli::client::send_request;
use camoufox::cli::instance::InstanceManager;
use camoufox::cli::ipc::DaemonRequest;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn launch_response_confirms_canonical_persistent_profile_only() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let alias = dir.path().join("profile-alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    let socket = dir.path().join("daemon.sock");
    let stop = Arc::new(AtomicBool::new(false));
    struct DaemonGuard {
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    }
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join().unwrap();
            }
        }
    }
    let _guard = DaemonGuard {
        stop: Arc::clone(&stop),
        worker: Some(std::thread::spawn({
            let socket = socket.clone();
            let stop = Arc::clone(&stop);
            move || camoufox::cli::daemon::serve(&socket, Arc::new(InstanceManager::new()), stop)
        })),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "private daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    for persistent in [true, false] {
        let response = send_request(
            &socket,
            &DaemonRequest::Launch {
                headless: Some(true),
                executable: None,
                downloads_dir: None,
                bypass_csp: false,
                dialogs: Default::default(),
                profile_dir: persistent.then(|| alias.display().to_string()),
            },
        )
        .unwrap();
        assert!(response.ok, "{response:?}");
        let data = response.data.unwrap();
        if persistent {
            assert_eq!(
                data["profile_dir"],
                profile.canonicalize().unwrap().to_str().unwrap()
            );
        } else {
            assert!(data.get("profile_dir").is_none(), "{data}");
        }
        assert!(
            send_request(
                &socket,
                &DaemonRequest::Stop {
                    instance_id: data["instance_id"].as_str().unwrap().to_owned(),
                }
            )
            .unwrap()
            .ok
        );
    }
    assert!(profile.join("prefs.js").is_file(), "profile survives stop");
}
