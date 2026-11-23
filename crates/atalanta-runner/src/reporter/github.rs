// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! GitHub Actions output: an annotation per failed or flaky test (shown on
//! the run and, with a file, on the PR diff), and a Markdown summary
//! appended to `$GITHUB_STEP_SUMMARY`.

use std::fmt::Write as _;
use std::io::Write as _;

use atalanta_common::{AtalantaError, Result, SuiteResult, TestResult, TestStatus};

/// Whether this process runs inside GitHub Actions.
pub fn in_github_actions() -> bool {
    atalanta_common::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
}

/// Print annotations to stdout and append the summary to
/// `$GITHUB_STEP_SUMMARY` when it is set. `suite_file` anchors the
/// annotations to the suite.
pub fn report(result: &SuiteResult, suite_file: &str) -> Result<()> {
    report_with(result, suite_file, &RunEnd::default())
}

/// How the run ended, for the summary's banner (C2).
#[derive(Debug, Clone, Default)]
pub struct RunEnd {
    /// `completed`, `timeout`, `interrupted:SIGTERM` (as in the JUnit
    /// `atalanta.termination` property); empty means completed.
    pub termination: String,
    /// The stopped run didn't return in time: the results are what had
    /// finished by then.
    pub partial: bool,
}

/// [`report`], with a banner atop the summary when the run didn't
/// complete.
pub fn report_with(result: &SuiteResult, suite_file: &str, end: &RunEnd) -> Result<()> {
    // Atalanta's own annotations must be read as commands again.
    resume_commands();
    let lines = capped_for(&step_key(), annotations(result, suite_file));
    for line in lines {
        println!("{line}");
    }
    if let Ok(path) = atalanta_common::env::var("GITHUB_STEP_SUMMARY") {
        let mut text = banner(result, end);
        text.push_str(&summary(result));
        // What the step's summary already holds, from this run or another
        // (the file is the step's own): a process-wide count in `atalanta
        // daemon` added up every client's run, and summaries went empty
        // for good once it passed 1 MiB (m-5).
        let written = std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
        fit_summary(&mut text, written);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| AtalantaError::Config(format!("GITHUB_STEP_SUMMARY {path}: {e}")))?;
        f.write_all(text.as_bytes())
            .map_err(|e| AtalantaError::Config(format!("GITHUB_STEP_SUMMARY {path}: {e}")))?;
    }
    Ok(())
}

/// `::add-mask::` for every registered secret: GitHub then masks it in
/// the job's log, this step's and later ones', not only in what Atalanta
/// prints itself.
pub fn mask_secrets() {
    for line in mask_lines() {
        println!("{line}");
    }
}

/// Shortest secret handed to GitHub's `::add-mask::`. GitHub masks a value
/// as a substring everywhere in the job's log, so a 3-digit PIN would shred
/// durations, ports and SHAs in every later step (M-2, research D-29).
/// Shorter secrets are still masked, whole-token, in what Atalanta prints.
const MIN_GITHUB_MASK_LEN: usize = 8;

fn mask_lines() -> Vec<String> {
    // Only secrets of 8+ characters, in every form: a short secret's
    // escaped form is long enough but would still mask a short value.
    // GitHub masks one line at a time: a multi-line secret is sent line
    // by line (each line is registered as a secret of its own, see
    // `redact::register_secret`), never as one value with `%0A` in it,
    // which matched nothing (C3).
    atalanta_common::redact::registered_secrets_at_least(MIN_GITHUB_MASK_LEN)
        .iter()
        .filter(|s| !s.contains(['\n', '\r']))
        .filter(|s| s.chars().count() >= MIN_GITHUB_MASK_LEN)
        .map(|s| format!("::add-mask::{}", escape_data(s)))
        .collect()
}

/// The token of the `::stop-commands::` in force, if any.
static STOPPED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Resumes workflow commands when dropped (see [`suspend_commands`]).
#[must_use = "commands resume when the guard is dropped"]
pub struct CommandsSuspended(bool);

impl Drop for CommandsSuspended {
    fn drop(&mut self) {
        if self.0 {
            resume_commands();
        }
    }
}

/// In GitHub Actions, stop the runner from reading workflow commands in
/// what the run prints until [`resume_commands`] (or the guard's drop).
/// App text, element labels and error messages are printed as they are, and
/// a line of theirs starting with `::` (`::add-path::`, `::set-output`,
/// `::error`) was executed by the runner (R-3). Call after
/// [`mask_secrets`], whose lines must still be read.
pub fn suspend_commands() -> CommandsSuspended {
    if !in_github_actions() {
        return CommandsSuspended(false);
    }
    CommandsSuspended(suspend_with(&mut |l| println!("{l}")))
}

