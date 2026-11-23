use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Resource metrics captured at a point in time.
///
/// `results.json` format 2 writes these keys in snake_case like the rest of
/// the document; the aliases read both.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    #[serde(alias = "java_heap_kb")]
    pub java_heap_kb: u64,
    #[serde(alias = "native_heap_kb")]
    pub native_heap_kb: u64,
    #[serde(alias = "total_pss_kb")]
    pub total_pss_kb: u64,
    #[serde(alias = "cpu_percent")]
    pub cpu_percent: f32,
    #[serde(alias = "timestamp_ms")]
    pub timestamp_ms: u64,
}

/// Resource delta comparing before/after metrics for a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDelta {
    pub before: ResourceSnapshot,
    pub after: ResourceSnapshot,
    #[serde(alias = "heap_growth_kb")]
    pub heap_growth_kb: i64,
}

/// The result of executing a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_index: usize,
    pub action_name: String,
    pub status: StepStatus,
    pub duration: Duration,
    pub screenshot: Option<PathBuf>,
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_delta: Option<ResourceDelta>,
    /// Time this step spent waiting for the screen to settle, in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u64>,
    /// The step's line in the suite YAML (a flow step's line when inlined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourcePos>,
}

/// Status of a single step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped,
}

/// The result of executing a single test case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub test_name: String,
    pub status: TestStatus,
    pub duration: Duration,
    pub steps: Vec<StepResult>,
    pub retries: u32,
    pub error_message: Option<String>,
    pub screenshots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_peak: Option<ResourceSnapshot>,
    /// Things that passed but need a look: healed selectors, re-planned
    /// natural-language steps. A heal is never a silent pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The failure came from the device or its connection, not the app.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub infrastructure_failure: bool,
    /// Error of each attempt that failed before the final one (retries).
    /// JUnit reports them as `flakyFailure` / `rerunFailure`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_attempts: Vec<String>,
    /// The test's `- name:` line in the suite YAML. Filled in after the run
    /// from the parsed suite (see `velocity_core::parser::stamp_sources`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourcePos>,
    /// The kept trace of this test (`traces/<test>/steps*.json`): its
    /// failed attempt, or the run itself with `--trace always`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<PathBuf>,
    /// The device the (final attempt of the) test ran on. History keys
    /// outcomes by its model and OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// A known failure the quarantine list forgives, with its reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined: Option<String>,
    /// Not run: reported from the result cache's earlier pass.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_cache: bool,
    /// The test's tags, for reports (Allure `tag` labels, JSON).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Velocity itself failed (a panic in a test, or the worker running it
    /// died): a bug in Velocity, not in the app or the device. The run
    /// exits 5, outranking a test failure (D-19), and the quarantine list
    /// never forgives it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal_error: bool,
    /// The AI checks `verify:` ran in this test, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<VerifyCheck>,
    /// Evidence kept for the test: video, logs, crash reports, `trace.html`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::evidence::Attachment>,
}

/// One AI check of a claim about the screen (`verify:`).
///
/// The model only cites elements; code decides. A "holds" that cites no
/// element on the screen is [`VerifyOutcome::Unknown`], never a pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyCheck {
    /// `final`, or `step N` (0-based, as in the step results).
    pub at: String,
    /// The claim checked: the test's description or name, an `assert:`
    /// step's intent, or a `checkpoint:` claim.
    pub claim: String,
    pub outcome: VerifyOutcome,
    /// Why, in the checker's words (or why code overruled it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The cited elements that exist on the screen, as listed to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

/// What a [`VerifyCheck`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyOutcome {
    /// The claim holds, backed by at least one element on screen.
    Holds,
    /// The checker says the claim does not hold.
    Fails,
    /// No usable verdict: no real element cited, or no answer.
    Unknown,
    /// The checker could not be asked (the model API failed).
    Error,
}

impl VerifyOutcome {
    pub fn passed(self) -> bool {
        self == VerifyOutcome::Holds
    }
}

impl std::fmt::Display for VerifyOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Holds => "holds",
            Self::Fails => "fails",
            Self::Unknown => "unknown",
            Self::Error => "error",
        })
    }
}

impl VerifyCheck {
    /// `verify final: fails — the cart is empty ("Cart (2)" expected)`.
    pub fn summary(&self) -> String {
        let mut s = format!("verify {} {:?}: {}", self.at, self.claim, self.outcome);
        if !self.reason.is_empty() {
            s.push_str(" — ");
            s.push_str(&self.reason);
        }
        s
    }
}

