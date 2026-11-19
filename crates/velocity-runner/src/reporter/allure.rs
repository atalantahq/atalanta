//! Allure results: one `<uuid>-result.json` per test in `allure-results/`,
//! with steps, failure details, flaky marking and screenshots attached
//! (copied next to the results, as Allure expects). Feed the directory to
//! `allure serve` or `allure generate`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use velocity_common::{Result, StepStatus, SuiteResult, TestResult, TestStatus, VelocityError};

/// Write results for `result` into `dir` (created if needed).
pub fn write_allure(result: &SuiteResult, dir: &str, suite_name: &str) -> Result<usize> {
    let dir = Path::new(dir);
    std::fs::create_dir_all(dir)
        .map_err(|e| VelocityError::Config(format!("create {}: {e}", dir.display())))?;
    let end = now_ms();
    // Tests ran one after another (or overlapping in parallel runs); lay
    // them out back to back ending now, which keeps durations right.
    // Saturating: a huge duration must not overflow (VEL-182).
    let total: u64 = result
        .tests
        .iter()
        .map(|t| t.duration.as_millis() as u64)
        .fold(0, u64::saturating_add);
    let mut start = end.saturating_sub(total);
    for (i, t) in result.tests.iter().enumerate() {
        let stop = start.saturating_add(t.duration.as_millis() as u64);
        let uuid = uuid_for(&t.test_name, i, end);
        let doc = test_doc(t, &uuid, suite_name, start, stop, dir)?;
        let path = dir.join(format!("{uuid}-result.json"));
        // Whole files only: `allure generate` running beside a run never
        // reads half a result (VEL-053).
        velocity_common::fsutil::write_atomic(
            &path,
            &serde_json::to_vec_pretty(&doc).unwrap_or_default(),
        )
        .map_err(|e| VelocityError::Config(format!("write {}: {e}", path.display())))?;
        start = stop;
    }
    Ok(result.tests.len())
}

