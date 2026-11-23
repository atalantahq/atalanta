//! Flaky hunting: run the suite several times under perturbation and report
//! the tests whose outcome changes.
//!
//! Plain reruns are a weak detector — on 52 known Android flaky tests, 100
//! reruns exposed 8 (Dong et al. 2021) — because they replay the same timing.
//! Each round here changes what flaky tests are sensitive to: test order
//! (shuffled, new seed per round), step timing (random jitter before each
//! step), and optionally device load (`--stress`, Android). Retries are off so
//! every outcome is observed raw.
//!
//! With `--parallel M` ([`hunt_parallel`]) the rounds don't take turns: every
//! (test, round) copy goes on one queue across the device pool, each round
//! with its own order and jitter seed, and copies of one test go to
//! different devices. A test that fails only on some devices then shows up
//! as a device effect rather than as a flaky test.
//!
//! With `--stress`, every other round (the 1st, 3rd, …) runs under load, so
//! a flake that needs it can be told from one that doesn't. For each flaky
//! test the report says where its passing and failing rounds diverge and
//! ranks candidate causes ([`crate::flake_cause`]); `--suggest` adds the
//! YAML change for the top one and re-runs the hunt with it applied in
//! memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use atalanta_common::{
    PlatformDriver, Result, RuntimeConfig, SourcePos, SuiteResult, TestStatus, TestSuite,
};

use crate::copies::{device_effect, tally_by_device, CopyKind, CopyOutcome, DeviceTally};
use crate::flake_cause::{self, Diagnosis, Observation};
use crate::runner::SuiteRunner;

/// What a hunt does besides finding flaky tests.
#[derive(Debug, Default, Clone)]
pub struct HuntOptions {
    /// `--suggest`: the YAML change for each flaky test's top cause, and
    /// the hunt re-run with the changes applied in memory (the suite file
    /// is never edited).
    pub suggest: bool,
    /// Each test's `- name:` line, for test-level changes.
    pub test_sources: HashMap<String, SourcePos>,
}

/// Per-test outcome counts across rounds.
#[derive(Debug, Default, Clone)]
pub struct HuntTally {
    pub passed: u32,
    pub failed: u32,
    /// The first failure message seen, for triage.
    pub first_failure: Option<String>,
    /// Seed of the round that first failed, to replay it.
    pub failing_seed: Option<u64>,
    /// Pass/fail counts per device (parallel hunts).
    pub by_device: BTreeMap<String, DeviceTally>,
    /// Set when the failures follow devices rather than the test.
    pub device_effect: Option<String>,
}

impl HuntTally {
    pub fn is_flaky(&self) -> bool {
        self.passed > 0 && self.failed > 0
    }
}

pub struct HuntReport {
    pub rounds: u32,
    pub tests: BTreeMap<String, HuntTally>,
    /// The last round's result, for the usual report files.
    pub last: SuiteResult,
    /// Where each flaky test's passing and failing rounds diverge.
    pub diagnoses: BTreeMap<String, Diagnosis>,
}

impl HuntReport {
    pub fn flaky(&self) -> impl Iterator<Item = (&String, &HuntTally)> {
        self.tests.iter().filter(|(_, t)| t.is_flaky())
    }

    /// The `hunt` field of the JSON report: rounds, and each flaky test
    /// with its counts and divergence.
    pub fn json(&self) -> serde_json::Value {
        let flaky: Vec<serde_json::Value> = self
            .flaky()
            .map(|(name, t)| {
                serde_json::json!({
                    "test": name,
                    "passed": t.passed,
                    "failed": t.failed,
                    "failingSeed": t.failing_seed,
                    "deviceEffect": t.device_effect,
                    "divergence": self.diagnoses.get(name),
                })
            })
            .collect();
        serde_json::json!({ "rounds": self.rounds, "flaky": flaky })
    }

    pub fn exit_code(&self) -> i32 {
        i32::from(self.flaky().next().is_some())
    }
}

pub async fn hunt(
    suite: TestSuite,
    config: RuntimeConfig,
    driver: &dyn PlatformDriver,
) -> Result<HuntReport> {
    hunt_with(suite, config, driver, &HuntOptions::default()).await
}

