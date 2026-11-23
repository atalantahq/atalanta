// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta run` end to end with the headless driver (no device): a run
//! cut short still writes its reports (VEL-044/188), a report that can't
//! be written doesn't stop the others (VEL-034), one artifacts folder
//! (VEL-033), `${VAR}` tags (VEL-175) and `--changed-since` against a
//! scratch git repository (VEL-035/036, renames).
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn atalanta() -> &'static str {
    env!("CARGO_BIN_EXE_atalanta")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vpartial-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `tests` as (name, tags, wait ms).
fn suite(dir: &Path, extra_config: &str, tests: &[(&str, &str, u64)]) {
    let mut yaml =
        format!("appId: com.example.app\nconfig:\n  platform: android\n{extra_config}tests:\n");
    for (name, tags, ms) in tests {
        yaml.push_str(&format!(
            "  - name: {name}\n    tags: [{tags}]\n    steps:\n      - wait: {{ ms: {ms} }}\n"
        ));
    }
    std::fs::write(dir.join("suite.yaml"), yaml).unwrap();
}

fn run(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::new(atalanta());
    cmd.args(["run", "suite.yaml", "--driver", "headless"])
        .args(extra)
        .current_dir(dir)
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GITHUB_ACTIONS")
        .env("NO_COLOR", "1");
    cmd
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn results(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

/// (status, error message) of test `name` in a results.json.
fn outcome(results: &serde_json::Value, name: &str) -> (String, String) {
    let t = results["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["test_name"] == name)
        .unwrap_or_else(|| panic!("{name} not in {results}"));
    (
        t["status"].as_str().unwrap_or_default().to_lowercase(),
        t["error_message"].as_str().unwrap_or_default().to_string(),
    )
}

// VEL-044: a timed-out run exits 4 *and* writes its reports: what ran,
// the test cut off, and the rest as not run.
#[test]
fn a_timed_out_run_still_writes_its_reports() {
    let dir = scratch("timeout");
    suite(
        &dir,
        "",
        &[("quick", "x", 1), ("slow", "x", 20000), ("never", "x", 1)],
    );
    let started = Instant::now();
    let out = run(&dir, &["--timeout", "2000", "--report", "json,junit"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(outcome(&r, "quick").0, "passed");
    let (status, msg) = outcome(&r, "slow");
    assert_eq!(status, "failed");
    assert!(msg.contains("suite timeout"), "{msg}");
    let (status, msg) = outcome(&r, "never");
    assert_eq!(status, "skipped");
    assert!(msg.contains("not run"), "{msg}");
    assert!(dir.join("atalanta-results/junit.xml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// VEL-188: Ctrl-C writes the reports of what ran (it left the previous
// run's green reports in place) and still exits 130.
#[test]
fn ctrl_c_writes_the_reports_of_what_ran() {
    let dir = scratch("sigint");
    suite(
        &dir,
        "",
        &[("quick", "x", 1), ("slow", "x", 20000), ("never", "x", 1)],
    );
    let mut child = run(&dir, &["--report", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    // Once "quick" has passed, "slow" is running.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("quick never passed");
        if line.contains("quick") {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(1000));
    // SAFETY: plain kill(2) on our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "did not exit after SIGINT");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(130));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(outcome(&r, "quick").0, "passed");
    let (status, msg) = outcome(&r, "slow");
    assert_eq!(status, "skipped");
    assert!(msg.contains("interrupted by Ctrl-C"), "{msg}");
    assert_eq!(outcome(&r, "never").0, "skipped");
    let _ = std::fs::remove_dir_all(&dir);
}

// VEL-034: a report that can't be written (here the GitHub step summary)
// no longer skips the reports after it; the run still fails on it.
#[test]
fn one_report_failing_does_not_skip_the_others() {
    let dir = scratch("report");
    suite(&dir, "", &[("t", "x", 1)]);
    let out = run(&dir, &["--report", "github,json"])
        // A directory: appending to it fails.
        .env("GITHUB_STEP_SUMMARY", &dir)
        .output()
        .unwrap();
    // D-20: Atalanta failing to write its output is an internal error (5),
    // not a config error (2).
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert!(text(&out).contains("github report"), "{}", text(&out));
    assert!(
        dir.join("atalanta-results/results.json").exists(),
        "{}",
        text(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// VEL-033: the suite's `artifacts.outputDir` holds the reports too, not
// only the history.
#[test]
fn the_suites_output_dir_holds_the_reports() {
    let dir = scratch("outdir");
    suite(&dir, "  artifacts:\n    outputDir: out\n", &[("t", "x", 1)]);
    let out = run(&dir, &["--report", "json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(dir.join("out/results.json").exists(), "{}", text(&out));
    assert!(dir.join("out/atalanta-history.json").exists());
    assert!(!dir.join("atalanta-results").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// VEL-175: `tags: [${TEAM}]` is interpolated, so `--tags` can match it.
#[test]
fn tags_written_with_variables_can_be_selected() {
    let dir = scratch("tags");
    suite(
        &dir,
        "",
        &[("mine", "\"${TEAM}\"", 1), ("theirs", "other", 1)],
    );
    let out = run(
        &dir,
        &[
            "--report",
            "json",
            "--env",
            "TEAM=payments",
            "--tags",
            "payments",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(r["tests"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(outcome(&r, "mine").0, "passed");
    let _ = std::fs::remove_dir_all(&dir);
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out));
}

// VEL-035/036 and renames: paths with non-ASCII names match the impact
// map (git C-quoted them), a renamed file's old path still selects its
// tests, a base that looks like an option is refused, and a mapping that
// matches nothing is reported.
#[test]
fn changed_since_reads_git_safely() {
    let dir = scratch("git");
    suite(
        &dir,
        "",
        &[("unicode", "x", 1), ("moved", "x", 1), ("other", "x", 1)],
    );
    std::fs::write(
        dir.join("atalanta-impact.yaml"),
        "tests:\n  unicode: [\"src/héllo.ts\"]\n  moved: [\"src/old.ts\"]\n  other: [\"src/typo/**\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/héllo.ts"), "a").unwrap();
    std::fs::write(dir.join("src/old.ts"), "b").unwrap();
    std::fs::write(dir.join(".gitignore"), "atalanta-results/\nleases/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("src/héllo.ts"), "changed").unwrap();
    git(&dir, &["mv", "src/old.ts", "src/new.ts"]);

    let out = run(&dir, &["--report", "json", "--changed-since", "HEAD"])
        .output()
        .unwrap();
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("unicode — covers src/héllo.ts"), "{all}");
    assert!(all.contains("moved — covers src/old.ts"), "{all}");
    assert!(all.contains("`src/typo/**` matches no file"), "{all}");

    let out = run(&dir, &["--changed-since", "--output=/tmp/x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = run(&dir, &["--changed-since", "no-such-ref"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("not a commit"), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Start the quick/slow/never suite, wait until "slow" is running, send
/// `signals` (with `gap` between them) and return the exit code.
fn interrupt(name: &str, signals: &[i32], gap: Duration) -> (PathBuf, Option<i32>) {
    let dir = scratch(name);
    suite(
        &dir,
        "",
        &[("quick", "x", 1), ("slow", "x", 20000), ("never", "x", 1)],
    );
    let mut child = run(&dir, &["--report", "json,junit,github"])
        .env("GITHUB_STEP_SUMMARY", dir.join("summary.md"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("quick never passed");
        if line.contains("quick") {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(1000));
    for (i, sig) in signals.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(gap);
        }
        // SAFETY: plain kill(2) on our own child.
        unsafe {
            libc::kill(child.id() as i32, *sig);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "did not exit after {signals:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    (dir, status.code())
}

// D-20: SIGTERM (how CI cancels a job) exits 143, not 130, and the
// reports say how the run ended.
#[test]
fn sigterm_exits_143_and_the_reports_say_so() {
    let (dir, code) = interrupt("sigterm", &[libc::SIGTERM], Duration::ZERO);
    assert_eq!(code, Some(143));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(r["termination"], "interrupted:SIGTERM", "{r}");
    let (status, msg) = outcome(&r, "slow");
    assert_eq!(status, "skipped");
    assert!(msg.contains("interrupted by SIGTERM"), "{msg}");
    let junit = std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap();
    assert!(
        junit.contains(r#"<property name="atalanta.termination" value="interrupted:SIGTERM"/>"#),
        "{junit}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// C2: the GitHub step summary of a terminated run says so at the top;
// its table alone read like a complete run with a few skips.
#[test]
fn a_terminated_runs_step_summary_has_a_banner() {
    let (dir, code) = interrupt("banner", &[libc::SIGTERM], Duration::ZERO);
    assert_eq!(code, Some(143));
    let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
    assert!(summary.starts_with("> [!WARNING]\n"), "{summary}");
    assert!(summary.contains("terminated (SIGTERM"), "{summary}");
    assert!(
        summary.contains("2 of 3 test(s) did not run or finish"),
        "{summary}"
    );
    let junit = std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap();
    assert!(!junit.contains("<skipped/>"), "{junit}");
    let _ = std::fs::remove_dir_all(&dir);
}

// D-20: SIGHUP (a closed terminal) stops the run cleanly too: 129.
#[test]
fn sighup_exits_129_after_writing_reports() {
    let (dir, code) = interrupt("sighup", &[libc::SIGHUP], Duration::ZERO);
    assert_eq!(code, Some(129));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(r["termination"], "interrupted:SIGHUP", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

// D-20: npm/yarn forward one Ctrl-C twice. A second Ctrl-C within a
// second is the same one, so the run still stops cleanly and writes its
// reports (it exited at once, with no report).
#[test]
fn a_doubled_ctrl_c_still_writes_the_reports() {
    let (dir, code) = interrupt(
        "dblint",
        &[libc::SIGINT, libc::SIGINT],
        Duration::from_millis(100),
    );
    assert_eq!(code, Some(130));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(r["termination"], "interrupted:SIGINT", "{r}");
    assert_eq!(outcome(&r, "quick").0, "passed");
    let _ = std::fs::remove_dir_all(&dir);
}

// A completed run says so too.
#[test]
fn a_completed_run_reports_its_termination() {
    let dir = scratch("completed");
    suite(&dir, "", &[("t", "x", 1)]);
    let out = run(&dir, &["--report", "json,junit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let r = results(&dir.join("atalanta-results/results.json"));
    assert_eq!(r["termination"], "completed", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A history in which every test in `names` passed just now: none is new,
/// stale or recently failed, so only the git/map rules select.
fn clean_history(dir: &Path, names: &[&str]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut outcomes = serde_json::Map::new();
    let mut last = serde_json::Map::new();
    for n in names {
        outcomes.insert(n.to_string(), serde_json::json!(["pass"]));
        last.insert(n.to_string(), serde_json::json!(now));
    }
    let h = serde_json::json!({ "version": 2, "durations": {}, "outcomes": outcomes, "last_run": last });
    std::fs::create_dir_all(dir.join("atalanta-results")).unwrap();
    std::fs::write(
        dir.join("atalanta-results/atalanta-history.json"),
        h.to_string(),
    )
    .unwrap();
}

// D-24: the changed set is `base...HEAD` (from the merge base), plus the
// working tree, plus untracked files. A commit only on `main` after the
// branch forked is not this branch's change; it used to select its tests.
// An unmapped test runs by default.
#[test]
fn changed_since_diffs_from_the_merge_base_and_runs_unmapped_tests() {
    let dir = scratch("mergebase");
    suite(
        &dir,
        "",
        &[
            ("feat", "x", 1),
            ("up", "x", 1),
            ("wip", "x", 1),
            ("loose", "x", 1),
        ],
    );
    std::fs::write(
        dir.join("atalanta-impact.yaml"),
        "tests:\n  feat: [src/feature.ts]\n  up: [src/upstream.ts]\n  wip: [src/wip.ts]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for f in ["feature.ts", "upstream.ts", "wip.ts"] {
        std::fs::write(dir.join("src").join(f), "a").unwrap();
    }
    std::fs::write(dir.join(".gitignore"), "atalanta-results/\nleases/\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("src/feature.ts"), "b").unwrap();
    git(&dir, &["commit", "-q", "-am", "feature"]);
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("src/upstream.ts"), "c").unwrap();
    git(&dir, &["commit", "-q", "-am", "upstream"]);
    git(&dir, &["checkout", "-q", "feature"]);
    // An uncommitted edit counts.
    std::fs::write(dir.join("src/wip.ts"), "d").unwrap();
    clean_history(&dir, &["feat", "up", "wip", "loose"]);

    let out = run(&dir, &["--report", "json", "--changed-since", "main"])
        .output()
        .unwrap();
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("feat — covers src/feature.ts"), "{all}");
    assert!(all.contains("wip — covers src/wip.ts"), "{all}");
    assert!(all.contains("loose — unmapped"), "{all}");
    assert!(all.contains("1 unmapped test(s) selected"), "{all}");
    assert!(
        !all.contains("up — "),
        "upstream-only change selected: {all}"
    );
    let r = results(&dir.join("atalanta-results/results.json"));
    let ran: Vec<&str> = r["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["test_name"].as_str().unwrap())
        .collect();
    assert!(!ran.contains(&"up"), "{ran:?}");

    // `impact: { unmapped: skip }` restores the old behaviour.
    std::fs::write(
        dir.join("atalanta-impact.yaml"),
        "impact:\n  unmapped: skip\ntests:\n  feat: [src/feature.ts]\n  up: [src/upstream.ts]\n  wip: [src/wip.ts]\n",
    )
    .unwrap();
    let out = run(&dir, &["--changed-since", "main"]).output().unwrap();
    assert!(!text(&out).contains("loose — "), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

// N-6 / D-24: a user's `diff.relative=true`, with the run started in a
// subdirectory, printed paths relative to it and root mappings missed.
#[test]
fn changed_since_ignores_diff_relative() {
    let dir = scratch("relative");
    let e2e = dir.join("e2e");
    std::fs::create_dir_all(&e2e).unwrap();
    suite(&e2e, "", &[("login", "x", 1), ("other", "x", 1)]);
    std::fs::write(
        e2e.join("atalanta-impact.yaml"),
        "tests:\n  login: [app/src/]\n  other: [app/other/]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("app/src")).unwrap();
    std::fs::write(dir.join("app/src/Login.kt"), "a").unwrap();
    std::fs::write(dir.join(".gitignore"), "atalanta-results/\nleases/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "diff.relative", "true"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("app/src/Login.kt"), "b").unwrap();
    git(&dir, &["commit", "-q", "-am", "change"]);
    clean_history(&e2e, &["login", "other"]);
    let out = run(&e2e, &["--changed-since", "HEAD~1"]).output().unwrap();
    let all = text(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");
    assert!(all.contains("login — covers app/src/Login.kt"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

// D-24: in a shallow clone there's no merge base; the error says how to
// fetch the history CI needs.
#[test]
fn a_shallow_clone_names_fetch_depth_0() {
    let dir = scratch("shallow");
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    suite(&origin, "", &[("t", "x", 1)]);
    std::fs::write(origin.join("atalanta-impact.yaml"), "tests:\n  t: [src/]\n").unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    git(&origin, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(origin.join("x.txt"), "1").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "f1"]);
    git(&origin, &["checkout", "-q", "main"]);
    std::fs::write(origin.join("y.txt"), "1").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "m1"]);
    let url = format!("file://{}", origin.display());
    git(
        &dir,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-single-branch",
            "-b",
            "feature",
            &url,
            "clone",
        ],
    );
    let clone = dir.join("clone");
    let out = run(&clone, &["--changed-since", "origin/main"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("fetch-depth: 0"), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

// D-18: an empty impact selection passes, unless --fail-if-none.
#[test]
fn fail_if_none_fails_an_empty_impact_selection() {
    let dir = scratch("ifnone");
    suite(&dir, "", &[("login", "x", 1)]);
    std::fs::write(
        dir.join("atalanta-impact.yaml"),
        "tests:\n  login: [src/]\n",
    )
    .unwrap();
    std::fs::write(dir.join("README.md"), "a").unwrap();
    std::fs::write(dir.join(".gitignore"), "atalanta-results/\nleases/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("README.md"), "b").unwrap();
    clean_history(&dir, &["login"]);
    let out = run(&dir, &["--changed-since", "HEAD"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("nothing to run"), "{}", text(&out));
    let out = run(&dir, &["--changed-since", "HEAD", "--fail-if-none"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("no test is affected"), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

// Review d4 m6: `--fail-if-none` decides on the git/map selection, the
// same on every shard. A test this shard's history picks (it failed last
// time) used to make this shard pass while a shard with a clean history
// exited 2.
#[test]
fn fail_if_none_ignores_history_picks() {
    let dir = scratch("ifnonehist");
    suite(&dir, "", &[("login", "x", 1)]);
    std::fs::write(
        dir.join("atalanta-impact.yaml"),
        "tests:\n  login: [src/]\n",
    )
    .unwrap();
    std::fs::write(dir.join("README.md"), "a").unwrap();
    std::fs::write(dir.join(".gitignore"), "atalanta-results/\nleases/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("README.md"), "b").unwrap();
    clean_history(&dir, &["login"]);
    let path = dir.join("atalanta-results/atalanta-history.json");
    let mut h: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    h["outcomes"]["login"] = serde_json::json!(["fail"]);
    std::fs::write(&path, h.to_string()).unwrap();
    let out = run(&dir, &["--changed-since", "HEAD", "--fail-if-none"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("no test is affected"), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

// A-10: `--junit-classname suite` puts the suite stem in every classname;
// the default is unchanged.
#[test]
fn junit_classname_suite_uses_the_suite_stem() {
    let dir = scratch("classname");
    suite(&dir, "", &[("t", "x", 1)]);
    let out = run(&dir, &["--report", "junit", "--junit-classname", "suite"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let junit = std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap();
    assert!(junit.contains(r#"classname="suite""#), "{junit}");
    let out = run(&dir, &["--report", "junit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let junit = std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap();
    assert!(junit.contains(r#"classname="atalanta""#), "{junit}");
    let _ = std::fs::remove_dir_all(&dir);
}

// N-2: early flake detection doesn't exist in the parallel runner, and the
// flag was silently ignored there. Refused (exit 2) until it does.
#[test]
fn detect_new_flakes_with_parallel_is_refused() {
    let dir = scratch("efdpar");
    suite(&dir, "", &[("t", "x", 1)]);
    let out = run(&dir, &["--parallel", "2", "--detect-new-flakes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("--detect-new-flakes") && text(&out).contains("--parallel"),
        "{}",
        text(&out)
    );
    let out = run(&dir, &["--parallel", "1", "--detect-new-flakes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}