/// Read workflow commands again; a no-op when they aren't stopped.
pub fn resume_commands() {
    resume_with(&mut |l| println!("{l}"));
}

/// Stop commands, printing through `out`. False when already stopped (the
/// outer guard resumes).
fn suspend_with(out: &mut dyn FnMut(String)) -> bool {
    let mut stopped = STOPPED.lock().unwrap_or_else(|e| e.into_inner());
    if stopped.is_some() {
        return false;
    }
    let token = random_token();
    out(format!("::stop-commands::{token}"));
    *stopped = Some(token);
    true
}

fn resume_with(out: &mut dyn FnMut(String)) {
    if let Some(token) = STOPPED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        out(format!("::{token}::"));
    }
}

/// 32 hex characters the app can't guess: std's per-process random hash
/// keys, so no new dependency.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::new();
    for i in 0..2u8 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u8(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        token.push_str(&format!("{:016x}", h.finish()));
    }
    token
}

/// GitHub shows 10 error and 10 warning annotations per step (and 50 per
/// job) and silently drops the rest. A folder run reports from one step.
const PER_STEP: usize = 10;

/// Annotations printed in the current step: its key (see [`step_key`])
/// and (errors, warnings, notices). Per step, not per process: in
/// `atalanta daemon` the counts added up across every client's run, and
/// after ten errors over the daemon's life no run got annotations (m-5).
static SHOWN: std::sync::Mutex<Option<(String, Shown)>> = std::sync::Mutex::new(None);

/// (errors, warnings, notices) shown.
type Shown = (usize, usize, usize);

/// Which GitHub step this run reports from (the client's environment in
/// the daemon): GitHub's limits are per step.
fn step_key() -> String {
    [
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_JOB",
        "GITHUB_ACTION",
        "GITHUB_STEP_SUMMARY",
    ]
    .iter()
    .map(|v| atalanta_common::env::var(v).unwrap_or_default())
    .collect::<Vec<_>>()
    .join("|")
}

/// [`capped`] against what step `key` has shown so far.
fn capped_for(key: &str, lines: Vec<String>) -> Vec<String> {
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    if shown.as_ref().is_none_or(|(k, _)| k != key) {
        *shown = Some((key.to_string(), (0, 0, 0)));
    }
    let (_, counts) = shown.as_mut().expect("set above");
    capped(lines, counts)
}

/// A step summary over 1 MiB fails to upload; stay under it.
const SUMMARY_LIMIT: usize = 1000 * 1024;

/// The annotations GitHub will show, errors first, within the per-step
/// limits `shown` has left; a notice says how many were left out.
fn capped(lines: Vec<String>, shown: &mut (usize, usize, usize)) -> Vec<String> {
    let (errors, warnings): (Vec<String>, Vec<String>) =
        lines.into_iter().partition(|l| l.starts_with("::error "));
    let mut out = Vec::new();
    let mut left_out = 0;
    for (group, used) in [(errors, &mut shown.0), (warnings, &mut shown.1)] {
        for line in group {
            if *used < PER_STEP {
                *used += 1;
                out.push(line);
            } else {
                left_out += 1;
            }
        }
    }
    if left_out > 0 && shown.2 < PER_STEP {
        shown.2 += 1;
        out.push(format!(
            "::notice title=Atalanta::{left_out} more failed or flaky test(s) not annotated (GitHub shows {PER_STEP} per step); see the job summary"
        ));
    }
    out
}

