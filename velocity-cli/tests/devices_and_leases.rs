//! D-25: `--device` is checked up front (exit 3, at once), and the
//! cross-process lease policy (`VELOCITY_LEASES=off`, a fatal lease dir
//! error with a hint).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn velocity() -> &'static str {
    env!("CARGO_BIN_EXE_velocity")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vdev-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn suite(dir: &Path) {
    std::fs::write(
        dir.join("suite.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n",
    )
    .unwrap();
}

#[test]
fn an_unknown_device_exits_3_at_once() {
    let dir = scratch("unknown");
    suite(&dir);
    // An adb that knows one emulator and does nothing else.
    let adb = dir.join("adb");
    std::fs::write(
        &adb,
        "#!/bin/sh\ncase \"$*\" in *devices*) printf 'List of devices attached\\nemulator-5554\\tdevice product:p model:Pixel\\n'; exit 0;; esac\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let out = Command::new(velocity())
        .args([
            "run",
            "suite.yaml",
            "--platform",
            "android",
            "--device",
            "emulator-9999",
        ])
        .current_dir(&dir)
        .env("VELOCITY_ADB_PATH", &adb)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        text(&out).contains("--device emulator-9999: no such device"),
        "{}",
        text(&out)
    );
    assert!(text(&out).contains("emulator-5554"), "{}", text(&out));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `velocity run suite.yaml --platform android <args>` against an adb
/// script `adb_body` (`$*` is its arguments).
fn run_with_adb(name: &str, adb_body: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let dir = scratch(name);
    suite(&dir);
    let adb = dir.join("adb");
    std::fs::write(&adb, format!("#!/bin/sh\n{adb_body}\n")).unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Command::new(velocity());
    cmd.args(["run", "suite.yaml", "--platform", "android"])
        .args(args)
        .current_dir(&dir)
        .env("VELOCITY_ADB_PATH", &adb)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1")
        .env_remove("VELOCITY_ADB_SERVERS")
        .env_remove("GITHUB_ACTIONS");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

const TWO_EMULATORS: &str = "case \"$*\" in *devices*) printf 'List of devices attached\\nemulator-5554\\tdevice product:p model:Pixel_7\\nemulator-5556\\tdevice product:p model:Pixel_8\\n'; exit 0;; esac\nexit 1";

// Review d4 M2: the up-front check matched exactly and case-sensitively,
// so `--parallel` filters the farm has always taken exited 3: a name
// substring (`pixel`), an id list in another case. It now uses the farm's
// own rule.
#[test]
fn the_device_check_takes_what_the_farm_takes() {
    for device in ["pixel", "EMULATOR-5554,emulator-5556", "*5556"] {
        let out = run_with_adb(
            "farmrule",
            TWO_EMULATORS,
            &["--parallel", "2", "--device", device, "--timeout", "20000"],
            &[],
        );
        assert!(
            !text(&out).contains("no such device"),
            "--device {device}: {}",
            text(&out)
        );
    }
    // A list names exact ids: a name in a list is still unknown.
    let out = run_with_adb(
        "farmrule-list",
        TWO_EMULATORS,
        &["--parallel", "2", "--device", "emulator-5554,Pixel_8"],
        &[],
    );
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("no such device"), "{}", text(&out));
}

// Review d7 m8: a sequential run uses `--device` as the device itself, so
// a substring (a typo, `emulator-555`) passed the check and then failed
// every step. Without `--parallel`, it is an exact id or name, ignoring
// case.
#[test]
fn a_sequential_device_is_an_exact_id_or_name() {
    for device in ["emulator-555", "pixel", "*5556"] {
        let out = run_with_adb("seqtypo", TWO_EMULATORS, &["--device", device], &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "--device {device}: {}",
            text(&out)
        );
        assert!(
            text(&out).contains(&format!("--device {device}: no such device")),
            "{}",
            text(&out)
        );
    }
    for device in ["EMULATOR-5554", "pixel_8"] {
        let out = run_with_adb(
            "seqexact",
            TWO_EMULATORS,
            &["--device", device, "--timeout", "20000"],
            &[],
        );
        assert!(
            !text(&out).contains("no such device"),
            "--device {device}: {}",
            text(&out)
        );
    }
}

// Review d4 m3/m4: a listing that fails is "don't know", not "no such
// device"; a remote `host:port/serial` adb id can't be listed without
// VELOCITY_ADB_SERVERS, so it isn't checked.
#[test]
fn the_device_check_steps_aside_when_it_cannot_know() {
    let out = run_with_adb(
        "listfail",
        "echo 'adb server wedged' >&2; exit 1",
        &["--device", "emulator-5554", "--timeout", "20000"],
        &[],
    );
    assert!(!text(&out).contains("no such device"), "{}", text(&out));
    assert!(
        text(&out).contains("not checking --device emulator-5554"),
        "{}",
        text(&out)
    );
    let out = run_with_adb(
        "remote",
        TWO_EMULATORS,
        &[
            "--device",
            "10.0.0.5:5037/emulator-5558",
            "--timeout",
            "20000",
        ],
        &[],
    );
    assert!(!text(&out).contains("no such device"), "{}", text(&out));
}

#[test]
fn leases_can_be_turned_off_and_a_bad_lease_dir_says_what_to_do() {
    let dir = scratch("leases");
    suite(&dir);
    // A file where the lease dir's parent should be: every lease fails.
    std::fs::write(dir.join("notadir"), "x").unwrap();
    let run = |off: bool| {
        let mut cmd = Command::new(velocity());
        cmd.args([
            "run",
            "suite.yaml",
            "--driver",
            "headless",
            "--report",
            "json",
        ])
        .current_dir(&dir)
        .env("VELOCITY_LEASE_DIR", dir.join("notadir/leases"))
        .env("NO_COLOR", "1")
        .env_remove("GITHUB_ACTIONS");
        if off {
            cmd.env("VELOCITY_LEASES", "off");
        } else {
            cmd.env_remove("VELOCITY_LEASES");
        }
        cmd.output().unwrap()
    };
    let out = run(false);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("VELOCITY_LEASE_DIR"), "{}", text(&out));
    let out = run(true);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}
