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
                let e = if is_infrastructure(&e)
                    || matches!(e, VelocityError::AppCrashed { .. })
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
            if let Err(why) = velocity_common::natural::check_goto(
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
        resolver: &Arc<dyn NaturalStepResolver>,
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
        resolver: &Arc<dyn NaturalStepResolver>,
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
            velocity_common::natural::step_shape(written.trim()),
        );
        let n = self.natural_seen.entry(key).or_insert(0);
        let scope = format!("{}#{n}", self.current_test);
        *n += 1;
        scope
    }

    /// Hold a verified-so-far plan of the running step for the cache.
    fn stage(&mut self, plan: Option<PendingPlan>) {
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
                .commit_pending(ready.into_iter().map(|(_, p)| p).collect())
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
    /// Code decides: see `velocity_nl::checker`.
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
        // A failed capture leaves the check to the tree alone: say so
        // rather than judging blind in silence (review 03 M3).
        let shot = match self.driver.screenshot(device_id).await {
            Ok(png) => Some(png),
            Err(e) => {
                tracing::warn!(at, error = %e, "screenshot failed; verifying from the tree only");
                None
            }
        };
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
                | Action::Exec(_)
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
        // Names that differ only in case are one file on macOS and Windows
        // but two on Linux: refuse them everywhere, so a suite recorded on
        // one OS can't silently share or split baselines on another.
        if let Some(other) = self
            .baseline_names
            .values()
            .find(|o| *o != name && o.to_lowercase() == name.to_lowercase())
        {
            return Err(VelocityError::Config(format!(
                "assertScreenshot names {other:?} and {name:?} differ only in case: both map \
                 to one baseline file on macOS and Windows; rename one"
            )));
        }
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
                            "{selector} (gone or hidden, e.g. behind a sheet, while waiting \
                             for it to be actionable)"
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
            Action::Exec(e) => self.run_exec(e, device_id).await,
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
                arguments,
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
                let result = self
                    .driver
                    .launch_app_with(device_id, id, clear, arguments)
                    .await;
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
            Action::Drag {
                from,
                to,
                hold_ms,
                move_ms,
            } => {
                let timeout = self.element_timeout_ms();
                let a = self.find_actionable(device_id, from, timeout).await?;
                let b = self.find_container(device_id, to, timeout).await?;
                self.driver
                    .drag(
                        device_id,
                        a.bounds.center(),
                        b.bounds.center(),
                        hold_ms.unwrap_or(DRAG_HOLD_MS),
                        move_ms.unwrap_or(DRAG_MOVE_MS),
                    )
                    .await
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
                    let dir = direction.unwrap_or(Direction::Down);
                    let (a, b) = if is_slider(&el) {
                        slider_stroke(&el.bounds, dir)
                    } else {
                        line_across(&el.bounds, dir)
                    };
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
    /// `exec` (row 3.2): an allowed program on the host, no shell; its exit
    /// code and output checked, its output captured. The allow-list is
    /// checked again here, whatever validated the suite.
    async fn run_exec(
        &mut self,
        e: &velocity_common::exec::ExecStep,
        device_id: &str,
    ) -> Result<()> {
        if let Some(why) = e.refused_by(self.config.exec.as_ref()) {
            return Err(VelocityError::Config(why));
        }
        let argv = e.run.argv();
        // A relative program path (`./scripts/seed.sh`) is the one the
        // allow-list names from the run's directory: resolve it here, before
        // `cwd` (which can be interpolated) moves the child elsewhere and
        // picks another file of the same name (audit 2026-11 EXEC-2).
        let program = {
            let p = std::path::Path::new(&argv[0]);
            if e.cwd.is_some() && p.is_relative() && p.components().count() > 1 {
                std::env::current_dir()
                    .map(|d| d.join(p))
                    .unwrap_or_else(|_| p.to_path_buf())
            } else {
                p.to_path_buf()
            }
        };
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&argv[1..]).env("VELOCITY_DEVICE_ID", device_id);
        if let Some(dir) = &e.cwd {
            cmd.current_dir(dir);
        }
        let limit =
            std::time::Duration::from_millis(self.step_timeout_ms.unwrap_or(EXEC_TIMEOUT_MS));
        // Its own process group, killed whole on timeout, on cancel and on
        // a second Ctrl-C: `kill_on_drop` alone killed only `sh` and left
        // the script's children running (review 01 #3, VEL-074).
        let out = match velocity_common::process::output_with_timeout(&mut cmd, limit).await {
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                return Err(VelocityError::Config(format!(
                    "exec `{}` did not finish within {} ms (timeoutMs)",
                    argv[0],
                    limit.as_millis()
                )))
            }
            Err(err) => return Err(VelocityError::Config(format!("exec `{}`: {err}", argv[0]))),
            Ok(o) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = |s: &str| {
            let t: String = s
                .chars()
                .rev()
                .take(600)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            velocity_common::redact::redact(t.trim()).into_owned()
        };
        let want = e.expect.exit_code.unwrap_or(0);
        let code = out.status.code();
        if code != Some(want) {
            return Err(VelocityError::AssertionFailed {
                expected: format!("exec `{}` to exit {want}", argv[0]),
                actual: format!(
                    "exit {}: {}",
                    code.map_or("by a signal".to_string(), |c| c.to_string()),
                    tail(if stderr.trim().is_empty() {
                        &stdout
                    } else {
                        &stderr
                    })
                ),
                selector: format!("exec {}", argv[0]),
                screenshot: None,
            });
        }
        if let Some(needle) = &e.expect.contains {
            if !stdout.contains(needle.as_str()) && !stderr.contains(needle.as_str()) {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("exec `{}` output containing {needle:?}", argv[0]),
                    actual: tail(&stdout),
                    selector: format!("exec {}", argv[0]),
                    screenshot: None,
                });
            }
        }
        if let Some(name) = &e.capture {
            if velocity_common::redact::is_secret_name(name) {
                velocity_common::redact::register_secret(&stdout);
            }
            self.captured.insert(name.clone(), stdout);
        }
        Ok(())
    }

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
        let data =
            match tokio::time::timeout(FAILURE_ARTIFACT_BUDGET, self.driver.screenshot(device_id))
                .await
            {
                Ok(Ok(png)) => png,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, step_index, "failure screenshot not taken");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(
                        step_index,
                        "failure screenshot not taken: the device didn't answer in time"
                    );
                    return None;
                }
            };
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

