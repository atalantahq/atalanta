use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::HashMap;

use velocity_common::live_events;
use velocity_common::natural::{NaturalStep, NaturalStepResolver, PendingPlan};
use velocity_common::{
    Action, Direction, Element, NaturalKind, Platform, PlatformDriver, ResourceDelta, Result,
    Selector, Step, StepResult, StepStatus, SuiteConfig, TestCase, TestResult, TestStatus,
    VelocityError,
};

use crate::baselines::baseline_file;
use crate::dialog_handler::DialogHandler;
use crate::native_sync::SyncManager;
use crate::profiler::ResourceProfiler;
use crate::selector::SelectorEngine;

mod actions;
mod failure;
mod lookup;
mod natural;
mod screenshot;
#[allow(unused_imports)]
use actions::*;
#[allow(unused_imports)]
use failure::*;
#[allow(unused_imports)]
use lookup::*;
#[allow(unused_imports)]
use natural::*;
#[allow(unused_imports)]
use screenshot::*;

/// How long an `exec` step may run without its own `timeoutMs`.
const EXEC_TIMEOUT_MS: u64 = 60_000;
/// `drag`: the hold before moving (past the platforms' long-press time,
/// about 500 ms) and the move itself.
const DRAG_HOLD_MS: u64 = 700;
const DRAG_MOVE_MS: u64 = 900;

pub struct TestExecutor<'a> {
    driver: &'a dyn PlatformDriver,
    sync_manager: SyncManager,
    selector_engine: SelectorEngine,
    dialog_handler: DialogHandler,
    config: SuiteConfig,
    profiler: Option<ResourceProfiler>,
    /// Resolver for natural-language steps; `None` disables natural steps.
    planner: Option<Arc<dyn NaturalStepResolver>>,
    /// Name of the running test — scopes natural-step cache keys.
    current_test: String,
    /// Steps in the current test (for live events).
    current_steps: usize,
    /// How often each natural step has appeared in the running test, so the
    /// same text on two screens ("tap Next") keeps two cache entries.
    natural_seen: HashMap<String, u32>,
    /// Notes for the test report (re-planned natural steps).
    warnings: Vec<String>,
    /// Natural-language plans that worked but aren't verified yet, with
    /// the step that staged each. Committed to the cache once a later step
    /// or the test confirms them, dropped otherwise (see
    /// [`PendingPlan`]).
    nl_pending: Vec<(usize, PendingPlan)>,
    /// `--verify`: over every test's and the suite's `verify:`.
    verify_cli: Option<velocity_common::VerifyLevel>,
    /// The running test's verify level.
    verify_level: velocity_common::VerifyLevel,
    /// Index of the running step in its test.
    current_step: usize,
    /// The AI checks run in the running test.
    verifications: Vec<velocity_common::VerifyCheck>,
    /// Settle time accumulated by the current step.
    step_settle_ms: u64,
    /// `--update-baselines[=missing]`: which baselines assertScreenshot
    /// writes (D-V1).
    update_baselines: velocity_common::BaselineUpdate,
    /// The baselines this run created (D-V1). Its own unless the runner
    /// shares the run's, so nothing outlives a run (review-d5 M-1).
    created_baselines: velocity_common::CreatedBaselines,
    /// Per device: whether `screenshots.freezeStatusBar` froze its status
    /// bar (asked once, D-V4).
    status_bar_frozen: HashMap<String, bool>,
    /// Which attempt of the current test this is (0 = first run).
    retry_attempt: u32,
    /// Batching (`config.batch`): the app this executor's last test left
    /// running after passing, so the next test's plain `launchApp` of it
    /// can be skipped.
    app_left_running: Option<String>,
    /// The suite's app, whose crash log a long element wait checks.
    app_id: String,
    /// Sync config and settings before a `setConfig`, restored at test end.
    config_before: Option<(
        velocity_common::SyncConfig,
        std::collections::HashMap<String, String>,
    )>,
    /// The running step's `settle:` override.
    step_settle: Option<velocity_common::StepSettle>,
    /// The running step's `force: true` (D-3).
    step_force: bool,
    /// What a heal does here: `healing.mode`, resolved (D-7).
    heal_mode: velocity_common::HealingMode,
    /// The suite file of the running step (for the `velocity heal` hint).
    suite_file: Option<String>,
    /// The running step's `appInFront` (D-2).
    step_app_in_front: Option<bool>,
    /// This test left the suite's app on purpose (openLink/goto, another
    /// app's launchApp, the home key) and hasn't launched it since: a
    /// negative check off-app is then expected (D-2).
    app_switched: bool,
    /// The sync timings before the running step's `settle:` override, put
    /// back after it (and by the next step or test when the watchdog
    /// cancelled it midway).
    step_sync_before: Option<velocity_common::SyncConfig>,
    /// This test added mocks (`mockRequest`), dropped when it ends.
    mocked: bool,
    /// The running test's `http` steps: one cookie jar, made at its first
    /// one and dropped when the next test starts.
    http: Option<velocity_api::HttpSession>,
    /// What the running test's `http` steps captured, for `${NAME}` in its
    /// later steps.
    captured: HashMap<String, String>,
    /// Screenshots held until the test ends: written only if it fails.
    held_shots: Vec<(PathBuf, Vec<u8>)>,
    /// The running test's video and log capture (`artifacts.video`/`logs`).
    evidence: Option<Box<dyn velocity_common::EvidenceSession>>,
    /// assertScreenshot baseline file → the name that first used it.
    baseline_names: HashMap<String, String>,
    /// The last `find_actionable` target was found only in the widest
    /// (merged-label) tier.
    loose_found: bool,
    /// This step's pre-action settle ended on a screen seen to hold still
    /// (not capped), and no lookup has used that yet: the first
    /// `find_actionable` then needs no second read (review M4).
    settled_for_lookup: bool,
    /// The last `find_element_smart` had to wait (missed at least once):
    /// the screen changed after the settle, so its verdict is stale.
    lookup_waited: bool,
    /// This step already dragged to bring a `within:` container in.
    area_dragged: bool,
    /// The running step's `timeoutMs`, overriding the sync timeout for
    /// element waits and assertions.
    step_timeout_ms: Option<u64>,
    /// Flaky hunting: (max delay ms, PRNG state) — a random pause before each
    /// step shifts the timing between test and app, which exposes races that
    /// plain reruns rarely hit.
    jitter: Option<(u64, u64)>,
    /// Whether the last failing step failed for infrastructure reasons
    /// (connection lost, device not ready) rather than in the app.
    last_failure_infra: bool,
    /// The last failed step's error was `AppCrashed`.
    last_failure_crashed: bool,
    /// The kind of the error behind the last failed step (MCP step tools
    /// report it).
    last_failure_kind: Option<velocity_common::ErrorKind>,
    /// The test stopped the suite's app (`stopApp`) and hasn't launched it
    /// since: its absence is expected, not a crash.
    app_stopped: bool,
    /// The suite's app's liveness, read since the last action (a negative
    /// assertion reads it once, not once per assertion).
    liveness_seen: Option<velocity_common::Liveness>,
    /// I3 (D-50, sticky crash): the crash the driver reported for the
    /// suite's app in this test ([`PlatformDriver::pending_crash`]). Once
    /// set, every later step fails with it at once, until the app is
    /// launched again.
    sticky_crash: Option<String>,
    /// Strict selectors, when set here rather than by the `selectors.strict`
    /// setting.
    strict_selectors: Option<bool>,
    /// The running test's trace, kept on failure (`--trace`).
    trace: crate::trace::TraceRecorder,
    /// The accessibility audit (`accessibility.audit`), `None` when off.
    a11y: Option<crate::a11y_run::AuditRun>,
}

/// Attempts to plan a natural step whose target isn't on screen yet (the
/// screen may still be loading). `waitUntil` conditions get more, since they
/// describe what the app is about to show.
const NOT_FOUND_ATTEMPTS: u32 = 2;
/// Stop the whole run the way Ctrl-C does (`q` at a pause): reports are
/// still written, and the run exits 130.
fn interrupt_run() {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-INT", &std::process::id().to_string()])
            .status();
    }
}

/// Smallest watchdog deadline for a step (tiny in tests so hangs are quick).
#[cfg(not(test))]
const STEP_DEADLINE_FLOOR_MS: u64 = 30_000;

#[cfg(test)]
const STEP_DEADLINE_FLOOR_MS: u64 = 300;
const WAIT_UNTIL_ATTEMPTS: u32 = 4;
/// Longest a failure screenshot or tree read may take (they run after the
/// step's watchdog, often on a device that just hung).
#[cfg(not(test))]
const FAILURE_ARTIFACT_BUDGET: Duration = Duration::from_secs(5);
#[cfg(test)]
const FAILURE_ARTIFACT_BUDGET: Duration = Duration::from_millis(200);
const NOT_FOUND_RETRY_DELAY: Duration = Duration::from_millis(1000);
/// Longest a driver may take to start a test's video and log capture
/// (the web driver may launch its browser).
const EVIDENCE_START_BUDGET: Duration = Duration::from_secs(20);
/// Longest writing a test's video and logs may take (muxing, a pull).
const EVIDENCE_FINISH_BUDGET: Duration = Duration::from_secs(60);

