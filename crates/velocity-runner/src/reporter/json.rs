use velocity_common::{Result, SuiteResult, VelocityError};

/// `"healed": n` and `"label": "PASSED (healed n)"` on each test whose
/// selectors were healed (D-7 / R4).
fn mark_healed(result: &SuiteResult, doc: &mut serde_json::Value) {
    let Some(tests) = doc.get_mut("tests").and_then(|t| t.as_array_mut()) else {
        return;
    };
    for (t, v) in result.tests.iter().zip(tests.iter_mut()) {
        let n = t.healed_count();
        if n == 0 {
            continue;
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("healed".into(), n.into());
            if let Some(label) = t.healed_label() {
                obj.insert("label".into(), label.into());
            }
        }
    }
}

/// Write a JSON report for the suite result to the given file path.
pub fn write_json(result: &SuiteResult, path: &str) -> Result<()> {
    write_json_with(result, path, None)
}

/// [`write_json`], with how the run ended (`completed`, `timeout`,
/// `interrupted:SIGTERM`) as a top-level `termination` field (D-20).
pub fn write_json_with(result: &SuiteResult, path: &str, termination: Option<&str>) -> Result<()> {
    write_json_full(result, path, termination, None)
}

/// [`write_json_with`], plus a flaky hunt's verdicts as a top-level `hunt`
/// field (`HuntReport::json`): rounds, and each flaky test's counts and
/// where its passing and failing rounds diverge.
pub fn write_json_full(
    result: &SuiteResult,
    path: &str,
    termination: Option<&str>,
    hunt: Option<&serde_json::Value>,
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct WithTermination<'a> {
        #[serde(flatten)]
        result: &'a SuiteResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        termination: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hunt: Option<&'a serde_json::Value>,
    }
    let wrapped = &WithTermination {
        result,
        termination,
        hunt,
    };
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            VelocityError::Config(format!("failed to create report directory: {e}"))
        })?;
    }

    let mut doc = serde_json::to_value(wrapped)
        .map_err(|e| VelocityError::Config(format!("failed to serialize suite result: {e}")))?;
    mark_healed(result, &mut doc);
    let json = serde_json::to_string_pretty(&doc)
        .map_err(|e| VelocityError::Config(format!("failed to serialize suite result: {e}")))?;

    velocity_common::fsutil::write_atomic(std::path::Path::new(path), json.as_bytes()).map_err(
        |e| VelocityError::Config(format!("failed to write JSON report at {path}: {e}")),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{TestResult, TestStatus};

    #[test]
    fn healed_tests_carry_their_count_and_label() {
        let mut t = TestResult::failed("a", String::new(), std::time::Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.warnings = vec!["selector text=\"A\" not found; healed to text=\"B\" (confidence 0.90) — update the selector".into()];
        let plain = TestResult {
            test_name: "b".into(),
            warnings: vec![],
            ..t.clone()
        };
        let result = SuiteResult {
            total: 2,
            passed: 2,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![t, plain],
            shard_index: None,
            shard_total: None,
        };
        let path =
            std::env::temp_dir().join(format!("vel-json-healed-{}.json", std::process::id()));
        write_json(&result, path.to_str().unwrap()).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc["tests"][0]["healed"], 1);
        assert_eq!(doc["tests"][0]["label"], "PASSED (healed 1)");
        assert!(doc["tests"][1].get("healed").is_none());
    }

    #[test]
    fn ai_checks_are_in_the_json_report() {
        let mut t = TestResult::failed("a", "x".into(), std::time::Duration::ZERO);
        t.verification = vec![velocity_common::VerifyCheck {
            at: "step 2".into(),
            claim: "the cart is open".into(),
            outcome: velocity_common::VerifyOutcome::Fails,
            reason: "empty cart".into(),
            evidence: vec!["[1] <text> text=\"Cart (0)\" @1,1".into()],
        }];
        let result = SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let path =
            std::env::temp_dir().join(format!("vel-json-verify-{}.json", std::process::id()));
        write_json(&result, path.to_str().unwrap()).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        let c = &doc["tests"][0]["verification"][0];
        assert_eq!(c["at"], "step 2");
        assert_eq!(c["outcome"], "fails");
        assert_eq!(c["evidence"][0], "[1] <text> text=\"Cart (0)\" @1,1");
    }

    #[test]
    fn a_failed_test_is_not_marked_healed() {
        // Review m5: `"healed": 1` on a failed test (report mode's
        // proposed heal, or a heal in a test that failed later).
        let mut t = TestResult::failed("a", "boom".into(), std::time::Duration::ZERO);
        t.warnings = vec!["selector text=\"A\" not found; healed to text=\"B\" (confidence 0.90) — update the selector".into()];
        let result = SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let path =
            std::env::temp_dir().join(format!("vel-json-unhealed-{}.json", std::process::id()));
        write_json(&result, path.to_str().unwrap()).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(doc["tests"][0].get("healed").is_none(), "{doc}");
    }
}