/// [`hunt`], with `--suggest` and the tests' source lines.
pub async fn hunt_with(
    suite: TestSuite,
    config: RuntimeConfig,
    driver: &dyn PlatformDriver,
    options: &HuntOptions,
) -> Result<HuntReport> {
    let rounds = config.hunt_rounds.unwrap_or(5).max(1);
    let device_id = match &config.device_id {
        Some(id) => id.clone(),
        None => driver
            .list_devices()
            .await?
            .first()
            .map(|d| d.id.clone())
            .ok_or_else(|| {
                crate::runner::device_unavailable(
                    "A device",
                    "none found. Boot a device first.".into(),
                )
            })?,
    };

    let stress = config.stress && driver.set_stress(&device_id, true).await;
    if config.stress && !stress {
        cprintln!(
            "  \x1b[33m⚠ --stress is not supported on this platform; continuing without\x1b[0m"
        );
    }
    let ran = run_rounds(&suite, &config, driver, &device_id, rounds, stress).await?;
    let mut report = HuntReport {
        rounds,
        diagnoses: diagnose_all(&suite, &ran.observations, options),
        tests: ran.tests,
        last: ran.last,
    };
    if options.suggest && config.stop.reason().is_none() {
        if let Some((fixed, tests)) = with_fixes(&suite, &report) {
            print_check_banner(rounds, &tests);
            let stress = config.stress && driver.set_stress(&device_id, true).await;
            let again = run_rounds(&fixed, &config, driver, &device_id, rounds, stress).await?;
            record_verdicts(&mut report, &again.tests);
        }
    }
    print_report(&report, options.suggest);
    Ok(report)
}

/// What [`run_rounds`] saw.
struct Rounds {
    tests: BTreeMap<String, HuntTally>,
    observations: BTreeMap<String, Vec<Observation>>,
    last: SuiteResult,
}

/// `rounds` sequential rounds on `device_id`, the 1st, 3rd, … under
/// stress when `stress` (already switched on by the caller); stress is off
/// when this returns.
async fn run_rounds(
    suite: &TestSuite,
    config: &RuntimeConfig,
    driver: &dyn PlatformDriver,
    device_id: &str,
    rounds: u32,
    stress: bool,
) -> Result<Rounds> {
    let base_seed = config.shuffle_seed.unwrap_or(1);
    let mut tests: BTreeMap<String, HuntTally> = BTreeMap::new();
    let mut observations: BTreeMap<String, Vec<Observation>> = BTreeMap::new();
    let mut last = None;
    // One run start for every round: a round's failure stays current
    // when a later round passes (review-s9 M3, f5-f6 MAJOR-1).
    let run_started = Some(
        config
            .run_started
            .unwrap_or_else(std::time::SystemTime::now),
    );
    for round in 0..rounds {
        let seed = base_seed.wrapping_add(u64::from(round));
        // Alternate rounds unstressed: the control a "fails only under
        // --stress" verdict needs.
        let stressed = stress && round % 2 == 0;
        if stress && round > 0 {
            driver.set_stress(device_id, stressed).await;
        }
        cprintln!(
            "\n  \x1b[1mFlaky hunt round {}/{rounds}\x1b[0m (seed {seed}{})",
            round + 1,
            if stressed { ", CPU stress" } else { "" }
        );
        let round_config = RuntimeConfig {
            run_started,
            retry_count: Some(0),
            fail_fast: false,
            failed_first: false,
            shuffle_seed: Some(seed),
            device_id: Some(device_id.to_string()),
            // Round 2 would serve round 1's passes from the cache and
            // never re-run a flaky test (VEL-324).
            cache_results: false,
            ..config.clone()
        };
        let result = SuiteRunner::run(suite.clone(), round_config, driver).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                if stress {
                    driver.set_stress(device_id, false).await;
                }
                return Err(e);
            }
        };
        // Results are in the order the tests ran.
        let mut before: Vec<String> = Vec::new();
        for t in &result.tests {
            let tally = tests.entry(t.test_name.clone()).or_default();
            match t.status {
                TestStatus::Passed | TestStatus::Retried => tally.passed += 1,
                // A lost device says nothing about the test (as in
                // `tally_copies` and `Observation::failed`).
                TestStatus::Failed if t.infrastructure_failure => continue,
                TestStatus::Failed => {
                    tally.failed += 1;
                    if tally.first_failure.is_none() {
                        tally.first_failure = t.error_message.clone();
                        tally.failing_seed = Some(seed);
                    }
                }
                TestStatus::Skipped => continue,
            }
            observations
                .entry(t.test_name.clone())
                .or_default()
                .push(Observation {
                    round,
                    seed,
                    stressed,
                    ran_before: Some(before.clone()),
                    result: t.clone(),
                });
            before.push(t.test_name.clone());
        }
        last = Some(result);
        // Ctrl-C: the rounds left would only skip every test.
        if config.stop.reason().is_some() {
            break;
        }
    }
    if stress {
        driver.set_stress(device_id, false).await;
    }
    Ok(Rounds {
        tests,
        observations,
        last: last.expect("at least one round"),
    })
}