/// `--pause-on-failure`: stop at the first failing step and wait for the
/// user. Set by the CLI only for an interactive terminal.
static PAUSE_ON_FAILURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// One pause prompt at a time (parallel devices).
static PAUSE_PROMPT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Turn `--pause-on-failure` on or off for this process.
pub fn set_pause_on_failure(on: bool) {
    PAUSE_ON_FAILURE.store(on, std::sync::atomic::Ordering::SeqCst);
}

fn pause_on_failure() -> bool {
    PAUSE_ON_FAILURE.load(std::sync::atomic::Ordering::SeqCst)
}

/// What the user chose at a `--pause-on-failure` prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseChoice {
    /// Enter: let the step fail and go on.
    Continue,
    /// `r`: run the step again.
    Retry,
    /// `q`: stop the run (as Ctrl-C does: reports are still written).
    Abort,
}

/// The prompt's answer: Enter continues, `r` retries, `q` aborts.
pub fn parse_pause_answer(line: &str) -> PauseChoice {
    match line.trim().to_ascii_lowercase().as_str() {
        "r" | "retry" => PauseChoice::Retry,
        "q" | "quit" | "abort" => PauseChoice::Abort,
        _ => PauseChoice::Continue,
    }
}

/// The step index in a held step screenshot's name (`step07_tap.png`).
fn held_step_index(path: &std::path::Path) -> Option<usize> {
    let name = path.file_name()?.to_str()?;
    let digits: String = name
        .strip_prefix("step")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Longest wait for a found target to become enabled and stop moving.
fn actionable_wait() -> Duration {
    velocity_common::settings::ms("VELOCITY_ACTIONABLE_WAIT_MS", 2000)
}

/// The same box give or take 1 px per edge: a spring's fractional frames
/// round either way between reads (review m1; the page's own check allows
/// 0.5 CSS px).
fn bounds_close(a: &velocity_common::Rect, b: &velocity_common::Rect) -> bool {
    (a.x - b.x).abs() <= 1
        && (a.y - b.y).abs() <= 1
        && (a.width - b.width).abs() <= 1
        && (a.height - b.height).abs() <= 1
}

/// Delay before lookup retry `n` (Playwright's schedule): quick at first,
/// for elements a frame away, then 500 ms.
fn retry_delay(n: usize) -> Duration {
    const SCHEDULE: [u64; 6] = [0, 20, 50, 100, 100, 250];
    Duration::from_millis(SCHEDULE.get(n + 1).copied().unwrap_or(500))
}

/// The pause before lookup retry `n`, or None once `deadline` has passed.
/// Capped at what is left, so the last check lands on the deadline (the
/// loops used to stop when the next back-off step overshot it, leaving up to
/// 500 ms of every timeout unused), and at a fifth of `timeout`, so a short
/// timeout still gets several checks (P7).
fn next_pause(n: usize, deadline: Instant, timeout: Duration) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return None;
    }
    let cap = (timeout / 5).max(Duration::from_millis(20));
    Some(retry_delay(n).min(cap).min(left))
}

/// Whether `setConfig` can change `key` to `value` mid-test: a named
/// setting, or one of the sync timings.
pub fn check_set_config(key: &str, value: &str) -> std::result::Result<(), String> {
    if velocity_common::settings::find(key).is_some() {
        return Ok(());
    }
    let mut probe = velocity_common::SyncConfig::default();
    set_sync_field(&mut probe, key, value)
}

/// When the post-action settle starts watching the crash log, and how often.
fn crash_watch_after() -> Duration {
    velocity_common::settings::ms("VELOCITY_CRASH_WATCH_AFTER_MS", 1000)
}
fn crash_watch_every() -> Duration {
    velocity_common::settings::ms("VELOCITY_CRASH_WATCH_EVERY_MS", 500)
}

/// How often a waiting element lookup checks whether the app crashed.
fn crash_check_every() -> Duration {
    velocity_common::settings::ms("VELOCITY_CRASH_CHECK_EVERY_MS", 1000)
}

impl<'a> TestExecutor<'a> {
    pub fn new(driver: &'a dyn PlatformDriver, mut config: SuiteConfig, app_id: &str) -> Self {
        let platform = config.platform.unwrap_or(Platform::Ios);
        // D-7: `report` in CI, `auto` locally, unless the suite says.
        let heal_mode = config.healing.effective_mode(in_ci_env());
        // On the web `web.settle` wins over `sync.*` (SUM-4). Folded into the
        // stored `config.sync` itself, so the resets after `setConfig` and at
        // the end of each test (`restore_test_config`) keep it.
        if platform == Platform::Web {
            if let Some(ws) = config.web.as_ref().and_then(|w| w.settle.clone()) {
                let sync = &mut config.sync;
                sync.reaction_ms = ws.reaction_ms.or(sync.reaction_ms);
                sync.quiet_ms = ws.quiet_ms.or(sync.quiet_ms);
                sync.settle_cap_ms = ws.cap_ms.or(sync.settle_cap_ms);
            }
        }
        let sync_manager = SyncManager::new(&config.sync, platform)
            .for_app(app_id)
            .with_framework(config.framework.unwrap_or_default());
        let selector_engine =
            SelectorEngine::with_healing_for(config.healing.clone(), platform, app_id);
        let dialog_handler = DialogHandler::new(config.dialog.clone()).for_app(app_id);
        let profiler = if config.performance.enabled {
            Some(ResourceProfiler::new(app_id.to_string()))
        } else {
            None
        };
        let trace = crate::trace::TraceRecorder::new(trace_mode(
            velocity_common::TraceMode::default(),
            &config,
        ));
        let a11y = crate::a11y_run::AuditRun::new(&config.accessibility);
        Self {
            driver,
            sync_manager,
            selector_engine,
            dialog_handler,
            config,
            profiler,
            planner: None,
            current_test: String::new(),
            current_steps: 0,
            natural_seen: HashMap::new(),
            warnings: Vec::new(),
            nl_pending: Vec::new(),
            verify_cli: None,
            verify_level: velocity_common::VerifyLevel::Off,
            current_step: 0,
            verifications: Vec::new(),
            step_settle_ms: 0,
            update_baselines: velocity_common::BaselineUpdate::Off,
            created_baselines: Default::default(),
            status_bar_frozen: HashMap::new(),
            retry_attempt: 0,
            app_left_running: None,
            app_id: app_id.to_string(),
            config_before: None,
            step_settle: None,
            step_force: false,
            heal_mode,
            suite_file: None,
            step_app_in_front: None,
            app_switched: false,
            step_sync_before: None,
            held_shots: Vec::new(),
            evidence: None,
            baseline_names: HashMap::new(),
            loose_found: false,
            settled_for_lookup: false,
            lookup_waited: false,
            area_dragged: false,
            mocked: false,
            http: None,
            captured: HashMap::new(),
            step_timeout_ms: None,
            jitter: None,
            last_failure_infra: false,
            last_failure_crashed: false,
            last_failure_kind: None,
            app_stopped: false,
            liveness_seen: None,
            sticky_crash: None,
            strict_selectors: None,
            trace,
            a11y,
        }
    }

    /// Keep the self-healing state (healed mappings, element snapshots,
    /// rejected heals) in `dir` instead of `./.velocity`. Tests use a temp
    /// dir: the default writes into whatever directory the test runs in,
    /// and what an earlier run left there changes how many trees the first
    /// lookup of a selector reads.
    pub fn with_state_dir(mut self, dir: &std::path::Path) -> Self {
        let platform = self.config.platform.unwrap_or(Platform::Ios);
        self.selector_engine = SelectorEngine::with_healing_in(
            self.config.healing.clone(),
            platform,
            &self.app_id,
            dir,
        );
        self
    }

    /// When to keep test traces (`--trace`; default: failed attempts).
    pub fn with_trace(mut self, mode: velocity_common::TraceMode) -> Self {
        let started = self.trace.run_started();
        self.trace = crate::trace::TraceRecorder::new(trace_mode(mode, &self.config))
            .with_run_started(started);
        self
    }

    /// The start of the run this executor serves: a pass makes only the
    /// traces written before it stale (VEL-205). Defaults to when the
    /// executor was built.
    pub fn with_run_started(mut self, at: std::time::SystemTime) -> Self {
        self.trace.set_run_started(at);
        self
    }

    /// Name part for the trace file of the next tests, for concurrent copies
    /// of one test (hunt rounds, preventive copies).
    pub fn set_trace_tag(&mut self, tag: Option<String>) {
        self.trace.set_tag(tag);
    }

    /// Keep the tree the driver just used for a lookup, for the trace and
    /// the accessibility audit. Only from a driver that caches its tree,
    /// where the read is a copy, not a device call.
    /// `from_tree`: the lookup matched on the driver's tree. A WDA query's
    /// hit (no bounds) came without one, and reading it would be a dump,
    /// which the audit never adds.
    async fn trace_cached_tree(&mut self, device_id: &str, from_tree: bool) {
        let audit = self.a11y.is_some() && from_tree;
        if !(self.trace.enabled() || audit) || !self.driver.has_hierarchy_cache() {
            return;
        }
        if let Ok(tree) = self.driver.get_hierarchy_cached(device_id).await {
            if self.trace.enabled() {
                self.trace.record_tree(&tree);
            }
            if audit {
                self.audit_screen(device_id, &tree);
            }
        }
    }

