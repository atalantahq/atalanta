//! The agent loop: observe → plan → ground → act → record.
//!
//! Each turn the model sees the current screen in full, the earlier turns
//! as one line each, the names of the values it may type, and any notes
//! (a rejected answer, a screen that didn't change, an operator's
//! instruction). It answers one [`GoalStep`]. Code then decides
//! everything that has to be right: the selector (from the element number,
//! via `robust_selector`), the value (from the run's values, by name),
//! whether the op is allowed, and whether the run succeeded (the checker
//! must cite real elements for the goal on the final screen).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use velocity_common::{
    Action, Direction, Key, Platform, PlatformDriver, Rect, Result, Selector, Step, StepStatus,
    SuiteConfig, VelocityError, VerifyCheck, VerifyLevel, VerifyOutcome,
};
use velocity_core::TestExecutor;
use velocity_nl::{
    GoalOp, GoalOutcome, GoalRequest, GoalStep, NaturalPlanner, ScreenChecker, Usage,
};

use crate::events::{EventSink, ExploreEvent, InstructionSource};
use crate::expect::{ExpectCheck, ExpectOutcome, ExpectationCounts, PostAssertion};
use crate::screen::{assertable, clip, element_name, Observation};
use crate::values::{self, Seen};

/// When a screenshot goes with the screen list (and only if the provider
/// takes images).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ScreenshotMode {
    Never,
    /// Only when the tree says little (fewer than four listed elements).
    #[default]
    Poor,
    /// Every turn.
    Always,
}

impl std::str::FromStr for ScreenshotMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "never" => Ok(Self::Never),
            "poor" => Ok(Self::Poor),
            "always" => Ok(Self::Always),
            other => Err(format!("{other:?}: never, poor or always")),
        }
    }
}

/// How the session starts the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LaunchMode {
    /// `launchApp` with `clearState: true`: a clean, repeatable start
    /// (the generated test starts the same way).
    Clean,
    /// `launchApp` keeping the app's data.
    #[default]
    Launch,
    /// Start from whatever is on screen; the test then has no launch step.
    Attach,
}

/// Where a typed value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ValueKind {
    /// A quoted literal in the goal: written into the test as is.
    Literal,
    /// `--data NAME=value`: written as `${NAME}`.
    Data,
    /// `--secret NAME`: read from `$NAME`, written as `${NAME}`, never sent
    /// to a model or shown.
    Secret,
}

/// A value the model may choose (by name) to type.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueSlot {
    pub name: String,
    pub kind: ValueKind,
    /// The text typed. Never serialized: reports and events carry names.
    #[serde(skip)]
    pub value: String,
}

impl std::fmt::Debug for ValueSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValueSlot({}, {:?})", self.name, self.kind)
    }
}

impl ValueSlot {
    /// What the generated YAML types: the literal, or `${NAME}`.
    pub fn yaml_text(&self) -> String {
        match self.kind {
            ValueKind::Literal => self.value.clone(),
            ValueKind::Data | ValueKind::Secret => format!("${{{}}}", self.name),
        }
    }

    /// The line the model sees.
    fn listing(&self) -> String {
        match self.kind {
            ValueKind::Literal => {
                format!("- {} = {:?} (quoted in the goal)", self.name, self.value)
            }
            ValueKind::Data => format!(
                "- {} (test data, {} characters)",
                self.name,
                self.value.chars().count()
            ),
            ValueKind::Secret => format!("- {} (secret)", self.name),
        }
    }
}

/// The values a goal brings: its quoted literals (`TEXT1`, `TEXT2`, …).
pub fn goal_literals(goal: &str) -> Vec<ValueSlot> {
    let mut out: Vec<ValueSlot> = Vec::new();
    for (open, close) in [('"', '"'), ('“', '”')] {
        let mut rest = goal;
        while let Some(a) = rest.find(open) {
            let after = &rest[a + open.len_utf8()..];
            let Some(b) = after.find(close) else { break };
            let lit = &after[..b];
            if !lit.trim().is_empty() && !out.iter().any(|s| s.value == lit) {
                out.push(ValueSlot {
                    name: format!("TEXT{}", out.len() + 1),
                    kind: ValueKind::Literal,
                    value: lit.to_string(),
                });
            }
            rest = &after[b + close.len_utf8()..];
        }
    }
    out
}

/// Everything a session needs besides the driver and the models.
#[derive(Debug, Clone)]
pub struct ExploreOptions {
    pub goal: String,
    pub app_id: String,
    pub device_id: String,
    pub platform: Platform,
    pub launch: LaunchMode,
    /// Planning turns (model calls for the goal) at most.
    pub max_steps: usize,
    pub timeout: Duration,
    /// Stop when the estimated spend passes this (needs `price`).
    pub max_cost_usd: Option<f64>,
    /// Stop when the model has used this many tokens (input + output).
    pub max_tokens: Option<u64>,
    /// Dollars per million input and output tokens, for the estimate.
    pub price: Option<(f64, f64)>,
    pub verify: VerifyLevel,
    /// `--data NAME=value`.
    pub data: Vec<(String, String)>,
    /// `--secret NAME`, with the value read from `$NAME`.
    pub secrets: Vec<(String, String)>,
    /// Actions in a row that leave the screen unchanged before the run
    /// gives up as stuck.
    pub stuck_limit: usize,
    /// The provider takes images (screenshots are sent per `screenshots`).
    pub images: bool,
    /// When a screenshot goes with the screen list.
    pub screenshots: ScreenshotMode,
    /// Benchmark mode (`--benchmark`): the app under test is only nominal
    /// (`launch` is refused; `openApp`, `home` move between apps), and the
    /// run is judged by an outside harness. The caller skips the YAML.
    pub benchmark: bool,
    /// `--values-from-goal`: the model may propose text to type itself;
    /// code accepts it only when grounded (see [`crate::values`]).
    pub values_from_goal: bool,
    /// The apps `openApp` may start, as (name the model sees, app id).
    pub apps: Vec<(String, String)>,
    /// Times one screen may be reached before the run counts as circling.
    pub max_visits: usize,
    /// Show the model how similar goals were done in this app: the one or
    /// two tests under `demo_dirs` most like the goal ([`crate::demos`]).
    /// On by default; never in a benchmark run.
    pub demos: bool,
    /// Where those tests are looked for (a few levels deep): the project's
    /// suites. Default: the current folder.
    pub demo_dirs: Vec<PathBuf>,
    /// Sync, dialog and other driver settings for the executor.
    pub config: SuiteConfig,
}

impl ExploreOptions {
    pub fn new(goal: &str, app_id: &str, device_id: &str, platform: Platform) -> Self {
        let mut config = SuiteConfig {
            platform: Some(platform),
            ..Default::default()
        };
        config.artifacts.on_failure = false;
        Self {
            goal: goal.to_string(),
            app_id: app_id.to_string(),
            device_id: device_id.to_string(),
            platform,
            launch: LaunchMode::Launch,
            max_steps: 40,
            timeout: Duration::from_secs(600),
            max_cost_usd: None,
            max_tokens: None,
            price: None,
            verify: VerifyLevel::EXPLORE_DEFAULT,
            data: vec![],
            secrets: vec![],
            stuck_limit: 3,
            images: false,
            screenshots: ScreenshotMode::Poor,
            benchmark: false,
            values_from_goal: false,
            apps: vec![],
            max_visits: MAX_VISITS,
            demos: true,
            demo_dirs: vec![PathBuf::from(".")],
            config,
        }
    }

