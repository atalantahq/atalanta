//! Copies of one test spread over the device pool: parallel flaky hunting
//! (`--hunt N --parallel M`) and preventive copies of tests history flags
//! as flaky (`--flaky-confidence P`).
//!
//! Both queue several copies of a test in the parallel runner. Copies of
//! one test go to different devices (and never two to one device at the
//! same time), so a failure that follows a device can be told from one
//! that follows the test.

use std::collections::BTreeMap;

use velocity_common::{TestResult, TestStatus};

/// Most copies of one test a run schedules, whatever the formula says: a
/// test that fails this often needs fixing, not more device time.
pub const MAX_COPIES: u32 = 10;

/// Why a queued item is a copy.
#[derive(Debug, Clone, PartialEq)]
pub enum CopyKind {
    /// Round `round` of a parallel hunt, with its own order and jitter seed.
    Hunt { round: u32, seed: u64 },
    /// Copy `index` (0-based) of `of` copies of a test history flags as
    /// flaky.
    Preventive { index: u32, of: u32 },
}

/// A copy, as the supervisor tracks it.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyTag {
    /// The copied test (or a dependency group's names, joined), so copies
    /// of one test can be kept off each other's devices.
    pub key: String,
    pub kind: CopyKind,
}

impl CopyTag {
    /// Trace file name part, so concurrent copies don't overwrite each
    /// other's trace.
    pub fn trace_tag(&self, device: &str) -> String {
        match self.kind {
            CopyKind::Hunt { round, .. } => format!("round{}-{device}", round + 1),
            CopyKind::Preventive { index, .. } => format!("copy{}-{device}", index + 1),
        }
    }
}

/// One finished copy.
#[derive(Debug, Clone)]
pub struct CopyOutcome {
    pub tag: CopyTag,
    pub device: String,
    pub result: TestResult,
}

/// Copies needed so that at least one passes with probability `target`,
/// for a test that fails a run with probability `p_fail`, the copies being
/// independent: the smallest n with p_failⁿ ≤ 1 − target, that is
/// n = ⌈ln(1 − target) / ln(p_fail)⌉.
///
/// Marathon's version squares the fail rate on every loop and schedules
/// too few copies. A test that never fails needs one run; one that always
/// fails is broken, and copies won't make it pass, so it also gets one.
pub fn copies_needed(p_fail: f64, target: f64) -> u32 {
    if !(0.0..1.0).contains(&target) || !(p_fail > 0.0 && p_fail < 1.0) {
        return 1;
    }
    // The epsilon keeps an exact ratio (ln 0.01 / ln 0.1 = 2.000…04) from
    // rounding up to one copy too many.
    let n = ((1.0 - target).ln() / p_fail.ln() - 1e-9).ceil();
    if n.is_finite() {
        (n as u32).clamp(1, MAX_COPIES)
    } else {
        MAX_COPIES
    }
}

