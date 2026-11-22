//! A suite run (`velocity run`) or an exploration (`velocity explore`) as
//! a child process (see the module docs of [`super`] for why a child).
//!
//! The child writes its output to the task's `run.log`, its reports and
//! traces under the task's `artifacts/`, and live events (one JSON line per
//! test and step, and per explore turn: `velocity_common::live_events`) to
//! `events.jsonl`, which this side reads while it runs. Stopping it is a
//! Ctrl-C to its process group: the run stops its test and writes its
//! reports, as a Ctrl-C in a terminal does; one that doesn't exit within a
//! grace period is killed. An exploration also gets `task_inject`
//! instructions, one line each, through its `--inject-file`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use velocity_common::{Platform, SuiteResult};

use super::{Failure, Progress, Summary, TaskBody, TaskContext, TaskKind, TaskOutcome, TaskState};

/// How long a stopped run may take to write its reports.
#[cfg(not(test))]
const STOP_GRACE: Duration = Duration::from_secs(30);
/// Short under test, so a child that ignores the Ctrl-C is killed soon (but
/// not so soon that a loaded machine kills one that is writing its reports).
#[cfg(test)]
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often progress is read.
const POLL: Duration = Duration::from_millis(400);

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub suite: PathBuf,
    pub device: Option<String>,
    pub platform: Option<Platform>,
    /// `--filter` glob over test names.
    pub filter: Option<String>,
    pub tags: Vec<String>,
    pub retries: Option<u32>,
    /// Keep a screenshot after every step (written for failing tests).
    pub step_screenshots: bool,
    /// Keep every test's trace, not only failures'.
    pub trace_always: bool,
}

impl RunRequest {
    /// `velocity run` arguments writing into `artifacts`.
    pub fn args(&self, artifacts: &Path) -> Vec<String> {
        let mut a = vec![
            "run".to_string(),
            self.suite.to_string_lossy().into_owned(),
            "--artifacts-dir".into(),
            artifacts.to_string_lossy().into_owned(),
            "--report".into(),
            "json".into(),
            "--trace".into(),
            if self.trace_always {
                "always"
            } else {
                "on-failure"
            }
            .into(),
        ];
        if let Some(d) = &self.device {
            a.extend(["--device".into(), d.clone()]);
        }
        if let Some(p) = self.platform {
            a.extend(["--platform".into(), p.to_string().to_lowercase()]);
        }
        if let Some(f) = &self.filter {
            a.extend(["--filter".into(), f.clone()]);
        }
        if !self.tags.is_empty() {
            a.extend(["--tags".into(), self.tags.join(",")]);
        }
        if let Some(r) = self.retries {
            a.extend(["--retries".into(), r.to_string()]);
        }
        if self.step_screenshots {
            a.extend(["--set".into(), "artifacts.stepScreenshots=true".into()]);
        }
        a
    }
}

/// What to explore (`velocity explore` arguments).
#[derive(Debug, Clone, Default)]
pub struct ExploreRequest {
    pub goal: String,
    pub app_id: String,
    pub device: Option<String>,
    pub platform: Option<Platform>,
    /// `provider:model`; else the configured provider's goal model.
    pub model: Option<String>,
    /// A fixed plan instead of a model (`--script`).
    pub script: Option<PathBuf>,
    /// Values the agent may type, by name.
    pub data: Vec<(String, String)>,
    /// Secrets the agent may type, by name (read from the environment).
    pub secrets: Vec<String>,
    pub max_steps: Option<usize>,
    /// `off`, `final`, `checkpoints` or `strict`.
    pub verify: Option<String>,
    pub replay: Option<usize>,
    /// Where the test is written.
    pub out: PathBuf,
    /// `--after`: a suite whose first test (a login) is replayed with no
    /// model before exploring.
    pub after: Option<PathBuf>,
}

impl ExploreRequest {
    /// `velocity explore` arguments reading instructions from `inject`.
    pub fn args(&self, inject: &Path) -> Vec<String> {
        let mut a = vec![
            "explore".to_string(),
            "--app-id".into(),
            self.app_id.clone(),
            "--json".into(),
            "--out".into(),
            self.out.to_string_lossy().into_owned(),
            "--inject-file".into(),
            inject.to_string_lossy().into_owned(),
        ];
        if let Some(d) = &self.device {
            a.extend(["--device".into(), d.clone()]);
        }
        if let Some(p) = self.platform {
            a.extend(["--platform".into(), p.to_string().to_lowercase()]);
        }
        if let Some(m) = &self.model {
            a.extend(["--model".into(), m.clone()]);
        }
        if let Some(s) = &self.script {
            a.extend(["--script".into(), s.to_string_lossy().into_owned()]);
        }
        for (k, v) in &self.data {
            a.extend(["--data".into(), format!("{k}={v}")]);
        }
        for s in &self.secrets {
            a.extend(["--secret".into(), s.clone()]);
        }
        if let Some(n) = self.max_steps {
            a.extend(["--max-steps".into(), n.to_string()]);
        }
        if let Some(v) = &self.verify {
            a.extend(["--verify".into(), v.clone()]);
        }
        if let Some(n) = self.replay {
            a.extend(["--replay".into(), n.to_string()]);
        }
        if let Some(p) = &self.after {
            a.extend(["--after".into(), p.to_string_lossy().into_owned()]);
        }
        // The goal is model text: last, after `--`, so `--values-from-model`
        // in it is never read as a flag (audit 2026-11; console I-2).
        a.extend(["--".into(), self.goal.clone()]);
        a
    }
}

