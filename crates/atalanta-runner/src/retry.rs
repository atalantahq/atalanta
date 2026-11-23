// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Retrying and reporting one test — shared by the sequential and parallel
//! runners.

use std::time::Instant;

use atalanta_common::{TestCase, TestResult, TestStatus};

/// The first of `test`'s dependencies that ran in this session and did not
/// pass (passed-on-retry counts as passed).
pub(crate) fn unmet_dependency<'a>(test: &'a TestCase, results: &[TestResult]) -> Option<&'a str> {
    test.depends_on
        .iter()
        .find(|d| {
            results.iter().any(|r| {
                &r.test_name == *d && !matches!(r.status, TestStatus::Passed | TestStatus::Retried)
            })
        })
        .map(String::as_str)
}
use atalanta_core::TestExecutor;

/// Most retries one test gets, whatever `--retries` or `retry.count` say
/// (Bazel bounds `--flaky_test_attempts` to 10 the same way).
pub const MAX_TEST_RETRIES: u32 = 10;

/// `retries` capped at [`MAX_TEST_RETRIES`], with a warning when that
/// changed it: the cap was silent, and `--parallel` didn't apply it (m-5).
pub(crate) fn capped_retries(retries: u32) -> u32 {
    if retries > MAX_TEST_RETRIES {
        let msg = format!("retries {retries} capped at {MAX_TEST_RETRIES} per test");
        tracing::warn!("{msg}");
        eprintln!("warning: {msg}");
    }
    retries.min(MAX_TEST_RETRIES)
}

/// Run `test`, retrying a failure up to `max_retries` times. A test that passes
/// only on retry is reported as [`TestStatus::Retried`] — flaky, not passed —
/// and keeps the first failure's message so the flake can be investigated.
pub(crate) async fn run_test_with_retries(
    executor: &mut TestExecutor<'_>,
    test: &TestCase,
    device_id: &str,
    app_id: &str,
    max_retries: u32,
    test_timeout: Option<std::time::Duration>,
) -> TestResult {
    run_test_with_retries_from(
        executor,
        test,
        device_id,
        app_id,
        max_retries,
        0,
        test_timeout,
    )
    .await
}

/// The failure of an attempt cut off by `--test-timeout`.
pub(crate) fn test_timeout_message(limit: std::time::Duration) -> String {
    format!(
        "test timed out after {} (--test-timeout): this attempt was stopped; the rest of the run goes on",
        short(limit)
    )
}

