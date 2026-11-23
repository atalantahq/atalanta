use std::time::{Duration, Instant};

use velocity_common::{
    PlatformDriver, Result, RuntimeConfig, SuiteResult, TestResult, TestStatus, VelocityError,
};
use velocity_core::TestExecutor;

use crate::history;
use crate::retry::{print_result, run_test_with_retries};
use crate::scheduler;

pub struct SuiteRunner;

/// How a run ended.
#[derive(Debug)]
pub enum Termination {
    /// Every planned test ran (or the plan skipped it).
    Completed,
    /// `--timeout` passed: the test it cut off failed and the rest were
    /// skipped. Holds the `SuiteTimeout` error (exit 4).
    TimedOut(VelocityError),
    /// The run was asked to stop (`RuntimeConfig::stop`): the test running
    /// then and the rest were skipped with this reason.
    Stopped(String),
}

/// A run's result, partial when it didn't complete, and how it ended.
#[derive(Debug)]
pub struct RunOutcome {
    pub result: SuiteResult,
    pub termination: Termination,
}

impl RunOutcome {
    /// The result; a run that timed out is its `SuiteTimeout` error.
    pub fn into_result(self) -> Result<SuiteResult> {
        match self.termination {
            Termination::TimedOut(e) => Err(e),
            _ => Ok(self.result),
        }
    }
}

/// What cut a run short, before it is known how far the run got.
pub(crate) enum Cut {
    Timeout,
    Stop(String),
}

impl Cut {
    /// Whatever comes first: the stop request or the deadline.
    pub(crate) async fn first(
        stop: &velocity_common::StopHandle,
        deadline: Option<tokio::time::Instant>,
    ) -> Cut {
        tokio::select! {
            reason = stop.stopped() => Cut::Stop(reason),
            _ = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => Cut::Timeout,
        }
    }

    /// Already cut: a stop was asked for or the deadline passed.
    pub(crate) fn now(
        stop: &velocity_common::StopHandle,
        deadline: Option<tokio::time::Instant>,
    ) -> Option<Cut> {
        if let Some(r) = stop.reason() {
            return Some(Cut::Stop(r.to_string()));
        }
        deadline
            .filter(|d| tokio::time::Instant::now() >= *d)
            .map(|_| Cut::Timeout)
    }

    /// The result of a test this cut short while it ran.
    pub(crate) fn cut_test(&self, name: &str, timeout_ms: u64, ran: Duration) -> TestResult {
        match self {
            Cut::Timeout => TestResult::failed(
                name,
                format!("suite timeout ({timeout_ms}ms) reached while this test ran"),
                ran,
            ),
            Cut::Stop(reason) => {
                TestResult::skipped(name, &format!("not finished: {reason} while it ran"))
            }
        }
    }

    /// Why a test this cut never ran.
    pub(crate) fn not_run(&self, timeout_ms: u64) -> String {
        match self {
            Cut::Timeout => format!("not run: suite timeout ({timeout_ms}ms) reached"),
            Cut::Stop(reason) => format!("not run: {reason}"),
        }
    }

    pub(crate) fn termination(
        self,
        timeout_ms: u64,
        completed: usize,
        total: usize,
    ) -> Termination {
        match self {
            Cut::Timeout => Termination::TimedOut(VelocityError::SuiteTimeout {
                timeout_ms,
                completed,
                total,
            }),
            Cut::Stop(reason) => Termination::Stopped(reason),
        }
    }
}

impl SuiteRunner {
    pub async fn run(
        suite: velocity_common::TestSuite,
        config: RuntimeConfig,
        driver: &dyn PlatformDriver,
    ) -> Result<SuiteResult> {
        Self::run_outcome(suite, config, driver)
            .await?
            .into_result()
    }