    /// The values the model may type: the goal's literals, the data, the
    /// secrets.
    pub fn values(&self) -> Vec<ValueSlot> {
        let mut v = goal_literals(&self.goal);
        for (k, val) in &self.data {
            v.push(ValueSlot {
                name: k.clone(),
                kind: ValueKind::Data,
                value: val.clone(),
            });
        }
        for (k, val) in &self.secrets {
            v.push(ValueSlot {
                name: k.clone(),
                kind: ValueKind::Secret,
                value: val.clone(),
            });
        }
        v
    }
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// The model said done and (unless `verify: off`) the checker
    /// confirmed the goal with real evidence.
    Success,
    /// The model said done, but the check didn't hold.
    Unverified,
    /// The model gave up.
    GaveUp,
    /// The screen stopped changing, or the run kept circling.
    Stuck,
    /// Out of turns, tokens or money.
    BudgetExhausted,
    TimedOut,
    /// Stopped from outside.
    Stopped,
    /// The model service failed, or the start of the run did.
    Failed,
}

impl Outcome {
    pub fn is_success(self) -> bool {
        self == Outcome::Success
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::Unverified => "unverified",
            Self::GaveUp => "gave up",
            Self::Stuck => "stuck",
            Self::BudgetExhausted => "budget exhausted",
            Self::TimedOut => "timed out",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        })
    }
}

/// One action the session took that worked, as the test will replay it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedStep {
    /// The planning turn (0 for a replayed prefix step).
    pub turn: usize,
    pub op: GoalOp,
    /// The action as run, with real values: never serialized (a typed
    /// secret is in it); `label` stands in.
    #[serde(skip)]
    pub action: Action,
    /// The action as a short label, values by name.
    pub label: String,
    /// For inputText: the value's name and kind (the YAML types
    /// [`ValueSlot::yaml_text`], never the real secret).
    pub typed: Option<ValueSlot>,
    /// The target's name, for logs.
    pub target: Option<String>,
    /// For a scroll: the direction content moved into view.
    pub scroll: Option<Direction>,
    pub before_title: String,
    pub after_title: String,
    pub changed: bool,
    pub ms: u64,
    /// A stable, distinctive element of the screen this step led to, when
    /// the screen changed: the test asserts it after the step.
    pub screen_assertion: Option<Selector>,
    /// The model's expected post-condition, as code judged it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expectation: Option<ExpectCheck>,
    /// A met expectation's assertion (built from the element that matched),
    /// written after the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_assertion: Option<PostAssertion>,
}

/// What a session produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreReport {
    pub goal: String,
    pub app_id: String,
    pub platform: Platform,
    pub launch: LaunchMode,
    pub outcome: Outcome,
    pub reason: String,
    pub steps: Vec<RecordedStep>,
    /// Assertions on the elements the checker cited for the goal.
    pub final_assertions: Vec<Selector>,
    pub checks: Vec<VerifyCheck>,
    pub turns: usize,
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub elapsed_ms: u64,
    pub values: Vec<ValueSlot>,
    /// The grounded answer to a question goal (`answer` op), if any.
    pub answer: Option<String>,
    /// The goal prompt's version ([`velocity_nl::goal::GOAL_PROMPT_VERSION`]).
    pub prompt_version: &'static str,
    /// The project tests shown to the model as demonstrations, as
    /// `file: test name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub demos: Vec<String>,
    /// How the model's expected post-conditions turned out.
    pub expectations: ExpectationCounts,
}

/// Steers a running session from elsewhere: inject an instruction, stop.
#[derive(Clone)]
pub struct ExploreHandle {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    cancel: Arc<AtomicBool>,
}

impl ExploreHandle {
    /// A handle over an existing instruction channel and stop flag.
    pub fn from_parts(
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self { tx, cancel }
    }

    /// Taken in at the next planning turn, shown to the model as an
    /// operator's instruction.
    pub fn inject(&self, instruction: String) {
        let _ = self.tx.send(instruction);
    }

    /// Stop at the next turn; the report says `stopped`.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// Lines of history the model sees at most; older turns are counted.
const HISTORY_LINES: usize = 12;
/// Characters of history at most.
const HISTORY_CHARS: usize = 2400;
/// Fewer listed elements than this is a poor tree: a screenshot helps.
const POOR_TREE: usize = 4;
/// Answers in a row code refused before the run gives up.
const MAX_REJECTS: usize = 3;
/// Times one screen may be reached before the run counts as circling.
const MAX_VISITS: usize = 6;
/// Done(success) answers whose check failed before the run ends unverified.
const MAX_FAILED_CHECKS: usize = 2;
/// The standing note in a benchmark run.
const BENCHMARK_NOTE: &str = "This run is judged on the device's final state (and your answer, if the goal asks a question); no test is written, so don't assert. Answer done once the goal is reached and saved.";
/// Characters of the model's memo carried to its next turn.
const MEMO_CHARS: usize = 600;
/// A screen reached again within this many turns is a loop the model is
/// told about by distance ("N turns ago"); an older revisit gets the plain
/// "seen before" note.
const REVISIT_TURNS: usize = 4;
/// The values-list line that allows copied text (`--values-from-goal`).
const COPY_RULE: &str = "- Or text copied exactly from the goal or from a text shown on a screen in this run: put that text itself in value (code checks it is there; plain numbers are allowed too)";

/// An explore session. Build it, take a [`handle`](Self::handle) if
/// something else should steer it, then [`run`](Self::run) it.
pub struct ExploreSession {
    driver: Arc<dyn PlatformDriver>,
    planner: Arc<dyn NaturalPlanner>,
    checker: Option<Arc<dyn ScreenChecker>>,
    opts: ExploreOptions,
    sink: Option<EventSink>,
    sources: Vec<Box<dyn InstructionSource>>,
    handle: ExploreHandle,
    prefix: Vec<RecordedStep>,
}

impl ExploreSession {
    pub fn new(
        driver: Arc<dyn PlatformDriver>,
        planner: Arc<dyn NaturalPlanner>,
        checker: Option<Arc<dyn ScreenChecker>>,
        opts: ExploreOptions,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            driver,
            planner,
            checker,
            opts,
            sink: None,
            sources: vec![Box::new(rx)],
            handle: ExploreHandle {
                tx,
                cancel: Arc::new(AtomicBool::new(false)),
            },
            prefix: vec![],
        }
    }