/// A `velocity` child: what both task bodies share.
pub struct Child {
    /// The `velocity` binary (the server's own, normally).
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The child's working directory (relative paths resolve here).
    pub cwd: Option<PathBuf>,
    /// Its stderr, and its stdout unless `stdout` is set.
    pub log: PathBuf,
    /// Its whole environment, when it may not inherit this process's (a
    /// suite an agent wrote: `crate::authored::child_env`); `None`
    /// inherits it.
    pub env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

/// How a child ended.
struct Exit {
    code: Option<i32>,
    stopped: bool,
}

impl Child {
    /// Start it, publish progress from `events` while it runs, append
    /// instructions to `inject` (when given), and stop it on request.
    async fn supervise(
        &self,
        ctx: &mut TaskContext,
        events: &Path,
        stdout: Option<&Path>,
        inject: Option<&Path>,
    ) -> Result<Exit, String> {
        let log = std::fs::File::create(&self.log)
            .map_err(|e| format!("log {}: {e}", self.log.display()))?;
        let out = match stdout {
            Some(p) => std::fs::File::create(p),
            None => log.try_clone(),
        }
        .map_err(|e| format!("log {}: {e}", self.log.display()))?;
        let mut cmd = tokio::process::Command::new(&self.program);
        if let Some(vars) = &self.env {
            cmd.env_clear().envs(vars.iter().map(|(k, v)| (k, v)));
        }
        cmd.args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(log))
            .env(velocity_common::live_events::EVENTS_FILE_ENV, events)
            .env("NO_COLOR", "1");
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // A stop is a SIGINT to the child's group, so the child must not
        // inherit "ignore SIGINT": a server started in the background
        // (`nohup`, a `&` job of a script) ignores it, and the stop would
        // wait out the grace period and end in a kill.
        #[cfg(unix)]
        // SAFETY: the closure only calls signal(2), which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = velocity_common::process::spawn_group(&mut cmd)
            .map_err(|e| format!("could not start {}: {e}", self.program.display()))?;
        let pid = child.id();
        let mut stopped = false;
        let mut stop_deadline: Option<tokio::time::Instant> = None;
        let mut tick = tokio::time::interval(POLL);
        let mut instructions = ctx.instructions.take();
        let mut tail = ProgressTail::default();
        let status = loop {
            tokio::select! {
                s = child.wait() => break s,
                _ = tick.tick() => {
                    ctx.set_progress(tail.read(events, progress_now(ctx)));
                    if stop_deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                        kill_group(pid);
                        let _ = child.start_kill();
                    }
                }
                changed = ctx.stop.changed(), if !stopped => {
                    if changed.is_err() || *ctx.stop.borrow() {
                        stopped = true;
                        interrupt(pid);
                        stop_deadline = Some(tokio::time::Instant::now() + STOP_GRACE);
                    }
                }
                Some(i) = async {
                    match instructions.as_mut() {
                        Some(r) => r.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(path) = inject {
                        append_line(path, &i.text);
                    }
                }
            }
        };
        ctx.set_progress(tail.read(events, progress_now(ctx)));
        Ok(Exit {
            code: status.ok().and_then(|s| s.code()),
            stopped,
        })
    }

    fn failure(&self, what: &str, code: Option<i32>) -> String {
        format!(
            "{what} exited with {}: {}",
            code.map_or("a signal".to_string(), |c| format!("code {c}")),
            log_tail(&self.log, 12)
        )
    }
}

/// One instruction as one line (the child reads complete lines).
fn append_line(path: &Path, text: &str) {
    use std::io::Write;
    let line = text.replace(['\n', '\r'], " ");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(format!("{}\n", line.trim()).as_bytes());
    }
}

fn error_outcome(e: String) -> TaskOutcome {
    TaskOutcome {
        state: TaskState::Error,
        summary: None,
        error: Some(e),
        exit_code: None,
    }
}

/// A `velocity run` child.
pub struct SuiteRun {
    pub child: Child,
    pub artifacts: PathBuf,
}

#[async_trait::async_trait]
impl TaskBody for SuiteRun {
    fn kind(&self) -> TaskKind {
        TaskKind::Run
    }

    async fn run(self: Box<Self>, mut ctx: TaskContext) -> TaskOutcome {
        let events = ctx.dir.join("events.jsonl");
        let exit = match self.child.supervise(&mut ctx, &events, None, None).await {
            Ok(e) => e,
            Err(e) => return error_outcome(e),
        };
        let result = read_results(&self.artifacts.join("results.json"));
        let summary = result.as_ref().map(Summary::of);
        let state = if exit.stopped {
            TaskState::Stopped
        } else {
            match exit.code {
                Some(0) => TaskState::Passed,
                Some(1) => TaskState::Failed,
                _ => TaskState::Error,
            }
        };
        let error =
            (state == TaskState::Error).then(|| self.child.failure("velocity run", exit.code));
        TaskOutcome {
            state,
            summary,
            error,
            exit_code: exit.code,
        }
    }
}

/// A `velocity explore … --json` child. Its stdout (the JSON result) goes to
/// `<task>/result.json`, its live lines to the log.
pub struct ExploreRun {
    pub child: Child,
    /// The file `task_inject` lines are appended to (the child's
    /// `--inject-file`).
    pub inject: PathBuf,
}

#[async_trait::async_trait]
impl TaskBody for ExploreRun {
    fn kind(&self) -> TaskKind {
        TaskKind::Explore
    }