/// What every heal note says (`selector text="Log in" not found; healed
/// to text="Sign in" (confidence 0.92) — update the selector`).
pub const HEAL_NOTE_MARK: &str = " not found; healed to ";

/// Whether a test warning is a heal note.
pub fn is_heal_note(warning: &str) -> bool {
    warning.starts_with("selector ") && warning.contains(HEAL_NOTE_MARK)
}

impl TestResult {
    /// How many selectors were healed in this test (its heal notes): each
    /// (selector, heal) pair once, however many attempts healed it, and
    /// only for a test that passed (on a retry too). A failed test used
    /// no heal to pass (review m5).
    pub fn healed_count(&self) -> usize {
        if !matches!(self.status, TestStatus::Passed | TestStatus::Retried) {
            return 0;
        }
        let mut pairs: Vec<&str> = self
            .warnings
            .iter()
            .filter(|w| is_heal_note(w))
            // `selector A not found; healed to B` without the confidence,
            // which can differ between attempts.
            .map(|w| w.split(" (confidence").next().unwrap_or(w))
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        pairs.len()
    }

    /// `PASSED (healed 2)` for a passing test whose selectors were healed
    /// (D-7, R4), `FLAKY (passed on retry 1, healed 2)` for one that passed
    /// on a retry: every reporter shows it. `None` otherwise.
    pub fn healed_label(&self) -> Option<String> {
        let n = self.healed_count();
        if n == 0 {
            return None;
        }
        match self.status {
            TestStatus::Passed => Some(format!("PASSED (healed {n})")),
            TestStatus::Retried => Some(format!(
                "FLAKY (passed on retry {}, healed {n})",
                self.retries
            )),
            _ => None,
        }
    }

    /// A test that failed before producing step results.
    pub fn failed(name: &str, error: String, duration: Duration) -> Self {
        Self {
            test_name: name.to_string(),
            status: TestStatus::Failed,
            duration,
            steps: vec![],
            retries: 0,
            error_message: Some(error),
            screenshots: vec![],
            resource_peak: None,
            warnings: vec![],
            infrastructure_failure: false,
            failed_attempts: vec![],
            source: None,
            trace: None,
            device: None,
            quarantined: None,
            from_cache: false,
            tags: vec![],
            internal_error: false,
            verification: vec![],
            attachments: Vec::new(),
        }
    }

    /// A test that was not run.
    pub fn skipped(name: &str, reason: &str) -> Self {
        Self {
            status: TestStatus::Skipped,
            error_message: Some(reason.to_string()),
            ..Self::failed(name, String::new(), Duration::ZERO)
        }
    }
}

/// Status of a test case.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    Retried,
}

/// The result of executing the entire test suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub retried: usize,
    pub duration: Duration,
    pub tests: Vec<TestResult>,
    pub shard_index: Option<usize>,
    pub shard_total: Option<usize>,
}

impl SuiteResult {
    /// 0 all passed; 5 when Velocity itself failed on some test (a panic,
    /// D-19), whatever else failed; 3 when every failure was the device's
    /// (unreachable, locked, unresponsive), so CI can tell "fix the farm"
    /// from "fix the app"; else 1. A locked device used to exit 1, like a
    /// real bug.
    pub fn exit_code(&self) -> i32 {
        self.exit_code_with(false)
    }