    /// Send every event to `sink`.
    pub fn with_events(mut self, sink: EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Also take instructions from `source` (an MCP task's inject channel).
    pub fn with_instructions(mut self, source: Box<dyn InstructionSource>) -> Self {
        self.sources.push(source);
        self
    }

    /// Stop when `flag` is set (an MCP task's cancellation).
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.handle.cancel = flag;
        self
    }

    /// Replay these steps (without the model) after the launch, then
    /// explore on from there: how a repair resumes.
    pub fn with_prefix(mut self, steps: Vec<RecordedStep>) -> Self {
        self.prefix = steps;
        self
    }

    pub fn handle(&self) -> ExploreHandle {
        self.handle.clone()
    }

    /// Run the session to its end. `Err` only for a setup mistake (no
    /// checker for the verify level) or a device that stopped answering;
    /// everything else is an [`Outcome`] in the report.
    pub async fn run(mut self) -> Result<ExploreReport> {
        let opts = self.opts.clone();
        if opts.verify != VerifyLevel::Off && self.checker.is_none() {
            return Err(VelocityError::Config(format!(
                "--verify {} needs a checker model; configure one or pass --verify off",
                opts.verify
            )));
        }
        for (_, v) in &opts.secrets {
            velocity_common::redact::register_secret(v);
        }
        // Read before the first turn, off the async workers: a project
        // folder of suites is a few hundred small files at most.
        let demos = if opts.demos && !opts.benchmark {
            let (dirs, app, goal, platform) = (
                opts.demo_dirs.clone(),
                opts.app_id.clone(),
                opts.goal.clone(),
                opts.platform,
            );
            tokio::task::spawn_blocking(move || crate::demos::find(&dirs, &app, platform, &goal))
                .await
                .unwrap_or_default()
        } else {
            vec![]
        };
        let driver = self.driver.clone();
        let mut run = Run {
            started: Instant::now(),
            executor: TestExecutor::new(&*driver, opts.config.clone(), &opts.app_id),
            driver: &*driver,
            planner: self.planner.clone(),
            checker: self.checker.clone(),
            sink: self.sink.take(),
            values: opts.values(),
            opts,
            report_steps: vec![],
            checks: vec![],
            final_assertions: vec![],
            step_index: 0,
            planner_start: self.planner.usage(),
            checker_start: self.checker.as_ref().map(|c| c.usage()).unwrap_or_default(),
            seen: Seen::default(),
            answer: None,
            demo_text: crate::demos::render(&demos, crate::demos::DEMO_CHARS),
            demo_names: demos
                .iter()
                .map(|d| {
                    let file = d.file.file_name().unwrap_or_default().to_string_lossy();
                    format!("{file}: {}", d.test)
                })
                .collect(),
            expectations: ExpectationCounts::default(),
        };
        let (outcome, reason, turns) = run.drive(&mut self).await?;
        Ok(run.finish(outcome, reason, turns))
    }
}

/// The state of one running session.
struct Run<'a> {
    started: Instant,
    executor: TestExecutor<'a>,
    driver: &'a dyn PlatformDriver,
    planner: Arc<dyn NaturalPlanner>,
    checker: Option<Arc<dyn ScreenChecker>>,
    sink: Option<EventSink>,
    values: Vec<ValueSlot>,
    opts: ExploreOptions,
    report_steps: Vec<RecordedStep>,
    checks: Vec<VerifyCheck>,
    final_assertions: Vec<Selector>,
    step_index: usize,
    planner_start: Usage,
    checker_start: Usage,
    /// Every text shown during the run (grounds copied values, answers).
    seen: Seen,
    answer: Option<String>,
    /// The demonstrations every goal turn carries ("" for none).
    demo_text: String,
    demo_names: Vec<String>,
    expectations: ExpectationCounts,
}

/// A model choice turned into an action by code.
struct Grounded {
    action: Action,
    typed: Option<ValueSlot>,
    target: Option<String>,
    scroll: Option<Direction>,
}