    async fn run(self: Box<Self>, mut ctx: TaskContext) -> TaskOutcome {
        let events = ctx.dir.join("events.jsonl");
        let result_file = ctx.dir.join("result.json");
        // Created empty first: the child reads only lines appended later.
        if let Err(e) = std::fs::write(&self.inject, "") {
            return error_outcome(format!("{}: {e}", self.inject.display()));
        }
        let exit = match self
            .child
            .supervise(&mut ctx, &events, Some(&result_file), Some(&self.inject))
            .await
        {
            Ok(e) => e,
            Err(e) => return error_outcome(e),
        };
        let result = std::fs::read(&result_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let summary = result.as_ref().map(explore_summary);
        let state = if exit.stopped {
            TaskState::Stopped
        } else {
            match exit.code {
                Some(0) => TaskState::Passed,
                // 1: not reached or a replay failed; 4: out of time.
                Some(1 | 4) if result.is_some() => TaskState::Failed,
                _ => TaskState::Error,
            }
        };
        let error = match state {
            TaskState::Error => Some(self.child.failure("velocity explore", exit.code)),
            TaskState::Failed => result
                .as_ref()
                .and_then(|r| r.pointer("/report/reason"))
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            _ => None,
        };
        TaskOutcome {
            state,
            summary,
            error,
            exit_code: exit.code,
        }
    }
}

/// A `velocity verify-change` child (row 1.4). Its verdict and evidence
/// are in `<bundle>/manifest.json`.
pub struct VerifyRun {
    pub child: Child,
    pub bundle: PathBuf,
}

#[async_trait::async_trait]
impl TaskBody for VerifyRun {
    fn kind(&self) -> TaskKind {
        TaskKind::Verify
    }

