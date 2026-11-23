//! M-5: a daemon client that goes away stops its run, whenever it leaves:
//! before the run has a stop handle (while it parses and builds its
//! driver), or in the middle of a folder run, where each suite gets its own
//! handle. The daemon serves one run at a time, so the next request being
//! answered promptly shows the run ended. Headless driver, no device.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn velocity() -> &'static str {
    env!("CARGO_BIN_EXE_velocity")
}

fn scratch(name: &str) -> PathBuf {
    // Unix socket paths must stay short.
    let dir = PathBuf::from(format!("/tmp/vdis-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("suites")).unwrap();
    dir
}

struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on our own child.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start_daemon(dir: &Path) -> Daemon {
    let child = Command::new(velocity())
        .args(["daemon", "start"])
        .current_dir(dir)
        .env("VELOCITY_DAEMON_SOCKET", dir.join("d.sock"))
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
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
    Daemon(child, dir.to_path_buf())
}

/// A one-test suite that waits `ms`.
fn suite(path: &Path, ms: u64) {
    std::fs::write(
        path,
        format!(
            "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: t\n    steps:\n      - wait: {{ ms: {ms} }}\n"
        ),
    )
    .unwrap();
}

/// Send a run request for `argv` the way `velocity run --daemon` does.
fn request_run(dir: &Path, argv: &[&str]) -> UnixStream {
    let mut s = UnixStream::connect(dir.join("d.sock")).unwrap();
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push((
        "VELOCITY_LEASE_DIR".into(),
        dir.join("leases").to_string_lossy().into_owned(),
    ));
    env.push(("NO_COLOR".into(), "1".into()));
    let req = serde_json::json!({
        "kind": "run",
        "cwd": dir.to_string_lossy(),
        "argv": argv,
        "env": env,
        "color": false,
    });
    s.write_all(req.to_string().as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    s
}

/// How long the daemon takes to answer a ping (it serves one connection
/// at a time, so this waits for the run in progress).
fn ping(dir: &Path) -> Duration {
    let start = Instant::now();
    let mut s = UnixStream::connect(dir.join("d.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(b"{\"kind\":\"ping\"}\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.contains("pong"), "no pong: {out:?}");
    start.elapsed()
}

#[test]
fn a_client_that_leaves_before_the_run_starts_stops_it() {
    let dir = scratch("early");
    suite(&dir.join("long.yaml"), 15_000);
    let _daemon = start_daemon(&dir);
    let client = request_run(
        &dir,
        &["long.yaml", "--driver", "headless", "--report", "json"],
    );
    // Gone at once: before the run parsed its suite or built its driver.
    drop(client);
    std::thread::sleep(Duration::from_millis(200));
    let waited = ping(&dir);
    assert!(
        waited < Duration::from_secs(8),
        "the run went on unseen for {waited:?}"
    );
}

#[test]
fn a_client_that_leaves_mid_folder_stops_the_rest_of_the_folder() {
    let dir = scratch("folder");
    for name in ["a", "b", "c", "d"] {
        suite(&dir.join("suites").join(format!("{name}.yaml")), 4_000);
    }
    let _daemon = start_daemon(&dir);
    let mut client = request_run(
        &dir,
        &["suites", "--driver", "headless", "--report", "json"],
    );
    // Leave once the first suite is running.
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let started = Instant::now();
    while !String::from_utf8_lossy(&seen).contains("Driver") {
        let n = client.read(&mut buf).unwrap();
        assert!(
            n > 0,
            "the run ended early: {}",
            String::from_utf8_lossy(&seen)
        );
        seen.extend_from_slice(&buf[..n]);
        assert!(started.elapsed() < Duration::from_secs(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    drop(client);
    let waited = ping(&dir);
    // Four suites of 4 s: the first one's remainder at most, not the rest.
    assert!(
        waited < Duration::from_secs(8),
        "later suites ran unseen: {waited:?}"
    );
}
