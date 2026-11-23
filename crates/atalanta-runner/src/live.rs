//! The results of the run in progress, kept as each test finishes (C2,
//! N-9). A stopped run normally returns its results itself; when it
//! doesn't within the CLI's stop grace (a driver call blocked in a
//! non-cancellable wait), the CLI drops the run and writes its partial
//! reports from here, instead of writing none.
//!
//! Process-wide: one run at a time per process (`atalanta run`, or one
//! client's run under `atalanta daemon`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use atalanta_common::{SuiteResult, TestResult, TestStatus};

struct Live {
    started: Option<Instant>,
    /// Every test the run plans to run, in order.
    planned: Vec<String>,
    /// Results as they finished, a test's latest one last.
    finished: Vec<TestResult>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    started: None,
    planned: Vec::new(),
    finished: Vec::new(),
});

fn live() -> std::sync::MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A run is starting: forget the previous one's.
pub fn reset() {
    let mut l = live();
    l.started = Some(Instant::now());
    l.planned.clear();
    l.finished.clear();
}

/// The tests the runner plans to run (after selection and sharding).
pub(crate) fn plan<'a>(names: impl IntoIterator<Item = &'a str>) {
    let mut l = live();
    for n in names {
        if !l.planned.iter().any(|p| p == n) {
            l.planned.push(n.to_string());
        }
    }
}

/// A test finished (or was skipped): its result as printed.
pub(crate) fn record(result: &TestResult) {
    let mut l = live();
    l.finished.retain(|r| r.test_name != result.test_name);
    l.finished.push(result.clone());
}

/// What the run has so far, as a suite result: every finished test, then
/// every planned test that didn't finish, skipped with `why`.
pub fn partial(why: &str) -> SuiteResult {
    let l = live();
    let ran = l.started.map(|s| s.elapsed()).unwrap_or_default();
    partial_from(&l.planned, &l.finished, why, ran)
}

/// [`partial`], from its parts.
pub fn partial_from(
    planned: &[String],
    finished: &[TestResult],
    why: &str,
    duration: Duration,
) -> SuiteResult {
    let mut tests: Vec<TestResult> = finished.to_vec();
    for name in planned {
        if !finished.iter().any(|r| &r.test_name == name) {
            tests.push(TestResult::skipped(name, &format!("not run: {why}")));
        }
    }
    let count = |s: TestStatus| tests.iter().filter(|t| t.status == s).count();
    SuiteResult {
        total: tests.len(),
        passed: count(TestStatus::Passed),
        failed: count(TestStatus::Failed),
        skipped: count(TestStatus::Skipped),
        retried: count(TestStatus::Retried),
        duration,
        tests,
        shard_index: None,
        shard_total: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_result_has_what_finished_and_the_rest_as_not_run() {
        let mut pass = TestResult::failed("login", String::new(), Duration::from_secs(2));
        pass.status = TestStatus::Passed;
        let fail = TestResult::failed("cart", "boom".into(), Duration::from_secs(1));
        let planned: Vec<String> = ["login", "cart", "pay", "logout"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = partial_from(
            &planned,
            &[pass, fail],
            "interrupted by SIGTERM (the run did not stop within 20s)",
            Duration::from_secs(25),
        );
        assert_eq!((r.total, r.passed, r.failed, r.skipped), (4, 1, 1, 2));
        let pay = r.tests.iter().find(|t| t.test_name == "pay").unwrap();
        assert_eq!(pay.status, TestStatus::Skipped);
        assert_eq!(
            pay.error_message.as_deref(),
            Some("not run: interrupted by SIGTERM (the run did not stop within 20s)")
        );
        assert_eq!(r.tests[0].test_name, "login");
        // A failure in it still fails the run.
        assert_eq!(r.exit_code(), 1);
    }
}