    async fn run(self: Box<Self>, mut ctx: TaskContext) -> TaskOutcome {
        let events = ctx.dir.join("events.jsonl");
        let exit = match self.child.supervise(&mut ctx, &events, None, None).await {
            Ok(e) => e,
            Err(e) => return error_outcome(e),
        };
        let manifest = std::fs::read(self.bundle.join("manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let passed = manifest.as_ref().is_some_and(|m| m["verdict"] == "passed");
        let summary = manifest.as_ref().map(|m| Summary {
            total: 1,
            passed: usize::from(passed),
            failed: usize::from(!passed),
            explore: Some(m.clone()),
            ..Default::default()
        });
        let state = if exit.stopped {
            TaskState::Stopped
        } else {
            match exit.code {
                Some(0) => TaskState::Passed,
                Some(1) if manifest.is_some() => TaskState::Failed,
                _ => TaskState::Error,
            }
        };
        let error = match state {
            TaskState::Error => Some(self.child.failure("velocity verify-change", exit.code)),
            TaskState::Failed => std::fs::read_to_string(self.bundle.join("failures.md")).ok(),
            _ => None,
        };
        TaskOutcome {
            state,
            summary,
            error,
            exit_code: exit.code,
        }
    }
}

/// `velocity gate --json` as a task: the trust gates on a written test.
pub struct GateRun {
    pub child: Child,
}

#[async_trait::async_trait]
impl TaskBody for GateRun {
    fn kind(&self) -> TaskKind {
        TaskKind::Gate
    }

    async fn run(self: Box<Self>, mut ctx: TaskContext) -> TaskOutcome {
        let events = ctx.dir.join("events.jsonl");
        let result_file = ctx.dir.join("result.json");
        let exit = match self
            .child
            .supervise(&mut ctx, &events, Some(&result_file), None)
            .await
        {
            Ok(e) => e,
            Err(e) => return error_outcome(e),
        };
        let result = std::fs::read(&result_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        let summary = result.as_ref().map(gate_summary);
        let state = if exit.stopped {
            TaskState::Stopped
        } else {
            match exit.code {
                Some(0) => TaskState::Passed,
                // 1: the test is not trusted; the reasons are in the result.
                Some(1) if result.is_some() => TaskState::Failed,
                _ => TaskState::Error,
            }
        };
        let error = match state {
            TaskState::Error => Some(self.child.failure("velocity gate", exit.code)),
            TaskState::Failed => result.as_ref().map(|r| {
                let reasons: Vec<&str> = r["gates"]["reasons"]
                    .as_array()
                    .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                    .unwrap_or_default();
                format!("not trusted: {}", reasons.join("; "))
            }),
            _ => None,
        };
        TaskOutcome {
            state,
            summary,
            error,
            exit_code: exit.code,
        }
    }
}

/// The summary of `velocity gate --json` output: the burn-in replays as the
/// counts, failed replays as failures, and the gates' verdict.
pub fn gate_summary(r: &serde_json::Value) -> Summary {
    use serde_json::json;
    let runs = r["replay"]["runs"].as_array().cloned().unwrap_or_default();
    let passed = runs.iter().filter(|x| x["passed"] == true).count();
    let failures = runs
        .iter()
        .enumerate()
        .filter(|(_, x)| x["passed"] != true)
        .map(|(i, x)| Failure {
            test: format!("replay {}", i + 1),
            step: x["failedStep"].as_u64().map(|s| format!("{}", s + 1)),
            error: x["error"]
                .as_str()
                .map(|e| velocity_common::redact::redact(e).into_owned()),
            screenshot: None,
            trace: None,
        })
        .collect();
    Summary {
        total: runs.len(),
        passed,
        failed: runs.len() - passed,
        failures,
        gates: Some(json!({
            "trusted": r["trusted"],
            "gates": r["gates"],
            "file": r["file"],
            "goal": r["goal"],
        })),
        ..Default::default()
    }
}

/// The summary of `velocity explore --json` output: the test written, the
/// replays, and what the model cost.
pub fn explore_summary(r: &serde_json::Value) -> Summary {
    use serde_json::{json, Value};
    let rep = &r["report"];
    let runs = r["replay"]["runs"].as_array().cloned().unwrap_or_default();
    let passed = runs.iter().filter(|x| x["passed"] == true).count();
    let failures = runs
        .iter()
        .enumerate()
        .filter(|(_, x)| x["passed"] != true)
        .map(|(i, x)| Failure {
            test: format!("replay {}", i + 1),
            step: x["failedStep"].as_u64().map(|s| format!("{}", s + 1)),
            error: x["error"]
                .as_str()
                .map(|e| velocity_common::redact::redact(e).into_owned()),
            screenshot: None,
            trace: None,
        })
        .collect();
    let files: Vec<Value> = r["files"].as_array().cloned().unwrap_or_default();
    let explore = json!({
        "outcome": rep["outcome"],
        "reason": rep["reason"],
        "yaml": files.iter().find(|f| f.as_str().is_some_and(|f| !f.ends_with(".nl.yaml"))),
        "files": files,
        "turns": rep["turns"],
        "actions": rep["steps"].as_array().map_or(0, Vec::len),
        "model_calls": rep["modelCalls"],
        "input_tokens": rep["inputTokens"],
        "output_tokens": rep["outputTokens"],
        "cost_usd": rep["costUsd"],
        "elapsed_ms": rep["elapsedMs"],
        "replay": r["replay"].as_object().map(|_| json!({
            "runs": runs.len(),
            "passed": passed,
            "ms": runs.iter().map(|x| x["ms"].clone()).collect::<Vec<_>>(),
            "model_calls": r["replay"]["modelCalls"],
        })),
        "repair": r["repair"],
        "next": files.first().and_then(Value::as_str).map(|f| format!(
            "review {f}, then run it with `velocity run {f}` (run_task suite={f}); commit it for CI"
        )),
    });
    Summary {
        total: runs.len(),
        passed,
        failed: runs.len() - passed,
        failures,
        explore: Some(explore),
        ..Default::default()
    }
}

/// The progress published so far (to keep `tests_total`).
fn progress_now(ctx: &TaskContext) -> Progress {
    super::lock(&ctx.shared)
        .tasks
        .get(&ctx.id)
        .map(|e| e.info.progress.clone())
        .unwrap_or_default()
}

/// Ctrl-C to the child's process group (it leads its own group).
fn interrupt(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: plain kill(2) on the group the child leads.
        unsafe {
            libc::kill(-pid, libc::SIGINT);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// SIGKILL to the child's whole process group: what it started (Chrome
/// runs in `velocity`'s own group) must not outlive it. Killing only the
/// leader orphaned them, and once the leader is reaped nothing else will.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) {
        // SAFETY: plain kill(2) on the group the child leads.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Progress from the events file: the test and step running now, how many
/// test attempts finished (and failed), and for an exploration its turn,
/// last operation and phase (`exploring`, `explored` once the session has
/// ended, `replaying` from its first replay's test, `repairing`).
pub fn read_progress(file: &Path, base: Progress) -> Progress {
    ProgressTail::default().read(file, base)
}

/// [`read_progress`] that reads only what was appended since its last
/// read: the supervisor polls every 400 ms on the server's runtime, and a
/// long exploration's events file grows to megabytes (review 01 #7).
#[derive(Default)]
struct ProgressTail {
    /// Bytes folded in so far.
    offset: u64,
    /// The start of a line not yet complete.
    partial: Vec<u8>,
    /// Progress from the lines folded in so far; `None` before any read.
    progress: Option<Progress>,
}

impl ProgressTail {
    fn read(&mut self, file: &Path, base: Progress) -> Progress {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(file) else {
            return self.progress.clone().unwrap_or(base);
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Replaced or truncated: start over.
            *self = Self::default();
        }
        let p = self.progress.get_or_insert_with(|| Progress {
            tests_total: base.tests_total,
            ..Default::default()
        });
        let mut new = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).is_ok() && f.read_to_end(&mut new).is_ok() {
            self.offset += new.len() as u64;
            self.partial.extend_from_slice(&new);
            if let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') {
                let done: Vec<u8> = self.partial.drain(..=end).collect();
                for line in String::from_utf8_lossy(&done).lines() {
                    apply_event(p, line);
                }
            }
        }
        let mut out = p.clone();
        // The last line may be complete without its newline yet; read it
        // as the whole-file read did, without consuming it.
        if !self.partial.is_empty() {
            apply_event(&mut out, &String::from_utf8_lossy(&self.partial));
        }
        out
    }
}

/// Fold one events line into `p`.
fn apply_event(p: &mut Progress, line: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let s = |k: &str| v[k].as_str().map(String::from);
    let n = |k: &str| v[k].as_u64().map(|n| n as usize);
    match v["event"].as_str() {
        Some("testStarted") => {
            // An exploration's tests are its replays.
            if p.phase.is_some() {
                p.phase = Some("replaying".into());
            }
            p.test = s("test");
            p.steps = n("steps");
            p.step = None;
            p.label = None;
        }
        Some("stepStarted") => {
            p.test = s("test");
            p.step = n("index").map(|i| i + 1);
            p.steps = n("total");
            p.label = s("step");
        }
        Some("testFinished") => {
            p.tests_finished += 1;
            // The writer's own type decides what a failure is (review 02
            // M4): a status this reader doesn't know counts as failed.
            let failed = match velocity_common::live_events::LiveEvent::parse(line) {
                Some(velocity_common::live_events::LiveEvent::TestFinished { status, .. }) => {
                    status.is_failure()
                }
                _ => true,
            };
            if failed {
                p.tests_failed += 1;
            }
        }
        Some("observed") => {
            p.phase = Some("exploring".into());
            p.turn = n("turn");
        }
        Some("planned") => {
            p.turn = n("turn");
            p.op = s("op");
            p.target = s("target");
        }
        Some("acted") => p.label = s("action"),
        Some("replayRun") => p.phase = Some("replaying".into()),
        Some("repair") => p.phase = Some("repairing".into()),
        Some("finished") => p.phase = Some("explored".into()),
        _ => {}
    }
}

fn read_results(path: &Path) -> Option<SuiteResult> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The last `n` lines of the log, masked. Only its end is read, and bytes
/// that aren't UTF-8 (a device tool's output) are replaced, not fatal.
fn log_tail(log: &Path, n: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    let mut cut = false;
    if let Ok(mut f) = std::fs::File::open(log) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        cut = len > WINDOW;
        if f.seek(SeekFrom::Start(len.saturating_sub(WINDOW))).is_ok() {
            let _ = f.take(WINDOW).read_to_end(&mut bytes);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // The window may begin inside a line.
    if cut && lines.len() > 1 {
        lines.remove(0);
    }
    let tail = lines[lines.len().saturating_sub(n)..].join("\n");
    velocity_common::redact::redact(&tail).into_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tasks::{TaskManager, TaskMeta};

    /// A stand-in for `velocity` (run through /bin/sh): writes progress
    /// and results.json like a run, sleeps `$FAKE_SLEEP` seconds, then exits
    /// with `$FAKE_EXIT`; on Ctrl-C it writes its results and exits 130.
    fn fake_velocity() -> PathBuf {
        let results = r#"{"total":2,"passed":1,"failed":1,"skipped":0,"retried":0,"duration":{"secs":1,"nanos":0},"tests":[{"test_name":"ok","status":"passed","duration":{"secs":1,"nanos":0},"steps":[],"retries":0,"error_message":null,"screenshots":[]},{"test_name":"pay","status":"failed","duration":{"secs":1,"nanos":0},"steps":[{"step_index":2,"action_name":"tap","status":"failed","duration":{"secs":0,"nanos":0},"screenshot":"/tmp/x.png","error_message":"Element not found: id=pay"}],"retries":0,"error_message":"Element not found: id=pay","screenshots":[]}],"shard_index":null,"shard_total":null}"#;
        crate::test_util::script(
            "fake-velocity",
            &format!(
                r#"#!/bin/sh
all="$@"
art=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--artifacts-dir" ]; then art="$2"; fi
  shift
done
mkdir -p "$art"
echo "$all" > "$art/../argv.txt"
finish() {{ printf '%s' '{results}' > "$art/results.json"; exit $1; }}
trap 'finish 130' INT
echo '{{"event":"testStarted","test":"ok","steps":3}}' >> "$VELOCITY_EVENTS_FILE"
echo '{{"event":"stepStarted","test":"ok","index":1,"total":3,"step":"tap"}}' >> "$VELOCITY_EVENTS_FILE"
echo "running on stdout"
i=0
while [ $i -lt "$FAKE_SLEEP" ]; do sleep 1; i=$((i+1)); done
echo '{{"event":"testFinished","test":"ok","status":"passed"}}' >> "$VELOCITY_EVENTS_FILE"
finish "$FAKE_EXIT"
"#
            ),
        )
    }

    fn start(m: &TaskManager, exit: i32, sleep_s: u32) -> crate::tasks::TaskInfo {
        let (id, dir) = m.new_task_dir().unwrap();
        let artifacts = dir.join("artifacts");
        let req = RunRequest {
            suite: "suite.yaml".into(),
            device: Some("emulator-5554".into()),
            platform: Some(Platform::Android),
            filter: Some("pay*".into()),
            step_screenshots: true,
            ..Default::default()
        };
        // Through sh, not exec'd directly: another test thread's fork can
        // hold the fresh script open for writing (ETXTBSY).
        let mut args = vec![
            format!("FAKE_EXIT={exit}"),
            format!("FAKE_SLEEP={sleep_s}"),
            "/bin/sh".to_string(),
            fake_velocity().to_string_lossy().into_owned(),
        ];
        args.extend(req.args(&artifacts));
        let body = SuiteRun {
            child: Child {
                program: PathBuf::from("/usr/bin/env"),
                args,
                cwd: None,
                log: dir.join("run.log"),
                env: None,
            },
            artifacts: artifacts.clone(),
        };
        let meta = TaskMeta {
            target: "suite.yaml".into(),
            device: req.device.clone(),
            log: dir.join("run.log"),
            artifacts,
            dir,
            tests_total: Some(2),
        };
        m.spawn(id, meta, Box::new(body))
    }

    // Review 01 #7: progress reads only the appended bytes, and a line
    // written in two pieces counts once.
    #[test]
    fn progress_is_read_from_where_the_last_read_stopped() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("events.jsonl");
        let mut f = std::fs::File::create(&file).unwrap();
        let mut tail = ProgressTail::default();
        let base = Progress {
            tests_total: Some(2),
            ..Default::default()
        };
        assert_eq!(tail.read(&file, base.clone()).tests_finished, 0);
        writeln!(
            f,
            r#"{{"event":"testFinished","test":"a","status":"passed"}}"#
        )
        .unwrap();
        write!(f, r#"{{"event":"testFinished","test":"b","#).unwrap();
        f.flush().unwrap();
        let p = tail.read(&file, base.clone());
        assert_eq!((p.tests_finished, p.tests_total), (1, Some(2)));
        writeln!(f, r#""status":"failed"}}"#).unwrap();
        f.flush().unwrap();
        let p = tail.read(&file, base.clone());
        assert_eq!((p.tests_finished, p.tests_failed), (2, 1));
        assert_eq!(
            tail.read(&file, base.clone()).tests_finished,
            2,
            "no double count"
        );
        assert_eq!(read_progress(&file, base).tests_finished, 2);
    }

    // Review 05 #1: a child given an environment sees only that.
    #[tokio::test]
    async fn a_child_with_its_own_environment_inherits_nothing_else() {
        // A variable every parent has, read rather than set: `set_var` in
        // the library's test binary races other tests (review 07 M7).
        let (parent_only, _) = std::env::vars()
            .find(|(k, v)| (k == "HOME" || k == "PATH") && !v.is_empty())
            .expect("HOME or PATH is set");
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let (id, dir) = m.new_task_dir().unwrap();
        let artifacts = dir.join("artifacts");
        let body = SuiteRun {
            child: Child {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![],
                cwd: None,
                log: dir.join("run.log"),
                env: Some(vec![("VEL_MCP_KEPT".into(), "kept".into())]),
            },
            artifacts: artifacts.clone(),
        };
        let meta = TaskMeta {
            target: "suite.yaml".into(),
            device: None,
            log: dir.join("run.log"),
            artifacts,
            dir,
            tests_total: None,
        };
        let info = m.spawn(id, meta, Box::new(body));
        let done = m.wait(&info.id, Duration::from_secs(30)).await.unwrap();
        let log = std::fs::read_to_string(&done.log).unwrap();
        assert!(log.contains("VEL_MCP_KEPT=kept"), "{log}");
        assert!(log.contains("VELOCITY_EVENTS_FILE="), "{log}");
        assert!(!log.contains(&format!("{parent_only}=")), "{log}");
    }

    #[test]
    fn run_arguments_are_argv_not_a_shell_line() {
        let req = RunRequest {
            suite: "my suite.yaml".into(),
            filter: Some("a; rm -rf /".into()),
            tags: vec!["smoke".into(), "fast".into()],
            retries: Some(1),
            ..Default::default()
        };
        let a = req.args(Path::new("/tmp/art"));
        assert_eq!(a[1], "my suite.yaml");
        assert!(a.windows(2).any(|w| w == ["--filter", "a; rm -rf /"]));
        assert!(a.windows(2).any(|w| w == ["--tags", "smoke,fast"]));
        assert!(a.windows(2).any(|w| w == ["--trace", "on-failure"]));
    }

    #[tokio::test]
    async fn a_failing_run_reports_a_machine_readable_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let info = start(&m, 1, 2);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while m.status(&info.id).unwrap().progress.step.is_none()
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mid = m.status(&info.id).unwrap();
        assert_eq!(mid.state, TaskState::Running);
        assert_eq!(mid.progress.step, Some(2), "{:?}", mid.progress);
        assert_eq!(mid.progress.steps, Some(3));
        assert_eq!(mid.progress.tests_total, Some(2));
        let done = m.wait(&info.id, Duration::from_secs(90)).await.unwrap();
        assert_eq!(done.state, TaskState::Failed, "{done:?}");
        let s = done.summary.unwrap();
        assert_eq!((s.passed, s.failed, s.flaky), (1, 1, 0));
        assert_eq!(s.failures[0].test, "pay");
        assert_eq!(s.failures[0].step.as_deref(), Some("3: tap"));
        assert_eq!(done.progress.tests_finished, 1);
        let log = std::fs::read_to_string(&done.log).unwrap();
        assert!(
            log.contains("running on stdout"),
            "child output goes to the log"
        );
        let argv = std::fs::read_to_string(done.dir.join("argv.txt")).unwrap();
        assert!(argv.contains("--filter pay*"), "{argv}");
        assert!(argv.contains("artifacts.stepScreenshots=true"), "{argv}");
    }

    #[tokio::test]
    async fn a_stopped_run_is_interrupted_and_keeps_its_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let info = start(&m, 0, 30);
        // Stop once it is running (its first progress line is out).
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while m.status(&info.id).unwrap().progress.step.is_none()
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        m.stop(&info.id).unwrap();
        let done = m.wait(&info.id, Duration::from_secs(40)).await.unwrap();
        assert_eq!(done.state, TaskState::Stopped, "{done:?}");
        assert_eq!(done.exit_code, Some(130));
        assert!(
            done.summary.is_some(),
            "the interrupted run wrote results.json"
        );
    }

    // A server started in the background (`nohup`, a `&` job of a script)
    // ignores SIGINT, and a child inherits that: the stop (a SIGINT to its
    // group) then waited out the grace period and ended in a kill.
    #[tokio::test]
    async fn a_stop_interrupts_the_child_when_the_server_ignores_sigint() {
        struct Restore(libc::sighandler_t);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: puts back the disposition `signal` returned.
                unsafe { libc::signal(libc::SIGINT, self.0) };
            }
        }
        // SAFETY: signal(2) with SIG_IGN; undone by `Restore`.
        let _restore = Restore(unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) });
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let info = start(&m, 0, 30);
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while m.status(&info.id).unwrap().progress.step.is_none()
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        m.stop(&info.id).unwrap();
        let done = m.wait(&info.id, Duration::from_secs(10)).await.unwrap();
        assert_eq!(done.state, TaskState::Stopped, "{done:?}");
        assert_eq!(done.exit_code, Some(130), "the child saw the Ctrl-C");
    }

