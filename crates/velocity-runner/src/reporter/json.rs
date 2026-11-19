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

/// The `results.json` format version, the top-level `format_version`.
/// Bumped when a field changes meaning or goes away; new fields are
/// added without a bump (readers must ignore fields they don't know).
/// Files written before this field existed have no `format_version`:
/// read them as version 1.
///
/// Version 2 (still readable as version 1):
/// - every `duration` (suite, test, step) has an integer `duration_ms`
///   beside it; the `{"secs", "nanos"}` object stays for version-1 readers
///   and is deprecated;
/// - the resource keys (`resource_peak`, `resource_delta`) are snake_case
///   like the rest of the document (`java_heap_kb`, was `javaHeapKb`).
///
/// [`parse_results`] reads both versions.
pub const FORMAT_VERSION: u32 = 2;

/// Read a `results.json` of any format version into a [`SuiteResult`]:
/// version 1 (`{"secs","nanos"}` durations, camelCase resource keys) and
/// version 2 (`duration_ms`, snake_case). A document with only
/// `duration_ms` (no `duration` object) reads too.
pub fn parse_results(text: &str) -> serde_json::Result<SuiteResult> {
    let mut doc: serde_json::Value = serde_json::from_str(text)?;
    fill_durations(&mut doc);
    serde_json::from_value(doc)
}

/// `duration` from `duration_ms` where only the latter is present.
fn fill_durations(doc: &mut serde_json::Value) {
    fn fill(v: &mut serde_json::Value) {
        let Some(obj) = v.as_object_mut() else { return };
        if !obj.contains_key("duration") {
            if let Some(ms) = obj.get("duration_ms").and_then(|m| m.as_u64()) {
                obj.insert(
                    "duration".into(),
                    serde_json::json!({ "secs": ms / 1000, "nanos": (ms % 1000) * 1_000_000 }),
                );
            }
        }
    }
    fill(doc);
    for t in each_mut(doc, "tests") {
        fill(t);
        for st in each_mut(t, "steps") {
            fill(st);
        }
    }
}

fn each_mut<'a>(
    v: &'a mut serde_json::Value,
    key: &str,
) -> impl Iterator<Item = &'a mut serde_json::Value> {
    v.get_mut(key)
        .and_then(|a| a.as_array_mut())
        .into_iter()
        .flatten()
}

/// Version 2's additions to the serialized document: `duration_ms`
/// beside each `duration`, snake_case resource keys.
fn to_format_2(doc: &mut serde_json::Value) {
    fn add_ms(v: &mut serde_json::Value) {
        let Some(obj) = v.as_object_mut() else { return };
        let ms = obj.get("duration").and_then(|d| {
            let secs = d.get("secs")?.as_u64()?;
            let nanos = d.get("nanos")?.as_u64()?;
            Some(secs * 1000 + nanos / 1_000_000)
        });
        if let Some(ms) = ms {
            obj.insert("duration_ms".into(), ms.into());
        }
    }
    fn snake_keys(v: &mut serde_json::Value) {
        let Some(obj) = v.as_object_mut() else { return };
        let old = std::mem::take(obj);
        for (k, mut val) in old {
            snake_keys(&mut val);
            obj.insert(snake(&k), val);
        }
    }
    fn snake(k: &str) -> String {
        let mut out = String::with_capacity(k.len() + 4);
        for c in k.chars() {
            if c.is_ascii_uppercase() {
                out.push('_');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }
    add_ms(doc);
    for t in each_mut(doc, "tests") {
        add_ms(t);
        if let Some(p) = t.get_mut("resource_peak") {
            snake_keys(p);
        }
        for st in each_mut(t, "steps") {
            add_ms(st);
            if let Some(d) = st.get_mut("resource_delta") {
                snake_keys(d);
            }
        }
    }
}

/// The sharded run's partition, the top-level `shard_plan`: what
/// `velocity results check` compares across shards.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShardPlanRecord {
    /// [`crate::scheduler::ShardPlan::fingerprint`]: equal on every shard
    /// that computed the same partition.
    pub fingerprint: String,
    /// The tests this shard was assigned, in plan order.
    pub tests: Vec<String>,
    /// How many tests the whole plan has, across every shard.
    pub planned_total: usize,
}

/// Top-level fields besides the results.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonExtras<'a> {
    /// How the run ended (`completed`, `timeout`, `interrupted:SIGTERM`).
    pub termination: Option<&'a str>,
    /// A flaky hunt's verdicts (`HuntReport::json`).
    pub hunt: Option<&'a serde_json::Value>,
    /// The partition, on a sharded run.
    pub shard_plan: Option<&'a ShardPlanRecord>,
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
    write_json_report(
        result,
        path,
        &JsonExtras {
            termination,
            hunt,
            shard_plan: None,
        },
    )
}

