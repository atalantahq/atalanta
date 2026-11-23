// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta results check`: did every job of a sharded run report? (NEW-001)
//!
//! A shard whose job died before uploading, uploaded an empty folder or
//! wrote a damaged report can't say so itself: the process that writes the
//! reports is the one that failed. So a step after the matrix downloads
//! every shard's artifacts and checks them here. It fails (exit 1) when:
//!
//! - fewer (or more) result files than `--expect-shards` arrived;
//! - two files claim the same shard, or a shard number is missing;
//! - a `results.json` doesn't parse, or its `junit.xml` is missing or empty;
//! - a shard ran no test, or the run as a whole ran none;
//! - the shards did not compute the same partition: their `shard_plan`
//!   fingerprints differ (different checkouts, filters or timings files),
//!   so a test may be in nobody's shard, or in two; or, with one plan, a
//!   test is planned in two shards or the shards' plans don't add up to
//!   the whole plan;
//! - a file was written by a newer Atalanta (`format_version` above
//!   [`FORMAT_VERSION`]).

use crate::reporter::json::{ShardPlanRecord, FORMAT_VERSION};

use std::path::{Path, PathBuf};

/// What the check found.
#[derive(Debug, Default)]
pub struct Checked {
    /// Each results file read, with its shard (0-based) and test count.
    pub files: Vec<(PathBuf, Option<usize>, usize)>,
    /// What is wrong; empty when the results are complete.
    pub problems: Vec<String>,
    pub tests: usize,
    pub failed: usize,
}

