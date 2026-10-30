//! History shared across CI jobs without a server (C1).
//!
//! Each CI job (a shard, a device in a matrix) writes its own
//! `velocity-history.json`. The jobs upload theirs as artifacts; a later
//! job (or the next run) downloads them and either merges them into one
//! file with `velocity history merge`, or reads them directly with
//! `velocity run --history-from 'history/*'`.
//!
//! **Merge rule, per test.** Every job usually starts from the same base
//! (the previous merged history), so the files share most of their
//! content, and concatenating outcome lists would count the base once per
//! job. Instead, per test, the file that ran it most recently (its
//! `last_run`) supplies the test's duration, outcomes, resource baseline
//! and run time: a shard's file for the tests that shard ran. Outcomes
//! per device profile (`byDevice`) are taken profile by profile, from the
//! file in which that profile ran the test most recently
//! (`byDeviceLastRun`), not the file that ran the test most recently: in
//! a device matrix every job's file also holds the base copy of the other
//! devices' profiles, and only the job that ran a device has its new
//! outcomes. A file with a profile time for some profiles of a test but
//! not this one ranks below every file with one, then by its test-wide
//! `last_run`. A file with no profile times for the test at all (written
//! by a Velocity without `byDeviceLastRun`) uses its test-wide `last_run`
//! for each profile. On a tie the later file given wins, so the result
//! never depends on anything but the arguments' order.
//!
//! **Mixed versions.** A job on an older Velocity rewrites its file
//! without `byDeviceLastRun`, so the merge can't tell the profile it ran
//! from its base copies of the others: they all rank by that job's
//! test-wide run time. A device that a newer job ran *earlier* than the
//! old job's run can lose its new outcomes to the old job's base copy.
//! Run one Velocity version across a matrix (see `docs/CI.md`).
//!
//! **Clocks.** Run times come from each job's machine. A time later than
//! the merge's own clock (a runner whose clock is fast) is unknown: it
//! ranks below every real time, so one such runner can't win a test's
//! outcomes in any merge; the value written out is clamped to the merge
//! time.

use std::path::{Path, PathBuf};

use velocity_common::{Result, VelocityError};

use crate::history::{self, TestHistory};

/// Merge `histories` (see the module docs for the rule). An empty slice
/// gives an empty history.
pub fn merge(histories: &[TestHistory]) -> TestHistory {
    merge_at(histories, history::now_secs())
}