/// Merge the copies of one test into the test's result: it passes if any
/// copy passed, and a pass after a failed copy is flaky (reported as
/// `Retried`, with each failed copy's error and device).
pub fn merge_preventive(mut copies: Vec<CopyOutcome>, p_fail: f64) -> Option<TestResult> {
    if copies.is_empty() {
        return None;
    }
    let n = copies.len();
    let (passed, failed): (Vec<CopyOutcome>, Vec<CopyOutcome>) = copies
        .drain(..)
        .partition(|c| matches!(c.result.status, TestStatus::Passed | TestStatus::Retried));
    let mut failed_on: Vec<String> = failed.iter().map(|c| c.device.clone()).collect();
    failed_on.sort();
    failed_on.dedup();
    let errors: Vec<String> = failed
        .iter()
        .map(|c| {
            format!(
                "[{}] {}",
                c.device,
                c.result.error_message.as_deref().unwrap_or("failed")
            )
        })
        .collect();
    let why = format!(
        "history flags it flaky (fails {:.0}% of runs): ran {n} cop{}",
        p_fail * 100.0,
        if n == 1 { "y" } else { "ies" }
    );
    let mut result = match (passed.into_iter().next(), failed.last()) {
        (Some(pass), None) => {
            let mut r = pass.result;
            r.warnings.insert(0, format!("{why}; all passed"));
            r
        }
        (Some(pass), Some(first_failed)) => {
            let mut r = pass.result;
            r.status = TestStatus::Retried;
            r.warnings.insert(
                0,
                format!(
                    "flaky: {} of {n} copies failed (on {}), passed on {}; {why}",
                    failed.len(),
                    failed_on.join(", "),
                    pass.device
                ),
            );
            if r.trace.is_none() {
                r.trace = first_failed.result.trace.clone();
            }
            r.failed_attempts = errors;
            r
        }
        (None, Some(last)) => {
            let mut r = last.result.clone();
            r.status = TestStatus::Failed;
            r.warnings.insert(
                0,
                format!("all {n} copies failed (on {}); {why}", failed_on.join(", ")),
            );
            r.failed_attempts = errors[..errors.len() - 1].to_vec();
            r
        }
        (None, None) => return None,
    };
    result.retries = result.failed_attempts.len() as u32;
    Some(result)
}

/// Pass/fail counts of one test on one device across hunt rounds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceTally {
    pub passed: u32,
    pub failed: u32,
}

/// Per-device tallies of one test's hunt copies, by device id.
pub fn tally_by_device<'a>(
    copies: impl IntoIterator<Item = &'a CopyOutcome>,
) -> BTreeMap<String, DeviceTally> {
    let mut out: BTreeMap<String, DeviceTally> = BTreeMap::new();
    for c in copies {
        let t = out.entry(c.device.clone()).or_default();
        match c.result.status {
            TestStatus::Passed | TestStatus::Retried => t.passed += 1,
            TestStatus::Failed if !c.result.infrastructure_failure => t.failed += 1,
            _ => {}
        }
    }
    out
}