/// Write `results.json`: the results, `format_version`, and `extras`.
pub fn write_json_report(result: &SuiteResult, path: &str, extras: &JsonExtras) -> Result<()> {
    #[derive(serde::Serialize)]
    struct WithTermination<'a> {
        format_version: u32,
        #[serde(flatten)]
        result: &'a SuiteResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        termination: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hunt: Option<&'a serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        shard_plan: Option<&'a ShardPlanRecord>,
    }
    let wrapped = &WithTermination {
        format_version: FORMAT_VERSION,
        result,
        termination: extras.termination,
        hunt: extras.hunt,
        shard_plan: extras.shard_plan,
    };
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| VelocityError::io("failed to create report directory", e))?;
    }

    let mut doc = serde_json::to_value(wrapped)
        .map_err(|e| VelocityError::io("failed to serialize suite result", e))?;
    mark_healed(result, &mut doc);
    to_format_2(&mut doc);
    let json = serde_json::to_string_pretty(&doc)
        .map_err(|e| VelocityError::io("failed to serialize suite result", e))?;

    velocity_common::fsutil::write_atomic(std::path::Path::new(path), json.as_bytes())
        .map_err(|e| VelocityError::io(format!("failed to write JSON report at {path}"), e))?;

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
        assert_eq!(doc["format_version"], FORMAT_VERSION);
        assert!(doc.get("shard_plan").is_none(), "only on a sharded run");
        assert_eq!(doc["tests"][0]["healed"], 1);
        assert_eq!(doc["tests"][0]["label"], "PASSED (healed 1)");
        assert!(doc["tests"][1].get("healed").is_none());
    }

    #[test]
    fn format_2_has_integer_ms_and_snake_case_and_both_versions_read() {
        let mut t = TestResult::failed("a", "x".into(), std::time::Duration::from_millis(1_250));
        t.resource_peak = Some(velocity_common::ResourceSnapshot {
            java_heap_kb: 1,
            native_heap_kb: 2,
            total_pss_kb: 3,
            cpu_percent: 0.5,
            timestamp_ms: 4,
        });
        let result = SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::from_millis(2_340),
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let path = std::env::temp_dir().join(format!("vel-json-v2-{}.json", std::process::id()));
        write_json(&result, path.to_str().unwrap()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["format_version"], 2);
        assert_eq!(doc["duration_ms"], 2_340);
        assert_eq!(doc["tests"][0]["duration_ms"], 1_250);
        // Version-1 readers still find the object form.
        assert_eq!(doc["duration"]["secs"], 2);
        assert_eq!(doc["tests"][0]["resource_peak"]["total_pss_kb"], 3);
        assert!(doc["tests"][0]["resource_peak"].get("totalPssKb").is_none());

        let back = parse_results(&text).unwrap();
        assert_eq!(back.duration, std::time::Duration::from_millis(2_340));
        assert_eq!(
            back.tests[0].resource_peak.as_ref().unwrap().total_pss_kb,
            3
        );

        // Version 1, as 0.1.0 wrote it.
        let v1 = r#"{"total":1,"passed":1,"failed":0,"skipped":0,"retried":0,
            "duration":{"secs":1,"nanos":500000000},
            "tests":[{"test_name":"t","status":"passed","duration":{"secs":1,"nanos":0},
              "steps":[],"retries":0,"error_message":null,"screenshots":[],
              "resource_peak":{"javaHeapKb":1,"nativeHeapKb":2,"totalPssKb":3,
                "cpuPercent":0.0,"timestampMs":4}}],
            "shard_index":null,"shard_total":null}"#;
        let r = parse_results(v1).unwrap();
        assert_eq!(r.duration, std::time::Duration::from_millis(1_500));
        assert_eq!(r.tests[0].resource_peak.as_ref().unwrap().total_pss_kb, 3);

        // Only `duration_ms` (a later format that drops the object).
        let ms_only = r#"{"total":0,"passed":0,"failed":0,"skipped":0,"retried":0,
            "duration_ms":1500,"tests":[],"shard_index":null,"shard_total":null}"#;
        assert_eq!(
            parse_results(ms_only).unwrap().duration,
            std::time::Duration::from_millis(1_500)
        );
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

    #[test]
    fn the_shard_plan_is_recorded_and_the_results_still_read_back() {
        let result = SuiteResult {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![],
            shard_index: Some(1),
            shard_total: Some(2),
        };
        let plan = ShardPlanRecord {
            fingerprint: "00ff".into(),
            tests: vec!["login".into()],
            planned_total: 3,
        };
        let path = std::env::temp_dir().join(format!("vel-json-plan-{}.json", std::process::id()));
        write_json_report(
            &result,
            path.to_str().unwrap(),
            &JsonExtras {
                shard_plan: Some(&plan),
                ..Default::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["shard_plan"]["fingerprint"], "00ff");
        assert_eq!(doc["shard_plan"]["planned_total"], 3);
        // Existing readers parse it as before.
        let back: SuiteResult = serde_json::from_str(&text).unwrap();
        assert_eq!(back.shard_index, Some(1));
    }
}
