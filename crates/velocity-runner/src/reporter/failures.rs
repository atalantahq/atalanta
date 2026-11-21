//! `failures.md` (row 2.2: REP-080, VOI-007), written by every run: each
//! failed test with the step that failed, the error, the errors the app
//! logged during that step, and the files that show it. The explanation only says what the evidence
//! shows, and cites it (a file and its lines, a screenshot); with nothing to
//! cite it says "insufficient evidence" instead of guessing. Showing sources
//! reduces reliance on wrong answers (CHI 2025).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use velocity_common::evidence::{AttachmentKind, LogLine};
use velocity_common::{Element, StepStatus, SuiteResult, TestResult, TestStatus};
use velocity_core::trace::{describe_failure, load_trace, TestTrace};

/// Characters of each log line shown in the error block, at most.
const LOG_LINE_CHARS: usize = 300;

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// One failed test's section, and whether its explanation cites evidence.
fn section(t: &TestResult, base: &Path) -> (String, bool) {
    let mut s = String::new();
    let _ = writeln!(s, "## {}\n", t.test_name);
    let step = t.steps.iter().find(|st| st.status == StepStatus::Failed);
    if let Some(st) = step {
        let at = st
            .source
            .as_ref()
            .map(|p| format!(" ({}:{})", p.file, p.line))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "- **Failed step:** {} `{}`{at}",
            st.step_index + 1,
            st.action_name
        );
    }
    let error = t
        .error_message
        .as_deref()
        .or(step.and_then(|st| st.error_message.as_deref()))
        .unwrap_or("(no error message)");
    let error = velocity_common::redact::redact(error);
    let _ = writeln!(s, "- **Error:** {}", error.trim());

    let mut evidence: Vec<String> = Vec::new();
    let shot = step
        .and_then(|st| st.screenshot.clone())
        .or_else(|| t.screenshots.last().cloned());
    if let Some(p) = &shot {
        evidence.push(format!("screenshot `{}`", rel(p, base)));
    }
    if let Some(p) = &t.trace {
        evidence.push(format!("trace `{}`", rel(p, base)));
    }
    let mut crash = None;
    for a in &t.attachments {
        let what = match a.kind {
            AttachmentKind::Video => "video",
            AttachmentKind::Screencast => "screencast",
            AttachmentKind::AppLog => "app log",
            AttachmentKind::DeviceLog => "device log",
            AttachmentKind::Crash => {
                crash = Some(a.path.clone());
                "crash report"
            }
            _ => "file",
        };
        evidence.push(format!("{what} `{}`", rel(&a.path, base)));
    }
    if !evidence.is_empty() {
        let _ = writeln!(s, "- **Evidence:** {}", evidence.join(", "));
    }
    // Load the trace and describe its failed step, if there is one.
    let description = t.trace.as_ref().and_then(|trace_path| {
        load_trace(trace_path).ok().and_then(|(trace, trees)| {
            describe_failure(&trace, &trees, 3).map(|d| (trace, trees, d))
        })
    });
    // The failed step's time window (test-time ms), from its trace.
    let window = description.as_ref().and_then(|(trace, _, _)| {
        trace
            .steps
            .iter()
            .rev()
            .find(|s| s.status == Some(StepStatus::Failed))
            .map(|s| (s.start_ms, s.duration_ms))
    });
    // Timed log lines from the trace's evidence, filtered to the failed step's
    // window (levels E/F only).
    let log_block = description.as_ref().and_then(|(trace, _trees, _)| {
        let log_name = trace.evidence.as_ref().and_then(|e| e.log_lines.as_ref())?;
        let folder = t
            .trace
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default();
        let path = folder.join(log_name);
        let lines: Vec<LogLine> = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        let (start, duration) = window?;
        let out: Vec<String> = lines
            .iter()
            .filter(|l| matches!(l.level, 'E' | 'F'))
            .filter(|l| l.t >= start - 500 && l.t <= start + duration as i64)
            .map(|l| {
                let text: String = l.text.chars().take(LOG_LINE_CHARS).collect();
                format!("{:>7} ms  {}", l.t, velocity_common::redact::redact(&text))
            })
            .collect();
        (!out.is_empty()).then_some((log_name.clone(), out))
    });

    // The explanation: only what a cited file shows.
    let lower = error.to_ascii_lowercase();
    let (why, cited) = explanation(&ExplanationCtx {
        lower: &lower,
        error: &error,
        crash: &crash,
        shot: &shot,
        base,
        infrastructure: t.infrastructure_failure,
        description: &description,
        log_block: &log_block,
    });
    let _ = writeln!(s, "- **Why:** {why}");
    if let Some(next) = try_next(&lower, t.infrastructure_failure) {
        let _ = writeln!(s, "- **Try:** {next}");
    }
    // Only when the Why cites them: device-wide error lines under an
    // unrelated Why read as the cause (a seeded run printed another app's
    // 74-line crash dump under a missing-element explanation).
    let logs_cited = why.starts_with("the app logged errors");
    if let Some((_, lines)) = log_block.as_ref().filter(|_| logs_cited) {
        let _ = writeln!(s, "\n```text");
        for l in lines {
            let _ = writeln!(s, "{l}");
        }
        let _ = writeln!(s, "```");
    }
    s.push('\n');
    (s, cited)
}