/// Cut `text` so the step summary stays under GitHub's limit, given what
/// was `written` before.
fn fit_summary(text: &mut String, written: usize) {
    let room = SUMMARY_LIMIT.saturating_sub(written);
    if text.len() <= room {
        return;
    }
    const NOTE: &str = "\n_… cut to fit GitHub's step summary limit; see the JUnit report._\n";
    let mut cut = room.saturating_sub(NOTE.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // Whole rows only.
    cut = text[..cut].rfind('\n').map_or(0, |i| i + 1);
    text.truncate(cut);
    if room >= NOTE.len() {
        text.push_str(NOTE);
    }
}

/// Workflow commands: `::error` for failures, `::warning` for flaky passes
/// and passes with warnings.
pub fn annotations(result: &SuiteResult, suite_file: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in &result.tests {
        let (level, msg) = match t.status {
            TestStatus::Failed if quarantined(t) => (
                "warning",
                format!(
                    "quarantined known failure: {}",
                    t.error_message.clone().unwrap_or_default()
                ),
            ),
            TestStatus::Failed => ("error", t.error_message.clone().unwrap_or_default()),
            TestStatus::Retried => (
                "warning",
                format!(
                    "flaky: passed after {} failed attempt(s). First failure: {}",
                    t.failed_attempts.len().max(t.retries as usize),
                    t.failed_attempts.first().map(String::as_str).unwrap_or("?")
                ),
            ),
            TestStatus::Passed if !t.from_cache && !t.warnings.is_empty() => (
                "warning",
                match t.healed_label() {
                    Some(label) => format!("{label}: {}", t.warnings.join("; ")),
                    None => t.warnings.join("; "),
                },
            ),
            _ => continue,
        };
        // The failing step's line puts the annotation on the PR diff where
        // the fix goes (a flow step's own line when it came from a flow);
        // otherwise the test's line.
        let at = failing_step_source(t).or(t.source.as_ref());
        let file = at
            .map(|p| p.file.as_str())
            .filter(|f| !f.is_empty())
            .unwrap_or(suite_file);
        let line = at.map(|p| format!(",line={}", p.line)).unwrap_or_default();
        out.push(format!(
            "::{level} file={}{line},title={}::{}",
            escape_property(file),
            escape_property(&format!("Atalanta: {}", t.test_name)),
            escape_data(&msg)
        ));
    }
    out
}

/// The job summary: totals, then a row per test that didn't simply pass.
/// A GitHub alert atop the step summary when the run was interrupted,
/// terminated or timed out: its table alone read like a complete run
/// with some skips (C2, D-20). Empty for a completed run.
pub fn banner(result: &SuiteResult, end: &RunEnd) -> String {
    let t = end.termination.as_str();
    let how = if t.is_empty() || t == "completed" {
        return String::new();
    } else if t == "timeout" {
        "stopped by the suite timeout (`--timeout`)".to_string()
    } else if let Some(sig) = t.strip_prefix("interrupted:") {
        let what = match sig {
            "SIGTERM" => "terminated (SIGTERM: the job was cancelled or timed out)",
            "SIGINT" => "interrupted (SIGINT)",
            "SIGHUP" => "terminated (SIGHUP)",
            other => return banner_text(result, end, &format!("interrupted ({other})")),
        };
        what.to_string()
    } else {
        "interrupted".to_string()
    };
    banner_text(result, end, &how)
}

fn banner_text(result: &SuiteResult, end: &RunEnd, how: &str) -> String {
    let not_run = result
        .tests
        .iter()
        .filter(|t| {
            t.status == TestStatus::Skipped
                && t.error_message
                    .as_deref()
                    .is_some_and(|m| m.starts_with("not run") || m.starts_with("not finished"))
        })
        .count();
    let mut s = format!(
        "> [!WARNING]\n> **Atalanta run {how}.** {not_run} of {} test(s) did not run or finish; these results are partial.\n",
        result.tests.len()
    );
    if end.partial {
        s.push_str(
            "> The run did not stop within its grace period: the tests shown are those that had finished by then.\n",
        );
    }
    s.push('\n');
    s
}

pub fn summary(result: &SuiteResult) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "### Atalanta: {} passed, {} failed, {} flaky, {} skipped ({:.1}s)\n",
        result.passed,
        result.failed,
        result.retried,
        result.skipped,
        result.duration.as_secs_f64()
    );
    let notable: Vec<&TestResult> = result
        .tests
        .iter()
        .filter(|t| t.status != TestStatus::Passed || !t.warnings.is_empty())
        .collect();
    if notable.is_empty() {
        return s;
    }
    s.push_str("| Test | Result | Time | Detail |\n|---|---|---|---|\n");
    for t in notable {
        let healed = t.healed_label().map(|l| format!("⚠️ {l}"));
        let status = match t.status {
            TestStatus::Passed if healed.is_some() => healed.as_deref().unwrap_or_default(),
            TestStatus::Passed => "⚠️ passed",
            TestStatus::Failed if quarantined(t) => "🔇 quarantined",
            TestStatus::Failed => "❌ failed",
            TestStatus::Retried => "🔁 flaky",
            TestStatus::Skipped => "⏭ skipped",
        };
        let detail = t
            .error_message
            .clone()
            .or_else(|| t.warnings.first().cloned())
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "| {} | {status} | {:.1}s | {} |",
            cell(&t.test_name),
            t.duration.as_secs_f64(),
            cell(first_line(&detail))
        );
    }
    s.push('\n');
    s
}