/// Every `results.json` under `inputs` (files, or folders searched to any
/// depth), sorted.
pub fn find(inputs: &[String]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            // Symlinked folders are not followed (no loops).
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&p, out),
                Ok(t) if t.is_file() && p.file_name().is_some_and(|n| n == "results.json") => {
                    out.push(p)
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    for i in inputs {
        let p = PathBuf::from(i);
        if p.is_dir() {
            walk(&p, &mut out);
        } else if p.is_file() {
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Check `files` (each a shard's `results.json`) against `expect_shards`.
pub fn check(files: &[PathBuf], expect_shards: Option<usize>) -> Checked {
    let mut c = Checked::default();
    let mut seen: Vec<Option<usize>> = Vec::new();
    let mut plans: Vec<(String, Option<ShardPlanRecord>)> = Vec::new();
    for f in files {
        let parsed = std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|s| {
                let r = crate::reporter::json::parse_results(&s).map_err(|e| e.to_string())?;
                let doc: serde_json::Value = serde_json::from_str(&s).unwrap_or_default();
                let version = doc
                    .get("format_version")
                    .and_then(serde_json::Value::as_u64);
                let plan = doc
                    .get("shard_plan")
                    .cloned()
                    .and_then(|p| serde_json::from_value::<ShardPlanRecord>(p).ok());
                Ok((r, version, plan))
            });
        let (r, version, plan) = match parsed {
            Ok(r) => r,
            Err(e) => {
                c.problems
                    .push(format!("{}: not a Atalanta results file: {e}", f.display()));
                continue;
            }
        };
        let shard = r.shard_index;
        let label = match (shard, r.shard_total) {
            (Some(i), Some(n)) => format!("shard {}/{n} ({})", i + 1, f.display()),
            _ => f.display().to_string(),
        };
        if let (Some(n), Some(total)) = (expect_shards, r.shard_total) {
            if total != n {
                c.problems.push(format!(
                    "{label}: ran as one of {total} shards, expected {n}"
                ));
            }
        }
        if let Some(v) = version.filter(|v| *v > u64::from(FORMAT_VERSION)) {
            c.problems.push(format!(
                "{label}: results format version {v} is newer than this Atalanta reads \
                 ({FORMAT_VERSION}); run `results check` with the Atalanta that wrote it"
            ));
        }
        if r.tests.is_empty() {
            c.problems.push(format!("{label}: ran no tests"));
        }
        if r.shard_total.is_some() {
            plans.push((label.clone(), plan));
        }
        let junit = f.with_file_name("junit.xml");
        match std::fs::read_to_string(&junit) {
            Ok(x) if x.contains("<testsuite") => {}
            Ok(_) => c
                .problems
                .push(format!("{label}: {} has no test suite", junit.display())),
            Err(_) => c.problems.push(format!("{label}: no junit.xml next to it")),
        }
        c.tests += r.tests.len();
        c.failed += r.failed;
        seen.push(shard);
        c.files.push((f.clone(), shard, r.tests.len()));
    }
    if let Some(n) = expect_shards {
        if c.files.len() != n {
            c.problems.push(format!(
                "{} results file(s) for {n} expected shard(s): a job uploaded nothing, or its \
                 results are somewhere else",
                c.files.len()
            ));
        }
        let mut numbered: Vec<usize> = seen.iter().flatten().copied().collect();
        numbered.sort();
        for w in numbered.windows(2) {
            if w[0] == w[1] {
                c.problems
                    .push(format!("two results files are shard {}/{n}", w[0] + 1));
            }
        }
        if seen.iter().all(Option::is_some) {
            let missing: Vec<String> = (0..n)
                .filter(|i| !numbered.contains(i))
                .map(|i| format!("{}/{n}", i + 1))
                .collect();
            if !missing.is_empty() {
                c.problems
                    .push(format!("no results from shard {}", missing.join(", ")));
            }
        }
    }
    check_plans(&plans, &mut c.problems);
    if files.is_empty() {
        c.problems.push("no results.json found".into());
    } else if c.tests == 0 && c.problems.is_empty() {
        c.problems.push("the run ran no tests".into());
    }
    c
}

/// Did every shard compute the same partition? `plans` holds each sharded
/// file's label and its `shard_plan` (`None`: written before Atalanta
/// recorded it).
fn check_plans(plans: &[(String, Option<ShardPlanRecord>)], problems: &mut Vec<String>) {
    let with: Vec<(&String, &ShardPlanRecord)> = plans
        .iter()
        .filter_map(|(l, p)| p.as_ref().map(|p| (l, p)))
        .collect();
    if with.is_empty() {
        return; // all from an older Atalanta: nothing to compare
    }
    for (label, plan) in plans {
        if plan.is_none() {
            problems.push(format!(
                "{label}: no shard plan recorded (written by an older Atalanta?), so it can't \
                 be compared with the other shards; run every shard with the same Atalanta"
            ));
        }
    }
    let (first_label, first) = with[0];
    let mut agree = true;
    for (label, plan) in &with[1..] {
        if plan.fingerprint != first.fingerprint {
            agree = false;
            problems.push(format!(
                "{label} planned partition {} but {first_label} planned {}: the shards \
                 disagree on which test runs where (different checkouts, --tags/--filter or \
                 --shard-timings files), so a test may have run twice or not at all. \
                 Give every shard the same commit, flags and timings file",
                plan.fingerprint, first.fingerprint
            ));
        }
    }
    if !agree {
        return;
    }
    let mut owner: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (label, plan) in &with {
        for t in &plan.tests {
            if let Some(other) = owner.insert(t.as_str(), label.as_str()) {
                if other != label.as_str() {
                    problems.push(format!("test {t:?} is planned in both {other} and {label}"));
                }
            }
        }
    }
    // Every shard arrived (checked above): their plans must add up to the
    // whole plan, or a test belongs to no shard.
    if with.len() == plans.len() && owner.len() < first.planned_total {
        let n = with.len();
        problems.push(format!(
            "the shards' plans cover {} of {} planned tests: a shard's results are missing \
             or it ran a different plan ({n} file(s) checked)",
            owner.len(),
            first.planned_total
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard(dir: &Path, i: usize, n: usize, tests: usize, junit: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let t: Vec<String> = (0..tests)
            .map(|k| {
                format!(
                    r#"{{"test_name":"t{i}-{k}","status":"passed","duration":{{"secs":1,"nanos":0}},"steps":[],"retries":0,"error_message":null,"screenshots":[]}}"#
                )
            })
            .collect();
        let json = format!(
            r#"{{"total":{tests},"passed":{tests},"failed":0,"skipped":0,"retried":0,"duration":{{"secs":1,"nanos":0}},"tests":[{}],"shard_index":{i},"shard_total":{n}}}"#,
            t.join(",")
        );
        std::fs::write(dir.join("results.json"), json).unwrap();
        if junit {
            std::fs::write(
                dir.join("junit.xml"),
                "<testsuites><testsuite name=\"x\"/></testsuites>",
            )
            .unwrap();
        }
    }

    #[test]
    fn complete_shards_pass() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..3 {
            shard(&d.path().join(format!("shard-{i}")), i, 3, 2, true);
        }
        let files = find(&[d.path().to_string_lossy().into_owned()]);
        let c = check(&files, Some(3));
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert_eq!((c.files.len(), c.tests), (3, 6));
    }

    #[test]
    fn a_shard_that_uploaded_nothing_fails() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 3, 2, true);
        shard(&d.path().join("c"), 2, 3, 2, true);
        std::fs::create_dir_all(d.path().join("b")).unwrap();
        let c = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(3));
        let all = c.problems.join("\n");
        assert!(all.contains("2 results file(s) for 3 expected"), "{all}");
        assert!(all.contains("no results from shard 2/3"), "{all}");
    }

    #[test]
    fn empty_damaged_and_duplicate_shards_fail() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 2, 0, true);
        shard(&d.path().join("b"), 0, 2, 1, false);
        let c = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2));
        let all = c.problems.join("\n");
        assert!(all.contains("ran no tests"), "{all}");
        assert!(all.contains("no junit.xml"), "{all}");
        assert!(all.contains("two results files are shard 1/2"), "{all}");
        assert!(all.contains("no results from shard 2/2"), "{all}");

        std::fs::write(d.path().join("a/results.json"), "{ truncated").unwrap();
        let c = check(
            &find(&[d.path().join("a").to_string_lossy().into_owned()]),
            None,
        );
        assert!(
            c.problems[0].contains("not a Atalanta results file"),
            "{:?}",
            c.problems
        );
    }

    /// Add a `shard_plan` to a shard written by [`shard`].
    fn with_plan(dir: &Path, fingerprint: &str, tests: &[&str], total: usize) {
        let p = dir.join("results.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["shard_plan"] = serde_json::json!({
            "fingerprint": fingerprint, "tests": tests, "planned_total": total
        });
        doc["format_version"] = 1.into();
        std::fs::write(&p, doc.to_string()).unwrap();
    }

    #[test]
    fn shards_that_agree_on_the_plan_pass() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 2, 1, true);
        shard(&d.path().join("b"), 1, 2, 1, true);
        with_plan(&d.path().join("a"), "f1", &["login"], 2);
        with_plan(&d.path().join("b"), "f1", &["checkout"], 2);
        let c = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2));
        assert!(c.problems.is_empty(), "{:?}", c.problems);
    }

    /// Review 12 #3: two shards computed different partitions; every one
    /// is green and non-empty, but `checkout` ran nowhere.
    #[test]
    fn shards_with_different_plans_fail_naming_both_fingerprints() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 2, 1, true);
        shard(&d.path().join("b"), 1, 2, 1, true);
        with_plan(&d.path().join("a"), "aaaa", &["login"], 2);
        with_plan(&d.path().join("b"), "bbbb", &["login"], 2);
        let c = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2));
        let all = c.problems.join("\n");
        assert!(all.contains("aaaa") && all.contains("bbbb"), "{all}");
    }

    #[test]
    fn a_test_planned_twice_or_in_no_shard_fails() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 2, 1, true);
        shard(&d.path().join("b"), 1, 2, 1, true);
        with_plan(&d.path().join("a"), "f", &["login"], 3);
        with_plan(&d.path().join("b"), "f", &["login"], 3);
        let all = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2))
            .problems
            .join("\n");
        assert!(all.contains("planned in both"), "{all}");
        assert!(all.contains("cover 1 of 3"), "{all}");
    }

    #[test]
    fn a_shard_without_a_plan_among_shards_with_one_fails_and_a_newer_format_too() {
        let d = tempfile::tempdir().unwrap();
        shard(&d.path().join("a"), 0, 2, 1, true);
        shard(&d.path().join("b"), 1, 2, 1, true);
        with_plan(&d.path().join("a"), "f", &["login"], 1);
        let all = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2))
            .problems
            .join("\n");
        assert!(all.contains("no shard plan recorded"), "{all}");

        let p = d.path().join("a/results.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["format_version"] = 99.into();
        std::fs::write(&p, doc.to_string()).unwrap();
        let all = check(&find(&[d.path().to_string_lossy().into_owned()]), Some(2))
            .problems
            .join("\n");
        assert!(all.contains("format version 99"), "{all}");
    }

    #[test]
    fn nothing_found_fails() {
        let d = tempfile::tempdir().unwrap();
        let c = check(&find(&[d.path().to_string_lossy().into_owned()]), None);
        assert_eq!(c.problems, ["no results.json found"]);
    }
}
