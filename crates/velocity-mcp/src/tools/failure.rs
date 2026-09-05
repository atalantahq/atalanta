//! `describe_failure`: what an agent needs to fix a failed test, from the
//! trace the run kept (`<artifacts>/traces/<test>/`): the failing step, its
//! selector and line in the suite, the error, and the elements on the last
//! screen that come closest to what the selector asked for (scored like
//! self-healing does).

use std::path::{Path, PathBuf};

use serde_json::Value;
use velocity_common::{Result, VelocityError};
use velocity_core::trace::{describe_failure as describe, load_trace, trace_dir};

/// Closest elements returned unless the caller asks for another number.
const DEFAULT_LIMIT: usize = 5;

pub fn describe_failure(config_path: &Option<String>, args: &Value) -> Result<Value> {
    let dir = artifacts_dir(config_path, args);
    let test = args.get("test").and_then(Value::as_str);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(DEFAULT_LIMIT);

    let mut files = trace_files(&dir, test);
    // Newest first: the failure the agent just saw.
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    for (file, _) in files {
        // The test passed since (VEL-205).
        if velocity_core::trace::is_stale(&file) {
            continue;
        }
        let Ok((trace, trees)) = load_trace(&file) else {
            continue;
        };
        let Some(d) = describe(&trace, &trees, limit) else {
            continue;
        };
        let mut out = serde_json::to_value(&d).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut out {
            map.insert("trace_file".into(), file.to_string_lossy().into());
            map.insert("hint".into(), hint(&d).into());
        }
        return Ok(out);
    }
    Err(VelocityError::Config(match test {
        Some(t) => format!(
            "no failed trace for '{t}' under {}; run it first (traces are kept for failed tests)",
            trace_dir(&dir, t).display()
        ),
        None => format!(
            "no failed trace under {}; run the suite first (traces are kept for failed tests)",
            dir.join("traces").display()
        ),
    }))
}

/// In `--read-only` mode `artifacts_dir` may only point under the working
/// directory, `./velocity-results` or the suite's `artifacts.outputDir`: the
/// tool reads JSON from it, and read-only must not mean "read any directory"
/// (VEL-S16). Returns the arguments with `artifacts_dir` replaced by the
/// canonical path that was checked, so the read can't follow a symlink
/// swapped in after the check (n3).
pub fn check_read_only_artifacts_dir(config_path: &Option<String>, args: &Value) -> Result<Value> {
    let Some(asked) = args.get("artifacts_dir").and_then(Value::as_str) else {
        return Ok(args.clone());
    };
    let output_dir = config_path
        .as_deref()
        .and_then(|p| velocity_core::parse_suite(p).ok())
        .map(|s| PathBuf::from(s.config.artifacts.output_dir));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = read_only_dir(
        asked,
        std::env::current_dir().ok().as_deref(),
        home.as_deref(),
        output_dir.as_deref(),
    )?;
    let mut args = args.clone();
    args["artifacts_dir"] = dir.to_string_lossy().into_owned().into();
    Ok(args)
}

/// The canonical `asked` when it lies under an allowed root. A root of `/`
/// or `$HOME` allows nothing: stdio MCP servers are often spawned with that
/// cwd (Claude Desktop on macOS uses `/`), and it made the check vacuous
/// (m10).
fn read_only_dir(
    asked: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
    output_dir: Option<&Path>,
) -> Result<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let home = home.and_then(canon);
    let broad = |r: &PathBuf| r.parent().is_none() || Some(r) == home.as_ref();
    let resolve = |p: &Path| match cwd {
        Some(c) if p.is_relative() => c.join(p),
        _ => p.to_path_buf(),
    };
    let roots: Vec<PathBuf> = [
        cwd.map(Path::to_path_buf),
        Some(resolve(Path::new("velocity-results"))),
        output_dir.map(resolve),
    ]
    .into_iter()
    .flatten()
    .filter_map(|r| canon(&r))
    .filter(|r| !broad(r))
    .collect();
    match canon(&resolve(Path::new(asked))) {
        Some(dir) if roots.iter().any(|r| dir.starts_with(r)) => Ok(dir),
        _ => Err(VelocityError::Config(format!(
            "artifacts_dir '{asked}' is outside the working directory and the suite's outputDir \
             (a working directory of / or $HOME doesn't count); a --read-only server only reads \
             artifacts there"
        ))),
    }
}

/// `artifacts_dir` argument, else the suite's `artifacts.outputDir`, else
/// the default `./velocity-results`.
fn artifacts_dir(config_path: &Option<String>, args: &Value) -> PathBuf {
    if let Some(d) = args.get("artifacts_dir").and_then(Value::as_str) {
        return PathBuf::from(d);
    }
    let from_suite = config_path
        .as_deref()
        .and_then(|p| velocity_core::parse_suite(p).ok())
        .map(|s| s.config.artifacts.output_dir);
    PathBuf::from(from_suite.unwrap_or_else(|| "./velocity-results".into()))
}

/// Every `steps*.json` under `<dir>/traces` (or just the test's folder),
/// with its modification time.
pub(crate) fn trace_files(dir: &Path, test: Option<&str>) -> Vec<(PathBuf, std::time::SystemTime)> {
    let folders: Vec<PathBuf> = match test {
        Some(t) => vec![trace_dir(dir, t)],
        None => std::fs::read_dir(dir.join("traces"))
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.is_some_and(|n| n.starts_with("steps") && n.ends_with(".json")) {
                let modified = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                out.push((p, modified));
            }
        }
    }
    out
}

