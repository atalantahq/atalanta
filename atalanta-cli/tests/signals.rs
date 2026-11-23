//! VEL-031: SIGTERM stops the daemon through its cleanup (socket removed),
//! both fresh and after it has served a run — once a run had installed
//! tokio's handlers, the daemon ignored SIGTERM and SIGINT for good.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn atalanta() -> &'static str {
    env!("CARGO_BIN_EXE_atalanta")
}

fn scratch(name: &str) -> PathBuf {
    // Unix socket paths must stay short.
    let dir = PathBuf::from(format!("/tmp/vsig-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_daemon(dir: &Path) -> Child {
    let child = Command::new(atalanta())
        .args(["daemon", "start"])
        .current_dir(dir)
        .env("ATALANTA_DAEMON_SOCKET", dir.join("d.sock"))
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.join("d.sock").exists() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

/// Wait up to `secs` for `child` to exit; kill it and fail otherwise.
fn exits_within(child: &mut Child, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

fn sigterm(child: &Child) {
    // SAFETY: plain kill(2) on our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
}

#[test]
fn a_fresh_daemon_cleans_up_on_sigterm() {
    let dir = scratch("fresh");
    let mut daemon = start_daemon(&dir);
    sigterm(&daemon);
    assert!(exits_within(&mut daemon, 10), "daemon ignored SIGTERM");
    assert!(!dir.join("d.sock").exists(), "socket left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_daemon_still_stops_on_sigterm_after_serving_a_run() {
    let dir = scratch("served");
    std::fs::write(
        dir.join("ok.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: waits\n    steps:\n      - wait: { ms: 10 }\n",
    )
    .unwrap();
    let mut daemon = start_daemon(&dir);
    let run = Command::new(atalanta())
        .args([
            "run", "ok.yaml", "--driver", "headless", "--daemon", "--report", "json",
        ])
        .current_dir(&dir)
        .env("ATALANTA_DAEMON_SOCKET", dir.join("d.sock"))
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    sigterm(&daemon);
    assert!(
        exits_within(&mut daemon, 10),
        "daemon ignored SIGTERM after a run"
    );
    assert!(!dir.join("d.sock").exists(), "socket left behind");
    let _ = std::fs::remove_dir_all(&dir);
}