/// [`merge`], with `now` (unix seconds) as the merge time that run times
/// in the future are clamped to.
fn merge_at(histories: &[TestHistory], now: u64) -> TestHistory {
    let mut out = TestHistory {
        version: histories.iter().map(|h| h.version).max().unwrap_or(0),
        ..Default::default()
    };
    let mut names: Vec<&String> = histories
        .iter()
        .flat_map(|h| {
            h.durations
                .keys()
                .chain(h.outcomes.keys())
                .chain(h.last_run.keys())
                .chain(h.resource_baselines.keys())
                .chain(h.by_device.keys())
        })
        .collect();
    names.sort();
    names.dedup();
    for name in names {
        // When file `i` last ran this test, as written.
        let at = |i: usize| histories[i].last_run.get(name).copied();
        // ... clamped to the merge time: the value written out.
        let ran = |i: usize| at(i).map(|t| t.min(now));
        // The most recent real run of this test wins. A time after the
        // merge's own clock (a runner whose clock is fast) is unknown: it
        // ranks below every real time, so it can't win against the runs
        // of the same batch (review-fi2 F4); files without a run time rank
        // below both, by position among themselves.
        let rank = |i: usize| (rank_time(at(i), now), i);
        let holding = |has: &dyn Fn(&TestHistory) -> bool| {
            (0..histories.len())
                .filter(|&i| has(&histories[i]))
                .max_by_key(|&i| rank(i))
        };
        if let Some(i) = holding(&|h| h.durations.contains_key(name)) {
            out.durations
                .insert(name.clone(), histories[i].durations[name]);
        }
        if let Some(i) = holding(&|h| h.outcomes.contains_key(name)) {
            out.outcomes
                .insert(name.clone(), histories[i].outcomes[name].clone());
            // From the same file: the kinds line up with its outcomes.
            if let Some(kinds) = histories[i].failure_kinds.get(name) {
                out.failure_kinds.insert(name.clone(), kinds.clone());
            }
        }
        if let Some(i) = holding(&|h| h.resource_baselines.contains_key(name)) {
            out.resource_baselines
                .insert(name.clone(), histories[i].resource_baselines[name].clone());
        }
        if let Some(at) = (0..histories.len()).filter_map(ran).max() {
            out.last_run.insert(name.clone(), at);
        }
        let mut profiles: Vec<&String> = histories
            .iter()
            .filter_map(|h| h.by_device.get(name))
            .flat_map(|m| m.keys())
            .collect();
        profiles.sort();
        profiles.dedup();
        for profile in profiles {
            // When file `i` ran this test on this profile, as written. A
            // file with no profile times for the test at all was written
            // by a Velocity without `byDeviceLastRun` (an older binary in
            // a mixed-version matrix rewrites the file without it): its
            // test-wide run time stands in, so the profile it ran isn't
            // lost to another job's base copy (review-fi2 F5).
            let on = |i: usize| match histories[i].by_device_last_run.get(name) {
                Some(m) => m.get(profile).copied(),
                None => at(i),
            };
            let best = (0..histories.len())
                .filter(|&i| {
                    histories[i]
                        .by_device
                        .get(name)
                        .is_some_and(|m| m.contains_key(profile))
                })
                .max_by_key(|&i| (rank_time(on(i), now), rank_time(at(i), now), i));
            if let Some(i) = best {
                out.by_device.entry(name.clone()).or_default().insert(
                    profile.clone(),
                    histories[i].by_device[name][profile].clone(),
                );
                if let Some(t) = on(i) {
                    out.by_device_last_run
                        .entry(name.clone())
                        .or_default()
                        .insert(profile.clone(), t.min(now));
                }
            }
        }
    }
    history::migrate(&mut out);
    out
}

/// How a run time ranks against `now` (the merge's clock): a real time
/// by its value, above a time in the future (unknown: a fast clock),
/// above no time.
fn rank_time(t: Option<u64>, now: u64) -> (u8, u64) {
    match t {
        Some(t) if t <= now => (2, t),
        Some(_) => (1, 0),
        None => (0, 0),
    }
}

/// The files `pattern` names, sorted: a path as is, or a glob where a
/// path component holds `*` (any run of characters, not `/`) and `**`
/// stands for any number of folders. A folder means its
/// `velocity-history.json`. Matching nothing gives an empty list.
pub fn expand(pattern: &str) -> Vec<PathBuf> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let mut found = if !pattern.contains('*') {
        let p = PathBuf::from(pattern);
        if p.exists() {
            vec![p]
        } else {
            Vec::new()
        }
    } else {
        let (root, rest) = if let Some(rest) = pattern.strip_prefix('/') {
            (PathBuf::from("/"), rest)
        } else {
            (PathBuf::from("."), pattern)
        };
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let mut out = Vec::new();
        walk(&root, &parts, &mut out);
        out
    };
    found.retain(|p| !p.is_dir() || p.join(history::HISTORY_FILENAME).is_file());
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, parts: &[&str], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = parts.split_first() else {
        out.push(dir.strip_prefix(".").unwrap_or(dir).to_path_buf());
        return;
    };
    if *first == "**" {
        walk(dir, rest, out);
        for sub in children(dir).into_iter().filter(|p| p.is_dir()) {
            walk(&sub, parts, out);
        }
    } else if first.contains('*') {
        for child in children(dir) {
            let name = child.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.is_some_and(|n| velocity_common::glob_match(first, &n)) {
                walk(&child, rest, out);
            }
        }
    } else {
        let next = dir.join(first);
        if next.exists() {
            walk(&next, rest, out);
        }
    }
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Loaded history files with their paths, and the patterns that matched
/// nothing.
pub type Loaded = (Vec<(PathBuf, TestHistory)>, Vec<String>);