    // The kill after the grace period takes the child's whole group: what
    // it started (a browser, a helper) must not outlive it.
    #[tokio::test]
    async fn a_killed_run_leaves_nothing_running_in_its_group() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let (id, dir) = m.new_task_dir().unwrap();
        let pid_file = tmp.path().join("helper.pid");
        let script = crate::test_util::script(
            "fake-velocity-stubborn",
            "#!/bin/sh\ntrap '' INT\nsleep 300 &\necho $! > \"$HELPER_PID_FILE\"\nwait\n",
        );
        let body = SuiteRun {
            child: Child {
                program: PathBuf::from("/usr/bin/env"),
                args: vec![
                    format!("HELPER_PID_FILE={}", pid_file.display()),
                    "/bin/sh".into(),
                    script.to_string_lossy().into_owned(),
                ],
                cwd: None,
                log: dir.join("run.log"),
                env: None,
            },
            artifacts: dir.join("artifacts"),
        };
        let meta = TaskMeta {
            target: "suite.yaml".into(),
            device: None,
            log: dir.join("run.log"),
            artifacts: dir.join("artifacts"),
            dir,
            tests_total: None,
        };
        let info = m.spawn(id, meta, Box::new(body));
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let helper = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never started"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        // The child ignores the Ctrl-C: it is killed after the grace period.
        m.stop(&info.id).unwrap();
        let done = m.wait(&info.id, Duration::from_secs(20)).await.unwrap();
        assert_eq!(done.state, TaskState::Stopped, "{done:?}");
        let alive = || {
            // SAFETY: signal 0 only checks that the process exists.
            unsafe { libc::kill(helper, 0) == 0 }
        };
        let gone = std::time::Instant::now() + Duration::from_secs(3);
        while alive() && std::time::Instant::now() < gone {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let survived = alive();
        if survived {
            // SAFETY: the helper this test started; don't leave it behind.
            unsafe { libc::kill(helper, libc::SIGKILL) };
        }
        assert!(!survived, "the child's helper outlived the kill");
    }

