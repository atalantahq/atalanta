//! C4 (VEL-174): one colour switch for everything `velocity` prints. The
//! runner wrote raw ANSI escapes whatever `NO_COLOR` or the terminal said.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vcolor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("suite.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: quick\n    steps:\n      - wait: { ms: 1 }\n",
    )
    .unwrap();
    dir
}

/// stdout of a headless run with `extra` flags and `env`.
fn run(name: &str, extra: &[&str], env: &[(&str, &str)]) -> String {
    let dir = scratch(name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_velocity"));
    cmd.args(["run", "suite.yaml", "--driver", "headless"])
        .args(extra)
        .current_dir(&dir)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn no_color_turns_off_every_escape_including_the_runners() {
    let out = run(
        "nocolor",
        &[],
        &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")],
    );
    assert!(out.contains("quick"), "{out}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

#[test]
fn color_never_and_a_pipe_mean_plain_text() {
    // A pipe (not a terminal), no variables: plain.
    let out = run("pipe", &[], &[]);
    assert!(!out.contains('\x1b'), "{out:?}");
    let out = run("never", &["--color", "never"], &[("CLICOLOR_FORCE", "1")]);
    assert!(!out.contains('\x1b'), "{out:?}");
}

#[test]
fn color_always_and_clicolor_force_colour_a_pipe() {
    let out = run("always", &["--color", "always"], &[("NO_COLOR", "1")]);
    // The runner's result line and the CLI's own `=>` lines both.
    assert!(out.contains("\x1b[32m●\x1b[0m quick"), "{out:?}");
    let out = run("force", &[], &[("CLICOLOR_FORCE", "1")]);
    assert!(out.contains('\x1b'), "{out:?}");
}