/// Load every history file the patterns name, in pattern order (each
/// pattern's matches sorted). A pattern matching nothing is returned in
/// the second list; a file that matches but can't be read is an error.
pub fn load_all(patterns: &[String]) -> Result<Loaded> {
    let mut loaded = Vec::new();
    let mut unmatched = Vec::new();
    for pattern in patterns {
        let files = expand(pattern);
        if files.is_empty() {
            unmatched.push(pattern.clone());
        }
        for f in files {
            let h = history::load_file(&f)?;
            loaded.push((f, h));
        }
    }
    Ok((loaded, unmatched))
}

/// `--history-from`: merge the histories other jobs wrote into the one
/// in `artifacts_dir` (which runs after this one then read and update),
/// under its lock. The local history counts as the first file. Returns
/// the files merged and the patterns that matched nothing.
pub fn seed(artifacts_dir: &str, patterns: &[String]) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let (loaded, unmatched) = load_all(patterns)?;
    let local = Path::new(artifacts_dir).join(history::HISTORY_FILENAME);
    let local = local.canonicalize().unwrap_or(local);
    // The local file itself, matched by a pattern: it is already the base.
    let others: Vec<(PathBuf, TestHistory)> = loaded
        .into_iter()
        .filter(|(p, _)| {
            let file = if p.is_dir() {
                p.join(history::HISTORY_FILENAME)
            } else {
                p.clone()
            };
            file.canonicalize().map_or(true, |c| c != local)
        })
        .collect();
    let files = others.iter().map(|(p, _)| p.clone()).collect();
    if !others.is_empty() {
        history::update_saved(artifacts_dir, |h| {
            let mut all = vec![std::mem::take(h)];
            all.extend(others.into_iter().map(|(_, h)| h));
            *h = merge(&all);
        })?;
    }
    Ok((files, unmatched))
}

/// `velocity history merge`: merge `inputs` (paths or globs) and save the
/// result to `output` (a `.json` file, or a folder that gets a
/// `velocity-history.json`). Returns the files merged and the merged
/// history. Matching no file at all is an error.
pub fn merge_files(inputs: &[String], output: &Path) -> Result<(Vec<PathBuf>, TestHistory)> {
    let (loaded, unmatched) = load_all(inputs)?;
    if loaded.is_empty() {
        return Err(VelocityError::Config(format!(
            "no history file matches {}",
            unmatched.join(", ")
        )));
    }
    for p in &unmatched {
        tracing::warn!("history merge: {p} matches no file");
    }
    let (files, histories): (Vec<PathBuf>, Vec<TestHistory>) = loaded.into_iter().unzip();
    let merged = merge(&histories);
    let is_file = output
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("json"));
    if is_file {
        let dir = output.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = dir {
            std::fs::create_dir_all(dir)
                .map_err(|e| VelocityError::Config(format!("{}: {e}", dir.display())))?;
        }
        let mut out = merged.clone();
        out.version = out.version.max(history::HISTORY_VERSION);
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| VelocityError::Config(format!("failed to serialize history: {e}")))?;
        velocity_common::fsutil::write_private(output, json.as_bytes())
            .map_err(|e| VelocityError::Config(format!("{}: {e}", output.display())))?;
    } else {
        history::save(&output.to_string_lossy(), &merged)?;
    }
    Ok((files, merged))
}