    /// Audit a tree the run has already read (`accessibility.audit`): once
    /// per distinct screen per test, its new findings as warnings. Never
    /// reads the screen itself.
    fn audit_screen(&mut self, device_id: &str, tree: &Element) {
        let Some(run) = self.a11y.as_mut() else {
            return;
        };
        let platform = self
            .config
            .platform
            .unwrap_or_else(|| Platform::of_device_id(device_id));
        let found = run.screen(tree, platform);
        self.warnings.extend(found);
    }

    /// Settle reads of the sync that just ran, for the trace.
    fn trace_settle(&mut self) {
        let reads = self.sync_manager.last_settle().reads;
        self.trace.record_settle_reads(reads);
    }

    /// Close a step in the trace. A failed step also gets the screen it
    /// failed on (one read, like the failure screenshot) and the elements
    /// nearest to its selector.
    async fn trace_step_end(
        &mut self,
        device_id: &str,
        result: &StepResult,
        (warnings_from, heals_from): (usize, usize),
    ) {
        live_events::step_finished(&self.current_test, self.current_steps, result);
        if !self.trace.enabled() {
            return;
        }
        // Not after a crash: the app's tree is gone (the launcher is in
        // front), and the dump waited out the window change (3.6 s of a 5.8 s
        // crash test). The failure screenshot shows the launcher anyway.
        if result.status == StepStatus::Failed && !self.last_failure_crashed {
            let read = self.driver.get_hierarchy_cached(device_id);
            if let Ok(Ok(tree)) = tokio::time::timeout(FAILURE_ARTIFACT_BUDGET, read).await {
                self.trace.record_tree(&tree);
                self.audit_screen(device_id, &tree);
            }
        }
        let warnings = self
            .warnings
            .get(warnings_from..)
            .unwrap_or_default()
            .to_vec();
        let heals = self
            .selector_engine
            .heal_notes()
            .get(heals_from..)
            .unwrap_or_default()
            .to_vec();
        self.trace.end_step(result, heals, warnings);
        if result.status == StepStatus::Failed {
            self.trace.fill_nearest(self.config.platform);
        }
    }

    /// Pause a random 0..`max_ms` before each step (seeded, reproducible).
    /// Tell the executor which attempt of a test runs next (0 = first). On
    /// a retry, natural steps re-plan instead of replaying the cache.
    pub fn set_retry_attempt(&mut self, attempt: u32) {
        self.retry_attempt = attempt;
    }

    /// Fail an action whose selector matches several elements (Playwright's
    /// strict mode) instead of acting on the first with a warning. Overrides
    /// the `selectors.strict` setting.
    pub fn with_strict_selectors(mut self, on: bool) -> Self {
        self.strict_selectors = Some(on);
        self
    }

    fn strict_selectors(&self) -> bool {
        self.strict_selectors
            .unwrap_or_else(|| velocity_common::settings::num("VELOCITY_SELECTORS_STRICT", 0) == 1)
    }

    /// Whether `selectors.strict` was left unset (S3): an ambiguous action
    /// then warns that it becomes an error in 1.0.
    fn strict_selectors_unset(&self) -> bool {
        self.strict_selectors.is_none()
            && velocity_common::env::var("VELOCITY_SELECTORS_STRICT")
                .map_or(true, |v| v.trim().is_empty())
    }

    /// Which assertScreenshot baselines to write: `true` / `Changed`
    /// (every missing or changed one), `Missing` (only missing ones), or
    /// `false` / `Off` (D-V1).
    pub fn with_update_baselines(
        mut self,
        mode: impl Into<velocity_common::BaselineUpdate>,
    ) -> Self {
        self.update_baselines = mode.into();
        self
    }

    /// Share the run's created-baselines registry (D-V1): a retry or
    /// another device of the same run fails on a baseline this run wrote;
    /// the next run starts fresh.
    pub fn with_created_baselines(mut self, run: velocity_common::CreatedBaselines) -> Self {
        self.created_baselines = run;
        self
    }

    pub fn with_jitter(mut self, max_ms: u64, seed: u64) -> Self {
        self.set_jitter(max_ms, seed);
        self
    }

    /// Change the jitter between tests: each parallel hunt round has its
    /// own seed, and one warm executor runs copies from several rounds.
    pub fn set_jitter(&mut self, max_ms: u64, seed: u64) {
        self.jitter = (max_ms > 0).then_some((max_ms, seed ^ 0x5DEE_CE66_D1CE_4E5B));
    }

    async fn pause_for_jitter(&mut self) {
        if let Some((max, state)) = self.jitter.as_mut() {
            // xorshift64*
            *state ^= *state >> 12;
            *state ^= *state << 25;
            *state ^= *state >> 27;
            let r = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            tokio::time::sleep(Duration::from_millis(r % (*max + 1))).await;
        }
    }

    /// Attach a natural-language resolver so `do:`/`assert:` and bare-string steps
    /// can be resolved to concrete actions at run time.
    pub fn with_planner(mut self, planner: Arc<dyn NaturalStepResolver>) -> Self {
        self.planner = Some(planner);
        self
    }

    /// `--verify`: the AI verification level for every test, over the
    /// tests' and the suite's `verify:`.
    pub fn with_verify(mut self, level: Option<velocity_common::VerifyLevel>) -> Self {
        self.verify_cli = level;
        self
    }

    pub async fn execute_test(
        &mut self,
        test: &TestCase,
        device_id: &str,
        app_id: &str,
    ) -> Result<TestResult> {
        use tracing::Instrument;
        let span = tracing::info_span!("test", name = %test.name, device = device_id);
        live_events::test_started(&test.name, device_id, test.steps.len());
        let result = self
            .execute_test_inner(test, device_id, app_id)
            .instrument(span)
            .await;
        live_events::test_finished(&test.name, result.as_ref().map_err(|e| e.to_string()));
        self.restore_test_config();
        if std::mem::take(&mut self.mocked) {
            let clear = self.driver.clear_mocks(device_id);
            let _ = tokio::time::timeout(FAILURE_ARTIFACT_BUDGET, clear).await;
        }
        result
    }

    /// A failed `optional: true` step, as a pass with a warning (D-12),
    /// unless it failed for the device or a crash. `block` is the optional
    /// `runFlow` the step fails inside (when the step isn't optional
    /// itself), with how many of its steps are left: they are skipped, so
    /// the one warning says so.
    fn forgive_optional(&mut self, mut r: StepResult, block: Option<(&str, usize)>) -> StepResult {
        if self.last_failure_infra || self.last_failure_crashed {
            return r;
        }
        let why = r.error_message.clone().unwrap_or_default();
        match block {
            Some((flow, left)) => self.warnings.push(format!(
                "optional flow {flow} stopped at {}: {why}; its remaining {left} step(s) \
                 skipped",
                r.action_name
            )),
            None => self
                .warnings
                .push(format!("optional step {} failed: {why}", r.action_name)),
        }
        r.status = StepStatus::Passed;
        r.action_name = format!("{} (optional, failed)", r.action_name);
        // Its failure screenshot is no evidence for this test's result, and
        // nothing links to it any more (n2).
        if let Some(shot) = r.screenshot.take() {
            let _ = std::fs::remove_file(shot);
        }
        self.last_failure_infra = false;
        self.last_failure_crashed = false;
        r
    }

    /// Undo a test's `setConfig`: the next test starts from the suite's own
    /// sync timings and settings.
    fn restore_test_config(&mut self) {
        if let Some((sync, settings)) = self.config_before.take() {
            self.config.sync = sync;
            *self.sync_manager.sync_config_mut() = self.config.sync.clone();
            velocity_common::env::set_settings(settings);
        }
    }