/// `90s`, `1.5s`, `250ms`.
fn short(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// [`run_test_with_retries`], the first attempt here being attempt
/// `base_attempt` of the test: a retry the parallel runner moved to another
/// device is not a first attempt, so the executor never skips its launch
/// for batching (it would start on whatever the device's previous test
/// left on screen).
pub(crate) async fn run_test_with_retries_from(
    executor: &mut TestExecutor<'_>,
    test: &TestCase,
    device_id: &str,
    app_id: &str,
    max_retries: u32,
    base_attempt: u32,
    test_timeout: Option<std::time::Duration>,
) -> TestResult {
    // `--retries 1000000` ran a failing test a million times (VEL-176).
    let max_retries = max_retries.min(MAX_TEST_RETRIES);
    let test_start = Instant::now();
    let mut attempts = 0u32;
    let mut first_failure: Option<String> = None;
    let mut failed_attempts = Vec::new();
    let mut warnings = Vec::new();
    // A flaky pass keeps the trace of the attempt that failed.
    let mut failed_trace = None;
    // D-19: the first attempt that panicked. A panic is Atalanta's own bug;
    // a later attempt passing doesn't make it go away, so the test still
    // ends as an internal error (exit 5). It is still retried, so the
    // report shows whether the app itself works.
    let mut panicked: Option<String> = None;

    let mut result = loop {
        executor.set_retry_attempt(base_attempt.saturating_add(attempts));
        // A panic inside a test (a driver or layout bug) fails that test
        // instead of killing the run: in `--parallel` it killed the actor
        // and the run waited for it forever.
        let attempt_start = Instant::now();
        let run = std::panic::AssertUnwindSafe(executor.execute_test(test, device_id, app_id));
        let run = futures::FutureExt::catch_unwind(run);
        let outcome = match test_timeout {
            // One test that waits on every step must not hold its device
            // until the suite timeout ends the whole run (review 12 #9).
            Some(limit) => tokio::time::timeout(limit, run).await.ok(),
            None => Some(run.await),
        };
        let mut result = match outcome {
            None => TestResult::failed(
                &test.name,
                test_timeout_message(test_timeout.unwrap_or_default()),
                attempt_start.elapsed(),
            ),
            Some(Ok(Ok(r))) => r,
            Some(Ok(Err(e))) => TestResult::failed(&test.name, e.to_string(), test_start.elapsed()),
            // A bug in Atalanta, not the app: exit 5 (D-19).
            Some(Err(panic)) => TestResult {
                internal_error: true,
                ..TestResult::failed(
                    &test.name,
                    format!(
                        "internal error: Atalanta panicked: {}",
                        panic_message(&*panic)
                    ),
                    test_start.elapsed(),
                )
            },
        };
        if result.status == TestStatus::Failed && attempts < max_retries {
            retire_heal_notes(&mut result.warnings, attempts);
        }
        warnings.append(&mut result.warnings);
        if result.internal_error && panicked.is_none() {
            panicked = result.error_message.clone();
        }
        if result.status != TestStatus::Failed || attempts >= max_retries {
            break result;
        }
        if first_failure.is_none() {
            first_failure = result.error_message.clone();
        }
        if result.trace.is_some() {
            failed_trace = result.trace.clone();
        }
        failed_attempts.push(
            result
                .error_message
                .clone()
                .unwrap_or_else(|| "failed".to_string()),
        );
        attempts += 1;
    };

    executor.set_retry_attempt(0);
    if attempts > 0 && result.status == TestStatus::Passed {
        result.status = TestStatus::Retried;
        if let Some(first) = first_failure {
            warnings.insert(
                0,
                format!("flaky: failed {attempts}x before passing: {first}"),
            );
        }
    }
    keep_internal_error(&mut result, panicked, &mut warnings);
    warnings.dedup();
    result.warnings = warnings;
    result.failed_attempts = failed_attempts;
    result.retries = attempts;
    result.duration = test_start.elapsed();
    if result.trace.is_none() {
        result.trace = failed_trace;
    }
    result
}

/// The heal notes of an attempt that failed and is retried stay in the
/// result as evidence, prefixed with the attempt, so they no longer read
/// as heal notes: only the final attempt's heals count as "healed"
/// (review-fixes #7; before, `FLAKY (passed on retry 1, healed 1)` for a
/// pass that healed nothing).
pub(crate) fn retire_heal_notes(warnings: &mut [String], attempt: u32) {
    for w in warnings.iter_mut() {
        if atalanta_common::result::is_heal_note(w) {
            *w = format!("attempt {} (failed): {w}", attempt + 1);
        }
    }
}

/// D-19: an attempt that panicked (`panicked`, its error) makes the whole
/// test an internal error, even when a retry then passed: the result is
/// Failed with `internal_error` set, so it exits 5, is never quarantined
/// away, and `--fail-on-flaky` has nothing to decide. The passing retry is
/// kept as a warning, as evidence that the app itself is fine.
pub(crate) fn keep_internal_error(
    result: &mut TestResult,
    panicked: Option<String>,
    warnings: &mut Vec<String>,
) {
    let Some(panic) = panicked else {
        return;
    };
    if result.internal_error {
        return;
    }
    if matches!(result.status, TestStatus::Passed | TestStatus::Retried) {
        warnings.insert(
            0,
            "a retry passed, but an earlier attempt hit an internal error (a Atalanta bug)"
                .to_string(),
        );
    }
    // Drop the "flaky: …" line: this is not a flaky pass.
    warnings.retain(|w| !w.starts_with("flaky"));
    result.status = TestStatus::Failed;
    result.internal_error = true;
    result.error_message = Some(panic);
}

/// The message a panic was raised with.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Print one test's result line, its failure reason and any warnings.
pub(crate) fn print_result(result: &TestResult, device: Option<&str>) {
    crate::live::record(result);
    let (icon, label) = match result.status {
        TestStatus::Passed => match result.healed_label() {
            // D-7 / R4: a heal is never a silent pass.
            Some(label) => ("\x1b[33m●\x1b[0m", label),
            None => ("\x1b[32m●\x1b[0m", "PASSED".to_string()),
        },
        TestStatus::Failed => ("\x1b[31m●\x1b[0m", "FAILED".to_string()),
        TestStatus::Skipped => ("\x1b[33m●\x1b[0m", "SKIPPED".to_string()),
        TestStatus::Retried => (
            "\x1b[33m●\x1b[0m",
            result
                .healed_label()
                .unwrap_or_else(|| format!("FLAKY (passed on retry {})", result.retries)),
        ),
    };
    let device = device.map(|d| format!(" [{d}]")).unwrap_or_default();
    let dur = result.duration.as_secs_f64();
    cprintln!(
        "  {icon} {}{device} ... {label} {dur:.1}s",
        result.test_name
    );
    if matches!(result.status, TestStatus::Failed | TestStatus::Skipped) {
        if let Some(ref msg) = result.error_message {
            println!("    └ {}", atalanta_common::redact::redact(msg));
        }
    }
    for w in &result.warnings {
        cprintln!(
            "    \x1b[33m⚠ {}\x1b[0m",
            atalanta_common::redact::redact(w)
        );
    }
}

#[cfg(test)]
mod heal_count_tests {
    use super::*;
    use std::time::Duration;

    /// review-fixes #7: attempt 1 heals and fails, attempt 2 passes without
    /// healing; the result (warnings composed as `run_test_with_retries`
    /// does) counts no heal, and keeps attempt 1's note as evidence.
    #[test]
    fn only_the_final_attempts_heals_count() {
        let note = "selector text=\"Log in\" not found; healed to text=\"Sign in\" \
                    (confidence 0.92) — update the selector"
            .to_string();
        let mut first = TestResult::failed("a", "x".into(), Duration::ZERO);
        first.warnings = vec![note.clone()];
        retire_heal_notes(&mut first.warnings, 0);
        let mut passed = TestResult::failed("a", String::new(), Duration::ZERO);
        passed.status = TestStatus::Retried;
        passed.retries = 1;
        passed.warnings = first.warnings.clone();
        assert_eq!(passed.healed_count(), 0, "{:?}", passed.warnings);
        assert_eq!(passed.healed_label(), None);
        assert!(passed.warnings[0].starts_with("attempt 1 (failed): selector "));
        // The passing attempt's own heal still counts.
        passed.warnings.push(note);
        assert_eq!(passed.healed_count(), 1);
    }
}