    // The log tail is the error a task reports: bytes that aren't UTF-8 (an
    // adb or simctl line) must not blank it, and a huge log isn't read whole.
    #[test]
    fn the_log_tail_survives_bytes_that_are_not_utf8_and_a_huge_log() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.log");
        std::fs::write(&log, b"first\nadb said \xff\xfe garbage\nlast line\n").unwrap();
        let tail = log_tail(&log, 12);
        assert!(
            tail.contains("last line") && tail.contains("garbage"),
            "{tail:?}"
        );
        let mut big = "x".repeat(2000).repeat(2_000).into_bytes();
        big.extend_from_slice(b"\nthe end\n");
        std::fs::write(&log, &big).unwrap();
        let tail = log_tail(&log, 3);
        assert!(
            tail.ends_with("the end"),
            "{}",
            &tail[tail.len().saturating_sub(40)..]
        );
        assert!(tail.len() < 100_000, "{} bytes", tail.len());
    }

    #[tokio::test]
    async fn a_missing_binary_is_an_error_not_a_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let (id, dir) = m.new_task_dir().unwrap();
        let body = SuiteRun {
            child: Child {
                program: tmp.path().join("no-such-velocity"),
                args: vec![],
                cwd: None,
                log: dir.join("run.log"),
                env: None,
            },
            artifacts: dir.join("artifacts"),
        };
        let meta = TaskMeta {
            target: "s".into(),
            device: None,
            log: dir.join("run.log"),
            artifacts: dir.join("artifacts"),
            dir,
            tests_total: None,
        };
        let info = m.spawn(id, meta, Box::new(body));
        let done = m.wait(&info.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.state, TaskState::Error);
        assert!(done.error.unwrap().contains("could not start"));
    }

    /// A stand-in for `velocity explore --json`: reports two turns, waits
    /// for an injected line (echoing it as an `injected` event), writes the
    /// YAML and prints the result; on Ctrl-C it prints a stopped result.
    fn fake_explore() -> PathBuf {
        crate::test_util::script(
            "fake-velocity-explore",
            r#"#!/bin/sh
out=""; inject=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift ;;
    --inject-file) inject="$2"; shift ;;
  esac
  shift
done
E="$VELOCITY_EVENTS_FILE"
stopped() { printf '%s' '{"report":{"outcome":"stopped","reason":"stopped","turns":1,"steps":[],"modelCalls":1,"inputTokens":10,"outputTokens":2},"yaml":null,"replay":null,"repair":null,"files":[]}'; exit 1; }
trap stopped INT
echo '{"event":"observed","turn":1,"title":"Settings","elements":20}' >> "$E"
echo '{"event":"planned","turn":1,"op":"tap","element":4,"target":"Network & internet"}' >> "$E"
i=0
while [ $i -lt 100 ] && ! grep -q . "$inject" 2>/dev/null; do sleep 0.1; i=$((i+1)); done
line=$(head -n 1 "$inject")
echo "{\"event\":\"injected\",\"turn\":2,\"instruction\":\"$line\"}" >> "$E"
[ "$line" = "wait" ] && { while :; do sleep 0.1; done; }
echo '{"event":"finished","outcome":"success"}' >> "$E"
echo '{"event":"testStarted","test":"replay","steps":3}' >> "$E"
echo '{"event":"replayRun","run":1,"passed":true,"ms":1200}' >> "$E"
printf 'appId: com.android.settings\n' > "$out"
printf '{"report":{"outcome":"success","reason":"the page is open","turns":2,"steps":[{},{}],"modelCalls":0,"inputTokens":0,"outputTokens":0,"elapsedMs":1400},"yaml":"appId: x","replay":{"runs":[{"passed":true,"ms":1200,"steps":[]}],"modelCalls":0},"repair":null,"files":["%s"]}' "$out"
"#,
        )
    }

    fn start_explore(m: &TaskManager) -> crate::tasks::TaskInfo {
        let (id, dir) = m.new_task_dir().unwrap();
        let inject = dir.join("inject.txt");
        let req = ExploreRequest {
            goal: "open Network & internet".into(),
            app_id: "com.android.settings".into(),
            device: Some("emulator-5554".into()),
            script: Some("plan.json".into()),
            data: vec![("EMAIL".into(), "a@b.c".into())],
            verify: Some("final".into()),
            out: dir.join("explore.yaml"),
            after: None,
            ..Default::default()
        };
        let mut args = vec![
            "/bin/sh".to_string(),
            fake_explore().to_string_lossy().into_owned(),
        ];
        args.extend(req.args(&inject));
        let body = ExploreRun {
            child: Child {
                program: PathBuf::from("/usr/bin/env"),
                args,
                cwd: None,
                log: dir.join("run.log"),
                env: None,
            },
            inject,
        };
        let meta = TaskMeta {
            target: req.goal.clone(),
            device: req.device.clone(),
            log: dir.join("run.log"),
            artifacts: dir.clone(),
            dir,
            tests_total: None,
        };
        m.spawn(id, meta, Box::new(body))
    }

    async fn until(m: &TaskManager, id: &str, f: impl Fn(&Progress) -> bool) -> Progress {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let p = m.status(id).unwrap().progress;
            if f(&p) || std::time::Instant::now() >= deadline {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[test]
    fn explore_arguments() {
        let req = ExploreRequest {
            goal: "open Wi-Fi; rm -rf /".into(),
            app_id: "a".into(),
            secrets: vec!["PASSWORD".into()],
            max_steps: Some(12),
            replay: Some(3),
            out: "t.yaml".into(),
            after: None,
            ..Default::default()
        };
        let a = req.args(Path::new("/tmp/i.txt"));
        assert_eq!(a[0], "explore");
        // The goal is last, after `--`: never a flag.
        assert_eq!(a[a.len() - 2..], ["--", "open Wi-Fi; rm -rf /"]);
        for pair in [
            ["--json", "--out"],
            ["--inject-file", "/tmp/i.txt"],
            ["--secret", "PASSWORD"],
            ["--max-steps", "12"],
            ["--replay", "3"],
        ] {
            assert!(a.windows(2).any(|w| w == pair), "{pair:?} in {a:?}");
        }
    }

    #[tokio::test]
    async fn an_explore_task_takes_instructions_and_reports_the_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let info = start_explore(&m);
        assert_eq!(info.kind, TaskKind::Explore);
        let p = until(&m, &info.id, |p| p.op.is_some()).await;
        assert_eq!(p.phase.as_deref(), Some("exploring"), "{p:?}");
        assert_eq!(p.turn, Some(1));
        assert_eq!(p.op.as_deref(), Some("tap"));
        assert_eq!(p.target.as_deref(), Some("Network & internet"));
        m.inject(&info.id, "use the\nsearch box").unwrap();
        let done = m.wait(&info.id, Duration::from_secs(60)).await.unwrap();
        assert_eq!(done.state, TaskState::Passed, "{done:?}");
        let events = std::fs::read_to_string(done.dir.join("events.jsonl")).unwrap();
        assert!(
            events.contains(r#""instruction":"use the search box""#),
            "the instruction reached the child as one line: {events}"
        );
        let s = done.summary.unwrap();
        assert_eq!((s.total, s.passed), (1, 1));
        let e = s.explore.unwrap();
        assert_eq!(e["outcome"], "success");
        let yaml = e["yaml"].as_str().unwrap();
        assert!(
            yaml.ends_with("explore.yaml") && Path::new(yaml).is_file(),
            "{e}"
        );
        assert_eq!(e["replay"]["passed"], 1);
        assert_eq!(e["model_calls"], 0);
        assert!(e["next"].as_str().unwrap().contains("velocity run"), "{e}");
        assert_eq!(done.progress.phase.as_deref(), Some("replaying"));
    }

    #[tokio::test]
    async fn a_stopped_explore_task_ends_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let m = TaskManager::new(tmp.path().join("tasks"), vec![]);
        let info = start_explore(&m);
        until(&m, &info.id, |p| p.op.is_some()).await;
        m.inject(&info.id, "wait").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        m.stop(&info.id).unwrap();
        let done = m.wait(&info.id, Duration::from_secs(40)).await.unwrap();
        assert_eq!(done.state, TaskState::Stopped, "{done:?}");
        assert_eq!(done.summary.unwrap().explore.unwrap()["outcome"], "stopped");
    }
}