    async fn execute_test_inner(
        &mut self,
        test: &TestCase,
        device_id: &str,
        app_id: &str,
    ) -> Result<TestResult> {
        let test_start = Instant::now();
        self.current_test = test.name.clone();
        self.current_steps = test.steps.len();
        self.natural_seen.clear();
        self.warnings.clear();
        self.nl_pending.clear();
        self.verifications.clear();
        self.verify_level = velocity_common::VerifyLevel::effective(
            self.verify_cli,
            test.verify,
            self.config.verify,
        );
        // Failed AI checks in a non-strict run: the test fails at the end.
        let mut verify_failed = false;
        self.last_failure_infra = false;
        self.last_failure_crashed = false;
        self.app_stopped = false;
        self.app_switched = false;
        // Captured values and cookies belong to one test: a retry, a shard
        // or a shuffled order never sees another test's.
        self.http = None;
        self.captured.clear();
        self.liveness_seen = None;
        self.sticky_crash = None;
        // Nothing found by an earlier test describes this one's screen.
        self.selector_engine.invalidate_cache();
        let _ = self.selector_engine.take_heal_notes();
        self.trace.begin_test();
        if let Some(run) = self.a11y.as_mut() {
            run.begin_test();
        }
        self.held_shots.clear();
        self.evidence = None;
        {
            let arts = &self.config.artifacts;
            if arts.video.on() || arts.logs.on() {
                let req = velocity_common::EvidenceRequest {
                    test: test.name.clone(),
                    app_id: app_id.to_string(),
                    video: arts.video,
                    video_seconds: arts.video_seconds,
                    logs: arts.logs,
                    started: self.trace.test_started(),
                };
                let start = self.driver.start_evidence(device_id, &req);
                match tokio::time::timeout(EVIDENCE_START_BUDGET, start).await {
                    Ok(session) => self.evidence = session,
                    Err(_) => tracing::warn!("video/log capture did not start in time"),
                }
            }
        }
        let mut step_results = Vec::with_capacity(test.steps.len());
        let mut screenshots = Vec::new();
        let mut test_error: Option<String> = None;

        // Batching: the previous test on this device passed with this app in
        // front, so a plain launch would only bring it forward (Android) or
        // cold-restart it (iOS). Skip it; `clearState` launches always run,
        // and so does an `isolated: true` test's (it wants a fresh app).
        let skip_launch = self.config.batch
            && !test.isolated
            && self.retry_attempt == 0
            && matches!(
                test.steps.first().map(|s| &s.action),
                Some(Action::LaunchApp {
                    app_id: a,
                    clear_state: false,
                    ..
                })
                    if self.app_left_running.as_deref() == Some(if a.is_empty() { app_id } else { a.as_str() })
            );
        self.app_left_running = None;

        // Optional `runFlow` blocks abandoned at a failure (M2).
        let mut abandoned: Vec<u32> = Vec::new();
        for (i, step) in test.steps.iter().enumerate() {
            if step
                .modifiers
                .optional_flows
                .iter()
                .any(|(b, _)| abandoned.contains(b))
            {
                step_results.push(StepResult {
                    step_index: i,
                    action_name: format!("{} (optional flow abandoned)", action_name(&step.action)),
                    status: StepStatus::Skipped,
                    duration: Duration::ZERO,
                    screenshot: None,
                    error_message: None,
                    resource_delta: None,
                    settle_ms: None,
                    source: step.source.clone(),
                });
                continue;
            }
            let step_start = Instant::now();
            live_events::step_started(&test.name, i, test.steps.len(), &step_label(&step.action));
            self.trace.begin_step(
                i,
                step_label(&step.action),
                action_selector(&step.action).cloned(),
            );
            let marks = (self.warnings.len(), self.selector_engine.heal_notes().len());
            if i == 0 && skip_launch {
                step_results.push(StepResult {
                    step_index: 0,
                    action_name: format!(
                        "{} (batched: app already running)",
                        action_name(&step.action)
                    ),
                    status: StepStatus::Passed,
                    duration: Duration::ZERO,
                    screenshot: None,
                    error_message: None,
                    resource_delta: None,
                    settle_ms: Some(0),
                    source: step.source.clone(),
                });
                if let Some(r) = step_results.last().cloned() {
                    self.trace_step_end(device_id, &r, marks).await;
                }
                continue;
            }
            // Watchdog: no step may hang the run. A timed-out step is cancelled
            // (its subprocesses are killed on drop) and fails as StepTimeout.
            // If the very first step hangs, the app never got going: that's
            // the device, not the test.
            let step_result = loop {
                let deadline = self.step_deadline(step);
                let r = match tokio::time::timeout(
                    deadline,
                    self.execute_step(step, device_id, app_id, i),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        self.restore_step_overrides();
                        self.last_failure_infra = i == 0;
                        Err(VelocityError::StepTimeout {
                            test_name: test.name.clone(),
                            step_index: i,
                            timeout_ms: deadline.as_millis() as u64,
                        })
                    }
                };
                // `--pause-on-failure`: a failure the test won't forgive
                // stops here, with the device as it is.
                let failed = r.as_ref().map_or(true, |x| x.status == StepStatus::Failed);
                let forgivable =
                    step.modifiers.optional || !step.modifiers.optional_flows.is_empty();
                if failed && !forgivable && pause_on_failure() {
                    let ended = match &r {
                        Ok(x) => x.clone(),
                        Err(e) => StepResult {
                            step_index: i,
                            action_name: action_name(&step.action),
                            status: StepStatus::Failed,
                            duration: step_start.elapsed(),
                            screenshot: None,
                            error_message: Some(located(step, e)),
                            resource_delta: None,
                            settle_ms: None,
                            source: step.source.clone(),
                        },
                    };
                    self.trace_step_end(device_id, &ended, marks).await;
                    match self.pause_at_failure(device_id, &test.name, &ended).await {
                        PauseChoice::Retry => {
                            self.trace.begin_step(
                                i,
                                step_label(&step.action),
                                action_selector(&step.action).cloned(),
                            );
                            continue;
                        }
                        PauseChoice::Abort => interrupt_run(),
                        PauseChoice::Continue => {}
                    }
                }
                break r;
            };

            if let Ok(result) = &step_result {
                self.trace_step_end(device_id, result, marks).await;
                if self.config.artifacts.step_screenshots && result.status == StepStatus::Passed {
                    self.hold_step_screenshot(device_id, i, &result.action_name)
                        .await;
                }
            }
            // `optional: true` (D-12): the step's failure is a warning and
            // the test goes on. Inside an optional `runFlow`, the rest of
            // that flow is skipped (M2). A lost device, a crashed app or a
            // step the watchdog cut off (a wedged device, m3) is not the
            // step's to forgive.
            let block = if step.modifiers.optional {
                None
            } else {
                step.modifiers.optional_flows.last().map(|(b, flow)| {
                    let left = test.steps[i + 1..]
                        .iter()
                        .filter(|s| s.modifiers.optional_flows.iter().any(|(x, _)| x == b))
                        .count();
                    (*b, flow.as_str(), left)
                })
            };
            let forgivable = step.modifiers.optional || block.is_some();
            let step_result = match step_result {
                Ok(r) if r.status == StepStatus::Failed && forgivable => {
                    Ok(self.forgive_optional(r, block.map(|(_, f, n)| (f, n))))
                }
                Err(e) if forgivable && !matches!(e, VelocityError::StepTimeout { .. }) => {
                    let r = StepResult {
                        step_index: i,
                        action_name: action_name(&step.action),
                        status: StepStatus::Failed,
                        duration: step_start.elapsed(),
                        screenshot: None,
                        error_message: Some(located(step, &e)),
                        resource_delta: None,
                        settle_ms: None,
                        source: step.source.clone(),
                    };
                    if self.last_failure_infra {
                        Err(e)
                    } else {
                        self.trace_step_end(device_id, &r, marks).await;
                        Ok(self.forgive_optional(r, block.map(|(_, f, n)| (f, n))))
                    }
                }
                other => other,
            };
            if let (Some((b, _, _)), Ok(r)) = (block, &step_result) {
                if r.status == StepStatus::Passed && r.action_name.ends_with("(optional, failed)") {
                    abandoned.push(b);
                }
            }
            // A step that really passed (not a forgiven failure) confirms
            // the plans staged before it; checkpoints run after it.
            let step_result = match step_result {
                Ok(mut r)
                    if r.status == StepStatus::Passed
                        && !r.action_name.ends_with("(optional, failed)") =>
                {
                    if !self.verify_level.checks_final() {
                        self.promote_pending(Some(i)).await;
                    }
                    if let Some(claim) = self.checkpoint_claim(step) {
                        let check = self
                            .verify_screen(device_id, &format!("step {i}"), &claim)
                            .await;
                        if !check.outcome.passed() {
                            if self.verify_level.is_strict() {
                                r.status = StepStatus::Failed;
                                r.error_message = Some(check.summary());
                            } else {
                                verify_failed = true;
                            }
                        }
                        self.verifications.push(check);
                    }
                    Ok(r)
                }
                other => other,
            };
            match step_result {
                Ok(result) => {
                    if result.status == StepStatus::Failed {
                        if let Some(ref path) = result.screenshot {
                            screenshots.push(path.clone());
                        }
                        test_error = result.error_message.clone();
                        step_results.push(result);
                        // Mark remaining steps as skipped
                        for j in (i + 1)..test.steps.len() {
                            step_results.push(StepResult {
                                step_index: j,
                                action_name: action_name(&test.steps[j].action),
                                status: StepStatus::Skipped,
                                duration: Duration::ZERO,
                                screenshot: None,
                                error_message: None,
                                resource_delta: None,
                                settle_ms: None,
                                source: test.steps[j].source.clone(),
                            });
                        }
                        break;
                    }
                    step_results.push(result);
                }
                Err(e) => {
                    let error_msg = located(step, &e);

                    // Take failure screenshot if configured
                    let screenshot = if self.config.artifacts.on_failure {
                        self.take_failure_screenshot(device_id, &test.name, i).await
                    } else {
                        None
                    };
                    if let Some(ref path) = screenshot {
                        screenshots.push(path.clone());
                    }

                    test_error = Some(error_msg.clone());
                    let failed = StepResult {
                        step_index: i,
                        action_name: action_name(&step.action),
                        status: StepStatus::Failed,
                        duration: step_start.elapsed(),
                        screenshot,
                        error_message: Some(error_msg),
                        resource_delta: None,
                        settle_ms: None,
                        source: step.source.clone(),
                    };
                    self.trace_step_end(device_id, &failed, marks).await;
                    step_results.push(failed);

                    // Mark remaining steps as skipped
                    for j in (i + 1)..test.steps.len() {
                        step_results.push(StepResult {
                            step_index: j,
                            action_name: action_name(&test.steps[j].action),
                            status: StepStatus::Skipped,
                            duration: Duration::ZERO,
                            screenshot: None,
                            error_message: None,
                            resource_delta: None,
                            settle_ms: None,
                            source: test.steps[j].source.clone(),
                        });
                    }
                    break;
                }
            }
        }