/// Where the step that failed is written, if it is known.
fn failing_step_source(t: &TestResult) -> Option<&atalanta_common::SourcePos> {
    t.steps
        .iter()
        .find(|s| s.status == atalanta_common::StepStatus::Failed)
        .and_then(|s| s.source.as_ref())
}

/// Marked by the quarantine list (see `quarantine.rs`).
fn quarantined(t: &TestResult) -> bool {
    t.quarantined.is_some()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

// Escaping rules of GitHub's workflow commands (actions/toolkit `command.ts`).
fn escape_data(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_property(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn suite(tests: Vec<TestResult>) -> SuiteResult {
        let count = |s: TestStatus| tests.iter().filter(|t| t.status == s).count();
        SuiteResult {
            total: tests.len(),
            passed: count(TestStatus::Passed),
            failed: count(TestStatus::Failed),
            skipped: count(TestStatus::Skipped),
            retried: count(TestStatus::Retried),
            duration: Duration::from_secs(3),
            tests,
            shard_index: None,
            shard_total: None,
        }
    }

    fn healed() -> TestResult {
        let mut t = TestResult::failed("login", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.warnings = vec!["selector text=\"Log in\" not found; healed to text=\"Sign in\" (confidence 0.92) — update the selector".into()];
        t
    }

    #[test]
    fn healed_passes_say_so_in_annotations_and_the_summary() {
        let r = suite(vec![healed()]);
        let notes = annotations(&r, "app.yaml");
        assert!(
            notes[0].starts_with("::warning") && notes[0].contains("PASSED (healed 1)"),
            "{notes:?}"
        );
        assert!(summary(&r).contains("PASSED (healed 1)"), "{}", summary(&r));
    }

    // V6: every pass served from the result cache carries a "not run" warning,
    // so each became a `::warning`; with 10 shown per step, they pushed a
    // real flaky test's annotation out of the run.
    #[test]
    fn cached_passes_take_none_of_the_annotation_slots() {
        let cached = |n: usize| {
            let mut t = TestResult::failed(&format!("cached {n}"), String::new(), Duration::ZERO);
            t.status = TestStatus::Passed;
            t.error_message = None;
            t.from_cache = true;
            t.warnings =
                vec!["not run: result cache (app, test, config and device unchanged)".into()];
            t
        };
        let mut flaky = TestResult::failed("flaky one", String::new(), Duration::ZERO);
        flaky.status = TestStatus::Retried;
        flaky.error_message = None;
        flaky.failed_attempts = vec!["timeout".into()];
        let mut tests: Vec<TestResult> = (0..30).map(cached).collect();
        tests.push(flaky);
        let notes = annotations(&suite(tests), "app.yaml");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("flaky one"), "{notes:?}");
    }

    // R-3: app text such as an error message holding "\n::error::x" or
    // "::add-path::" ran as a workflow command. Output is wrapped in a
    // stop-commands block whose token appears once more, at the resume.
    #[test]
    fn printed_app_text_cannot_run_workflow_commands() {
        let mut out = Vec::new();
        assert!(suspend_with(&mut |l| out.push(l)));
        assert!(
            !suspend_with(&mut |l| out.push(l)),
            "nested: no second stop"
        );
        out.push("Element not found:\n::error::x".into());
        resume_with(&mut |l| out.push(l));
        resume_with(&mut |l| out.push(l));
        let token = out[0]
            .strip_prefix("::stop-commands::")
            .unwrap()
            .to_string();
        assert_eq!(token.len(), 32);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[2], format!("::{token}::"));
        assert_eq!(out.iter().filter(|l| l.contains(&token)).count(), 2);
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn failures_become_errors_and_flakes_warnings() {
        let failed = TestResult::failed(
            "login, then pay",
            "Element not found:\nx".into(),
            Duration::ZERO,
        );
        let mut flaky = TestResult::failed("search", String::new(), Duration::ZERO);
        flaky.status = TestStatus::Retried;
        flaky.error_message = None;
        flaky.failed_attempts = vec!["timeout".into()];
        let lines = annotations(&suite(vec![failed, flaky]), "tests/app.yaml");
        assert_eq!(lines.len(), 2);
        assert!(lines[0]
            .starts_with("::error file=tests/app.yaml,title=Atalanta%3A login%2C then pay::"));
        assert!(lines[0].ends_with("Element not found:%0Ax"));
        assert!(lines[1].starts_with("::warning "));
        assert!(lines[1].contains("First failure: timeout"));
    }

    #[test]
    fn quarantined_failures_are_warnings() {
        let mut t = TestResult::failed("known", "boom".into(), Duration::ZERO);
        t.warnings.push("quarantined: DEMO-1".into());
        t.quarantined = Some("DEMO-1".into());
        let r = suite(vec![t]);
        assert!(annotations(&r, "s.yaml")[0].starts_with("::warning "));
        assert!(summary(&r).contains("🔇 quarantined"));
    }

    #[test]
    fn summary_lists_only_tests_that_need_a_look() {
        let mut ok = TestResult::failed("ok", String::new(), Duration::ZERO);
        ok.status = TestStatus::Passed;
        ok.error_message = None;
        let bad = TestResult::failed("bad | test", "boom".into(), Duration::ZERO);
        let s = summary(&suite(vec![ok, bad]));
        assert!(s.contains("1 passed, 1 failed"));
        assert!(s.contains("| bad \\| test | ❌ failed |"));
        assert!(!s.contains("| ok |"));
    }

    #[test]
    fn annotations_sit_on_the_failing_step_line_or_else_the_test_line() {
        use atalanta_common::{SourcePos, StepResult, StepStatus};
        let mut on_step = TestResult::failed("pay", "Element not found".into(), Duration::ZERO);
        on_step.source = Some(SourcePos::new("tests/app.yaml", 20, 5));
        for (i, (status, line)) in [(StepStatus::Passed, 22), (StepStatus::Failed, 42)]
            .into_iter()
            .enumerate()
        {
            on_step.steps.push(StepResult {
                step_index: i,
                action_name: "tap".into(),
                status,
                duration: Duration::ZERO,
                screenshot: None,
                error_message: None,
                resource_delta: None,
                settle_ms: None,
                source: Some(SourcePos::new("tests/app.yaml", line, 9)),
            });
        }
        let mut on_test = TestResult::failed("boot", "device lost".into(), Duration::ZERO);
        on_test.source = Some(SourcePos::new("tests/app.yaml", 7, 5));
        let lines = annotations(&suite(vec![on_step, on_test]), "tests/app.yaml");
        assert!(
            lines[0].starts_with("::error file=tests/app.yaml,line=42,title="),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("::error file=tests/app.yaml,line=7,title="),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn annotations_stay_within_githubs_per_step_limits() {
        let mut tests: Vec<TestResult> = (0..15)
            .map(|i| TestResult::failed(&format!("f{i}"), "boom".into(), Duration::ZERO))
            .collect();
        for i in 0..3 {
            let mut t = TestResult::failed(&format!("w{i}"), "x".into(), Duration::ZERO);
            t.status = TestStatus::Retried;
            tests.push(t);
        }
        let mut shown = (0, 0, 0);
        let out = capped(annotations(&suite(tests), "s.yaml"), &mut shown);
        assert_eq!(out.iter().filter(|l| l.starts_with("::error ")).count(), 10);
        assert_eq!(
            out.iter().filter(|l| l.starts_with("::warning ")).count(),
            3
        );
        assert!(
            out.last().unwrap().starts_with("::notice ") && out.last().unwrap().contains("5 more")
        );
        // A second suite in the same step: no error annotations left.
        let more = vec![TestResult::failed("g", "boom".into(), Duration::ZERO)];
        let out = capped(annotations(&suite(more), "s.yaml"), &mut shown);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("::notice "), "{out:?}");
    }

    // m-5: the caps are per step. A later step (another daemon client's
    // run) gets its own ten error annotations.
    #[test]
    fn annotation_caps_are_per_step_not_per_process() {
        let fails = |n: usize| -> Vec<TestResult> {
            (0..n)
                .map(|i| TestResult::failed(&format!("f{i}"), "boom".into(), Duration::ZERO))
                .collect()
        };
        let errors = |out: &[String]| out.iter().filter(|l| l.starts_with("::error ")).count();
        let out = capped_for("run1|step-a", annotations(&suite(fails(12)), "s.yaml"));
        assert_eq!(errors(&out), 10);
        // The same step: nothing left.
        let out = capped_for("run1|step-a", annotations(&suite(fails(1)), "s.yaml"));
        assert_eq!(errors(&out), 0);
        // Another step: a fresh allowance.
        let out = capped_for("run2|step-b", annotations(&suite(fails(3)), "s.yaml"));
        assert_eq!(errors(&out), 3);
    }

    // C2: an interrupted or terminated run's summary looked complete.
    #[test]
    fn an_interrupted_run_gets_a_banner_and_a_completed_one_none() {
        let mut ok = TestResult::failed("a", String::new(), Duration::from_secs(1));
        ok.status = TestStatus::Passed;
        let cut = TestResult::skipped("b", "not finished: interrupted by SIGTERM while it ran");
        let rest = TestResult::skipped("c", "not run: interrupted by SIGTERM");
        let skip = TestResult::skipped("d", "Skipped due to --fail-fast");
        let r = suite(vec![ok, cut, rest, skip]);
        let end = |t: &str, partial| RunEnd {
            termination: t.into(),
            partial,
        };
        assert_eq!(banner(&r, &end("completed", false)), "");
        assert_eq!(banner(&r, &RunEnd::default()), "");
        let b = banner(&r, &end("interrupted:SIGTERM", false));
        assert!(b.starts_with("> [!WARNING]\n"), "{b}");
        assert!(b.contains("terminated (SIGTERM"), "{b}");
        assert!(b.contains("2 of 4 test(s) did not run or finish"), "{b}");
        assert!(!b.contains("grace"), "{b}");
        let b = banner(&r, &end("interrupted:SIGINT", true));
        assert!(b.contains("interrupted (SIGINT)"), "{b}");
        assert!(b.contains("did not stop within its grace period"), "{b}");
        assert!(banner(&r, &end("timeout", false)).contains("suite timeout"));
    }

    #[test]
    fn the_summary_is_cut_to_githubs_limit() {
        let mut text = "row\n".repeat(400_000);
        fit_summary(&mut text, 0);
        assert!(text.len() <= SUMMARY_LIMIT);
        assert!(text.ends_with("JUnit report._\n"));
        let mut small = "ok\n".to_string();
        fit_summary(&mut small, 0);
        assert_eq!(small, "ok\n");
    }

    #[test]
    fn registered_secrets_are_masked_for_github() {
        atalanta_common::redact::register_secret("gh-mask-7Qz%secret");
        let lines = mask_lines();
        assert!(
            lines.contains(&"::add-mask::gh-mask-7Qz%25secret".to_string()),
            "{lines:?}"
        );
    }

    // M-2: a 3-character secret went to `::add-mask::`, and GitHub then
    // masked every `123` in the job log.
    #[test]
    fn short_secrets_are_not_sent_to_github() {
        atalanta_common::redact::register_secret("481");
        atalanta_common::redact::register_secret("Pin7Abc");
        let lines = mask_lines();
        assert!(
            !lines
                .iter()
                .any(|l| l == "::add-mask::481" || l == "::add-mask::Pin7Abc"),
            "{lines:?}"
        );
        // Atalanta's own masking is unchanged.
        assert_eq!(
            atalanta_common::redact::redact("pin 481 ok"),
            format!("pin {} ok", atalanta_common::redact::MASK)
        );
    }

    // C3: a multi-line secret is masked with one `add-mask` per line;
    // the whole value with `%0A` in it masked nothing.
    #[test]
    fn a_multi_line_secret_is_masked_line_by_line() {
        atalanta_common::redact::register_secret(
            "c3-gh-first-line-AAAA\r\nc3-gh-second-line-BBBB\nshort\n",
        );
        let lines = mask_lines();
        assert!(
            lines.contains(&"::add-mask::c3-gh-first-line-AAAA".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"::add-mask::c3-gh-second-line-BBBB".to_string()),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("c3-gh") && (l.contains("%0A") || l.contains("%0D"))),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l == "::add-mask::short"), "{lines:?}");
    }

    // D-29: a short secret's escaped forms are 8+ characters, but still a
    // short secret's: no `add-mask` for any of them.
    #[test]
    fn no_form_of_a_short_secret_is_sent_to_github() {
        atalanta_common::redact::register_secret("d29\"<&");
        let lines = mask_lines();
        assert!(!lines.iter().any(|l| l.contains("d29")), "{lines:?}");
    }
}