/// A diagnosis for every test seen both passing and failing.
fn diagnose_all(
    suite: &TestSuite,
    observations: &BTreeMap<String, Vec<Observation>>,
    options: &HuntOptions,
) -> BTreeMap<String, Diagnosis> {
    let cx = flake_cause::Context {
        suite: Some(suite),
        test_sources: Some(&options.test_sources),
        suggest: options.suggest,
    };
    observations
        .iter()
        .filter_map(|(name, obs)| Some((name.clone(), flake_cause::diagnose(name, obs, &cx)?)))
        .collect()
}

/// The suite with every suggested change applied, and the tests changed;
/// `None` when there is nothing to check.
fn with_fixes(suite: &TestSuite, report: &HuntReport) -> Option<(TestSuite, Vec<String>)> {
    let mut fixed = suite.clone();
    let mut changed = Vec::new();
    for (name, d) in &report.diagnoses {
        let Some(fix) = d.suggestion.as_ref().and_then(|s| s.fix.as_ref()) else {
            continue;
        };
        if fix.apply(&mut fixed, name) {
            changed.push(name.clone());
        }
    }
    (!changed.is_empty()).then_some((fixed, changed))
}

fn print_check_banner(rounds: u32, tests: &[String]) {
    cprintln!(
        "\n  \x1b[1mChecking the suggested changes\x1b[0m: {rounds} more rounds with them applied \
         in memory (the suite file is not changed) for {}",
        tests.join(", ")
    );
}

/// Each changed test's re-run verdict, into its suggestion.
fn record_verdicts(report: &mut HuntReport, again: &BTreeMap<String, HuntTally>) {
    for (name, d) in report.diagnoses.iter_mut() {
        let Some(s) = d.suggestion.as_mut() else {
            continue;
        };
        let before = (d.passing_rounds, d.failing_rounds);
        let after = again
            .get(name)
            .map_or((0, 0), |t| (t.passed as usize, t.failed as usize));
        s.verified = Some(flake_cause::verdict(before, after));
    }
}

/// [`hunt`] across up to `concurrency` devices: (test, round) copies on one
/// queue (see `ParallelRunner::hunt`). CPU stress is not applied here: the
/// pool's devices are only known to the device farm once leased.
pub async fn hunt_parallel(
    suite: TestSuite,
    config: RuntimeConfig,
    driver: Arc<dyn PlatformDriver>,
    concurrency: usize,
) -> Result<HuntReport> {
    hunt_parallel_with(suite, config, driver, concurrency, &HuntOptions::default()).await
}

/// [`hunt_parallel`], with `--suggest` and the tests' source lines.
pub async fn hunt_parallel_with(
    suite: TestSuite,
    config: RuntimeConfig,
    driver: Arc<dyn PlatformDriver>,
    concurrency: usize,
    options: &HuntOptions,
) -> Result<HuntReport> {
    let rounds = config.hunt_rounds.unwrap_or(5).max(1);
    cprintln!(
        "\n  \x1b[1mFlaky hunt: {rounds} rounds across up to {concurrency} devices\x1b[0m (seeds {}..{})",
        config.shuffle_seed.unwrap_or(1),
        config
            .shuffle_seed
            .unwrap_or(1)
            .wrapping_add(u64::from(rounds - 1))
    );
    let (last, copies) = crate::parallel::ParallelRunner::hunt(
        suite.clone(),
        config.clone(),
        driver.clone(),
        concurrency,
        rounds,
    )
    .await?;
    let mut report = HuntReport {
        rounds,
        tests: tally_copies(&copies),
        diagnoses: diagnose_all(&suite, &copy_observations(&copies), options),
        last,
    };
    if options.suggest && config.stop.reason().is_none() {
        if let Some((fixed, tests)) = with_fixes(&suite, &report) {
            print_check_banner(rounds, &tests);
            let (_, again) =
                crate::parallel::ParallelRunner::hunt(fixed, config, driver, concurrency, rounds)
                    .await?;
            record_verdicts(&mut report, &tally_copies(&again));
        }
    }
    print_report(&report, options.suggest);
    Ok(report)
}

