//! `velocity run <folder>`: tag selection across the folder (M-1) and
//! Ctrl-C between and during suites (M-2). Headless driver, no device.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn velocity() -> &'static str {
    env!("CARGO_BIN_EXE_velocity")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vfolder-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("suites")).unwrap();
    dir
}

/// A one-test suite that waits `ms`, tagged `tags`.
fn suite(dir: &Path, file: &str, tags: &str, ms: u64) {
    std::fs::write(
        dir.join("suites").join(file),
        format!(
            "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: t\n    tags: [{tags}]\n    steps:\n      - wait: {{ ms: {ms} }}\n"
        ),
    )
    .unwrap();
}

fn run(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::new(velocity());
    cmd.args(["run", "suites", "--driver", "headless", "--report", "json"])
        .args(extra)
        .current_dir(dir)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1");
    cmd
}

#[test]
fn tags_need_to_match_in_one_suite_of_the_folder_not_all() {
    let dir = scratch("tags");
    suite(&dir, "a.yaml", "smoke", 1);
    suite(&dir, "b.yaml", "regression", 1);
    let out = run(&dir, &["--tags", "smoke"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // No suite matches: still a usage error.
    let out = run(&dir, &["--tags", "smkoe"]).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder named like a suite (`old.yaml/`) is not a suite, and `a.yaml`
/// next to `a.yml` each keep their own reports: the folder's file stem named
/// both artifact folders, so the second run overwrote the first's results.
#[test]
fn folders_named_like_suites_are_skipped_and_equal_stems_do_not_share_results() {
    let dir = scratch("stems");
    suite(&dir, "a.yaml", "smoke", 1);
    suite(&dir, "a.yml", "smoke", 1);
    std::fs::create_dir_all(dir.join("suites/old.yaml")).unwrap();
    let out = run(&dir, &[]).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    for stem in ["a.yaml", "a.yml"] {
        assert!(
            dir.join("velocity-results")
                .join(stem)
                .join("results.json")
                .exists(),
            "{stem}: {text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run the folder, send SIGINT `delay` after `marker` shows on stdout, and
/// return (exit code, stdout, stderr).
fn interrupt_after(dir: &Path, marker: &str, delay: Duration) -> (Option<i32>, String, String) {
    let mut child = run(dir, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line.clone());
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains(marker) => break,
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill();
                panic!("never saw {marker}");
            }
        }
    }
    std::thread::sleep(delay);
    // SAFETY: plain kill(2) on our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("did not exit after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (
        status.code(),
        reader.join().unwrap(),
        err_reader.join().unwrap(),
    )
}

#[test]
fn ctrl_c_in_the_first_suite_stops_the_folder() {
    let dir = scratch("stop");
    suite(&dir, "a.yaml", "x", 8000);
    suite(&dir, "b.yaml", "x", 1);
    let (code, out, err) = interrupt_after(&dir, "a.yaml", Duration::from_millis(1500));
    assert_eq!(code, Some(130), "{out}{err}");
    assert!(!out.contains("b.yaml"), "the next suite ran:\n{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_c_in_a_later_suite_still_cleans_up() {
    let dir = scratch("later");
    suite(&dir, "a.yaml", "x", 1);
    suite(&dir, "b.yaml", "x", 8000);
    let (code, out, err) = interrupt_after(&dir, "b.yaml", Duration::from_millis(1500));
    assert_eq!(code, Some(130), "{out}{err}");
    assert!(
        err.contains("received") && err.contains("cleaning up"),
        "exited without cleanup:\n{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// D-44/45: `--framework react_native` with the headless driver is a
/// configuration error naming the fix, not the old sidecar stub.
#[test]
fn react_native_headless_is_a_config_error() {
    let dir = scratch("rn");
    suite(&dir, "a.yaml", "smoke", 1);
    let out = run(&dir, &["--framework", "rn"]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("React Native") && err.contains("headless"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