/// "fails only on emulator-5556 (3/3), passes on emulator-5554 (3/3)" when
/// the failures follow devices: every device either always failed or
/// always passed, and both kinds ran. `None` when the test flaked on one
/// device (the test's own flakiness) or ran on a single device.
pub fn device_effect(by_device: &BTreeMap<String, DeviceTally>) -> Option<String> {
    let ran: Vec<(&String, &DeviceTally)> = by_device
        .iter()
        .filter(|(_, t)| t.passed + t.failed > 0)
        .collect();
    if ran.len() < 2 || ran.iter().any(|(_, t)| t.passed > 0 && t.failed > 0) {
        return None;
    }
    let fmt = |pick: fn(&DeviceTally) -> bool, count: fn(&DeviceTally) -> u32| -> Vec<String> {
        ran.iter()
            .filter(|(_, t)| pick(t))
            .map(|(d, t)| format!("{d} ({}/{})", count(t), t.passed + t.failed))
            .collect()
    };
    let failing = fmt(|t| t.failed > 0, |t| t.failed);
    let passing = fmt(|t| t.passed > 0, |t| t.passed);
    if failing.is_empty() || passing.is_empty() {
        return None;
    }
    Some(format!(
        "fails only on {}, passes on {}: a device effect, not the test",
        failing.join(", "),
        passing.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn copy(device: &str, status: TestStatus, err: Option<&str>) -> CopyOutcome {
        let mut result = TestResult::failed("pay", err.unwrap_or("").into(), Duration::ZERO);
        result.status = status;
        if err.is_none() {
            result.error_message = None;
        }
        CopyOutcome {
            tag: CopyTag {
                key: "pay".into(),
                kind: CopyKind::Preventive { index: 0, of: 3 },
            },
            device: device.into(),
            result,
        }
    }

    #[test]
    fn copies_needed_follows_the_independent_copies_formula() {
        // 0.3^4 = 0.0081 ≤ 0.01 < 0.3^3 = 0.027.
        assert_eq!(copies_needed(0.3, 0.99), 4);
        assert_eq!(copies_needed(0.5, 0.99), 7);
        assert_eq!(copies_needed(0.1, 0.95), 2);
        assert_eq!(copies_needed(0.05, 0.9), 1);
        assert_eq!(copies_needed(0.1, 0.99), 2, "0.1² = 0.01 exactly");
        for (p, target) in [(0.3, 0.99), (0.5, 0.999), (0.2, 0.9)] {
            let n = copies_needed(p, target) as i32;
            assert!(p.powi(n) <= 1.0 - target + 1e-12, "{p} {target} {n}");
            assert!(
                p.powi(n - 1) > 1.0 - target,
                "{p} {target} {n} is not the smallest"
            );
        }
    }

    #[test]
    fn copies_needed_has_sane_edges() {
        assert_eq!(copies_needed(0.0, 0.99), 1, "never fails");
        assert_eq!(copies_needed(1.0, 0.99), 1, "always fails: broken");
        assert_eq!(copies_needed(0.3, 1.0), 1, "target must be below 1");
        assert_eq!(copies_needed(0.9, 0.9999), MAX_COPIES, "capped");
    }

    #[test]
    fn a_pass_after_a_failed_copy_is_flaky_and_says_where_it_failed() {
        let merged = merge_preventive(
            vec![
                copy(
                    "emulator-5556",
                    TestStatus::Failed,
                    Some("Element not found"),
                ),
                copy("emulator-5554", TestStatus::Passed, None),
                copy("emulator-5558", TestStatus::Passed, None),
            ],
            0.3,
        )
        .unwrap();
        assert_eq!(merged.status, TestStatus::Retried);
        assert_eq!(
            merged.failed_attempts,
            vec!["[emulator-5556] Element not found".to_string()]
        );
        assert!(
            merged.warnings[0].starts_with(
                "flaky: 1 of 3 copies failed (on emulator-5556), passed on emulator-5554"
            ),
            "{:?}",
            merged.warnings
        );
    }

    #[test]
    fn all_copies_passing_is_a_pass_and_all_failing_a_failure() {
        let ok = merge_preventive(
            vec![
                copy("a", TestStatus::Passed, None),
                copy("b", TestStatus::Passed, None),
            ],
            0.2,
        )
        .unwrap();
        assert_eq!(ok.status, TestStatus::Passed);
        assert!(
            ok.warnings[0].contains("ran 2 copies; all passed"),
            "{:?}",
            ok.warnings
        );

        let bad = merge_preventive(
            vec![
                copy("a", TestStatus::Failed, Some("x")),
                copy("b", TestStatus::Failed, Some("y")),
            ],
            0.2,
        )
        .unwrap();
        assert_eq!(bad.status, TestStatus::Failed);
        assert_eq!(bad.error_message.as_deref(), Some("y"));
        assert_eq!(bad.failed_attempts, vec!["[a] x".to_string()]);
    }

    #[test]
    fn failures_that_follow_a_device_are_called_a_device_effect() {
        let hunt = |device: &str, status| CopyOutcome {
            tag: CopyTag {
                key: "t".into(),
                kind: CopyKind::Hunt { round: 0, seed: 1 },
            },
            ..copy(device, status, None)
        };
        let split = [
            hunt("a", TestStatus::Failed),
            hunt("a", TestStatus::Failed),
            hunt("b", TestStatus::Passed),
            hunt("b", TestStatus::Passed),
        ];
        let note = device_effect(&tally_by_device(&split)).unwrap();
        assert_eq!(
            note,
            "fails only on a (2/2), passes on b (2/2): a device effect, not the test"
        );
        let mixed = [
            hunt("a", TestStatus::Failed),
            hunt("a", TestStatus::Passed),
            hunt("b", TestStatus::Passed),
        ];
        assert_eq!(device_effect(&tally_by_device(&mixed)), None);
    }
}