/// Text of `marker` in `text`, trimmed, with a leading `:` removed.
fn after(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)?;
    let rest = &text[start + marker.len()..];
    Some(rest.trim_start_matches(':').trim().to_string())
}

/// The `X` and `Y` of the executor's `; X was in front, not Y` note.
fn in_front(text: &str) -> Option<(String, String)> {
    let (_, rest) = text.rsplit_once("; ")?;
    let (front, ours) = rest.split_once(" was in front, not ")?;
    (!front.is_empty() && !ours.is_empty()).then(|| (front.to_string(), ours.trim().to_string()))
}

/// The `X` and `Y` from `expected "X", got "Y"` in `text`.
fn assertion_values(text: &str) -> Option<(String, String)> {
    let lower = text.to_ascii_lowercase();
    let i = lower.find("expected \"")?;
    let after = &text[i + "expected \"".len()..];
    let j = after.find('"')?;
    let x = &after[..j];
    let k = after.find("\", got \"")?;
    let rest = &after[k + "\", got \"".len()..];
    let l = rest.find('"')?;
    let y = &rest[..l];
    Some((x.to_string(), y.to_string()))
}

/// Up to 6 distinct, non-empty `text` (else `label`) values of the failed
/// step's last tree (else the closest earlier one, as `describe_failure`
/// picks it), in depth-first order, each cut to 40 characters, quoted and
/// comma separated. `None` when there are none.
fn screen_texts(trees: &HashMap<String, Element>, trace: &TestTrace) -> Option<String> {
    let pos = trace
        .steps
        .iter()
        .rposition(|s| s.status == Some(StepStatus::Failed))?;
    let tree = trace.steps[..=pos]
        .iter()
        .rev()
        .find_map(|s| s.trees.last())
        .and_then(|h| trees.get(h))?;
    let mut texts: Vec<String> = Vec::new();
    collect_texts(tree, &mut texts);
    let mut seen = std::collections::HashSet::new();
    let texts: Vec<String> = texts
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .take(6)
        .map(|t| format!("`{}`", t))
        .collect();
    (!texts.is_empty()).then_some(texts.join(", "))
}

fn collect_texts(el: &Element, out: &mut Vec<String>) {
    let value = el.text.as_deref().or(el.label.as_deref()).unwrap_or("");
    if !value.is_empty() {
        let cut: String = value.chars().take(40).collect();
        out.push(cut.to_string());
    }
    for c in &el.children {
        collect_texts(c, out);
    }
}