        // A crash on the last step, or one that a fast-settling launcher hid
        // from the post-action watch, passed the test: no step failed, so the
        // crash log was never read. Read it once now (afresh: a crash after
        // an earlier check counts too).
        let mut in_front = true;
        if !step_results.is_empty() && step_results.iter().all(|r| r.status == StepStatus::Passed) {
            self.liveness_seen = None;
            let dead = self.check_alive(device_id).await;
            // Another app in front: a batched next test must launch ours.
            in_front = !matches!(
                self.liveness_seen,
                Some(velocity_common::Liveness::Background(_))
            );
            if let Some(err) = dead {
                let msg = format!("after the last step: {err}");
                self.last_failure_crashed = true;
                if let Some(last) = step_results.last_mut() {
                    last.status = StepStatus::Failed;
                    last.error_message = Some(msg.clone());
                }
                test_error = Some(msg);
            }
        }

        // `verify:` final: the test's goal, on the last screen, once every
        // step passed.
        if self.verify_level.checks_final()
            && !step_results.is_empty()
            && step_results.iter().all(|r| r.status != StepStatus::Failed)
        {
            let goal = test
                .description
                .as_deref()
                .unwrap_or(&test.name)
                .to_string();
            let check = self.verify_screen(device_id, "final", &goal).await;
            if !check.outcome.passed() {
                verify_failed = true;
            }
            self.verifications.push(check);
        }
        if verify_failed {
            let failed: Vec<String> = self
                .verifications
                .iter()
                .filter(|c| !c.outcome.passed())
                .map(|c| c.summary())
                .collect();
            let msg = format!("AI verification failed: {}", failed.join("; "));
            if let Some(last) = step_results
                .iter_mut()
                .rev()
                .find(|r| r.status == StepStatus::Passed)
            {
                last.status = StepStatus::Failed;
                last.error_message = Some(msg.clone());
            }
            test_error.get_or_insert(msg);
        }
        // `accessibility: { fail: true }`: an error finding fails a test
        // whose steps passed, on its last step, as a failed AI check does.
        if let Some(run) = &self.a11y {
            if let Some(note) = run.left_out_note() {
                self.warnings.push(note);
            }
            let passed = step_results.iter().all(|r| r.status != StepStatus::Failed);
            if let Some(msg) = run.failure().filter(|_| passed) {
                if let Some(last) = step_results
                    .iter_mut()
                    .rev()
                    .find(|r| r.status == StepStatus::Passed)
                {
                    last.status = StepStatus::Failed;
                    last.error_message = Some(msg.clone());
                }
                test_error.get_or_insert(msg);
            }
        }

        // A step skipped with an abandoned optional flow (M2) is no
        // failure; every other skip follows a failed step.
        let all_passed = step_results.iter().all(|r| r.status != StepStatus::Failed);
        // The cache contract: plans still pending are committed only when
        // the whole test passed (checks included), and dropped otherwise.
        if all_passed {
            self.promote_pending(None).await;
        } else if !self.nl_pending.is_empty() {
            tracing::info!(
                plans = self.nl_pending.len(),
                "velocity-nl: test failed; not caching its unverified plans"
            );
            self.nl_pending.clear();
        }
        // Stopped at any point (not just the last step) and not launched
        // again, or another app left in front: the next test must launch.
        if all_passed && !self.app_stopped && in_front {
            self.app_left_running = Some(app_id.to_string());
        }
        let status = if all_passed {
            TestStatus::Passed
        } else {
            TestStatus::Failed
        };

        // Held screenshots are evidence for a failure only.
        let held = std::mem::take(&mut self.held_shots);
        if status == TestStatus::Failed {
            for (path, data) in held {
                if write_artifact(&path, data).await {
                    if let Some(index) = held_step_index(&path) {
                        self.trace.set_screenshot(index, path.clone());
                    }
                    screenshots.push(path);
                }
            }
        }

        // Video and logs: kept by their modes; crashes they saw explain
        // the failure.
        let failed = status == TestStatus::Failed;
        let mut attachments = Vec::new();
        if let Some(session) = self.evidence.take() {
            let root = PathBuf::from(&self.config.artifacts.output_dir);
            let dir = crate::trace::trace_dir(&root, &test.name);
            let suffix = self.trace.file_suffix(self.retry_attempt);
            let out = velocity_common::EvidenceOut {
                dir: &dir,
                suffix: &suffix,
                keep_video: self.config.artifacts.video.keeps(failed),
                keep_logs: self.config.artifacts.logs.keeps(failed),
            };
            let ev = match tokio::time::timeout(EVIDENCE_FINISH_BUDGET, session.finish(out)).await {
                Ok(ev) => ev,
                Err(_) => {
                    tracing::warn!("writing the video and logs timed out");
                    velocity_common::Evidence::default()
                }
            };
            if failed {
                let extra: Vec<String> = ev
                    .notes
                    .iter()
                    // Not when the error already says it (the driver's own
                    // crash check quotes the same exception).
                    .filter(|n| {
                        let said = test_error.as_deref().unwrap_or("");
                        let what = n.split_once(": ").map_or(n.as_str(), |(_, w)| w);
                        !said.contains(what)
                    })
                    .map(|n| format!("log: {n}"))
                    .collect();
                if !extra.is_empty() {
                    let add = extra.join("\n");
                    let joined = |e: Option<String>| match e {
                        Some(e) if !e.is_empty() => format!("{e}\n{add}"),
                        _ => add.clone(),
                    };
                    test_error = Some(joined(test_error.take()));
                    if let Some(r) = step_results
                        .iter_mut()
                        .rev()
                        .find(|r| r.status == StepStatus::Failed)
                    {
                        r.error_message = Some(joined(r.error_message.take()));
                    }
                }
            }
            if !ev.attachments.is_empty() || !ev.log_lines.is_empty() {
                use velocity_common::AttachmentKind as K;
                let rel = |k: &[K]| -> Vec<String> {
                    ev.attachments
                        .iter()
                        .filter(|a| k.contains(&a.kind))
                        .map(|a| crate::trace_html::relative(&a.path, &dir))
                        .collect()
                };
                let evidence = crate::trace::TraceEvidence {
                    video: rel(&[K::Video]).into_iter().next(),
                    video_offset_ms: ev.video_offset_ms,
                    screencast: rel(&[K::Screencast]).into_iter().next(),
                    logs: rel(&[K::AppLog, K::DeviceLog]),
                    log_lines: None,
                    notes: ev.notes.clone(),
                };
                self.trace.set_evidence(evidence, ev.log_lines);
            }
            attachments = ev.attachments;
            if !attachments.is_empty() {
                tracing::info!("video/logs for '{}' saved to {}", test.name, dir.display());
            }
        }

        let resource_peak = self.profiler.as_ref().and_then(|p| p.peak()).cloned();
        // A heal is kept for `velocity heal` when its test passed, or, in
        // report mode, as the patch the failure proposed (D-7).
        self.selector_engine.healer().settle_heals(
            &test.name,
            status == TestStatus::Passed || self.heal_mode == velocity_common::HealingMode::Report,
        );
        self.selector_engine.persist();
        let mut warnings = std::mem::take(&mut self.warnings);
        warnings.extend(self.selector_engine.take_heal_notes());
        let trace = self.trace.finish(
            std::path::Path::new(&self.config.artifacts.output_dir),
            &test.name,
            device_id,
            self.config.platform,
            self.retry_attempt,
            status,
        );
        if let Some(html) = self.trace.last_html() {
            attachments.push(velocity_common::Attachment::new(
                velocity_common::AttachmentKind::Trace,
                html,
            ));
        }
        if failed {
            attachments.extend(self.crash_log_files().into_iter().map(|p| {
                velocity_common::Attachment::new(velocity_common::AttachmentKind::Crash, p)
            }));
        }