/// A finger stroke across `b` in `finger`'s direction, from 80% to 20% of
/// the element along that axis, through its middle.
/// A slider (Android SeekBar or Slider, iOS Slider, the web's range input
/// or `role=slider`).
fn is_slider(el: &velocity_common::Element) -> bool {
    let t = el.element_type.to_ascii_lowercase();
    t.contains("seekbar") || t.contains("slider") || el.role.as_deref() == Some("slider")
}

/// A swipe along a slider goes to its far end: the 20%-80% stroke of
/// [`line_across`] could never set it to its maximum or minimum, so
/// "brightness to max" ended at 80% of the track (AndroidWorld rerun,
/// 2026-11-08). From a fifth in to two points past the far edge, where the
/// slider clamps.
fn slider_stroke(b: &velocity_common::Rect, finger: Direction) -> ((i32, i32), (i32, i32)) {
    let (x, y, w, h) = (
        i64::from(b.x),
        i64::from(b.y),
        i64::from(b.width),
        i64::from(b.height),
    );
    let (cx, cy) = (to_i32(x + w / 2), to_i32(y + h / 2));
    match finger {
        Direction::Right => ((to_i32(x + w / 5), cy), (to_i32(x + w + 2), cy)),
        Direction::Left => ((to_i32(x + w * 4 / 5), cy), (to_i32(x - 2), cy)),
        Direction::Up => ((cx, to_i32(y + h * 4 / 5)), (cx, to_i32(y - 2))),
        Direction::Down => ((cx, to_i32(y + h / 5)), (cx, to_i32(y + h + 2))),
    }
}

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