    /// [`run`](Self::run), but a run cut short by `--timeout` or a stop
    /// request still returns its results: what ran, the test cut off, and
    /// the rest as skipped with the reason (VEL-044). History and the
    /// result cache keep only the tests that finished.
    pub async fn run_outcome(
        suite: velocity_common::TestSuite,
        config: RuntimeConfig,
        driver: &dyn PlatformDriver,
    ) -> Result<RunOutcome> {
        let suite_start = Instant::now();
        scheduler::check_shard(&config)?;

        let suite_timeout = config.suite_timeout_ms.map(Duration::from_millis);

        // Determine the device and take its cross-process lease, so another
        // Velocity run (or an MCP agent) can't drive it at the same time.
        let (device_id, _device_lock) = match &config.device_id {
            Some(id) => match crate::lease::lock_waiting(id).await {
                Ok(lock) => (id.clone(), lock),
                Err(crate::lease::LeaseError::Held(holder)) => {
                    return Err(device_unavailable(
                        &format!("Device {id}"),
                        format!(
                            "in use by another Velocity process ({holder}). \
                             Wait for it to finish or pick another device with --device."
                        ),
                    ))
                }
                Err(crate::lease::LeaseError::Io(e)) => {
                    return Err(device_unavailable(&format!("Device {id}"), e))
                }
            },
            None => {
                let devices = driver.list_devices().await?;
                if devices.is_empty() {
                    return Err(device_unavailable(
                        "A device",
                        "none found. Boot a device first.".to_string(),
                    ));
                }
                // Booted devices first; then the first one nobody else holds.
                let mut ordered: Vec<_> = devices.iter().collect();
                ordered.sort_by_key(|d| d.state != velocity_common::DeviceState::Booted);
                let mut held = Vec::new();
                let mut picked = None;
                for d in ordered {
                    match crate::lease::try_lock(&d.id) {
                        Ok(lock) => {
                            picked = Some((d.id.clone(), lock));
                            break;
                        }
                        Err(crate::lease::LeaseError::Held(holder)) => {
                            held.push(format!("{} ({holder})", d.id))
                        }
                        Err(crate::lease::LeaseError::Io(e)) => {
                            return Err(device_unavailable(&format!("Device {}", d.id), e))
                        }
                    }
                }
                picked.ok_or_else(|| {
                    device_unavailable(
                        "Every device",
                        format!("in use by another Velocity process: {}", held.join(", ")),
                    )
                })?
            }
        };

        let mut hist = history::load(&config.artifacts_dir).unwrap_or_default();
        let tests = scheduler::plan_tests(&suite.tests, &config, &hist);
        crate::live::plan(tests.iter().map(|t| t.name.as_str()));
        let planned: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
        // Every test in the suite, selected or not: history keeps them (m-6).
        let suite_tests: std::collections::HashSet<String> =
            suite.tests.iter().map(|t| t.name.clone()).collect();
        let (shard_index, shard_total) = match (config.shard_index, config.shard_total) {
            (Some(i), Some(t)) => (Some(i), Some(t)),
            _ => (None, None),
        };

        let total_tests = tests.len();
        let test_timeout = config.test_timeout_ms.map(std::time::Duration::from_millis);
        let retry_count =
            crate::retry::capped_retries(config.retry_count.unwrap_or(suite.config.retry.count));
        let app_id = suite.app_id.clone();

        println!("\n  Velocity v{}", env!("CARGO_PKG_VERSION"));
        println!(
            "  Tests: {total_tests}{}",
            match (shard_index, shard_total) {
                (Some(i), Some(t)) => format!(" (shard {}/{t})", i + 1),
                _ => String::new(),
            }
        );
        if let Some(seed) = config.shuffle_seed {
            println!("  Order: shuffled (seed {seed}; rerun with --shuffle --seed {seed})");
        }
        // `app:` / `--app`: the build under test goes on first (skipped
        // when the device already has this exact build).
        if let Some(app) = crate::install::build_for(&suite, config.platform) {
            crate::install::ensure_installed(driver, &device_id, &app, &app_id).await?;
        }
        println!();

        let mut executor = TestExecutor::new(driver, suite.config.clone(), &app_id)
            .with_update_baselines(config.update_baselines)
            .with_created_baselines(config.created_baselines.clone())
            .with_trace(config.trace)
            .with_run_started(
                config
                    .run_started
                    .unwrap_or_else(std::time::SystemTime::now),
            );
        if let Some(max) = config.jitter_ms {
            executor = executor.with_jitter(max, config.shuffle_seed.unwrap_or(0));
        }
        if let Some(dir) = &config.state_dir {
            executor = executor.with_state_dir(dir);
        }
        executor = executor.with_verify(config.verify);
        let resolver = crate::nl::build_resolver_for_suite(&suite, config.verify)?;
        if let Some(resolver) = &resolver {
            executor = executor.with_planner(resolver.clone());
        }
        let mut results: Vec<TestResult> = Vec::with_capacity(total_tests);
        let new_tests = early_flake_candidates(&tests, &hist, &config);
        // An `--update-baselines` run must run every test: a cached pass
        // would leave its baselines as they were (VEL-050).
        let mut result_cache = (config.cache_results && !config.update_baselines.is_on())
            .then(|| crate::result_cache::ResultCache::load(&config.artifacts_dir));

        // `--stress` for the whole run (a hunt sets it per round itself).
        let stressed = config.stress
            && config.hunt_rounds.is_none()
            && driver.set_stress(&device_id, true).await;
        if config.stress && config.hunt_rounds.is_none() && !stressed {
            cprintln!(
                "  \x1b[33m⚠ --stress is not supported on this platform; continuing without\x1b[0m"
            );
        }
        // `--timeout` stops the test running when it passes, not only the
        // next one from starting.
        let deadline = suite_timeout.map(|t| tokio::time::Instant::from_std(suite_start) + t);
        let timeout_ms = config.suite_timeout_ms.unwrap_or(0);
        let mut cut: Option<Cut> = None;
        // Tests cut off mid-run: their result says so, but they never
        // reach the history.
        let mut unfinished: Vec<String> = Vec::new();
        for test in &tests {
            if let Some(c) = Cut::now(&config.stop, deadline) {
                cut = Some(c);
                break;
            }

            // A test that builds on another's state can't pass if that one
            // didn't — report it as skipped rather than as a second failure.
            if let Some(dep) = crate::retry::unmet_dependency(test, &results) {
                let result = TestResult::skipped(
                    &test.name,
                    &format!("Skipped: dependency '{dep}' did not pass"),
                );
                print_result(&result, None);
                results.push(result);
                continue;
            }

            let cache_key = match &result_cache {
                Some(_) if crate::result_cache::cacheable(test, &tests) => {
                    crate::result_cache::key(driver, &device_id, &app_id, &suite.config, test).await
                }
                _ => None,
            };
            if let (Some(cache), Some(key)) = (&result_cache, &cache_key) {
                if let Some(hit) = cache.hit(key, test) {
                    print_result(&hit, None);
                    results.push(hit);
                    continue;
                }
            }

            let started = Instant::now();
            let mut result = tokio::select! {
                r = run_test_with_retries(&mut executor, test, &device_id, &app_id, retry_count, test_timeout) => r,
                c = Cut::first(&config.stop, deadline) => {
                    let r = c.cut_test(&test.name, timeout_ms, started.elapsed());
                    print_result(&r, None);
                    unfinished.push(test.name.clone());
                    results.push(r);
                    cut = Some(c);
                    break;
                }
            };
            if result.status == TestStatus::Passed && new_tests.contains(&test.name) {
                let runs = early_flake_runs(result.duration);
                let mut failures = Vec::new();
                for _ in 0..runs {
                    // Cut short: the test passed; the extra runs just stop.
                    let again = tokio::select! {
                        r = run_test_with_retries(&mut executor, test, &device_id, &app_id, 0, test_timeout) => r,
                        c = Cut::first(&config.stop, deadline) => {
                            cut = Some(c);
                            break;
                        }
                    };
                    if again.status == TestStatus::Failed {
                        failures.push(again.error_message.unwrap_or_else(|| "failed".into()));
                    }
                }
                if failures.is_empty() {
                    println!("    new test: passed {runs} extra run(s)");
                } else if failures.len() as u32 == runs && cut.is_none() {
                    // Passing only on its first run is no flake: the test
                    // breaks itself (state it leaves, order). It fails, or
                    // it merges green (VEL-337).
                    result.status = TestStatus::Failed;
                    result.error_message = Some(format!(
                        "new test passed once, then failed all {runs} early-flake-detection runs: {}",
                        failures[0]
                    ));
                    result.retries = runs;
                    result.failed_attempts.extend(failures);
                } else {
                    result.status = TestStatus::Retried;
                    result.retries = failures.len() as u32;
                    result.warnings.insert(
                        0,
                        format!(
                            "new test is flaky: failed {} of {runs} early-flake-detection runs",
                            failures.len()
                        ),
                    );
                    result.failed_attempts.extend(failures);
                }
            }

            // A memory breach fails the test before it is cached or shown.
            history::apply_memory_check(
                &mut result,
                hist.resource_baselines.get(&test.name),
                suite.config.performance.pss_growth_threshold_pct,
            );

            // After the early-flake runs: a new test that turned out flaky
            // (or broken) was cached as a pass first and skipped from then
            // on (VEL-S10).
            if let (Some(cache), Some(key)) = (&mut result_cache, cache_key) {
                cache.record(key, &result);
            }

            print_result(&result, None);

            let failed = result.status == TestStatus::Failed;
            results.push(result);

            if config.fail_fast && failed {
                for remaining in tests.iter().skip(results.len()) {
                    results.push(TestResult::skipped(
                        &remaining.name,
                        "Skipped due to --fail-fast",
                    ));
                }
                break;
            }
            if cut.is_some() {
                break;
            }
        }
        if stressed {
            driver.set_stress(&device_id, false).await;
        }
        let completed = results.len() - unfinished.len();
        if let Some(c) = &cut {
            let why = c.not_run(timeout_ms);
            for remaining in tests.iter().skip(results.len()) {
                let r = TestResult::skipped(&remaining.name, &why);
                results.push(r);
            }
            cprintln!(
                "\n  \x1b[33m⚠ {}; {} of {total_tests} test(s) did not finish\x1b[0m",
                why.trim_start_matches("not run: "),
                total_tests - completed
            );
        }

        if let Some(cache) = &mut result_cache {
            cache.save();
        }
        if let Some(resolver) = &resolver {
            let pruned = resolver.finish().await;
            if pruned > 0 {
                println!("  Pruned {pruned} unused natural-language cache entries");
            }
        }

        // NEW-001: every planned test has a result, or a failed one.
        crate::completeness::account_for_every_test(&planned, &mut results);

        let suite_duration = suite_start.elapsed();
        let passed = results
            .iter()
            .filter(|r| r.status == TestStatus::Passed)
            .count();
        let failed = results
            .iter()
            .filter(|r| r.status == TestStatus::Failed)
            .count();
        let skipped = results
            .iter()
            .filter(|r| r.status == TestStatus::Skipped)
            .count();
        let retried = results
            .iter()
            .filter(|r| r.status == TestStatus::Retried)
            .count();

        // Update history
        // Hunt rounds are deliberate stress experiments, not real outcomes.
        let profiles = match config.hunt_rounds {
            None => Some(history::device_profiles(driver).await),
            Some(_) => None,
        };
        let finished: Vec<TestResult> = results
            .iter()
            .filter(|r| !unfinished.contains(&r.test_name))
            .cloned()
            .collect();
        if let Ok(saved) = history::update_saved(&config.artifacts_dir, |h| {
            if let Some(profiles) = &profiles {
                history::update_with_devices(h, &finished, profiles);
            }
            history::forget_stale(h, &suite_tests);
        }) {
            hist = saved;
        }
        history::note_flakiness(&hist, &mut results);

        println!();
        println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "  {passed} passed, {failed} failed{}{} — {:.1}s",
            if retried > 0 {
                format!(" ({retried} flaky)")
            } else {
                String::new()
            },
            if skipped > 0 {
                format!(", {skipped} skipped")
            } else {
                String::new()
            },
            suite_duration.as_secs_f64()
        );

