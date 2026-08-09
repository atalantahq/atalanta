use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::HashMap;

use velocity_common::live_events;
use velocity_common::{
    Action, Direction, Element, NaturalKind, Platform, PlatformDriver, ResourceDelta, Result,
    Selector, Step, StepResult, StepStatus, SuiteConfig, TestCase, TestResult, TestStatus,
    VelocityError,
};
use velocity_nl::{NaturalResolver, NaturalStep};

use crate::baselines::baseline_file;
use crate::dialog_handler::DialogHandler;
use crate::native_sync::SyncManager;
use crate::profiler::ResourceProfiler;
use crate::selector::SelectorEngine;

pub struct TestExecutor<'a> {
    driver: &'a dyn PlatformDriver,
    sync_manager: SyncManager,
    selector_engine: SelectorEngine,
    dialog_handler: DialogHandler,
    config: SuiteConfig,
    profiler: Option<ResourceProfiler>,
    /// Resolver for natural-language steps; `None` disables natural steps.
    planner: Option<Arc<NaturalResolver>>,
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
    /// [`velocity_nl::PendingPlan`]).
    nl_pending: Vec<(usize, velocity_nl::PendingPlan)>,
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

/// `sync.timeoutMs`, `sync.reactionMs`, `sync.quietMs`, `sync.settleCapMs`.
fn set_sync_field(
    sync: &mut velocity_common::SyncConfig,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let field = key.strip_prefix("sync.").ok_or_else(|| {
        format!("setConfig: `{key}` isn't a setting or a sync timing (`velocity settings` lists settings)")
    })?;
    // The key first: `sync.mode: none` said "`none` isn't milliseconds"
    // when the problem is that `mode` can't change mid-test (VEL-218).
    let ms = |field: &str| -> std::result::Result<u64, String> {
        // The key first: `sync.mode: none` said "`none` isn't milliseconds".
        if !matches!(
            field,
            "timeoutMs"
                | "timeout_ms"
                | "reactionMs"
                | "reaction_ms"
                | "quietMs"
                | "quiet_ms"
                | "settleCapMs"
                | "settle_cap_ms"
        ) {
            return Err(format!(
                "setConfig: sync.{field} can't change mid-test (timeoutMs, reactionMs, quietMs, settleCapMs can)"
            ));
        }
        value
            .trim()
            .parse()
            .map_err(|_| format!("setConfig.{key}: `{value}` isn't milliseconds"))
    };
    let v = ms(field)?;
    match field {
        "timeoutMs" | "timeout_ms" => sync.timeout_ms = v,
        "reactionMs" | "reaction_ms" => sync.reaction_ms = Some(v),
        "quietMs" | "quiet_ms" => sync.quiet_ms = Some(v),
        _ => sync.settle_cap_ms = Some(v),
    }
    Ok(())
}