/// Picks the explanation for a failed test, mirroring `velocity-mcp`'s
/// `failure.rs` `hint` wording for not-found traces.
fn explanation(ctx: &ExplanationCtx) -> (String, bool) {
    let lower = ctx.lower;
    let error = ctx.error;
    if let Some(c) = &ctx.crash {
        return (
            format!(
                "the app crashed during the test (crash report `{}`).",
                rel(c, ctx.base)
            ),
            true,
        );
    }
    if ctx.infrastructure {
        return (
            "the device or its connection failed, not the app (exit 3).".to_string(),
            true,
        );
    }
    if let Some(text) = lower
        .find("configuration error")
        .and_then(|i| after(&error[i..], &error[i..i + "configuration error".len()]))
    {
        return (
            format!("a configuration problem, not the app: {text}."),
            true,
        );
    }
    if let Some((x, y)) = assertion_values(error) {
        let mut why = format!("the check ran and failed: expected {x}, got {y}");
        if let Some(p) = &ctx.shot {
            let _ = write!(why, " (screenshot `{}`)", rel(p, ctx.base));
        }
        let _ = write!(why, ".");
        return (why, true);
    }
    let not_found = lower.contains("not found")
        || lower.contains("not visible")
        || lower.contains("no element");
    // The executor's note when another app was in front: "...; X was in
    // front, not Y".
    if let Some((front, ours)) = not_found.then(|| in_front(error)).flatten() {
        let mut why = format!("another app was in front: {front}, not the app under test ({ours})");
        if let Some(p) = &ctx.shot {
            let _ = write!(why, " (screenshot `{}`)", rel(p, ctx.base));
        }
        let _ = write!(why, "; the step looked for its element in that app.");
        if let Some((trace, trees, _)) = &ctx.description {
            if let Some(texts) = screen_texts(trees, trace) {
                let _ = write!(why, " The screen shows: {texts}.");
            }
        }
        return (why, true);
    }
    if let Some((trace, trees, d)) = &ctx.description {
        if not_found && d.selector.is_some() && d.matched.is_none() {
            if let Some(best) = d
                .closest
                .first()
                .filter(|b| b.score.unwrap_or(0.0) >= velocity_core::trace::RESEMBLES)
            {
                let mut why = format!(
                    "`{}` is not on the screen it saw",
                    d.selector.as_deref().unwrap_or("")
                );
                if let Some(p) = &d.screenshot {
                    let _ = write!(why, " (screenshot `{}`)", rel(p, ctx.base));
                }
                let _ = write!(
                    why,
                    "; the closest element is {} (score {:.02}). If that is the target, change the selector to it; otherwise the step ran on a different screen than intended.",
                    best.describe,
                    best.score.unwrap_or(0.0)
                );
                if let Some(texts) = screen_texts(trees, trace) {
                    let _ = write!(why, " The screen shows: {texts}.");
                }
                return (why, true);
            } else if d.tree_elements > 0 || !d.closest.is_empty() {
                let mut why = format!(
                    "`{}` is not on the screen it saw, and nothing on it resembles it",
                    d.selector.as_deref().unwrap_or("")
                );
                if let Some(p) = &d.screenshot {
                    let _ = write!(why, " (screenshot `{}`)", rel(p, ctx.base));
                }
                let _ = write!(
                    why,
                    "; the step most likely ran on a different screen or app than intended."
                );
                if let Some(texts) = screen_texts(trees, trace) {
                    let _ = write!(why, " The screen shows: {texts}.");
                }
                return (why, true);
            }
        }
    }
    // Fallback: logged errors during the step, then the old not-found-with-
    // screenshot sentence, then insufficient evidence.
    if let Some((name, _)) = &ctx.log_block {
        return (
            format!(
                "the app logged errors during the failed step (`{}`); check them against the step.",
                rel(Path::new(name), ctx.base)
            ),
            true,
        );
    }
    if not_found && ctx.shot.is_some() {
        return (
            format!(
                "the element the step looked for was not on the screen it saw (screenshot `{}`): the screen changed, or the selector no longer matches.",
                rel(ctx.shot.as_deref().unwrap_or(Path::new("")), ctx.base)
            ),
            true,
        );
    }
    (
        "insufficient evidence: no log errors, crash report or screenshot point to a cause. Open the trace and rerun with `--video always --logs always`.".to_string(),
        false,
    )
}

