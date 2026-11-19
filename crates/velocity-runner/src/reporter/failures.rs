//! `failures.md` (row 2.2: REP-080, VOI-007), written by every run: each
//! failed test with the step that failed, the error, the end of its app log,
//! and the files that show it. The explanation only says what the evidence
//! shows, and cites it (a file and its lines, a screenshot); with nothing to
//! cite it says "insufficient evidence" instead of guessing. Showing sources
//! reduces reliance on wrong answers (CHI 2025).

use std::fmt::Write as _;
use std::path::Path;

use velocity_common::evidence::AttachmentKind;
use velocity_common::{StepStatus, SuiteResult, TestResult, TestStatus};

/// Lines of the app log kept, at most.
const LOG_TAIL_LINES: usize = 20;
/// Characters of each kept line, at most.
const LOG_LINE_CHARS: usize = 300;

/// The last lines of `path` mentioning an error, else its last lines, with
/// their 1-based line numbers.
fn log_tail(path: &Path) -> Option<(Vec<(usize, String)>, bool)> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let hot = |l: &str| {
        let l = l.to_ascii_lowercase();
        ["error", "exception", "fatal", "crash", " e ", "e/", "panic"]
            .iter()
            .any(|k| l.contains(k))
    };
    let errors: Vec<usize> = (0..lines.len()).filter(|&i| hot(lines[i])).collect();
    let (picked, found_errors): (Vec<usize>, bool) = if errors.is_empty() {
        (lines.len().saturating_sub(LOG_TAIL_LINES)..lines.len())
            .collect::<Vec<_>>()
            .pipe(|v| (v, false))
    } else {
        (
            errors[errors.len().saturating_sub(LOG_TAIL_LINES)..].to_vec(),
            true,
        )
    };
    let out = picked
        .into_iter()
        .map(|i| {
            let l: String = lines[i].chars().take(LOG_LINE_CHARS).collect();
            (i + 1, velocity_common::redact::redact(&l).into_owned())
        })
        .collect::<Vec<_>>();
    (!out.is_empty()).then_some((out, found_errors))
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

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
    let mut app_log = None;
    let mut crash = None;
    for a in &t.attachments {
        let what = match a.kind {
            AttachmentKind::Video => "video",
            AttachmentKind::Screencast => "screencast",
            AttachmentKind::AppLog => {
                app_log = Some(a.path.clone());
                "app log"
            }
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
    let tail = app_log.as_deref().and_then(log_tail);

    // The explanation: only what a cited file shows.
    let lower = error.to_ascii_lowercase();
    let (why, cited) = if let Some(c) = &crash {
        (
            format!(
                "the app crashed during the test (crash report `{}`).",
                rel(c, base)
            ),
            true,
        )
    } else if let Some((lines, true)) = &tail {
        let (first, last) = (lines[0].0, lines[lines.len() - 1].0);
        (
            format!(
                "the app logged errors while the step ran (`{}` lines {first}-{last}, below); check them against the step.",
                rel(app_log.as_deref().unwrap_or(Path::new("app.log")), base)
            ),
            true,
        )
    } else if (lower.contains("not found")
        || lower.contains("not visible")
        || lower.contains("no element"))
        && shot.is_some()
    {
        (
            format!(
                "the element the step looked for was not on the screen it saw (screenshot `{}`): the screen changed, or the selector no longer matches.",
                rel(shot.as_deref().unwrap_or(Path::new("")), base)
            ),
            true,
        )
    } else if t.infrastructure_failure {
        (
            "the device or its connection failed, not the app (exit 3).".to_string(),
            true,
        )
    } else {
        (
            "insufficient evidence: no log errors, crash report or screenshot point to a cause. Open the trace and rerun with `--video always --logs always`.".to_string(),
            false,
        )
    };
    let _ = writeln!(s, "- **Why:** {why}");
    if let Some(next) = try_next(&lower, t.infrastructure_failure) {
        let _ = writeln!(s, "- **Try:** {next}");
    }
    if let Some((lines, _)) = tail {
        let _ = writeln!(s, "\n```text");
        for (n, l) in lines {
            let _ = writeln!(s, "{n:>6}  {l}");
        }
        let _ = writeln!(s, "```");
    }
    s.push('\n');
    (s, cited)
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
    use velocity_common::evidence::Attachment;
    use velocity_common::StepResult;

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
        let log = d.path().join("traces/pay/app.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(
            &log,
            "I/App: start\nI/App: cart\nE/App: PaymentException: card declined\nI/App: retry\n",
        )
        .unwrap();
        let mut pay = failed("pay", "assertVisible text=Thanks: not found");
        pay.attachments
            .push(Attachment::new(AttachmentKind::AppLog, log.clone()));
        let mut gone = failed("search", "element not found: text=Results");
        gone.steps[0].screenshot = Some(d.path().join("shots/search-3.png"));
        let mystery = failed("odd", "assertion failed");
        let md = render(&suite(vec![pay, gone, mystery]), d.path());
        assert!(
            md.contains("2 explanation(s) cite evidence; 1 say \"insufficient evidence\""),
            "{md}"
        );
        assert!(md.contains("`traces/pay/app.log` lines 3-3"), "{md}");
        assert!(
            md.contains("     3  E/App: PaymentException: card declined"),
            "{md}"
        );
        assert!(md.contains("screenshot `shots/search-3.png`"), "{md}");
        assert!(md.contains("- **Failed step:** 3 `tap`"), "{md}");
        assert!(md.contains("insufficient evidence: no log errors"), "{md}");
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
