//! Sharding end to end with the headless driver: `--shard M/N` (D-22),
//! timings loaded once and fatal when unreadable, JUnit timings and the
//! plan fingerprint (D-23), and `velocity shard-plan` (D-49).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn velocity() -> &'static str {
    env!("CARGO_BIN_EXE_velocity")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vshard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn suite(dir: &Path, n: usize) {
    let mut yaml = "appId: com.example.app\nconfig:\n  platform: android\ntests:\n".to_string();
    for i in 0..n {
        yaml.push_str(&format!(
            "  - name: t{i}\n    steps:\n      - wait: {{ ms: 1 }}\n"
        ));
    }
    std::fs::write(dir.join("suite.yaml"), yaml).unwrap();
}

fn velocity_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(velocity())
        .args(args)
        .current_dir(dir)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env_remove("GITHUB_ACTIONS")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn plan_of(out: &str) -> String {
    let at = out
        .find("plan=")
        .unwrap_or_else(|| panic!("no plan= in {out}"));
    out[at + 5..at + 21].to_string()
}

fn ran(dir: &Path, artifacts: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(dir.join(artifacts).join("results.json")).unwrap();
    let r: serde_json::Value = serde_json::from_str(&raw).unwrap();
    r["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["test_name"].as_str().unwrap().to_string())
        .collect()
}

// D-22/D-23/D-49: shards given as 1-based M/N partition the suite, all
// print the same plan fingerprint, and `shard-plan` prints it too.
#[test]
fn shards_agree_on_one_plan_and_shard_plan_prints_it() {
    let dir = scratch("agree");
    suite(&dir, 7);
    std::fs::write(
        dir.join("timings.xml"),
        r#"<testsuite><testcase name="t0" time="60"/><testcase name="t1" time="2"/><testcase name="t2" time="2"/><testcase name="t3" time="2"/></testsuite>"#,
    )
    .unwrap();
    let mut plans = Vec::new();
    let mut all = Vec::new();
    for m in 1..=3 {
        let shard = format!("{m}/3");
        let out_dir = format!("out{m}");
        let out = velocity_in(
            &dir,
            &[
                "run",
                "suite.yaml",
                "--driver",
                "headless",
                "--report",
                "json",
                "--shard",
                &shard,
                "--shard-timings",
                "timings.xml",
                "--artifacts-dir",
                &out_dir,
            ],
        );
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert!(
            text(&out).contains(&format!("Shard {m}/3:")),
            "{}",
            text(&out)
        );
        plans.push(plan_of(&text(&out)));
        all.extend(ran(&dir, &out_dir));
    }
    assert!(plans.iter().all(|p| p == &plans[0]), "{plans:?}");
    all.sort();
    assert_eq!(all, (0..7).map(|i| format!("t{i}")).collect::<Vec<_>>());

    let out = velocity_in(
        &dir,
        &[
            "shard-plan",
            "suite.yaml",
            "--total",
            "3",
            "--shard-timings",
            "timings.xml",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(plan_of(&text(&out)), plans[0], "{}", text(&out));
    assert!(text(&out).contains("balanced by timings"), "{}", text(&out));
    // The 60 s test is alone on its shard.
    assert!(
        text(&out).contains("shard 1/3 (1 tests)\n  t0\n"),
        "{}",
        text(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Review d4 m8: `shard-plan --env A=1,B=2` set A to `1,B=2` where `run`
// splits it, so tags built from them, and the plan, differed.
#[test]
fn shard_plan_reads_env_like_run() {
    let dir = scratch("planenv");
    let mut yaml = "appId: com.example.app\nconfig:\n  platform: android\ntests:\n".to_string();
    for i in 0..5 {
        let tag = if i % 2 == 0 { "team-${B}" } else { "other" };
        yaml.push_str(&format!(
            "  - name: t{i}\n    tags: [\"{tag}\"]\n    steps:\n      - wait: {{ ms: 1 }}\n"
        ));
    }
    std::fs::write(dir.join("suite.yaml"), yaml).unwrap();
    let out = velocity_in(
        &dir,
        &[
            "run",
            "suite.yaml",
            "--driver",
            "headless",
            "--report",
            "json",
            "--shard",
            "1/2",
            "--env",
            "A=1,B=2",
            "--tags",
            "team-2",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let run_plan = plan_of(&text(&out));
    let out = velocity_in(
        &dir,
        &[
            "shard-plan",
            "suite.yaml",
            "--total",
            "2",
            "--env",
            "A=1,B=2",
            "--tags",
            "team-2",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("(3 tests in 2 shards"),
        "{}",
        text(&out)
    );
    assert_eq!(plan_of(&text(&out)), run_plan, "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

// D-23: a timings file that can't be read fails the run (exit 2) before
// anything runs; it used to fall back to count-based sharding on that
// shard only.
#[test]
fn an_unreadable_timings_file_is_fatal() {
    let dir = scratch("corrupt");
    suite(&dir, 3);
    std::fs::write(dir.join("bad.json"), "{ not json").unwrap();
    for timings in ["bad.json", "missing.json"] {
        let out = velocity_in(
            &dir,
            &[
                "run",
                "suite.yaml",
                "--driver",
                "headless",
                "--report",
                "json",
                "--shard",
                "1/2",
                "--shard-timings",
                timings,
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(!dir.join("velocity-results/results.json").exists());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// D-22: both shard forms together is a usage error.
#[test]
fn both_shard_forms_together_exit_2() {
    let dir = scratch("both");
    suite(&dir, 2);
    let out = velocity_in(
        &dir,
        &[
            "run",
            "suite.yaml",
            "--driver",
            "headless",
            "--shard",
            "1/2",
            "--shard-index",
            "0",
            "--shard-total",
            "2",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = velocity_in(
        &dir,
        &[
            "run",
            "suite.yaml",
            "--driver",
            "headless",
            "--shard",
            "3/2",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}