        crate::incidents::print_incidents(&results);
        let flaky = history::flaky_report(&hist, &results);
        if !flaky.is_empty() {
            cprintln!("\n  \x1b[33mFlaky by history\x1b[0m (flip or retry rate ≥ 15%):");
            for line in flaky {
                println!("    {line}");
            }
        }

        let result = SuiteResult {
            total: results.len(),
            passed,
            failed,
            skipped,
            retried,
            duration: suite_duration,
            tests: results,
            shard_index,
            shard_total,
        };
        Ok(RunOutcome {
            result,
            termination: match cut {
                Some(c) => c.termination(timeout_ms, completed, total_tests),
                None => Termination::Completed,
            },
        })
    }
}

/// No device the run can use: a device problem (exit 3), not a
/// configuration error (exit 2) as it was reported (VEL-051).
pub(crate) fn device_unavailable(what: &str, detail: String) -> VelocityError {
    VelocityError::ServiceUnavailable {
        service: what.to_string(),
        detail,
    }
}

/// Extra runs for a new test that passed, by its duration (Datadog's early
/// flake detection): short tests are cheap to repeat; past 5 minutes, none.
fn early_flake_runs(duration: Duration) -> u32 {
    match duration.as_secs() {
        0..5 => 10,
        5..10 => 5,
        10..30 => 3,
        30..300 => 2,
        _ => 0,
    }
}