/// A parallel hunt's copies as observations, rounds in order. The order
/// tests ran in on each device isn't kept, so the order rules don't apply.
fn copy_observations(copies: &[CopyOutcome]) -> BTreeMap<String, Vec<Observation>> {
    let mut out: BTreeMap<String, Vec<Observation>> = BTreeMap::new();
    for c in copies {
        let CopyKind::Hunt { round, seed } = c.tag.kind else {
            continue;
        };
        out.entry(c.result.test_name.clone())
            .or_default()
            .push(Observation {
                round,
                seed,
                stressed: false,
                ran_before: None,
                result: c.result.clone(),
            });
    }
    for obs in out.values_mut() {
        obs.sort_by_key(|o| o.round);
    }
    out
}

/// Per-test tallies of a parallel hunt's copies, rounds in order.
fn tally_copies(copies: &[CopyOutcome]) -> BTreeMap<String, HuntTally> {
    let mut sorted: Vec<&CopyOutcome> = copies.iter().collect();
    sorted.sort_by_key(|c| match c.tag.kind {
        CopyKind::Hunt { round, .. } => round,
        CopyKind::Preventive { index, .. } => index,
    });
    let mut tests: BTreeMap<String, HuntTally> = BTreeMap::new();
    for c in &sorted {
        let tally = tests.entry(c.result.test_name.clone()).or_default();
        match c.result.status {
            TestStatus::Passed | TestStatus::Retried => tally.passed += 1,
            // A lost device says nothing about the test.
            TestStatus::Failed if c.result.infrastructure_failure => {}
            TestStatus::Failed => {
                tally.failed += 1;
                if tally.first_failure.is_none() {
                    tally.first_failure = c
                        .result
                        .error_message
                        .as_ref()
                        .map(|m| format!("[{}] {m}", c.device));
                    if let CopyKind::Hunt { seed, .. } = c.tag.kind {
                        tally.failing_seed = Some(seed);
                    }
                }
            }
            TestStatus::Skipped => {}
        }
    }
    for (name, tally) in tests.iter_mut() {
        tally.by_device = tally_by_device(
            sorted
                .iter()
                .copied()
                .filter(|c| &c.result.test_name == name),
        );
        tally.device_effect = device_effect(&tally.by_device);
    }
    tests
}

