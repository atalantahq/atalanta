//! C1: history shared across CI jobs without a server: `atalanta history
//! merge` and `atalanta run --history-from`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn atalanta() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_atalanta"));
    c.env_remove("GITHUB_ACTIONS").env("NO_COLOR", "1");
    c
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vshared-{name}-{}", std::process::id()));
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

/// A job's uploaded history: `test` → (outcomes, last run).
fn job_history(dir: &Path, tests: &[(&str, &[&str], u64)]) {
    let mut outcomes = serde_json::Map::new();
    let mut last = serde_json::Map::new();
    let mut durations = serde_json::Map::new();
    for (name, o, at) in tests {
        outcomes.insert(name.to_string(), serde_json::json!(o));
        last.insert(name.to_string(), serde_json::json!(at));
        durations.insert(name.to_string(), serde_json::json!(100));
    }
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("atalanta-history.json"),
        serde_json::json!({ "version": 2, "durations": durations, "outcomes": outcomes, "last_run": last })
            .to_string(),
    )
    .unwrap();
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn history_merge_combines_the_jobs_uploads() {
    let dir = scratch("merge");
    let t = now();
    // Both shards started from the same base and each ran its own test.
    job_history(
        &dir.join("dl/history-1"),
        &[("a", &["pass", "fail"], t), ("b", &["pass"], t - 100)],
    );
    job_history(
        &dir.join("dl/history-2"),
        &[("a", &["pass"], t - 100), ("b", &["pass", "pass"], t)],
    );
    let out = atalanta()
        .args([
            "history",
            "merge",
            "dl/*",
            "-o",
            "merged/atalanta-history.json",
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("Merged 2 history file(s)"),
        "{}",
        text(&out)
    );
    let merged: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("merged/atalanta-history.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(merged["outcomes"]["a"], serde_json::json!(["pass", "fail"]));
    assert_eq!(merged["outcomes"]["b"], serde_json::json!(["pass", "pass"]));

    // Nothing to merge: a usage error.
    let out = atalanta()
        .args(["history", "merge", "nothing/*"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_from_lets_last_failed_see_another_jobs_failure() {
    let dir = scratch("from");
    std::fs::write(
        dir.join("suite.yaml"),
        "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: a\n    steps:\n      - wait: { ms: 1 }\n  - name: b\n    steps:\n      - wait: { ms: 1 }\n",
    )
    .unwrap();
    let t = now();
    // Another job ran `b` and it failed; this job's own history knows
    // only passes.
    job_history(
        &dir.join("atalanta-results"),
        &[("a", &["pass"], t), ("b", &["pass"], t - 50)],
    );
    job_history(&dir.join("dl/other-job"), &[("b", &["pass", "fail"], t)]);
    let out = atalanta()
        .args(["run", "suite.yaml", "--driver", "headless", "--last-failed"])
        .args(["--history-from", "dl/*", "--history-from", "missing/*"])
        .args(["--report", "json"])
        .current_dir(&dir)
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .output()
        .unwrap();
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("Merged 1 history file"), "{all}");
    assert!(all.contains("--history-from missing/*"), "{all}");
    assert!(all.contains("Re-running 1 test"), "{all}");
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("atalanta-results/results.json")).unwrap(),
    )
    .unwrap();
    let ran: Vec<&str> = results["tests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["status"] != "skipped")
        .map(|t| t["test_name"].as_str().unwrap())
        .collect();
    assert_eq!(ran, vec!["b"], "{results}");

    // A history file that doesn't parse: exit 2.
    std::fs::create_dir_all(dir.join("bad")).unwrap();
    std::fs::write(dir.join("bad/atalanta-history.json"), "{not json").unwrap();
    let out = atalanta()
        .args([
            "run",
            "suite.yaml",
            "--driver",
            "headless",
            "--history-from",
            "bad",
        ])
        .current_dir(&dir)
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}