/// What `assertSystemState` expected and didn't see (empty: all good). A
/// title matches a playing session's title that contains it.
fn system_state_mismatch(
    state: &velocity_common::SystemState,
    media_playing: Option<bool>,
    media_title: Option<&str>,
    in_call: Option<bool>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(want) = media_playing {
        if state.media_playing != want {
            out.push(format!("media playing={want}"));
        }
    }
    if let Some(t) = media_title {
        if !state.media_titles.iter().any(|x| x.contains(t)) {
            out.push(format!("media playing with title {t:?}"));
        }
    }
    if let Some(want) = in_call {
        if state.in_call != want {
            out.push(format!("in call={want}"));
        }
    }
    out
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
    pub fn with_planner(mut self, planner: Arc<NaturalResolver>) -> Self {
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
                Some(Action::LaunchApp { app_id: a, clear_state: false })
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
                let e = if is_infrastructure(&e)
                    || matches!(e, VelocityError::AppCrashed { .. })
                    || matches!(step.action, Action::Http(_))
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

    /// Run one natural-language step: resolve it (cache first, then the
    /// planner), execute it, and enforce its `waitUntil` condition.
    ///
    /// A cached resolution that fails is re-planned against the current screen
    /// and retried once in the same run, so a changed UI heals without failing
    /// the test (the re-plan is reported as a warning).
    async fn execute_natural(
        &mut self,
        (intent, intent_template): (&str, Option<&str>),
        kind: NaturalKind,
        (data, data_name): (Option<&str>, Option<&str>),
        wait_until: Option<&str>,
        device_id: &str,
        app_id: &str,
    ) -> (Result<()>, Option<ResourceDelta>) {
        let Some(resolver) = self.planner.clone() else {
            return (
                Err(VelocityError::Config(
                    "natural-language step encountered but no NL planner is configured \
                     (set ANTHROPIC_API_KEY or OPENAI_API_KEY to enable)"
                        .to_string(),
                )),
                None,
            );
        };
        let platform = self.config.platform.unwrap_or(Platform::Ios);
        let scope = self.natural_scope(kind, intent_template.unwrap_or(intent), data.is_some());
        // A retry attempt re-plans: replaying the action that just failed
        // would only fail the same way (N-2).
        let fresh = self.retry_attempt > 0;
        // D-30: `nl.dataPolicy: describe` keeps data values from the model.
        let describe_data = self.config.nl.data_policy == velocity_common::NlDataPolicy::Describe;
        let step = NaturalStep {
            intent,
            kind,
            data,
            data_name,
            wait_until,
            platform,
            scope: &scope,
            fresh,
            describe_data,
            intent_template,
        };
        let (result, delta) = self
            .resolve_and_run(&resolver, &step, device_id, app_id, NOT_FOUND_ATTEMPTS)
            .await;
        let main = match result {
            Ok(r) => r,
            Err(e) => return (Err(e), delta),
        };

        // waitUntil is a check, not a hint: after the action, the named state
        // must appear (within the usual element timeout).
        if let Some(condition) = wait_until {
            let until_scope = format!("{scope}/until");
            let until = NaturalStep {
                intent: condition,
                kind: NaturalKind::Assert,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
                platform,
                scope: &until_scope,
                fresh,
                describe_data,
            };
            match self
                .resolve_and_run(&resolver, &until, device_id, app_id, WAIT_UNTIL_ATTEMPTS)
                .await
            {
                (Ok(check), _) => {
                    let staged = resolver.prepare(&until, check.action, check.twins, None);
                    self.stage(staged);
                }
                (Err(e), _) => {
                    // The step's plan didn't reach its goal: don't keep it (N-1).
                    resolver.invalidate(&step).await;
                    // An unmet waitUntil is the test failing its check, not
                    // a configuration problem (VEL-222); a lost device or an
                    // unavailable model API keeps its own variant.
                    let e = match e {
                        VelocityError::ServiceUnavailable { .. } => e,
                        _ if is_infrastructure(&e) || is_crash(&e) => e,
                        e => VelocityError::AssertionFailed {
                            expected: format!("waitUntil {condition:?}"),
                            actual: format!("not reached: {e}"),
                            selector: String::new(),
                            screenshot: None,
                        },
                    };
                    return (Err(e), delta);
                }
            }
        }
        // Staged only now that the action and its check both passed (N-1);
        // written once the run confirms it (see `promote_pending`).
        let staged = resolver.prepare(&step, main.action, main.twins, main.effect);
        self.stage(staged);
        (Ok(()), delta)
    }

    /// For an indexed selector, how many elements its base matches on the
    /// current screen (the "twins" an index picks among).
    async fn twin_count(&mut self, device_id: &str, action: &Action) -> Option<usize> {
        let Some(Selector::Index { selector, .. }) = action_selector(action) else {
            return None;
        };
        let tree = self.driver.get_hierarchy_cached(device_id).await.ok()?;
        self.audit_screen(device_id, &tree);
        let (w, h) = self.driver.screen_size(device_id).await.ok()?;
        let screen = velocity_common::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        Some(velocity_common::selector_match::match_count(
            &tree,
            selector,
            Some(&screen),
        ))
    }

    /// Run `action` for a natural step and observe it: the twin count before,
    /// and whether the screen reacted after (for act steps).
    async fn run_observed(
        &mut self,
        action: &Action,
        kind: NaturalKind,
        device_id: &str,
        app_id: &str,
    ) -> (Result<Resolved>, Option<ResourceDelta>) {
        // D-17: a planned or cached `goto` stays on http(s) and on the
        // current page's origin unless the suite allows otherwise.
        if let Action::Goto { url } = action {
            let current = self.driver.current_url(device_id).await;
            if let Err(why) = velocity_nl::grounding::check_goto(
                url,
                current.as_deref(),
                self.config.nl.allow_cross_origin,
            ) {
                return (Err(VelocityError::Config(why)), None);
            }
        }
        let twins = self.twin_count(device_id, action).await;
        let (result, delta) = self.run_action(action, device_id, app_id).await;
        let effect = (kind == NaturalKind::Act && is_mutating(action))
            .then(|| self.sync_manager.last_settle().reacted);
        (
            result.map(|()| Resolved {
                action: action.clone(),
                twins,
                effect,
            }),
            delta,
        )
    }

    async fn resolve_and_run(
        &mut self,
        resolver: &Arc<NaturalResolver>,
        step: &NaturalStep<'_>,
        device_id: &str,
        app_id: &str,
        attempts: u32,
    ) -> (Result<Resolved>, Option<ResourceDelta>) {
        if let Some(hit) = resolver.cached_hit(step).await {
            // Fast path: no model call, and no screen capture unless the
            // selector is indexed.
            let cached = hit.action.clone();
            let twins_now = self.twin_count(device_id, &cached).await;
            let (result, delta) = match (hit.twins, twins_now) {
                // Identity check (N-4): the rows an index picks among changed,
                // so it may now hit a different "twin".
                (Some(was), Some(now)) if was != now => (
                    Err(VelocityError::Config(format!(
                        "the {was} matching elements it chose among are now {now}"
                    ))),
                    None,
                ),
                _ => {
                    self.run_observed(&cached, step.kind, device_id, app_id)
                        .await
                }
            };
            // Effect check (N-5): it changed the screen when planned and
            // changes nothing now. That fails the step, and no second,
            // re-planned action runs in it (D-17, VEL-349): the first one
            // already acted. The next attempt plans afresh.
            if let Ok(r) = &result {
                if hit.effect == Some(true) && r.effect == Some(false) {
                    resolver.invalidate(step).await;
                    return (
                        Err(VelocityError::AssertionFailed {
                            expected: format!(
                                "{} to change the screen, as it did when it was planned",
                                describe(&cached)
                            ),
                            actual: "no visible effect; not re-planned in this step \
                                     (the cache entry was dropped, so the next attempt plans it afresh)"
                                .into(),
                            selector: action_selector(&cached)
                                .map(|s| s.to_string())
                                .unwrap_or_default(),
                            screenshot: None,
                        }),
                        delta,
                    );
                }
            }
            let first_err = match result {
                Ok(r) => {
                    tracing::info!(intent = step.intent, "velocity-nl: cache hit");
                    return (Ok(r), delta);
                }
                Err(e) => e,
            };
            // A failed natural-language assertion is never re-planned onto
            // the current screen (D-17, VEL-213): a re-plan would look for
            // whatever is there and could pass on another element. The
            // entry is dropped, so the next attempt plans it afresh.
            if step.kind == NaturalKind::Assert {
                tracing::info!(intent = step.intent, reason = %first_err, "velocity-nl: cached assertion failed; not re-planning");
                resolver.invalidate(step).await;
                return (
                    Err(annotate(
                        &cached,
                        first_err,
                        Some("a failed assertion is not re-planned onto the current screen"),
                    )),
                    delta,
                );
            }
            tracing::info!(intent = step.intent, reason = %first_err, "velocity-nl: cached step failed; re-planning");
            resolver.invalidate(step).await;
            // N-8: re-plan with the old resolution as a hint.
            let hint = format!(
                "This step previously resolved to {} and that no longer works: {first_err}. \
                 Prefer the same element if it is still on screen.",
                describe(&cached)
            );
            let replanned = match self
                .plan_natural(resolver, step, device_id, attempts, Some(&hint))
                .await
            {
                Ok(a) => a,
                Err(e) => return (Err(replan_failed(&cached, first_err, e)), None),
            };
            if format!("{replanned:?}") == format!("{cached:?}") && hit.twins == twins_now {
                // Same answer on the current screen: the UI didn't move, the
                // step genuinely failed. Don't run it twice.
                return (Err(annotate(&cached, first_err, None)), None);
            }
            let (result, delta) = self
                .run_observed(&replanned, step.kind, device_id, app_id)
                .await;
            return match result {
                Ok(r) => {
                    self.warnings.push(format!(
                        "natural step {:?}: cached {} stopped working ({first_err}); \
                         re-planned to {}",
                        step.intent,
                        describe(&cached),
                        describe(&replanned)
                    ));
                    (Ok(r), delta)
                }
                Err(e) => (Err(annotate(&replanned, e, None)), None),
            };
        }

        tracing::info!(
            intent = step.intent,
            reason = if step.fresh {
                "retry attempt"
            } else {
                "not cached"
            },
            "velocity-nl: planning"
        );
        let action = match self
            .plan_natural(resolver, step, device_id, attempts, None)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("natural-language resolution failed: {e}");
                return (Err(planning_error(e, msg)), None);
            }
        };
        let (result, delta) = self
            .run_observed(&action, step.kind, device_id, app_id)
            .await;
        match result {
            Ok(r) => (Ok(r), delta),
            Err(e) => (Err(annotate(&action, e, None)), None),
        }
    }

    async fn plan_natural(
        &mut self,
        resolver: &Arc<NaturalResolver>,
        step: &NaturalStep<'_>,
        device_id: &str,
        attempts: u32,
        hint: Option<&str>,
    ) -> Result<Action> {
        let mut attempt = 1;
        loop {
            self.settle(device_id).await;
            let hierarchy = self.driver.get_hierarchy_cached(device_id).await?;
            self.trace.record_tree(&hierarchy);
            self.audit_screen(device_id, &hierarchy);
            // Ground in the rect the driver matches in (VEL-018).
            let screen =
                self.driver
                    .screen_size(device_id)
                    .await
                    .ok()
                    .map(|(w, h)| velocity_common::Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    });
            match resolver
                .plan_on_screen(step, &hierarchy, screen.as_ref(), hint)
                .await
            {
                Err(VelocityError::ElementNotFound { .. }) if attempt < attempts => {
                    attempt += 1;
                    tokio::time::sleep(NOT_FOUND_RETRY_DELAY).await;
                    self.driver.invalidate_hierarchy_cache().await;
                }
                other => return other,
            }
        }
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

    /// `Test name#n` — the cache scope for the n-th occurrence of a natural
    /// step in the running test.
    /// The cache scope of a natural step: the test and how many times this
    /// step (by its written shape, quoted values blanked, data presence;
    /// never its values) came before in the test.
    fn natural_scope(&mut self, kind: NaturalKind, written: &str, has_data: bool) -> String {
        let key = format!(
            "{kind:?}\u{1f}{}\u{1f}{has_data}",
            velocity_nl::step_shape(written.trim()),
        );
        let n = self.natural_seen.entry(key).or_insert(0);
        let scope = format!("{}#{n}", self.current_test);
        *n += 1;
        scope
    }

    /// Hold a verified-so-far plan of the running step for the cache.
    fn stage(&mut self, plan: Option<velocity_nl::PendingPlan>) {
        if let Some(plan) = plan {
            let step = self.current_step;
            self.nl_pending.push((step, plan));
        }
    }

    /// Commit the pending plans staged before step `before` (all of them
    /// for `None`): a later step passing confirms the screen they led to.
    async fn promote_pending(&mut self, before: Option<usize>) {
        let Some(resolver) = self.planner.clone() else {
            self.nl_pending.clear();
            return;
        };
        let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.nl_pending)
            .into_iter()
            .partition(|(i, _)| before.is_none_or(|b| *i < b));
        self.nl_pending = keep;
        if !ready.is_empty() {
            resolver
                .commit_pending(ready.into_iter().map(|(_, p)| p))
                .await;
        }
    }

    /// The claim to check after `step` under `verify: checkpoints` or
    /// `strict`: its `checkpoint:` claim, or for an `assert:` step (and a
    /// natural step with `checkpoint: true`) its `waitUntil` or intent.
    fn checkpoint_claim(&self, step: &Step) -> Option<String> {
        if !self.verify_level.checks_checkpoints() {
            return None;
        }
        let own = match &step.action {
            Action::Natural {
                intent, wait_until, ..
            } => Some(wait_until.clone().unwrap_or_else(|| intent.clone())),
            _ => None,
        };
        match step.modifiers.checkpoint.as_deref() {
            Some("") => own,
            Some(claim) => Some(claim.to_string()),
            None => match &step.action {
                Action::Natural {
                    kind: NaturalKind::Assert,
                    ..
                } => own,
                _ => None,
            },
        }
    }

    /// Ask the AI checker whether `claim` holds on the current screen.
    /// Code decides: see [`velocity_nl::checker`].
    async fn verify_screen(
        &mut self,
        device_id: &str,
        at: &str,
        claim: &str,
    ) -> velocity_common::VerifyCheck {
        let unavailable = |reason: String| velocity_common::VerifyCheck {
            at: at.to_string(),
            claim: claim.to_string(),
            outcome: velocity_common::VerifyOutcome::Error,
            reason,
            evidence: vec![],
        };
        let Some(resolver) = self.planner.clone() else {
            return unavailable(
                "no AI checker is configured (set ANTHROPIC_API_KEY or OPENAI_API_KEY)".into(),
            );
        };
        self.settle(device_id).await;
        let hierarchy = match self.driver.get_hierarchy_cached(device_id).await {
            Ok(h) => h,
            Err(e) => return unavailable(format!("could not read the screen: {e}")),
        };
        self.audit_screen(device_id, &hierarchy);
        let screen =
            self.driver
                .screen_size(device_id)
                .await
                .ok()
                .map(|(w, h)| velocity_common::Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                });
        let shot = self.driver.screenshot(device_id).await.ok();
        let platform = self.config.platform.unwrap_or(Platform::Ios);
        let check = resolver
            .check(
                at,
                claim,
                &hierarchy,
                screen.as_ref(),
                platform,
                shot.as_deref(),
            )
            .await;
        tracing::info!(at, outcome = %check.outcome, "velocity-nl: verify");
        check
    }

    /// Execute a concrete action with pre/post sync, dialog handling and
    /// resource profiling.
    async fn run_action(
        &mut self,
        action: &Action,
        device_id: &str,
        app_id: &str,
    ) -> (Result<()>, Option<ResourceDelta>) {
        // Pre-action sync (skip for wait, screenshot, and app lifecycle actions)
        let skip_sync = matches!(
            action,
            Action::Wait { .. }
                | Action::Screenshot { .. }
                | Action::LaunchApp { .. }
                | Action::StopApp { .. }
                | Action::Goto { .. }
                | Action::GoBack
                | Action::Refresh
                | Action::SetConfig { .. }
                | Action::MockRequest { .. }
                | Action::ClearMocks
                | Action::Http(_)
        );
        let settle_off = self.step_settle.as_ref().is_some_and(|s| s.skip);
        self.settled_for_lookup = false;
        if !skip_sync && !settle_off {
            // Dismiss any system dialogs before waiting for idle, unless the
            // step itself targets the dialog ("tap: Don't Allow").
            self.dialog_handler
                .dismiss_unless_target(self.driver, device_id, action_selector(action))
                .await;
            // The lookup after a stable settle skips the strict re-read
            // (fd3), which is only sound on a tree read after the settle
            // started. Android's settle compares lite dumps that never fill
            // the lookup cache, so a tree cached by the dialog check above or
            // by an earlier `assertVisible` (TTL 2 s) could be from mid-slide
            // and get tapped at stale coordinates (review-fixes #1). Drop it
            // here: on iOS the settle's own reads refill it (no extra read);
            // on Android the lookup reads once, fresh.
            self.driver.invalidate_hierarchy_cache().await;
            self.selector_engine.invalidate_cache();

            // Collect pre-action metrics concurrently with sync (zero added latency)
            let sync_mgr = &mut self.sync_manager;
            let driver = self.driver;
            let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

            let snap_fut = async {
                match &pkg {
                    Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                    None => None,
                }
            };

            let (sync_result, raw_before) =
                tokio::join!(sync_mgr.wait_for_idle(driver, device_id), snap_fut);
            // Best-effort, like post-action sync: a screen that never stops
            // animating (spinner, carousel) must not fail the step by itself.
            // The element lookup below still waits for its target.
            self.step_settle_ms += self.sync_manager.last_settle().duration.as_millis() as u64;
            self.trace_settle();
            let sync_result = match sync_result {
                // The screen can't be read for a reason no wait fixes ("add
                // a launchApp step"): the action can't do better, so fail
                // with that now instead of after its lookup's whole wait.
                // Native mode's probe errors are config-kind too, and stay
                // best-effort.
                Err(e) if is_config(&e) && !self.sync_manager.probe_errors_surface() => {
                    return (Err(e), None)
                }
                other => other,
            };
            if let Err(e) = &sync_result {
                tracing::debug!("pre-action sync: {e}");
            }
            if sync_result.is_err() || self.sync_manager.last_settle().capped {
                self.note_unsettled("before", action);
            }
            let st = self.sync_manager.last_settle();
            self.settled_for_lookup = sync_result.is_ok() && st.stable && !st.capped;

            if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_before) {
                profiler.record_before(ResourceProfiler::snapshot_from_raw(raw));
            }
        }

        // D-2: whether the test is leaving the suite's app on purpose.
        match action {
            Action::Goto { .. }
            | Action::PressKey {
                key: velocity_common::Key::Home,
            } => self.app_switched = true,
            Action::LaunchApp { app_id: a, .. } => {
                self.app_switched = !a.is_empty() && a != &self.app_id;
            }
            _ => {}
        }
        // Drivers that check actionability themselves (web) follow the
        // step's rule.
        let mode = self.act_mode();
        // Boxed: `execute_action` holds every action's state (~9 KB), which
        // otherwise sits inline in each of `run_action`'s callers.
        let result = velocity_common::actionability::with_mode(
            mode,
            Box::pin(self.execute_action(action, device_id, app_id)),
        )
        .await;
        // Anything but a read may have changed whether the app is alive.
        if !is_read_only(action) {
            self.liveness_seen = None;
        }

        // Post-action sync for actions that modify UI state
        let mutating = is_mutating(action);
        if !mutating {
            // Nothing settled after it, so the screen may have moved
            // (review-s8 m-d).
            self.sync_manager.forget_picture();
        }

        if let Err(e) = result {
            // The driver refused the target (VEL-274): it didn't act on it
            // anyway, whatever find_actionable's warning said.
            if matches!(e, VelocityError::NotActionable { .. }) {
                if let Some(sel) = action_selector(action) {
                    let prefix = format!("target {sel} ");
                    self.warnings
                        .retain(|w| !(w.starts_with(&prefix) && w.ends_with(ACTED_ANYWAY)));
                }
            }
            return (Err(e), None);
        }
        let mut resource_delta = None;
        if mutating && settle_off {
            // `settle: none`: act and move on (the next lookup still waits
            // for its element).
            self.selector_engine.invalidate_cache();
            self.sync_manager.invalidate();
            self.driver.invalidate_hierarchy_cache().await;
            return (Ok(()), None);
        }
        if matches!(action, Action::ScrollUntilVisible { .. }) {
            // Audit #11: the loop settled after its last swipe, and with no
            // swipe nothing moved. A second settle here saw no change and
            // so waited out the whole reaction floor plus its reads (6-9 s
            // a suite). The sync engine keeps the loop's settled screen, so
            // the next step's wait confirms it with one read; only the
            // lookup caches go, as after any action.
            self.selector_engine.invalidate_cache();
            self.driver.invalidate_hierarchy_cache().await;
            if let Some(pkg) = self.profiler.as_ref().map(|p| p.package().to_string()) {
                if let Ok(raw) = self.driver.collect_resource_metrics(device_id, &pkg).await {
                    let snap = ResourceProfiler::snapshot_from_raw(raw);
                    resource_delta = self.profiler.as_mut().and_then(|p| p.record_after(snap));
                }
            }
            return (Ok(()), resource_delta);
        }
        if mutating {
            self.selector_engine.invalidate_cache();
            self.sync_manager.invalidate();
            self.driver.invalidate_hierarchy_cache().await;

            // Collect post-action metrics concurrently with sync
            let sync_mgr = &mut self.sync_manager;
            let driver = self.driver;
            let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

            let snap_fut = async {
                match &pkg {
                    Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                    None => None,
                }
            };

            // A crashed app leaves the launcher in front, whose live
            // wallpaper never settles: the wait ran to its 5 s cap before the
            // crash was seen. Watch the crash log alongside, from 1 s on (a
            // normal settle is done by then and pays nothing). Dropping the
            // settle is safe: tree-server calls are cancel-safe (fix 34).
            let app_id = self.app_id.clone();
            let crash_watch = async {
                tokio::time::sleep(crash_watch_after()).await;
                loop {
                    if let Some(detail) = driver.crash_report(device_id, &app_id).await {
                        return detail;
                    }
                    tokio::time::sleep(crash_watch_every()).await;
                }
            };
            let settle_or_crash = async {
                tokio::select! {
                    r = sync_mgr.wait_for_idle(driver, device_id) => Ok(r),
                    detail = crash_watch => Err(detail),
                }
            };
            let (outcome, raw_after) = tokio::join!(settle_or_crash, snap_fut);
            let sync_result = match outcome {
                Ok(r) => r,
                Err(detail) => {
                    return (
                        Err(VelocityError::AppCrashed {
                            app_id: self.app_id.clone(),
                            detail,
                        }),
                        None,
                    )
                }
            };
            // Best-effort: a screen that keeps animating doesn't fail the
            // step, but the report says so.
            self.step_settle_ms += self.sync_manager.last_settle().duration.as_millis() as u64;
            self.trace_settle();
            if sync_result.is_err() || self.sync_manager.last_settle().capped {
                // A crashed app leaves the launcher in front, whose live
                // wallpaper never lets the screen settle: the cap is the first
                // sign. Say so here instead of in the next step's wait (the
                // crash test then spent another 1.7 s and more before failing).
                // Only on this slow path, so a normal step pays nothing.
                let app_id = self.app_id.clone();
                if let Some(detail) = self.driver.crash_report(device_id, &app_id).await {
                    return (Err(VelocityError::AppCrashed { app_id, detail }), None);
                }
                self.note_unsettled("after", action);
            }

            if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_after) {
                resource_delta = profiler.record_after(ResourceProfiler::snapshot_from_raw(raw));
            }

            // Check for system dialogs after the action.
            // Skip for coordinate-based actions (TapXY, InputRaw) — these target
            // canvas/WebView elements that never trigger system permission dialogs.
            let can_trigger_dialog =
                !matches!(action, Action::TapXY { .. } | Action::InputRaw { .. });
            if can_trigger_dialog {
                self.dialog_handler
                    .dismiss_if_present(self.driver, device_id)
                    .await;
            }
        }
        (Ok(()), resource_delta)
    }

    /// I3 (D-50): the crash the driver already knows of for the suite's
    /// app, sticky for the rest of the test. No device round trip (the
    /// driver answers from what it has seen), so it is asked before every
    /// step and on every attempt of an element wait. `None` once the test
    /// stopped the app on purpose.
    async fn pending_crash(&mut self, device_id: &str) -> Option<VelocityError> {
        if self.app_stopped {
            return None;
        }
        let app_id = self.app_id.clone();
        if self.sticky_crash.is_none() {
            let latched = self.driver.pending_crash(device_id, &app_id).await?;
            // The driver's crash report says why, when it has one.
            let detail = match self.driver.crash_report(device_id, &app_id).await {
                Some(d) => d,
                None => latched,
            };
            self.sticky_crash = Some(detail);
        }
        self.sticky_crash
            .clone()
            .map(|detail| VelocityError::AppCrashed { app_id, detail })
    }

    /// The suite's app crashed (the driver's liveness says so), unless the
    /// test stopped it on purpose. Read at most once between actions: a run
    /// of negative assertions pays for one read (an `simctl` call on iOS),
    /// not one each.
    async fn check_alive(&mut self, device_id: &str) -> Option<VelocityError> {
        if self.app_stopped {
            return None;
        }
        let app_id = self.app_id.clone();
        if self.liveness_seen.is_none() {
            self.liveness_seen = Some(self.driver.app_liveness(device_id, &app_id).await);
        }
        match &self.liveness_seen {
            Some(velocity_common::Liveness::Dead(detail)) => Some(VelocityError::AppCrashed {
                app_id,
                detail: detail.clone(),
            }),
            _ => None,
        }
    }

    /// Warn when a bare selector matched several elements on screen: the
    /// action went to the first in tree order, which may not be the one
    /// meant. In strict mode (`selectors.strict`) it fails the step instead.
    /// Both name ready-made `childOf:` / `index:` selectors for each match.
    ///
    /// R5 (D-6): the count comes from the driver that resolves, on every
    /// driver: the shared matcher over the driver's own tree where that is
    /// how it resolves, the page itself on the web (and for an iOS element
    /// WDA found without a tree match), so the count is the one the lookup
    /// saw. It used to run only with a tree cache, and counted the web's
    /// element tree, where `className` and `id` mean something else.
    async fn note_if_ambiguous(
        &mut self,
        device_id: &str,
        selector: &Selector,
        found: &Element,
    ) -> Result<()> {
        if matches!(selector, Selector::Index { .. }) {
            return Ok(());
        }
        let strict = self.strict_selectors();
        let Some((n, matches, picks)) = self.ambiguous_matches(device_id, selector, found).await
        else {
            return Ok(());
        };
        if n <= 1 {
            return Ok(());
        }
        let picks: Vec<String> = picks
            .iter()
            .map(|p| format!("{{ {} }}", selector_fields(p)))
            .collect();
        let pick_hint = if picks.is_empty() {
            "Add index:, or below:/childOf: to pick one".to_string()
        } else {
            format!("Pick one with {}", picks.join(" or "))
        };
        if strict {
            let listed: Vec<String> = matches
                .iter()
                .take(5)
                .map(|e| {
                    format!(
                        "{} at ({}, {})",
                        crate::healing::describe_element(e),
                        e.bounds.x,
                        e.bounds.y
                    )
                })
                .collect();
            return Err(VelocityError::AssertionFailed {
                expected: format!("{selector} to match one element (selectors.strict)"),
                actual: format!("{n} match: {}. {pick_hint}", listed.join("; ")),
                selector: selector.to_string(),
                screenshot: None,
            });
        }
        let mut note = format!(
            "{selector} matched {n} elements on screen; used {}. {pick_hint}",
            velocity_common::selector_match::BARE_PICK_RULE
        );
        if self.strict_selectors_unset() {
            note.push_str(&format!(" {STRICT_STAGING}"));
        }
        if !self.warnings.contains(&note) {
            self.warnings.push(note);
        }
        Ok(())
    }

    /// How many elements an action on `selector` could equally mean, those
    /// matches, and a selector picking each (see [`Self::note_if_ambiguous`]).
    async fn ambiguous_matches(
        &self,
        device_id: &str,
        selector: &Selector,
        found: &Element,
    ) -> Option<(usize, Vec<Element>, Vec<Selector>)> {
        use velocity_common::{selector_match as sm, Relation};
        let web = self.config.platform == Some(Platform::Web);
        let wda_hit = found.bounds.is_empty();
        if web || wda_hit || !self.driver.has_hierarchy_cache() {
            // Count what the driver's own lookup finds: one query, as the
            // resolve was. A layout relation picks the nearest, so its
            // other matches aren't a blind choice.
            if matches!(selector, Selector::Relative { relation, .. }
                if !matches!(relation, Relation::ChildOf | Relation::ContainsChild))
            {
                return None;
            }
            // Review MAJOR 8: the count of the lookup that just resolved
            // (web counts on screen in that same page call; headless,
            // Flutter and RN on the tree they read), so a clear selector
            // costs no second lookup. Without one, only strict mode looks
            // again (and a WDA hit, which has no tree).
            let counted = if wda_hit {
                None
            } else {
                self.driver.last_match_count(device_id, selector)
            };
            match counted {
                Some(n) if n <= 1 => return Some((n, Vec::new(), Vec::new())),
                Some(_) => {}
                None if !wda_hit && !self.strict_selectors() => return None,
                None => {}
            }
            let all: Vec<Element> = self
                .driver
                .find_elements(device_id, selector)
                .await
                .ok()?
                .into_iter()
                .filter(|e| e.visible && (wda_hit || !e.bounds.is_empty()))
                .collect();
            if all.len() <= 1 || web || wda_hit {
                // No tree to name a container from: an index per match.
                let picks = (0..all.len().min(5))
                    .map(|i| Selector::Index {
                        selector: Box::new(selector.clone()),
                        index: i,
                    })
                    .collect();
                // A count above what the page holds now is stale (an
                // earlier screen's): trust the fresh list (review-fixes #6).
                let n = counted.map_or(all.len(), |n| n.min(all.len()));
                return Some((n, all, picks));
            }
            // Ambiguous on a driver that resolves on its tree: read it
            // once more for container suggestions (only now, so a clear
            // selector costs no extra read).
        }
        let tree = self.driver.get_hierarchy_cached(device_id).await.ok()?;
        let (w, h) = self.driver.screen_size(device_id).await.ok()?;
        let screen = velocity_common::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let n = sm::ambiguity(&tree, selector, Some(&screen));
        if n <= 1 {
            return Some((n, Vec::new(), Vec::new()));
        }
        let matches = sm::find_all_ref(&tree, selector, Some(&screen))
            .into_iter()
            .cloned()
            .collect();
        let picks = sm::suggest_picks(&tree, selector, Some(&screen), 5);
        Some((n, matches, picks))
    }

    /// `assertScreenshot`: mask the status bar (or freeze it, D-V4) and
    /// any `mask` elements, cut to `crop` if set, and compare with the
    /// baseline in
    /// `<suite dir>/velocity-baselines/<platform>[-headless]/<W>x<H>/<name>.png`
    /// (`W`x`H`: the device's full capture, D-V3).
    ///
    /// Playwright's capture: a first frame that matches the baseline passes
    /// at once; otherwise frames are taken until two in a row match each
    /// other under the step's own rule, threshold and cluster limit, and
    /// the still one is compared. A screen that never holds still fails
    /// with the previous frame, the actual one and their diff, and is never
    /// written as a baseline (B5/B6). A missing baseline is written and the
    /// step fails, on a laptop and in CI alike, unless the run is
    /// `--update-baselines[=missing]` (D-V1).
    async fn assert_screenshot(
        &mut self,
        device_id: &str,
        name: &str,
        step: ScreenshotStep<'_>,
    ) -> Result<()> {
        use crate::visual::{compare_images_rule, VisualConfig, VisualEngine};
        use velocity_common::BaselineUpdate;
        let threshold = step.threshold.unwrap_or(0.98);
        let cluster = step.cluster_px.unwrap_or(crate::visual::DEFAULT_CLUSTER_PX);
        let rule = step.rule;
        let frozen = self.freeze_status_bar(device_id).await;
        let first_full = self.capture_png(device_id).await?;
        let (full_w, full_h) = crate::visual::png_size(&first_full).unwrap_or((0, 0));
        let crop = match step.crop {
            Some(c) => Some(self.screenshot_crop(device_id, &first_full, c).await?),
            None => None,
        };
        let full_masks = self
            .screenshot_masks_with(device_id, &first_full, step.mask, !frozen)
            .await?;
        let masks = match &crop {
            Some(c) => crate::visual::crop_masks(&full_masks, full_w, full_h, c),
            None => full_masks,
        };
        let first = cut(&first_full, crop.as_ref())?;
        let size = crate::visual::png_size(&first);

        let place = self.baseline_place(name, (full_w, full_h), size);
        // Two names that clean to the same file would share (and overwrite)
        // one baseline (VEL-201). New names never do (they carry a hash);
        // a legacy file two names fall back to is an error.
        let key = place.read.clone().unwrap_or_else(|| place.write.clone());
        // Lower-cased: "Home" and "home" are one file on macOS and Windows.
        let key = key.to_string_lossy().to_lowercase();
        match self.baseline_names.get(&key) {
            Some(other) if other != name => {
                return Err(VelocityError::Config(format!(
                    "assertScreenshot names {other:?} and {name:?} both map to baseline \
                     file {key}; rename one"
                )));
            }
            Some(_) => {}
            None => {
                self.baseline_names.insert(key, name.to_string());
            }
        }
        if let (Some(read), true) = (&place.read, place.legacy) {
            let note = format!(
                "assertScreenshot({name}): using baseline {} from an older layout (no \
                 <W>x<H> folder); copy it to {} (or re-record with --update-baselines)",
                read.display(),
                place.write.display()
            );
            if !self.warnings.contains(&note) {
                self.warnings.push(note);
            }
        }
        // review-fixes #3: a size folder with no baseline for the name is
        // a missing baseline for that size, even when another `<W>x<H>`
        // folder holds the name (a rotation, or a second device): `=missing`
        // records it, so a farm can add a device and a first parallel run
        // can record two sizes. A plain run fails (as for any missing
        // baseline) without writing, and says how to record the size: a
        // rotated screen must not be committed by accident.
        if let (None, Some((bw, bh)), BaselineUpdate::Off) =
            (&place.read, place.other_size, self.update_baselines)
        {
            return Err(VelocityError::AssertionFailed {
                expected: format!("screen matches baseline {name}"),
                actual: format!(
                    "size mismatch: baseline is {bw}x{bh}, capture is {full_w}x{full_h}: no \
                     baseline at this size yet (a rotated screen or another device); record \
                     it with --update-baselines=missing"
                ),
                selector: name.to_string(),
                screenshot: None,
            });
        }
        // The same size folder holds a file of another pixel size (a crop
        // whose element changed size, a file copied in by hand): a real
        // mismatch. Only `--update-baselines` (changed) replaces it.
        if let (Some(read), Some((bw, bh)), Some((cw, ch)), true) = (
            &place.read,
            place.wrong_size,
            size,
            self.update_baselines != BaselineUpdate::Changed,
        ) {
            return Err(VelocityError::AssertionFailed {
                expected: format!("screen matches baseline {name}"),
                actual: format!(
                    "size mismatch: baseline {} is {bw}x{bh}, capture is {cw}x{ch}; \
                     re-record it with --update-baselines (on this device only)",
                    read.display()
                ),
                selector: name.to_string(),
                screenshot: None,
            });
        }
        let file = place
            .write
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let diffs_dir = PathBuf::from(&self.config.artifacts.output_dir).join("diffs");
        let baseline = place.read.as_ref().and_then(|p| std::fs::read(p).ok());
        // An earlier run's review record for this baseline (under the
        // current file name, or the legacy one it read) is stale once a
        // comparison passes or the baseline is rewritten: accepting it
        // would put that run's old frame back over the good baseline.
        let read_name = place
            .read
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|f| f.to_string_lossy().into_owned());
        // Only records of this baseline (this size folder and platform):
        // another device's failure of the same file name stays for review.
        let review_paths: Vec<PathBuf> = std::iter::once(place.write.clone())
            .chain(place.read.clone())
            .collect();
        let clear_review = |diffs: &std::path::Path| {
            let paths: Vec<&std::path::Path> = review_paths.iter().map(PathBuf::as_path).collect();
            crate::baselines::clear_record(diffs, &file, &paths);
            if let Some(r) = read_name.as_deref().filter(|r| *r != file) {
                crate::baselines::clear_record(diffs, r, &paths);
            }
        };

        // `--update-baselines` (changed) writes; `=missing` and plain runs
        // compare what exists.
        let checking = self.update_baselines != BaselineUpdate::Changed;
        // A baseline that isn't an image can't match anything: say which
        // file, rather than "invalid PNG" after waiting for a still screen
        // (review-s8 m-f). Update mode just replaces it.
        if let (true, Some(read), Some(b)) = (checking, &place.read, &baseline) {
            if crate::visual::png_size(b).is_none() {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("baseline {} to be a PNG image", read.display()),
                    actual: "a file that doesn't decode as one; re-record it with \
                             --update-baselines"
                        .into(),
                    selector: name.to_string(),
                    screenshot: None,
                });
            }
        }
        // Masks over most of the screen (or of the crop) leave nothing to
        // compare. Every frame-to-frame comparison failed on that, so the
        // step waited for a still screen it could never see and advised
        // masking more (review-s8 m-f): report the coverage itself, at once.
        if let Err(e) = compare_images_rule(&first, &first, &masks, threshold, cluster, rule) {
            return Err(match e {
                VelocityError::AssertionFailed {
                    expected,
                    actual,
                    screenshot,
                    ..
                } => VelocityError::AssertionFailed {
                    expected,
                    actual,
                    selector: name.to_string(),
                    screenshot,
                },
                e => e,
            });
        }
        let same = |a: &[u8], b: &[u8]| {
            compare_images_rule(a, b, &masks, threshold, cluster, rule).is_ok_and(|r| r.passed)
        };
        // A baseline this process created moments ago (an earlier attempt
        // of this test) is not one anybody reviewed: a retry must not pass
        // on it (D-V1).
        let unreviewed = place
            .read
            .as_ref()
            .is_some_and(|p| self.created_baselines.contains(p));
        if checking && !unreviewed && baseline.as_deref().is_some_and(|b| same(b, &first)) {
            clear_review(&diffs_dir);
            return Ok(());
        }

        let wait = self
            .step_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(screenshot_stable_wait);
        let expected = |f: &[u8]| checking && baseline.as_deref().is_some_and(|b| same(b, f));
        let crop_ref = crop.as_ref();
        let (previous, frame, still) = self
            .still_frame(device_id, first, wait, &same, &expected, crop_ref)
            .await?;
        if !still {
            let waited = format!("{:.1} s", wait.as_secs_f64());
            if checking && !unreviewed && baseline.as_deref().is_some_and(|b| same(b, &frame)) {
                self.warnings.push(format!(
                    "assertScreenshot({name}): the screen never held still for {waited}; \
                     its last frame matches the baseline"
                ));
                clear_review(&diffs_dir);
                return Ok(());
            }
            // Its images are now an unstable screen's: nothing to accept.
            // The record is keyed on the file read (a legacy name too).
            clear_review(&diffs_dir);
            let engine = VisualEngine::new(VisualConfig {
                diffs_dir,
                ..VisualConfig::default()
            });
            let diff = crate::visual::generate_diff_overlay_rule(&previous, &frame, &masks, rule);
            let shot = engine.save_frames(
                &file,
                &[("previous", &previous), ("actual", &frame)],
                diff.as_deref(),
            )?;
            let kept = if checking && baseline.is_some() {
                ""
            } else {
                "; not saved as the baseline"
            };
            return Err(VelocityError::AssertionFailed {
                expected: format!("a still screen to compare with baseline {name}"),
                actual: format!(
                    "the screen never held still for {waited}{kept} (mask what moves, \
                     or raise threshold/clusterPx)"
                ),
                selector: name.to_string(),
                screenshot: shot,
            });
        }

        let write_dir = place
            .write
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_default();
        if !checking {
            clear_review(&diffs_dir);
            VisualEngine::new(VisualConfig {
                threshold,
                rule,
                cluster_px: cluster,
                baselines_dir: write_dir,
                diffs_dir,
                update_baselines: true,
            })
            .assert_screenshot(&file, &frame, &masks)?;
            self.created_baselines.forget(&place.write);
            return Ok(());
        }
        let path = place.write.display().to_string();
        if unreviewed {
            return Err(baseline_created(name, &path));
        }
        let Some(read) = place.read.filter(|_| baseline.is_some()) else {
            // D-V1: written, then failed everywhere (no CI sniffing), so a
            // new assertion never passes without a reviewed baseline.
            // `--update-baselines=missing` writes it and passes.
            VisualEngine::new(VisualConfig {
                update_baselines: true,
                baselines_dir: write_dir,
                ..VisualConfig::default()
            })
            .assert_screenshot(&file, &frame, &[])?;
            clear_review(&diffs_dir);
            if self.update_baselines == BaselineUpdate::Missing {
                self.warnings.push(format!(
                    "assertScreenshot: created baseline {path}; review and commit it"
                ));
                return Ok(());
            }
            self.created_baselines.note(&place.write);
            return Err(baseline_created(name, &path));
        };
        let read_file = read
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let engine = VisualEngine::new(VisualConfig {
            threshold,
            rule,
            baselines_dir: read.parent().map(|d| d.to_path_buf()).unwrap_or_default(),
            diffs_dir: diffs_dir.clone(),
            update_baselines: false,
            cluster_px: cluster,
        });
        let r = engine.assert_screenshot(&read_file, &frame, &masks)?;
        if r.passed {
            clear_review(&diffs_dir);
            return Ok(());
        }
        let at = r
            .largest_cluster_bounds
            .map(|b| format!(" at ({}, {}) {}×{}", b.x, b.y, b.width, b.height))
            .unwrap_or_default();
        let actual = format!(
            "{:.2}% similar; largest change {} px{at}",
            r.similarity * 100.0,
            r.largest_cluster
        );
        // U1: what `velocity baselines` and the inspector's review page
        // need to accept this frame. The images are named after the file
        // read (a legacy one keeps its old name); accepting writes the
        // current layout's path at this capture size.
        let record = crate::baselines::ReviewRecord {
            name: name.to_string(),
            baseline: place.write.clone(),
            detail: actual.clone(),
            baseline_hash: None,
        };
        if let Err(e) = crate::baselines::record_failure(&diffs_dir, &read_file, &record) {
            tracing::warn!("assertScreenshot({name}): couldn't record the failure for review: {e}");
        }
        Err(VelocityError::AssertionFailed {
            expected: format!("screen matches baseline {name}"),
            actual,
            selector: name.to_string(),
            screenshot: r.diff_image_path,
        })
    }

    /// `screenshots.freezeStatusBar` (D-V4): ask the driver once per
    /// device to freeze the status bar; whether it took.
    async fn freeze_status_bar(&mut self, device_id: &str) -> bool {
        if !self.config.screenshots.freeze_status_bar || !has_status_bar(&self.config) {
            return false;
        }
        if let Some(done) = self.status_bar_frozen.get(device_id) {
            return *done;
        }
        let done = self.driver.freeze_status_bar(device_id).await;
        if !done {
            self.warnings.push(format!(
                "screenshots.freezeStatusBar: {device_id}'s status bar couldn't be frozen; \
                 it is masked instead"
            ));
        }
        self.status_bar_frozen.insert(device_id.to_string(), done);
        done
    }

    /// Where `crop:` cuts the frame, in screenshot pixels (VT-12): the
    /// element's bounds or the rect, in screen coordinates, scaled to the
    /// capture and cut to it. An element that isn't on screen, or a rect
    /// off the screen, fails the step.
    async fn screenshot_crop(
        &mut self,
        device_id: &str,
        png: &[u8],
        crop: &velocity_common::ScreenshotCrop,
    ) -> Result<crate::visual::MaskRegion> {
        let bounds = match crop {
            velocity_common::ScreenshotCrop::Rect(r) => *r,
            velocity_common::ScreenshotCrop::Element(sel) => {
                // Resolved as an action's target is, so the driver records
                // its match count for the ambiguity check below.
                let mut found = self.driver.find_element(device_id, sel).await.ok();
                if found.is_none() {
                    let tree = self.driver.get_hierarchy(device_id).await?;
                    found = velocity_common::selector_match::find_all_ref(&tree, sel, None)
                        .into_iter()
                        .next()
                        .cloned();
                }
                let found = found.ok_or_else(|| VelocityError::AssertionFailed {
                    expected: format!("crop target {sel} on screen"),
                    actual: "not found".into(),
                    selector: sel.to_string(),
                    screenshot: None,
                })?;
                // Several matches: the first may not be the one meant, and
                // the region compared would change with it. Warn as an
                // action does, or fail under selectors.strict (review-d5 m-2).
                self.note_if_ambiguous(device_id, sel, &found).await?;
                found.bounds
            }
        };
        let (sw, sh) = self.driver.screen_size(device_id).await?;
        let (pw, ph) = crate::visual::png_size(png).unwrap_or((sw as u32, sh as u32));
        let (fx, fy) = (pw as f64 / sw.max(1) as f64, ph as f64 / sh.max(1) as f64);
        let screen = velocity_common::Rect {
            x: 0,
            y: 0,
            width: sw,
            height: sh,
        };
        mask_region(&bounds, &screen, (fx, fy))
            .and_then(|r| crate::visual::clip_region(&r, pw, ph))
            .ok_or_else(|| VelocityError::AssertionFailed {
                expected: format!(
                    "crop ({}, {}) {}x{} on the {sw}x{sh} screen",
                    bounds.x, bounds.y, bounds.width, bounds.height
                ),
                actual: "nothing of it is on screen".into(),
                selector: "crop".into(),
                screenshot: None,
            })
    }

    /// A screenshot that decodes as a PNG. A corrupt capture (a truncated
    /// transfer) is taken again, twice; then it is a device failure (exit
    /// 3, an infrastructure failure the runner may retry elsewhere), not a
    /// configuration error (VEL-203) nor an app failure (review-s8 m-c).
    async fn capture_png(&mut self, device_id: &str) -> Result<Vec<u8>> {
        let mut last = 0;
        for _ in 0..3 {
            let png = self.driver.screenshot_for_compare(device_id).await?;
            if crate::visual::png_size(&png).is_some() {
                return Ok(png);
            }
            last = png.len();
        }
        Err(VelocityError::ScreenCaptureFailed {
            device_id: device_id.to_string(),
            detail: format!("3 captures in a row were not PNG images (last: {last} bytes)"),
        })
    }

    /// Capture until two frames in a row are `same`, or one is `expected`
    /// (it matches the baseline), for up to `wait`: (previous frame, last
    /// frame, whether it got there). Polls at 100, 250, 500 ms, then every
    /// second, as Playwright does.
    async fn still_frame(
        &mut self,
        device_id: &str,
        first: Vec<u8>,
        wait: Duration,
        same: &(dyn Fn(&[u8], &[u8]) -> bool + Sync),
        expected: &(dyn Fn(&[u8]) -> bool + Sync),
        crop: Option<&crate::visual::MaskRegion>,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        const PAUSES: [u64; 3] = [100, 250, 500];
        let deadline = Instant::now() + wait;
        let mut previous: Option<Vec<u8>> = None;
        let mut frame = first;
        for n in 0.. {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let pause = Duration::from_millis(PAUSES.get(n).copied().unwrap_or(1000));
            tokio::time::sleep(pause.min(left)).await;
            let next = cut(&self.capture_png(device_id).await?, crop)?;
            if expected(&next) || same(&frame, &next) {
                return Ok((frame, next, true));
            }
            previous = Some(std::mem::replace(&mut frame, next));
        }
        Ok((previous.unwrap_or_else(|| frame.clone()), frame, false))
    }

    /// Where `assertScreenshot(name)` keeps its baseline: written to
    /// `<suite dir>/velocity-baselines/<platform>[-headless]/<W>x<H>/<file>`
    /// (`-headless` so headless renders never meet device baselines, B1;
    /// `W`x`H` the full capture, so devices and orientations get their own,
    /// D-V3). Read from there, else (with a warning) from where older
    /// versions kept it: no size folder, the name's old file name, the
    /// shared platform folder for headless runs, or the folder the run
    /// started in (B2). A legacy file is used only when it is `size`, the
    /// size of what is compared.
    fn baseline_place(
        &self,
        name: &str,
        (w, h): (u32, u32),
        size: Option<(u32, u32)>,
    ) -> BaselinePlace {
        let platform = self
            .config
            .platform
            .map(|p| p.to_string())
            .unwrap_or_else(|| "device".into());
        let headless =
            self.config.driver.unwrap_or_default() == velocity_common::DriverMode::Headless;
        let space = crate::baselines::baseline_space(&platform, headless);
        let suite_dir = self.config.suite_dir.clone().unwrap_or_default();
        let root = suite_dir.join(crate::baselines::BASELINES_DIR);
        let write = crate::baselines::baseline_path(&suite_dir, &space, (w, h), name);
        let file = baseline_file(name);
        let sized = write
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let mut files = vec![file];
        let old = legacy_baseline_file(name);
        if !files.contains(&old) {
            files.push(old);
        }
        let current = files.iter().map(|f| sized.join(f)).find(|p| p.is_file());
        if current.is_some() {
            let wrong_size = current
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| crate::visual::png_size(&b))
                .filter(|s| size.is_some_and(|want| want != *s));
            return BaselinePlace {
                legacy: current.as_ref().is_some_and(|r| *r != write),
                read: current,
                write,
                other_size: None,
                wrong_size,
            };
        }
        let mut dirs = vec![root.join(&space)];
        if headless {
            dirs.push(root.join(&platform));
        }
        let cwd = PathBuf::from("velocity-baselines");
        if cwd != root {
            dirs.push(cwd.join(&space));
            if headless {
                dirs.push(cwd.join(&platform));
            }
        }
        let fits = |p: &PathBuf| {
            size.is_none()
                || std::fs::read(p)
                    .ok()
                    .and_then(|b| crate::visual::png_size(&b))
                    .is_none_or(|s| Some(s) == size)
        };
        let read = dirs
            .iter()
            .flat_map(|d| files.iter().map(move |f| d.join(f)))
            .find(|p| p.is_file() && fits(p));
        // review-d5 m-3: the same name at another capture size.
        let other_size = if read.is_none() {
            let mut sizes: Vec<(u32, u32)> = std::fs::read_dir(root.join(&space))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let (sw, sh) = name.split_once('x')?;
                    let size = (sw.parse().ok()?, sh.parse().ok()?);
                    (size != (w, h) && files.iter().any(|f| e.path().join(f).is_file()))
                        .then_some(size)
                })
                .collect();
            sizes.sort_unstable();
            sizes.first().copied()
        } else {
            None
        };
        BaselinePlace {
            legacy: read.is_some(),
            read,
            write,
            other_size,
            wrong_size: None,
        }
    }

    /// What `assertScreenshot` leaves out of the comparison: the system
    /// bars the driver reports (Android's SystemUI windows, iOS's status
    /// bar), a fixed guess only when it can't say, and the `mask` elements,
    /// each cut to the screen.
    #[cfg(test)]
    async fn screenshot_masks(
        &mut self,
        device_id: &str,
        png: &[u8],
        mask: &[Selector],
    ) -> Result<Vec<crate::visual::MaskPreset>> {
        self.screenshot_masks_with(device_id, png, mask, true).await
    }

    /// [`Self::screenshot_masks`]; `bars: false` leaves the system bars in
    /// (their content is frozen, D-V4).
    async fn screenshot_masks_with(
        &mut self,
        device_id: &str,
        png: &[u8],
        mask: &[Selector],
        bars: bool,
    ) -> Result<Vec<crate::visual::MaskPreset>> {
        use crate::visual::{MaskPreset, MaskRegion};
        // The system bars the driver reports (Android's SystemUI windows,
        // iOS's status bar); a fixed guess only when it can't say.
        let bars = if bars && has_status_bar(&self.config) {
            self.driver.system_bars(device_id).await
        } else {
            Some(Vec::new())
        };
        let mut masks = Vec::new();
        let guess = bars.is_none();
        if guess {
            masks.push(MaskPreset::StatusBar);
        }
        let ios = guess && self.config.platform == Some(velocity_common::Platform::Ios);
        let mut regions = bars.unwrap_or_default();
        // Each mask resolves through the driver, as actions do (web
        // `css:`/`xpath:` included, VEL-223), and every match is masked: a
        // list of rows with a timestamp each (B4). The shared matcher over
        // the tree covers drivers whose own lookup finds nothing.
        let mut tree = None;
        for sel in mask {
            let mut found: Vec<velocity_common::Rect> = self
                .driver
                .find_elements(device_id, sel)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.bounds)
                .collect();
            if found.is_empty() {
                if tree.is_none() {
                    tree = Some(self.driver.get_hierarchy(device_id).await?);
                }
                if let Some(t) = &tree {
                    found = velocity_common::selector_match::find_all_ref(t, sel, None)
                        .into_iter()
                        .map(|e| e.bounds)
                        .collect();
                }
            }
            if found.is_empty() {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("mask target {sel} on screen"),
                    actual: "not found".into(),
                    selector: sel.to_string(),
                    screenshot: None,
                });
            }
            regions.extend(found);
        }
        if !regions.is_empty() || ios {
            let (sw, sh) = self.driver.screen_size(device_id).await?;
            let (pw, ph) = crate::visual::png_size(png).unwrap_or((sw as u32, sh as u32));
            // Tree coordinates are points on iOS, pixels on Android.
            let (fx, fy) = (pw as f64 / sw.max(1) as f64, ph as f64 / sh.max(1) as f64);
            if ios {
                // The status bar is up to 62 pt tall (Dynamic Island
                // iPhones): 186 px at 3x, more than the preset's 4%.
                masks.push(MaskPreset::Custom(MaskRegion {
                    x: 0,
                    y: 0,
                    width: pw,
                    height: (IOS_STATUS_BAR_PT * fy).ceil() as u32,
                }));
            }
            let screen = velocity_common::Rect {
                x: 0,
                y: 0,
                width: sw,
                height: sh,
            };
            masks.extend(
                regions
                    .iter()
                    .filter_map(|b| mask_region(b, &screen, (fx, fy)))
                    .map(MaskPreset::Custom),
            );
        }
        Ok(masks)
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

    /// Find an element, waiting for it to appear (up to `timeout_ms`).
    ///
    /// Waiting on the element itself is the fix the flaky-test literature
    /// keeps landing on — UI sync can report "idle" while a network response
    /// is still on its way, so a single lookup after sync is a race. The first
    /// lookup reuses the tree the sync just captured; retries back off from
    /// 100ms to 500ms and force a fresh hierarchy each time. Polling never
    /// heals — an element that hasn't rendered yet must be waited for, not
    /// healed onto a similar element from the previous screen. Healing runs
    /// once, only after the wait is exhausted.
    async fn find_element_smart(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
        allow_healing: bool,
    ) -> Result<Element> {
        let deadline = deadline_in(timeout_ms);
        let mut attempt = 0usize;
        let mut crash_checked = Instant::now();
        self.lookup_waited = false;

        loop {
            // I3: a crash the driver already knows of (a device's crash
            // watcher) ends the wait at once, at no cost.
            if let Some(e) = self.pending_crash(device_id).await {
                return Err(e);
            }
            // A crashed app never shows the element: stop waiting as soon as
            // the crash log says so (the Android crash test took 16 s, the
            // whole element timeout, to say "crashed"). At most once a second,
            // so ordinary waits barely pay for it.
            if attempt > 0 && crash_checked.elapsed() >= crash_check_every() {
                crash_checked = Instant::now();
                let app_id = self.app_id.clone();
                if let Some(detail) = self.driver.crash_report(device_id, &app_id).await {
                    return Err(VelocityError::AppCrashed { app_id, detail });
                }
            }
            if attempt > 0 {
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                // A dialog can appear while we wait (Playwright re-runs its
                // pre-checks on every retry); its read primes the cache the
                // lookup then uses.
                self.dialog_handler
                    .dismiss_unless_target(self.driver, device_id, Some(selector))
                    .await;
            }
            match self
                .selector_engine
                .find_element_no_heal(self.driver, device_id, selector)
                .await
            {
                Ok(el) => {
                    self.trace.record_match(&el);
                    self.trace_cached_tree(device_id, !el.bounds.is_empty())
                        .await;
                    return Ok(el);
                }
                // The device is gone, or the setup is wrong: waiting for the
                // element is pointless.
                Err(e) if is_infrastructure(&e) || is_config(&e) => return Err(e),
                Err(e) => {
                    let pause = next_pause(attempt, deadline, Duration::from_millis(timeout_ms));
                    attempt += 1;
                    self.lookup_waited = true;
                    let Some(delay) = pause else {
                        // One last look that may match a part of a merged
                        // label (RN's "Points, 120, View rewards"): only now,
                        // so it never beats the real target to the screen.
                        if let Some(el) = self.find_loosely(device_id, selector).await? {
                            return Ok(el);
                        }
                        if allow_healing {
                            let heals = self.selector_engine.heal_count();
                            let healed = self
                                .selector_engine
                                .find_element(self.driver, device_id, selector)
                                .await
                                .map_err(|e| with_timeout(e, timeout_ms));
                            if let Ok(el) = &healed {
                                // `find_element` looks the selector up first
                                // and heals only on a miss: a target that
                                // arrived just now is the real one (M1).
                                let did_heal = self.selector_engine.heal_count() > heals;
                                // `healing.mode: report` (D-7): the heal is
                                // proposed, not used.
                                if did_heal
                                    && self.heal_mode == velocity_common::HealingMode::Report
                                {
                                    let err = self.heal_proposed(selector, timeout_ms);
                                    self.selector_engine.unuse_last_heal();
                                    return Err(err);
                                }
                                self.trace.record_match(el);
                            }
                            return healed;
                        }
                        return Err(with_timeout(e, timeout_ms));
                    };
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// The failure for a heal that `healing.mode: report` doesn't use: the
    /// patch it proposes and the command that applies it (D-7).
    fn heal_proposed(&self, selector: &Selector, timeout_ms: u64) -> VelocityError {
        let suite = self.suite_file.as_deref().unwrap_or("<suite.yaml>");
        // This step's heal (M1): the last mapping from this selector to the
        // one it just healed to, never another step's or test's.
        let from = selector.to_string();
        let to = self
            .selector_engine
            .healed_to(selector)
            .map(|s| s.to_string());
        let mapping = self
            .selector_engine
            .healer()
            .healed_mappings()
            .into_iter()
            .rev()
            .find(|m| m.original_selector == from && Some(&m.healed_selector) == to.as_ref());
        let patch = match &mapping {
            Some(m) => match m.rewrite() {
                Some((key, old, new)) => format!(
                    "{key}: {old:?} → {key}: {new:?} (confidence {:.2})",
                    m.confidence
                ),
                None => format!(
                    "{} → {} (confidence {:.2}; edit by hand)",
                    m.original_selector, m.healed_selector, m.confidence
                ),
            },
            None => "the healed selector".to_string(),
        };
        let patch = velocity_common::redact::redact(&patch).into_owned();
        let reject = reject_hint(&from, suite);
        VelocityError::ElementNotFound {
            selector: format!(
                "{selector} (healing.mode is report: the heal was not used. Proposed patch: \
                 {patch}. Accept it with `velocity heal --accept --config {suite}`, or {reject})"
            ),
            timeout_ms,
            screenshot: None,
            hierarchy_snapshot: None,
        }
    }

    /// `selector` matched against parts of merged labels too (the widest
    /// tier), once, with a warning when that found what the stricter tiers
    /// didn't. Under a wider ceiling than the step's it is a no-op.
    async fn find_loosely(
        &mut self,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Option<Element>> {
        use velocity_common::selector_match::{ceiling, with_ceiling, Tier};
        if ceiling() >= Tier::Loose {
            return Ok(None);
        }
        self.selector_engine.invalidate_cache();
        self.driver.invalidate_hierarchy_cache().await;
        let found = with_ceiling(
            Tier::Loose,
            self.selector_engine
                .find_element_no_heal(self.driver, device_id, selector),
        )
        .await;
        match found {
            Ok(el) => {
                let note = format!(
                    "{selector} matched only part of an element's text or label \
                     ({}); use textContains or the whole label to say so",
                    crate::healing::describe_element(&el)
                );
                if !self.warnings.contains(&note) {
                    self.warnings.push(note);
                }
                self.loose_found = true;
                self.trace.record_match(&el);
                Ok(Some(el))
            }
            Err(e) if is_infrastructure(&e) || is_crash(&e) => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// The actionability rule for the running step (D-3): `force: true`
    /// turns the checks off, `actionability: lenient` makes them warn.
    fn act_mode(&self) -> velocity_common::actionability::ActMode {
        use velocity_common::actionability::ActMode;
        if self.step_force {
            ActMode::Force
        } else if self.config.actionability == velocity_common::Actionability::Lenient {
            ActMode::Lenient
        } else {
            ActMode::Strict
        }
    }

    /// Find the target of a tap, press or text entry and check it can take
    /// the action (D-3): enabled, and at the same place in two reads in a
    /// row (a moving target gets the tap at its old position). Waits up to
    /// `actionableWaitMs` (2 s); then the step fails as `NotActionable`,
    /// or, with `actionability: lenient`, acts anyway with a warning (the
    /// rule before D-3, when only an unsettled screen asked for the second
    /// read). `force: true` skips both checks.
    async fn find_actionable(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
    ) -> Result<Element> {
        self.find_actionable_as(device_id, selector, timeout_ms, true)
            .await
    }

    /// A `within:` container to drag: it must hold still, but a scroll
    /// view that reports `enabled=false` is still dragged (with a warning).
    async fn find_container(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
    ) -> Result<Element> {
        self.find_actionable_as(device_id, selector, timeout_ms, false)
            .await
    }

    async fn find_actionable_as(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
        must_be_enabled: bool,
    ) -> Result<Element> {
        use velocity_common::actionability::ActMode;
        self.loose_found = false;
        // Only the step's first lookup may lean on its settle.
        let settled = std::mem::take(&mut self.settled_for_lookup);
        let mut el = self
            .find_element_smart(device_id, selector, timeout_ms, true)
            .await?;
        // Counted before the bounds check: a WDA hit (no bounds) can be
        // ambiguous too (R5).
        self.note_if_ambiguous(device_id, selector, &el).await?;
        // Elements from a direct query (no bounds) have nothing to check.
        if el.bounds.is_empty() {
            return Ok(el);
        }
        let mode = self.act_mode();
        if mode == ActMode::Force {
            return Ok(el);
        }
        // Strict: proof the target holds still (VEL-072, P13). The settle
        // that just saw two identical trees in a row (or the probe's idle)
        // is that proof, when the lookup hit at once on the screen it left:
        // a second read is a full `/source` on iOS, ~540 ms a tap (review
        // M4). Only a capped or overturned settle, none at all, or a lookup
        // that had to wait asks for the re-read. Web checks stability in
        // the page (`act`'s two-frame check), so not here too. Lenient:
        // only when the screen hadn't settled by the cap.
        let proven_still = settled && !self.lookup_waited && !self.loose_found;
        let web = self.config.platform == Some(Platform::Web);
        let mut need_stable = match mode {
            ActMode::Strict => !proven_still && !web,
            _ => self.sync_manager.last_settle().capped,
        };
        if el.enabled && !need_stable {
            return Ok(el);
        }
        let limit = Instant::now() + actionable_wait().min(Duration::from_millis(timeout_ms));
        // A healed target is waited for under the selector it healed to: the
        // original one just failed, so re-querying it read as "gone" (N-3).
        let healed = self.selector_engine.healed_to(selector);
        let selector = healed.as_ref().unwrap_or(selector);
        // The first re-read comes a frame later; waits for a disabled
        // target back off as before.
        let mut attempt = if el.enabled { 0 } else { 1 };
        loop {
            tokio::time::sleep(retry_delay(attempt)).await;
            attempt += 1;
            self.selector_engine.invalidate_cache();
            self.driver.invalidate_hierarchy_cache().await;
            // A target found in the widest tier is re-read in it.
            let max = if self.loose_found {
                velocity_common::selector_match::Tier::Loose
            } else {
                velocity_common::selector_match::ceiling()
            };
            let vanished = match velocity_common::selector_match::with_ceiling(
                max,
                self.selector_engine
                    .find_element_no_heal(self.driver, device_id, selector),
            )
            .await
            {
                Ok(again) => {
                    if bounds_close(&again.bounds, &el.bounds) {
                        need_stable = false;
                    }
                    el = again;
                    false
                }
                Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => return Err(e),
                Err(_) => true,
            };
            let enabled_ok = el.enabled || !must_be_enabled;
            if enabled_ok && !need_stable && !vanished {
                if !el.enabled {
                    let note = format!("target {selector} was disabled{ACTED_ANYWAY}");
                    if !self.warnings.contains(&note) {
                        self.warnings.push(note);
                    }
                }
                return Ok(el);
            }
            if Instant::now() >= limit {
                // Never act on an element that is no longer on screen: its
                // old bounds now hold something else.
                if vanished {
                    return Err(VelocityError::ElementNotFound {
                        selector: format!(
                            "{selector} (gone while waiting for it to be actionable)"
                        ),
                        timeout_ms,
                        screenshot: None,
                        hierarchy_snapshot: None,
                    });
                }
                let why = if !enabled_ok {
                    "was disabled"
                } else {
                    "was still moving"
                };
                if mode == ActMode::Strict {
                    let waited = actionable_wait().min(Duration::from_millis(timeout_ms));
                    let reason = if enabled_ok {
                        // n1: say what was wrong. Two reads in a row with the
                        // same box (±1 px) pass, so a view caught at rest
                        // even once is tapped.
                        format!(
                            "it was still moving after {} ms: its position or size changed \
                             between every two reads (a continuously animating view: a \
                             pulsing button, a marquee, a carousel?). If tapping it mid-motion \
                             is fine, add `force: true` to the step, or set `actionability: \
                             lenient` in the suite config",
                            waited.as_millis()
                        )
                    } else {
                        format!(
                            "it {why} after {} ms. If the view works anyway (a custom or \
                             React Native view that reports enabled=false), add `force: true` \
                             to the step, or set `actionability: lenient` in the suite config",
                            waited.as_millis()
                        )
                    };
                    return Err(VelocityError::NotActionable {
                        selector: selector.to_string(),
                        reason,
                    });
                }
                let note = format!("target {selector} {why}{ACTED_ANYWAY}");
                if !self.warnings.contains(&note) {
                    self.warnings.push(note);
                }
                return Ok(el);
            }
        }
    }

    async fn screen_rect(&mut self, device_id: &str) -> Option<velocity_common::Rect> {
        self.driver
            .screen_size(device_id)
            .await
            .ok()
            .map(|(w, h)| velocity_common::Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            })
    }

    /// The on-screen part of a `within:` container, for the strokes of a
    /// scroll inside it. A container below the fold is brought onto the
    /// screen first (VEL-069: the wheel or drag went to off-screen
    /// coordinates and nothing ever moved), and its bounds are read again
    /// after the actionability check, which may itself have scrolled it in
    /// (web scrolls a target into view).
    async fn scroll_area(
        &mut self,
        device_id: &str,
        container: &Selector,
    ) -> Result<velocity_common::Rect> {
        let timeout = self.element_timeout_ms();
        let found = self.find_container(device_id, container, timeout).await?;
        // Drivers that can scroll an element into view do it first (web,
        // VEL-069); the rest return it unchanged and the drags below apply.
        let mut bounds = self.driver.reveal(device_id, &found).await?.bounds;
        let Some(screen) = self.screen_rect(device_id).await else {
            return Ok(bounds);
        };
        // Drags to bring it in happen once per step: a `scrollUntilVisible`
        // asks for the area on every scroll (review m5).
        let drags = if self.area_dragged {
            0
        } else {
            CONTAINER_DRAGS
        };
        for attempt in 0..=drags {
            if attempt > 0 || !bounds.is_empty() {
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                match self
                    .selector_engine
                    .find_element_no_heal(self.driver, device_id, container)
                    .await
                {
                    Ok(el) => {
                        // A drag that moved nothing won't move it next time.
                        if attempt > 0 && el.bounds == bounds {
                            break;
                        }
                        bounds = el.bounds
                    }
                    Err(e) if is_infrastructure(&e) || is_crash(&e) => return Err(e),
                    Err(_) => {}
                }
            }
            if bounds.is_empty() {
                return Ok(bounds);
            }
            if let Some(shown) = shown_part(&screen, &bounds) {
                return Ok(shown);
            }
            if attempt == drags {
                break;
            }
            self.area_dragged = true;
            // Off screen on some axis: drag the page to bring its middle in.
            let (_, cy) = bounds.center();
            let axis = if cy < screen.y || cy > screen.y + screen.height {
                Direction::Down
            } else {
                Direction::Right
            };
            let Some((a, b)) = aim_at(&screen, &bounds, axis, false) else {
                break;
            };
            self.driver.scroll_coords(device_id, a, b).await?;
            let _ = self
                .sync_manager
                .wait_for_idle(self.driver, device_id)
                .await;
        }
        // Never stroke off the screen (VEL-069): what shows of it, or fail.
        intersection(&screen, &bounds).ok_or_else(|| VelocityError::NotActionable {
            selector: container.to_string(),
            reason: "the within: container is off screen and dragging didn't bring it in".into(),
        })
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

    async fn execute_action(
        &mut self,
        action: &Action,
        device_id: &str,
        app_id: &str,
    ) -> Result<()> {
        match action {
            Action::MockRequest { rule } => {
                self.mocked = true;
                self.driver.add_mock(device_id, rule.clone()).await
            }
            Action::ClearMocks => self.driver.clear_mocks(device_id).await,
            Action::Http(req) => self.run_http(req).await,
            Action::SetConfig { values } => {
                // Remember what the test started with, once: the test's end
                // puts it back (`restore_test_config`).
                if self.config_before.is_none() {
                    self.config_before = Some((
                        self.config.sync.clone(),
                        velocity_common::env::settings_snapshot(),
                    ));
                }
                let mut named = velocity_common::env::settings_snapshot();
                for (k, v) in values {
                    check_set_config(k, v).map_err(VelocityError::Config)?;
                    if let Some(s) = velocity_common::settings::find(k) {
                        named.insert(s.env.to_string(), v.clone());
                    } else {
                        set_sync_field(&mut self.config.sync, k, v)
                            .map_err(VelocityError::Config)?;
                    }
                }
                velocity_common::env::set_settings(named);
                *self.sync_manager.sync_config_mut() = self.config.sync.clone();
                Ok(())
            }
            Action::AssertSystemState {
                media_playing,
                media_title,
                in_call,
            } => {
                // Polled like any assertion: playback or a call can start a
                // moment after the tap that caused it.
                let deadline = self.assertion_deadline();
                let mut attempt = 0usize;
                loop {
                    let state = self.driver.system_state(device_id).await?;
                    let problems = system_state_mismatch(
                        &state,
                        *media_playing,
                        media_title.as_deref(),
                        *in_call,
                    );
                    if problems.is_empty() {
                        return Ok(());
                    }
                    let pause = next_pause(
                        attempt,
                        deadline,
                        Duration::from_millis(self.element_timeout_ms()),
                    );
                    attempt += 1;
                    let Some(delay) = pause else {
                        return Err(VelocityError::AssertionFailed {
                            expected: problems.join("; "),
                            actual: format!(
                                "media playing={} titles={:?}, in call={}",
                                state.media_playing, state.media_titles, state.in_call
                            ),
                            selector: "system state".into(),
                            screenshot: None,
                        });
                    };
                    tokio::time::sleep(delay).await;
                }
            }
            Action::LaunchApp {
                app_id: override_id,
                clear_state,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                // A stronger reset (reinstall / snapshot) replaces the clear;
                // if it can't run, fall back to the plain clear.
                let mut clear = *clear_state;
                if clear && self.config.reset.strategy != velocity_common::ResetStrategy::Clear {
                    match self
                        .driver
                        .reset_app(device_id, id, &self.config.reset)
                        .await
                    {
                        Ok(done) => {
                            tracing::debug!(?done, "app reset");
                            clear = false;
                        }
                        Err(e) => {
                            let note = format!(
                                "reset strategy {:?} unavailable ({e}); used clear instead",
                                self.config.reset.strategy
                            );
                            if !self.warnings.contains(&note) {
                                self.warnings.push(note);
                            }
                        }
                    }
                }
                let result = self.driver.launch_app(device_id, id, clear).await;
                if result.is_ok() {
                    // Another app in front: the suite's app is no longer
                    // the one a batched next test would find.
                    self.app_stopped = id != self.app_id;
                    if id == self.app_id {
                        // A fresh process: an earlier crash is history.
                        self.sticky_crash = None;
                    }
                    // The probe binds its port inside the freshly launched app —
                    // re-probe native sync instead of trusting a pre-launch failure.
                    self.sync_manager.reset_native();
                }
                result
            }
            Action::StopApp {
                app_id: override_id,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                let result = self.driver.stop_app(device_id, id).await;
                if result.is_ok() && id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::Tap { selector } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.tap(device_id, &element).await
            }
            Action::DoubleTap { selector } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.double_tap(device_id, &element).await
            }
            Action::LongPress {
                selector,
                duration_ms,
            } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                let ms = duration_ms.unwrap_or(1000);
                self.driver.long_press(device_id, &element, ms).await
            }
            Action::InputText { selector, text } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.input_text(device_id, &element, text).await
            }
            Action::ClearText { selector } => {
                // Like inputText: RN's clearText taps the field first, so it
                // must be actionable, not just present (review MINOR 11).
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.clear_text(device_id, &element).await
            }
            Action::AssertVisible { selector } => {
                // Waits for visible, not just present (Playwright's toBeVisible,
                // Maestro's assertVisible): on the web a matching node is often
                // in the DOM but still hidden for a moment — a screen that the
                // next frame shows — and failing on the first read lost that race.
                let timeout = self.element_timeout_ms();
                let deadline = deadline_in(timeout);
                let mut attempt = 0usize;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    self.loose_found = false;
                    let element = self
                        .find_element_smart(device_id, selector, left.as_millis() as u64, false)
                        .await?;
                    // D-10: one part of a merged label ("Home, tab, 1 of 4")
                    // is not the element the assertion names: fail, naming
                    // the label it was part of.
                    if self.loose_found {
                        self.loose_found = false;
                        return Err(VelocityError::AssertionFailed {
                            expected: "visible".to_string(),
                            actual: format!(
                                "only part of the merged label {:?} matched ({}); assert \
                                 the whole label, or use textContains to mean a part",
                                merged_label(&element),
                                crate::healing::describe_element(&element)
                            ),
                            selector: format!("{selector}"),
                            screenshot: None,
                        });
                    }
                    if self.driver.is_element_visible(device_id, &element).await? {
                        return Ok(());
                    }
                    let pause = next_pause(attempt, deadline, Duration::from_millis(timeout));
                    attempt += 1;
                    let Some(delay) = pause else {
                        return Err(VelocityError::AssertionFailed {
                            expected: "visible".to_string(),
                            actual: format!("present but not visible after {timeout}ms"),
                            selector: format!("{selector}"),
                            screenshot: None,
                        });
                    };
                    tokio::time::sleep(delay).await;
                    self.selector_engine.invalidate_cache();
                    self.driver.invalidate_hierarchy_cache().await;
                }
            }
            Action::AssertNotVisible { selector } => {
                // Passes as soon as the element is gone; waits (up to the sync
                // timeout) while it's still there — a spinner or toast that is
                // on its way out shouldn't fail the step.
                // Never heal here: healing onto a similar element would make a
                // legitimately-removed element look "still visible".
                let deadline = self.assertion_deadline();
                let mut poll = Duration::from_millis(100);
                // Only a successful read without the element means "gone".
                // A lookup that failed (a garbled dump, a dead session) says
                // nothing about the screen: keep reading, and if no read
                // works by the deadline, fail with that error.
                let mut unreadable = None;
                let gone = loop {
                    let gone = match self
                        .selector_engine
                        .find_element_no_heal(self.driver, device_id, selector)
                        .await
                    {
                        Err(e) if is_not_found(&e) => true,
                        // A lost device or a crashed app is not "the element is gone".
                        Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                            return Err(e)
                        }
                        Err(e) => {
                            unreadable = Some(e);
                            false
                        }
                        Ok(element) => {
                            unreadable = None;
                            !self.driver.is_element_visible(device_id, &element).await?
                        }
                    };
                    if gone || !self.poll_pause(&mut poll, deadline).await {
                        break gone;
                    }
                };
                if let (false, Some(e)) = (gone, unreadable) {
                    return Err(e);
                }
                // A crashed app shows none of its elements: that's not a pass.
                if gone {
                    if let Some(e) = self.check_alive(device_id).await {
                        return Err(e);
                    }
                    // Another app's screen lacks the element too: that is no
                    // pass (D-2, VEL-014), unless the step says so
                    // (`appInFront: false`) or the test itself left the app
                    // (openLink/goto, another app's launchApp, the home key).
                    if let Some(velocity_common::Liveness::Background(front)) = &self.liveness_seen
                    {
                        let front = front.as_deref().unwrap_or("another app").to_string();
                        if self.step_app_in_front == Some(false) || self.app_switched {
                            let note = format!(
                                "assertNotVisible {selector}: the app under test wasn't in front ({front} was); \
                                 the element was checked on that screen"
                            );
                            if !self.warnings.contains(&note) {
                                self.warnings.push(note);
                            }
                        } else {
                            return Err(VelocityError::AssertionFailed {
                                expected: format!(
                                    "not visible in {} (the app under test)",
                                    self.app_id
                                ),
                                actual: format!(
                                    "{front} was in front, so the check proves nothing; \
                                     add `appInFront: false` to the step if that is intended"
                                ),
                                selector: format!("{selector}"),
                                screenshot: None,
                            });
                        }
                    }
                }
                if !gone {
                    return Err(VelocityError::AssertionFailed {
                        expected: "not visible".to_string(),
                        actual: "visible".to_string(),
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::AssertText { selector, expected } => {
                // Text often settles after the element appears (a counter, a
                // price loading in), so keep reading until it matches.
                let timeout = self.element_timeout_ms();
                self.loose_found = false;
                self.find_element_smart(device_id, selector, timeout, false)
                    .await?;
                // Found only as part of a merged label at the deadline: read
                // that element, not one the polling tier never sees (which
                // waited a second timeout and failed with "": review-s8 m-b).
                let tier = if self.loose_found {
                    velocity_common::selector_match::Tier::Loose
                } else {
                    velocity_common::selector_match::ceiling()
                };
                let deadline = self.assertion_deadline();
                let mut poll = Duration::from_millis(100);
                let mut actual = String::new();
                let matched = loop {
                    let found = velocity_common::selector_match::with_ceiling(
                        tier,
                        self.selector_engine
                            .find_element_no_heal(self.driver, device_id, selector),
                    )
                    .await;
                    match found {
                        Ok(element) => {
                            // The driver compares: on web both sides are
                            // normalised alike (M-1).
                            let (hit, text) = self
                                .driver
                                .element_text_matches(device_id, &element, expected)
                                .await?;
                            actual = text;
                            if hit {
                                break true;
                            }
                        }
                        // A lost device is not a wrong text.
                        Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                            return Err(e)
                        }
                        Err(_) => {}
                    }
                    if !self.poll_pause(&mut poll, deadline).await {
                        break false;
                    }
                };
                if !matched {
                    return Err(VelocityError::AssertionFailed {
                        expected: expected.clone(),
                        actual,
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::ScrollUntilVisible {
                selector,
                direction,
                max_scrolls,
                within,
            } => {
                // `direction` is where the content scrolls (Maestro's meaning:
                // `down` reveals what is below), so the finger moves the other
                // way. It used to be passed to `swipe`, whose direction is the
                // finger's: `down` dragged the list back to the top and nothing
                // below the fold was ever found (VelocityLab, Android and iOS).
                let finger = finger_for_scroll(*direction);
                let started = std::time::Instant::now();
                let mut last_aimed: Option<velocity_common::Rect> = None;
                for _ in 0..*max_scrolls {
                    // The container first: bringing it on screen can scroll
                    // the page (web's `reveal`), and the target read before
                    // that was aimed at with stale bounds (review M-1).
                    let within_area = match within {
                        Some(container) => Some(self.scroll_area(device_id, container).await?),
                        None => None,
                    };
                    // No healing while scrolling — a healed near-match would
                    // stop the scroll before the real element is on screen.
                    let found = self
                        .selector_engine
                        .find_element_no_heal(self.driver, device_id, selector)
                        .await;
                    if let Err(e) = &found {
                        if is_infrastructure(e) || is_crash(e) || is_config(e) {
                            return found.map(|_| ());
                        }
                    }
                    let mut aimed = None;
                    if let Ok(element) = found {
                        let visible = self
                            .driver
                            .is_element_visible(device_id, &element)
                            .await
                            .unwrap_or(false);
                        // Already in the tree but off screen (iOS lists keep
                        // ~20 rows past each edge): drag straight to it. A
                        // fling moves more than a screen, so a row could pass
                        // from below the screen to above it between two
                        // reads and never be "visible" (the scroll ran past
                        // row_150 to the end).
                        let area = match within_area {
                            Some(area) => Some(area),
                            None => self.screen_rect(device_id).await,
                        };
                        aimed = area.and_then(|a| aim_at(&a, &element.bounds, *direction, visible));
                        if visible && aimed.is_none() {
                            return Ok(());
                        }
                        // VEL-068: the last drag didn't move it (a fixed
                        // header, a screen that can't scroll, the end of a
                        // list): it is as far in as it will get.
                        if visible && aimed.is_some() && last_aimed == Some(element.bounds) {
                            return Ok(());
                        }
                        last_aimed = aimed.map(|_| element.bounds);
                    }
                    self.selector_engine.invalidate_cache();
                    if let Some((a, b)) = aimed {
                        self.driver.scroll_coords(device_id, a, b).await?;
                        let _ = self
                            .sync_manager
                            .wait_for_idle(self.driver, device_id)
                            .await;
                        continue;
                    }
                    match within_area {
                        // Drag the named container, not the middle of the
                        // screen: on a two-pane tablet the middle is the other
                        // pane (a POS tablet's Settings list never moved).
                        Some(area) => {
                            let (a, b) = line_across(&area, finger);
                            self.driver.scroll_coords(device_id, a, b).await?;
                        }
                        None => self.driver.scroll(device_id, finger).await?,
                    }
                    let _ = self
                        .sync_manager
                        .wait_for_idle(self.driver, device_id)
                        .await;
                }
                // The last scroll may have revealed it: look once more.
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                match self
                    .selector_engine
                    .find_element_no_heal(self.driver, device_id, selector)
                    .await
                {
                    Ok(element) => {
                        if self
                            .driver
                            .is_element_visible(device_id, &element)
                            .await
                            .unwrap_or(false)
                        {
                            return Ok(());
                        }
                    }
                    Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                        return Err(e)
                    }
                    Err(_) => {}
                }
                Err(VelocityError::ElementNotFound {
                    selector: format!("{selector} ({max_scrolls} scrolls {direction:?})"),
                    timeout_ms: started.elapsed().as_millis() as u64,
                    screenshot: None,
                    hierarchy_snapshot: None,
                })
            }
            Action::Swipe {
                direction,
                from,
                to,
                within,
            } => {
                if let (Some(from), Some(to)) = (from, to) {
                    self.driver.swipe_coords(device_id, *from, *to).await
                } else if let Some(container) = within {
                    // Across the element (a carousel above the fold): the
                    // middle of the screen missed it and nothing moved.
                    let timeout = self.element_timeout_ms();
                    let el = self.find_container(device_id, container, timeout).await?;
                    let (a, b) = line_across(&el.bounds, direction.unwrap_or(Direction::Down));
                    self.driver.swipe_coords(device_id, a, b).await
                } else {
                    let dir = direction.unwrap_or(Direction::Down);
                    // One stroke on every device driver: each drew its own
                    // (Android 40%, iOS/RN half, Flutter a third of the
                    // screen), so a suite scrolled further on one platform
                    // than another (VEL-S53). Web scrolls the page by its
                    // own rule; a driver with no screen size keeps its own.
                    let size = if self.config.platform == Some(Platform::Web) {
                        None
                    } else {
                        self.driver.screen_size(device_id).await.ok()
                    };
                    match size {
                        Some((w, h)) if w > 0 && h > 0 => {
                            let (a, b) = default_swipe_line(w, h, dir);
                            self.driver.swipe_coords(device_id, a, b).await
                        }
                        _ => self.driver.swipe(device_id, dir).await,
                    }
                }
            }
            Action::Screenshot { filename } => {
                use velocity_common::ScreenshotPolicy;
                let policy = self.config.artifacts.screenshots;
                if policy == ScreenshotPolicy::Never {
                    return Ok(());
                }
                let data = self.driver.screenshot(device_id).await?;
                // The file name only: `../x.png` or an absolute path wrote
                // outside the artifacts dir (VEL-197).
                let fname = filename
                    .as_deref()
                    .and_then(|f| std::path::Path::new(f).file_name())
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("screenshot_{}.png", chrono_millis()));
                let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
                if policy == ScreenshotPolicy::OnFailure {
                    self.held_shots.push((path, data));
                    return Ok(());
                }
                // Best-effort write; don't fail the step if artifacts dir doesn't exist
                write_artifact(&path, data).await;
                Ok(())
            }
            // Both leave the app stopped on purpose (see the trait): its
            // exit is not a crash, and the next test must launch it.
            Action::SaveState { name } => {
                let result = self.driver.save_app_state(device_id, app_id, name).await;
                if result.is_ok() && app_id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::RestoreState { name } => {
                let result = self.driver.restore_app_state(device_id, app_id, name).await;
                if result.is_ok() && app_id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::AssertScreenshot {
                name,
                threshold,
                cluster_px,
                mask,
                tolerance,
                comparator,
                crop,
            } => {
                let shot = ScreenshotStep {
                    threshold: *threshold,
                    cluster_px: *cluster_px,
                    mask,
                    rule: crate::visual::PixelRule {
                        comparator: comparator.unwrap_or_default(),
                        tolerance: tolerance
                            .unwrap_or(crate::visual::PixelRule::default().tolerance),
                    },
                    crop: crop.as_ref(),
                };
                self.assert_screenshot(device_id, name, shot).await
            }
            Action::PressKey { key } => self.driver.press_key(device_id, key.clone()).await,
            Action::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                Ok(())
            }
            Action::RunFlow { .. } => {
                // Flows should be resolved before execution
                Err(VelocityError::Config(
                    "runFlow encountered during execution; flows must be resolved first"
                        .to_string(),
                ))
            }
            Action::TapXY { x, y } => self.driver.tap_xy(device_id, *x, *y).await,
            Action::InputRaw { text } => self.driver.input_raw(device_id, text).await,
            Action::Goto { url } => self.driver.navigate(device_id, url).await,
            Action::GoBack => self.driver.go_back(device_id).await,
            Action::Refresh => self.driver.refresh_page(device_id).await,
            // Natural steps are resolved to a concrete action before execute_action
            // is reached; seeing one here means resolution was skipped.
            Action::Natural { .. } => Err(VelocityError::Internal(anyhow::anyhow!(
                "internal: natural-language step reached executor unresolved"
            ))),
        }
    }

    /// An `http` step: sent through the test's session (its cookie jar),
    /// and what it captured kept for the test's later steps.
    async fn run_http(&mut self, req: &velocity_common::HttpRequest) -> Result<()> {
        if self.http.is_none() {
            let config = self.config.http.clone().unwrap_or_default();
            self.http = Some(velocity_api::HttpSession::new(&config)?);
        }
        let session = self.http.as_ref().expect("made above");
        let timeout = session.request_timeout(self.step_timeout_ms);
        let passed = session.run(req, timeout).await?;
        tracing::info!("{}", passed.summary);
        self.captured.extend(passed.captured);
        Ok(())
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

    /// `stepScreenshots`: the screen after step `index`, held in memory.
    async fn hold_step_screenshot(&mut self, device_id: &str, index: usize, action: &str) {
        let Ok(data) = self.driver.screenshot(device_id).await else {
            return;
        };
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let path = PathBuf::from(&self.config.artifacts.output_dir)
            .join("steps")
            .join(format!("{}{attempt}", clean(&self.current_test)))
            .join(format!("step{index:02}_{}.png", clean(action)));
        self.held_shots.push((path, data));
    }

    /// The crash reports `save_crash_logs` copied for the running test.
    fn crash_log_files(&self) -> Vec<PathBuf> {
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let dir = PathBuf::from(&self.config.artifacts.output_dir)
            .join("crashlogs")
            .join(format!("{}{attempt}", clean(&self.current_test)));
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// `--pause-on-failure`: write what there is so far (screenshot,
    /// video and logs, `trace.html`), say where, and wait for Enter, `r`
    /// or `q`. The device is left exactly as the failure left it.
    async fn pause_at_failure(
        &mut self,
        device_id: &str,
        test: &str,
        failed: &StepResult,
    ) -> PauseChoice {
        let _one_at_a_time = PAUSE_PROMPT.lock().await;
        let i = failed.step_index;
        let shot = match &failed.screenshot {
            Some(p) => Some(p.clone()),
            None => self.take_failure_screenshot(device_id, test, i).await,
        };
        let root = PathBuf::from(&self.config.artifacts.output_dir);
        let dir = crate::trace::trace_dir(&root, test);
        let suffix = self.trace.file_suffix(self.retry_attempt);
        let mut files = Vec::new();
        if let Some(session) = self.evidence.as_mut() {
            let out = velocity_common::EvidenceOut {
                dir: &dir,
                suffix: &suffix,
                keep_video: self.config.artifacts.video.on(),
                keep_logs: self.config.artifacts.logs.on(),
            };
            if let Ok(ev) =
                tokio::time::timeout(EVIDENCE_FINISH_BUDGET, session.checkpoint(out)).await
            {
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
                files = ev.attachments.iter().map(|a| a.path.clone()).collect();
                self.trace.set_evidence(evidence, ev.log_lines);
            }
        }
        let html = self.trace.snapshot_html(
            &root,
            test,
            device_id,
            self.config.platform,
            self.retry_attempt,
        );
        let mut msg = format!(
            "\n=> Paused at step {} of '{test}' on {device_id}: {}\n",
            i + 1,
            velocity_common::redact::redact(failed.error_message.as_deref().unwrap_or("failed"))
        );
        if let Some(p) = &shot {
            msg.push_str(&format!("   screenshot: {}\n", p.display()));
        }
        if let Some(p) = &html {
            msg.push_str(&format!("   trace:      {}\n", p.display()));
        }
        for f in &files {
            msg.push_str(&format!("   evidence:   {}\n", f.display()));
        }
        msg.push_str(
            "   The device is left as it is: inspect it live with `velocity inspect` in another \
             terminal.\n   [Enter] continue  [r] retry the step  [q] abort the run: ",
        );
        eprint!("{msg}");
        let answer = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await;
        match answer {
            Ok(Ok(line)) => parse_pause_answer(&line),
            _ => PauseChoice::Continue,
        }
    }

    /// D-50: the crash reports of a crash into the test's artifacts
    /// (`crashlogs/<test>/`), copied off a physical iPhone with devicectl.
    async fn save_crash_logs(&self, device_id: &str, app_id: &str) {
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let dir = PathBuf::from(&self.config.artifacts.output_dir)
            .join("crashlogs")
            .join(format!("{}{attempt}", clean(&self.current_test)));
        let copy = self.driver.copy_crash_logs(device_id, app_id, &dir);
        match tokio::time::timeout(Duration::from_secs(150), copy).await {
            Ok(files) if !files.is_empty() => {
                tracing::info!("crash report(s) saved to {}", dir.display());
            }
            Ok(_) => {}
            Err(_) => tracing::warn!("copying the crash reports timed out"),
        }
    }

    async fn take_failure_screenshot(
        &self,
        device_id: &str,
        context: &str,
        step_index: usize,
    ) -> Option<PathBuf> {
        // Its own small budget: after a watchdog timeout the device is
        // likely the thing that hung, and this ran outside the watchdog
        // (one hung step kept a run going 12.5 min; VEL-351).
        let data = tokio::time::timeout(FAILURE_ARTIFACT_BUDGET, self.driver.screenshot(device_id))
            .await
            .ok()?
            .ok()?;
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        // Named by test (and retry) as well as step: `failure_tap_step2.png`
        // from one test overwrote another test's, so a suite with several
        // failures kept the evidence of only the last one.
        let test = clean(&self.current_test);
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let fname = if test.is_empty() || context == self.current_test {
            format!("failure_{}_step{step_index}{attempt}.png", clean(context))
        } else {
            format!(
                "failure_{test}__{}_step{step_index}{attempt}.png",
                clean(context)
            )
        };
        let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
        write_artifact(&path, data).await.then_some(path)
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
        Action::Tap { .. } => "tap".to_string(),
        Action::DoubleTap { .. } => "doubleTap".to_string(),
        Action::LongPress { .. } => "longPress".to_string(),
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

/// A finger stroke across `b` in `finger`'s direction, from 80% to 20% of
/// the element along that axis, through its middle.
fn line_across(b: &velocity_common::Rect, finger: Direction) -> ((i32, i32), (i32, i32)) {
    // In i64: hostile bounds overflowed i32 (VEL-231, review m6).
    let (x, y, w, h) = (
        i64::from(b.x),
        i64::from(b.y),
        i64::from(b.width),
        i64::from(b.height),
    );
    let (cx, cy) = (to_i32(x + w / 2), to_i32(y + h / 2));
    let (near_x, far_x) = (to_i32(x + w / 5), to_i32(x + w * 4 / 5));
    let (near_y, far_y) = (to_i32(y + h / 5), to_i32(y + h * 4 / 5));
    match finger {
        Direction::Left => ((far_x, cy), (near_x, cy)),
        Direction::Right => ((near_x, cy), (far_x, cy)),
        Direction::Up => ((cx, far_y), (cx, near_y)),
        Direction::Down => ((cx, near_y), (cx, far_y)),
    }
}

/// The stroke of a `swipe` with only a direction, on a `w`×`h` screen:
/// centred, half the width across, 40% of the height up or down (30%–70%:
/// from 25%, a landscape phone's swipe started on the toolbar above the
/// list and nothing scrolled). Android's and headless's line.
fn default_swipe_line(w: i32, h: i32, finger: Direction) -> ((i32, i32), (i32, i32)) {
    let (cx, cy) = (w / 2, h / 2);
    let (dx, dy) = (w / 4, h / 5);
    match finger {
        Direction::Up => ((cx, cy + dy), (cx, cy - dy)),
        Direction::Down => ((cx, cy - dy), (cx, cy + dy)),
        Direction::Left => ((cx + dx, cy), (cx - dx, cy)),
        Direction::Right => ((cx - dx, cy), (cx + dx, cy)),
    }
}

/// `v` as a screen coordinate, clamped into i32.
fn to_i32(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The part of `b` on `screen`, if any.
fn intersection(
    screen: &velocity_common::Rect,
    b: &velocity_common::Rect,
) -> Option<velocity_common::Rect> {
    let x0 = b.x.max(screen.x);
    let y0 = b.y.max(screen.y);
    let x1 =
        b.x.saturating_add(b.width)
            .min(screen.x.saturating_add(screen.width));
    let y1 =
        b.y.saturating_add(b.height)
            .min(screen.y.saturating_add(screen.height));
    (x1 > x0 && y1 > y0).then(|| velocity_common::Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// How find_actionable's "acted anyway" warning ends.
const ACTED_ANYWAY: &str = "; acted on it anyway";

/// Drags allowed to bring a `within:` container onto the screen.
const CONTAINER_DRAGS: usize = 5;

/// The part of `b` on `screen`, when enough of it shows to stroke across:
/// on each axis, at least a quarter of the screen or the whole of `b`.
fn shown_part(
    screen: &velocity_common::Rect,
    b: &velocity_common::Rect,
) -> Option<velocity_common::Rect> {
    let x0 = b.x.max(screen.x);
    let y0 = b.y.max(screen.y);
    let x1 =
        b.x.saturating_add(b.width)
            .min(screen.x.saturating_add(screen.width));
    let y1 =
        b.y.saturating_add(b.height)
            .min(screen.y.saturating_add(screen.height));
    let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    let enough = |shown: i32, whole: i32, len: i32| shown >= whole.min(len / 4).max(1);
    (enough(w, b.width, screen.width) && enough(h, b.height, screen.height)).then_some(
        velocity_common::Rect {
            x: x0,
            y: y0,
            width: w,
            height: h,
        },
    )
}

/// A drag that brings `target`, off `area` along the scroll axis, to the
/// middle of `area`: the finger moves the gap (at most 60% of the area, so
/// the stroke stays inside it; a longer gap takes several drags). `None`
/// when the target is empty or already inside the area on that axis.
fn aim_at(
    area: &velocity_common::Rect,
    target: &velocity_common::Rect,
    scroll: Direction,
    on_screen: bool,
) -> Option<((i32, i32), (i32, i32))> {
    if target.width <= 0 || target.height <= 0 {
        return None;
    }
    let vertical = matches!(scroll, Direction::Up | Direction::Down);
    let w = i64::from;
    let (lo, len, t) = if vertical {
        (
            w(area.y),
            w(area.height),
            w(target.y) + w(target.height) / 2,
        )
    } else {
        (w(area.x), w(area.width), w(target.x) + w(target.width) / 2)
    };
    // On screen but in the outer band: under a navigation bar or the home
    // indicator. A tap there hit the status bar, which scrolls an iOS list
    // back to the top ("Item 137" tapped at y=65). Bring it in first.
    let band = if on_screen { len * 12 / 100 } else { 0 };
    if len <= 0 || (t >= lo + band && t <= lo + len - band) {
        return None;
    }
    let mid = lo + len / 2;
    let gap = (t - mid).clamp(-len * 3 / 5, len * 3 / 5);
    let (a, b) = (to_i32(mid + gap / 2), to_i32(mid - gap / 2));
    let cross = if vertical {
        to_i32(w(area.x) + w(area.width) / 2)
    } else {
        to_i32(w(area.y) + w(area.height) / 2)
    };
    Some(if vertical {
        ((cross, a), (cross, b))
    } else {
        ((a, cross), (b, cross))
    })
}

/// The finger's direction for a scroll that moves the content `scroll`:
/// the opposite one (to see what is below, drag the content up).
fn finger_for_scroll(scroll: Direction) -> Direction {
    match scroll {
        Direction::Down => Direction::Up,
        Direction::Up => Direction::Down,
        Direction::Left => Direction::Right,
        Direction::Right => Direction::Left,
    }
}

fn chrono_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Where an `assertScreenshot` baseline is written, and where it was found
/// (`legacy`: somewhere older versions kept it).
struct BaselinePlace {
    write: PathBuf,
    read: Option<PathBuf>,
    legacy: bool,
    /// Nothing to read at this capture size, but the name has a baseline
    /// in another `<W>x<H>` folder (a rotation or a device swap, D-V3):
    /// that size, which is a size mismatch, not a missing baseline.
    other_size: Option<(u32, u32)>,
    /// `read` is in this size's folder but its pixel size isn't what is
    /// compared: that pixel size.
    wrong_size: Option<(u32, u32)>,
}

/// One `assertScreenshot`'s settings, as the step gives them.
struct ScreenshotStep<'a> {
    threshold: Option<f64>,
    cluster_px: Option<usize>,
    mask: &'a [Selector],
    rule: crate::visual::PixelRule,
    crop: Option<&'a velocity_common::ScreenshotCrop>,
}

/// `png` cut to `crop` (the whole frame without one).
fn cut(png: &[u8], crop: Option<&crate::visual::MaskRegion>) -> Result<Vec<u8>> {
    let Some(c) = crop else {
        return Ok(png.to_vec());
    };
    crate::visual::crop_png(png, c).ok_or_else(|| VelocityError::AssertionFailed {
        expected: format!(
            "the crop {}x{} at ({}, {}) on screen",
            c.width, c.height, c.x, c.y
        ),
        actual: "the capture no longer contains it (the screen size changed)".into(),
        selector: "crop".into(),
        screenshot: None,
    })
}

/// The failure for a baseline that was missing and has just been written.
fn baseline_created(name: &str, path: &str) -> VelocityError {
    VelocityError::AssertionFailed {
        expected: format!("baseline {path} for {name}"),
        actual: "missing; created it from this screen. Review and commit it, then re-run \
                 (or run with --update-baselines=missing to accept new baselines)"
            .into(),
        selector: name.to_string(),
        screenshot: Some(PathBuf::from(path)),
    }
}

/// How long `assertScreenshot` waits for the screen to hold still when the
/// step sets no `timeoutMs`.
fn screenshot_stable_wait() -> Duration {
    velocity_common::settings::ms("VELOCITY_SCREENSHOT_STABLE_MS", 5000)
}

/// The file name older versions gave `name` (lossy: `a b` and `a/b` were
/// both `a_b.png`), still read so existing baselines keep matching.
fn legacy_baseline_file(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        + ".png"
}

/// Only a device screen has a status bar (clock, battery) to mask. A web
/// viewport or a headless render starts with the app's own header; masking
/// its top 4% let a wrong price or a broken nav bar pass assertScreenshot.
fn has_status_bar(config: &SuiteConfig) -> bool {
    config.driver.unwrap_or_default() == velocity_common::DriverMode::Device
        && config.platform != Some(velocity_common::Platform::Web)
}

/// Whether this run is on a CI server: the shared detector, reading the
/// daemon client's environment when there is one (review MINOR 9; it read
/// the daemon's own process environment).
fn in_ci_env() -> bool {
    velocity_common::env::in_ci()
}

/// Whether a CI variable's value says "CI": set, and not empty, `0` or
/// `false` (m7: create-react-app users set `CI=false` locally, which
/// turned on report-mode healing and refused missing baselines).
#[cfg(test)]
fn ci_flag(value: &std::ffi::OsStr) -> bool {
    velocity_common::env::ci_flag(&value.to_string_lossy())
}

/// The part of `b` (tree coordinates) on `screen`, scaled to screenshot
/// pixels by `(fx, fy)`. A partly off-screen element used to keep its full
/// size from a clamped origin and so masked what lay past it (VEL-063).
fn mask_region(
    b: &velocity_common::Rect,
    screen: &velocity_common::Rect,
    (fx, fy): (f64, f64),
) -> Option<crate::visual::MaskRegion> {
    let (x0, y0) = (b.x.max(screen.x), b.y.max(screen.y));
    let x1 =
        (i64::from(b.x) + i64::from(b.width)).min(i64::from(screen.x) + i64::from(screen.width));
    let y1 =
        (i64::from(b.y) + i64::from(b.height)).min(i64::from(screen.y) + i64::from(screen.height));
    let (w, h) = (x1 - i64::from(x0), y1 - i64::from(y0));
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(crate::visual::MaskRegion {
        x: (f64::from(x0) * fx) as u32,
        y: (f64::from(y0) * fy) as u32,
        width: (w as f64 * fx).ceil() as u32,
        height: (h as f64 * fy).ceil() as u32,
    })
}

/// Tallest iOS status bar, in points (Dynamic Island iPhones).
const IOS_STATUS_BAR_PT: f64 = 62.0;

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

/// `tap text="Login"` — an action with its selector, for messages.
/// A natural step's resolution that worked, with what was observed.
struct Resolved {
    action: Action,
    twins: Option<usize>,
    effect: Option<bool>,
}

fn action_selector(action: &Action) -> Option<&Selector> {
    match action {
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_model_api_outage_is_not_a_device_failure() {
        // Review M5: an Anthropic 429/529 storm marked every NL failure as
        // infrastructure, and the farm quarantined healthy devices.
        let down = VelocityError::ServiceUnavailable {
            service: "Anthropic API".into(),
            detail: "HTTP 529".into(),
        };
        assert!(down.is_transient());
        assert!(!is_infrastructure(&down));
        let act = Action::Tap {
            selector: Selector::Text("Pay".into()),
        };
        assert!(!is_infrastructure(&annotate(&act, down, None)));
    }

    #[test]
    fn a_natural_step_keeps_infrastructure_and_crash_errors() {
        // VEL-073: they became Config errors, losing infrastructure_failure
        // and the crash flag for natural steps.
        let act = Action::Tap {
            selector: Selector::Text("Pay".into()),
        };
        let lost = VelocityError::AdbConnectionLost {
            device_id: "d".into(),
            reason: "gone".into(),
        };
        let e = annotate(&act, lost, None);
        assert!(is_infrastructure(&e), "{e:?}");
        let crashed = VelocityError::AppCrashed {
            app_id: "app".into(),
            detail: "boom".into(),
        };
        match annotate(&act, crashed, None) {
            VelocityError::AppCrashed { detail, .. } => {
                assert!(
                    detail.contains("boom") && detail.contains("Pay"),
                    "{detail}"
                )
            }
            other => panic!("{other:?}"),
        }
        // Anything else still says what the step resolved to.
        let e = annotate(&act, VelocityError::Config("x".into()), None);
        assert!(e.to_string().contains("resolved to"), "{e}");
    }

    #[test]
    fn strokes_stay_inside_the_element() {
        let b = Rect {
            x: 40,
            y: 300,
            width: 1000,
            height: 600,
        };
        assert_eq!(line_across(&b, Direction::Left), ((840, 600), (240, 600)));
        assert_eq!(line_across(&b, Direction::Up), ((540, 780), (540, 420)));
        for d in [
            Direction::Up,
            Direction::Down,
            Direction::Left,
            Direction::Right,
        ] {
            let ((x1, y1), (x2, y2)) = line_across(&b, d);
            for (x, y) in [(x1, y1), (x2, y2)] {
                assert!(
                    x > b.x && x < b.x + b.width && y > b.y && y < b.y + b.height,
                    "{d:?}"
                );
            }
        }
    }

    #[test]
    fn set_config_accepts_settings_and_sync_timings_only() {
        assert!(check_set_config("android.scroll", "semantic").is_ok());
        assert!(check_set_config("sync.quietMs", "50").is_ok());
        assert!(check_set_config("sync.quietMs", "fast")
            .unwrap_err()
            .contains("milliseconds"));
        assert!(check_set_config("sync.mode", "none").is_err());
        assert!(check_set_config("andriod.scroll", "x").is_err());
        let mut cfg = velocity_common::SyncConfig::default();
        set_sync_field(&mut cfg, "sync.settleCapMs", "250").unwrap();
        assert_eq!(cfg.settle_cap_ms, Some(250));
    }

    #[test]
    fn system_state_expectations_report_each_mismatch() {
        let playing = velocity_common::SystemState {
            media_playing: true,
            media_titles: vec!["Velocity Lab Song".into()],
            in_call: false,
        };
        assert!(
            system_state_mismatch(&playing, Some(true), Some("Lab Song"), Some(false)).is_empty()
        );
        let m = system_state_mismatch(&playing, Some(false), Some("Other"), Some(true));
        assert_eq!(m.len(), 3, "{m:?}");
    }

    #[test]
    fn an_off_screen_target_gets_one_aimed_drag() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 1000,
        };
        let row = |y| Rect {
            x: 0,
            y,
            width: 400,
            height: 50,
        };
        // Below the screen: the finger moves up by the gap, capped at 60%.
        assert_eq!(
            aim_at(&screen, &row(1175), Direction::Down, false),
            Some(((200, 800), (200, 200)))
        );
        assert_eq!(
            aim_at(&screen, &row(1020), Direction::Down, false),
            Some(((200, 772), (200, 228)))
        );
        // Above (overshot): the finger moves down.
        assert_eq!(
            aim_at(&screen, &row(-200), Direction::Down, false),
            Some(((200, 200), (200, 800)))
        );
        // On screen, or no frame: the normal scroll.
        assert_eq!(aim_at(&screen, &row(400), Direction::Down, true), None);
        // Visible but under the top bar: one small drag down to the middle.
        assert_eq!(
            aim_at(&screen, &row(40), Direction::Down, true),
            Some(((200, 283), (200, 717)))
        );
        assert_eq!(
            aim_at(
                &screen,
                &Rect {
                    x: 0,
                    y: 2000,
                    width: 0,
                    height: 0
                },
                Direction::Down,
                false
            ),
            None
        );
    }

    #[test]
    fn scrolling_down_swipes_the_finger_up() {
        assert_eq!(finger_for_scroll(Direction::Down), Direction::Up);
        assert_eq!(finger_for_scroll(Direction::Up), Direction::Down);
        assert_eq!(finger_for_scroll(Direction::Right), Direction::Left);
        assert_eq!(finger_for_scroll(Direction::Left), Direction::Right);
    }
    use std::sync::Mutex;
    use velocity_common::selector_match::find_in_tree;
    use velocity_common::{DeviceInfo, Key, Rect, SyncMode};
    use velocity_nl::{NaturalPlanner, Op, PlanRequest, PlannedStep};

    fn node(id: &str, text: &str, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: Some(text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    /// A screen that changes on a schedule: after `n` element lookups,
    /// `change` runs once. Counts lookups, dumps and taps.
    type ScreenChange = Box<dyn FnOnce(&mut Element) + Send>;

    struct ScriptDriver {
        /// Capture video and logs: the notes its logs report.
        evidence: Option<Vec<String>>,
        /// (keep_video, keep_logs) of every finished capture.
        evidence_finished: Arc<Mutex<Vec<(bool, bool)>>>,
        screen: Mutex<Element>,
        script: Mutex<Vec<(usize, ScreenChange)>>,
        finds: AtomicUsize,
        dumps: AtomicUsize,
        taps: Mutex<Vec<String>>,
        /// Where each tap landed (the tapped element's top edge).
        tap_ys: Mutex<Vec<i32>>,
        /// The actionability rule each tap ran under (D-3).
        tap_modes: Mutex<Vec<velocity_common::actionability::ActMode>>,
        /// `navigate` (openLink/goto) succeeds.
        navigates: std::sync::atomic::AtomicBool,
        /// What `current_url` says.
        page_url: Mutex<Option<String>>,
        cached: bool,
        /// Lookups left that fail as an unreadable screen (CommandFailed).
        unreadable: AtomicUsize,
        /// Every lookup fails as a lost device.
        lost: std::sync::atomic::AtomicBool,
        /// Every lookup and tree read fails as the headless driver's "add a
        /// launchApp step" (a config error, VEL-255).
        unlaunched: std::sync::atomic::AtomicBool,
        /// What `app_liveness` says (None: the trait's default), and how
        /// often it was asked.
        liveness: Mutex<Option<velocity_common::Liveness>>,
        liveness_reads: AtomicUsize,
        /// What `system_bars` says.
        bars: Option<Vec<Rect>>,
        /// Lookups left before every lookup fails as a lost device.
        lose_after: AtomicUsize,
        /// `element_text_matches` ignores case (a driver that normalises
        /// the text it compares, as web does).
        fold_text: bool,
        /// Every drag (`swipe_coords` / `scroll_coords`).
        strokes: Mutex<Vec<Stroke>>,
        /// A drag moves the whole screen's content with the finger.
        pans: bool,
        /// `tap` on a disabled element fails as NotActionable (web).
        refuse_disabled: bool,
        /// `reveal` scrolls an off-screen element to y = 200, moving the
        /// whole screen (web's `scrollIntoViewIfNeeded`), and counts calls.
        reveals: bool,
        revealed: AtomicUsize,
        /// What `screenshot` returns, in turn (the last one repeats, or all
        /// of them cycle with `cycle_frames`), and how often it was asked.
        frames: Mutex<Vec<Vec<u8>>>,
        cycle_frames: bool,
        shots: AtomicUsize,
        /// How often `freeze_status_bar` was asked (it always takes).
        freezes: AtomicUsize,
        /// Added to the screen right after the deadline's widest-tier look
        /// misses (a target that renders just as the wait runs out).
        late: Mutex<Option<Element>>,
        /// `find_elements` calls (the ambiguity check's extra lookups).
        find_alls: AtomicUsize,
        /// Count on-screen matches in `find_element`, as the real drivers do.
        counts: bool,
        match_counts: velocity_common::selector_match::MatchCounts,
        /// Model Android's full-tree lookup cache: `find_element` and
        /// `get_hierarchy_cached` resolve against the cached tree (filled
        /// from the screen when empty, before the script advances); the
        /// settle's compare reads see the live screen and never fill it;
        /// `invalidate_hierarchy_cache` drops it.
        lookup_cache: Option<Mutex<Option<Element>>>,
        /// I3: the crash `pending_crash` reports once a tap landed and at
        /// least `pending_after` lookups ran; a launch clears it.
        pending: Mutex<Option<String>>,
        pending_after: AtomicUsize,
    }

    type Stroke = ((i32, i32), (i32, i32));

    fn shift(el: &mut Element, dx: i32, dy: i32) {
        for c in &mut el.children {
            c.bounds.x += dx;
            c.bounds.y += dy;
            shift(c, dx, dy);
        }
    }

    impl ScriptDriver {
        fn new(children: Vec<Element>) -> Self {
            Self {
                screen: Mutex::new(Element {
                    children,
                    text: None,
                    element_type: "hierarchy".into(),
                    ..node("hierarchy", "", 0)
                }),
                script: Mutex::new(Vec::new()),
                finds: AtomicUsize::new(0),
                dumps: AtomicUsize::new(0),
                taps: Mutex::new(Vec::new()),
                tap_ys: Mutex::new(Vec::new()),
                tap_modes: Mutex::new(Vec::new()),
                navigates: Default::default(),
                page_url: Mutex::new(None),
                cached: false,
                unreadable: AtomicUsize::new(0),
                lost: Default::default(),
                unlaunched: Default::default(),
                liveness: Mutex::new(None),
                liveness_reads: AtomicUsize::new(0),
                bars: None,
                lose_after: AtomicUsize::new(usize::MAX),
                fold_text: false,
                strokes: Mutex::new(Vec::new()),
                pans: false,
                refuse_disabled: false,
                reveals: false,
                revealed: AtomicUsize::new(0),
                frames: Mutex::new(Vec::new()),
                cycle_frames: false,
                shots: AtomicUsize::new(0),
                freezes: AtomicUsize::new(0),
                late: Mutex::new(None),
                find_alls: AtomicUsize::new(0),
                counts: true,
                match_counts: Default::default(),
                lookup_cache: None,
                pending: Mutex::new(None),
                pending_after: AtomicUsize::new(0),
                evidence: None,
                evidence_finished: Default::default(),
            }
        }

        fn after(self, n: usize, change: impl FnOnce(&mut Element) + Send + 'static) -> Self {
            self.script.lock().unwrap().push((n, Box::new(change)));
            self
        }

        fn advance(&self) {
            let n = self.finds.fetch_add(1, Ordering::SeqCst) + 1;
            let mut script = self.script.lock().unwrap();
            let mut screen = self.screen.lock().unwrap();
            let mut i = 0;
            while i < script.len() {
                if script[i].0 <= n {
                    (script.remove(i).1)(&mut screen);
                } else {
                    i += 1;
                }
            }
        }
    }
    /// A capture that writes a fake video and log when kept.
    struct FakeEvidence {
        notes: Vec<String>,
        finished: Arc<Mutex<Vec<(bool, bool)>>>,
    }

    #[async_trait::async_trait]
    impl velocity_common::EvidenceSession for FakeEvidence {
        async fn finish(
            self: Box<Self>,
            out: velocity_common::EvidenceOut<'_>,
        ) -> velocity_common::Evidence {
            use velocity_common::{Attachment, AttachmentKind};
            self.finished
                .lock()
                .unwrap()
                .push((out.keep_video, out.keep_logs));
            let mut ev = velocity_common::Evidence {
                notes: self.notes.clone(),
                ..Default::default()
            };
            std::fs::create_dir_all(out.dir).unwrap();
            if out.keep_video {
                let p = out.file("video", "mp4");
                std::fs::write(&p, b"mp4").unwrap();
                ev.video_offset_ms = Some(-40);
                ev.attachments
                    .push(Attachment::new(AttachmentKind::Video, p));
            }
            if out.keep_logs {
                let p = out.file("app", "log");
                std::fs::write(&p, b"log").unwrap();
                ev.attachments
                    .push(Attachment::new(AttachmentKind::AppLog, p));
                ev.log_lines.push(velocity_common::evidence::LogLine {
                    t: 5,
                    level: 'E',
                    text: "boom".into(),
                });
            }
            ev
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for ScriptDriver {
        async fn start_evidence(
            &self,
            _: &str,
            _: &velocity_common::EvidenceRequest,
        ) -> Option<Box<dyn velocity_common::EvidenceSession>> {
            self.evidence.clone().map(|notes| {
                Box::new(FakeEvidence {
                    notes,
                    finished: self.evidence_finished.clone(),
                }) as Box<dyn velocity_common::EvidenceSession>
            })
        }
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![])
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
            *self.pending.lock().unwrap() = None;
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn pending_crash(&self, _: &str, _: &str) -> Option<String> {
            if self.taps.lock().unwrap().is_empty()
                || self.finds.load(Ordering::SeqCst) < self.pending_after.load(Ordering::SeqCst)
            {
                return None;
            }
            self.pending.lock().unwrap().clone()
        }
        async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
            if self.unlaunched.load(Ordering::SeqCst) {
                self.finds.fetch_add(1, Ordering::SeqCst);
                return Err(VelocityError::Config("add a launchApp step".into()));
            }
            let cached_tree = self.lookup_cache.as_ref().map(|c| {
                c.lock()
                    .unwrap()
                    .get_or_insert_with(|| self.screen.lock().unwrap().clone())
                    .clone()
            });
            self.advance();
            if self
                .lose_after
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_err()
            {
                self.lost.store(true, Ordering::SeqCst);
            }
            if self.lost.load(Ordering::SeqCst) {
                return Err(VelocityError::AdbConnectionLost {
                    device_id: "d".into(),
                    reason: "device offline".into(),
                });
            }
            if self
                .unreadable
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(VelocityError::CommandFailed {
                    device_id: "d".into(),
                    command: "uiautomator dump".into(),
                    detail: "null root node".into(),
                });
            }
            let found = {
                let live = self.screen.lock().unwrap();
                let screen = cached_tree.as_ref().unwrap_or(&live);
                let found = find_in_tree(screen, selector, None);
                if found.is_some() && self.counts {
                    // Shown matches, as the web's page call counts them.
                    let n = velocity_common::selector_match::find_all_ref(screen, selector, None)
                        .into_iter()
                        .filter(|e| e.visible && !e.bounds.is_empty())
                        .count();
                    self.match_counts.record(device_id, selector, n);
                } else if found.is_none() {
                    self.match_counts.clear(device_id);
                }
                found
            };
            if velocity_common::selector_match::ceiling()
                == velocity_common::selector_match::Tier::Loose
            {
                if let Some(el) = self.late.lock().unwrap().take() {
                    self.screen.lock().unwrap().children.push(el);
                }
            }
            found.ok_or_else(|| VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
        }
        fn last_match_count(&self, device_id: &str, selector: &Selector) -> Option<usize> {
            self.match_counts.get(device_id, selector)
        }
        async fn find_elements(&self, _: &str, selector: &Selector) -> Result<Vec<Element>> {
            self.find_alls.fetch_add(1, Ordering::SeqCst);
            let screen = self.screen.lock().unwrap();
            Ok(
                velocity_common::selector_match::find_all_ref(&screen, selector, None)
                    .into_iter()
                    .cloned()
                    .collect(),
            )
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            self.dumps.fetch_add(1, Ordering::SeqCst);
            if self.unlaunched.load(Ordering::SeqCst) {
                return Err(VelocityError::Config("add a launchApp step".into()));
            }
            Ok(self.screen.lock().unwrap().clone())
        }
        fn has_hierarchy_cache(&self) -> bool {
            self.cached || self.lookup_cache.is_some()
        }
        async fn get_hierarchy_cached(&self, d: &str) -> Result<Element> {
            match &self.lookup_cache {
                Some(c) => Ok(c
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| self.screen.lock().unwrap().clone())
                    .clone()),
                None => self.get_hierarchy(d).await,
            }
        }
        async fn invalidate_hierarchy_cache(&self) {
            if let Some(c) = &self.lookup_cache {
                *c.lock().unwrap() = None;
            }
        }
        async fn current_url(&self, _: &str) -> Option<String> {
            self.page_url.lock().unwrap().clone()
        }
        async fn navigate(&self, _: &str, _: &str) -> Result<()> {
            if self.navigates.load(Ordering::SeqCst) {
                return Ok(());
            }
            Err(VelocityError::Config(
                "openLink/goto isn't supported on this platform".into(),
            ))
        }
        async fn tap(&self, _: &str, el: &Element) -> Result<()> {
            if self.refuse_disabled && !el.enabled {
                return Err(VelocityError::NotActionable {
                    selector: el.platform_id.clone(),
                    reason: "disabled".into(),
                });
            }
            self.taps
                .lock()
                .unwrap()
                .push(el.text.clone().unwrap_or_default());
            self.tap_ys.lock().unwrap().push(el.bounds.y);
            self.tap_modes
                .lock()
                .unwrap()
                .push(velocity_common::actionability::mode());
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
        async fn swipe_coords(&self, _: &str, a: (i32, i32), b: (i32, i32)) -> Result<()> {
            self.strokes.lock().unwrap().push((a, b));
            if self.pans {
                shift(&mut self.screen.lock().unwrap(), b.0 - a.0, b.1 - a.1);
            }
            Ok(())
        }
        async fn press_key(&self, _: &str, _: Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            let n = self.shots.fetch_add(1, Ordering::SeqCst);
            let frames = self.frames.lock().unwrap();
            if frames.is_empty() {
                return Ok(vec![]);
            }
            let i = if self.cycle_frames {
                n % frames.len()
            } else {
                n.min(frames.len() - 1)
            };
            Ok(frames[i].clone())
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1080, 1920))
        }
        async fn app_liveness(&self, d: &str, a: &str) -> velocity_common::Liveness {
            self.liveness_reads.fetch_add(1, Ordering::SeqCst);
            let set = self.liveness.lock().unwrap().clone();
            match set {
                Some(l) => l,
                None => match self.crash_report(d, a).await {
                    Some(detail) => velocity_common::Liveness::Dead(detail),
                    None => velocity_common::Liveness::Unknown,
                },
            }
        }
        async fn system_bars(&self, _: &str) -> Option<Vec<Rect>> {
            self.bars.clone()
        }
        async fn freeze_status_bar(&self, _: &str) -> bool {
            self.freezes.fetch_add(1, Ordering::SeqCst);
            true
        }
        async fn get_element_text(&self, _: &str, el: &Element) -> Result<String> {
            Ok(el.text.clone().unwrap_or_default())
        }
        async fn element_text_matches(
            &self,
            d: &str,
            el: &Element,
            expected: &str,
        ) -> Result<(bool, String)> {
            let actual = self.get_element_text(d, el).await?;
            let hit = if self.fold_text {
                actual.to_lowercase() == expected.to_lowercase()
            } else {
                actual == expected
            };
            Ok((hit, actual))
        }
        async fn is_element_visible(&self, _: &str, el: &Element) -> Result<bool> {
            Ok(find_in_tree(
                &self.screen.lock().unwrap(),
                &Selector::Id(el.platform_id.clone()),
                None,
            )
            .is_some())
        }
        async fn reveal(&self, _: &str, el: &Element) -> Result<Element> {
            let mut el = el.clone();
            if !self.reveals {
                return Ok(el);
            }
            self.revealed.fetch_add(1, Ordering::SeqCst);
            if el.bounds.y < 0 || el.bounds.y + el.bounds.height > 1920 {
                let dy = 200 - el.bounds.y;
                shift(&mut self.screen.lock().unwrap(), 0, dy);
                el.bounds.y += dy;
            }
            Ok(el)
        }
    }

    fn config(timeout_ms: u64) -> SuiteConfig {
        let mut c = SuiteConfig {
            platform: Some(Platform::Android),
            ..Default::default()
        };
        c.sync.mode = SyncMode::None;
        c.sync.timeout_ms = timeout_ms;
        c.dialog.enabled = false;
        c.healing.enabled = false;
        c.artifacts.on_failure = false;
        c
    }

    fn step(action: Action) -> Step {
        Step {
            action,
            timeout_ms: None,
            settle: None,
            source: None,
            modifiers: Default::default(),
        }
    }

    fn test_case(steps: Vec<Step>) -> TestCase {
        TestCase {
            name: "t".into(),
            tags: vec![],
            isolated: false,
            depends_on: vec![],
            steps,
            description: None,
            verify: None,
        }
    }

    fn tap(text: &str) -> Action {
        Action::Tap {
            selector: Selector::Text(text.into()),
        }
    }

    /// VEL-218: the default refusals named internal methods ("navigate()
    /// not supported") and said "Android only" on Android.
    #[tokio::test]
    async fn default_refusals_name_the_step_not_the_method() {
        let d = ScriptDriver::new(vec![]);
        let msgs = [
            d.navigate("d", "x").await.unwrap_err().to_string(),
            d.go_back("d").await.unwrap_err().to_string(),
            d.refresh_page("d").await.unwrap_err().to_string(),
            d.system_state("d").await.unwrap_err().to_string(),
        ];
        for m in &msgs {
            assert!(!m.contains("()") && !m.contains("Android only"), "{m}");
        }
        assert!(msgs[0].contains("openLink"), "{}", msgs[0]);
        assert!(msgs[1].contains("goBack"), "{}", msgs[1]);
    }

    /// P4: the suite's `framework` reaches the sync probe's `config`.
    #[test]
    fn the_suite_framework_reaches_native_sync() {
        let driver = ScriptDriver::new(vec![]);
        let mut cfg = config(300);
        cfg.framework = Some(velocity_common::Framework::ReactNative);
        let ex = TestExecutor::new(&driver, cfg, "app");
        assert_eq!(
            ex.sync_manager.framework(),
            velocity_common::Framework::ReactNative
        );
        let ex = TestExecutor::new(&driver, config(300), "app");
        assert_eq!(
            ex.sync_manager.framework(),
            velocity_common::Framework::Native
        );
    }

    #[tokio::test]
    async fn waits_for_an_element_that_renders_late_in_every_sync_mode() {
        for mode in [SyncMode::None, SyncMode::Polling] {
            let driver =
                ScriptDriver::new(vec![]).after(3, |s| s.children.push(node("b", "Buy", 0)));
            let mut cfg = config(3000);
            cfg.sync.mode = mode;
            cfg.sync.interval_ms = 10;
            let mut ex = TestExecutor::new(&driver, cfg, "app");
            let r = ex
                .execute_step(&step(tap("Buy")), "d", "app", 0)
                .await
                .unwrap();
            assert_eq!(
                r.status,
                StepStatus::Passed,
                "{mode:?}: {:?}",
                r.error_message
            );
            assert_eq!(*driver.taps.lock().unwrap(), ["Buy"]);
        }
    }

    /// A screen no wait can read (headless with no `launchApp`, VEL-255)
    /// failed each step only at its element timeout: 10.2 s a test. Now at
    /// once, from the pre-action settle or the lookup; a not-found still
    /// waits (the test above).
    #[tokio::test]
    async fn a_config_error_fails_the_step_at_once() {
        for mode in [SyncMode::None, SyncMode::Polling] {
            let gone = Action::AssertNotVisible {
                selector: Selector::Text("Buy".into()),
            };
            for action in [tap("Buy"), gone] {
                let driver = ScriptDriver::new(vec![]);
                driver.unlaunched.store(true, Ordering::SeqCst);
                let mut cfg = config(5000);
                cfg.sync.mode = mode;
                let mut ex = TestExecutor::new(&driver, cfg, "app");
                let started = std::time::Instant::now();
                let r = ex.execute_step(&step(action), "d", "app", 0).await.unwrap();
                assert_eq!(r.status, StepStatus::Failed, "{mode:?}");
                let msg = r.error_message.unwrap_or_default();
                assert!(msg.contains("launchApp"), "{msg}");
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "{mode:?}: {:?}",
                    started.elapsed()
                );
                // Polling's settle reads the screen first and stops there.
                let finds = driver.finds.load(Ordering::SeqCst);
                assert_eq!(finds, usize::from(mode == SyncMode::None), "{mode:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_failed_step_names_its_line_and_its_trace_keeps_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut driver =
            ScriptDriver::new(vec![node("cart", "Cart", 0), node("pay", "Pay now", 200)]);
        driver.cached = true;
        let mut cfg = config(300);
        cfg.artifacts.on_failure = true;
        cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let at = |line| Some(velocity_common::SourcePos::new("tests/app.yaml", line, 9));
        let steps = vec![
            Step {
                source: at(41),
                ..step(tap("Cart"))
            },
            Step {
                source: at(42),
                ..step(tap("Pay"))
            },
        ];
        let r = ex
            .execute_test(&test_case(steps), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.clone().unwrap();
        assert!(
            msg.starts_with("tests/app.yaml:42: tap { text: \"Pay\" }: Element not found"),
            "{msg}"
        );
        assert_eq!(r.steps[1].source.as_ref().unwrap().line, 42);

        let path = r.trace.expect("a failed test keeps its trace");
        let (trace, trees) = crate::trace::load_trace(&path).unwrap();
        assert_eq!(trace.steps.len(), 2);
        let cart = trace.steps[0].matched.as_ref().expect("matched element");
        assert_eq!(cart.text.as_deref(), Some("Cart"));
        assert!(!trace.steps[0].trees.is_empty(), "lookup tree kept");
        assert!(!trace.steps[1].trees.is_empty(), "failure tree kept");
        assert_eq!(trace.steps[1].nearest[0].text.as_deref(), Some("Pay now"));
        assert_eq!(trees.len(), 1, "one screen, one tree file");
        let d = crate::trace::describe_failure(&trace, &trees, 3).unwrap();
        assert_eq!(d.source.unwrap().line, 42);
        assert_eq!(d.closest[0].text.as_deref(), Some("Pay now"));
    }

    #[tokio::test]
    async fn a_passing_test_keeps_no_trace_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let driver = ScriptDriver::new(vec![node("cart", "Cart", 0)]);
        let mut cfg = config(300);
        cfg.artifacts.on_failure = true;
        cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
        let mut ex = TestExecutor::new(&driver, cfg.clone(), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Cart"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert!(r.trace.is_none());
        assert!(!dir.path().join("traces").exists());

        let mut ex =
            TestExecutor::new(&driver, cfg, "app").with_trace(velocity_common::TraceMode::Always);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Cart"))]), "d", "app")
            .await
            .unwrap();
        assert!(r.trace.unwrap().exists());
    }

    #[tokio::test]
    async fn step_timeout_overrides_sync_timeout() {
        let driver = ScriptDriver::new(vec![]);
        let mut ex = TestExecutor::new(&driver, config(60_000), "app");
        let start = Instant::now();
        let r = ex
            .execute_step(
                &Step {
                    action: tap("Missing"),
                    timeout_ms: Some(300),
                    settle: None,
                    source: None,
                    modifiers: Default::default(),
                },
                "d",
                "app",
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(r.error_message.unwrap().contains("after 300ms"));
    }

    #[tokio::test]
    async fn assert_not_visible_waits_for_the_element_to_go() {
        let driver =
            ScriptDriver::new(vec![node("spin", "Loading", 0)]).after(3, |s| s.children.clear());
        let mut ex = TestExecutor::new(&driver, config(3000), "app");
        let r = ex
            .execute_step(
                &step(Action::AssertNotVisible {
                    selector: Selector::Text("Loading".into()),
                }),
                "d",
                "app",
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn assert_text_waits_for_the_text_to_settle() {
        let driver = ScriptDriver::new(vec![node("total", "$0.00", 0)])
            .after(3, |s| s.children[0].text = Some("$42.00".into()));
        let mut ex = TestExecutor::new(&driver, config(3000), "app");
        let r = ex
            .execute_step(
                &step(Action::AssertText {
                    selector: Selector::Id("total".into()),
                    expected: "$42.00".into(),
                }),
                "d",
                "app",
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    /// B-1/M-1: assertText leaves the comparison to the driver, so a
    /// driver that normalises its text (web) normalises `expected` alike.
    #[tokio::test]
    async fn assert_text_compares_through_the_driver() {
        let mut driver = ScriptDriver::new(vec![node("go", "SUBMIT", 0)]);
        driver.fold_text = true;
        let mut ex = TestExecutor::new(&driver, config(500), "app");
        let r = ex
            .execute_step(
                &step(Action::AssertText {
                    selector: Selector::Id("go".into()),
                    expected: "Submit".into(),
                }),
                "d",
                "app",
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    /// Always picks element 0 and counts calls.
    struct FirstElement(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl NaturalPlanner for FirstElement {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PlannedStep {
                op: Op::Tap,
                element: Some(0),
                text: String::new(),
                direction: "none".into(),
                ms: 0,
                key: Default::default(),
            })
        }
    }

    fn resolver(name: &str, calls: Arc<AtomicUsize>) -> Arc<NaturalResolver> {
        let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(NaturalResolver::new(
            Box::new(FirstElement(calls)),
            dir.join("nl-cache.json"),
        ))
    }

    fn natural(intent: &str) -> Step {
        step(Action::Natural {
            intent: intent.into(),
            kind: NaturalKind::Act,
            data: None,
            data_name: None,
            wait_until: None,
            intent_template: None,
        })
    }

    #[tokio::test]
    async fn cached_natural_step_needs_no_dump_and_no_model_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("hit", calls.clone());
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let test = test_case(vec![natural("tap buy")]);

        let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl.clone());
        assert_eq!(
            ex.execute_test(&test, "d", "app").await.unwrap().status,
            TestStatus::Passed
        );
        let dumps_after_plan = driver.dumps.load(Ordering::SeqCst);

        let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl);
        assert_eq!(
            ex.execute_test(&test, "d", "app").await.unwrap().status,
            TestStatus::Passed
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "rerun must not call the model"
        );
        assert_eq!(
            driver.dumps.load(Ordering::SeqCst),
            dumps_after_plan,
            "a cache hit must not capture the screen"
        );
    }

    // The cache contract: a plan is written only once the run confirmed it.
    // Here the step after it fails, so the plan is never cached and the next
    // run plans again; once a test passes, its plans are kept.
    #[tokio::test]
    async fn a_plan_is_cached_only_after_the_run_confirms_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("confirm", calls.clone());
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let unconfirmed = test_case(vec![natural("tap buy"), step(tap("Nowhere"))]);
        let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl.clone());
        let r = ex.execute_test(&unconfirmed, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            nl.cached(&first_step("tap buy", NaturalKind::Act))
                .await
                .is_none(),
            "the step after it failed: not cached"
        );

        let confirmed = test_case(vec![natural("tap buy"), step(tap("Buy"))]);
        let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl.clone());
        let r = ex.execute_test(&confirmed, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "planned again");
        assert!(nl
            .cached(&first_step("tap buy", NaturalKind::Act))
            .await
            .is_some());

        // A plan in a test's last step is kept when the test passes.
        let last = test_case(vec![natural("tap buy")]);
        let nl2 = resolver("confirm-last", Arc::new(AtomicUsize::new(0)));
        let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl2.clone());
        ex.execute_test(&last, "d", "app").await.unwrap();
        assert!(nl2
            .cached(&first_step("tap buy", NaturalKind::Act))
            .await
            .is_some());
    }

    // Two runs of one data-driven step with different values share the
    // cache entry and type each run's own value, with no second model call.
    #[tokio::test]
    async fn a_data_driven_step_replays_with_each_runs_value() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut typed = answer(Op::InputText, Some(0));
        typed.text = String::new();
        let nl = answering("datadriven", vec![typed], calls.clone());
        let field = |id: &str| Element {
            element_type: "EditText".into(),
            text: None,
            label: Some("Email".into()),
            ..node(id, "", 0)
        };
        let typed_into = |value: &str| {
            test_case(vec![step(Action::Natural {
                intent: "type the email".into(),
                kind: NaturalKind::Act,
                data: Some(value.into()),
                data_name: Some("${EMAIL}".into()),
                wait_until: None,
                intent_template: None,
            })])
        };
        let driver = ScriptDriver::new(vec![field("email")]);
        for value in ["ann@example.com", "bob@example.com"] {
            let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
            let r = ex
                .execute_test(&typed_into(value), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second value replays");
        let file = std::fs::read_to_string(nl.cache_path()).unwrap();
        assert!(
            !file.contains("ann@example.com") && !file.contains("bob@"),
            "{file}"
        );
    }

    /// A checker with a fixed verdict that counts its calls.
    struct Verdict(
        velocity_nl::CheckAnswer,
        Arc<AtomicUsize>,
        Mutex<Vec<String>>,
    );

    #[async_trait::async_trait]
    impl velocity_nl::ScreenChecker for Verdict {
        async fn check(
            &self,
            req: &velocity_nl::CheckRequest<'_>,
        ) -> Result<velocity_nl::CheckAnswer> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.2.lock().unwrap().push(req.claim.to_string());
            Ok(self.0.clone())
        }
    }

    fn checked(
        name: &str,
        holds: bool,
        ids: &[i64],
        checks: Arc<AtomicUsize>,
    ) -> (Arc<NaturalResolver>, Arc<AtomicUsize>) {
        let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let r = NaturalResolver::new(
            Box::new(FirstElement(calls.clone())),
            dir.join("nl-cache.json"),
        )
        .with_checker(Box::new(Verdict(
            velocity_nl::CheckAnswer {
                holds,
                evidence_element_ids: ids.to_vec(),
                reason: "fake".into(),
            },
            checks,
            Mutex::new(vec![]),
        )));
        (Arc::new(r), calls)
    }

    fn with_verify(level: velocity_common::VerifyLevel) -> SuiteConfig {
        SuiteConfig {
            verify: Some(level),
            ..config(300)
        }
    }

    fn checkpoint(action: Action, claim: &str) -> Step {
        let mut s = step(action);
        s.modifiers.checkpoint = Some(claim.into());
        s
    }

    // verify: off (the default) never asks the model, even with a
    // checker configured and checkpoints written.
    #[tokio::test]
    async fn verify_off_makes_no_model_call() {
        use velocity_common::VerifyLevel;
        let checks = Arc::new(AtomicUsize::new(0));
        let (nl, _) = checked("verify-off", true, &[0], checks.clone());
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let test = test_case(vec![checkpoint(tap("Buy"), "bought")]);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert!(r.verification.is_empty());
        assert_eq!(checks.load(Ordering::SeqCst), 0);
        // --verify off wins over the suite's level too.
        let mut ex = TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app")
            .with_planner(nl)
            .with_verify(Some(VerifyLevel::Off));
        ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(checks.load(Ordering::SeqCst), 0);
    }

    // verify: final checks the description on the last screen. A verdict
    // that cites no real element is unknown, and fails the test.
    #[tokio::test]
    async fn verify_final_needs_real_evidence() {
        use velocity_common::{VerifyLevel, VerifyOutcome};
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let mut test = test_case(vec![step(tap("Buy"))]);
        test.description = Some("the purchase went through".into());

        let checks = Arc::new(AtomicUsize::new(0));
        let (nl, _) = checked("verify-final-ok", true, &[0], checks.clone());
        let mut ex =
            TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app").with_planner(nl);
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(r.verification.len(), 1);
        assert_eq!(r.verification[0].at, "final");
        assert_eq!(r.verification[0].claim, "the purchase went through");
        assert_eq!(r.verification[0].outcome, VerifyOutcome::Holds);
        assert!(r.verification[0].evidence[0].contains("Buy"));

        // "Holds", citing element 42 of a one-element screen: not a pass.
        let (nl, _) = checked("verify-final-fake", true, &[42], checks.clone());
        let mut ex = TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app")
            .with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert_eq!(r.verification[0].outcome, VerifyOutcome::Unknown);
        let err = r.error_message.unwrap();
        assert!(
            err.contains("AI verification failed") && err.contains("unknown"),
            "{err}"
        );

        // The name stands in for a missing description; a test that fails
        // its checks keeps none of its plans.
        let mut plain = test_case(vec![natural("tap buy")]);
        plain.name = "buying works".into();
        let mut ex = TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app")
            .with_planner(nl.clone());
        let r = ex.execute_test(&plain, "d", "app").await.unwrap();
        assert_eq!(r.verification[0].claim, "buying works");
        assert_eq!(r.status, TestStatus::Failed);
        let key = NaturalStep {
            scope: "buying works#0",
            ..first_step("tap buy", NaturalKind::Act)
        };
        assert!(
            nl.cached(&key).await.is_none(),
            "unverified plans aren't cached"
        );
    }

    // checkpoints: a failed check is recorded and the test goes on, failing
    // at the end; strict stops at the first failed check.
    #[tokio::test]
    async fn checkpoints_report_at_the_end_and_strict_stops_at_once() {
        use velocity_common::{VerifyLevel, VerifyOutcome};
        let test = test_case(vec![
            checkpoint(tap("Buy"), "the cart is open"),
            step(tap("Pay")),
        ]);

        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
        let checks = Arc::new(AtomicUsize::new(0));
        let (nl, _) = checked("verify-cp", false, &[], checks.clone());
        let mut ex = TestExecutor::new(&driver, with_verify(VerifyLevel::Checkpoints), "app")
            .with_planner(nl);
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(*driver.taps.lock().unwrap(), ["Buy", "Pay"], "went on");
        assert_eq!(r.status, TestStatus::Failed);
        let at: Vec<(&str, VerifyOutcome)> = r
            .verification
            .iter()
            .map(|c| (c.at.as_str(), c.outcome))
            .collect();
        assert_eq!(
            at,
            [
                ("step 0", VerifyOutcome::Fails),
                ("final", VerifyOutcome::Fails)
            ]
        );
        assert!(r.error_message.unwrap().contains("the cart is open"));

        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
        let (nl, _) = checked("verify-strict", false, &[], checks.clone());
        let mut ex =
            TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app").with_planner(nl);
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(
            *driver.taps.lock().unwrap(),
            ["Buy"],
            "stopped at the check"
        );
        assert_eq!(r.status, TestStatus::Failed);
        assert_eq!(r.steps[1].status, StepStatus::Skipped);
        assert_eq!(r.verification.len(), 1);

        // A per-test level wins over the suite's.
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
        let (nl, _) = checked("verify-test", false, &[], checks.clone());
        let mut off = test.clone();
        off.verify = Some(VerifyLevel::Off);
        let mut ex =
            TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app").with_planner(nl);
        let r = ex.execute_test(&off, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert!(r.verification.is_empty());
    }

    #[tokio::test]
    async fn stale_cached_step_is_replanned_in_the_same_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("heal", calls.clone());
        let test = test_case(vec![natural("tap checkout")]);

        let v1 = ScriptDriver::new(vec![node("old_btn", "Checkout", 0)]);
        let mut ex = TestExecutor::new(&v1, config(300), "app").with_planner(nl.clone());
        ex.execute_test(&test, "d", "app").await.unwrap();

        // The app changed: the cached id no longer exists.
        let v2 = ScriptDriver::new(vec![node("new_btn", "Check out", 0)]);
        let mut ex = TestExecutor::new(&v2, config(300), "app").with_planner(nl);
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*v2.taps.lock().unwrap(), ["Check out"]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            r.warnings.iter().any(|w| w.contains("re-planned")),
            "{:?}",
            r.warnings
        );
    }

    /// A planner that gives these answers in turn (the last one repeats).
    struct Answers(Mutex<Vec<PlannedStep>>, Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl NaturalPlanner for Answers {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.1.fetch_add(1, Ordering::SeqCst);
            let mut a = self.0.lock().unwrap();
            Ok(if a.len() > 1 {
                a.remove(0)
            } else {
                a[0].clone()
            })
        }
    }

    fn answer(op: Op, element: Option<usize>) -> PlannedStep {
        PlannedStep {
            op,
            element,
            text: String::new(),
            direction: "none".into(),
            ms: 0,
            key: Default::default(),
        }
    }

    fn answering(
        name: &str,
        answers: Vec<PlannedStep>,
        calls: Arc<AtomicUsize>,
    ) -> Arc<NaturalResolver> {
        let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(NaturalResolver::new(
            Box::new(Answers(Mutex::new(answers), calls)),
            dir.join("nl-cache.json"),
        ))
    }

    /// The cache key the executor uses for the first natural step of test
    /// `t` on Android.
    fn first_step<'a>(intent: &'a str, kind: NaturalKind) -> NaturalStep<'a> {
        NaturalStep {
            intent,
            kind,
            data: None,
            data_name: None,
            wait_until: None,
            intent_template: None,
            platform: Platform::Android,
            scope: "t#0",
            fresh: false,
            describe_data: false,
        }
    }

    #[tokio::test]
    async fn a_cached_assertion_that_fails_is_not_replanned_onto_the_screen() {
        // D-17 / VEL-213: re-planning a failed check looked for whatever
        // was on screen and passed on another element.
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = answering(
            "assert-213",
            vec![answer(Op::AssertVisible, Some(0))],
            calls.clone(),
        );
        let check = |intent: &str| {
            step(Action::Natural {
                intent: intent.into(),
                kind: NaturalKind::Assert,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
            })
        };
        let test = test_case(vec![check("the order total is shown")]);
        let v1 = ScriptDriver::new(vec![node("total", "Total $12", 0)]);
        let mut ex = TestExecutor::new(&v1, config(300), "app").with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        // The total is gone; something else is on screen.
        let v2 = ScriptDriver::new(vec![node("promo", "Free shipping", 0)]);
        let mut ex = TestExecutor::new(&v2, config(300), "app").with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "not re-planned");
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("not re-planned"), "{err}");
        // Dropped from the cache: the next attempt plans it afresh.
        assert!(nl
            .cached(&first_step("the order total is shown", NaturalKind::Assert))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_cached_action_without_its_effect_fails_without_a_second_action() {
        // D-17 / VEL-349: the re-plan ran a second, different action in the
        // same step after the first one had already acted.
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = answering("effect-349", vec![answer(Op::Tap, Some(1))], calls.clone());
        // Planned earlier, when tapping Buy changed the screen.
        nl.commit(
            &first_step("tap buy", NaturalKind::Act),
            Action::Tap {
                selector: Selector::Id("buy".into()),
            },
            None,
            Some(true),
        )
        .await;
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("other", "Other", 50)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
        let r = ex
            .execute_test(&test_case(vec![natural("tap buy")]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert_eq!(*driver.taps.lock().unwrap(), ["Buy"], "one action only");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "not re-planned in the step"
        );
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("no visible effect"), "{err}");
        assert!(nl
            .cached(&first_step("tap buy", NaturalKind::Act))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_natural_goto_stays_on_the_current_origin() {
        // D-17: a cached (or planned) goto to another origin is refused
        // unless `nl.allowCrossOrigin: true`.
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = answering("goto-origin", vec![answer(Op::NotFound, None)], calls);
        nl.commit(
            &first_step("open the deals page", NaturalKind::Act),
            Action::Goto {
                url: "https://evil.example/deals".into(),
            },
            None,
            None,
        )
        .await;
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        driver.navigates.store(true, Ordering::SeqCst);
        *driver.page_url.lock().unwrap() = Some("https://shop.example/cart".into());
        let test = test_case(vec![natural("open the deals page")]);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("nl.allowCrossOrigin"), "{err}");
        // Allowed by the suite.
        nl.commit(
            &first_step("open the deals page", NaturalKind::Act),
            Action::Goto {
                url: "https://evil.example/deals".into(),
            },
            None,
            None,
        )
        .await;
        let mut cfg = config(300);
        cfg.nl.allow_cross_origin = true;
        let mut ex = TestExecutor::new(&driver, cfg, "app").with_planner(nl);
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    /// Records the data each request carried.
    struct DataSeen(Arc<Mutex<Vec<Option<String>>>>);

    #[async_trait::async_trait]
    impl NaturalPlanner for DataSeen {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.0.lock().unwrap().push(req.data.map(String::from));
            Ok(answer(Op::InputText, Some(0)))
        }
    }

    #[tokio::test]
    async fn the_suite_data_policy_reaches_the_planner() {
        // D-30: `nl.dataPolicy: describe`.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let dir = std::env::temp_dir().join(format!("vel-exec-{}-d30", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nl = Arc::new(NaturalResolver::new(
            Box::new(DataSeen(seen.clone())),
            dir.join("nl-cache.json"),
        ));
        let driver = ScriptDriver::new(vec![node("name", "Name", 0)]);
        let typed = step(Action::Natural {
            intent: "type the customer name".into(),
            kind: NaturalKind::Act,
            data: Some("Ada Lovelace".into()),
            data_name: None,
            wait_until: None,
            intent_template: None,
        });
        let mut cfg = config(300);
        cfg.nl.data_policy = velocity_common::NlDataPolicy::Describe;
        let mut ex = TestExecutor::new(&driver, cfg, "app").with_planner(nl);
        let r = ex
            .execute_test(&test_case(vec![typed]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty());
        for d in &seen {
            let d = d.as_deref().unwrap_or_default();
            assert!(d.starts_with("<value, 12 chars>"), "{d}");
        }
    }

    #[tokio::test]
    async fn repeated_step_text_gets_separate_cache_entries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("scope", calls.clone());
        let driver = ScriptDriver::new(vec![node("next", "Next", 0)]);
        let test = test_case(vec![natural("tap next"), natural("tap next")]);
        let mut ex = TestExecutor::new(&driver, config(1000), "app").with_planner(nl);
        ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each occurrence is planned on its own screen"
        );
    }

    /// Taps and screenshots never return (a wedged device).
    struct HangingTap(ScriptDriver);

    #[async_trait::async_trait]
    impl PlatformDriver for HangingTap {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            self.0.list_devices().await
        }
        async fn boot_device(&self, d: &str) -> Result<()> {
            self.0.boot_device(d).await
        }
        async fn shutdown_device(&self, d: &str) -> Result<()> {
            self.0.shutdown_device(d).await
        }
        async fn install_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.install_app(d, a).await
        }
        async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
            self.0.launch_app(d, a, c).await
        }
        async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.stop_app(d, a).await
        }
        async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
            self.0.find_element(d, s).await
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            self.0.find_elements(d, s).await
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            self.0.get_hierarchy(d).await
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
            std::future::pending().await
        }
        async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
            self.0.double_tap(d, e).await
        }
        async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
            self.0.long_press(d, e, ms).await
        }
        async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
            self.0.input_text(d, e, t).await
        }
        async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
            self.0.clear_text(d, e).await
        }
        async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
            self.0.swipe(d, dir).await
        }
        async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
            self.0.swipe_coords(d, f, t).await
        }
        async fn press_key(&self, d: &str, k: Key) -> Result<()> {
            self.0.press_key(d, k).await
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            std::future::pending().await
        }
        async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
            self.0.screen_size(d).await
        }
        async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
            self.0.get_element_text(d, e).await
        }
        async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
            self.0.is_element_visible(d, e).await
        }
    }

    /// The app dies on the tap: the device's settle never goes quiet (the
    /// launcher's live wallpaper) and the crash log names the exception.
    /// The second field is how long the device's settle takes to give up.
    struct CrashingTap(ScriptDriver, Duration);

    #[async_trait::async_trait]
    impl PlatformDriver for CrashingTap {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            self.0.list_devices().await
        }
        async fn boot_device(&self, d: &str) -> Result<()> {
            self.0.boot_device(d).await
        }
        async fn shutdown_device(&self, d: &str) -> Result<()> {
            self.0.shutdown_device(d).await
        }
        async fn install_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.install_app(d, a).await
        }
        async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
            self.0.launch_app(d, a, c).await
        }
        async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.stop_app(d, a).await
        }
        async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
            self.0.find_element(d, s).await
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            self.0.find_elements(d, s).await
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            self.0.get_hierarchy(d).await
        }
        async fn tap(&self, d: &str, e: &Element) -> Result<()> {
            self.0.tap(d, e).await
        }
        async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
            self.0.double_tap(d, e).await
        }
        async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
            self.0.long_press(d, e, ms).await
        }
        async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
            self.0.input_text(d, e, t).await
        }
        async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
            self.0.clear_text(d, e).await
        }
        async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
            self.0.swipe(d, dir).await
        }
        async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
            self.0.swipe_coords(d, f, t).await
        }
        async fn press_key(&self, d: &str, k: Key) -> Result<()> {
            self.0.press_key(d, k).await
        }
        async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
            self.0.screenshot(d).await
        }
        async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
            self.0.screen_size(d).await
        }
        async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
            self.0.get_element_text(d, e).await
        }
        async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
            self.0.is_element_visible(d, e).await
        }
        async fn settle_on_device(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: u64,
        ) -> Option<velocity_common::DeviceSettle> {
            tokio::time::sleep(self.1).await;
            Some(velocity_common::DeviceSettle {
                settled: false,
                reacted: true,
                elapsed_ms: self.1.as_millis() as u64,
            })
        }
        async fn crash_report(&self, _: &str, _: &str) -> Option<String> {
            (!self.0.taps.lock().unwrap().is_empty())
                .then(|| "java.lang.IllegalStateException: boom".to_string())
        }
        async fn copy_crash_logs(
            &self,
            _: &str,
            app: &str,
            dest: &std::path::Path,
        ) -> Vec<std::path::PathBuf> {
            std::fs::create_dir_all(dest).unwrap();
            let f = dest.join(format!("{app}.ips"));
            std::fs::write(&f, "{}").unwrap();
            vec![f]
        }
    }

    /// A failed test keeps its video and logs next to its trace, says what
    /// the logs saw, and links everything; a pass keeps nothing.
    #[tokio::test]
    async fn a_failure_keeps_video_logs_and_trace_html() {
        let mut driver = ScriptDriver::new(vec![node("b", "Pay", 0)]);
        driver.evidence = Some(vec![
            "crash (pid 7): java.lang.IllegalStateException: boom".into()
        ]);
        let out = tempfile::tempdir().unwrap();
        let mut c = config(300);
        c.artifacts.on_failure = true;
        c.artifacts.output_dir = out.path().to_string_lossy().into_owned();
        let mut ex =
            TestExecutor::new(&driver, c, "app").with_trace(velocity_common::TraceMode::OnFailure);
        let pass = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(pass.status, TestStatus::Passed);
        assert!(pass.attachments.is_empty(), "{:?}", pass.attachments);
        let fail = ex
            .execute_test(
                &test_case(vec![step(tap("Pay")), step(tap("Nowhere"))]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(fail.status, TestStatus::Failed);
        assert_eq!(
            *driver.evidence_finished.lock().unwrap(),
            vec![(false, false), (true, true)]
        );
        let kinds: Vec<_> = fail.attachments.iter().map(|a| a.kind).collect();
        use velocity_common::AttachmentKind as K;
        assert_eq!(
            kinds,
            vec![K::Video, K::AppLog, K::Trace],
            "{:?}",
            fail.attachments
        );
        for a in &fail.attachments {
            assert!(a.path.exists(), "{}", a.path.display());
        }
        let msg = fail.error_message.clone().unwrap();
        assert!(msg.contains("log: crash (pid 7)"), "{msg}");
        // The trace records step times and points at the evidence.
        let (trace, _) = crate::trace::load_trace(fail.trace.as_ref().unwrap()).unwrap();
        let ev = trace.evidence.unwrap();
        assert_eq!(ev.video.as_deref(), Some("video.mp4"));
        assert_eq!(ev.video_offset_ms, Some(-40));
        assert_eq!(ev.logs, vec!["app.log".to_string()]);
        assert_eq!(ev.log_lines.as_deref(), Some("logs.json"));
        assert!(trace.steps[1].start_ms >= trace.steps[0].start_ms);
        let html = std::fs::read_to_string(&fail.attachments[2].path).unwrap();
        assert!(html.contains("video.mp4") && html.contains("boom"));
    }

    #[test]
    fn pause_answers_and_held_step_names() {
        assert_eq!(parse_pause_answer("\n"), PauseChoice::Continue);
        assert_eq!(parse_pause_answer("r\n"), PauseChoice::Retry);
        assert_eq!(parse_pause_answer(" Q "), PauseChoice::Abort);
        assert_eq!(parse_pause_answer("what"), PauseChoice::Continue);
        assert_eq!(
            held_step_index(std::path::Path::new("/x/step07_tap.png")),
            Some(7)
        );
        assert_eq!(held_step_index(std::path::Path::new("/x/shot.png")), None);
    }

    /// D-50: a crash's reports land in the test's artifacts.
    #[tokio::test]
    async fn a_crash_saves_the_crash_reports_with_the_artifacts() {
        let driver = CrashingTap(
            ScriptDriver::new(vec![node("b", "Crash", 0)]),
            Duration::ZERO,
        );
        let out = tempfile::tempdir().unwrap();
        let mut c = config(10_000);
        c.sync.mode = SyncMode::default();
        c.artifacts.on_failure = true;
        c.artifacts.output_dir = out.path().to_string_lossy().into_owned();
        let mut ex = TestExecutor::new(&driver, c, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Crash"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let saved: Vec<_> = walk(out.path());
        assert!(
            saved
                .iter()
                .any(|p| p.contains("crashlogs") && p.ends_with("app.ips")),
            "{saved:?}"
        );
        fn walk(d: &std::path::Path) -> Vec<String> {
            let mut out = Vec::new();
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk(&p));
                } else {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
            out
        }
    }

    #[tokio::test]
    async fn a_crash_is_reported_at_the_step_that_caused_it() {
        let driver = CrashingTap(
            ScriptDriver::new(vec![node("b", "Crash", 0)]),
            Duration::ZERO,
        );
        let mut c = config(10_000);
        c.sync.mode = SyncMode::default();
        let mut ex = TestExecutor::new(&driver, c, "app");
        let start = Instant::now();
        let r = ex
            .execute_test(
                &test_case(vec![step(tap("Crash")), step(tap("Crash"))]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap();
        assert!(msg.contains("crashed") && msg.contains("boom"), "{msg}");
        assert_eq!(
            driver.0.taps.lock().unwrap().len(),
            1,
            "stops at the crashing step"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn a_crash_is_seen_before_the_settle_cap() {
        // The launcher's wallpaper keeps the settle busy for its full 5 s.
        let driver = CrashingTap(
            ScriptDriver::new(vec![node("b", "Crash", 0)]),
            Duration::from_secs(5),
        );
        let mut c = config(10_000);
        c.sync.mode = SyncMode::default();
        let mut ex = TestExecutor::new(&driver, c, "app");
        let start = Instant::now();
        let r = ex
            .execute_test(&test_case(vec![step(tap("Crash"))]), "d", "app")
            .await
            .unwrap();
        assert!(r.error_message.unwrap().contains("crashed"));
        let took = start.elapsed();
        assert!(took < Duration::from_millis(2500), "{took:?}");
    }

    #[tokio::test]
    async fn a_hung_step_is_cut_off_by_the_watchdog() {
        let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
        let mut ex = TestExecutor::new(&driver, config(50), "app");
        let start = Instant::now();
        let r = ex
            .execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(r.error_message.unwrap().contains("Step timeout"));
        assert!(
            r.infrastructure_failure,
            "a hang on the first step is the device's fault"
        );
    }

    /// An API on 127.0.0.1 that answers every request with `body` (JSON).
    fn json_api(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}")
    }

    fn http(yaml: &str) -> Action {
        Action::Http(Box::new(serde_yaml::from_str(yaml).unwrap()))
    }

    #[tokio::test]
    async fn a_value_an_api_returns_reaches_a_ui_step() {
        // Seed through the API, then find the seeded thing on screen.
        let api = json_api(r#"{"id": "o-7"}"#);
        let driver = ScriptDriver::new(vec![node("row1", "o-6", 0), node("row2", "o-7", 100)]);
        let mut config = config(2000);
        config.captured_vars = ["ORDER_ID".to_string()].into();
        let mut ex = TestExecutor::new(&driver, config, "app");
        let seed = step(http(&format!(
            "{{ method: POST, url: \"{api}/orders\", capture: {{ ORDER_ID: id }} }}"
        )));
        let open = step(tap("${ORDER_ID}"));
        let r = ex
            .execute_test(&test_case(vec![seed, open.clone()]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(r.steps[0].action_name, format!("http(POST {api}/orders)"));
        assert_eq!(driver.taps.lock().unwrap().as_slice(), ["o-7"]);
        // The next test starts without it: no stale value from another test.
        let r = ex
            .execute_test(&test_case(vec![open]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let e = r.error_message.unwrap_or_default();
        assert!(e.contains("`${ORDER_ID}` has no value yet"), "{e}");
        assert_eq!(driver.taps.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_api_check_is_a_test_failure_an_optional_step_forgives() {
        let api = json_api(r#"{"ready": false}"#);
        let driver = ScriptDriver::new(vec![node("ok", "OK", 0)]);
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let check = format!("{{ url: \"{api}/status\", expect: {{ json: {{ ready: true }} }} }}");
        let r = ex
            .execute_test(&test_case(vec![step(http(&check))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let e = r.error_message.unwrap_or_default();
        assert!(e.contains("ready: expected true, got false"), "{e}");
        // Never put down to an app crash or the device.
        assert!(!e.contains("crashed"), "{e}");
        let mut optional = step(http(&check));
        optional.modifiers.optional = true;
        let r = ex
            .execute_test(&test_case(vec![optional, step(tap("OK"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_disabled_target_is_tapped_once_it_becomes_enabled() {
        let mut pay = node("pay", "Pay", 0);
        pay.enabled = false;
        // Enabled from the third lookup on (e.g. after a form validates).
        let driver = ScriptDriver::new(vec![pay]).after(3, |screen| {
            screen.children[0].enabled = true;
        });
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert!(driver.finds.load(Ordering::SeqCst) >= 3, "it waited");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[tokio::test]
    async fn clear_text_waits_for_its_target_to_be_actionable() {
        let mut field = node("name", "Name", 0);
        field.enabled = false;
        let driver = ScriptDriver::new(vec![field]).after(3, |screen| {
            screen.children[0].enabled = true;
        });
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let clear = Action::ClearText {
            selector: Selector::Text("Name".into()),
        };
        let r = ex
            .execute_test(&test_case(vec![step(clear)]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert!(driver.finds.load(Ordering::SeqCst) >= 3, "it waited");
    }

    /// A node with no text whose label merges its parts (RN on iOS).
    fn merged(id: &str, label: &str, y: i32) -> Element {
        Element {
            text: None,
            label: Some(label.into()),
            ..node(id, "", y)
        }
    }

    #[tokio::test]
    async fn a_merged_label_part_is_not_the_element_for_a_negative_check() {
        // Review M2: "Invoice 3, Delete" read as a Delete button that never
        // went away.
        let driver = ScriptDriver::new(vec![merged("row", "Invoice 3, Delete", 100)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_step(&step(not_visible("Delete")), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn the_real_target_beats_a_merged_label_that_came_first() {
        // Review M3: the tab bar's "Home, tab, 1 of 4" is up before the new
        // screen's "Home" title renders; the first poll took the tab.
        let driver = ScriptDriver::new(vec![merged("tab", "Home, tab, 1 of 4", 1800)])
            .after(3, |screen| screen.children.push(node("title", "Home", 100)));
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let r = ex
            .execute_step(&step(tap("Home")), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*driver.tap_ys.lock().unwrap(), [100]);
    }

    #[tokio::test]
    async fn a_merged_label_part_is_used_only_at_the_deadline_and_never_asserted() {
        // Review M3: the widest tier still finds a merged label, once the
        // stricter ones had the whole timeout.
        let driver = ScriptDriver::new(vec![merged("tab", "Home, tab, 1 of 4", 1800)]);
        let mut ex = TestExecutor::new(&driver, config(400), "app");
        let started = Instant::now();
        let visible = Action::AssertVisible {
            selector: Selector::Text("Home".into()),
        };
        let r = ex
            .execute_step(&step(visible), "d", "app", 0)
            .await
            .unwrap();
        // D-10: an assertion on a merged-label part fails, naming the label.
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("Home, tab, 1 of 4"), "{msg}");
        assert!(started.elapsed() >= Duration::from_millis(350));
        let r = ex
            .execute_step(&step(tap("Home")), "d", "app", 1)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*driver.tap_ys.lock().unwrap(), [1800]);
    }

    #[tokio::test]
    async fn a_target_that_stays_disabled_fails_as_not_actionable() {
        // D-3: strict by default. The step fails, names the way out, and
        // nothing is tapped.
        let mut pay = node("pay", "Pay", 0);
        pay.enabled = false;
        let driver = ScriptDriver::new(vec![pay]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(driver.taps.lock().unwrap().is_empty());
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("not actionable"), "{err}");
        assert!(err.contains("was disabled"), "{err}");
        assert!(err.contains("force: true"), "{err}");
    }

    #[tokio::test]
    async fn a_lenient_suite_taps_a_disabled_target_with_a_warning() {
        let mut pay = node("pay", "Pay", 0);
        pay.enabled = false;
        let driver = ScriptDriver::new(vec![pay]);
        let mut cfg = config(300);
        cfg.actionability = velocity_common::Actionability::Lenient;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert_eq!(driver.taps.lock().unwrap().len(), 1);
        assert!(
            r.warnings.iter().any(|w| w.contains("was disabled")),
            "{:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn force_taps_a_disabled_target_at_once() {
        // A React Native view that reports enabled=false but works.
        let mut pay = node("pay", "Pay", 0);
        pay.enabled = false;
        let driver = ScriptDriver::new(vec![pay]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let mut forced = step(tap("Pay"));
        forced.modifiers.force = true;
        let r = ex
            .execute_test(&test_case(vec![forced]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().len(), 1);
        assert_eq!(
            driver.finds.load(Ordering::SeqCst),
            1,
            "no actionability reads"
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[tokio::test]
    async fn a_target_that_moves_between_reads_is_tapped_where_it_stops() {
        // VEL-072 / P13: the stable check runs on every action, not only
        // when the settle hit its cap. Found at y=0, then at y=100 for good.
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]).after(2, |screen| {
            shift(screen, 0, 100);
        });
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
    }

    /// A polling settle with short timings, for the read-count tests.
    fn polling(timeout_ms: u64) -> SuiteConfig {
        let mut c = config(timeout_ms);
        c.sync.mode = SyncMode::Polling;
        c.sync.quiet_ms = Some(30);
        c.sync.reaction_ms = Some(30);
        c.sync.settle_cap_ms = Some(1000);
        c
    }

    #[tokio::test]
    async fn a_settled_screen_needs_no_second_read_before_a_strict_tap() {
        // Review M4: the settle just saw two identical trees, so the strict
        // stable check reuses them instead of reading the tree again (a full
        // `/source`, ~540 ms, on iOS). Before: 2 lookups per tap; now 1.
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        let mut ex = TestExecutor::new(&driver, polling(2000), "app");
        let r = ex
            .execute_test(
                &test_case(vec![step(tap("Pay")), step(tap("Pay"))]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().len(), 2);
        assert_eq!(
            driver.finds.load(Ordering::SeqCst),
            2,
            "one lookup per tap, no re-read"
        );
    }

    /// Audit #11: the scroll loop settles after each swipe, and with no
    /// swipe nothing moved, yet the step settled again afterwards: the
    /// whole reaction floor's reads on an unchanged screen. Before, at the
    /// default 300 ms floor: 15 dumps and 355 ms of settle with no swipe, 5
    /// with one; now 2 (32 ms) and 3, and the next step's wait is one read.
    #[tokio::test]
    async fn scroll_until_visible_settles_once() {
        for (swipes, driver) in [
            (0, ScriptDriver::new(vec![node("row", "Row 1", 800)])),
            (
                1,
                ScriptDriver::new(vec![]).after(2, |s| s.children.push(node("row", "Row 1", 800))),
            ),
        ] {
            let mut cfg = polling(2000);
            cfg.sync.reaction_ms = None;
            let mut ex = TestExecutor::new(&driver, cfg, "app");
            let scroll = Action::ScrollUntilVisible {
                selector: Selector::Text("Row 1".into()),
                direction: Direction::Down,
                max_scrolls: 5,
                within: None,
            };
            let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
            assert_eq!(driver.finds.load(Ordering::SeqCst), swipes + 1);
            // The pre-action settle's two reads, plus one in-loop settle
            // read per swipe; nothing after the loop.
            let dumps = driver.dumps.load(Ordering::SeqCst);
            assert_eq!(dumps, 2 + swipes, "swipes={swipes}");
            assert!(r.settle_ms.unwrap_or(0) < 200, "{:?}", r.settle_ms);
            if swipes > 0 {
                // (This fake's row lands after the loop's settle, so the
                // next wait rightly reads it again.)
                continue;
            }
            let next = step(Action::AssertVisible {
                selector: Selector::Text("Row 1".into()),
            });
            let r = ex.execute_step(&next, "d", "app", 1).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
            assert_eq!(driver.dumps.load(Ordering::SeqCst) - dumps, 1);
        }
    }

    #[tokio::test]
    async fn a_settled_strict_tap_never_resolves_on_a_tree_cached_before_the_settle() {
        // review-fixes #1: `assertVisible: Pay` caches the full tree with
        // Pay mid-slide (y=0); the sheet then comes to rest at y=100. The
        // tap's settle compares live reads (never the lookup cache) and is
        // stable, so the lookup must not hit on the pre-settle cached tree
        // and skip the re-read. Before: tapped at y=0.
        let mut driver =
            ScriptDriver::new(vec![node("pay", "Pay", 0)]).after(1, |screen| shift(screen, 0, 100));
        driver.lookup_cache = Some(Mutex::new(None));
        let mut ex = TestExecutor::new(&driver, polling(2000), "app");
        let seen = step(Action::AssertVisible {
            selector: Selector::Text("Pay".into()),
        });
        let r = ex
            .execute_test(&test_case(vec![seen, step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
    }

    #[tokio::test]
    async fn without_a_settle_a_strict_tap_still_reads_twice() {
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        let mut ex = TestExecutor::new(&driver, config(2000), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.finds.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_lookup_that_had_to_wait_is_read_again_after_a_settle() {
        // The target arrived after the settle (the first lookup missed):
        // the settle's verdict says nothing about it, so it is re-read, and
        // tapped where it stops.
        let driver = ScriptDriver::new(vec![node("other", "Other", 500)])
            .after(2, |screen| screen.children.push(node("pay", "Pay", 0)))
            .after(3, |screen| shift(screen, 0, 100));
        let mut ex = TestExecutor::new(&driver, polling(2000), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
    }

    #[tokio::test]
    async fn web_leaves_the_stable_check_to_the_page() {
        // Review M4: `injected.js` `act` waits two frames for a still box;
        // the executor doesn't read the tree a second time on top.
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        let mut cfg = config(2000);
        cfg.platform = Some(Platform::Web);
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.finds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_one_pixel_wobble_is_still() {
        // Review m1: a spring's fractional frames round either way.
        let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        for n in 2..400 {
            let dy = if n % 2 == 0 { 1 } else { -1 };
            driver = driver.after(n, move |screen| shift(screen, 0, dy));
        }
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_target_that_never_stops_moving_fails_naming_the_animation() {
        // A marquee: its box changes between every two reads. Strict fails,
        // says it was moving (not "reports enabled=false"), and names
        // `force: true`.
        let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        for n in 2..400 {
            driver = driver.after(n, |screen| shift(screen, 10, 0));
        }
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(driver.taps.lock().unwrap().is_empty());
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("still moving"), "{err}");
        assert!(err.contains("continuously animating"), "{err}");
        assert!(err.contains("force: true"), "{err}");
        assert!(!err.contains("enabled=false"), "{err}");
    }

    #[tokio::test]
    async fn the_actionability_mode_reaches_the_driver() {
        use velocity_common::actionability::ActMode;
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        let mut forced = step(tap("Pay"));
        forced.modifiers.force = true;
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay")), forced]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        let mut cfg = config(300);
        cfg.actionability = velocity_common::Actionability::Lenient;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        ex.execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(
            *driver.tap_modes.lock().unwrap(),
            [ActMode::Strict, ActMode::Force, ActMode::Lenient]
        );
    }

    #[test]
    fn lookup_retries_start_fast_then_back_off() {
        let delays: Vec<u64> = (0..7).map(|n| retry_delay(n).as_millis() as u64).collect();
        assert_eq!(delays, vec![20, 50, 100, 100, 250, 500, 500]);
    }

    #[test]
    fn selector_fields_round_trip_through_the_parser() {
        // VEL-242: a combining accent or a control character made invalid YAML.
        for text in [
            "Cafe\u{301}",
            "a\u{1}b",
            "say \"hi\"\n\tthere",
            "日本 \\ x",
            "del\u{7f}",
            "c1\u{85}\u{9f}",
            "ls\u{2028}ps\u{2029}",
            "nc\u{fffe}",
            "bom\u{feff}",
            "\u{1b}[31mred",
        ] {
            let sel = Selector::Compound(vec![
                Selector::Text(text.into()),
                Selector::ClassName("Button".into()),
            ]);
            let yaml = format!(
                "appId: a\ntests:\n  - name: t\n    steps:\n      - tap: {{ {} }}\n",
                selector_fields(&sel)
            );
            let suite = crate::parser::parse_suite_from_str(&yaml)
                .unwrap_or_else(|e| panic!("{text:?}: {e}\n{yaml}"));
            match &suite.tests[0].steps[0].action {
                Action::Tap { selector } => {
                    assert_eq!(selector.to_string(), sel.to_string(), "{yaml}")
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn retries_use_the_whole_timeout() {
        // P7: a 300 ms timeout stopped once the next step (250 ms) would
        // overshoot, and never slept the remainder.
        let deadline = Instant::now() + Duration::from_millis(30);
        let t = Duration::from_millis(1000);
        assert!(next_pause(6, deadline, t).unwrap() <= Duration::from_millis(30));
        assert!(next_pause(6, Instant::now() + Duration::from_secs(9), t).unwrap() <= t / 5);
        assert!(next_pause(0, Instant::now() - Duration::from_millis(1), t).is_none());
    }

    #[tokio::test]
    async fn an_element_that_appears_just_before_the_deadline_is_found() {
        // P7: with a 1 s timeout the old loop looked at 0, 20, 70, 170, 270
        // and 520 ms, then stopped (the next 500 ms step overshot). The
        // element shows at 700 ms; now there are checks at 670, 870, 1000.
        let driver = ScriptDriver::new(vec![]);
        let mut ex = TestExecutor::new(&driver, config(1000), "app");
        let screen = &driver.screen;
        let appear = async {
            tokio::time::sleep(Duration::from_millis(700)).await;
            screen
                .lock()
                .unwrap()
                .children
                .push(node("late", "Late", 0));
        };
        let s = step(tap("Late"));
        let (r, _) = tokio::join!(ex.execute_step(&s, "d", "app", 0), appear);
        let r = r.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_healed_target_is_waited_for_until_enabled() {
        // N-3: the actionability wait re-queried the selector that had just
        // failed, so a healed, briefly disabled button was "gone".
        let mut btn = node("submit", "Submit orders", 0);
        btn.enabled = false;
        let driver = ScriptDriver::new(vec![btn]).after(12, |s| s.children[0].enabled = true);
        let mut cfg = config(1500);
        cfg.healing.enabled = true;
        cfg.healing.mode = Some(velocity_common::HealingMode::Auto);
        cfg.healing.persist_healed = false;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Submit orders"]);
        assert_eq!(r.healed_count(), 1, "{:?}", r.warnings);
    }

    #[tokio::test]
    async fn report_mode_fails_a_healed_step_with_the_patch_and_the_command() {
        // D-7: in CI (the default there) a heal is proposed, not used.
        let driver = ScriptDriver::new(vec![node("submit", "Submit orders", 0)]);
        let mut cfg = config(300);
        cfg.healing.enabled = true;
        cfg.healing.mode = Some(velocity_common::HealingMode::Report);
        cfg.healing.persist_healed = false;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let mut t = step(tap("Submit order"));
        t.source = Some(velocity_common::SourcePos::new("tests/shop.yaml", 7, 9));
        let r = ex
            .execute_test(&test_case(vec![t]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            driver.taps.lock().unwrap().is_empty(),
            "the heal was not used"
        );
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("healing.mode is report"), "{err}");
        assert!(
            err.contains(r#"text: "Submit order" → text: "Submit orders""#),
            "{err}"
        );
        assert!(
            err.contains("velocity heal --accept --config tests/shop.yaml"),
            "{err}"
        );
        // Kept for `velocity heal --accept` although the test failed.
        let kept = ex.selector_engine.healer().healed_mappings();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].test.as_deref(), Some("t"));
        // `off`: no heal at all.
        let mut cfg = config(300);
        cfg.healing.enabled = true;
        cfg.healing.mode = Some(velocity_common::HealingMode::Off);
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
            .await
            .unwrap();
        let err = r.error_message.unwrap_or_default();
        assert!(!err.contains("heal"), "{err}");
        assert!(ex.selector_engine.healer().healed_mappings().is_empty());
    }

    #[tokio::test]
    async fn report_mode_uses_a_target_that_arrives_during_the_last_look() {
        // Review M1: the healing lookup looks the selector up first; a
        // target that rendered just then is the real one, not a heal. It
        // failed as "the heal was not used" with another test's patch.
        let driver = ScriptDriver::new(vec![node("other", "Something else", 500)]);
        *driver.late.lock().unwrap() = Some(node("submit", "Submit order", 0));
        let mut cfg = config(300);
        cfg.healing.enabled = true;
        cfg.healing.mode = Some(velocity_common::HealingMode::Report);
        cfg.healing.persist_healed = false;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Submit order"]);
    }

    #[tokio::test]
    async fn report_mode_proposes_this_steps_heal_not_the_last_one() {
        // Review M1: the patch came from `healed_mappings().pop()`, the last
        // heal of any step or test.
        let driver = ScriptDriver::new(vec![
            node("submit", "Submit orders", 0),
            node("cancel", "Cancel orders", 300),
        ]);
        let mut cfg = config(300);
        cfg.healing.enabled = true;
        cfg.healing.mode = Some(velocity_common::HealingMode::Report);
        cfg.healing.persist_healed = false;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        // A heal from an earlier step, with an unrelated mapping last.
        let r = ex
            .execute_test(&test_case(vec![step(tap("Cancel order"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            r.healed_count() == 0,
            "a heal that wasn't used is not counted: {:?}",
            r.warnings
        );
        let r = ex
            .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
            .await
            .unwrap();
        let err = r.error_message.unwrap_or_default();
        assert!(
            err.contains(r#"text: "Submit order" → text: "Submit orders""#),
            "{err}"
        );
        assert!(!err.contains("Cancel"), "{err}");
        // m6: the reject command names this heal alone.
        assert!(
            err.contains(r#"velocity heal --reject 'text="Submit order"'"#),
            "{err}"
        );
    }

    /// Review NIT 12: a selector holding a masked value can't be rejected
    /// by its (masked) text; the hint names the number instead, and never
    /// the value.
    #[test]
    fn a_reject_hint_never_names_a_masked_selector() {
        let plain = reject_hint(r#"text="Submit order""#, "s.yaml");
        assert!(
            plain.contains(r#"velocity heal --reject 'text="Submit order"' --config s.yaml"#),
            "{plain}"
        );
        let quoted = reject_hint("text=\"it's\"", "s.yaml");
        assert!(quoted.contains(r#"'text="it'\''s"'"#), "{quoted}");
        velocity_common::redact::register_secret("nit12-Zq9xV7pL3kW");
        let masked = reject_hint(r#"text="Card nit12-Zq9xV7pL3kW""#, "s.yaml");
        assert!(!masked.contains("nit12-Zq9xV7pL3kW"), "{masked}");
        assert!(masked.contains("--reject <N>"), "{masked}");
    }

    fn shot(name: &str) -> Step {
        Step {
            timeout_ms: Some(600),
            ..step(Action::AssertScreenshot {
                name: name.into(),
                threshold: None,
                cluster_px: None,
                mask: vec![],
                tolerance: None,
                comparator: None,
                crop: None,
            })
        }
    }

    /// An executor whose suite lives in `dir` (baselines under it).
    fn in_suite(dir: &std::path::Path) -> SuiteConfig {
        let mut c = config(100);
        c.suite_dir = Some(dir.to_path_buf());
        c.artifacts.output_dir = dir.join("out").to_string_lossy().into_owned();
        c
    }

    fn put(path: &std::path::Path, data: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    #[tokio::test]
    async fn screenshot_names_that_share_a_baseline_file_are_an_error() {
        // VEL-201: "a.b" and "a b" both read velocity-baselines/<p>/a_b.png.
        // New names get their own files; two names that fall back to one
        // legacy file are still an error.
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[]);
        put(
            &dir.path().join("velocity-baselines/android/vel201_a_b.png"),
            &screen,
        );
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex
            .execute_step(&shot("vel201 a.b"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let r = ex
            .execute_step(&shot("vel201 a b"), "d", "app", 1)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("both map to baseline"), "{msg}");
    }

    #[test]
    fn clean_names_keep_their_file_and_others_get_a_hash() {
        // VT-2 / VEL-201.
        assert_eq!(baseline_file("login"), "login.png");
        assert_eq!(baseline_file("home-2_dark"), "home-2_dark.png");
        let (a, b) = (baseline_file("cart total"), baseline_file("cart/total"));
        assert_ne!(a, b);
        assert!(a.starts_with("cart_total-") && a.ends_with(".png"), "{a}");
        // Non-ASCII letters no longer reach the file system as they are.
        assert!(baseline_file("café").is_ascii());
        assert!(baseline_file(&"x".repeat(300)).len() < 120);
    }

    #[tokio::test]
    async fn names_that_differ_only_in_case_are_an_error() {
        // Review m7: "Home" and "home" share one file on a case-insensitive
        // file system (default macOS, Windows).
        // "Home" has a baseline, so the test doesn't depend on whether
        // `CI` is set (a missing one fails there: review-s8 MAJOR-2).
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[]);
        put(
            &dir.path().join("velocity-baselines/android/Home.png"),
            &screen,
        );
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("Home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let r = ex.execute_step(&shot("home"), "d", "app", 1).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert!(r.error_message.unwrap_or_default().contains("both map"));
    }

    #[test]
    fn strokes_survive_hostile_bounds() {
        // Review m6: line_across and aim_at still added in i32.
        let huge = Rect {
            x: i32::MAX - 10,
            y: i32::MAX - 10,
            width: i32::MAX,
            height: i32::MAX,
        };
        for d in [
            Direction::Up,
            Direction::Down,
            Direction::Left,
            Direction::Right,
        ] {
            let _ = line_across(&huge, d);
            let _ = aim_at(&huge, &huge, d, true);
            let screen = Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 1920,
            };
            let _ = aim_at(&screen, &huge, d, false);
        }
    }

    #[tokio::test]
    async fn a_first_frame_that_matches_passes_with_one_capture() {
        // VT-5 / B5: two captures and a 250 ms pause at least, every time.
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        put(
            &dir.path().join("velocity-baselines/android/home.png"),
            &screen,
        );
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.shots.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_blinking_caret_is_still_under_the_steps_tolerance() {
        // VT-5 / B5: a 1 px speck flipping between frames used to keep the
        // zero-diff gate from ever seeing a still screen.
        let dir = tempfile::tempdir().unwrap();
        let driver = ScriptDriver {
            cycle_frames: true,
            ..ScriptDriver::new(vec![])
        };
        *driver.frames.lock().unwrap() = vec![
            crate::visual::painted(40, 40, &[]),
            crate::visual::painted(40, 40, &[(20, 20, 1, 1)]),
        ];
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
        let r = ex
            .execute_step(&shot("caret"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.shots.load(Ordering::SeqCst), 2);
        assert!(ex.warnings.iter().all(|w| !w.contains("held still")));
        assert!(dir
            .path()
            .join("velocity-baselines/android/40x40/caret.png")
            .is_file());
    }

    #[tokio::test]
    async fn an_unstable_screen_fails_and_never_becomes_the_baseline() {
        // VT-5 / B6: the frame after the timeout was written as the
        // baseline with only a warning.
        let dir = tempfile::tempdir().unwrap();
        let driver = ScriptDriver {
            cycle_frames: true,
            ..ScriptDriver::new(vec![])
        };
        let (a, b) = (
            crate::visual::painted(40, 40, &[]),
            crate::visual::painted(40, 40, &[(10, 0, 2, 40)]),
        );
        *driver.frames.lock().unwrap() = vec![a.clone(), b.clone()];
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
        let r = ex.execute_step(&shot("spin"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.contains("never held still") && msg.contains("not saved"),
            "{msg}"
        );
        assert!(!dir
            .path()
            .join("velocity-baselines/android/40x40/spin.png")
            .exists());
        for kind in ["previous", "actual", "diff"] {
            assert!(
                dir.path()
                    .join(format!("out/diffs/spin-{kind}.png"))
                    .is_file(),
                "{kind}"
            );
        }
        // Without --update-baselines and no baseline: no baseline either.
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("spin"), "d", "app", 1).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert!(!dir
            .path()
            .join("velocity-baselines/android/40x40/spin.png")
            .exists());
        // With a baseline equal to one of the frames: passes.
        put(&dir.path().join("velocity-baselines/android/spin.png"), &b);
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("spin"), "d", "app", 2).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_corrupt_capture_is_taken_again_and_never_a_config_error() {
        // VT-4 / VEL-203: "invalid PNG" was a Config error (exit 2).
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[]);
        put(
            &dir.path().join("velocity-baselines/android/c.png"),
            &screen,
        );
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![vec![0x89, b'P', b'N', b'G'], screen];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("c"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        // Garbage every time: a device failure, not configuration.
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![vec![1, 2, 3]];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let plain = ScreenshotStep {
            threshold: None,
            cluster_px: None,
            mask: &[],
            rule: crate::visual::PixelRule::default(),
            crop: None,
        };
        let e = ex.assert_screenshot("d", "c", plain).await.unwrap_err();
        assert_ne!(e.kind(), velocity_common::ErrorKind::ConfigError, "{e}");
        assert_eq!(e.exit_code(), 3, "{e}");
        assert_eq!(driver.shots.load(Ordering::SeqCst), 3);
        // The run's outcome too: an infrastructure failure, so the suite
        // exits 3, not 1 (review-s8 m-c).
        assert!(is_infrastructure(&e), "{e}");
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![vec![1, 2, 3]];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex
            .execute_test(&test_case(vec![shot("c")]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(r.infrastructure_failure, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn huge_masks_and_unreadable_baselines_say_so_at_once() {
        // Review-s8 m-f: masks over most of the screen failed every
        // frame-to-frame comparison, so the step waited for a still screen
        // and advised masking more; a corrupt baseline read "invalid PNG".
        let dir = tempfile::tempdir().unwrap();
        let mut big = node("big", "list", 0);
        big.bounds.width = 5_000;
        big.bounds.height = 9_000;
        let mut driver = ScriptDriver::new(vec![big]);
        driver.bars = Some(vec![]);
        *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 40, &[])];
        let masked = |name: &str| Step {
            timeout_ms: Some(3_000),
            ..step(Action::AssertScreenshot {
                name: name.into(),
                threshold: None,
                cluster_px: None,
                mask: vec![Selector::Id("big".into())],
                tolerance: None,
                comparator: None,
                crop: None,
            })
        };
        for update in [false, true] {
            let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
                .with_update_baselines(update);
            let start = Instant::now();
            let r = ex.execute_step(&masked("m"), "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Failed);
            let msg = r.error_message.unwrap_or_default();
            assert!(
                msg.contains("masks cover") && !msg.contains("held still"),
                "{msg}"
            );
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        put(
            &dir.path().join("velocity-baselines/android/bad.png"),
            b"not a png",
        );
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("bad"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.contains("bad.png") && msg.contains("re-record"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn assert_text_reads_a_merged_label_it_found_loosely() {
        // Review-s8 m-b: the lookup found "Points" as part of a merged
        // label at its deadline, then the read loop (Fold) never found it,
        // waited a second timeout and failed with actual "".
        let mut row = node("row", "Points\n120", 0);
        row.element_type = "View".into();
        let driver = ScriptDriver::new(vec![row]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let start = Instant::now();
        let r = ex
            .execute_step(
                &step(Action::AssertText {
                    selector: Selector::Text("Points".into()),
                    expected: "Points\n120".into(),
                }),
                "d",
                "app",
                0,
            )
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(start.elapsed() < Duration::from_millis(1_500));
    }

    #[tokio::test]
    async fn every_match_of_a_mask_is_masked() {
        // VT-3 / VEL-223, B4: only the first row's timestamp was masked.
        let mut driver =
            ScriptDriver::new(vec![node("ts", "10:01", 300), node("ts", "10:02", 900)]);
        driver.bars = Some(vec![]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let masks = ex
            .screenshot_masks("d", &[], &[Selector::Id("ts".into())])
            .await
            .unwrap();
        assert_eq!(regions(&masks), [(0, 300, 100, 40), (0, 900, 100, 40)]);
    }

    #[test]
    fn headless_runs_keep_their_own_baselines_next_to_the_suite() {
        // B1: headless and device Android runs shared android/<name>.png.
        // B2: the folder came from the process's working directory.
        // D-V3: and each capture size has its own folder.
        let dir = tempfile::tempdir().unwrap();
        let driver = ScriptDriver::new(vec![]);
        let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let size = (1080, 1920);
        let device = ex.baseline_place("home", size, None);
        assert_eq!(
            device.write,
            dir.path()
                .join("velocity-baselines/android/1080x1920/home.png")
        );
        let mut c = in_suite(dir.path());
        c.driver = Some(velocity_common::DriverMode::Headless);
        let ex = TestExecutor::new(&driver, c, "app");
        let headless = ex.baseline_place("home", size, None);
        assert_eq!(
            headless.write,
            dir.path()
                .join("velocity-baselines/android-headless/1080x1920/home.png")
        );
        assert!(headless.read.is_none());
        // An existing shared (pre-D-V3) baseline is still read, with a hint.
        let legacy = dir.path().join("velocity-baselines/android/home.png");
        put(&legacy, b"x");
        let p = ex.baseline_place("home", size, None);
        assert_eq!(p.read.as_deref(), Some(legacy.as_path()));
        assert!(p.legacy);
        // A baseline in the folder the run started from is found too.
        let name = format!("vel-b2-{}", std::process::id());
        let cwd = PathBuf::from("velocity-baselines/android").join(format!("{name}.png"));
        put(&cwd, b"x");
        let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let p = ex.baseline_place(&name, size, None);
        let _ = std::fs::remove_file(&cwd);
        let _ = std::fs::remove_dir("velocity-baselines/android");
        let _ = std::fs::remove_dir("velocity-baselines");
        assert_eq!(p.read, Some(cwd));
        assert!(p.legacy);
        assert_eq!(
            p.write,
            dir.path()
                .join(format!("velocity-baselines/android/1080x1920/{name}.png"))
        );
    }

    /// review-d5 m-3 / review-fixes #3: a capture at a size that has no
    /// baseline yet while another size has one (a rotation, another
    /// device) fails a plain run without writing, naming
    /// `--update-baselines=missing`; `=missing` records the new size and
    /// leaves the other size alone.
    #[tokio::test]
    async fn a_baseline_only_at_another_size_is_missing_for_this_size() {
        use velocity_common::BaselineUpdate;
        let dir = tempfile::tempdir().unwrap();
        let portrait = crate::visual::painted(40, 60, &[(5, 5, 10, 10)]);
        let landscape = crate::visual::painted(60, 40, &[(5, 5, 10, 10)]);
        let upright = dir.path().join("velocity-baselines/android/40x60/home.png");
        put(&upright, &portrait);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![landscape.clone()];
        let rotated = dir.path().join("velocity-baselines/android/60x40/home.png");
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .with_update_baselines(BaselineUpdate::Off);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.contains("size mismatch: baseline is 40x60, capture is 60x40")
                && msg.contains("--update-baselines=missing"),
            "{msg}"
        );
        assert!(!rotated.exists(), "a plain run wrote the rotated screen");
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .with_update_baselines(BaselineUpdate::Missing);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(std::fs::read(&rotated).unwrap(), landscape);
        assert_eq!(std::fs::read(&upright).unwrap(), portrait, "untouched");
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .with_update_baselines(BaselineUpdate::Changed);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    /// review-fixes #3: a file in the capture's own size folder with another
    /// pixel size is a real size mismatch, and `=missing` doesn't hide it.
    #[tokio::test]
    async fn a_wrong_sized_file_in_the_same_size_folder_is_a_size_mismatch() {
        use velocity_common::BaselineUpdate;
        let dir = tempfile::tempdir().unwrap();
        let small = crate::visual::painted(20, 20, &[(5, 5, 4, 4)]);
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        let file = dir.path().join("velocity-baselines/android/40x40/home.png");
        put(&file, &small);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        for mode in [BaselineUpdate::Off, BaselineUpdate::Missing] {
            let mut ex =
                TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(mode);
            let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Failed, "{mode:?}");
            let msg = r.error_message.unwrap_or_default();
            assert!(
                msg.contains("size mismatch") && msg.contains("is 20x20, capture is 40x40"),
                "{mode:?}: {msg}"
            );
            assert_eq!(std::fs::read(&file).unwrap(), small, "{mode:?}");
        }
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .with_update_baselines(BaselineUpdate::Changed);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(std::fs::read(&file).unwrap(), screen);
    }

    /// review-fixes #3: a first recording on two screen sizes in one
    /// parallel run with `=missing`: both devices record their own size and
    /// pass (before, the second one saw the first's fresh file and failed
    /// with "size mismatch" on every run).
    #[tokio::test]
    async fn two_sizes_record_their_first_baselines_in_one_parallel_run() {
        use velocity_common::BaselineUpdate;
        let dir = tempfile::tempdir().unwrap();
        let phone = crate::visual::painted(40, 60, &[(5, 5, 10, 10)]);
        let tablet = crate::visual::painted(60, 80, &[(5, 5, 10, 10)]);
        let (da, db) = (ScriptDriver::new(vec![]), ScriptDriver::new(vec![]));
        *da.frames.lock().unwrap() = vec![phone.clone()];
        *db.frames.lock().unwrap() = vec![tablet.clone()];
        let run = velocity_common::CreatedBaselines::default();
        let device = |drv| {
            TestExecutor::new(drv, in_suite(dir.path()), "app")
                .with_created_baselines(run.clone())
                .with_update_baselines(BaselineUpdate::Missing)
        };
        let (mut a, mut b) = (device(&da), device(&db));
        let shot_a = shot("home");
        let shot_b = shot("home");
        let (ra, rb) = tokio::join!(
            a.execute_step(&shot_a, "a", "app", 0),
            b.execute_step(&shot_b, "b", "app", 0)
        );
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        assert_eq!(ra.status, StepStatus::Passed, "{:?}", ra.error_message);
        assert_eq!(rb.status, StepStatus::Passed, "{:?}", rb.error_message);
        // Sequentially too (device a strictly first), which is the order
        // that failed before.
        let base = dir.path().join("velocity-baselines/android");
        assert_eq!(std::fs::read(base.join("40x60/home.png")).unwrap(), phone);
        assert_eq!(std::fs::read(base.join("60x80/home.png")).unwrap(), tablet);
        std::fs::remove_dir_all(&base).unwrap();
        let (mut a, mut b) = (device(&da), device(&db));
        let ra = a.execute_step(&shot_a, "a", "app", 0).await.unwrap();
        let rb = b.execute_step(&shot_b, "b", "app", 0).await.unwrap();
        assert_eq!(ra.status, StepStatus::Passed, "{:?}", ra.error_message);
        assert_eq!(rb.status, StepStatus::Passed, "{:?}", rb.error_message);
        // The next plain run passes on both.
        for (drv, id) in [(&da, "a"), (&db, "b")] {
            let mut ex = TestExecutor::new(drv, in_suite(dir.path()), "app");
            let r = ex.execute_step(&shot("home"), id, "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        }
    }

    #[tokio::test]
    async fn baselines_live_under_the_capture_size_and_legacy_ones_warn() {
        // D-V3: <platform>/<W>x<H>/<name>.png; the old flat file is read
        // (with a warning) only when it is the captured size.
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
        let legacy = dir.path().join("velocity-baselines/android/size.png");
        put(&legacy, &screen);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("size"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(
            ex.warnings
                .iter()
                .any(|w| w.contains("older layout") && w.contains("40x30")),
            "{:?}",
            ex.warnings
        );
        // Re-recording writes only the new layout.
        let other = crate::visual::painted(40, 30, &[(20, 5, 10, 10)]);
        *driver.frames.lock().unwrap() = vec![other.clone()];
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
        let r = ex.execute_step(&shot("size"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let sized = dir.path().join("velocity-baselines/android/40x30/size.png");
        assert_eq!(std::fs::read(&sized).unwrap(), other);
        assert_eq!(std::fs::read(&legacy).unwrap(), screen, "legacy untouched");
        // Another device size never reads that legacy file.
        let small = crate::visual::painted(20, 20, &[]);
        let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let p = ex.baseline_place("size", (20, 20), crate::visual::png_size(&small));
        assert!(p.read.is_none(), "{:?}", p.read);
    }

    /// U1: a mismatch leaves a review record naming the baseline at this
    /// capture size, so `velocity baselines accept` (and the inspector's
    /// review page) can replace it; accepting makes the next run pass.
    #[tokio::test]
    async fn a_mismatch_is_recorded_for_review_and_accepting_it_passes() {
        let dir = tempfile::tempdir().unwrap();
        let old = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
        let baseline = dir.path().join("velocity-baselines/android/40x30/home.png");
        put(&baseline, &old);
        let new = crate::visual::painted(40, 30, &[(20, 5, 15, 15)]);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![new.clone()];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);

        let out = dir.path().join("out");
        let list = crate::baselines::pending(&out);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].id, "diffs/home");
        assert_eq!(list[0].name, "home");
        assert_eq!(
            list[0].baseline.canonicalize().unwrap(),
            baseline.canonicalize().unwrap()
        );
        assert!(list[0].detail.contains("similar"), "{}", list[0].detail);
        crate::baselines::accept(&list[0]).unwrap();
        assert_eq!(std::fs::read(&baseline).unwrap(), new);
        assert!(crate::baselines::pending(&out).is_empty());

        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    /// fi56 MAJOR 1: a record from a failed run outlived a later pass (or
    /// `--update-baselines`), and `baselines accept all` then wrote that
    /// run's old frame over the good baseline.
    #[tokio::test]
    async fn a_later_pass_or_update_clears_the_review_record() {
        let dir = tempfile::tempdir().unwrap();
        let good = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
        let broken = crate::visual::painted(40, 30, &[(20, 5, 15, 15)]);
        let baseline = dir.path().join("velocity-baselines/android/40x30/home.png");
        put(&baseline, &good);
        let out = dir.path().join("out");
        let driver = ScriptDriver::new(vec![]);
        let run = |frame: &Vec<u8>, update: bool| {
            *driver.frames.lock().unwrap() = vec![frame.clone()];
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(update)
        };

        // Monday: it fails and is recorded.
        let r = run(&broken, false)
            .execute_step(&shot("home"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert_eq!(crate::baselines::pending(&out).len(), 1);
        // Tuesday: the app is fixed and the run passes.
        let r = run(&good, false)
            .execute_step(&shot("home"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let list = crate::baselines::pending(&out);
        assert!(
            crate::baselines::select(&list, "all").is_empty(),
            "{list:?}"
        );
        assert_eq!(std::fs::read(&baseline).unwrap(), good);

        // The same with --update-baselines instead of a pass.
        let r = run(&broken, false)
            .execute_step(&shot("home"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert_eq!(crate::baselines::pending(&out).len(), 1);
        let reviewed = crate::visual::painted(40, 30, &[(1, 1, 4, 4)]);
        let r = run(&reviewed, true)
            .execute_step(&shot("home"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(crate::baselines::pending(&out).is_empty());
        assert_eq!(std::fs::read(&baseline).unwrap(), reviewed);
    }

    /// review-fi56 m1: records are named by file only, so a pass of
    /// `home` on another screen size (same output dir) deleted this size's
    /// fresh failure before anyone reviewed it.
    #[tokio::test]
    async fn a_pass_on_another_size_keeps_this_sizes_review_record() {
        let dir = tempfile::tempdir().unwrap();
        let phone = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
        let tablet = crate::visual::painted(60, 80, &[(5, 5, 10, 10)]);
        put(
            &dir.path().join("velocity-baselines/android/40x30/home.png"),
            &phone,
        );
        put(
            &dir.path().join("velocity-baselines/android/60x80/home.png"),
            &tablet,
        );
        let out = dir.path().join("out");
        let driver = ScriptDriver::new(vec![]);
        // The phone fails...
        *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 30, &[(20, 5, 15, 15)])];
        let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .execute_step(&shot("home"), "phone", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        // ...then the tablet passes the same assertScreenshot.
        *driver.frames.lock().unwrap() = vec![tablet.clone()];
        let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .execute_step(&shot("home"), "tablet", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let list = crate::baselines::pending(&out);
        assert_eq!(list.len(), 1, "the phone's failure is still up for review");
        assert!(
            list[0].baseline.ends_with("40x30/home.png"),
            "{:?}",
            list[0].baseline
        );
        // The phone passing clears it.
        *driver.frames.lock().unwrap() = vec![phone.clone()];
        let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .execute_step(&shot("home"), "phone", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(crate::baselines::pending(&out).is_empty());
    }

    /// fi56 (review MINOR): an unstable screen cleared the current-layout
    /// record while the failure had been recorded under the legacy file
    /// name it read, so the old frame stayed acceptable.
    #[tokio::test]
    async fn an_unstable_screen_clears_the_legacy_named_record() {
        let dir = tempfile::tempdir().unwrap();
        let name = "cart total";
        let legacy = legacy_baseline_file(name);
        assert_ne!(legacy, baseline_file(name));
        let old = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        put(
            &dir.path()
                .join("velocity-baselines/android/40x40")
                .join(&legacy),
            &old,
        );
        let out = dir.path().join("out");
        let driver = ScriptDriver {
            cycle_frames: true,
            ..ScriptDriver::new(vec![])
        };
        *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 40, &[(20, 5, 15, 15)])];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot(name), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert_eq!(crate::baselines::pending(&out).len(), 1);

        *driver.frames.lock().unwrap() = vec![
            crate::visual::painted(40, 40, &[]),
            crate::visual::painted(40, 40, &[(10, 0, 2, 40)]),
        ];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot(name), "d", "app", 0).await.unwrap();
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("never held still"), "{msg}");
        let list = crate::baselines::pending(&out);
        assert!(list.is_empty(), "{list:?}");
    }

    #[tokio::test]
    async fn a_missing_baseline_is_created_and_fails_everywhere() {
        // D-V1: it was created and the step passed unless a CI variable was
        // set; an unknown CI passed silently (VEL-214).
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("created") && msg.contains("commit"), "{msg}");
        let created = dir.path().join("velocity-baselines/android/40x40/new.png");
        assert_eq!(std::fs::read(&created).unwrap(), screen);
        // A retry in the same run doesn't pass on the unreviewed file.
        let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed, "retry flipped to a pass");
        // The next run compares with it.
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    /// review-d5 M-1: two runs in one process (`velocity daemon`, MCP):
    /// the second passes once the baseline exists, in every mode that
    /// compares; the registry no longer outlives the run.
    #[tokio::test]
    async fn a_created_baseline_passes_on_the_next_run_in_the_same_process() {
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        // Run 1 creates it and fails.
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        drop(ex);
        // Run 2 (plain) and run 3 (=missing) pass on it.
        for mode in [
            velocity_common::BaselineUpdate::Off,
            velocity_common::BaselineUpdate::Missing,
        ] {
            let mut ex =
                TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(mode);
            let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
            assert_eq!(
                r.status,
                StepStatus::Passed,
                "{mode:?}: {:?}",
                r.error_message
            );
        }
    }

    /// review-d5 M-1: one run's parallel devices share the registry: a
    /// device that finds the baseline another device of this run just
    /// created fails too; the next run's devices pass.
    #[tokio::test]
    async fn parallel_devices_share_one_runs_created_baselines() {
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        let run = velocity_common::CreatedBaselines::default();
        let device = |run: &velocity_common::CreatedBaselines| {
            TestExecutor::new(&driver, in_suite(dir.path()), "app")
                .with_created_baselines(run.clone())
        };
        let (mut a, mut b) = (device(&run), device(&run));
        let r = a.execute_step(&shot("home"), "a", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let r = b.execute_step(&shot("home"), "b", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed, "device b passed on it");
        assert_eq!(run.len(), 1);
        let next = velocity_common::CreatedBaselines::default();
        let (mut a, mut b) = (device(&next), device(&next));
        for (ex, id) in [(&mut a, "a"), (&mut b, "b")] {
            let r = ex.execute_step(&shot("home"), id, "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        }
    }

    #[tokio::test]
    async fn update_baselines_missing_writes_only_missing_ones_and_passes() {
        // D-V1: `--update-baselines=missing`.
        let dir = tempfile::tempdir().unwrap();
        let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![screen.clone()];
        let missing = velocity_common::BaselineUpdate::Missing;
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(missing);
        let r = ex
            .execute_step(&shot("fresh"), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let fresh = dir
            .path()
            .join("velocity-baselines/android/40x40/fresh.png");
        assert_eq!(std::fs::read(&fresh).unwrap(), screen);
        assert!(ex.warnings.iter().any(|w| w.contains("created baseline")));
        // An existing baseline that differs is still compared, and kept.
        let old = crate::visual::painted(40, 40, &[(25, 25, 10, 10)]);
        let kept = dir.path().join("velocity-baselines/android/40x40/kept.png");
        put(&kept, &old);
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(missing);
        let r = ex.execute_step(&shot("kept"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert_eq!(std::fs::read(&kept).unwrap(), old);
    }

    fn shot_with(name: &str, f: impl FnOnce(&mut Action)) -> Step {
        let mut s = shot(name);
        f(&mut s.action);
        s
    }

    #[tokio::test]
    async fn tolerance_and_the_perceptual_comparator_are_per_step() {
        // D-V2: a uniform grey +20 drift (another host's shading) fails
        // the default rule; `tolerance: 24` or `comparator: perceptual`
        // accept it.
        let dir = tempfile::tempdir().unwrap();
        let grey = |v: u8| {
            let mut png = Vec::new();
            let mut e = png::Encoder::new(&mut png, 20, 20);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[v, v, v, 255].repeat(400)).unwrap();
            w.finish().unwrap();
            png
        };
        put(
            &dir.path()
                .join("velocity-baselines/android/20x20/drift.png"),
            &grey(100),
        );
        let driver = ScriptDriver::new(vec![]);
        *driver.frames.lock().unwrap() = vec![grey(120)];
        let run = |step: Step| {
            let driver = &driver;
            let dir = dir.path().to_path_buf();
            async move {
                let mut ex = TestExecutor::new(driver, in_suite(&dir), "app");
                ex.execute_step(&step, "d", "app", 0).await.unwrap().status
            }
        };
        assert_eq!(run(shot("drift")).await, StepStatus::Failed);
        let tol = shot_with("drift", |a| {
            if let Action::AssertScreenshot { tolerance, .. } = a {
                *tolerance = Some(24);
            }
        });
        assert_eq!(run(tol).await, StepStatus::Passed);
        let perceptual = shot_with("drift", |a| {
            if let Action::AssertScreenshot { comparator, .. } = a {
                *comparator = Some(velocity_common::Comparator::Perceptual);
            }
        });
        assert_eq!(run(perceptual).await, StepStatus::Passed);
    }

    #[tokio::test]
    async fn crop_compares_only_the_cropped_part() {
        // VT-12 (maskLimit decision): two screens that differ outside the
        // card pass; inside, they fail. The 50% mask limit still holds
        // inside the crop.
        let dir = tempfile::tempdir().unwrap();
        let mut card = node("card", "Card", 0);
        card.bounds = Rect {
            x: 0,
            y: 0,
            width: 540,
            height: 960,
        };
        let mut driver = ScriptDriver::new(vec![card]);
        driver.bars = Some(vec![]);
        // 108x192 frames: a tenth of the 1080x1920 screen.
        let base = crate::visual::painted(108, 192, &[(10, 10, 10, 10)]);
        let outside = crate::visual::painted(108, 192, &[(10, 10, 10, 10), (70, 120, 20, 20)]);
        let inside = crate::visual::painted(108, 192, &[(10, 10, 10, 10), (20, 40, 20, 20)]);
        for (crop, label) in [
            (
                velocity_common::ScreenshotCrop::Element(Selector::Id("card".into())),
                "element",
            ),
            (
                velocity_common::ScreenshotCrop::Rect(Rect {
                    x: 0,
                    y: 0,
                    width: 540,
                    height: 960,
                }),
                "rect",
            ),
        ] {
            let name = format!("card-{label}");
            let step = shot_with(&name, |a| {
                if let Action::AssertScreenshot { crop: c, .. } = a {
                    *c = Some(crop.clone());
                }
            });
            *driver.frames.lock().unwrap() = vec![base.clone()];
            let mut ex =
                TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
            let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
            let stored = std::fs::read(
                dir.path()
                    .join(format!("velocity-baselines/android/108x192/{name}.png")),
            )
            .unwrap();
            assert_eq!(crate::visual::png_size(&stored), Some((54, 96)), "{label}");
            for (frame, want) in [
                (&outside, StepStatus::Passed),
                (&inside, StepStatus::Failed),
            ] {
                *driver.frames.lock().unwrap() = vec![frame.clone()];
                let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
                let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
                assert_eq!(r.status, want, "{label}: {:?}", r.error_message);
            }
        }
        // A mask over most of the crop is still refused.
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let step = shot_with("card-element", |a| {
            if let Action::AssertScreenshot { crop, mask, .. } = a {
                *crop = Some(velocity_common::ScreenshotCrop::Element(Selector::Id(
                    "card".into(),
                )));
                *mask = vec![Selector::Id("card".into())];
            }
        });
        let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert!(r.error_message.unwrap_or_default().contains("masks cover"));
    }

    /// review-d5 m-2: a `crop:` selector that matches several elements
    /// warns, as an action does, and fails under `selectors.strict`.
    #[tokio::test]
    async fn an_ambiguous_crop_selector_warns_or_fails_when_strict() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = node("price", "Price", 0);
        a.bounds = Rect {
            x: 0,
            y: 0,
            width: 540,
            height: 400,
        };
        let mut b = a.clone();
        b.bounds.y = 900;
        let mut driver = ScriptDriver::new(vec![a, b]);
        driver.bars = Some(vec![]);
        *driver.frames.lock().unwrap() = vec![crate::visual::painted(108, 192, &[])];
        let step = shot_with("price", |act| {
            if let Action::AssertScreenshot { crop, .. } = act {
                *crop = Some(velocity_common::ScreenshotCrop::Element(Selector::Text(
                    "Price".into(),
                )));
            }
        });
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
        let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(
            ex.warnings.iter().any(|w| w.contains("matched 2")),
            "{:?}",
            ex.warnings
        );
        // Review NIT 14: an ambiguous crop uses the match an action would
        // (the first in tree order), and the warning says so.
        assert!(
            ex.warnings.iter().any(|w| w.contains(&format!(
                "used {}",
                velocity_common::selector_match::BARE_PICK_RULE
            ))),
            "{:?}",
            ex.warnings
        );
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
            .with_update_baselines(true)
            .with_strict_selectors(true);
        let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("2 match"), "{msg}");
    }

    #[tokio::test]
    async fn a_frozen_status_bar_is_compared_instead_of_masked() {
        // D-V4: `screenshots.freezeStatusBar` asks the driver once per
        // device; where it took, the bar is no longer masked.
        let dir = tempfile::tempdir().unwrap();
        let mut driver = ScriptDriver::new(vec![]);
        driver.bars = Some(vec![Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 200,
        }]);
        let base = crate::visual::painted(108, 192, &[]);
        // A change inside the status bar (y < 20 px).
        let clock = crate::visual::painted(108, 192, &[(40, 5, 20, 10)]);
        put(
            &dir.path()
                .join("velocity-baselines/android/108x192/bar.png"),
            &base,
        );
        *driver.frames.lock().unwrap() = vec![clock];
        let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("bar"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "masked by default");
        assert_eq!(driver.freezes.load(Ordering::SeqCst), 0);
        let mut c = in_suite(dir.path());
        c.screenshots.freeze_status_bar = true;
        let mut ex = TestExecutor::new(&driver, c, "app");
        let r = ex.execute_step(&shot("bar"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed, "the bar is compared");
        let _ = ex.execute_step(&shot("bar"), "d", "app", 1).await.unwrap();
        assert_eq!(driver.freezes.load(Ordering::SeqCst), 1, "asked once");
    }

    #[tokio::test]
    async fn a_refused_tap_is_not_reported_as_acted_on() {
        // VEL-274: "acted on it anyway" and then "Element not found".
        let mut off = node("pay", "Pay", 0);
        off.enabled = false;
        let mut driver = ScriptDriver::new(vec![off]);
        driver.refuse_disabled = true;
        let mut ex = TestExecutor::new(&driver, config(200), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(msg.contains("not actionable"), "{msg}");
        assert!(
            !r.warnings.iter().any(|w| w.contains("acted on it anyway")),
            "{:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn tapping_one_of_several_matches_is_reported() {
        let mut driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
        driver.cached = true;
        let mut ex = TestExecutor::new(&driver, config(500), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert!(
            r.warnings.iter().any(|w| w.contains("matched 2 elements")),
            "{:?}",
            r.warnings
        );
    }

    fn main_step<'a>(intent: &'a str, scope: &'a str) -> NaturalStep<'a> {
        NaturalStep {
            intent,
            kind: NaturalKind::Act,
            data: None,
            data_name: None,
            wait_until: None,
            intent_template: None,
            platform: Platform::Android,
            scope,
            fresh: false,
            describe_data: false,
        }
    }

    #[tokio::test]
    async fn a_step_whose_wait_until_fails_is_not_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("until", calls.clone());
        // Tapping "Buy" never shows a receipt: the check can't be planned.
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let test = test_case(vec![step(Action::Natural {
            intent: "tap buy".into(),
            kind: NaturalKind::Act,
            data: None,
            data_name: None,
            wait_until: Some("the receipt is shown".into()),
            intent_template: None,
        })]);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            nl.cached(&main_step("tap buy", "t#0")).await.is_none(),
            "a plan that didn't reach its waitUntil must not be replayed"
        );
    }

    /// The model API is down for every call.
    struct Outage;

    #[async_trait::async_trait]
    impl NaturalPlanner for Outage {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            Err(VelocityError::ServiceUnavailable {
                service: "Anthropic API".into(),
                detail: "HTTP 529".into(),
            })
        }
    }

    #[tokio::test]
    async fn natural_step_errors_keep_their_type() {
        // NB-1: an API outage became a Config error (permanent, exit 2),
        // and an unmet waitUntil a configuration problem (VEL-222).
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let dir = std::env::temp_dir().join(format!("vel-exec-{}-outage", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nl = Arc::new(NaturalResolver::new(
            Box::new(Outage),
            dir.join("nl-cache.json"),
        ));
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl);
        let (r, _) = ex
            .execute_natural(
                ("tap buy", None),
                NaturalKind::Act,
                (None, None),
                None,
                "d",
                "app",
            )
            .await;
        let e = r.unwrap_err();
        assert!(
            matches!(e, VelocityError::ServiceUnavailable { .. }) && e.is_transient(),
            "{e:?}"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let mut ex = TestExecutor::new(&driver, config(300), "app")
            .with_planner(resolver("until-typed", calls));
        let (r, _) = ex
            .execute_natural(
                ("tap buy", None),
                NaturalKind::Act,
                (None, None),
                Some("the receipt is shown"),
                "d",
                "app",
            )
            .await;
        let e = r.unwrap_err();
        assert!(matches!(e, VelocityError::AssertionFailed { .. }), "{e:?}");
        assert!(e.to_string().contains("receipt"), "{e}");
    }

    #[test]
    fn a_natural_step_that_finds_nothing_stays_not_found() {
        let act = Action::Tap {
            selector: Selector::Text("Pay".into()),
        };
        let e = annotate(
            &act,
            VelocityError::ElementNotFound {
                selector: "text=\"Pay\"".into(),
                timeout_ms: 300,
                screenshot: None,
                hierarchy_snapshot: None,
            },
            Some("re-plan failed: x"),
        );
        assert!(is_not_found(&e), "{e:?}");
        assert!(e.to_string().contains("natural step resolved to"), "{e}");
        assert!(e.to_string().contains("re-plan failed: x"), "{e}");
    }

    #[test]
    fn a_model_outage_during_a_re_plan_stays_an_outage() {
        // Review M5: a cached step's element vanished, the re-plan hit a
        // 529, and the step ended as ElementNotFound without the 529.
        let act = Action::Tap {
            selector: Selector::Text("Pay".into()),
        };
        let gone = || VelocityError::ElementNotFound {
            selector: "text=\"Pay\"".into(),
            timeout_ms: 300,
            screenshot: None,
            hierarchy_snapshot: None,
        };
        let outage = VelocityError::ServiceUnavailable {
            service: "model API".into(),
            detail: "HTTP 529 overloaded".into(),
        };
        let e = replan_failed(&act, gone(), outage);
        assert!(
            matches!(e, VelocityError::ServiceUnavailable { .. }),
            "{e:?}"
        );
        let msg = e.to_string();
        assert!(
            msg.contains("529") && msg.contains("re-plan failed"),
            "{msg}"
        );
        assert_ne!(e.exit_code(), 1);
        // A plan that simply didn't work: the first failure, with the note.
        let e = replan_failed(&act, gone(), VelocityError::Config("no plan".into()));
        assert!(is_not_found(&e), "{e:?}");
        assert!(e.to_string().contains("re-plan failed: "), "{e}");
    }

    #[tokio::test]
    async fn a_retry_attempt_re_plans_instead_of_replaying() {
        let calls = Arc::new(AtomicUsize::new(0));
        let nl = resolver("retry", calls.clone());
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let test = test_case(vec![natural("tap buy")]);
        let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl.clone());
        ex.execute_test(&test, "d", "app").await.unwrap();
        assert!(nl.cached(&main_step("tap buy", "t#0")).await.is_some());
        let planned = calls.load(Ordering::SeqCst);
        ex.set_retry_attempt(1);
        ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            planned + 1,
            "the retry asked the model"
        );
    }

    #[tokio::test]
    async fn batching_skips_a_plain_relaunch_after_a_passing_test() {
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let mut cfg = config(500);
        cfg.batch = true;
        let launch = || {
            step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
            })
        };
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let first = ex
            .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
            .await
            .unwrap();
        assert!(!first.steps[0].action_name.contains("batched"));
        let second = ex
            .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
            .await
            .unwrap();
        assert!(
            second.steps[0].action_name.contains("batched"),
            "{:?}",
            second.steps[0]
        );
        // A clearState launch always runs.
        let third = ex
            .execute_test(
                &test_case(vec![step(Action::LaunchApp {
                    app_id: String::new(),
                    clear_state: true,
                })]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert!(!third.steps[0].action_name.contains("batched"));
    }

    fn not_visible(text: &str) -> Action {
        Action::AssertNotVisible {
            selector: Selector::Text(text.into()),
        }
    }

    #[tokio::test]
    async fn assert_not_visible_fails_when_the_screen_cannot_be_read() {
        // VEL-012: every read fails while the spinner is on screen.
        let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
        driver.unreadable.store(usize::MAX, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(not_visible("Loading"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap();
        assert!(msg.contains("null root node"), "{msg}");
    }

    #[tokio::test]
    async fn assert_not_visible_reads_again_after_a_failed_read() {
        // One failed read is not "gone"; the next good read decides.
        let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
        driver.unreadable.store(1, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(400), "app");
        let r = ex
            .execute_test(&test_case(vec![step(not_visible("Loading"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed, "still on screen");
        assert!(r.error_message.unwrap().contains("not visible"));

        let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
        driver.unreadable.store(1, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(400), "app");
        let r = ex
            .execute_test(&test_case(vec![step(not_visible("Gone"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_crash_does_not_pass_a_negative_assertion_or_the_last_step() {
        // VEL-014: with no settle the post-action crash watch never runs.
        for steps in [
            vec![step(tap("Pay")), step(not_visible("Payment failed"))],
            vec![step(tap("Pay"))],
        ] {
            let driver = CrashingTap(ScriptDriver::new(vec![node("p", "Pay", 0)]), Duration::ZERO);
            let mut ex = TestExecutor::new(&driver, config(300), "app");
            let r = ex
                .execute_test(&test_case(steps), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Failed);
            let msg = r.error_message.unwrap();
            assert!(msg.contains("crashed") && msg.contains("boom"), "{msg}");
            assert_eq!(r.steps.last().unwrap().status, StepStatus::Failed);
        }
        // A test that stops the app on purpose is not a crash.
        let driver = CrashingTap(ScriptDriver::new(vec![node("p", "Pay", 0)]), Duration::ZERO);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(Action::StopApp {
                        app_id: String::new(),
                    }),
                    step(not_visible("Payment failed")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    fn visible(text: &str) -> Action {
        Action::AssertVisible {
            selector: Selector::Text(text.into()),
        }
    }

    /// I3 (D-50 sticky crash): a crash the driver saw on its own (a
    /// physical iPhone's crash watcher) fails every later step at once with
    /// the crash: no tap lands on the home screen, no element wait runs to
    /// its timeout.
    #[tokio::test]
    async fn a_crash_the_driver_saw_fails_every_later_step_at_once() {
        let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
        *driver.pending.lock().unwrap() = Some("SIGABRT in PaymentSheet".into());
        let mut ex = TestExecutor::new(&driver, config(10_000), "app");
        let start = Instant::now();
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(tap("Pay")),
                    step(visible("Receipt")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.clone().unwrap();
        assert!(msg.contains("crashed") && msg.contains("SIGABRT"), "{msg}");
        assert_eq!(r.steps[1].status, StepStatus::Failed, "{:?}", r.steps);
        assert_eq!(r.steps[2].status, StepStatus::Skipped);
        assert_eq!(
            driver.taps.lock().unwrap().len(),
            1,
            "nothing tapped after the crash"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );

        // The crash shows up while a wait runs: the wait ends at once.
        let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
        *driver.pending.lock().unwrap() = Some("EXC_BAD_ACCESS".into());
        driver.pending_after.store(3, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(10_000), "app");
        let start = Instant::now();
        let r = ex
            .execute_test(
                &test_case(vec![step(tap("Pay")), step(visible("Receipt"))]),
                "d",
                "app",
            )
            .await
            .unwrap();
        let msg = r.error_message.clone().unwrap();
        assert!(
            msg.contains("crashed") && msg.contains("EXC_BAD_ACCESS"),
            "{msg}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    /// I3: a relaunch clears the sticky crash, and so does stopping the
    /// app on purpose; launchApp itself is never refused.
    #[tokio::test]
    async fn a_relaunch_or_a_stop_ends_the_sticky_crash() {
        let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
        *driver.pending.lock().unwrap() = Some("boom".into());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(Action::LaunchApp {
                        app_id: String::new(),
                        clear_state: false,
                    }),
                    step(tap("Pay")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().len(), 2);

        let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
        *driver.pending.lock().unwrap() = Some("boom".into());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(Action::StopApp {
                        app_id: String::new(),
                    }),
                    step(not_visible("Pay sheet")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    /// iOS-like: save/restore terminate the app without clearing the
    /// driver's launch record, so `crash_report` sees a launched app with
    /// no process and calls it a crash (unless the executor knows better).
    struct StateKeeping(ScriptDriver, std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl PlatformDriver for StateKeeping {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            self.0.list_devices().await
        }
        async fn boot_device(&self, d: &str) -> Result<()> {
            self.0.boot_device(d).await
        }
        async fn shutdown_device(&self, d: &str) -> Result<()> {
            self.0.shutdown_device(d).await
        }
        async fn install_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.install_app(d, a).await
        }
        async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
            self.1.store(true, Ordering::SeqCst);
            self.0.launch_app(d, a, c).await
        }
        async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
            self.0.stop_app(d, a).await
        }
        async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
            self.0.find_element(d, s).await
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            self.0.find_elements(d, s).await
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            self.0.get_hierarchy(d).await
        }
        async fn tap(&self, d: &str, e: &Element) -> Result<()> {
            self.0.tap(d, e).await
        }
        async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
            self.0.double_tap(d, e).await
        }
        async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
            self.0.long_press(d, e, ms).await
        }
        async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
            self.0.input_text(d, e, t).await
        }
        async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
            self.0.clear_text(d, e).await
        }
        async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
            self.0.swipe(d, dir).await
        }
        async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
            self.0.swipe_coords(d, f, t).await
        }
        async fn press_key(&self, d: &str, k: Key) -> Result<()> {
            self.0.press_key(d, k).await
        }
        async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
            self.0.screenshot(d).await
        }
        async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
            self.0.screen_size(d).await
        }
        async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
            self.0.get_element_text(d, e).await
        }
        async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
            self.0.is_element_visible(d, e).await
        }
        async fn save_app_state(&self, _: &str, _: &str, _: &str) -> Result<()> {
            self.1.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn restore_app_state(&self, _: &str, _: &str, _: &str) -> Result<()> {
            self.1.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn crash_report(&self, _: &str, _: &str) -> Option<String> {
            (!self.1.load(Ordering::SeqCst))
                .then(|| "the app's process exited unexpectedly".to_string())
        }
        async fn pending_crash(&self, d: &str, a: &str) -> Option<String> {
            self.0.pending_crash(d, a).await
        }
    }

    /// fi56 (review MINOR): after a crash, `restoreState` (which touches
    /// no UI and stops the app on purpose) failed at once with the crash,
    /// so a restore-and-relaunch recovery couldn't run.
    #[tokio::test]
    async fn restore_state_runs_after_a_crash() {
        let driver = StateKeeping(
            ScriptDriver::new(vec![node("p", "Pay", 0)]),
            std::sync::atomic::AtomicBool::new(true),
        );
        *driver.0.pending.lock().unwrap() = Some("boom".into());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(Action::RestoreState {
                        name: "loggedIn".into(),
                    }),
                    step(Action::LaunchApp {
                        app_id: String::new(),
                        clear_state: false,
                    }),
                    step(tap("Pay")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.0.taps.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn save_and_restore_state_are_not_a_crash() {
        // B-1: saveState/restoreState leave the app stopped on purpose.
        let restore = Action::RestoreState {
            name: "loggedIn".into(),
        };
        for steps in [
            vec![
                step(tap("Pay")),
                step(Action::SaveState {
                    name: "loggedIn".into(),
                }),
            ],
            vec![step(restore.clone()), step(not_visible("Payment failed"))],
            vec![step(restore)],
        ] {
            let driver = StateKeeping(
                ScriptDriver::new(vec![node("p", "Pay", 0)]),
                std::sync::atomic::AtomicBool::new(true),
            );
            let mut ex = TestExecutor::new(&driver, config(300), "app");
            let r = ex
                .execute_test(&test_case(steps), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        }
    }

    #[tokio::test]
    async fn a_tap_after_an_assertion_uses_the_moved_bounds() {
        // VEL-340: assertVisible cached Coffee; a promo row then pushed it down.
        let driver = ScriptDriver::new(vec![node("coffee", "Coffee", 0)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let seen = Action::AssertVisible {
            selector: Selector::Text("Coffee".into()),
        };
        let r = ex.execute_step(&step(seen), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed);
        driver.screen.lock().unwrap().children =
            vec![node("promo", "Promo", 0), node("coffee", "Coffee", 50)];
        let r = ex
            .execute_step(&step(tap("Coffee")), "d", "app", 1)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Passed);
        assert_eq!(
            *driver.tap_ys.lock().unwrap(),
            [50],
            "tapped where Coffee is now"
        );
    }

    #[tokio::test]
    async fn a_system_dialog_the_step_answers_is_left_to_the_step() {
        // VEL-011: `tap: Don't Allow` on a permission prompt; the handler
        // used to tap "Allow" first.
        let mut alert = node("", "", 300);
        alert.text = None;
        alert.label = Some("\u{201c}Shop\u{201d} Would Like to Use Your Location".into());
        alert.element_type = "XCUIElementTypeAlert".into();
        alert.bounds.height = 200;
        alert.children = vec![
            node("deny", "Don't Allow", 320),
            node("allow", "Allow", 380),
        ];
        let driver = ScriptDriver::new(vec![alert]);
        let mut cfg = config(300);
        cfg.dialog = Default::default();
        cfg.dialog.max_dismissals = 1;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Don't Allow"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        // (The fake keeps the alert on screen, so the post-action pass may
        // answer it again; what matters is the step's own tap came first.)
        assert_eq!(driver.taps.lock().unwrap()[0], "Don't Allow");
    }

    #[test]
    fn only_device_screens_get_a_status_bar_mask() {
        // VEL-013: the top 4% of a web page or headless render was never compared.
        let mut c = config(100);
        assert!(has_status_bar(&c));
        c.platform = Some(Platform::Ios);
        assert!(has_status_bar(&c));
        c.driver = Some(velocity_common::DriverMode::Headless);
        assert!(!has_status_bar(&c));
        c.driver = None;
        c.platform = Some(Platform::Web);
        assert!(!has_status_bar(&c));
    }

    #[tokio::test]
    async fn scroll_until_visible_looks_after_its_last_scroll() {
        // VEL-056: the row the only allowed scroll revealed was never seen.
        let driver =
            ScriptDriver::new(vec![]).after(2, |s| s.children.push(node("row", "Row 9", 0)));
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Row 9".into()),
            direction: Direction::Down,
            max_scrolls: 1,
            within: None,
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn scroll_until_visible_stops_aiming_when_nothing_moves() {
        // VEL-068: a visible row in the top 12% band of a screen that
        // can't scroll was dragged at on every one of the scrolls.
        let driver = ScriptDriver::new(vec![node("top", "Header", 20)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Header".into()),
            direction: Direction::Down,
            max_scrolls: 5,
            within: None,
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.strokes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scroll_until_visible_brings_a_container_below_the_fold_in() {
        // VEL-069: a `within:` list at y=3000 was dragged at off-screen
        // coordinates and never moved.
        let mut list = node("inner2", "", 3000);
        list.text = None;
        list.bounds.width = 1000;
        list.bounds.height = 600;
        list.children = vec![node("low", "Low 45", 3900)];
        let mut driver = ScriptDriver::new(vec![list]);
        driver.pans = true;
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Low 45".into()),
            direction: Direction::Down,
            max_scrolls: 10,
            within: Some(Selector::Id("inner2".into())),
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        for (a, b) in driver.strokes.lock().unwrap().iter() {
            for (_, y) in [a, b] {
                assert!((0..=1920).contains(y), "stroke off screen: {a:?} -> {b:?}");
            }
        }
    }

    /// VEL-S53: a direction-only swipe is the executor's stroke on every
    /// device driver (here 1080x1920: centred, 30%-70% of the height), and
    /// the driver's own page scroll on the web.
    #[tokio::test]
    async fn a_direction_swipe_is_the_same_stroke_on_every_device_driver() {
        let driver = ScriptDriver::new(vec![]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        for direction in [Some(Direction::Up), None] {
            let swipe = Action::Swipe {
                direction,
                from: None,
                to: None,
                within: None,
            };
            let r = ex.execute_step(&step(swipe), "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Passed, "{r:?}");
        }
        assert_eq!(
            *driver.strokes.lock().unwrap(),
            vec![((540, 1344), (540, 576)), ((540, 576), (540, 1344))]
        );
        let web = ScriptDriver::new(vec![]);
        let mut cfg = config(300);
        cfg.platform = Some(Platform::Web);
        let mut ex = TestExecutor::new(&web, cfg, "app");
        let swipe = Action::Swipe {
            direction: Some(Direction::Up),
            from: None,
            to: None,
            within: None,
        };
        ex.execute_step(&step(swipe), "d", "app", 0).await.unwrap();
        assert!(web.strokes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_container_is_revealed_before_its_target_is_aimed_at() {
        // Review T-1: removing `reveal` from `scroll_area` broke no test.
        // M-1: the target was read before the reveal scrolled the page, so
        // the first stroke aimed at its stale, pre-scroll bounds.
        let mut list = node("inner4", "", 3000);
        list.text = None;
        list.bounds.width = 1000;
        list.bounds.height = 600;
        list.children = vec![node("row", "Row 3", 3100)];
        let mut driver = ScriptDriver::new(vec![list]);
        driver.reveals = true;
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Row 3".into()),
            direction: Direction::Down,
            max_scrolls: 3,
            within: Some(Selector::Id("inner4".into())),
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert!(driver.revealed.load(Ordering::SeqCst) > 0, "never revealed");
        let strokes = driver.strokes.lock().unwrap();
        assert!(strokes.is_empty(), "aimed at stale bounds: {strokes:?}");
    }

    #[tokio::test]
    async fn a_container_that_wont_come_in_gets_one_drag_and_no_blind_strokes() {
        // Review m5: up to 5 drags on every call (two per scroll), then a
        // stroke at the raw off-screen bounds.
        let mut list = node("inner3", "", 3000);
        list.text = None;
        list.bounds.width = 1000;
        list.bounds.height = 600;
        list.children = vec![node("low", "Low 46", 3100)];
        let driver = ScriptDriver::new(vec![list]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Low 46".into()),
            direction: Direction::Down,
            max_scrolls: 3,
            within: Some(Selector::Id("inner3".into())),
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let strokes = driver.strokes.lock().unwrap();
        assert_eq!(strokes.len(), 1, "{strokes:?}");
        for (a, b) in strokes.iter() {
            for (_, y) in [a, b] {
                assert!((0..=1920).contains(y), "stroke off screen: {a:?} -> {b:?}");
            }
        }
    }

    #[test]
    fn only_the_shown_part_of_a_container_is_stroked() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 2000,
        };
        let r = |y, h| Rect {
            x: 0,
            y,
            width: 1000,
            height: h,
        };
        assert_eq!(shown_part(&screen, &r(1800, 1000)), None);
        assert_eq!(shown_part(&screen, &r(1000, 2000)), Some(r(1000, 1000)));
        assert_eq!(shown_part(&screen, &r(1980, 10)), Some(r(1980, 10)));
        assert_eq!(shown_part(&screen, &r(3000, 600)), None);
    }

    #[tokio::test]
    async fn a_lost_device_is_infrastructure_in_assert_text_and_scrolling() {
        // VEL-073: reported as a wrong text / not found, infra=false. The
        // device goes away after a set number of lookups, not on a timer
        // (C-m6: a 30 ms flip raced the 300 ms deadline on a busy runner).
        for (action, good_reads) in [
            (
                Action::AssertText {
                    selector: Selector::Text("Total".into()),
                    expected: "$10".into(),
                },
                2,
            ),
            (
                Action::ScrollUntilVisible {
                    selector: Selector::Text("Row 9".into()),
                    direction: Direction::Down,
                    max_scrolls: 3,
                    within: None,
                },
                0,
            ),
        ] {
            let driver = ScriptDriver::new(vec![node("t", "Total", 0)]);
            driver.lose_after.store(good_reads, Ordering::SeqCst);
            let mut ex = TestExecutor::new(&driver, config(5_000), "app");
            let r = ex
                .execute_test(&test_case(vec![step(action)]), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Failed);
            assert!(r.infrastructure_failure, "{:?}", r.error_message);
        }
    }

    #[tokio::test]
    async fn batching_relaunches_after_a_mid_test_stop() {
        // VEL-071: only a stop as the very last step counted.
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let mut cfg = config(500);
        cfg.batch = true;
        let launch = || {
            step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
            })
        };
        let stop = step(Action::StopApp {
            app_id: String::new(),
        });
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let first = ex
            .execute_test(
                &test_case(vec![launch(), stop, step(Action::Wait { ms: 1 })]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(first.status, TestStatus::Passed);
        let second = ex
            .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
            .await
            .unwrap();
        assert!(!second.steps[0].action_name.contains("batched"));
    }

    #[tokio::test]
    async fn a_disabled_target_that_vanishes_is_not_tapped() {
        // VEL-072: the stale element was tapped at its old bounds.
        let mut submit = node("submit", "Submit", 0);
        submit.enabled = false;
        let driver = ScriptDriver::new(vec![submit]).after(2, |s| s.children.clear());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_step(&step(tap("Submit")), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        assert!(driver.taps.lock().unwrap().is_empty());
    }

    fn with_liveness(driver: ScriptDriver, l: velocity_common::Liveness) -> ScriptDriver {
        *driver.liveness.lock().unwrap() = Some(l);
        driver
    }

    #[tokio::test]
    async fn the_drivers_liveness_decides_a_negative_check_and_the_test() {
        // P2: a driver that knows the app died (its front-app check) though
        // the crash log is silent (ANR, OOM kill, force-stop).
        let dead = || velocity_common::Liveness::Dead("killed by the low-memory killer".into());
        for steps in [
            vec![step(tap("Pay")), step(not_visible("Payment failed"))],
            vec![step(tap("Pay"))],
        ] {
            let driver = with_liveness(ScriptDriver::new(vec![node("p", "Pay", 0)]), dead());
            let mut ex = TestExecutor::new(&driver, config(300), "app");
            let r = ex
                .execute_test(&test_case(steps), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Failed);
            let msg = r.error_message.unwrap();
            assert!(msg.contains("low-memory killer"), "{msg}");
        }
    }

    #[tokio::test]
    async fn negative_checks_read_liveness_once_between_actions() {
        // C-m4: one liveness read per assertNotVisible pass (a `simctl`
        // call on iOS) cost tens of seconds over a suite.
        let driver = with_liveness(
            ScriptDriver::new(vec![node("p", "Pay", 0)]),
            velocity_common::Liveness::Foreground,
        );
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(
                &test_case(vec![
                    step(tap("Pay")),
                    step(not_visible("Error")),
                    step(not_visible("Retry")),
                    step(not_visible("Cancel")),
                ]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(
            driver.liveness_reads.load(Ordering::SeqCst),
            2,
            "one for the three checks after the tap, one at the end"
        );
    }

    #[tokio::test]
    async fn another_app_in_front_is_noted_and_never_batched() {
        let driver = with_liveness(
            ScriptDriver::new(vec![node("p", "Pay", 0)]),
            velocity_common::Liveness::Background(Some("com.android.launcher3".into())),
        );
        let mut cfg = config(300);
        cfg.batch = true;
        let launch = || {
            step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
            })
        };
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let mut off_app = step(not_visible("Payment failed"));
        off_app.modifiers.app_in_front = Some(false);
        let first = ex
            .execute_test(&test_case(vec![launch(), off_app]), "d", "app")
            .await
            .unwrap();
        // `appInFront: false` (D-2): passes, and it is said.
        assert_eq!(first.status, TestStatus::Passed);
        assert!(
            first
                .warnings
                .iter()
                .any(|w| w.contains("launcher3") && w.contains("wasn't in front")),
            "{:?}",
            first.warnings
        );
        let second = ex
            .execute_test(&test_case(vec![launch()]), "d", "app")
            .await
            .unwrap();
        assert!(
            !second.steps[0].action_name.contains("batched"),
            "the launcher was in front: launch the app"
        );
    }

    #[tokio::test]
    async fn a_negative_check_off_app_fails_unless_the_test_left_the_app() {
        // D-2 (VEL-014): the launcher lacks every element of the app, so
        // `assertNotVisible` there proved nothing and passed.
        let driver = with_liveness(
            ScriptDriver::new(vec![node("p", "Pay", 0)]),
            velocity_common::Liveness::Background(Some("com.android.launcher3".into())),
        );
        driver.navigates.store(true, Ordering::SeqCst);
        let launch = |app: &str| {
            step(Action::LaunchApp {
                app_id: app.into(),
                clear_state: false,
            })
        };
        let run = |steps: Vec<Step>| {
            let driver = &driver;
            async move {
                let mut ex = TestExecutor::new(driver, config(300), "app");
                ex.execute_test(&test_case(steps), "d", "app")
                    .await
                    .unwrap()
            }
        };
        let r = run(vec![launch(""), step(not_visible("Error"))]).await;
        assert_eq!(r.status, TestStatus::Failed);
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("launcher3"), "{err}");
        assert!(err.contains("appInFront: false"), "{err}");
        // Left on purpose: the check runs on that screen, with a note.
        let home = step(Action::PressKey {
            key: velocity_common::Key::Home,
        });
        let link = step(Action::Goto {
            url: "app://promo".into(),
        });
        for (left, noted) in [(home, true), (link, true), (launch("com.other"), false)] {
            let what = action_name(&left.action);
            let r = run(vec![launch("app"), left, step(not_visible("Error"))]).await;
            assert_eq!(
                r.status,
                TestStatus::Passed,
                "{what}: {:?}",
                r.error_message
            );
            // Another app launched by the test is not "the app" at all:
            // its liveness isn't read, so there is nothing to note.
            assert_eq!(
                r.warnings.iter().any(|w| w.contains("wasn't in front")),
                noted,
                "{what}: {:?}",
                r.warnings
            );
        }
        // Until the suite's app is launched again.
        let r = run(vec![
            launch("com.other"),
            launch("app"),
            step(not_visible("Error")),
        ])
        .await;
        assert_eq!(r.status, TestStatus::Failed);
    }

    #[tokio::test]
    async fn an_optional_step_that_fails_is_a_warning_and_the_test_goes_on() {
        // D-12: Maestro's `optional: true` (and `assertWithAI`).
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        let mut ex = TestExecutor::new(&driver, config(200), "app");
        let mut banner = step(Action::AssertVisible {
            selector: Selector::Text("Promo banner".into()),
        });
        banner.modifiers.optional = true;
        let r = ex
            .execute_test(&test_case(vec![banner, step(tap("Pay"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(r.steps[0].status, StepStatus::Passed);
        assert!(r.steps[0].action_name.contains("optional, failed"));
        assert_eq!(driver.taps.lock().unwrap().len(), 1, "the test went on");
        assert!(
            r.warnings
                .iter()
                .any(|w| w.starts_with("optional step") && w.contains("Promo banner")),
            "{:?}",
            r.warnings
        );
        // Without it, the same step fails the test.
        let r = ex
            .execute_test(
                &test_case(vec![step(Action::AssertVisible {
                    selector: Selector::Text("Promo banner".into()),
                })]),
                "d",
                "app",
            )
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
    }

    #[tokio::test]
    async fn an_optional_flow_stops_at_its_first_failure_and_the_test_goes_on() {
        // Review M2: Maestro's `runFlow: { file: onboarding.yaml, optional:
        // true }`. With no onboarding on screen, "Skip" fails; the flow's
        // "Continue" must not then run against the real screen.
        let mut run = step(Action::RunFlow {
            flow_id: "onboarding".into(),
        });
        run.modifiers.optional = true;
        let suite = velocity_common::TestSuite {
            app_id: "app".into(),
            config: SuiteConfig::default(),
            flows: vec![velocity_common::Flow {
                id: "onboarding".into(),
                steps: vec![step(tap("Skip")), step(tap("Continue"))],
            }],
            tests: vec![test_case(vec![run, step(tap("Pay"))])],
        };
        let test = crate::resolve_flows(&suite).unwrap().remove(0);
        let driver = ScriptDriver::new(vec![node("c", "Continue", 0), node("p", "Pay", 300)]);
        let mut ex = TestExecutor::new(&driver, config(200), "app");
        let r = ex.execute_test(&test, "d", "app").await.unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.steps);
        assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Pay"]);
        assert_eq!(r.steps[1].status, StepStatus::Skipped);
        let notes: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.starts_with("optional"))
            .collect();
        assert_eq!(notes.len(), 1, "{:?}", r.warnings);
        assert!(notes[0].contains("onboarding"), "{notes:?}");
    }

    #[tokio::test]
    async fn an_optional_step_does_not_forgive_the_watchdog() {
        // Review m3: a step cut off after hanging means a wedged device;
        // carrying on paid every later step's watchdog too.
        let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
        let mut ex = TestExecutor::new(&driver, config(50), "app");
        let mut hang = step(tap("Buy"));
        hang.modifiers.optional = true;
        let seen = step(Action::AssertVisible {
            selector: Selector::Text("Buy".into()),
        });
        let r = ex
            .execute_test(&test_case(vec![seen, hang]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            r.error_message.unwrap_or_default().contains("Step timeout"),
            "{:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn an_optional_step_does_not_forgive_a_lost_device() {
        let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
        driver.lost.store(true, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(200), "app");
        let mut t = step(tap("Pay"));
        t.modifiers.optional = true;
        let r = ex
            .execute_test(&test_case(vec![t]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(r.infrastructure_failure);
    }

    #[tokio::test]
    async fn an_isolated_test_is_never_batched() {
        // VEL-212: `isolated: true` was parsed and ignored.
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let mut cfg = config(500);
        cfg.batch = true;
        let launch = || {
            step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
            })
        };
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        ex.execute_test(&test_case(vec![launch()]), "d", "app")
            .await
            .unwrap();
        let mut isolated = test_case(vec![launch()]);
        isolated.isolated = true;
        let r = ex.execute_test(&isolated, "d", "app").await.unwrap();
        assert!(
            !r.steps[0].action_name.contains("batched"),
            "{:?}",
            r.steps[0]
        );
    }

    fn regions(masks: &[crate::visual::MaskPreset]) -> Vec<(u32, u32, u32, u32)> {
        masks
            .iter()
            .map(|m| {
                let r = m.to_region(1080, 1920);
                (r.x, r.y, r.width, r.height)
            })
            .collect()
    }

    #[tokio::test]
    async fn screenshots_mask_the_bars_the_driver_reports() {
        // P8: Android's status and navigation bars, not 4% of the height.
        let mut driver = ScriptDriver::new(vec![node("clock", "12:00", 1800)]);
        let bar = |y, height| Rect {
            x: 0,
            y,
            width: 1080,
            height,
        };
        driver.bars = Some(vec![bar(0, 63), bar(1794, 126)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let masks = ex.screenshot_masks("d", &[], &[]).await.unwrap();
        assert_eq!(regions(&masks), [(0, 0, 1080, 63), (0, 1794, 1080, 126)]);
        // A driver that can't tell keeps the old guess.
        driver.bars = None;
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let masks = ex.screenshot_masks("d", &[], &[]).await.unwrap();
        assert_eq!(regions(&masks), [(0, 0, 1080, 76)]);
        // VEL-063: an element partly below the screen is masked only where
        // it is on screen.
        driver.bars = Some(vec![]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let masks = ex
            .screenshot_masks("d", &[], &[Selector::Text("12:00".into())])
            .await
            .unwrap();
        // 1800 + 40 is on screen (1920): all of it; move it lower.
        assert_eq!(regions(&masks), [(0, 1800, 100, 40)]);
        driver.screen.lock().unwrap().children[0].bounds.y = 1900;
        let masks = ex
            .screenshot_masks("d", &[], &[Selector::Text("12:00".into())])
            .await
            .unwrap();
        assert_eq!(regions(&masks), [(0, 1900, 100, 20)]);
    }

    /// S3: while `selectors.strict` is unset, an ambiguous action's warning
    /// says it becomes an error in 1.0 and how to keep today's behaviour;
    /// with it set to false it doesn't, and nothing fails either way.
    #[tokio::test]
    async fn an_ambiguous_action_warns_of_the_1_0_default_while_strict_is_unset() {
        let driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        let w = r
            .warnings
            .iter()
            .find(|w| w.contains("matched 2 elements"))
            .unwrap_or_else(|| panic!("{:?}", r.warnings));
        if velocity_common::env::var("VELOCITY_SELECTORS_STRICT").is_err() {
            assert!(w.contains(STRICT_STAGING), "{w}");
            assert!(w.contains("selectors.strict: false"), "{w}");
        }
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(false);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        let w = r
            .warnings
            .iter()
            .find(|w| w.contains("matched 2 elements"))
            .unwrap_or_else(|| panic!("{:?}", r.warnings));
        assert!(!w.contains("1.0"), "{w}");
    }

    #[tokio::test]
    async fn ambiguity_is_counted_on_every_driver_with_ready_made_picks() {
        // D-6 / R5: a driver without a tree cache warned nothing; now every
        // driver counts what its own lookup finds, and the warning names a
        // selector picking each match.
        let row = |id: &str, y: i32| Element {
            children: vec![node("", "Add", y + 10)],
            ..node(id, "", y)
        };
        let rows = || vec![row("com.app:id/row_a", 0), row("com.app:id/row_b", 100)];
        let driver = ScriptDriver::new(rows());
        assert!(!driver.has_hierarchy_cache());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        let w = r
            .warnings
            .iter()
            .find(|w| w.contains("matched 2 elements"))
            .unwrap_or_else(|| panic!("{:?}", r.warnings));
        assert!(
            w.contains(r#"{ text: "Add", childOf: { id: "row_a" } }"#),
            "{w}"
        );
        assert!(
            w.contains(r#"{ text: "Add", childOf: { id: "row_b" } }"#),
            "{w}"
        );
        // Strict mode fails and names the same picks.
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(
            r.error_message.as_deref().unwrap_or("").contains("childOf"),
            "{:?}",
            r.error_message
        );
        // On the web the page's own matches count, hidden ones not (one
        // inside a closed <details>): no warning.
        let mut hidden = node("b", "Save", 100);
        hidden.visible = false;
        let driver = ScriptDriver::new(vec![node("a", "Save", 0), hidden]);
        let mut web = config(300);
        web.platform = Some(Platform::Web);
        let mut ex = TestExecutor::new(&driver, web.clone(), "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Save"))]), "d", "app")
            .await
            .unwrap();
        assert!(
            !r.warnings.iter().any(|w| w.contains("matched")),
            "{:?}",
            r.warnings
        );
        let driver = ScriptDriver::new(vec![node("a", "Save", 0), node("b", "Save", 100)]);
        let mut ex = TestExecutor::new(&driver, web, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Save"))]), "d", "app")
            .await
            .unwrap();
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("matched 2") && w.contains(r#"{ text: "Save", index: 1 }"#)),
            "{:?}",
            r.warnings
        );
    }

    #[tokio::test]
    async fn the_ambiguity_check_adds_no_lookup_to_a_clear_tap() {
        // Review MAJOR 8: every action on web, Flutter, RN and headless
        // paid a second full lookup (`find_elements`) just to count. The
        // count now comes from the lookup that resolved.
        async fn tap_once(
            driver: &ScriptDriver,
            platform: Option<Platform>,
            strict: bool,
        ) -> TestResult {
            let mut cfg = config(300);
            cfg.platform = platform;
            let mut ex = TestExecutor::new(driver, cfg, "app").with_strict_selectors(strict);
            ex.execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
                .await
                .unwrap()
        }
        for platform in [None, Some(Platform::Web)] {
            let driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Other", 100)]);
            let r = tap_once(&driver, platform, false).await;
            assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
            assert_eq!(
                driver.find_alls.load(Ordering::SeqCst),
                0,
                "{platform:?}: lookups per tap beyond the resolve"
            );
            // Ambiguous: the listing (and only then) looks again.
            let driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Buy", 100)]);
            let r = tap_once(&driver, platform, false).await;
            assert!(
                r.warnings.iter().any(|w| w.contains("matched 2")),
                "{:?}",
                r.warnings
            );
        }
        // A driver that doesn't count: no extra lookup unless strict.
        let mut driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Buy", 100)]);
        driver.counts = false;
        tap_once(&driver, None, false).await;
        assert_eq!(driver.find_alls.load(Ordering::SeqCst), 0);
        let r = tap_once(&driver, None, true).await;
        assert_eq!(r.status, TestStatus::Failed, "strict still counts");
        assert_eq!(driver.find_alls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ci_is_detected_beyond_the_ci_variable() {
        // VEL-214: Jenkins, Azure Pipelines and CodeBuild don't set CI.
        for var in ["JENKINS_URL", "TF_BUILD", "CODEBUILD_BUILD_ID", "CI"] {
            assert!(in_ci(|k| k == var), "{var}");
        }
        assert!(!in_ci(|_| false));
    }

    #[test]
    fn ci_false_or_zero_is_not_ci() {
        use std::ffi::OsStr;
        for v in ["false", "FALSE", "0", "", " "] {
            assert!(!ci_flag(OsStr::new(v)), "{v:?}");
        }
        for v in ["true", "1", "yes", "github"] {
            assert!(ci_flag(OsStr::new(v)), "{v:?}");
        }
    }

    /// review-fixes #6: a count left from an earlier screen (more matches
    /// than the page holds now) never fails a strict tap as ambiguous.
    #[tokio::test]
    async fn a_stale_match_count_never_makes_a_lookup_ambiguous() {
        let mut driver = ScriptDriver::new(vec![node("a", "Add", 0)]);
        driver.counts = false;
        driver
            .match_counts
            .record("d", &Selector::Text("Add".into()), 2);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.taps.lock().unwrap().len(), 1);
    }

    /// review-fixes #13: the fake records counts under the device it was
    /// asked about (it used `"d"` whatever the device).
    #[tokio::test]
    async fn the_script_driver_counts_per_device() {
        let driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
        let add = Selector::Text("Add".into());
        driver.find_element("phone-2", &add).await.unwrap();
        assert_eq!(driver.last_match_count("phone-2", &add), Some(2));
        assert_eq!(driver.last_match_count("d", &add), None);
        assert!(driver
            .find_element("phone-2", &Selector::Text("Gone".into()))
            .await
            .is_err());
        assert_eq!(
            driver.last_match_count("phone-2", &add),
            None,
            "a miss clears"
        );
    }

    #[tokio::test]
    async fn strict_selectors_fail_an_ambiguous_tap_on_any_driver() {
        // P11, opt-in: two "Add" buttons; the driver keeps no tree cache
        // (web, headless), where not even the warning was given (VEL-220).
        let two = || ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
        let driver = two();
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap();
        assert!(msg.contains("2 match") && msg.contains("(0, 100)"), "{msg}");
        assert!(driver.taps.lock().unwrap().is_empty(), "nothing tapped");
        // Off (the default): the first is tapped, as before.
        let driver = two();
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(false);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        // One match passes in strict mode too.
        let driver = ScriptDriver::new(vec![node("a", "Add", 0)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
        let r = ex
            .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn web_suites_get_the_tree_settle_they_ask_for() {
        // VEL-067: `sync.mode: polling` was dropped on the web.
        for (mode, settles) in [(SyncMode::Polling, true), (SyncMode::Auto, false)] {
            let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
            let mut cfg = config(2000);
            cfg.platform = Some(Platform::Web);
            cfg.sync.mode = mode;
            cfg.sync.interval_ms = 10;
            let mut ex = TestExecutor::new(&driver, cfg, "app");
            let r = ex
                .execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
                .await
                .unwrap();
            assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
            assert_eq!(driver.dumps.load(Ordering::SeqCst) > 0, settles, "{mode:?}");
        }
    }

    #[tokio::test]
    async fn a_hung_device_cannot_hold_the_run_in_failure_artifacts() {
        // VEL-351: the failure screenshot after a watchdog timeout ran
        // outside the watchdog, on the device that had just hung.
        let dir = tempfile::tempdir().unwrap();
        let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
        let mut cfg = config(50);
        cfg.artifacts.on_failure = true;
        cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let t = test_case(vec![step(tap("Buy"))]);
        let run = ex.execute_test(&t, "d", "app");
        let r = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the run must end")
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
    }

    /// SUM-4: on the web `web.settle` wins over `sync.*` (a cross-platform
    /// suite tunes each side), what it leaves unset falls back to `sync.*`,
    /// and off the web it does nothing.
    #[test]
    fn web_settle_wins_over_sync_on_the_web_only() {
        let driver = ScriptDriver::new(vec![]);
        let suite = |platform: Platform| {
            let mut c = config(50);
            c.platform = Some(platform);
            c.sync.reaction_ms = Some(500);
            c.sync.quiet_ms = Some(90);
            c.web = Some(velocity_common::test_types::WebConfig {
                settle: Some(velocity_common::test_types::WebSettle {
                    reaction_ms: Some(77),
                    cap_ms: Some(600),
                    ..Default::default()
                }),
                ..Default::default()
            });
            c
        };
        let mut ex = TestExecutor::new(&driver, suite(Platform::Web), "app");
        let cfg = ex.sync_manager.sync_config_mut().clone();
        assert_eq!(
            (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms),
            (Some(77), Some(90), Some(600))
        );
        let mut ex = TestExecutor::new(&driver, suite(Platform::Android), "app");
        let cfg = ex.sync_manager.sync_config_mut().clone();
        assert_eq!(
            (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms),
            (Some(500), Some(90), None)
        );
    }

    #[tokio::test]
    async fn web_settle_survives_set_config_and_the_next_test() {
        // review-d6 M1: the overlay lived only in the SyncManager's clone,
        // so `setConfig` and `restore_test_config` reset it to `sync.*`.
        let driver = ScriptDriver::new(vec![]);
        let mut c = config(50);
        c.platform = Some(Platform::Web);
        c.sync.reaction_ms = Some(300);
        c.web = Some(velocity_common::test_types::WebConfig {
            settle: Some(velocity_common::test_types::WebSettle {
                reaction_ms: Some(40),
                cap_ms: Some(800),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut ex = TestExecutor::new(&driver, c, "app");
        let timings = |ex: &mut TestExecutor| {
            let cfg = ex.sync_manager.sync_config_mut().clone();
            (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms)
        };
        let set = step(Action::SetConfig {
            values: [("sync.quietMs".to_string(), "200".to_string())]
                .into_iter()
                .collect(),
        });
        let r = ex
            .execute_test(&test_case(vec![set]), "web", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        // Test 2 runs with web.settle again, not sync.* or the defaults.
        assert_eq!(timings(&mut ex), (Some(40), None, Some(800)));
        // And inside a test, setConfig changes only the key it sets.
        ex.execute_action(
            &Action::SetConfig {
                values: [("sync.quietMs".to_string(), "200".to_string())]
                    .into_iter()
                    .collect(),
            },
            "web",
            "app",
        )
        .await
        .unwrap();
        assert_eq!(timings(&mut ex), (Some(40), Some(200), Some(800)));
    }

    #[tokio::test]
    async fn a_cancelled_step_does_not_leak_its_settle_override() {
        // VEL-058: restored only when the step body returned.
        let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
        let mut ex = TestExecutor::new(&driver, config(50), "app");
        let cap = ex.sync_manager.sync_config_mut().settle_cap_ms;
        let hung = Step {
            settle: Some(velocity_common::StepSettle {
                cap_ms: Some(cap.unwrap_or_default() + 1234),
                ..Default::default()
            }),
            ..step(tap("Buy"))
        };
        let r = ex
            .execute_test(&test_case(vec![hung]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert_eq!(ex.sync_manager.sync_config_mut().settle_cap_ms, cap);
        assert!(ex.step_settle.is_none());
    }

    #[tokio::test]
    async fn huge_timeouts_do_not_overflow() {
        // VEL-191: `base * 3` and `Instant + timeoutMs` overflowed.
        let driver = ScriptDriver::new(vec![node("b", "Buy", 0)]);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let huge = Step {
            timeout_ms: Some(u64::MAX),
            ..step(tap("Buy"))
        };
        assert_eq!(ex.step_deadline(&huge), Duration::from_millis(u64::MAX));
        let wait = step(Action::Wait { ms: u64::MAX });
        assert_eq!(ex.step_deadline(&wait), Duration::from_millis(u64::MAX));
        let r = ex
            .execute_test(&test_case(vec![huge]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn a_screenshot_name_cannot_leave_the_artifacts_dir() {
        // VEL-197: `join` with `..` or an absolute path wrote outside it.
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("out");
        let driver = ScriptDriver::new(vec![]);
        let mut cfg = config(300);
        cfg.artifacts.output_dir = out.to_string_lossy().into_owned();
        cfg.artifacts.screenshots = velocity_common::ScreenshotPolicy::Always;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let shot = step(Action::Screenshot {
            filename: Some("../escaped.png".into()),
        });
        let r = ex
            .execute_test(&test_case(vec![shot]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed);
        assert!(!root.path().join("escaped.png").exists());
        assert!(out.join("escaped.png").exists());
    }

    /// A screen with an unlabeled icon button, and the driver's lookup
    /// cache on (Android's), so the audit reads only cached trees.
    fn a11y_driver() -> ScriptDriver {
        let icon = Element {
            text: None,
            element_type: "android.widget.ImageButton".into(),
            ..node("menu", "", 100)
        };
        let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0), icon]);
        driver.lookup_cache = Some(Mutex::new(None));
        driver
    }

    fn a11y_steps() -> TestCase {
        let seen = || {
            step(Action::AssertVisible {
                selector: Selector::Text("Pay".into()),
            })
        };
        test_case(vec![seen(), step(tap("Pay")), seen()])
    }

    #[tokio::test]
    async fn the_audit_warns_once_per_problem_and_reads_no_extra_trees() {
        let run = |cfg: velocity_common::AccessibilityConfig| async move {
            let driver = a11y_driver();
            let mut c = config(2000);
            c.accessibility = cfg;
            let mut ex = TestExecutor::new(&driver, c, "app");
            let r = ex.execute_test(&a11y_steps(), "d", "app").await.unwrap();
            let audits = ex.a11y.as_ref().map_or(0, |a| a.audits());
            (r, driver.dumps.load(Ordering::SeqCst), audits)
        };
        // Off by default: no findings.
        let (off, dumps_off, _) = run(Default::default()).await;
        assert_eq!(off.status, TestStatus::Passed, "{:?}", off.error_message);
        assert!(off.warnings.iter().all(|w| !w.starts_with("accessibility")));
        // On: the same screen read by three lookups is audited once, its
        // one problem reported once, and no tree is read for it.
        let on = velocity_common::AccessibilityConfig {
            audit: true,
            ..Default::default()
        };
        let (r, dumps_on, audits) = run(on.clone()).await;
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        let found: Vec<&String> = r
            .warnings
            .iter()
            .filter(|w| w.starts_with("accessibility: "))
            .collect();
        assert_eq!(found.len(), 1, "{:?}", r.warnings);
        assert!(
            found[0].contains("image-unlabeled") && found[0].contains("#menu"),
            "{found:?}"
        );
        assert_eq!(audits, 1);
        assert_eq!(dumps_on, dumps_off, "the audit read the screen itself");
        // `fail: true`: the error finding fails the test on its last step.
        let (r, _, _) = run(velocity_common::AccessibilityConfig { fail: true, ..on }).await;
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.starts_with("accessibility audit found 1 error(s)"),
            "{msg}"
        );
        assert_eq!(r.steps.last().unwrap().status, StepStatus::Failed);
    }

    #[tokio::test]
    async fn the_audit_caps_its_warnings_per_test() {
        let icons: Vec<Element> = (0..25)
            .map(|i| Element {
                text: None,
                element_type: "android.widget.ImageButton".into(),
                ..node(&format!("icon{i}"), "", 100 + i * 50)
            })
            .collect();
        let mut children = vec![node("pay", "Pay", 0)];
        children.extend(icons);
        let mut driver = ScriptDriver::new(children);
        driver.lookup_cache = Some(Mutex::new(None));
        let mut c = config(2000);
        c.accessibility.audit = true;
        let mut ex = TestExecutor::new(&driver, c, "app");
        let r = ex.execute_test(&a11y_steps(), "d", "app").await.unwrap();
        let found: Vec<&String> = r
            .warnings
            .iter()
            .filter(|w| w.starts_with("accessibility: "))
            .collect();
        assert_eq!(
            found.len(),
            crate::a11y_run::MAX_PER_TEST + 1,
            "{:?}",
            r.warnings
        );
        assert!(
            found.last().unwrap().contains("5 more finding(s) left out"),
            "{found:?}"
        );
    }
}
