//! The whole `velocity explore` pipeline as one library call: explore,
//! write the test, replay it N times with no model, and on a failed replay
//! try one repair round. The CLI, the MCP server (`run_task(kind=explore)`)
//! and the console all drive this.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use velocity_common::{PlatformDriver, Result, VelocityError};
use velocity_nl::{NaturalPlanner, ScreenChecker};

use crate::events::{EventSink, ExploreEvent, InstructionSource};
use crate::replay::{replay_once, ReplayReport};
use crate::session::{ExploreHandle, ExploreOptions, ExploreReport, ExploreSession};
use crate::yaml::{generate, generate_nl, GenOptions, Generated, Origin};

/// What a job produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    /// The session the test was written from (the repaired one, if any).
    pub report: ExploreReport,
    /// The first session, when a repair replaced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_report: Option<ExploreReport>,
    #[serde(skip)]
    pub generated: Option<Generated>,
    /// The YAML written (None on a dry run or a failed session).
    pub yaml: Option<String>,
    /// `--keep-nl`: the same session as plain-English steps.
    pub nl_yaml: Option<String>,
    /// The replays of the test that was written.
    pub replay: Option<ReplayReport>,
    /// The failed replay that started a repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_before_repair: Option<ReplayReport>,
    /// What the repair did, when one ran.
    pub repair: Option<String>,
    /// The trust gates (row 1.3), when they ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<crate::gates::GateReport>,
}

impl JobResult {
    /// The run reached the goal and (when asked) every replay passed.
    pub fn ok(&self) -> bool {
        self.report.outcome.is_success()
            && self.yaml.is_some()
            && self.replay.as_ref().is_none_or(|r| r.all_passed())
            && self.gates.as_ref().is_none_or(|g| g.trusted)
    }
}

/// Instructions shared between the first session and a repair session.
struct Shared(Arc<Mutex<Vec<Box<dyn InstructionSource>>>>);

impl InstructionSource for Shared {
    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(mut sources) = self.0.lock() {
            for s in sources.iter_mut() {
                out.extend(s.drain());
            }
        }
        out
    }
}

/// One `velocity explore` run.
pub struct ExploreJob {
    pub driver: Arc<dyn PlatformDriver>,
    pub planner: Arc<dyn NaturalPlanner>,
    pub checker: Option<Arc<dyn ScreenChecker>>,
    pub opts: ExploreOptions,
    pub gen: GenOptions,
    /// Replays after writing the test (0: none).
    pub replays: usize,
    /// One repair round when a replay fails.
    pub repair: bool,
    /// Explore only: no YAML, no replay.
    pub dry_run: bool,
    /// Also produce the plain-English version.
    pub keep_nl: bool,
    /// Run the trust gates after the replays (row 1.3; the CLI turns them
    /// on): jitter on every replay after the first, the wrong-path replay
    /// and the goal-text rule.
    pub gates: bool,
    /// The jitter's largest pause before a step, in ms.
    pub replay_jitter_ms: u64,
    /// `--after`: steps replayed with no model before exploring (a login,
    /// [`crate::after::login_prefix`]); the test starts with them too.
    pub after: Vec<crate::session::RecordedStep>,
    sink: Arc<Mutex<Option<EventSink>>>,
    sources: Arc<Mutex<Vec<Box<dyn InstructionSource>>>>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    cancel: Arc<AtomicBool>,
}

