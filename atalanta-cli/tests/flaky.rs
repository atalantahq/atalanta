// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta history flaky`: per-test flake rate over the last N runs
//! (C4: the rolling flake rate CI can gate on).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vflaky-{name}-{}", std::process::id()));
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

/// A history file with one test's recent outcomes (pass/flaky/fail).
fn history(dir: &Path, tests: &[(&str, &[&str])]) {
    let mut outcomes = serde_json::Map::new();
    for (name, o) in tests {
        outcomes.insert(
            name.to_string(),
            serde_json::json!(o.iter().map(|o| o.to_string()).collect::<Vec<_>>()),
        );
    }
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("atalanta-history.json"),
        serde_json::json!({ "version": 3, "durations": {}, "outcomes": outcomes }).to_string(),
    )
    .unwrap()
}

fn flaky(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atalanta"))
        .args(["history", "flaky"])
        .args([
            "--file",
            dir.join("atalanta-history.json").to_str().unwrap(),
        ])
        .args(args)
        .current_dir(dir)
        .env_remove("GITHUB_ACTIONS")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn history_flaky_table_and_exit() {
    let dir = scratch("table");
    history(
        &dir,
        &[
            ("login", &["pass", "flaky", "pass", "flaky", "pass"]), // 2 retries / (5 + 4) = 0.222
            ("pay", &["pass", "pass", "pass", "pass", "pass"]),     // 0/5
        ],
    );
    let out = flaky(&[], &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(all.contains("login"), "{all}");
    assert!(all.contains("pay"), "{all}");
    assert!(all.contains("0.222"), "{all}");
    assert!(all.contains("0.000"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_flaky_under_threshold_exits_zero() {
    let dir = scratch("under");
    history(
        &dir,
        &[("login", &["pass", "pass", "pass", "pass", "pass"])],
    );
    let out = flaky(&[], &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_flaky_fewer_than_five_runs_never_trips() {
    let dir = scratch("few");
    // 4 runs, all flaky: 4 / (4 + 3) = 0.571, but below FLAG_MIN_RUNS, so exit 0.
    history(&dir, &[("login", &["flaky", "flaky", "flaky", "flaky"])]);
    let out = flaky(&[], &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("0.571"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_flaky_window_cuts() {
    let dir = scratch("window");
    // 8 runs: the last 5 are flaky, flaky, pass, pass, flaky -> 3 / (5 + 4).
    history(
        &dir,
        &[(
            "login",
            &[
                "flaky", "flaky", "flaky", "flaky", "flaky", "pass", "pass", "flaky",
            ],
        )],
    );
    let out = flaky(&["--window", "5"], &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(all.contains("0.333"), "{all}");
    // A window under 5 could never reach the 5 runs a test needs to be
    // judged, so the gate never tripped: a usage error now. So is a
    // threshold outside 0..1 (NaN never compared greater).
    for args in [
        &["--window", "3"][..],
        &["--max-rate", "NaN"],
        &["--max-rate", "2"],
    ] {
        let out = flaky(args, &dir);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_flaky_json() {
    let dir = scratch("json");
    history(
        &dir,
        &[("login", &["pass", "flaky", "pass", "pass", "pass"])],
    );
    let out = flaky(&["--json"], &dir);
    let all = text(&out);
    // 5 runs, one passed on retry: 1 / (5 + 4) > maxRate 0.05, exit 1.
    assert_eq!(out.status.code(), Some(1), "{all}");
    let v: serde_json::Value = serde_json::from_str(&all).unwrap();
    assert_eq!(v["window"], 20);
    assert_eq!(v["maxRate"], 0.05);
    assert_eq!(v["tests"][0]["name"], "login");
    assert_eq!(v["tests"][0]["runs"], 5);
    assert_eq!(v["tests"][0]["flaky"], 1);
    assert!(v["tests"][0].get("infra").is_none());
    assert_eq!(v["tests"][0]["flips"], 0);
    assert!((v["tests"][0]["rate"].as_f64().unwrap() - 1.0 / 9.0).abs() < 1e-9);
    assert_eq!(v["over"].as_array().unwrap(), &[serde_json::json!("login")]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A CI without retries: pass and fail alternating is flaky, as `atalanta
/// run`'s "history: flakiness" says (the gate read it as 0 and passed).
#[test]
fn history_flaky_counts_pass_fail_flips() {
    let dir = scratch("flips");
    history(
        &dir,
        &[("login", &["pass", "fail", "pass", "fail", "pass", "fail"])],
    );
    let out = flaky(&["--json"], &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(1), "{all}");
    let v: serde_json::Value = serde_json::from_str(&all).unwrap();
    assert_eq!(v["tests"][0]["flaky"], 0);
    assert_eq!(v["tests"][0]["flips"], 5);
    assert!((v["tests"][0]["rate"].as_f64().unwrap() - 5.0 / 11.0).abs() < 1e-9);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_flaky_missing_file_exits_two() {
    let dir = scratch("missing");
    let out = flaky(&["--file", "nope/atalanta-history.json"], &dir);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}