/// Tests to give extra runs: new to the history, and independent (a test in
/// a `dependsOn` chain can't be repeated on its own). Off when more than
/// 30% of the run is new — then it's a new suite, not a few new tests.
fn early_flake_candidates(
    tests: &[velocity_common::TestCase],
    hist: &history::TestHistory,
    config: &RuntimeConfig,
) -> std::collections::HashSet<String> {
    if !config.detect_new_flakes || tests.is_empty() {
        return Default::default();
    }
    let depended_on: std::collections::HashSet<&str> = tests
        .iter()
        .flat_map(|t| t.depends_on.iter().map(String::as_str))
        .collect();
    let new: std::collections::HashSet<String> = tests
        .iter()
        .filter(|t| !hist.outcomes.contains_key(&t.name))
        .filter(|t| t.depends_on.is_empty() && !depended_on.contains(t.name.as_str()))
        .map(|t| t.name.clone())
        .collect();
    if new.len() as f64 > 0.3 * tests.len() as f64 {
        println!(
            "  Early flake detection off: {} of {} tests are new (limit 30%)",
            new.len(),
            tests.len()
        );
        return Default::default();
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_new_tests_get_more_extra_runs() {
        assert_eq!(early_flake_runs(Duration::from_secs(2)), 10);
        assert_eq!(early_flake_runs(Duration::from_secs(7)), 5);
        assert_eq!(early_flake_runs(Duration::from_secs(20)), 3);
        assert_eq!(early_flake_runs(Duration::from_secs(120)), 2);
        assert_eq!(early_flake_runs(Duration::from_secs(900)), 0);
    }

    fn case(name: &str, deps: &[&str]) -> velocity_common::TestCase {
        velocity_common::TestCase {
            name: name.into(),
            tags: vec![],
            isolated: false,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            steps: vec![],
            description: None,
            verify: None,
        }
    }

    #[test]
    fn only_independent_new_tests_and_never_a_mostly_new_suite() {
        let mut hist = history::TestHistory::default();
        for n in ["a", "b", "c", "d", "e", "f", "g"] {
            hist.outcomes
                .insert(n.into(), vec![history::HistoryOutcome::Pass]);
        }
        let config = RuntimeConfig {
            detect_new_flakes: true,
            ..Default::default()
        };
        let mut tests: Vec<_> = ["a", "b", "c", "d", "e", "f", "g"]
            .iter()
            .map(|n| case(n, &[]))
            .collect();
        tests.push(case("new", &[]));
        tests.push(case("new-dependent", &["a"]));
        let picked = early_flake_candidates(&tests, &hist, &config);
        assert_eq!(picked.len(), 1);
        assert!(picked.contains("new"));

        let all_new: Vec<_> = (0..4).map(|i| case(&format!("n{i}"), &[])).collect();
        assert!(early_flake_candidates(&all_new, &hist, &config).is_empty());
    }

    /// A device whose app `com.<x>` shows its button on the launches
    /// `shows(x, n)` allows (n counts that app's launches from 1).
    struct FlipDriver {
        device: String,
        launches: std::sync::Mutex<std::collections::HashMap<String, usize>>,
        current: std::sync::Mutex<String>,
        shows: fn(&str, usize) -> bool,
    }

    impl FlipDriver {
        fn new(device: &str, shows: fn(&str, usize) -> bool) -> Self {
            Self {
                device: device.into(),
                launches: Default::default(),
                current: Default::default(),
                shows,
            }
        }
        fn launches(&self, app: &str) -> usize {
            self.launches.lock().unwrap().get(app).copied().unwrap_or(0)
        }
        fn visible(&self) -> bool {
            let app = self.current.lock().unwrap().clone();
            (self.shows)(&app, self.launches(&app))
        }
        fn button() -> Element {
            Element {
                hint: None,
                platform_id: "go".into(),
                label: None,
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
                children: vec![],
                ..Default::default()
            }
        }
    }

    use velocity_common::{DeviceInfo, DeviceState, Direction, Element, Key, Rect, Selector};

    #[async_trait::async_trait]
    impl PlatformDriver for FlipDriver {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![DeviceInfo {
                id: self.device.clone(),
                name: self.device.clone(),
                platform: velocity_common::Platform::Android,
                state: DeviceState::Booted,
                os_version: Some("14".into()),
                device_type: Default::default(),
            }])
        }
        async fn app_fingerprint(&self, _: &str, _: &str) -> Option<String> {
            Some("build-1".into())
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
        async fn launch_app(&self, _: &str, app: &str, _: bool) -> Result<()> {
            *self.launches.lock().unwrap().entry(app.into()).or_default() += 1;
            *self.current.lock().unwrap() = app.into();
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(&self, _: &str, s: &Selector) -> Result<Element> {
            if self.visible() {
                Ok(Self::button())
            } else {
                Err(VelocityError::ElementNotFound {
                    selector: format!("{s:?}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                })
            }
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            Ok(self.find_element(d, s).await.into_iter().collect())
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            let mut root = Self::button();
            root.platform_id = "root".into();
            root.text = None;
            root.bounds.width = 1080;
            root.bounds.height = 2400;
            if self.visible() {
                root.children.push(Self::button());
            }
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
            Ok(self.visible())
        }
        async fn collect_resource_metrics(&self, _: &str, _: &str) -> Result<(u64, u64, u64, f32)> {
            Ok((
                0,
                0,
                FLIP_PSS_KB.load(std::sync::atomic::Ordering::SeqCst),
                0.0,
            ))
        }
    }

    /// The total PSS [`FlipDriver`] reports (read only by suites with
    /// `performance.enabled`).
    static FLIP_PSS_KB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // NEW-021: a memory breach fails the test in the sequential runner, and
    // the next run still fails: the breached run never became the baseline.
    #[tokio::test]
    async fn a_memory_breach_fails_the_run_and_the_next_one() {
        use std::sync::atomic::Ordering::SeqCst;
        let (dir, config) = scratch("membreach");
        let suite = || {
            let mut s = flip_suite(&["mem"]);
            s.config.performance.enabled = true;
            s
        };
        let mut statuses = Vec::new();
        for pss in [100_000, 130_000, 130_000, 105_000] {
            FLIP_PSS_KB.store(pss, SeqCst);
            let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |_, _| true);
            let r = SuiteRunner::run(suite(), config.clone(), &driver)
                .await
                .unwrap();
            statuses.push((r.tests[0].status, r.tests[0].error_message.clone()));
        }
        assert_eq!(statuses[0].0, TestStatus::Passed, "{statuses:?}");
        for (status, msg) in &statuses[1..3] {
            assert_eq!(*status, TestStatus::Failed, "{statuses:?}");
            assert!(
                msg.as_deref().unwrap().contains("memory regression"),
                "{msg:?}"
            );
        }
        assert_eq!(statuses[3].0, TestStatus::Passed, "{statuses:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One test per app, each launching `com.<name>` and checking the button.
    fn flip_suite(names: &[&str]) -> velocity_common::TestSuite {
        let mut yaml = String::from(
            "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\n    quietMs: 10\n    reactionMs: 10\ntests:\n",
        );
        for n in names {
            yaml.push_str(&format!(
                "  - name: {n}\n    steps:\n      - launchApp: {{ appId: com.{n} }}\n      - assertVisible: {{ text: \"Go\" }}\n"
            ));
        }
        velocity_core::parser::parse_suite_from_str(&yaml).unwrap()
    }

    fn scratch(name: &str) -> (std::path::PathBuf, RuntimeConfig) {
        let dir = std::env::temp_dir().join(format!("velocity-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = RuntimeConfig {
            artifacts_dir: dir.to_string_lossy().into_owned(),
            device_id: Some(format!("run-{name}-{}", std::process::id())),
            // Healing state in the scratch dir, not this crate's `.velocity/`.
            state_dir: Some(dir.join(".velocity")),
            ..Default::default()
        };
        (dir, config)
    }

    // D-19 (review M1): a panic on the first attempt and a pass on the
    // retry ended FLAKY with exit 0; the panic now sticks: Failed,
    // internal error, exit 5, whatever the quarantine or --fail-on-flaky say.
    #[tokio::test]
    async fn a_panic_then_a_passing_retry_is_still_an_internal_error() {
        let (dir, config) = scratch("panicpass");
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |app, n| {
            if app == "com.boom" && n == 1 {
                panic!("driver bug on the first launch");
            }
            true
        });
        let config = RuntimeConfig {
            retry_count: Some(2),
            ..config
        };
        let mut r = SuiteRunner::run(flip_suite(&["boom"]), config, &driver)
            .await
            .unwrap();
        let t = &r.tests[0];
        // The retry still ran: the evidence that the app works is kept.
        assert_eq!(driver.launches("com.boom"), 2);
        assert_eq!(t.status, TestStatus::Failed, "{t:#?}");
        assert!(t.internal_error, "{t:#?}");
        assert!(t.error_message.as_deref().unwrap().contains("panicked"));
        assert!(t.failed_attempts[0].contains("panicked"), "{t:#?}");
        assert!(t.warnings.iter().any(|w| w.contains("a retry passed")));
        assert_eq!(r.exit_code(), 5);
        assert_eq!(r.exit_code_with(true), 5);
        let q: crate::quarantine::Quarantine =
            serde_yaml::from_str("quarantine:\n  - test: boom\n    signature: panicked\n").unwrap();
        let v = q.apply(&mut r);
        assert!(!v.all_forgiven());
        assert!(r.tests[0].quarantined.is_none());
        assert_eq!(r.exit_code_with(true), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-176: `--retries` was unbounded in the sequential runner.
    #[tokio::test]
    async fn retries_are_capped() {
        let (dir, config) = scratch("retrycap");
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |_, _| false);
        let config = RuntimeConfig {
            retry_count: Some(50),
            ..config
        };
        let r = SuiteRunner::run(flip_suite(&["never"]), config, &driver)
            .await
            .unwrap();
        assert_eq!(r.tests[0].status, TestStatus::Failed);
        assert_eq!(
            driver.launches("com.never"),
            1 + crate::retry::MAX_TEST_RETRIES as usize
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-051: a `--device` another Velocity process holds is a device
    // problem (exit 3), not a configuration error (exit 2).
    #[tokio::test]
    async fn a_device_held_by_another_process_exits_3() {
        let (dir, config) = scratch("held");
        let id = config.device_id.clone().unwrap();
        let _held = crate::lease::try_lock(&id).unwrap();
        let driver = FlipDriver::new(&id, |_, _| true);
        let e = SuiteRunner::run(flip_suite(&["a"]), config, &driver)
            .await
            .unwrap_err();
        assert_eq!(e.exit_code(), 3, "{e}");
        assert!(e.to_string().contains("another Velocity process"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-337: a new test that passes once and then fails every early-flake
    // rerun is a failure, not "FLAKY (passed on retry 0)" with exit 0.
    #[tokio::test]
    async fn a_new_test_failing_every_rerun_fails_the_run() {
        let (dir, config) = scratch("efd");
        let mut hist = history::TestHistory::default();
        for n in ["a", "b", "c"] {
            hist.outcomes
                .insert(n.into(), vec![history::HistoryOutcome::Pass]);
        }
        history::save(&config.artifacts_dir, &hist).unwrap();
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |app, n| {
            app != "com.fresh" || n == 1
        });
        let config = RuntimeConfig {
            detect_new_flakes: true,
            ..config
        };
        let r = SuiteRunner::run(flip_suite(&["a", "b", "c", "fresh"]), config, &driver)
            .await
            .unwrap();
        let fresh = r.tests.iter().find(|t| t.test_name == "fresh").unwrap();
        assert_eq!(fresh.status, TestStatus::Failed, "{fresh:#?}");
        assert_eq!(r.exit_code(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-S10: the pass was cached before the early-flake runs found the
    // new test flaky, so the next run skipped it as a cached pass.
    #[tokio::test]
    async fn a_new_test_found_flaky_is_not_cached_as_a_pass() {
        let (dir, config) = scratch("efdcache");
        let mut hist = history::TestHistory::default();
        for n in ["a", "b", "c"] {
            hist.outcomes
                .insert(n.into(), vec![history::HistoryOutcome::Pass]);
        }
        history::save(&config.artifacts_dir, &hist).unwrap();
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |app, n| {
            app != "com.fresh" || n % 2 == 1
        });
        let config = RuntimeConfig {
            detect_new_flakes: true,
            cache_results: true,
            ..config
        };
        let suite = || flip_suite(&["a", "b", "c", "fresh"]);
        let first = SuiteRunner::run(suite(), config.clone(), &driver)
            .await
            .unwrap();
        let fresh = |r: &SuiteResult| r.tests.iter().find(|t| t.test_name == "fresh").cloned();
        assert_eq!(fresh(&first).unwrap().status, TestStatus::Retried);
        let second = SuiteRunner::run(suite(), config, &driver).await.unwrap();
        assert!(
            !fresh(&second).unwrap().from_cache,
            "a flaky test ran again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-324: hunt rounds never come from the result cache.
    #[tokio::test]
    async fn a_hunt_reruns_tests_the_cache_has_a_pass_for() {
        let (dir, config) = scratch("hunt");
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |_, n| n % 2 == 1);
        let config = RuntimeConfig {
            cache_results: true,
            hunt_rounds: Some(4),
            ..config
        };
        let report = crate::hunt::hunt(flip_suite(&["flip"]), config, &driver)
            .await
            .unwrap();
        assert_eq!(driver.launches("com.flip"), 4);
        assert_eq!(report.flaky().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-050: `--update-baselines` runs every test, cache or not.
    #[tokio::test]
    async fn updating_baselines_bypasses_the_result_cache() {
        let (dir, config) = scratch("baselines");
        let driver = FlipDriver::new(config.device_id.as_deref().unwrap(), |_, _| true);
        let config = RuntimeConfig {
            cache_results: true,
            ..config
        };
        let first = SuiteRunner::run(flip_suite(&["t"]), config.clone(), &driver)
            .await
            .unwrap();
        assert!(!first.tests[0].from_cache);
        let cached = SuiteRunner::run(flip_suite(&["t"]), config.clone(), &driver)
            .await
            .unwrap();
        assert!(cached.tests[0].from_cache, "control: the cache works");
        let update = RuntimeConfig {
            update_baselines: velocity_common::BaselineUpdate::Changed,
            ..config
        };
        let r = SuiteRunner::run(flip_suite(&["t"]), update, &driver)
            .await
            .unwrap();
        assert!(!r.tests[0].from_cache);
        assert_eq!(driver.launches("com.t"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two quick tests, then two that wait 3 s each.
    fn slow_suite() -> velocity_common::TestSuite {
        let mut suite = flip_suite(&["a", "b"]);
        let slow = velocity_core::parser::parse_suite_from_str(
            "appId: com.example\ntests:\n  - name: slow1\n    steps:\n      - wait: { ms: 3000 }\n  - name: slow2\n    steps:\n      - wait: { ms: 3000 }\n",
        )
        .unwrap();
        suite.tests.extend(slow.tests);
        suite
    }

    // VEL-044: `--timeout` stops the test that is running, and a timed-out
    // run still returns what ran (for the reports) and saves its history —
    // it used to wait for the test, then drop every result.
    #[tokio::test]
    async fn a_timed_out_run_keeps_its_results_and_skips_the_rest() {
        let (dir, config) = scratch("timeout");
        let driver = FlipDriver::new(&config.device_id.clone().unwrap(), |_, _| true);
        let config = RuntimeConfig {
            suite_timeout_ms: Some(1500),
            ..config
        };
        let started = Instant::now();
        let out = SuiteRunner::run_outcome(slow_suite(), config, &driver)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        match &out.termination {
            Termination::TimedOut(e) => assert_eq!(e.exit_code(), 4),
            other => panic!("{other:?}"),
        }
        let status = |n: &str| {
            out.result
                .tests
                .iter()
                .find(|t| t.test_name == n)
                .unwrap_or_else(|| panic!("{n} missing"))
                .clone()
        };
        assert_eq!(status("a").status, TestStatus::Passed);
        assert_eq!(status("b").status, TestStatus::Passed);
        let cut = status("slow1");
        assert_eq!(cut.status, TestStatus::Failed);
        assert!(cut.error_message.unwrap().contains("suite timeout"));
        let rest = status("slow2");
        assert_eq!(rest.status, TestStatus::Skipped);
        assert!(rest
            .error_message
            .unwrap()
            .contains("not run: suite timeout"));
        // History has the tests that finished, not the one cut off.
        let hist = history::load(&dir.to_string_lossy()).unwrap();
        assert!(hist.outcomes.contains_key("a") && !hist.outcomes.contains_key("slow1"));
        // `run` keeps its old contract: a timeout is an error.
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review 12 #9: `--test-timeout` fails the one slow test and the run
    /// goes on to the next, where `--timeout` ends the whole run.
    #[tokio::test]
    async fn a_test_past_its_test_timeout_fails_and_the_run_goes_on() {
        let (dir, config) = scratch("testtimeout");
        let driver = FlipDriver::new(&config.device_id.clone().unwrap(), |_, _| true);
        let config = RuntimeConfig {
            test_timeout_ms: Some(300),
            ..config
        };
        let out = SuiteRunner::run_outcome(slow_suite(), config, &driver)
            .await
            .unwrap();
        assert!(
            matches!(out.termination, Termination::Completed),
            "{:?}",
            out.termination
        );
        let get = |n: &str| {
            out.result
                .tests
                .iter()
                .find(|t| t.test_name == n)
                .unwrap_or_else(|| panic!("{n} missing"))
                .clone()
        };
        assert_eq!(get("a").status, TestStatus::Passed);
        for slow in ["slow1", "slow2"] {
            let t = get(slow);
            assert_eq!(t.status, TestStatus::Failed, "{slow}");
            assert!(
                t.error_message.unwrap().contains("--test-timeout"),
                "{slow}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-188: a stop request (Ctrl-C) ends the run at once with what ran;
    // the rest is reported as interrupted, not left out.
    #[tokio::test]
    async fn a_stopped_run_returns_what_ran() {
        let (dir, config) = scratch("stop");
        let driver = FlipDriver::new(&config.device_id.clone().unwrap(), |_, _| true);
        let stop = config.stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            stop.stop("interrupted by Ctrl-C");
        });
        let started = Instant::now();
        let out = SuiteRunner::run_outcome(slow_suite(), config, &driver)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(&out.termination, Termination::Stopped(r) if r.contains("Ctrl-C")));
        let r = &out.result;
        assert_eq!(r.tests.len(), 4);
        assert_eq!(r.passed, 2);
        assert_eq!(r.skipped, 2);
        assert!(r.tests[2]
            .error_message
            .as_deref()
            .unwrap()
            .contains("interrupted by Ctrl-C while it ran"));
        assert_eq!(
            r.tests[3].error_message.as_deref(),
            Some("not run: interrupted by Ctrl-C")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