impl ExploreJob {
    pub fn new(
        driver: Arc<dyn PlatformDriver>,
        planner: Arc<dyn NaturalPlanner>,
        checker: Option<Arc<dyn ScreenChecker>>,
        opts: ExploreOptions,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        Self {
            driver,
            planner,
            checker,
            opts,
            gen: GenOptions::default(),
            replays: 1,
            repair: true,
            dry_run: false,
            keep_nl: false,
            gates: false,
            replay_jitter_ms: crate::gates::DEFAULT_JITTER_MS,
            after: Vec::new(),
            sink: Arc::new(Mutex::new(None)),
            sources: Arc::new(Mutex::new(vec![Box::new(rx)])),
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_events(self, sink: EventSink) -> Self {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
        self
    }

    pub fn with_instructions(self, source: Box<dyn InstructionSource>) -> Self {
        self.sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(source);
        self
    }

    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = flag;
        self
    }

    /// Inject instructions and stop the job from elsewhere.
    pub fn handle(&self) -> ExploreHandle {
        ExploreHandle::from_parts(self.tx.clone(), self.cancel.clone())
    }

    fn emit(&self, e: ExploreEvent) {
        if let Ok(mut s) = self.sink.lock() {
            if let Some(sink) = s.as_mut() {
                sink(&e.masked());
            }
        }
    }

    fn session(&self, opts: ExploreOptions) -> ExploreSession {
        let sink = self.sink.clone();
        ExploreSession::new(
            self.driver.clone(),
            self.planner.clone(),
            self.checker.clone(),
            opts,
        )
        .with_cancel(self.cancel.clone())
        .with_instructions(Box::new(Shared(self.sources.clone())))
        .with_events(Box::new(move |e| {
            if let Ok(mut s) = sink.lock() {
                if let Some(sink) = s.as_mut() {
                    sink(e);
                }
            }
        }))
    }

    fn model_calls(&self) -> u64 {
        self.planner.usage().calls + self.checker.as_ref().map_or(0, |c| c.usage().calls)
    }

    /// `${NAME}` values for the replay: the data and the secrets.
    fn overrides(&self) -> HashMap<String, String> {
        self.opts
            .data
            .iter()
            .chain(self.opts.secrets.iter())
            .cloned()
            .collect()
    }

    async fn replay(&self, yaml: &str, started: Instant) -> Result<ReplayReport> {
        let mut report = ReplayReport::default();
        let overrides = self.overrides();
        for run in 1..=self.replays {
            let before = self.model_calls();
            let r = replay_once(
                &*self.driver,
                yaml,
                &self.opts.device_id,
                self.opts.platform,
                &overrides,
                &self.opts.config,
                (self.gates && run > 1).then_some((self.replay_jitter_ms, run as u64)),
            )
            .await?;
            let calls = self.model_calls() - before;
            if calls > 0 {
                return Err(VelocityError::Internal(anyhow::anyhow!(
                    "the replay made {calls} model call(s); it must make none"
                )));
            }
            self.emit(ExploreEvent::ReplayRun {
                at_ms: started.elapsed().as_millis() as u64,
                run,
                passed: r.passed,
                ms: r.ms,
                steps: r.steps.len(),
                failed_step: r.failed_step,
                error: r.error.clone(),
                model_calls: calls,
            });
            let failed = !r.passed;
            report.runs.push(r);
            if failed {
                break;
            }
        }
        Ok(report)
    }

    /// Explore, write, replay, repair once if needed.
    pub async fn run(self) -> Result<JobResult> {
        let gates = self.gates;
        let driver = self.driver.clone();
        let opts = self.opts.clone();
        let overrides = self.overrides();
        let jitter = self.replay_jitter_ms;
        let mut result = self.run_inner().await?;
        if !gates {
            return Ok(result);
        }
        let (Some(yaml), Some(replay)) = (result.yaml.clone(), result.replay.as_ref()) else {
            return Ok(result);
        };
        let (runs, passed) = (replay.runs.len(), replay.passed());
        let check = crate::gates::CheckOptions {
            device_id: &opts.device_id,
            platform: opts.platform,
            overrides: &overrides,
            config: &opts.config,
            replays: runs,
            jitter_ms: jitter,
        };
        result.gates = Some(
            crate::gates::decide_gates(&*driver, &yaml, &opts.goal, runs, passed, &check).await?,
        );
        Ok(result)
    }

    async fn run_inner(self) -> Result<JobResult> {
        let started = Instant::now();
        let first = self
            .session(self.opts.clone())
            .with_prefix(self.after.clone())
            .run()
            .await?;
        let mut result = JobResult {
            report: first,
            first_report: None,
            generated: None,
            yaml: None,
            nl_yaml: None,
            replay: None,
            replay_before_repair: None,
            repair: None,
            gates: None,
        };
        if self.dry_run || !result.report.outcome.is_success() {
            return Ok(result);
        }
        let mut generated = generate(&result.report, &self.gen)?;
        if self.keep_nl {
            result.nl_yaml = Some(generate_nl(&result.report, &self.gen)?);
        }
        if self.replays == 0 {
            result.yaml = Some(generated.yaml.clone());
            result.generated = Some(generated);
            return Ok(result);
        }
        let replay = self.replay(&generated.yaml, started).await?;
        let failed_at = replay
            .runs
            .last()
            .filter(|r| !r.passed)
            .and_then(|r| r.failed_step);
        if replay.all_passed() || !self.repair {
            result.yaml = Some(generated.yaml.clone());
            result.generated = Some(generated);
            result.replay = Some(replay);
            return Ok(result);
        }

        // One repair round.
        let origin = failed_at.and_then(|k| generated.origins.get(k).copied());
        let reason = replay
            .runs
            .last()
            .and_then(|r| r.error.clone())
            .unwrap_or_else(|| "the replay failed".into());
        match origin {
            Some(Origin::ScreenAssert(i)) => {
                self.emit(ExploreEvent::Repair {
                    at_ms: started.elapsed().as_millis() as u64,
                    action: "dropped an assertion that didn't hold on replay".into(),
                    from_step: Some(i),
                    reason: reason.clone(),
                });
                result.report.steps[i].screen_assertion = None;
                result.repair = Some(format!(
                    "dropped the screen assertion after step {} ({reason})",
                    i + 1
                ));
            }
            Some(Origin::ExpectAssert(i)) => {
                self.emit(ExploreEvent::Repair {
                    at_ms: started.elapsed().as_millis() as u64,
                    action: "dropped an expectation's assertion that didn't hold on replay".into(),
                    from_step: Some(i),
                    reason: reason.clone(),
                });
                result.report.steps[i].post_assertion = None;
                result.repair = Some(format!(
                    "dropped the expectation's assertion after step {} ({reason})",
                    i + 1
                ));
            }
            Some(Origin::Final(n)) => {
                self.emit(ExploreEvent::Repair {
                    at_ms: started.elapsed().as_millis() as u64,
                    action: "dropped a final assertion that didn't hold on replay".into(),
                    from_step: None,
                    reason: reason.clone(),
                });
                if n < result.report.final_assertions.len() {
                    result.report.final_assertions.remove(n);
                }
                result.repair = Some(format!("dropped a final assertion ({reason})"));
            }
            Some(Origin::Step(i)) => {
                let mut opts = self.opts.clone();
                opts.max_steps = self
                    .opts
                    .max_steps
                    .saturating_sub(result.report.turns)
                    .max(5);
                // The repair spends what the first session left of the
                // token and cost budgets: a run capped at --max-cost X
                // cost up to 2X, as each session counted from zero.
                let spent = result.report.input_tokens + result.report.output_tokens;
                opts.max_tokens = opts.max_tokens.map(|m| m.saturating_sub(spent));
                if let (Some(max), Some(cost)) = (opts.max_cost_usd, result.report.cost_usd) {
                    opts.max_cost_usd = Some((max - cost).max(0.0));
                }
                if opts.max_tokens == Some(0) || opts.max_cost_usd.is_some_and(|c| c <= 0.0) {
                    result.repair = Some(format!(
                        "not repairable: the token or cost budget is spent ({reason})"
                    ));
                    result.yaml = Some(generated.yaml.clone());
                    result.generated = Some(generated);
                    result.replay = Some(replay);
                    return Ok(result);
                }
                self.emit(ExploreEvent::Repair {
                    at_ms: started.elapsed().as_millis() as u64,
                    action: "exploring on from the step before the failure".into(),
                    from_step: Some(i),
                    reason: reason.clone(),
                });
                let prefix = result.report.steps[..i].to_vec();
                let again = self.session(opts).with_prefix(prefix).run().await?;
                result.repair = Some(format!(
                    "re-explored from step {} ({reason}): {}",
                    i + 1,
                    again.outcome
                ));
                let first = std::mem::replace(&mut result.report, again);
                result.first_report = Some(first);
                if !result.report.outcome.is_success() {
                    result.replay_before_repair = Some(replay);
                    return Ok(result);
                }
            }
            Some(Origin::Launch) | None => {
                result.repair = Some(format!("not repairable: {reason}"));
                result.yaml = Some(generated.yaml.clone());
                result.generated = Some(generated);
                result.replay = Some(replay);
                return Ok(result);
            }
        }
        generated = generate(&result.report, &self.gen)?;
        if self.keep_nl {
            result.nl_yaml = Some(generate_nl(&result.report, &self.gen)?);
        }
        let second = self.replay(&generated.yaml, started).await?;
        result.yaml = Some(generated.yaml.clone());
        result.generated = Some(generated);
        result.replay = Some(second);
        result.replay_before_repair = Some(replay);
        Ok(result)
    }
}