        Ok(TestResult {
            test_name: test.name.clone(),
            status,
            duration: test_start.elapsed(),
            steps: step_results,
            retries: 0,
            error_message: test_error,
            screenshots,
            resource_peak,
            warnings,
            infrastructure_failure: status == TestStatus::Failed && self.last_failure_infra,
            failed_attempts: vec![],
            source: None,
            trace,
            device: Some(device_id.to_string()),
            quarantined: None,
            from_cache: false,
            tags: test.tags.clone(),
            internal_error: false,
            verification: std::mem::take(&mut self.verifications),
            attachments,
        })
    }

    /// The last failed step's error kind, and whether the app crashed.
    pub fn last_failure(&self) -> (Option<velocity_common::ErrorKind>, bool) {
        (self.last_failure_kind, self.last_failure_crashed)
    }

    pub async fn execute_step(
        &mut self,
        step: &Step,
        device_id: &str,
        app_id: &str,
        step_index: usize,
    ) -> Result<StepResult> {
        use tracing::Instrument;
        let span = tracing::info_span!(
            "step",
            index = step_index,
            action = %action_name(&step.action)
        );
        // Lookups stop at the case-folded tier; only a lookup's last look
        // at its deadline widens to merged-label parts (review M2/M3).
        velocity_common::selector_match::with_ceiling(
            velocity_common::selector_match::Tier::Fold,
            self.execute_step_inner(step, device_id, app_id, step_index)
                .instrument(span),
        )
        .await
    }

    async fn execute_step_inner(
        &mut self,
        step: &Step,
        device_id: &str,
        app_id: &str,
        step_index: usize,
    ) -> Result<StepResult> {
        self.restore_step_overrides();
        self.current_step = step_index;
        self.pause_for_jitter().await;
        let step_start = Instant::now();
        // `${NAME}`s that earlier `http` steps of this test captured.
        let bound = match self.bind_captured(step) {
            Ok(b) => b,
            Err(e) => {
                return Ok(StepResult {
                    step_index,
                    action_name: action_name(&step.action),
                    status: StepStatus::Failed,
                    duration: step_start.elapsed(),
                    screenshot: None,
                    error_message: Some(located(step, &e)),
                    resource_delta: None,
                    settle_ms: None,
                    source: step.source.clone(),
                })
            }
        };
        let step = bound.as_ref().unwrap_or(step);
        // Every step reads the screen afresh. The cache is only checked by
        // "is it still there", which a moved element passes: a `tap` after an
        // `assertVisible` of the same selector used the bounds from before a
        // banner pushed the list down, and tapped whatever moved into place.
        self.selector_engine.invalidate_cache();
        self.step_timeout_ms = step.timeout_ms;
        self.step_force = step.modifiers.force;
        if let Some(src) = step.source.as_ref().filter(|s| !s.file.is_empty()) {
            self.suite_file = Some(src.file.clone());
        }
        self.step_app_in_front = step.modifiers.app_in_front;
        self.area_dragged = false;
        self.step_settle_ms = 0;
        // `settle: { … }`: this step's timings, put back after it.
        self.step_settle = step.settle.clone();
        self.step_sync_before = step.settle.as_ref().filter(|s| !s.skip).map(|s| {
            let before = self.sync_manager.sync_config_mut().clone();
            let cfg = self.sync_manager.sync_config_mut();
            // Set, so never swapped for a web default (NB-8).
            if let Some(v) = s.reaction_ms {
                cfg.reaction_ms = Some(v);
            }
            if let Some(v) = s.quiet_ms {
                cfg.quiet_ms = Some(v);
            }
            if let Some(v) = s.cap_ms {
                cfg.settle_cap_ms = Some(v);
            }
            before
        });
        // Boxed: the body's state machine (every action's, ~13 KB) was
        // inlined into this future, `execute_step`'s and the test loop's,
        // each ~28-30 KB on the caller's stack (clippy `large_futures`).
        // One allocation a step.
        let result =
            Box::pin(self.execute_step_body(step, device_id, app_id, step_index, step_start)).await;
        self.restore_step_overrides();
        result
    }

    /// Undo the running step's `settle:` override. Also called when the
    /// watchdog dropped the step before it could (VEL-058: the override
    /// leaked into every later step and test).
    fn restore_step_overrides(&mut self) {
        if let Some(before) = self.step_sync_before.take() {
            *self.sync_manager.sync_config_mut() = before;
        }
        self.step_settle = None;
        self.step_force = false;
        self.step_app_in_front = None;
    }

    async fn execute_step_body(
        &mut self,
        step: &Step,
        device_id: &str,
        app_id: &str,
        step_index: usize,
        step_start: Instant,
    ) -> Result<StepResult> {
        // I3: the app crashed earlier in this test; the step can't reach
        // it, so it fails now instead of waiting out its timeout (or
        // tapping the home screen).
        let mut sticky = if crash_sticks_to(&step.action) {
            self.pending_crash(device_id).await
        } else {
            None
        };
        let (name, result, resource_delta) = match &step.action {
            _ if sticky.is_some() => (
                action_name(&step.action),
                sticky.take().map_or(Ok(()), Err),
                None,
            ),
            Action::Natural {
                intent,
                kind,
                data,
                data_name,
                wait_until,
                intent_template,
            } => {
                // Boxed: only natural steps pay for the planner's state
                // (~13 KB) in their step future.
                let (result, delta) = Box::pin(self.execute_natural(
                    (intent, intent_template.as_deref()),
                    *kind,
                    (data.as_deref(), data_name.as_deref()),
                    wait_until.as_deref(),
                    device_id,
                    app_id,
                ))
                .await;
                (action_name(&step.action), result, delta)
            }
            other => {
                let (result, delta) = self.run_action(other, device_id, app_id).await;
                (action_name(other), result, delta)
            }
        };

        Ok(match result {
            Ok(()) => StepResult {
                step_index,
                action_name: name,
                status: StepStatus::Passed,
                duration: step_start.elapsed(),
                screenshot: None,
                error_message: None,
                resource_delta,
                settle_ms: Some(self.step_settle_ms),
                source: step.source.clone(),
            },
            Err(e) => {
                // An app that crashed fails a later step with "not found" or a
                // wrong value: say what happened instead (VelocityLab's crash
                // button was reported as `home_title` not found after 10 s).
                // A configuration error is the suite's, never a crash's.
                let e = if is_infrastructure(&e)
                    || matches!(
                        e,
                        VelocityError::AppCrashed { .. } | VelocityError::Config(_)
                    )
                    || matches!(step.action, Action::Http(_) | Action::Exec(_))
                {
                    e
                } else {
                    match self.driver.crash_report(device_id, app_id).await {
                        Some(detail) => VelocityError::AppCrashed {
                            app_id: app_id.to_string(),
                            detail,
                        },
                        None => e,
                    }
                };
                self.last_failure_infra = is_infrastructure(&e);
                self.last_failure_crashed = matches!(e, VelocityError::AppCrashed { .. });
                self.last_failure_kind = Some(e.kind());
                if self.last_failure_crashed && self.config.artifacts.on_failure {
                    self.save_crash_logs(device_id, app_id).await;
                }
                let screenshot = if self.config.artifacts.on_failure {
                    self.take_failure_screenshot(device_id, &name, step_index)
                        .await
                } else {
                    None
                };
                StepResult {
                    step_index,
                    action_name: name,
                    status: StepStatus::Failed,
                    duration: step_start.elapsed(),
                    screenshot,
                    error_message: Some(located(step, &e)),
                    resource_delta: None,
                    settle_ms: Some(self.step_settle_ms),
                    source: step.source.clone(),
                }
            }
        })
    }

    /// Dismiss system dialogs and wait (best-effort) for the UI to settle.
    async fn settle(&mut self, device_id: &str) {
        self.dialog_handler
            .dismiss_if_present(self.driver, device_id)
            .await;
        if let Err(e) = self
            .sync_manager
            .wait_for_idle(self.driver, device_id)
            .await
        {
            tracing::debug!("pre-plan sync: {e}");
        }
    }

    /// Record (once per test and action kind) that the screen hadn't
    /// settled by the cap: it explains slow steps and hints at a
    /// never-ending animation.
    fn note_unsettled(&mut self, when: &str, action: &Action) {
        let kind = format!("{action:?}");
        let kind = kind.split([' ', '{', '(']).next().unwrap_or("action");
        let cap = self.sync_manager.settle_cap_ms();
        // Name what kept it busy (P9): Espresso names the idling resource.
        let why = self
            .sync_manager
            .busy_reason()
            .map(|w| format!(" ({w})"))
            .unwrap_or_default();
        let note = format!(
            "screen still changing {cap} ms {when} {kind}{why}; continued without a settled \
             screen"
        );
        if !self.warnings.contains(&note) {
            self.warnings.push(note);
        }
    }

    /// Between polls of a waiting assertion: back off (100ms → 500ms) and
    /// force a fresh hierarchy. Returns false once `deadline` has passed;
    /// the last pause is cut short so one more check lands on it (P7).
    async fn poll_pause(&mut self, poll: &mut Duration, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        tokio::time::sleep((*poll).min(left)).await;
        *poll = (*poll * 2).min(Duration::from_millis(500));
        self.selector_engine.invalidate_cache();
        self.driver.invalidate_hierarchy_cache().await;
        true
    }

    /// Hard ceiling for one step (the watchdog): 3× its element timeout,
    /// at least 30 s, plus what the step legitimately spends waiting.
    fn step_deadline(&self, step: &Step) -> Duration {
        let base = step.timeout_ms.unwrap_or(self.config.sync.timeout_ms);
        // Saturating: a huge timeoutMs or wait overflowed (VEL-191).
        let mut ms = base.saturating_mul(3).max(STEP_DEADLINE_FLOOR_MS);
        match &step.action {
            Action::Wait { ms: w } => ms = ms.saturating_add(*w),
            Action::ScrollUntilVisible { max_scrolls, .. } => {
                ms = ms.saturating_add((*max_scrolls as u64).saturating_mul(base))
            }
            Action::LaunchApp { .. } => ms = ms.max(120_000),
            // May call the planner (model latency) and enforce waitUntil.
            Action::Natural { .. } => ms = ms.max(300_000),
            Action::Exec(_) => {
                ms = ms.max(
                    step.timeout_ms
                        .unwrap_or(EXEC_TIMEOUT_MS)
                        .saturating_add(5_000),
                )
            }
            // Each attempt may take the request timeout, with the pauses
            // between them.
            Action::Http(req) => {
                let per = step
                    .timeout_ms
                    .or(self.config.http.as_ref().and_then(|h| h.timeout_ms))
                    .unwrap_or(velocity_api::DEFAULT_TIMEOUT_MS);
                let (retries, pause) = req.retry.as_ref().map_or((0, 0), |r| {
                    let pause = r
                        .interval_ms
                        .unwrap_or(velocity_api::DEFAULT_RETRY_INTERVAL_MS);
                    (u64::from(r.count), pause)
                });
                let all = per
                    .saturating_add(pause)
                    .saturating_mul(retries + 1)
                    .saturating_add(5_000);
                ms = ms.max(all);
            }
            _ => {}
        }
        Duration::from_millis(ms)
    }

    /// How long element waits and assertions may take: the step's
    /// `timeoutMs`, else the sync timeout.
    fn element_timeout_ms(&self) -> u64 {
        self.step_timeout_ms.unwrap_or(self.config.sync.timeout_ms)
    }

    fn assertion_deadline(&self) -> Instant {
        deadline_in(self.element_timeout_ms())
    }

    /// `step` with the captured `${NAME}`s filled in, or `None` when it has
    /// none. The names are the suite's (`config.captured_vars`, so one used
    /// before its capture fails clearly) and whatever this test captured.
    fn bind_captured(&self, step: &Step) -> Result<Option<Step>> {
        if self.config.captured_vars.is_empty() && self.captured.is_empty() {
            return Ok(None);
        }
        let mut names = self.config.captured_vars.clone();
        names.extend(self.captured.keys().cloned());
        crate::env::bind_step(step, &self.captured, &names)
    }
}