    /// [`exit_code`](Self::exit_code), where `fail_on_flaky` (D-21:
    /// `--fail-on-flaky`, `retry.flakyResult: fail`) makes a test that
    /// passed only on retry count as a failure (exit 1). Reports still
    /// show it as flaky.
    pub fn exit_code_with(&self, fail_on_flaky: bool) -> i32 {
        let failed: Vec<&TestResult> = self
            .tests
            .iter()
            .filter(|t| t.status == TestStatus::Failed)
            .collect();
        if failed.iter().any(|t| t.internal_error) {
            return 5;
        }
        let flaky = fail_on_flaky && self.tests.iter().any(|t| t.status == TestStatus::Retried);
        if self.failed == 0 && failed.is_empty() {
            if flaky {
                1
            } else {
                0
            }
        } else if !flaky && !failed.is_empty() && failed.iter().all(|t| t.infrastructure_failure) {
            3
        } else {
            1
        }
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::*;

    fn suite(tests: Vec<TestResult>) -> SuiteResult {
        let failed = tests
            .iter()
            .filter(|t| t.status == TestStatus::Failed)
            .count();
        SuiteResult {
            total: tests.len(),
            passed: tests.len() - failed,
            failed,
            skipped: 0,
            retried: 0,
            duration: Duration::ZERO,
            tests,
            shard_index: None,
            shard_total: None,
        }
    }

    fn passed(name: &str) -> TestResult {
        TestResult {
            status: TestStatus::Passed,
            ..TestResult::failed(name, String::new(), Duration::ZERO)
        }
    }

    fn device_failure(name: &str) -> TestResult {
        TestResult {
            infrastructure_failure: true,
            ..TestResult::failed(name, "locked".into(), Duration::ZERO)
        }
    }

    #[test]
    fn device_only_failures_exit_3_and_any_test_failure_exits_1() {
        assert_eq!(suite(vec![passed("a")]).exit_code(), 0);
        assert_eq!(
            suite(vec![device_failure("a"), device_failure("b")]).exit_code(),
            3
        );
        assert_eq!(suite(vec![passed("a"), device_failure("b")]).exit_code(), 3);
        let bug = TestResult::failed("c", "not found".into(), Duration::ZERO);
        assert_eq!(suite(vec![device_failure("a"), bug]).exit_code(), 1);
    }

    // D-19: a panic is an internal error, exit 5, and outranks a real
    // test failure (1) and a device failure (3) in the same run.
    #[test]
    fn an_internal_error_exits_5_and_outranks_a_test_failure() {
        let panicked = TestResult {
            internal_error: true,
            ..TestResult::failed("p", "Velocity panicked".into(), Duration::ZERO)
        };
        let bug = TestResult::failed("c", "not found".into(), Duration::ZERO);
        assert_eq!(suite(vec![panicked.clone()]).exit_code(), 5);
        assert_eq!(suite(vec![bug, panicked.clone()]).exit_code(), 5);
        assert_eq!(suite(vec![device_failure("a"), panicked]).exit_code(), 5);
    }

    // D-21: a flaky pass exits 1 only when asked to.
    #[test]
    fn a_flaky_pass_fails_the_run_only_with_fail_on_flaky() {
        let flaky = TestResult {
            status: TestStatus::Retried,
            ..TestResult::failed("f", String::new(), Duration::ZERO)
        };
        let s = suite(vec![passed("a"), flaky]);
        assert_eq!(s.exit_code(), 0);
        assert_eq!(s.exit_code_with(true), 1);
        assert_eq!(suite(vec![passed("a")]).exit_code_with(true), 0);
    }
}

#[cfg(test)]
mod healed_tests {
    use super::*;

    #[test]
    fn heal_notes_are_counted_and_labelled_on_passes_only() {
        let mut t = TestResult::failed("a", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        assert_eq!(t.healed_label(), None);
        t.warnings = vec![
            "selector text=\"Log in\" not found; healed to text=\"Sign in\" (confidence 0.92) — update the selector".into(),
            "target text=\"Pay\" was disabled; acted on it anyway".into(),
            "selector id=\"x\" not found; healed to id=\"y\" (confidence 0.90) — update the selector".into(),
        ];
        assert_eq!(t.healed_count(), 2);
        assert_eq!(t.healed_label().as_deref(), Some("PASSED (healed 2)"));
        t.status = TestStatus::Failed;
        assert_eq!(t.healed_label(), None);
    }

    #[test]
    fn a_heal_counts_once_across_retries_on_retried_passes_and_never_on_failures() {
        // Review m5: retry.rs appends every attempt's warnings.
        let note = |c: &str| {
            format!(
                "selector text=\"Log in\" not found; healed to text=\"Sign in\" \
                 (confidence {c}) — update the selector"
            )
        };
        let mut t = TestResult::failed("a", String::new(), Duration::ZERO);
        t.warnings = vec![note("0.92"), "other".into(), note("0.91")];
        assert_eq!(t.healed_count(), 0, "a failed test used no heal to pass");
        t.status = TestStatus::Passed;
        assert_eq!(t.healed_count(), 1, "the same heal on two attempts");
        t.status = TestStatus::Retried;
        t.retries = 1;
        assert_eq!(
            t.healed_label().as_deref(),
            Some("FLAKY (passed on retry 1, healed 1)")
        );
    }
}