/// The inputs to [`explanation`].
struct ExplanationCtx<'a> {
    lower: &'a str,
    error: &'a str,
    crash: &'a Option<PathBuf>,
    shot: &'a Option<PathBuf>,
    base: &'a Path,
    infrastructure: bool,
    description: &'a Option<(
        TestTrace,
        HashMap<String, Element>,
        velocity_core::trace::FailureDescription,
    )>,
    log_block: &'a Option<(String, Vec<String>)>,
}

/// What to do next for a failure whose error reads like `lower` (review
/// 13 M4): the human counterpart of the agent errors' `next`.
fn try_next(lower: &str, infrastructure: bool) -> Option<&'static str> {
    if infrastructure {
        return Some(
            "`velocity doctor --fix` (restarts adb, reconnects offline devices), then run again",
        );
    }
    if lower.contains("timed out after") && lower.contains("--test-timeout") {
        return Some(
            "find the slow step in the trace; raise `--test-timeout` if the test is long by design",
        );
    }
    if lower.contains("not actionable") {
        return Some("wait for the element with `assertVisible` first, or add `force: true` to the step if it is covered on purpose");
    }
    if lower.contains("did not stabilize") || lower.contains("sync timeout") {
        return Some("`settle: none` on the step if the screen animates for ever (a spinner, a video), or raise `sync.settleCapMs`");
    }
    if lower.contains("element not found") || lower.contains("not found:") {
        return Some("`velocity trace view` shows the screen and the nearest elements; `velocity inspect` picks a selector");
    }
    None
}

/// The whole file. `base` is the artifacts folder paths are shown from.
pub fn render(result: &SuiteResult, base: &Path) -> String {
    let failed: Vec<&TestResult> = result
        .tests
        .iter()
        .filter(|t| t.status == TestStatus::Failed)
        .collect();
    let mut out = String::from("# Failures\n\n");
    if failed.is_empty() {
        let _ = writeln!(
            out,
            "None: {} of {} tests passed.",
            result.passed + result.retried,
            result.total
        );
        return out;
    }
    let mut cited = 0;
    let mut body = String::new();
    for t in &failed {
        let (s, c) = section(t, base);
        cited += usize::from(c);
        body.push_str(&s);
    }
    let _ = writeln!(
        out,
        "{} of {} tests failed. {cited} explanation(s) cite evidence; {} say \"insufficient evidence\".\n",
        failed.len(),
        result.total,
        failed.len() - cited
    );
    out.push_str(&body);
    out
}