/// Write a screenshot (its directory too), off the runtime's workers: a
/// full-resolution PNG is megabytes, and a `std::fs` write in an async fn
/// stalled every other task on that worker (another device's steps in a
/// parallel run) for as long as the disk took. True when it was written.
async fn write_artifact(path: &std::path::Path, data: Vec<u8>) -> bool {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, data).is_ok()
    })
    .await
    .unwrap_or(false)
}

fn action_name(action: &Action) -> String {
    match action {
        Action::LaunchApp { .. } => "launchApp".to_string(),
        Action::StopApp { .. } => "stopApp".to_string(),
        Action::AssertSystemState { .. } => "assertSystemState".to_string(),
        Action::SetConfig { .. } => "setConfig".to_string(),
        Action::MockRequest { .. } => "mockRequest".to_string(),
        Action::ClearMocks => "clearMocks".to_string(),
        Action::Http(req) => format!("http({} {})", req.method(), req.url),
        Action::Exec(e) => format!(
            "exec({})",
            e.run.argv().first().map(String::as_str).unwrap_or("")
        ),
        Action::Tap { .. } => "tap".to_string(),
        Action::DoubleTap { .. } => "doubleTap".to_string(),
        Action::LongPress { .. } => "longPress".to_string(),
        Action::Drag { .. } => "drag".to_string(),
        Action::InputText { .. } => "inputText".to_string(),
        Action::ClearText { .. } => "clearText".to_string(),
        Action::AssertVisible { .. } => "assertVisible".to_string(),
        Action::AssertNotVisible { .. } => "assertNotVisible".to_string(),
        Action::AssertText { .. } => "assertText".to_string(),
        Action::ScrollUntilVisible { .. } => "scrollUntilVisible".to_string(),
        Action::Swipe { .. } => "swipe".to_string(),
        Action::Screenshot { .. } => "screenshot".to_string(),
        Action::AssertScreenshot { name, .. } => format!("assertScreenshot({name})"),
        Action::SaveState { name } => format!("saveState({name})"),
        Action::RestoreState { name } => format!("restoreState({name})"),
        Action::PressKey { .. } => "pressKey".to_string(),
        Action::Wait { .. } => "wait".to_string(),
        Action::RunFlow { .. } => "runFlow".to_string(),
        Action::TapXY { x, y } => format!("tapXY({x},{y})"),
        Action::InputRaw { .. } => "inputRaw".to_string(),
        Action::Goto { url } => format!("goto({url})"),
        Action::GoBack => "goBack".to_string(),
        Action::Refresh => "refresh".to_string(),
        Action::Natural { intent, kind, .. } => {
            let verb = match kind {
                velocity_common::NaturalKind::Act => "do",
                velocity_common::NaturalKind::Assert => "assert",
            };
            format!("{verb}({intent})")
        }
    }
}

/// `artifacts.onFailure: false` turns off failure artifacts, the trace
/// included; `--trace always` still asks for one.
fn trace_mode(
    mode: velocity_common::TraceMode,
    config: &SuiteConfig,
) -> velocity_common::TraceMode {
    use velocity_common::TraceMode;
    match mode {
        TraceMode::OnFailure if !config.artifacts.on_failure => TraceMode::Off,
        other => other,
    }
}

/// A step's error with where the step is written, when that is known:
/// `tests/app.yaml:42: tap { text: "Pay" }: Element not found ...`.
fn located(step: &Step, e: &VelocityError) -> String {
    match &step.source {
        Some(pos) => format!("{pos}: {}: {e}", step_label(&step.action)),
        None => e.to_string(),
    }
}

/// An action as it reads in YAML, short: `tap { text: "Pay" }`.
pub(crate) fn step_label(action: &Action) -> String {
    match action_selector(action) {
        Some(sel) => format!("{} {{ {} }}", action_name(action), selector_fields(sel)),
        None => action_name(action),
    }
}

