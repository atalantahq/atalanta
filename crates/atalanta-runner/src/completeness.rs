// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! NEW-001: a run reports every test it planned, once.
//!
//! A worker that died (a panicked actor, a lost task) or a bookkeeping bug
//! could leave a planned test with no result, and the run would pass with
//! fewer tests than it set out to run. Before the totals are counted, each
//! planned test without a result gets a failed one, marked an internal
//! error (exit 5): a silent pass becomes a loud failure.

use std::collections::HashSet;
use std::time::Duration;

use atalanta_common::TestResult;

/// Add a failed, internal-error result for each `planned` test that has
/// none in `results`. Returns how many were missing.
pub fn account_for_every_test(planned: &[String], results: &mut Vec<TestResult>) -> usize {
    let have: HashSet<&str> = results.iter().map(|r| r.test_name.as_str()).collect();
    let missing: Vec<String> = planned
        .iter()
        .filter(|n| !have.contains(n.as_str()))
        .cloned()
        .collect();
    for name in &missing {
        let mut r = TestResult::failed(
            name,
            "no result: the test was planned but the run never reported it (a worker \
             stopped, or a Atalanta bug); this is not a pass"
                .into(),
            Duration::ZERO,
        );
        r.internal_error = true;
        results.push(r);
    }
    missing.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::TestStatus;

    #[test]
    fn a_planned_test_with_no_result_fails_as_an_internal_error() {
        let planned = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut results = vec![
            TestResult::failed("a", "x".into(), Duration::ZERO),
            TestResult::skipped("c", "not run: timeout"),
        ];
        assert_eq!(account_for_every_test(&planned, &mut results), 1);
        let b = results.iter().find(|r| r.test_name == "b").unwrap();
        assert_eq!(b.status, TestStatus::Failed);
        assert!(b.internal_error);
        assert!(b.error_message.as_deref().unwrap().contains("not a pass"));
        assert_eq!(account_for_every_test(&planned, &mut results), 0);
    }
}