impl<'a> Run<'a> {
    fn at(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn emit(&mut self, e: ExploreEvent) {
        if let Some(sink) = self.sink.as_mut() {
            sink(&e);
        }
    }

    fn usage(&self) -> Usage {
        let p = self.planner.usage().since(self.planner_start);
        let c = self
            .checker
            .as_ref()
            .map(|c| c.usage().since(self.checker_start))
            .unwrap_or_default();
        Usage {
            calls: p.calls + c.calls,
            input_tokens: p.input_tokens + c.input_tokens,
            output_tokens: p.output_tokens + c.output_tokens,
        }
    }

    fn cost(&self) -> Option<f64> {
        let u = self.usage();
        self.opts
            .price
            .map(|(i, o)| (u.input_tokens as f64 * i + u.output_tokens as f64 * o) / 1e6)
    }

    async fn observe(&mut self) -> Result<Observation> {
        let d = &self.opts.device_id;
        let root = self.driver.get_hierarchy(d).await?;
        let rect = self
            .driver
            .screen_size(d)
            .await
            .ok()
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            });
        let obs = Observation::new(root, rect, self.opts.platform, &self.opts.goal);
        for el in obs.elements() {
            for t in [&el.text, &el.label].into_iter().flatten() {
                self.seen.add(t);
            }
        }
        Ok(obs)
    }

    /// Run one action through the deterministic executor (sync, settle,
    /// dialogs, actionability), as the replay will.
    async fn exec(&mut self, action: &Action) -> Result<(bool, u64, Option<u64>, Option<String>)> {
        let step = Step {
            action: action.clone(),
            timeout_ms: None,
            settle: None,
            source: None,
            modifiers: Default::default(),
        };
        let t = Instant::now();
        let i = self.step_index;
        self.step_index += 1;
        let r = self
            .executor
            .execute_step(
                &step,
                &self.opts.device_id.clone(),
                &self.opts.app_id.clone(),
                i,
            )
            .await?;
        Ok((
            r.status == StepStatus::Passed,
            t.elapsed().as_millis() as u64,
            r.settle_ms,
            r.error_message,
        ))
    }

    /// Start the app (per the launch mode) and replay the prefix.
    async fn start(&mut self, prefix: Vec<RecordedStep>) -> Result<Option<String>> {
        let launch = match self.opts.launch {
            LaunchMode::Clean => Some(true),
            LaunchMode::Launch => Some(false),
            LaunchMode::Attach => None,
        };
        if let Some(clear_state) = launch {
            let action = Action::LaunchApp {
                app_id: self.opts.app_id.clone(),
                clear_state,
            };
            let (ok, ms, _, err) = self.exec(&action).await?;
            let at = self.at();
            self.emit(ExploreEvent::Prefix {
                at_ms: at,
                step: 0,
                action: "launchApp".into(),
                ok,
                ms,
            });
            if !ok {
                return Ok(Some(format!(
                    "launching {} failed: {}",
                    self.opts.app_id,
                    err.unwrap_or_default()
                )));
            }
        }
        for (i, mut s) in prefix.into_iter().enumerate() {
            let (ok, ms, _, err) = self.exec(&s.action).await?;
            let at = self.at();
            self.emit(ExploreEvent::Prefix {
                at_ms: at,
                step: i + 1,
                action: crate::yaml::action_label(&s.action),
                ok,
                ms,
            });
            if !ok {
                return Ok(Some(format!(
                    "replaying step {} of the repair prefix failed: {}",
                    i + 1,
                    err.unwrap_or_default()
                )));
            }
            s.turn = 0;
            s.ms = ms;
            self.report_steps.push(s);
        }
        Ok(None)
    }

    async fn drive(&mut self, session: &mut ExploreSession) -> Result<(Outcome, String, usize)> {
        let prefix = std::mem::take(&mut session.prefix);
        if let Some(why) = self.start(prefix).await? {
            return Ok((Outcome::Failed, why, 0));
        }
        if !self.demo_text.is_empty() {
            let note = format!(
                "showing the model how similar goals were done: {}",
                self.demo_names.join("; ")
            );
            let at = self.at();
            self.emit(ExploreEvent::Note {
                at_ms: at,
                turn: 0,
                note,
            });
        }
        let mut obs = self.observe().await?;
        // The screen at the start of each turn (index 0 is turn 1).
        let mut screens: Vec<u64> = Vec::new();
        let mut first_seen: HashMap<u64, usize> = HashMap::new();
        let mut visits: HashMap<u64, usize> = HashMap::new();
        first_seen.insert(obs.hash, 0);
        *visits.entry(obs.hash).or_default() += 1;
        let mut history: Vec<String> = self
            .report_steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("#{} (replayed) {}", i + 1, step_line(s)))
            .collect();
        let mut notes: Vec<String> = Vec::new();
        let mut noop_streak = 0usize;
        // Of those, how many were actions that missed their expectation.
        let mut missed_in_streak = 0usize;
        let mut rejects = 0usize;
        let mut failed_checks = 0usize;
        let mut asserts_here = 0usize;
        let mut memo = String::new();
        let mut turn = 0usize;
        let opts = self.opts.clone();
        loop {
            if session.handle.is_stopped() {
                return Ok((Outcome::Stopped, "stopped from outside".into(), turn));
            }
            if turn >= opts.max_steps {
                return Ok((
                    Outcome::BudgetExhausted,
                    format!("used all {} turns (--max-steps)", opts.max_steps),
                    turn,
                ));
            }
            if self.started.elapsed() >= opts.timeout {
                return Ok((
                    Outcome::TimedOut,
                    format!("ran past --timeout {:?}", opts.timeout),
                    turn,
                ));
            }
            let u = self.usage();
            if let Some(max) = opts.max_tokens {
                if u.input_tokens + u.output_tokens >= max {
                    return Ok((
                        Outcome::BudgetExhausted,
                        format!(
                            "used {} tokens (--max-tokens {max})",
                            u.input_tokens + u.output_tokens
                        ),
                        turn,
                    ));
                }
            }
            if let (Some(max), Some(cost)) = (opts.max_cost_usd, self.cost()) {
                if cost >= max {
                    return Ok((
                        Outcome::BudgetExhausted,
                        format!("spent an estimated ${cost:.4} (--max-cost {max})"),
                        turn,
                    ));
                }
            }
            turn += 1;

            for source in session.sources.iter_mut() {
                for instruction in source.drain() {
                    let at = self.at();
                    self.emit(ExploreEvent::Injected {
                        at_ms: at,
                        turn,
                        instruction: instruction.clone(),
                    });
                    notes.push(format!("Operator instruction: {instruction}"));
                }
            }
            if opts.benchmark {
                notes.push(BENCHMARK_NOTE.into());
            }
            screens.push(obs.hash);
            let seen_at = first_seen.get(&obs.hash).copied().filter(|&k| k + 1 < turn);
            if let Some(ago) = revisit(&screens, REVISIT_TURNS) {
                // On-demand reflection (MobileUse): a loop is named the turn
                // it closes, with its length, so the model can see which of
                // its own steps led back here.
                let note = format!(
                    "You are back on a screen you saw {ago} turns ago (at turn {}), with the same elements: the steps since then led back here without changing it. If that was not the plan, don't repeat them; try another way.",
                    turn - ago
                );
                let at = self.at();
                self.emit(ExploreEvent::Note {
                    at_ms: at,
                    turn,
                    note: note.clone(),
                });
                notes.push(note);
            } else if let Some(k) = seen_at {
                notes.push(format!(
                    "You have seen this screen before (after turn {k}); don't repeat what didn't work."
                ));
            }
            let want_shot = match opts.screenshots {
                ScreenshotMode::Never => false,
                ScreenshotMode::Poor => obs.index.len() < POOR_TREE,
                ScreenshotMode::Always => true,
            };
            let screenshot = if opts.images && want_shot {
                self.driver.screenshot(&opts.device_id).await.ok()
            } else {
                None
            };
            let at = self.at();
            self.emit(ExploreEvent::Observed {
                at_ms: at,
                turn,
                title: obs.title.clone(),
                elements: obs.index.len(),
                hash: format!("{:016x}", obs.hash),
                seen_at,
                screenshot_sent: screenshot.is_some(),
            });

            let history_text = trim_history(&history);
            let mut values_text = self
                .values
                .iter()
                .map(ValueSlot::listing)
                .collect::<Vec<_>>()
                .join("\n");
            if opts.values_from_goal {
                if !values_text.is_empty() {
                    values_text.push('\n');
                }
                values_text.push_str(COPY_RULE);
            }
            let apps_text = opts
                .apps
                .iter()
                .map(|(name, _)| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n");
            let memo_text = velocity_common::redact::redact(&memo).into_owned();
            let screen_text = velocity_common::redact::redact(&obs.rendered).into_owned();
            let notes_text = velocity_common::redact::redact(
                &notes
                    .iter()
                    .map(|n| format!("- {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .into_owned();
            notes.clear();
            let before = self.planner.usage();
            let t = Instant::now();
            let answer = self
                .planner
                .plan_goal(&GoalRequest {
                    goal: &opts.goal,
                    platform: opts.platform,
                    demos: &self.demo_text,
                    screen: &screen_text,
                    history: &history_text,
                    values: &values_text,
                    apps: &apps_text,
                    memo: &memo_text,
                    notes: &notes_text,
                    turns_left: opts.max_steps - turn + 1,
                    screenshot: screenshot.as_deref(),
                })
                .await;
            let model_ms = t.elapsed().as_millis() as u64;
            let spent = self.planner.usage().since(before);
            let step = match answer {
                Ok(s) => s,
                Err(e) if velocity_nl::planner::is_model_failure(&e) => {
                    let why = format!("the answer could not be used: {e}");
                    self.reject(turn, &why, &mut notes, &mut history);
                    rejects += 1;
                    if rejects >= MAX_REJECTS {
                        return Ok((
                            Outcome::GaveUp,
                            "the model kept giving unusable answers".into(),
                            turn,
                        ));
                    }
                    continue;
                }
                Err(e) => {
                    return Ok((
                        Outcome::Failed,
                        format!(
                            "the model service failed: {}",
                            velocity_common::redact::redact(&e.to_string())
                        ),
                        turn,
                    ))
                }
            };
            if !step.memo.trim().is_empty() {
                memo = clip(step.memo.trim(), MEMO_CHARS);
            }
            let target = step
                .element
                .and_then(|n| obs.index.get(n))
                .map(|el| element_name(el, opts.platform));
            let at = self.at();
            self.emit(ExploreEvent::Planned {
                at_ms: at,
                turn,
                op: op_name(step.op).into(),
                element: step.element,
                target: target.clone(),
                reason: clip(&step.reason, 200),
                model_ms,
                input_tokens: spent.input_tokens,
                output_tokens: spent.output_tokens,
            });

            if step.op == GoalOp::Done {
                match step.outcome {
                    GoalOutcome::Success => {
                        if let Some(answer) = &self.answer {
                            // A question goal: the answer was grounded by
                            // code when it was given; the final screen
                            // needn't show the goal "reached".
                            return Ok((
                                Outcome::Success,
                                format!(
                                    "answered {:?} (grounded in text shown in this run)",
                                    clip(answer, 120)
                                ),
                                turn,
                            ));
                        }
                        if !opts.verify.checks_final() {
                            return Ok((
                                Outcome::Success,
                                format!(
                                    "the model reported success (not checked: --verify off): {}",
                                    step.reason
                                ),
                                turn,
                            ));
                        }
                        let fresh = self.observe().await?;
                        let check = self.check_goal(&fresh, &history).await;
                        if check.outcome == VerifyOutcome::Holds {
                            self.final_assertions =
                                evidence_selectors(&check, &fresh, &self.user_values());
                            let reason = format!("checked: {}", check.reason);
                            self.checks.push(check);
                            return Ok((Outcome::Success, reason, turn));
                        }
                        failed_checks += 1;
                        let why = format!("{:?}: {}", check.outcome, check.reason).to_lowercase();
                        self.checks.push(check);
                        if failed_checks >= MAX_FAILED_CHECKS {
                            return Ok((
                                Outcome::Unverified,
                                format!("the model said the goal was reached, but the check didn't confirm it ({why})"),
                                turn,
                            ));
                        }
                        notes.push(format!(
                            "You answered done/success, but an independent check of the goal on this screen did not confirm it ({why}). Keep going, or give up."
                        ));
                        history.push(format!("#{turn} done(success) rejected by the check"));
                        obs = fresh;
                        continue;
                    }
                    _ => {
                        let reason = if step.reason.trim().is_empty() {
                            "the model gave up".to_string()
                        } else {
                            step.reason.clone()
                        };
                        return Ok((Outcome::GaveUp, reason, turn));
                    }
                }
            }

            if step.op == GoalOp::Answer {
                match self.ground_answer(&step, &obs) {
                    Ok(grounding) => {
                        rejects = 0;
                        let answer = step.value.trim().to_string();
                        let at = self.at();
                        self.emit(ExploreEvent::Answered {
                            at_ms: at,
                            turn,
                            answer: velocity_common::redact::redact(&answer).into_owned(),
                            sources: grounding
                                .parts
                                .iter()
                                .map(|(_, s)| format!("{s:?}").to_lowercase())
                                .collect(),
                        });
                        history.push(format!(
                            "#{turn} answered {:?} (accepted; now answer done)",
                            clip(&answer, 80)
                        ));
                        self.answer = Some(answer);
                    }
                    Err(why) => {
                        self.reject(turn, &why, &mut notes, &mut history);
                        rejects += 1;
                        if rejects >= MAX_REJECTS {
                            return Ok((
                                Outcome::GaveUp,
                                format!("the model kept giving answers code refused (last: {why})"),
                                turn,
                            ));
                        }
                    }
                }
                continue;
            }

            let grounded = match self.ground(&step, &obs) {
                Ok(g) => g,
                Err(why) => {
                    self.reject(turn, &why, &mut notes, &mut history);
                    rejects += 1;
                    if rejects >= MAX_REJECTS {
                        return Ok((
                            Outcome::GaveUp,
                            format!("the model kept giving answers code refused (last: {why})"),
                            turn,
                        ));
                    }
                    continue;
                }
            };
            rejects = 0;

            if step.op == GoalOp::Assert {
                if asserts_here >= 2 {
                    notes.push("Enough assertions on this screen; answer done, or move on.".into());
                    history.push(format!(
                        "#{turn} assert skipped (two already on this screen)"
                    ));
                    continue;
                }
                let (ok, ms, _, err) = self.exec(&grounded.action).await?;
                let label = crate::yaml::action_label(&grounded.action);
                let at = self.at();
                self.emit(ExploreEvent::Asserted {
                    at_ms: at,
                    turn,
                    assertion: label.clone(),
                    ok,
                });
                if ok {
                    asserts_here += 1;
                    self.report_steps.push(RecordedStep {
                        turn,
                        op: step.op,
                        label: label.clone(),
                        action: grounded.action,
                        typed: None,
                        target: grounded.target,
                        scroll: None,
                        before_title: obs.title.clone(),
                        after_title: obs.title.clone(),
                        changed: false,
                        ms,
                        screen_assertion: None,
                        expectation: None,
                        post_assertion: None,
                    });
                    history.push(format!("#{turn} {label} holds"));
                    if opts.verify.checks_checkpoints() && !step.reason.trim().is_empty() {
                        let check = self.checkpoint(&obs, &step.reason, turn).await;
                        let failed = !check.outcome.passed();
                        self.checks.push(check);
                        if failed && opts.verify.is_strict() {
                            return Ok((
                                Outcome::Unverified,
                                format!("a checkpoint failed under --verify strict at turn {turn}"),
                                turn,
                            ));
                        }
                    }
                } else {
                    history.push(format!("#{turn} {label} failed"));
                    notes.push(format!(
                        "Your assertion failed: {}",
                        err.unwrap_or_default()
                    ));
                }
                continue;
            }

            let (ok, ms, settle_ms, err) = self.exec(&grounded.action).await?;
            let after = self.observe().await?;
            let changed = after.hash != obs.hash;
            let label = crate::yaml::action_label_named(&grounded.action, grounded.typed.as_ref());
            // The model's post-condition, judged by code on the screen the
            // action led to (Q-Reflector).
            let expectation = step.expect.as_ref().map(|e| {
                if ok {
                    self.check_expectation(e, &obs, &after)
                } else {
                    crate::expect::not_checkable(e, &obs, "the action failed")
                }
            });
            let missed = expectation
                .as_ref()
                .is_some_and(|c| c.outcome == ExpectOutcome::NotMet);
            if let Some(c) = &expectation {
                self.expectations.add(c.outcome);
            }
            let at = self.at();
            self.emit(ExploreEvent::Acted {
                at_ms: at,
                turn,
                action: label.clone(),
                ok,
                ms,
                settle_ms,
                changed,
                error: err
                    .as_ref()
                    .map(|e| velocity_common::redact::redact(e).into_owned()),
                expectation: expectation.clone(),
            });
            if !ok {
                notes.push(format!(
                    "Your last action failed on the device: {}",
                    velocity_common::redact::redact(&err.unwrap_or_default())
                ));
                history.push(format!("#{turn} {label} FAILED"));
                noop_streak += 1;
            } else {
                let scrolled = step.op == GoalOp::Scroll || step.op == GoalOp::Swipe;
                let screen_assertion = (changed && !scrolled)
                    .then(|| distinctive(&obs, &after, &self.user_values()))
                    .flatten();
                // A scroll is written as scrollUntilVisible or a swipe of
                // another distance, so what it showed isn't asserted.
                let post_assertion = expectation
                    .as_ref()
                    .filter(|_| !scrolled)
                    .and_then(|c| c.assertion.clone());
                let rec = RecordedStep {
                    turn,
                    op: step.op,
                    label: label.clone(),
                    action: grounded.action,
                    typed: grounded.typed,
                    target: grounded.target,
                    scroll: grounded.scroll,
                    before_title: obs.title.clone(),
                    after_title: after.title.clone(),
                    changed,
                    ms,
                    screen_assertion,
                    expectation: expectation.clone(),
                    post_assertion,
                };
                let mut line = format!("#{turn} {}", step_line(&rec));
                if let Some(k) = first_seen.get(&after.hash).filter(|_| changed) {
                    line.push_str(&format!(" (seen after turn {k})"));
                }
                if let Some(c) = expectation.as_ref().filter(|_| missed) {
                    line.push_str(&format!(" (expected {}: not met)", c.expected));
                }
                history.push(line);
                self.report_steps.push(rec);
                // A missed expectation counts as an unchanged screen: the
                // action didn't do what it was for. A met one on a screen
                // that visibly changed (a switch flipped) is progress even
                // when the structure, and so the hash, is the same.
                let met = expectation
                    .as_ref()
                    .is_some_and(|c| c.outcome == ExpectOutcome::Met);
                let progressed =
                    changed || step.op == GoalOp::Wait || (met && !after.looks_same(&obs));
                if progressed && !missed {
                    noop_streak = 0;
                    missed_in_streak = 0;
                } else {
                    noop_streak += 1;
                    missed_in_streak += usize::from(missed);
                }
            }
            if changed {
                asserts_here = 0;
            }
            let missing = if missed_in_streak > 0 {
                " or missed what they were expected to do"
            } else {
                ""
            };
            if noop_streak >= opts.stuck_limit {
                return Ok((
                    Outcome::Stuck,
                    format!("{noop_streak} actions in a row left the screen unchanged{missing}"),
                    turn,
                ));
            }
            if let Some(c) = expectation.as_ref().filter(|_| missed) {
                let note = format!(
                    "You expected {} after {label}, but {}: the action did not do what you meant. Read this screen and re-plan from it.",
                    c.expected,
                    c.detail.as_deref().unwrap_or("it didn't happen")
                );
                let at = self.at();
                self.emit(ExploreEvent::Note {
                    at_ms: at,
                    turn,
                    note: note.clone(),
                });
                notes.push(note);
            }
            let reflection = if noop_streak >= 2 {
                Some(format!(
                    "Your last {noop_streak} actions did not change the screen{missing}. Try something different, or give up."
                ))
            } else if ok && step.op != GoalOp::Wait && after.looks_same(&obs) {
                // On-demand reflection (MobileUse): the first action with
                // no visible effect is named at once, not after a streak.
                // Not by the hash: a switch that flipped or a list that
                // moved changed the screen though not its structure.
                // A scroll is named the way the model asked for it, not by
                // the finger's direction its label shows.
                let what = if step.op == GoalOp::Scroll {
                    format!("scroll {}", content_word(&step.direction))
                } else {
                    label.clone()
                };
                Some(no_effect_note(step.op, &what))
            } else {
                None
            };
            if let Some(note) = reflection {
                let at = self.at();
                self.emit(ExploreEvent::Note {
                    at_ms: at,
                    turn,
                    note: note.clone(),
                });
                notes.push(note);
            }
            first_seen.entry(after.hash).or_insert(turn);
            let v = visits.entry(after.hash).or_default();
            *v += 1;
            if changed && *v >= opts.max_visits {
                return Ok((
                    Outcome::Stuck,
                    format!(
                        "reached the screen \"{}\" {} times: going in circles",
                        after.title, *v
                    ),
                    turn,
                ));
            }
            obs = after;
        }
    }

    fn reject(
        &mut self,
        turn: usize,
        why: &str,
        notes: &mut Vec<String>,
        history: &mut Vec<String>,
    ) {
        let at = self.at();
        self.emit(ExploreEvent::Rejected {
            at_ms: at,
            turn,
            why: why.to_string(),
        });
        notes.push(format!(
            "Your last answer was rejected: {why}. Answer again."
        ));
        history.push(format!("#{turn} answer rejected"));
    }

    /// Judge an expectation. A text that only names a value (`EMAIL`,
    /// `${EMAIL}`) is a name, not something the screen shows.
    fn check_expectation(
        &self,
        e: &velocity_nl::Expectation,
        before: &Observation,
        after: &Observation,
    ) -> ExpectCheck {
        if let velocity_nl::Expectation::Appears { text } = e {
            let t = text.trim().trim_start_matches("${").trim_end_matches('}');
            if self.values.iter().any(|v| v.name.eq_ignore_ascii_case(t)) {
                return crate::expect::not_checkable(e, before, "it names a value, not a text");
            }
        }
        crate::expect::check(e, before, after, &self.user_values())
    }

    fn user_values(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|v| v.kind != ValueKind::Literal)
            .map(|v| v.value.clone())
            .collect()
    }

    /// Code's half of a turn: check the model's choice and build the action.
    fn ground(&self, step: &GoalStep, obs: &Observation) -> std::result::Result<Grounded, String> {
        let platform = self.opts.platform;
        let element = || -> std::result::Result<(Selector, String, bool), String> {
            let n = step.element.ok_or_else(|| {
                format!("{} needs an element number from the list", op_name(step.op))
            })?;
            let el = obs.index.get(n).ok_or_else(|| {
                format!(
                    "element {n} is not in the list (valid: 0..{})",
                    obs.index.len().saturating_sub(1)
                )
            })?;
            let sel = obs.selector_for(n).ok_or("no selector")?;
            Ok((sel, element_name(el, platform), el.enabled))
        };
        let enabled =
            |r: (Selector, String, bool)| -> std::result::Result<(Selector, String), String> {
                if r.2 {
                    Ok((r.0, r.1))
                } else {
                    Err(format!("\"{}\" is disabled", r.1))
                }
            };
        let plain = |action: Action| Grounded {
            action,
            typed: None,
            target: None,
            scroll: None,
        };
        let content_dir = || match step.direction.as_str() {
            "up" => Some(Direction::Up),
            "down" => Some(Direction::Down),
            "left" => Some(Direction::Left),
            "right" => Some(Direction::Right),
            _ => None,
        };
        Ok(match step.op {
            GoalOp::Tap | GoalOp::LongPress | GoalOp::ClearText => {
                let (selector, name) = enabled(element()?)?;
                let action = match step.op {
                    GoalOp::Tap => Action::Tap { selector },
                    GoalOp::LongPress => Action::LongPress {
                        selector,
                        duration_ms: (step.ms > 0).then_some(step.ms.min(10_000)),
                    },
                    _ => Action::ClearText { selector },
                };
                Grounded {
                    action,
                    typed: None,
                    target: Some(name),
                    scroll: None,
                }
            }
            GoalOp::InputText => {
                let (selector, name) = enabled(element()?)?;
                let wanted = step
                    .value
                    .trim()
                    .trim_start_matches("${")
                    .trim_end_matches('}');
                let named = self
                    .values
                    .iter()
                    .find(|v| v.name.eq_ignore_ascii_case(wanted))
                    .cloned();
                let slot = match named {
                    Some(slot) => slot,
                    None if self.opts.values_from_goal => {
                        values::ground_typed(&step.value, &self.opts.goal, &self.seen)?;
                        ValueSlot {
                            name: format!("{:?}", clip(step.value.trim(), 40)),
                            kind: ValueKind::Literal,
                            value: step.value.clone(),
                        }
                    }
                    None if self.values.is_empty() => {
                        return Err("there are no values to type; if the goal needs text, give up and name the missing value".into())
                    }
                    None => {
                        return Err(format!(
                            "value must be one of the listed names ({}), not the text itself",
                            self.values.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
                        ))
                    }
                };
                Grounded {
                    action: Action::InputText {
                        selector,
                        text: slot.value.clone(),
                    },
                    typed: Some(slot),
                    target: Some(name),
                    scroll: None,
                }
            }
            GoalOp::Assert if self.opts.benchmark => {
                return Err("assert is off in a benchmark run (no test is written); act, answer, or answer done".into())
            }
            GoalOp::Assert => {
                let (robust, name, _) = element()?;
                // The same preference as the screen assertions (content,
                // then the robust selector), so the two dedupe.
                let el = step.element.and_then(|n| obs.index.get(n));
                let selector = el
                    .map(|el| crate::screen::content_selectors(el, obs))
                    .unwrap_or_default()
                    .into_iter()
                    .find(|s| assertable(s, &self.user_values()))
                    .unwrap_or(robust);
                if !assertable(&selector, &self.user_values()) {
                    return Err(format!(
                        "\"{name}\" can't be asserted on: its text changes between runs or isn't unique; pick a stable element"
                    ));
                }
                Grounded {
                    action: Action::AssertVisible { selector },
                    typed: None,
                    target: Some(name),
                    scroll: None,
                }
            }
            GoalOp::Scroll => {
                let dir = content_dir().unwrap_or(Direction::Down);
                // Inside a list or picker when the model names one.
                let (within, target) = match step.element {
                    Some(_) => {
                        let (sel, name, _) = element()?;
                        (Some(sel), Some(name))
                    }
                    None => (None, None),
                };
                Grounded {
                    action: Action::Swipe {
                        direction: Some(opposite(dir)),
                        from: None,
                        to: None,
                        within,
                    },
                    typed: None,
                    target,
                    scroll: Some(dir),
                }
            }
            GoalOp::Swipe => {
                let dir =
                    content_dir().ok_or("swipe needs a direction: up, down, left or right")?;
                // Inside a carousel, chip row or picker when the model names one.
                let (within, target) = match step.element {
                    Some(_) => {
                        let (sel, name, _) = element()?;
                        (Some(sel), Some(name))
                    }
                    None => (None, None),
                };
                Grounded {
                    action: Action::Swipe {
                        direction: Some(dir),
                        from: None,
                        to: None,
                        within,
                    },
                    typed: None,
                    target,
                    scroll: None,
                }
            }
            GoalOp::Back => plain(if platform == Platform::Web {
                Action::GoBack
            } else {
                Action::PressKey { key: Key::Back }
            }),
            GoalOp::Home => plain(Action::PressKey { key: Key::Home }),
            GoalOp::Enter => plain(Action::PressKey { key: Key::Enter }),
            GoalOp::OpenApp => {
                let wanted = step.value.trim();
                let (name, app_id) = self
                    .opts
                    .apps
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(wanted))
                    .ok_or_else(|| {
                        if self.opts.apps.is_empty() {
                            "no apps are listed to open; use the screen (home, then tap the app's icon)".to_string()
                        } else {
                            format!(
                                "value must name one of the apps you may open ({})",
                                self.opts.apps.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                            )
                        }
                    })?;
                Grounded {
                    action: Action::LaunchApp {
                        app_id: app_id.clone(),
                        clear_state: false,
                    },
                    typed: None,
                    target: Some(name.clone()),
                    scroll: None,
                }
            }
            GoalOp::Launch if self.opts.benchmark => {
                return Err(
                    "launch is off in a benchmark run (there is no single app under test); use openApp or home".into(),
                )
            }
            GoalOp::Launch => plain(Action::LaunchApp {
                app_id: self.opts.app_id.clone(),
                clear_state: false,
            }),
            GoalOp::Wait => {
                if step.ms > 10_000 {
                    return Err(format!("wait {} ms is too long (at most 10000)", step.ms));
                }
                plain(Action::Wait {
                    ms: if step.ms > 0 { step.ms } else { 1000 },
                })
            }
            GoalOp::Done | GoalOp::Answer => {
                unreachable!("done and answer are handled before grounding")
            }
        })
    }

    /// Code's check on an answer: a cited element must be listed, and the
    /// answer must be grounded in text shown during the run.
    fn ground_answer(
        &self,
        step: &GoalStep,
        obs: &Observation,
    ) -> std::result::Result<values::AnswerGrounding, String> {
        let cited = match step.element {
            Some(n) => {
                let el = obs.index.get(n).ok_or_else(|| {
                    format!(
                        "element {n} is not in the list (valid: 0..{}); cite -1 if the answer was on an earlier screen",
                        obs.index.len().saturating_sub(1)
                    )
                })?;
                Some(
                    [&el.label, &el.text]
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
            None => None,
        };
        values::ground_answer(&step.value, &self.opts.goal, &self.seen, cited.as_deref())
    }

    async fn screenshot_for_check(&self) -> Option<Vec<u8>> {
        if self.opts.images {
            self.driver.screenshot(&self.opts.device_id).await.ok()
        } else {
            None
        }
    }

    async fn check_goal(&mut self, obs: &Observation, history: &[String]) -> VerifyCheck {
        let shot = self.screenshot_for_check().await;
        let checker = self.checker.clone().expect("checked at start");
        // A benchmark goal is judged on the end state ("delete X", "save the
        // note"), which a single screen can show only as a result: the claim
        // says so and carries the runner's own log of what was done.
        let claim = if self.opts.benchmark {
            end_state_claim(&self.opts.goal, history)
        } else {
            self.opts.goal.clone()
        };
        let check = velocity_nl::verify_claim(
            &*checker,
            "final",
            &claim,
            &obs.root,
            obs.rect.as_ref(),
            self.opts.platform,
            shot.as_deref(),
        )
        .await;
        self.emit_check(&check);
        check
    }

    async fn checkpoint(&mut self, obs: &Observation, claim: &str, turn: usize) -> VerifyCheck {
        let checker = self.checker.clone().expect("checked at start");
        let check = velocity_nl::verify_claim(
            &*checker,
            &format!("turn {turn}"),
            claim,
            &obs.root,
            obs.rect.as_ref(),
            self.opts.platform,
            None,
        )
        .await;
        self.emit_check(&check);
        check
    }

    fn emit_check(&mut self, check: &VerifyCheck) {
        let at = self.at();
        self.emit(ExploreEvent::Checked {
            at_ms: at,
            claim: check.claim.clone(),
            outcome: format!("{:?}", check.outcome).to_lowercase(),
            evidence: check.evidence.clone(),
            reason: check.reason.clone(),
        });
    }

    fn finish(mut self, outcome: Outcome, reason: String, turns: usize) -> ExploreReport {
        let u = self.usage();
        let cost = self.cost();
        let at = self.at();
        let reason = velocity_common::redact::redact(&reason).into_owned();
        let actions = self.report_steps.len();
        self.emit(ExploreEvent::Finished {
            at_ms: at,
            outcome,
            reason: reason.clone(),
            turns,
            actions,
            model_calls: u.calls,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cost_usd: cost,
            expectations: self.expectations,
        });
        ExploreReport {
            goal: self.opts.goal.clone(),
            app_id: self.opts.app_id.clone(),
            platform: self.opts.platform,
            launch: self.opts.launch,
            outcome,
            reason,
            steps: std::mem::take(&mut self.report_steps),
            final_assertions: std::mem::take(&mut self.final_assertions),
            checks: std::mem::take(&mut self.checks),
            turns,
            model_calls: u.calls,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cost_usd: cost,
            elapsed_ms: at,
            values: self.values.clone(),
            answer: self.answer.take(),
            prompt_version: velocity_nl::goal::GOAL_PROMPT_VERSION,
            demos: std::mem::take(&mut self.demo_names),
            expectations: self.expectations,
        }
    }
}

/// The final check's claim in a benchmark run: the screen is the end state
/// of the goal after the logged actions (code's own lines, newest last).
fn end_state_claim(goal: &str, history: &[String]) -> String {
    let log = trim_history(history);
    format!(
        "This screen is the end state after carrying out the goal \"{goal}\". \
         The runner's log of the actions already done: {log}. \
         The claim holds if this screen shows the result those actions should leave \
         (a created item listed, a deleted item gone, a value or switch in its new state, \
         the saved content shown) and nothing on it contradicts the goal."
    )
}

/// How many turns ago the current screen (the last of `screens`) was last
/// the screen, when the turn before was on another one and that was at most
/// `window` turns ago: the run left this screen and came back.
fn revisit(screens: &[u64], window: usize) -> Option<usize> {
    let (&now, earlier) = screens.split_last()?;
    let (&prev, _) = earlier.split_last()?;
    if prev == now {
        return None;
    }
    let at = earlier.iter().rposition(|&h| h == now)?;
    let ago = screens.len() - 1 - at;
    (ago <= window).then_some(ago)
}

/// The way a scroll moves content, as the model wrote it ("down" when it
/// gave none, as grounding does).
fn content_word(direction: &str) -> &str {
    match direction {
        "up" | "down" | "left" | "right" => direction,
        _ => "down",
    }
}

/// The note after an action that worked but left the screen as it was.
fn no_effect_note(op: GoalOp, label: &str) -> String {
    match op {
        GoalOp::Scroll | GoalOp::Swipe => format!(
            "Your last action ({label}) had no visible effect: the content did not move, so it may be at its end. Look elsewhere, or scroll the other way."
        ),
        _ => format!(
            "Your last action ({label}) had no visible effect: the screen is unchanged. Check you picked the right element, or try another way."
        ),
    }
}

fn opposite(d: Direction) -> Direction {
    match d {
        Direction::Up => Direction::Down,
        Direction::Down => Direction::Up,
        Direction::Left => Direction::Right,
        Direction::Right => Direction::Left,
    }
}

pub(crate) fn op_name(op: GoalOp) -> &'static str {
    let i = GoalOp::NAMES
        .iter()
        .position(|n| {
            serde_json::from_value::<GoalOp>(serde_json::Value::String((*n).into())).ok()
                == Some(op)
        })
        .unwrap_or(0);
    GoalOp::NAMES[i]
}

/// One history line for a recorded step: what was done, where it led.
fn step_line(s: &RecordedStep) -> String {
    let what = match (s.op, &s.target) {
        (GoalOp::OpenApp, Some(app)) => format!("openApp {app:?}"),
        (GoalOp::Scroll, Some(list)) => format!(
            "scroll {} inside {list:?}",
            format!("{:?}", s.scroll.unwrap_or(Direction::Down)).to_lowercase()
        ),
        (GoalOp::Swipe, Some(area)) => format!(
            "{} inside {area:?}",
            crate::yaml::action_label_named(&s.action, None)
        ),
        _ => crate::yaml::action_label_named(&s.action, s.typed.as_ref()),
    };
    if s.changed {
        format!("{what} -> \"{}\"", s.after_title)
    } else {
        format!("{what} (screen unchanged)")
    }
}

/// The history the model sees: the newest lines within the budget, with
/// the older ones counted.
fn trim_history(lines: &[String]) -> String {
    let mut kept: Vec<&String> = Vec::new();
    let mut chars = 0;
    for l in lines.iter().rev() {
        if kept.len() >= HISTORY_LINES || chars + l.len() > HISTORY_CHARS {
            break;
        }
        chars += l.len() + 1;
        kept.push(l);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    let mut out = String::new();
    if omitted > 0 {
        out.push_str(&format!("(… {omitted} earlier turns omitted)\n"));
    }
    out.push_str(
        &kept
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    velocity_common::redact::redact(&out).into_owned()
}

/// A stable element of `after` that `before` didn't have: what the test
/// asserts to know the step reached the right screen. Prefers the screen's
/// title, then an accessibility label, then a unique text, then anything
/// else (an id), top-most first; by content, not by a shared id.
fn distinctive(
    before: &Observation,
    after: &Observation,
    user_values: &[String],
) -> Option<Selector> {
    let old = before.keys();
    let platform = after.platform();
    let mut best: Option<(i32, i32, Selector)> = None;
    for n in 0..after.index.len() {
        let el = after.index.get(n)?;
        if old.contains(&crate::screen::element_key(el, platform)) {
            continue;
        }
        if el.label.is_none() && el.text.is_none() {
            continue;
        }
        // By content first: a shared id (`collapsing_toolbar`) is on every
        // page and wouldn't show which one this is.
        // And never one that also matches on the screen before: it would
        // pass without the step having done anything.
        let mut candidates = crate::screen::content_selectors(el, after);
        candidates.extend(after.selector_for(n));
        let Some(sel) = candidates
            .into_iter()
            .find(|s| assertable(s, user_values) && before.matches(s).is_empty())
        else {
            continue;
        };
        let name = element_name(el, platform);
        let rank = match &sel {
            _ if name == after.title => 0,
            Selector::AccessibilityId(_) => 1,
            Selector::Text(_) => 2,
            _ => 3,
        };
        // A long label (a paragraph of help text) reads worse in a test
        // than a short one.
        let long = i32::from(name.chars().count() > 40);
        let key = (rank + 4 * long, el.bounds.y);
        if best.as_ref().is_none_or(|b| (b.0, b.1) > key) {
            best = Some((key.0, key.1, sel));
        }
    }
    best.map(|b| b.2)
}

/// The cited evidence of a check that held, as assertable selectors.
fn evidence_selectors(
    check: &VerifyCheck,
    obs: &Observation,
    user_values: &[String],
) -> Vec<Selector> {
    // `verify_claim` lists the screen the way `obs` was built (same root,
    // rect and query), so a cited line is one of ours.
    let lines: Vec<String> = obs
        .rendered
        .lines()
        .map(|l| velocity_common::redact::redact(l).into_owned())
        .collect();
    let mut out: Vec<Selector> = Vec::new();
    for ev in &check.evidence {
        let Some(n) = lines.iter().position(|l| l == ev) else {
            continue;
        };
        let Some(el) = obs.index.get(n) else { continue };
        // Content first, as for the other assertions, so the same element
        // asserted twice dedupes.
        let mut candidates = crate::screen::content_selectors(el, obs);
        candidates.extend(obs.selector_for(n));
        if let Some(sel) = candidates
            .into_iter()
            .find(|s| assertable(s, user_values) && !out.contains(s))
        {
            out.push(sel);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_literals_become_named_values() {
        let v = goal_literals("search for \"blue shoes\" then type “size 9” and \"blue shoes\"");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "TEXT1");
        assert_eq!(v[0].value, "blue shoes");
        assert_eq!(v[1].value, "size 9");
        assert_eq!(v[0].yaml_text(), "blue shoes");
        let s = ValueSlot {
            name: "PASSWORD".into(),
            kind: ValueKind::Secret,
            value: "hunter2".into(),
        };
        assert_eq!(s.yaml_text(), "${PASSWORD}");
        assert!(!s.listing().contains("hunter2"));
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!serde_json::to_string(&s).unwrap().contains("hunter2"));
    }

    #[test]
    fn history_keeps_the_newest_lines() {
        let lines: Vec<String> = (1..=30).map(|i| format!("#{i} tap \"x\"")).collect();
        let h = trim_history(&lines);
        assert!(h.starts_with("(… 18 earlier turns omitted)"), "{h}");
        assert!(h.ends_with("#30 tap \"x\""));
        assert_eq!(h.lines().count(), HISTORY_LINES + 1);
    }

    #[test]
    fn the_benchmark_claim_carries_the_goal_and_the_log() {
        let c = end_state_claim(
            "Delete the recipe Pad Thai",
            &["#1 tap { text: \"Pad Thai\" } -> \"Pad Thai\"".into()],
        );
        assert!(c.contains("\"Delete the recipe Pad Thai\""), "{c}");
        assert!(c.contains("#1 tap"), "{c}");
        assert!(c.contains("a deleted item gone"), "{c}");
    }

    #[test]
    fn a_revisit_is_named_only_after_leaving_and_within_the_window() {
        // Left at turn 2, back at turn 4.
        assert_eq!(revisit(&[1, 1, 2, 1], 4), Some(2));
        // Staying put is not a revisit (that is the no-effect note).
        assert_eq!(revisit(&[1, 2, 2], 4), None);
        // Too long ago: the plain "seen before" note covers it.
        assert_eq!(revisit(&[1, 2, 3, 4, 5, 6, 1], 4), None);
        assert_eq!(revisit(&[1, 2, 3, 4, 1], 4), Some(4));
        assert_eq!(revisit(&[1], 4), None);
        assert_eq!(revisit(&[], 4), None);
        assert!(no_effect_note(GoalOp::Scroll, "swipe down").contains("at its end"));
        assert!(no_effect_note(GoalOp::Tap, "tap { text: \"A\" }").contains("right element"));
    }

    #[test]
    fn op_names_round_trip() {
        assert_eq!(op_name(GoalOp::Tap), "tap");
        assert_eq!(op_name(GoalOp::Done), "done");
        assert_eq!(op_name(GoalOp::InputText), "inputText");
    }
}
