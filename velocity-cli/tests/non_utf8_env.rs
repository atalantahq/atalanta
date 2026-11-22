//! A variable in the environment that is not UTF-8 must not panic the
//! client of `velocity run --daemon`: it builds the request from the whole
//! environment with `std::env::vars()`, which panics on one.
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;

#[test]
fn a_non_utf8_variable_does_not_panic_the_daemon_client() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("suite.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: quick\n    steps:\n      - wait: { ms: 1 }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_velocity"))
        .args(["run", "suite.yaml", "--driver", "headless", "--daemon"])
        .current_dir(dir.path())
        .env("OLDPWD", OsString::from_vec(b"/tmp/caf\xe9".to_vec()))
        // No daemon there: the run happens in this process.
        .env("VELOCITY_DAEMON_SOCKET", dir.path().join("none.sock"))
        .env("VELOCITY_LEASE_DIR", dir.path().join("leases"))
        .env("NO_COLOR", "1")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}