/// Remove every Allure result, container and attachment from `dir`
/// (`velocity run --clean-allure`); other files (`categories.json`,
/// `executor.json`, `history/`) are kept. Returns how many were removed.
///
/// Results are never deleted otherwise (M-4). Deleting "this suite's"
/// earlier results lost data: a `--last-failed` or `--tags` rerun wiped the
/// full run's results, two shards writing one dir wiped each other's, and
/// `ios/login.yaml` and `android/login.yaml` share a label. Results of one
/// run can't be told apart from another's after the fact, so the safe
/// choice is to let the user say when the directory starts afresh.
pub fn clean_dir(dir: &str) -> usize {
    let mut removed = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let allure_file = name.ends_with("-result.json")
            || name.ends_with("-container.json")
            || name.contains("-attachment")
            || name.contains("-shot");
        if allure_file
            && entry.file_type().is_ok_and(|t| t.is_file())
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn test_doc(
    t: &TestResult,
    uuid: &str,
    suite: &str,
    start: u64,
    stop: u64,
    dir: &Path,
) -> Result<Value> {
    let status = match t.status {
        TestStatus::Passed | TestStatus::Retried => "passed",
        TestStatus::Failed if t.infrastructure_failure => "broken",
        TestStatus::Failed => "failed",
        TestStatus::Skipped => "skipped",
    };
    let mut details = serde_json::Map::new();
    if let Some(msg) = &t.error_message {
        details.insert("message".into(), json!(msg));
    }
    if t.status == TestStatus::Retried {
        details.insert("flaky".into(), json!(true));
        details.insert(
            "trace".into(),
            json!(format!(
                "Earlier attempts:\n{}",
                t.failed_attempts.join("\n---\n")
            )),
        );
    }
    if t.quarantined.is_some() {
        details.insert("muted".into(), json!(true));
    }
    if !t.warnings.is_empty() && !details.contains_key("trace") {
        details.insert("trace".into(), json!(t.warnings.join("\n")));
    }
    // D-7 / R4: a heal is never a silent pass.
    if let Some(label) = t.healed_label() {
        details.insert("message".into(), json!(label));
    }

    let mut step_start = start;
    let steps: Vec<Value> = t
        .steps
        .iter()
        .map(|s| {
            let stop = step_start + s.duration.as_millis() as u64;
            let v = json!({
                "name": s.action_name,
                "status": match s.status {
                    StepStatus::Passed => "passed",
                    StepStatus::Failed => "failed",
                    StepStatus::Skipped => "skipped",
                },
                "stage": "finished",
                "start": step_start,
                "stop": stop,
                "statusDetails": s.error_message.as_ref().map(|m| json!({ "message": m })).unwrap_or(json!({})),
                "parameters": s.settle_ms.map(|ms| vec![json!({ "name": "settle ms", "value": ms.to_string() })]).unwrap_or_default(),
            });
            step_start = stop;
            v
        })
        .collect();

    let mut attachments = Vec::new();
    for (n, shot) in t.screenshots.iter().enumerate() {
        let source = format!("{uuid}-shot{n}.png");
        if link_or_copy(shot, &dir.join(&source)) {
            attachments.push(
                json!({ "name": "failure screenshot", "source": source, "type": "image/png" }),
            );
        }
    }

    // Video, logs and the trace viewer, copied like the screenshots.
    for (n, a) in t.attachments.iter().enumerate() {
        let ext = a
            .path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| "txt".into());
        let source = format!("{uuid}-evidence{n}.{ext}");
        if link_or_copy(&a.path, &dir.join(&source)) {
            attachments.push(json!({ "name": a.title(), "source": source, "type": a.mime() }));
        }
    }

    Ok(json!({
        "uuid": uuid,
        "historyId": format!("{:016x}", fnv(&format!("{suite}\u{1f}{}", t.test_name))),
        "name": t.test_name,
        "fullName": format!("{suite}: {}", t.test_name),
        "status": status,
        "statusDetails": Value::Object(details),
        "stage": "finished",
        "start": start,
        "stop": stop,
        "steps": steps,
        "attachments": attachments,
        "labels": labels(suite, t),
    }))
}