/// Counts of what a merged history holds, for the CLI to print. Flagged
/// tests are also counted by the kind of failure they flake on most
/// (`3 flagged flaky (2 timeout, 1 not-found)`) when history knows it.
pub fn describe(h: &TestHistory) -> String {
    let flagged: Vec<&String> = h.outcomes.keys().filter(|t| h.flagged_flaky(t)).collect();
    let mut by_kind: std::collections::BTreeMap<history::FailureKind, usize> = Default::default();
    for t in &flagged {
        let top = h
            .flakiness_by_kind(t)
            .into_iter()
            .find(|(k, _)| *k != history::FailureKind::Unknown);
        if let Some((k, _)) = top {
            *by_kind.entry(k).or_default() += 1;
        }
    }
    let mut kinds: Vec<(history::FailureKind, usize)> = by_kind.into_iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let kinds = if kinds.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = kinds
            .iter()
            .map(|(k, n)| format!("{n} {}", k.label()))
            .collect();
        format!(" ({})", parts.join(", "))
    };
    let devices: std::collections::HashSet<&String> =
        h.by_device.values().flat_map(|m| m.keys()).collect();
    format!(
        "{} test(s), {} with outcomes, {} flagged flaky{kinds}, {} device profile(s)",
        h.durations.len().max(h.outcomes.len()),
        h.outcomes.len(),
        flagged.len(),
        devices.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Outcome;
    use std::collections::HashMap;

    fn job(tests: &[(&str, u64, &[Outcome], u64)]) -> TestHistory {
        let mut h = TestHistory {
            version: history::HISTORY_VERSION,
            ..Default::default()
        };
        for (name, ms, outcomes, at) in tests {
            h.durations.insert(name.to_string(), *ms);
            h.outcomes.insert(name.to_string(), outcomes.to_vec());
            h.last_run.insert(name.to_string(), *at);
        }
        h
    }

    use Outcome::{Fail, Pass};

    #[test]
    fn each_test_comes_from_the_job_that_ran_it_last() {
        // Both shards start from the same base; shard 1 ran `a`, shard 2
        // ran `b`. Concatenating would count the base twice.
        let base: &[(&str, u64, &[Outcome], u64)] =
            &[("a", 1000, &[Pass], 100), ("b", 2000, &[Pass], 100)];
        let mut s1 = job(base);
        s1.durations.insert("a".into(), 1100);
        s1.outcomes.insert("a".into(), vec![Pass, Fail]);
        s1.last_run.insert("a".into(), 200);
        let mut s2 = job(base);
        s2.durations.insert("b".into(), 2500);
        s2.outcomes.insert("b".into(), vec![Pass, Pass]);
        s2.last_run.insert("b".into(), 210);
        let m = merge(&[s1.clone(), s2.clone()]);
        assert_eq!(m.durations["a"], 1100);
        assert_eq!(m.outcomes["a"], vec![Pass, Fail]);
        assert_eq!(m.last_run["a"], 200);
        assert_eq!(m.durations["b"], 2500);
        assert_eq!(m.outcomes["b"], vec![Pass, Pass]);
        // The order the files are given in doesn't matter here.
        let r = merge(&[s2, s1]);
        assert_eq!(r.outcomes, m.outcomes);
        assert_eq!(r.durations, m.durations);
    }

    #[test]
    fn a_test_only_one_job_knows_is_kept_and_ties_go_to_the_later_file() {
        let a = job(&[("only-a", 10, &[Fail], 5), ("t", 1, &[Pass], 7)]);
        let b = job(&[("only-b", 20, &[Pass], 6), ("t", 2, &[Fail], 7)]);
        let m = merge(&[a, b]);
        assert_eq!(m.durations["only-a"], 10);
        assert_eq!(m.durations["only-b"], 20);
        assert_eq!(m.durations["t"], 2, "a tie: the later file");
        assert_eq!(m.version, history::HISTORY_VERSION);
    }

    // Failure kinds line up with the outcomes they describe, so they come
    // from the same file; `describe` counts flagged tests by kind.
    #[test]
    fn failure_kinds_come_with_their_outcomes() {
        use history::FailureKind::{NotFound, Timeout};
        let flips = [Pass, Fail, Pass, Fail, Pass, Fail];
        let mut old = job(&[("pay", 10, &flips, 100)]);
        old.failure_kinds
            .insert("pay".into(), vec![NotFound, NotFound, NotFound]);
        let mut new = job(&[("pay", 10, &flips, 200)]);
        new.failure_kinds
            .insert("pay".into(), vec![Timeout, Timeout, Timeout]);
        let m = merge(&[new.clone(), old.clone()]);
        assert_eq!(m.failure_kinds["pay"], vec![Timeout, Timeout, Timeout]);
        assert!(
            describe(&m).contains("1 flagged flaky (1 timeout)"),
            "{}",
            describe(&m)
        );
        // A file without kinds (an older Velocity's) brings none.
        new.failure_kinds.clear();
        let m = merge(&[old, new]);
        assert!(m.failure_kinds.is_empty());
        assert!(
            describe(&m).contains("1 flagged flaky, "),
            "{}",
            describe(&m)
        );
    }

    #[test]
    fn device_profiles_from_a_matrix_are_all_kept() {
        let mut ios = job(&[("login", 900, &[Pass], 300)]);
        ios.by_device.insert(
            "login".into(),
            HashMap::from([("iPhone SE / iOS 17".to_string(), vec![Fail, Pass])]),
        );
        let mut android = job(&[("login", 950, &[Pass], 301)]);
        android.by_device.insert(
            "login".into(),
            HashMap::from([("Pixel 7 / Android 14".to_string(), vec![Pass])]),
        );
        let m = merge(&[ios, android]);
        let profiles = &m.by_device["login"];
        assert_eq!(profiles["iPhone SE / iOS 17"], vec![Fail, Pass]);
        assert_eq!(profiles["Pixel 7 / Android 14"], vec![Pass]);
    }

    /// A matrix job's history for `login`, from JSON (so the test reads
    /// like the file): its test-wide run time, and per profile the
    /// outcomes and, when given, the time that profile ran.
    fn matrix_job(last_run: u64, profiles: &[(&str, &str, Option<u64>)]) -> TestHistory {
        let by_device: Vec<String> = profiles
            .iter()
            .map(|(p, o, _)| format!(r#""{p}":{o}"#))
            .collect();
        let times: Vec<String> = profiles
            .iter()
            .filter_map(|(p, _, t)| t.map(|t| format!(r#""{p}":{t}"#)))
            .collect();
        let json = format!(
            r#"{{"version":2,"durations":{{"login":900}},"outcomes":{{"login":["pass"]}},
                "last_run":{{"login":{last_run}}},
                "byDevice":{{"login":{{{}}}}},
                "byDeviceLastRun":{{"login":{{{}}}}}}}"#,
            by_device.join(","),
            times.join(",")
        );
        serde_json::from_str(&json).unwrap()
    }

    const IPHONE: &str = "iPhone 15 / iOS 17";
    const PIXEL: &str = "Pixel 7 / Android 14";

    #[test]
    fn each_device_keeps_its_own_new_outcome_when_jobs_share_a_base() {
        // Base H0: login passed once on each device, at t=50. Both matrix
        // jobs start from it. The iPhone job runs at t=100 and fails; the
        // Pixel job runs at t=101 and fails. Each file still holds the
        // other device's base copy, and the Pixel file has the later
        // test-wide `last_run`.
        let iphone = matrix_job(
            100,
            &[
                (IPHONE, r#"["pass","fail"]"#, Some(100)),
                (PIXEL, r#"["pass"]"#, Some(50)),
            ],
        );
        let pixel = matrix_job(
            101,
            &[
                (IPHONE, r#"["pass"]"#, Some(50)),
                (PIXEL, r#"["pass","fail"]"#, Some(101)),
            ],
        );
        for files in [
            vec![iphone.clone(), pixel.clone()],
            vec![pixel.clone(), iphone.clone()],
        ] {
            let m = merge(&files);
            let d = &m.by_device["login"];
            assert_eq!(d[IPHONE], vec![Pass, Fail], "the iPhone job's failure");
            assert_eq!(d[PIXEL], vec![Pass, Fail], "the Pixel job's failure");
            assert_eq!(m.by_device_last_run["login"][IPHONE], 100);
            assert_eq!(m.by_device_last_run["login"][PIXEL], 101);
        }
    }

    #[test]
    fn the_device_that_ran_earlier_keeps_its_outcome_too() {
        // The reverse: the Pixel job ran first (t=100) and failed; the
        // iPhone job ran later (t=101) and passed. The later job's base
        // copy of the Pixel profile must not replace the Pixel failure.
        let pixel = matrix_job(
            100,
            &[
                (IPHONE, r#"["pass"]"#, Some(50)),
                (PIXEL, r#"["pass","fail"]"#, Some(100)),
            ],
        );
        let iphone = matrix_job(
            101,
            &[
                (IPHONE, r#"["pass","pass"]"#, Some(101)),
                (PIXEL, r#"["pass"]"#, Some(50)),
            ],
        );
        for files in [
            vec![pixel.clone(), iphone.clone()],
            vec![iphone.clone(), pixel.clone()],
        ] {
            let d = merge(&files).by_device["login"].clone();
            assert_eq!(d[PIXEL], vec![Pass, Fail]);
            assert_eq!(d[IPHONE], vec![Pass, Pass]);
        }
    }

    #[test]
    fn a_base_copy_without_a_profile_time_loses_to_the_job_that_ran_it() {
        // H0 was written before `byDeviceLastRun`: the base copies carry
        // no profile time, the profile each job ran does.
        let iphone = matrix_job(
            100,
            &[
                (IPHONE, r#"["pass","fail"]"#, Some(100)),
                (PIXEL, r#"["pass"]"#, None),
            ],
        );
        let pixel = matrix_job(
            101,
            &[
                (IPHONE, r#"["pass"]"#, None),
                (PIXEL, r#"["pass","pass"]"#, Some(101)),
            ],
        );
        let m = merge(&[iphone, pixel]);
        assert_eq!(m.by_device["login"][IPHONE], vec![Pass, Fail]);
        assert_eq!(m.by_device["login"][PIXEL], vec![Pass, Pass]);
        // Neither file has a profile time: the test-wide rule, as before.
        let a = matrix_job(100, &[(IPHONE, r#"["fail"]"#, None)]);
        let b = matrix_job(101, &[(IPHONE, r#"["pass"]"#, None)]);
        assert_eq!(merge(&[b, a]).by_device["login"][IPHONE], vec![Pass]);
    }

    #[test]
    fn a_runner_with_a_fast_clock_does_not_freeze_the_history() {
        // A runner 6 hours fast and a runner with a correct clock both ran
        // `t` in this batch, the real one 5 minutes before the merge (as a
        // merge job runs after the jobs). The real run must win in either
        // file order, not by a tie (review-fi2 F4), and the merged run
        // times must not lie in the future.
        let now = history::now_secs();
        let fast = now + 6 * 3600;
        let real_at = now - 300;
        let mut skewed = job(&[("t", 100, &[Fail], fast)]);
        skewed.by_device.insert(
            "t".into(),
            HashMap::from([(IPHONE.to_string(), vec![Fail])]),
        );
        skewed
            .by_device_last_run
            .insert("t".into(), HashMap::from([(IPHONE.to_string(), fast)]));
        let mut real = job(&[("t", 200, &[Pass], real_at)]);
        real.by_device.insert(
            "t".into(),
            HashMap::from([(IPHONE.to_string(), vec![Pass])]),
        );
        real.by_device_last_run
            .insert("t".into(), HashMap::from([(IPHONE.to_string(), real_at)]));
        for files in [
            vec![skewed.clone(), real.clone()],
            vec![real.clone(), skewed.clone()],
        ] {
            let m = merge(&files);
            assert_eq!(m.durations["t"], 200);
            assert_eq!(m.outcomes["t"], vec![Pass]);
            assert_eq!(m.by_device["t"][IPHONE], vec![Pass]);
            let after = history::now_secs();
            assert!(m.last_run["t"] <= after, "{} > {after}", m.last_run["t"]);
            assert!(m.by_device_last_run["t"][IPHONE] <= after);
        }
        // Alone, the fast runner's file still counts (clamped).
        let m = merge(&[skewed]);
        assert_eq!(m.outcomes["t"], vec![Fail]);
        assert!(m.last_run["t"] <= history::now_secs());
    }

    #[test]
    fn an_old_binarys_file_without_profile_times_keeps_its_device() {
        // Mixed versions (review-fi2 F5): the iPhone job runs an older
        // Velocity, which rewrote its file without `byDeviceLastRun`; it
        // ran at t=100 and failed. The Pixel job (new) ran at t=101 and
        // carries the iPhone base copy with its base time, t=50.
        let json = r#"{"version":2,"durations":{"login":900},"outcomes":{"login":["pass","fail"]},
            "last_run":{"login":100},
            "byDevice":{"login":{"iPhone 15 / iOS 17":["pass","fail"],"Pixel 7 / Android 14":["pass"]}}}"#;
        let old: TestHistory = serde_json::from_str(json).unwrap();
        assert!(old.by_device_last_run.is_empty());
        let pixel = matrix_job(
            101,
            &[
                (IPHONE, r#"["pass"]"#, Some(50)),
                (PIXEL, r#"["pass","fail"]"#, Some(101)),
            ],
        );
        for files in [vec![old.clone(), pixel.clone()], vec![pixel, old]] {
            let m = merge(&files);
            let d = &m.by_device["login"];
            assert_eq!(d[IPHONE], vec![Pass, Fail], "the old job's failure");
            assert_eq!(d[PIXEL], vec![Pass, Fail]);
            // The merged file has every profile's time again.
            assert_eq!(m.by_device_last_run["login"][IPHONE], 100);
            assert_eq!(m.by_device_last_run["login"][PIXEL], 101);
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vel-shared-hist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn globs_find_downloaded_artifacts() {
        let dir = scratch("glob");
        for job in ["history-shard-1", "history-shard-2", "other"] {
            let d = dir.join("dl").join(job);
            history::save(d.to_str().unwrap(), &TestHistory::default()).unwrap();
        }
        std::fs::write(dir.join("dl/notes.txt"), "x").unwrap();
        let root = dir.to_string_lossy();
        let shards = expand(&format!("{root}/dl/history-*"));
        assert_eq!(shards.len(), 2, "{shards:?}");
        assert!(shards[0].ends_with("history-shard-1"));
        let files = expand(&format!("{root}/**/velocity-history.json"));
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(expand(&format!("{root}/nothing-*")).is_empty());
        // A plain path, as is.
        assert_eq!(expand(&format!("{root}/dl/other")).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_files_writes_a_file_or_a_folder() {
        let dir = scratch("files");
        let a = dir.join("a");
        let b = dir.join("b");
        history::save(a.to_str().unwrap(), &job(&[("x", 5, &[Pass], 1)])).unwrap();
        history::save(b.to_str().unwrap(), &job(&[("y", 6, &[Fail], 2)])).unwrap();
        let out = dir.join("merged/h.json");
        let pattern = format!("{}/*", dir.to_string_lossy());
        let (files, merged) = merge_files(&[pattern], &out).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(merged.durations.len(), 2);
        let back = history::load_file(&out).unwrap();
        assert_eq!(back.durations["y"], 6);
        let (_, _) = merge_files(
            &[
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            &dir.join("folder"),
        )
        .unwrap();
        assert_eq!(
            history::load(dir.join("folder").to_str().unwrap())
                .unwrap()
                .durations
                .len(),
            2
        );
        let err = merge_files(&["/no/such/*".to_string()], &out).unwrap_err();
        assert!(err.to_string().contains("no history file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeding_merges_other_jobs_into_the_local_history() {
        let dir = scratch("seed");
        let local = dir.join("velocity-results");
        history::save(local.to_str().unwrap(), &job(&[("mine", 1, &[Pass], 50)])).unwrap();
        let other = dir.join("dl/shard-2");
        history::save(other.to_str().unwrap(), &job(&[("theirs", 2, &[Fail], 60)])).unwrap();
        let patterns = vec![
            format!("{}/dl/*", dir.to_string_lossy()),
            format!("{}/missing-*", dir.to_string_lossy()),
            // The local file itself is not merged into itself.
            local.to_string_lossy().into_owned(),
        ];
        let (files, unmatched) = seed(local.to_str().unwrap(), &patterns).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(unmatched.len(), 1);
        let h = history::load(local.to_str().unwrap()).unwrap();
        assert_eq!(h.durations["mine"], 1);
        assert_eq!(h.durations["theirs"], 2);
        assert!(h.failed_last("theirs"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