/// One line saying what to change.
fn hint(d: &velocity_core::trace::FailureDescription) -> String {
    let at = d
        .source
        .as_ref()
        .map(|p| format!("{p}: "))
        .unwrap_or_default();
    match (&d.selector, d.closest.first(), &d.matched) {
        (Some(sel), _, Some(m)) => format!(
            "{at}{} found {sel} ({}) but the step still failed; check the error",
            d.step, m.describe
        ),
        (Some(sel), Some(best), None) => format!(
            "{at}{sel} is not on screen; the closest element is {} (score {:.2}). \
             If that is the intended target, change the selector to it",
            best.describe,
            best.score.unwrap_or(0.0)
        ),
        (Some(sel), None, None) if d.tree_elements == 0 => format!(
            "{at}{sel} was not found and no screen tree was recorded (the driver keeps no tree cache)"
        ),
        (Some(sel), None, None) => {
            format!("{at}{sel} was not found and nothing on screen resembles it")
        }
        (None, _, _) => format!(
            "{at}{} failed: {}",
            d.step,
            d.error.as_deref().unwrap_or("no error recorded")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use velocity_common::{
        Element, Rect, Selector, SourcePos, StepResult, StepStatus, TestStatus, TraceMode,
    };
    use velocity_core::trace::TraceRecorder;

    /// m10: a cwd of `/` or `$HOME` is not an allowance; the suite's
    /// outputDir and `./velocity-results` still are.
    #[test]
    fn read_only_ignores_a_cwd_of_root_or_home() {
        let tmp = std::env::temp_dir().join(format!("vel-ro-{}", std::process::id()));
        let home = tmp.join("home");
        let out = home.join("proj/out");
        let elsewhere = home.join("secrets");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let root = Path::new("/");
        for cwd in [root, home.as_path()] {
            let err = read_only_dir(&s(&elsewhere), Some(cwd), Some(&home), None);
            assert!(err.is_err(), "cwd {cwd:?} allowed {elsewhere:?}");
            assert!(read_only_dir("/etc", Some(cwd), Some(&home), None).is_err());
            let ok = read_only_dir(&s(&out), Some(cwd), Some(&home), Some(&out)).unwrap();
            assert_eq!(
                ok,
                std::fs::canonicalize(&out).unwrap(),
                "outputDir still allowed"
            );
        }
        // An ordinary project cwd still allows itself.
        let proj = home.join("proj");
        assert!(read_only_dir(&s(&out), Some(&proj), Some(&home), None).is_ok());
        assert!(read_only_dir(&s(&elsewhere), Some(&proj), Some(&home), None).is_err());
        // `..` and symlinks are resolved before the check.
        let up = format!("{}/../../secrets", s(&out));
        assert!(read_only_dir(&up, Some(&proj), Some(&home), None).is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    fn button(text: &str, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: String::new(),
            label: None,
            text: Some(text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 0,
                y,
                width: 300,
                height: 60,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn write_failed_trace(dir: &Path) {
        let screen = Element {
            text: None,
            element_type: "hierarchy".into(),
            children: vec![button("Checkout", 100), button("Pay now", 400)],
            ..button("", 0)
        };
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.begin_test();
        rec.begin_step(
            0,
            "tap { text: \"Pay\" }".into(),
            Some(Selector::Text("Pay".into())),
        );
        rec.record_tree(&screen);
        rec.end_step(
            &StepResult {
                step_index: 0,
                action_name: "tap".into(),
                status: StepStatus::Failed,
                duration: Duration::from_millis(5),
                screenshot: None,
                error_message: Some("Element not found: text=\"Pay\"".into()),
                resource_delta: None,
                settle_ms: None,
                source: Some(SourcePos::new("tests/app.yaml", 42, 9)),
            },
            vec![],
            vec![],
        );
        rec.finish(
            dir,
            "pay with card",
            "emulator-5554",
            None,
            0,
            TestStatus::Failed,
        )
        .unwrap();
    }

    #[test]
    fn describes_the_failing_step_with_its_line_and_the_closest_elements() {
        let dir = std::env::temp_dir().join(format!("vel-mcp-df-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_failed_trace(&dir);
        let args = serde_json::json!({ "artifacts_dir": dir, "limit": 2 });
        let v = describe_failure(&None, &args).unwrap();
        assert_eq!(v["test"], "pay with card");
        assert_eq!(v["step"], "tap { text: \"Pay\" }");
        assert_eq!(v["selector"], "text=\"Pay\"");
        assert_eq!(v["source"]["line"], 42);
        assert_eq!(v["closest"][0]["text"], "Pay now");
        assert!(v["closest"].as_array().unwrap().len() <= 2);
        let hint = v["hint"].as_str().unwrap();
        assert!(hint.starts_with("tests/app.yaml:42: "), "{hint}");
        assert!(hint.contains("text=\"Pay now\""), "{hint}");

        let by_name = describe_failure(
            &None,
            &serde_json::json!({ "artifacts_dir": dir, "test": "pay with card" }),
        )
        .unwrap();
        assert_eq!(by_name["step_index"], 0);
        let missing = describe_failure(
            &None,
            &serde_json::json!({ "artifacts_dir": dir, "test": "other" }),
        );
        assert!(missing
            .unwrap_err()
            .to_string()
            .contains("no failed trace for 'other'"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