fn print_report(report: &HuntReport, suggest: bool) {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Flaky hunt: {} rounds", report.rounds);
    let mut any = false;
    for (name, t) in report.flaky() {
        any = true;
        cprintln!(
            "  \x1b[33m● FLAKY\x1b[0m {name}: passed {}, failed {}{}",
            t.passed,
            t.failed,
            t.failing_seed
                .map(|s| format!(" — replay with --shuffle --seed {s}"))
                .unwrap_or_default()
        );
        if let Some(msg) = &t.first_failure {
            println!("    └ {}", atalanta_common::redact::redact(msg));
        }
        if t.by_device.len() > 1 {
            let per: Vec<String> = t
                .by_device
                .iter()
                .map(|(d, c)| format!("{d} {}✓ {}✗", c.passed, c.failed))
                .collect();
            println!("    └ by device: {}", per.join(", "));
        }
        if let Some(note) = &t.device_effect {
            println!("    └ {}", atalanta_common::redact::redact(note));
        }
        if let Some(d) = report.diagnoses.get(name) {
            for line in flake_cause::render(d, suggest) {
                let line = atalanta_common::redact::redact(&line);
                // Causes and patch lines are indented under their heading.
                if line.starts_with(' ') {
                    println!("      {line}");
                } else {
                    println!("    └ {line}");
                }
            }
        }
    }
    let broken: Vec<_> = report
        .tests
        .iter()
        .filter(|(_, t)| t.passed == 0 && t.failed > 0)
        .map(|(n, _)| n.as_str())
        .collect();
    if !broken.is_empty() {
        cprintln!(
            "  \x1b[31m●\x1b[0m failed every round (broken, not flaky): {}",
            broken.join(", ")
        );
    }
    if !any {
        cprintln!("  \x1b[32m●\x1b[0m no flaky tests found");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flaky_means_both_outcomes_seen() {
        let t = |passed, failed| HuntTally {
            passed,
            failed,
            ..Default::default()
        };
        assert!(t(3, 1).is_flaky());
        assert!(!t(4, 0).is_flaky());
        assert!(!t(0, 4).is_flaky(), "always failing is broken, not flaky");
    }

    #[test]
    fn the_json_report_carries_each_flaky_test_and_its_divergence() {
        let tally = HuntTally {
            passed: 2,
            failed: 1,
            failing_seed: Some(3),
            ..Default::default()
        };
        let last = SuiteResult {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![],
            shard_index: None,
            shard_total: None,
        };
        let report = HuntReport {
            rounds: 3,
            tests: BTreeMap::from([
                ("flaky".to_string(), tally),
                (
                    "steady".to_string(),
                    HuntTally {
                        passed: 3,
                        ..Default::default()
                    },
                ),
            ]),
            last,
            diagnoses: BTreeMap::new(),
        };
        let v = report.json();
        assert_eq!(v["rounds"], 3);
        assert_eq!(v["flaky"].as_array().unwrap().len(), 1);
        assert_eq!(v["flaky"][0]["test"], "flaky");
        assert_eq!(v["flaky"][0]["failingSeed"], 3);
        assert!(v["flaky"][0]["divergence"].is_null());

        let path = std::env::temp_dir().join(format!("vel-hunt-json-{}.json", std::process::id()));
        crate::reporter::json::write_json_full(
            &report.last,
            path.to_str().unwrap(),
            Some("completed"),
            Some(&v),
        )
        .unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(doc["hunt"]["flaky"][0]["passed"], 2);
        assert_eq!(doc["termination"], "completed");
        assert!(doc.get("tests").is_some(), "the usual fields stay");
    }
    use atalanta_common::{
        AtalantaError, DeviceInfo, DeviceState, Direction, Element, Key, PlatformDriver, Rect,
        Selector,
    };
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// Every launch is counted; on launch number `lost_on` the device is
    /// offline (an infrastructure failure), otherwise the button is there.
    struct LosesTheDevice {
        device: String,
        launches: AtomicUsize,
        lost_on: usize,
    }

    impl LosesTheDevice {
        fn button() -> Element {
            Element {
                platform_id: "go".into(),
                text: Some("Go".into()),
                element_type: "Button".into(),
                bounds: Rect {
                    x: 10,
                    y: 10,
                    width: 100,
                    height: 40,
                },
                enabled: true,
                visible: true,
                ..Default::default()
            }
        }
        fn offline(&self) -> bool {
            self.launches.load(SeqCst) == self.lost_on
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for LosesTheDevice {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![DeviceInfo {
                id: self.device.clone(),
                name: self.device.clone(),
                platform: atalanta_common::Platform::Android,
                state: DeviceState::Booted,
                os_version: Some("14".into()),
                device_type: Default::default(),
            }])
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            self.launches.fetch_add(1, SeqCst);
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(&self, d: &str, _: &Selector) -> Result<Element> {
            if self.offline() {
                Err(AtalantaError::AdbConnectionLost {
                    device_id: d.into(),
                    reason: "device offline".into(),
                })
            } else {
                Ok(Self::button())
            }
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            Ok(self.find_element(d, s).await.into_iter().collect())
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            if self.offline() {
                return Err(AtalantaError::AdbConnectionLost {
                    device_id: d.into(),
                    reason: "device offline".into(),
                });
            }
            let mut root = Self::button();
            root.platform_id = "root".into();
            root.text = None;
            root.bounds.width = 1080;
            root.bounds.height = 2400;
            root.children.push(Self::button());
            Ok(root)
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
            Ok(())
        }
        async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
            Ok(())
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str, _: Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1080, 2400))
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            Ok("Go".into())
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            Ok(!self.offline())
        }
    }

    // V7: the parallel hunt ignores a round lost to the device
    // (`tally_copies`); the sequential one counted it as a failed round, so a
    // test that passed every other round was reported FLAKY and the hunt
    // exited 1.
    #[tokio::test]
    async fn a_round_lost_to_the_device_does_not_make_a_test_flaky() {
        let dir = std::env::temp_dir().join(format!("vel-hunt-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let device = format!("hunt-lost-{}", std::process::id());
        let driver = LosesTheDevice {
            device: device.clone(),
            launches: AtomicUsize::new(0),
            lost_on: 3,
        };
        let suite = atalanta_core::parser::parse_suite_from_str(
            "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\n    quietMs: 10\n    reactionMs: 10\ntests:\n  - name: t\n    steps:\n      - launchApp: { appId: com.t }\n      - assertVisible: { text: \"Go\" }\n",
        )
        .unwrap();
        let config = RuntimeConfig {
            artifacts_dir: dir.to_string_lossy().into_owned(),
            device_id: Some(device),
            state_dir: Some(dir.join(".atalanta")),
            hunt_rounds: Some(5),
            ..Default::default()
        };
        let report = hunt(suite, config, &driver).await.unwrap();
        let t = &report.tests["t"];
        assert_eq!((t.passed, t.failed), (4, 0), "{t:?}");
        assert!(!t.is_flaky());
        assert_eq!(report.exit_code(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
