//! `velocity doctor` and `velocity init` end to end, against fake tools on
//! PATH (no device is touched). The checks and the generated files have
//! their own unit tests (`velocity-doctor`, `commands/init.rs` goldens).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn velocity() -> &'static str {
    env!("CARGO_BIN_EXE_velocity")
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        // Cargo's scratch dir, not $TMPDIR: on macOS the first exec of a new
        // script under /var/folders can stall for seconds while it is scanned.
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("vonboard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["bin", "home", "proj"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        Self { dir }
    }

    fn tool(&self, name: &str, body: &str) {
        let p = self.dir.join("bin").join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn proj(&self) -> PathBuf {
        self.dir.join("proj")
    }

    /// `velocity <args>` in `proj/` with only the fake tools on PATH.
    fn run(&self, args: &[&str]) -> Output {
        // A generous per-command deadline for `doctor`: fake tools on a
        // loaded host can be slow to start.
        let mut argv: Vec<&str> = args.to_vec();
        if args.first() == Some(&"doctor") {
            argv.extend(["--timeout", "20000"]);
        }
        Command::new(velocity())
            .args(&argv)
            .current_dir(self.proj())
            .env_clear()
            .env(
                "PATH",
                format!("{}:/bin:/usr/bin", self.dir.join("bin").display()),
            )
            .env("HOME", self.dir.join("home"))
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const ADB: &str = r#"while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  version) printf 'Android Debug Bridge version 1.0.41\nVersion 35.0.2-1\n';;
  devices) printf 'List of devices attached\nemulator-5554\tdevice product:sdk model:Pixel_7\n';;
  shell) case "$*" in *getprop*) printf '34\n14\nPixel 7\n';; *pm\ path*) echo package:/data/app/base.apk;; esac;;
esac"#;

#[test]
fn doctor_json_reports_every_area_and_exits_0_when_usable() {
    let s = Sandbox::new("json");
    s.tool("adb", ADB);
    let out = s.run(&["doctor", "--json", "--platform", "android"]);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&out)));
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    for id in [
        "host.os",
        "host.resources",
        "host.leases",
        "android.adb_server",
        "android.devices",
        "android.api",
        "web.chrome",
        "ai.keys",
        "project.suites",
    ] {
        assert!(ids.contains(&id), "{id} missing: {ids:?}");
    }
    let devices = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "android.devices")
        .unwrap();
    assert_eq!(devices["status"], "ok", "{devices}");
    assert_ne!(v["verdict"], "blocked", "{}", text(&out));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[test]
fn doctor_blocks_with_2_on_a_broken_suite_and_3_on_a_missing_toolchain() {
    let s = Sandbox::new("block");
    s.tool("adb", ADB);
    std::fs::write(
        s.proj().join("velocity.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: t\n    steps:\n      - frobnicate: {}\n",
    )
    .unwrap();
    let out = s.run(&["doctor"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("✗ suites"), "{}", text(&out));
    assert!(
        text(&out).contains("Run: velocity validate velocity.yaml"),
        "{}",
        text(&out)
    );

    let s = Sandbox::new("noadb");
    let out = s.run(&["doctor", "--platform", "android"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("✗ adb"), "{}", text(&out));
    assert!(text(&out).contains("Verdict: blocked"), "{}", text(&out));
}

#[test]
fn doctor_leaves_stale_leases_alone_and_reports_them_harmless() {
    // A lease left by an exited process blocks nobody (lease.rs): the
    // doctor reports it as fine instead of warning after every run, and
    // --fix has nothing to clear.
    let s = Sandbox::new("fix");
    s.tool("adb", ADB);
    let leases = s.dir.join("home/.velocity/leases");
    std::fs::create_dir_all(&leases).unwrap();
    std::fs::write(
        leases.join("emulator-5556.lock"),
        "pid=999999996 cmd=velocity run\n",
    )
    .unwrap();
    let out = s.run(&["doctor", "--fix"]);
    let t = text(&out);
    assert!(t.contains("blocks nothing"), "{t}");
    assert!(!t.contains("cleared 1 of 1 stale lease file"), "{t}");
    assert_eq!(
        std::fs::read_to_string(leases.join("emulator-5556.lock")).unwrap(),
        "pid=999999996 cmd=velocity run\n"
    );
}

#[test]
fn init_refuses_to_overwrite_and_needs_a_device_or_url() {
    let s = Sandbox::new("init");
    s.tool(
        "adb",
        "case \"$1\" in devices) printf 'List of devices attached\\n';; esac",
    );

    // Nothing running: a device problem, at once.
    let out = s.run(&["init", "--platform", "android", "--yes"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        text(&out).contains("velocity devices up --android 1"),
        "{}",
        text(&out)
    );

    // Web needs a URL.
    let out = s.run(&["init", "--platform", "web", "--yes"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));

    // An existing suite is never overwritten without --force, and nothing
    // on the device is touched first.
    let dir = s.proj().join("velocity");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.yaml"), "keep me").unwrap();
    let out = s.run(&[
        "init",
        "--platform",
        "android",
        "--app-id",
        "com.example.app",
        "--device",
        "emulator-5554",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("velocity/app.yaml already exists"),
        "{}",
        text(&out)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("app.yaml")).unwrap(),
        "keep me"
    );
    assert!(!Path::new(&s.proj().join(".gitignore")).exists());
}