/// Write `<dir>/failures.md`; its path.
pub fn write(result: &SuiteResult, dir: &str) -> velocity_common::Result<String> {
    let path = format!("{dir}/failures.md");
    std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&path, render(result, Path::new(dir))))
        .map_err(|e| velocity_common::VelocityError::io(format!("failed to write {path}"), e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use velocity_common::{Rect, Selector, StepResult};
    use velocity_core::trace::{StepTrace, TestTrace, TraceEvidence};

    fn failed(name: &str, err: &str) -> TestResult {
        let mut t = TestResult::failed(name, err.into(), Duration::from_secs(1));
        t.steps = vec![StepResult {
            step_index: 2,
            action_name: "tap".into(),
            status: StepStatus::Failed,
            duration: Duration::from_millis(5),
            screenshot: None,
            error_message: Some(err.into()),
            resource_delta: None,
            settle_ms: None,
            source: None,
        }];
        t
    }

    fn suite(tests: Vec<TestResult>) -> SuiteResult {
        SuiteResult {
            total: tests.len(),
            passed: 0,
            failed: tests.len(),
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests,
            shard_index: None,
            shard_total: None,
        }
    }

    #[test]
    fn each_explanation_cites_a_file_or_says_insufficient_evidence() {
        let d = tempfile::tempdir().unwrap();
        let mut gone = failed("search", "element not found: text=Results");
        gone.steps[0].screenshot = Some(d.path().join("shots/search-3.png"));
        let mystery = failed("odd", "assertion failed");
        let md = render(&suite(vec![gone, mystery]), d.path());
        assert!(
            md.contains("1 explanation(s) cite evidence; 1 say \"insufficient evidence\""),
            "{md}"
        );
        assert!(md.contains("screenshot `shots/search-3.png`"), "{md}");
        assert!(md.contains("- **Failed step:** 3 `tap`"), "{md}");
        assert!(md.contains("insufficient evidence: no log errors"), "{md}");
    }

    /// Builds a trace folder with `steps.json`, `trees/<hash>.json` and
    /// `logs.json`, and returns the trace path and the test dir.
    fn trace_fixture(
        d: &tempfile::TempDir,
        trace: &TestTrace,
        trees: &HashMap<String, Element>,
        logs: &[LogLine],
    ) -> PathBuf {
        let folder = d.path().join("traces/t");
        std::fs::create_dir_all(folder.join("trees")).unwrap();
        let path = folder.join("steps.json");
        std::fs::write(&path, serde_json::to_string(trace).unwrap()).unwrap();
        let hashes: Vec<_> = trace.steps.iter().flat_map(|s| &s.trees).cloned().collect();
        for h in hashes {
            if let Some(tree) = trees.get(&h) {
                std::fs::write(
                    folder.join("trees").join(format!("{h}.json")),
                    serde_json::to_string(tree).unwrap(),
                )
                .unwrap();
            }
        }
        if !logs.is_empty() {
            std::fs::write(
                folder.join("logs.json"),
                serde_json::to_string(logs).unwrap(),
            )
            .unwrap();
        }
        path
    }

    fn trace_test_result(err: &str, trace_path: PathBuf) -> TestResult {
        let mut t = failed("traced", err);
        t.trace = Some(trace_path);
        if let Some(st) = t.steps.first_mut() {
            st.screenshot = Some(PathBuf::from("shots/t.png"));
        }
        t
    }

    #[test]
    fn trace_not_found_names_closest_element_and_screen_texts() {
        let d = tempfile::tempdir().unwrap();
        let tree = Element {
            platform_id: "root".into(),
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            children: vec![
                Element {
                    platform_id: "wifi".into(),
                    text: Some("Wi-Fi".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 100,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
                Element {
                    platform_id: "gen".into(),
                    text: Some("General".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 200,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
                Element {
                    platform_id: "priv".into(),
                    text: Some("Privacy".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 300,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let hash = velocity_core::trace::tree_hash(&tree);
        let trace = TestTrace {
            version: 1,
            test: "traced".into(),
            device: "d".into(),
            platform: Some(velocity_common::Platform::Android),
            attempt: 0,
            status: TestStatus::Failed,
            steps: vec![StepTrace {
                index: 0,
                label: "tap".into(),
                start_ms: 1000,
                selector: Some(Selector::Text("Genral".into())),
                source: None,
                status: Some(StepStatus::Failed),
                duration_ms: 200,
                settle_ms: 0,
                settle_reads: 0,
                matched: None,
                nearest: vec![],
                heals: vec![],
                warnings: vec![],
                trees: vec![hash.clone()],
                error: Some("Element not found".into()),
                screenshot: Some(d.path().join("shots/t.png")),
            }],
            evidence: Some(TraceEvidence {
                video: None,
                video_offset_ms: None,
                screencast: None,
                logs: vec![],
                log_lines: Some("logs.json".into()),
                notes: vec![],
            }),
        };
        // The step ran 1000-1200 ms. A path with `e/` (counted as an error
        // before), an error outside the step, and one inside it from another
        // app: none is the cause, and none is printed under this Why.
        let logs = [
            LogLine {
                t: 1100,
                level: 'I',
                text: "constructing bundle record for file:///x/simruntime/Applications/y".into(),
            },
            LogLine {
                t: 5000,
                level: 'E',
                text: "outside the step window".into(),
            },
            LogLine {
                t: 1150,
                level: 'F',
                text: "another app: Fatal signal 6 (SIGABRT)".into(),
            },
        ];
        let path = trace_fixture(&d, &trace, &HashMap::from([(hash.clone(), tree)]), &logs);
        let t = trace_test_result("Element not found: text=\"Genral\"", path);
        let md = render(&suite(vec![t]), d.path());
        assert!(md.contains("closest element is text=\"General\""), "{md}");
        assert!(
            md.contains("The screen shows: `Wi-Fi`, `General`, `Privacy`."),
            "{md}"
        );
        assert!(!md.contains("simruntime"), "{md}");
        assert!(!md.contains("outside the step window"), "{md}");
        assert!(!md.contains("SIGABRT"), "not cited, so not printed: {md}");
        // One line: a newline inside the Why broke the Markdown bullet.
        let why = md.lines().find(|l| l.starts_with("- **Why:**")).unwrap();
        assert!(why.contains("The screen shows:"), "{md}");
    }

    #[test]
    fn a_weak_match_is_not_offered_as_the_target() {
        let d = tempfile::tempdir().unwrap();
        let tree = Element {
            platform_id: "root".into(),
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            children: vec![
                Element {
                    platform_id: "wifi".into(),
                    text: Some("Wi-Fi".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 100,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
                Element {
                    platform_id: "gen".into(),
                    text: Some("General".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 200,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
                Element {
                    platform_id: "priv".into(),
                    text: Some("Privacy".into()),
                    visible: true,
                    bounds: Rect {
                        x: 10,
                        y: 300,
                        width: 100,
                        height: 40,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let hash = velocity_core::trace::tree_hash(&tree);
        let trace = TestTrace {
            version: 1,
            test: "traced".into(),
            device: "d".into(),
            platform: Some(velocity_common::Platform::Android),
            attempt: 0,
            status: TestStatus::Failed,
            steps: vec![StepTrace {
                index: 0,
                label: "tap".into(),
                start_ms: 1000,
                selector: Some(Selector::Text("Airplane mode".into())),
                source: None,
                status: Some(StepStatus::Failed),
                duration_ms: 200,
                settle_ms: 0,
                settle_reads: 0,
                matched: None,
                nearest: vec![],
                heals: vec![],
                warnings: vec![],
                trees: vec![hash.clone()],
                error: Some("Element not found".into()),
                screenshot: Some(d.path().join("shots/t.png")),
            }],
            evidence: Some(TraceEvidence {
                video: None,
                video_offset_ms: None,
                screencast: None,
                logs: vec![],
                log_lines: Some("logs.json".into()),
                notes: vec![],
            }),
        };
        // The step ran 1000-1200 ms. A path with `e/` (counted as an error
        // before), an error outside the step, and one inside it from another
        // app: none is the cause, and none is printed under this Why.
        let logs = [
            LogLine {
                t: 1100,
                level: 'I',
                text: "constructing bundle record for file:///x/simruntime/Applications/y".into(),
            },
            LogLine {
                t: 5000,
                level: 'E',
                text: "outside the step window".into(),
            },
            LogLine {
                t: 1150,
                level: 'F',
                text: "another app: Fatal signal 6 (SIGABRT)".into(),
            },
        ];
        let path = trace_fixture(&d, &trace, &HashMap::from([(hash.clone(), tree)]), &logs);
        let t = trace_test_result("Element not found: text=\"Airplane mode\"", path);
        let md = render(&suite(vec![t]), d.path());
        // Nothing on a screen of Wi-Fi, General, Privacy resembles
        // "Airplane mode": the report says so instead of suggesting one.
        assert!(!md.contains("closest element"), "{md}");
        assert!(md.contains("nothing on it resembles it"), "{md}");
        assert!(
            md.contains("The screen shows: `Wi-Fi`, `General`, `Privacy`."),
            "{md}"
        );
    }

    #[test]
    fn logged_error_inside_window_cites_the_block_only() {
        let d = tempfile::tempdir().unwrap();
        let tree = Element {
            platform_id: "root".into(),
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            children: vec![],
            ..Default::default()
        };
        let hash = velocity_core::trace::tree_hash(&tree);
        let trace = TestTrace {
            version: 1,
            test: "traced".into(),
            device: "d".into(),
            platform: Some(velocity_common::Platform::Android),
            attempt: 0,
            status: TestStatus::Failed,
            steps: vec![StepTrace {
                index: 0,
                label: "tap".into(),
                start_ms: 1000,
                selector: None,
                source: None,
                status: Some(StepStatus::Failed),
                duration_ms: 200,
                settle_ms: 0,
                settle_reads: 0,
                matched: None,
                nearest: vec![],
                heals: vec![],
                warnings: vec![],
                trees: vec![hash.clone()],
                error: Some("Step timeout".into()),
                screenshot: None,
            }],
            evidence: Some(TraceEvidence {
                video: None,
                video_offset_ms: None,
                screencast: None,
                logs: vec![],
                log_lines: Some("logs.json".into()),
                notes: vec![],
            }),
        };
        let logs = vec![
            LogLine {
                t: 1200,
                level: 'E',
                text: "E/App: something broke".into(),
            },
            LogLine {
                t: 100,
                level: 'E',
                text: "E/App: old error".into(),
            },
        ];
        let path = trace_fixture(&d, &trace, &HashMap::from([(hash.clone(), tree)]), &logs);
        let t = trace_test_result("Step timeout", path);
        let md = render(&suite(vec![t]), d.path());
        assert!(
            md.contains("the app logged errors during the failed step (`logs.json`); check them against the step."),
            "{md}"
        );
        assert!(md.contains(" 1200 ms  E/App: something broke"), "{md}");
        assert!(!md.contains("old error"), "{md}");
    }

    #[test]
    fn assertion_failure_states_expected_and_got() {
        let d = tempfile::tempdir().unwrap();
        let t = failed(
            "checked",
            "Assertion failed: expected \"Information\", got \"About\"",
        );
        let md = render(&suite(vec![t]), d.path());
        assert!(
            md.contains("the check ran and failed: expected Information, got About."),
            "{md}"
        );
    }

    #[test]
    fn another_app_in_front_is_named() {
        let d = tempfile::tempdir().unwrap();
        let t = failed(
            "front",
            "assertVisible: Element not found: text=\"Network\" after 10000ms; \
             com.google.android.calendar was in front, not com.android.settings",
        );
        let md = render(&suite(vec![t]), d.path());
        assert!(
            md.contains(
                "another app was in front: com.google.android.calendar, not the app under test (com.android.settings)"
            ),
            "{md}"
        );
    }

    #[test]
    fn configuration_error_is_restated() {
        let d = tempfile::tempdir().unwrap();
        let t = failed(
            "cfg",
            "Configuration error: com.X is not installed on simulator BD7E26D9",
        );
        let md = render(&suite(vec![t]), d.path());
        assert!(
            md.contains(
                "a configuration problem, not the app: com.X is not installed on simulator BD7E26D9."
            ),
            "{md}"
        );
    }

    #[test]
    fn no_trace_keeps_the_old_screenshot_sentence() {
        let d = tempfile::tempdir().unwrap();
        let mut t = failed("gone", "element not found: text=Results");
        t.steps[0].screenshot = Some(d.path().join("shots/x.png"));
        let md = render(&suite(vec![t]), d.path());
        assert!(
            md.contains("the element the step looked for was not on the screen it saw (screenshot `shots/x.png`):"),
            "{md}"
        );
    }

    #[test]
    fn known_failure_kinds_say_what_to_try_next() {
        assert!(
            try_next("element not found: text=\"pay\" after 5000ms", false)
                .unwrap()
                .contains("trace view")
        );
        assert!(
            try_next("element text=\"pay\" is not actionable: covered", false)
                .unwrap()
                .contains("force: true")
        );
        assert!(
            try_next("sync timeout: ui did not stabilize within 3000ms", false)
                .unwrap()
                .contains("settle: none")
        );
        assert!(try_next("anything", true).unwrap().contains("doctor --fix"));
        assert_eq!(try_next("assertion failed: expected 1, got 2", false), None);
    }

    #[test]
    fn a_green_run_says_none_and_secrets_stay_masked() {
        let ok = suite(vec![]);
        assert!(render(&ok, Path::new(".")).contains("None: 0 of 0 tests passed."));
        velocity_common::redact::register_secret("hunter2-failures-md");
        let leak = failed(
            "login",
            "typed hunter2-failures-md into password: not found",
        );
        let md = render(&suite(vec![leak]), Path::new("."));
        assert!(!md.contains("hunter2-failures-md"), "{md}");
    }
}