/// A selector's YAML fields: `text: "Pay", index: 1`.
/// `v` as a YAML double-quoted scalar. Rust's `{:?}` wrote `\u{301}` for a
/// combining accent, and a JSON string leaves DEL, C1 controls and U+FFFE
/// raw, which YAML rejects even inside quotes (VEL-242): every character
/// outside YAML's printable set (and the line/paragraph separators and BOM,
/// which some readers mangle) is escaped.
fn yaml_quoted(v: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control()
                || matches!(
                    c,
                    '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
                ) =>
            {
                let n = u32::from(c);
                if n <= 0xff {
                    let _ = write!(out, "\\x{n:02x}");
                } else {
                    let _ = write!(out, "\\u{n:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// How to reject the heal of `from` (report mode): by its selector, which
/// `--reject` takes (m6), shell-quoted; by its number in the listing when
/// the selector holds a masked value, since the masked text would match no
/// pending heal (review NIT 12).
fn reject_hint(from: &str, suite: &str) -> String {
    let shown = velocity_common::redact::redact(from);
    if shown != from {
        return format!(
            "reject it by its number: `velocity heal --config {suite}` lists it, then \
             `velocity heal --reject <N> --config {suite}` (its selector holds a masked value)"
        );
    }
    let which = from.replace('\'', "'\\''");
    format!("reject it with `velocity heal --reject '{which}' --config {suite}`")
}

/// Added to an ambiguous action's warning while `selectors.strict` is unset
/// (S3): strict becomes the default in 1.0. There is no flip now.
pub const STRICT_STAGING: &str =
    "This will be an error in 1.0; set `selectors.strict: false` to keep today's behaviour.";

pub fn selector_fields(sel: &Selector) -> String {
    let q = |v: &str| yaml_quoted(v);
    match sel {
        Selector::Id(v) => format!("id: {}", q(v)),
        Selector::Text(v) => format!("text: {}", q(v)),
        Selector::TextContains(v) => format!("textContains: {}", q(v)),
        Selector::TextMatches(v) => format!("textMatches: {}", q(v)),
        Selector::IdMatches(v) => format!("idMatches: {}", q(v)),
        Selector::AccessibilityId(v) => format!("accessibilityId: {}", q(v)),
        Selector::ClassName(v) => format!("className: {}", q(v)),
        Selector::Css(v) => format!("css: {}", q(v)),
        Selector::XPath(v) => format!("xpath: {}", q(v)),
        Selector::Role(v) => format!("role: {}", q(v)),
        Selector::Name(v) => format!("name: {}", q(v)),
        Selector::Checked(b) => format!("checked: {b}"),
        Selector::Focused(b) => format!("focused: {b}"),
        Selector::Selected(b) => format!("selected: {b}"),
        Selector::Enabled(b) => format!("enabled: {b}"),
        Selector::Index { selector, index } => {
            format!("{}, index: {index}", selector_fields(selector))
        }
        Selector::Compound(parts) => parts
            .iter()
            .map(selector_fields)
            .collect::<Vec<_>>()
            .join(", "),
        Selector::Relative {
            target,
            relation,
            anchor,
        } => format!(
            "{}, {relation}: {{ {} }}",
            selector_fields(target),
            selector_fields(anchor)
        ),
    }
}

fn chrono_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// A lookup that read the screen and didn't find the element.
fn is_not_found(e: &VelocityError) -> bool {
    match e {
        VelocityError::ElementNotFound { .. } => true,
        VelocityError::AtSource { inner, .. } => is_not_found(inner),
        _ => false,
    }
}

/// I3: whether a step fails at once after the app crashed. Not the steps
/// that bring an app back or stop it, nor the host-only ones.
fn crash_sticks_to(action: &Action) -> bool {
    !matches!(
        action,
        Action::LaunchApp { .. }
            | Action::StopApp { .. }
            // Puts the app's data back with the app stopped: no UI (fi56).
            | Action::RestoreState { .. }
            | Action::SetConfig { .. }
            | Action::MockRequest { .. }
            | Action::ClearMocks
            // Talks to a server, not the app.
            | Action::Http(_)
            // Runs on the host.
            | Action::Exec(_)
    )
}

/// An error no waiting can fix: the suite or the driver's setup is wrong
/// (the headless driver's "add a launchApp step", a selector kind the
/// platform can't run, a bad regex). Lookups waiting for their element
/// retried it until the step timeout: 10.2 s per test on a headless suite
/// with no `launchApp`. Not-found and transient errors still wait.
fn is_config(e: &VelocityError) -> bool {
    use velocity_common::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::ConfigError | ErrorKind::InvalidSelector | ErrorKind::PlatformNotSupported
    )
}

fn is_crash(e: &VelocityError) -> bool {
    match e {
        VelocityError::AppCrashed { .. } => true,
        VelocityError::AtSource { inner, .. } => is_crash(inner),
        _ => false,
    }
}

/// Errors that say the device or its connection failed, not the app.
fn is_infrastructure(e: &VelocityError) -> bool {
    use velocity_common::ErrorKind;
    // Not retried (kind ConfigError), but the device's fault, not the test's.
    if matches!(e, VelocityError::DeviceLocked { .. })
        || matches!(e, VelocityError::AtSource { inner, .. } if matches!(**inner, VelocityError::DeviceLocked { .. }))
    {
        return true;
    }
    // An outside service (the model API) being down is retried like a lost
    // connection, but it is not the device's fault: never quarantine for it
    // (review M5).
    let inner = match e {
        VelocityError::AtSource { inner, .. } => inner.as_ref(),
        e => e,
    };
    if matches!(inner, VelocityError::ServiceUnavailable { .. }) {
        return false;
    }
    matches!(
        e.kind(),
        ErrorKind::ConnectionLost
            | ErrorKind::SessionExpired
            | ErrorKind::DeviceNotReady
            | ErrorKind::DeviceUnresponsive
    )
}

/// `ms` from now, or as far as an `Instant` goes (`Instant + Duration`
/// panics on overflow, and timeoutMs has no upper bound; VEL-191).
fn deadline_in(ms: u64) -> Instant {
    let now = Instant::now();
    now.checked_add(Duration::from_millis(ms))
        .unwrap_or_else(|| now + Duration::from_secs(100 * 365 * 24 * 3600))
}

/// Fill in the real wait time on a not-found error raised by a single lookup.
fn with_timeout(e: VelocityError, timeout_ms: u64) -> VelocityError {
    match e {
        VelocityError::ElementNotFound {
            selector,
            screenshot,
            hierarchy_snapshot,
            ..
        } => VelocityError::ElementNotFound {
            selector,
            timeout_ms,
            screenshot,
            hierarchy_snapshot,
        },
        other => other,
    }
}

fn action_selector(action: &Action) -> Option<&Selector> {
    match action {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::Drag { from: selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    }
}

/// Checks that only read the screen: the app is as alive after them as
/// before.
fn is_read_only(action: &Action) -> bool {
    matches!(
        action,
        Action::AssertVisible { .. }
            | Action::AssertNotVisible { .. }
            | Action::AssertText { .. }
            | Action::AssertScreenshot { .. }
            | Action::AssertSystemState { .. }
            | Action::Screenshot { .. }
    )
}

/// Actions after which the executor waits for the screen to settle.
fn is_mutating(action: &Action) -> bool {
    matches!(
        action,
        Action::Goto { .. }
            | Action::GoBack
            | Action::Refresh
            | Action::Tap { .. }
            | Action::DoubleTap { .. }
            | Action::LongPress { .. }
            | Action::Drag { .. }
            | Action::InputText { .. }
            | Action::ClearText { .. }
            | Action::Swipe { .. }
            | Action::ScrollUntilVisible { .. }
            | Action::PressKey { .. }
            | Action::LaunchApp { .. }
            | Action::StopApp { .. }
            | Action::TapXY { .. }
            | Action::InputRaw { .. }
            | Action::RestoreState { .. }
    )
}

fn describe(action: &Action) -> String {
    let selector = match action {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    };
    match selector {
        Some(sel) => format!("{} {sel}", action_name(action)),
        None => action_name(action),
    }
}

/// Prefix a natural step's failure with the action it resolved to.
fn annotate(action: &Action, e: VelocityError, extra: Option<&str>) -> VelocityError {
    let mut msg = format!("resolved to {}: {e}", describe(action));
    if let Some(extra) = extra {
        msg.push_str("; ");
        msg.push_str(extra);
    }
    match e {
        // Keep assertion failures typed so reporters show expected/actual.
        VelocityError::AssertionFailed { .. } => e,
        // A lost device or a crashed app is not the step's (config) fault:
        // keep the variant, so the test is marked infrastructure / crashed
        // and retried or reported as such (VEL-073).
        VelocityError::AppCrashed { app_id, detail } => VelocityError::AppCrashed {
            app_id,
            detail: format!(
                "{detail} (natural step {msg_action})",
                msg_action = describe(action)
            ),
        },
        // Not found stays not found (NB-1): reporters, retries and
        // `is_not_found` checks key on the variant. The replan note isn't
        // added: the element's absence is the failure.
        VelocityError::ElementNotFound {
            selector,
            timeout_ms,
            screenshot,
            hierarchy_snapshot,
        } => VelocityError::ElementNotFound {
            selector: format!(
                "{selector} (natural step resolved to {}{})",
                describe(action),
                extra.map(|x| format!("; {x}")).unwrap_or_default()
            ),
            timeout_ms,
            screenshot,
            hierarchy_snapshot,
        },
        _ => planning_error(e, msg),
    }
}

/// A cached natural step failed with `first_err` and re-planning it failed
/// with `e`. An outage, a lost device or a crash during the re-plan is the
/// step's error (review M5: a 529 there ended as "Element not found", exit
/// 1, with the re-plan text dropped); otherwise the first failure stands,
/// noting the re-plan.
fn replan_failed(cached: &Action, first_err: VelocityError, e: VelocityError) -> VelocityError {
    match e {
        VelocityError::ServiceUnavailable { service, detail } => {
            VelocityError::ServiceUnavailable {
                service,
                detail: format!(
                    "{detail} (re-plan failed after the cached step {} failed: {first_err})",
                    describe(cached)
                ),
            }
        }
        e if is_infrastructure(&e) || is_crash(&e) => annotate(cached, e, None),
        e => annotate(cached, first_err, Some(&format!("re-plan failed: {e}"))),
    }
}

/// `e` from a natural step (planning, or running what it planned), as the
/// step's error: infrastructure, crashes, an unavailable model API (NB-1:
/// transient, so the run retries it) and timeouts keep their own variant;
/// anything else becomes a configuration error that says `msg`.
fn planning_error(e: VelocityError, msg: String) -> VelocityError {
    match e {
        VelocityError::AssertionFailed { .. }
        | VelocityError::NotActionable { .. }
        | VelocityError::ServiceUnavailable { .. }
        | VelocityError::ElementNotFound { .. }
        | VelocityError::SyncTimeout { .. }
        | VelocityError::StepTimeout { .. } => e,
        _ if is_infrastructure(&e) || is_crash(&e) => e,
        _ => VelocityError::Config(msg),
    }
}

/// The merged text or label a loose (merged-label part) match came from.
fn merged_label(el: &Element) -> String {
    [&el.label, &el.text]
        .into_iter()
        .flatten()
        .find(|s| s.contains('\n') || s.contains(", "))
        .or(el.label.as_ref())
        .or(el.text.as_ref())
        .cloned()
        .unwrap_or_default()
}

/// Whether this runs on a CI server, by the variables CI systems set
/// (`set` says whether one is). Only `CI` was checked, which Jenkins, Azure
/// Pipelines and CodeBuild don't set; healing defaults to `report` there (D-7).
#[cfg(test)]
fn in_ci(set: impl Fn(&str) -> bool) -> bool {
    velocity_common::env::CI_VARS.iter().any(|k| set(k))
}

#[cfg(test)]
mod tests;