/// Suite, framework, one `tag` per test tag (Allure filters and groups by
/// these; tags never reached the report), and the device the test ran on.
fn labels(suite: &str, t: &TestResult) -> Value {
    let mut out = vec![
        json!({ "name": "suite", "value": suite }),
        json!({ "name": "framework", "value": "velocity" }),
    ];
    out.extend(
        t.tags
            .iter()
            .map(|tag| json!({ "name": "tag", "value": tag })),
    );
    if let Some(device) = &t.device {
        out.push(json!({ "name": "host", "value": device }));
    }
    if t.healed_count() > 0 {
        out.push(json!({ "name": "tag", "value": format!("healed {}", t.healed_count()) }));
    }
    Value::Array(out)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A UUID-shaped id, unique per test and run.
fn uuid_for(name: &str, index: usize, at: u64) -> String {
    let a = fnv(&format!("{name}\u{1f}{index}\u{1f}{at}"));
    let b = fnv(&format!(
        "{at}\u{1f}{index}\u{1f}{name}\u{1f}{}",
        std::process::id()
    ));
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Put `src` at `dst` as a hard link when both are on one filesystem (a
/// video is not duplicated per report), else a copy. Allure never writes
/// to its attachments, so sharing the inode is safe.
fn link_or_copy(src: &std::path::Path, dst: &std::path::Path) -> bool {
    let _ = std::fs::remove_file(dst);
    std::fs::hard_link(src, dst).is_ok() || std::fs::copy(src, dst).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn healed() -> TestResult {
        let mut t = TestResult::failed("login", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.warnings = vec!["selector text=\"Log in\" not found; healed to text=\"Sign in\" (confidence 0.92) — update the selector".into()];
        t
    }

    #[test]
    fn healed_passes_say_so() {
        let doc = test_doc(&healed(), "u", "app", 0, 1, Path::new(".")).unwrap();
        assert_eq!(doc["status"], "passed");
        assert_eq!(doc["statusDetails"]["message"], "PASSED (healed 1)");
        assert!(doc["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["value"] == "healed 1"));
    }

    #[test]
    fn writes_one_result_per_test_with_allure_fields() {
        let dir = std::env::temp_dir().join(format!("vel-allure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut flaky = TestResult::failed("search", String::new(), Duration::from_millis(900));
        flaky.status = TestStatus::Retried;
        flaky.error_message = None;
        flaky.failed_attempts = vec!["timeout".into()];
        let mut infra = TestResult::failed("login", "adb gone".into(), Duration::from_millis(100));
        infra.infrastructure_failure = true;
        let result = SuiteResult {
            total: 2,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 1,
            duration: Duration::from_secs(1),
            tests: vec![flaky, infra],
            shard_index: None,
            shard_total: None,
        };
        assert_eq!(
            write_allure(&result, dir.to_str().unwrap(), "app").unwrap(),
            2
        );
        let docs: Vec<Value> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| serde_json::from_slice(&std::fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect();
        let by = |n: &str| docs.iter().find(|d| d["name"] == n).unwrap().clone();
        assert_eq!(by("search")["status"], "passed");
        assert_eq!(by("search")["statusDetails"]["flaky"], true);
        assert_eq!(
            by("login")["status"],
            "broken",
            "infrastructure, not the app"
        );
        assert_eq!(by("login")["uuid"].as_str().unwrap().len(), 36);
        assert!(by("login")["stop"].as_u64() >= by("login")["start"].as_u64());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod clean_tests {
    use super::*;
    use std::time::Duration;

    fn one(name: &str) -> SuiteResult {
        SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests: vec![TestResult::failed(name, "x".into(), Duration::MAX)],
            shard_index: None,
            shard_total: None,
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                let doc: Value = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
                doc["name"].as_str().map(str::to_string)
            })
            .collect()
    }

    // M-4: a rerun subset (`--last-failed`), a second shard or a same-stem
    // suite deleted the earlier run's results for that suite label.
    #[test]
    fn a_rerun_or_another_shard_never_deletes_earlier_results() {
        let dir = std::env::temp_dir().join(format!("vel-allure-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_str().unwrap();
        write_allure(&one("shard 0 test"), d, "app").unwrap();
        write_allure(&one("shard 1 test"), d, "app").unwrap();
        let names = names_in(&dir);
        assert!(names.contains(&"shard 0 test".to_string()), "{names:?}");
        assert!(names.contains(&"shard 1 test".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-326: results of earlier runs piled up; `--clean-allure` empties
    // the directory's Allure files (only those).
    #[test]
    fn clean_dir_removes_only_allure_files() {
        let dir = std::env::temp_dir().join(format!("vel-allure-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_str().unwrap();
        write_allure(&one("old name"), d, "app").unwrap();
        std::fs::write(dir.join("abc-shot0.png"), "x").unwrap();
        std::fs::write(dir.join("categories.json"), "[]").unwrap();
        assert_eq!(clean_dir(d), 2);
        write_allure(&one("new name"), d, "app").unwrap();
        assert_eq!(names_in(&dir), vec!["new name".to_string()]);
        assert!(dir.join("categories.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn test_tags_and_device_become_labels() {
        let mut t = TestResult::failed("Login", "x".into(), std::time::Duration::ZERO);
        t.tags = vec!["smoke".into(), "login".into()];
        t.device = Some("emulator-5554".into());
        let l = labels("lab", &t);
        let pairs: Vec<(String, String)> = l
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["name"].as_str().unwrap().into(),
                    v["value"].as_str().unwrap().into(),
                )
            })
            .collect();
        for want in [
            ("tag", "smoke"),
            ("tag", "login"),
            ("host", "emulator-5554"),
            ("suite", "lab"),
        ] {
            assert!(pairs.contains(&(want.0.into(), want.1.into())), "{pairs:?}");
        }
    }
}
