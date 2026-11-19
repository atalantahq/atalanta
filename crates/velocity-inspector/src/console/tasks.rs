//! The console's task queue: suite runs and explore sessions.
//!
//! A task is a [`ConsoleTask`] on one device. Tasks on the same device run
//! one at a time, in the order they were queued; each streams events (a
//! step started or finished, a log line, an explore observation) to every
//! page watching `GET /api/tasks/{id}/events` and keeps them for later
//! viewers. A finished run's artifacts (`trace.html`, `video.mp4`, logs,
//! reports) are served from its own folder by
//! `GET /api/tasks/{id}/files/{path}`, confined to that folder.
//!
//! **Explore.** An explore session is a `velocity explore … --json` child
//! ([`ExploreChild`]), like a suite run: it takes the device's lease
//! itself, writes its events (`velocity_common::live_events`: its turns,
//! and its replays' tests and steps) to the task's `events.jsonl`, reads
//! the page's instructions from an `--inject-file`, and prints its result
//! (the YAML, the replays, the model's calls and tokens), which the page
//! shows and can save into the project ([`save`]). A build without
//! explore keeps [`ExploreUnavailable`], which answers 501.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as UrlPath, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch};

use super::live::unix_us;
use super::project::confine;
use crate::state::AppState;

pub type ApiError = (StatusCode, String);

/// Events a task keeps for viewers that join late.
const MAX_EVENTS: usize = 20_000;
/// The longest log line kept.
const MAX_LINE: usize = 2000;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
    Error,
}

impl TaskStatus {
    pub fn finished(self) -> bool {
        !matches!(self, TaskStatus::Queued | TaskStatus::Running)
    }
}

/// One thing a task reported.
#[derive(Serialize, Clone, Debug)]
pub struct TaskEvent {
    pub seq: u64,
    /// Unix ms.
    pub t: u64,
    /// `log`, `testStarted`, `stepStarted`, `stepFinished`, `testFinished`,
    /// `summary`, `explore`, `instruction`, `status`.
    pub kind: String,
    pub data: Value,
}

/// How a task ended.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub status: TaskStatus,
    pub summary: String,
}

impl TaskOutcome {
    pub fn new(status: TaskStatus, summary: impl Into<String>) -> Self {
        Self {
            status,
            summary: summary.into(),
        }
    }
}

/// What a running task is given.
pub struct TaskCtx {
    pub id: u64,
    pub device_id: String,
    /// The task's own artifacts folder (created).
    pub out_dir: PathBuf,
    /// The project folder (suites are relative to it).
    pub project_dir: PathBuf,
    /// Instructions typed into the page's "inject" box, in order.
    pub instructions: mpsc::Receiver<String>,
    /// Turns `true` when the task is cancelled.
    pub cancel: watch::Receiver<bool>,
    record: Arc<TaskRecord>,
}

impl TaskCtx {
    /// Report an event to every viewer.
    pub fn emit(&self, kind: &str, data: Value) {
        self.record.push(kind, data);
    }
    pub fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
}

/// Something the console runs on a device.
#[async_trait::async_trait]
pub trait ConsoleTask: Send + 'static {
    /// `run` or `explore`.
    fn kind(&self) -> &'static str;
    fn title(&self) -> String;
    /// Whether it takes typed instructions while it runs.
    fn accepts_instructions(&self) -> bool {
        false
    }
    /// It drives the device from this process (so the console holds the
    /// lease for it), rather than from a child process that takes the
    /// lease itself (a suite run).
    fn drives_in_process(&self) -> bool {
        false
    }
    async fn run(self: Box<Self>, ctx: TaskCtx) -> TaskOutcome;
}

/// An explore session to start: checked by the route (the plan inside the
/// project, no value that reads as a flag).
#[derive(Debug, Clone, Default)]
pub struct ExploreSpec {
    pub goal: String,
    pub device_id: String,
    pub app_id: String,
    /// A fixed plan instead of a model (`--script`), inside the project.
    pub script: Option<PathBuf>,
    /// `provider:model`; else the configured provider's goal model.
    pub model: Option<String>,
    pub max_steps: Option<usize>,
    pub replay: Option<usize>,
    /// `off`, `final`, `checkpoints` or `strict`.
    pub verify: Option<String>,
}

/// Starts explore sessions (see the module docs).
pub trait ExploreProvider: Send + Sync {
    /// Whether explore is in this build (the page hides the prompt if not).
    fn available(&self) -> bool {
        true
    }
    fn start(&self, spec: ExploreSpec) -> Result<Box<dyn ConsoleTask>, String>;
}

/// No explore in this build.
pub struct ExploreUnavailable;

pub const EXPLORE_UNAVAILABLE: &str = "explore is not available in this build";

impl ExploreProvider for ExploreUnavailable {
    fn available(&self) -> bool {
        false
    }
    fn start(&self, _: ExploreSpec) -> Result<Box<dyn ConsoleTask>, String> {
        Err(EXPLORE_UNAVAILABLE.into())
    }
}

// ── Records and the queue ───────────────────────────────────────────────

struct RecordState {
    status: TaskStatus,
    created_ms: u64,
    started_ms: Option<u64>,
    finished_ms: Option<u64>,
    summary: String,
    events: VecDeque<TaskEvent>,
    dropped: u64,
    next_seq: u64,
}

pub struct TaskRecord {
    pub id: u64,
    pub kind: &'static str,
    pub title: String,
    pub device_id: String,
    pub out_dir: PathBuf,
    accepts_instructions: bool,
    state: Mutex<RecordState>,
    tx: broadcast::Sender<TaskEvent>,
    inject: mpsc::Sender<String>,
    cancel: watch::Sender<bool>,
}

fn now_ms() -> u64 {
    unix_us() / 1000
}

impl TaskRecord {
    fn lock(&self) -> std::sync::MutexGuard<'_, RecordState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, kind: &str, data: Value) {
        let mut s = self.lock();
        let ev = TaskEvent {
            seq: s.next_seq,
            t: now_ms(),
            kind: kind.to_string(),
            data,
        };
        s.next_seq += 1;
        s.events.push_back(ev.clone());
        if s.events.len() > MAX_EVENTS {
            s.events.pop_front();
            s.dropped += 1;
        }
        // Sent under the lock, so a viewer that subscribes and then copies
        // the history sees each event once.
        let _ = self.tx.send(ev);
    }

    fn set_status(&self, status: TaskStatus, summary: Option<String>) {
        {
            let mut s = self.lock();
            s.status = status;
            match status {
                TaskStatus::Running => s.started_ms = Some(now_ms()),
                st if st.finished() => s.finished_ms = Some(now_ms()),
                _ => {}
            }
            if let Some(m) = &summary {
                s.summary = m.clone();
            }
        }
        self.push(
            "status",
            json!({"status": status, "summary": summary.unwrap_or_default()}),
        );
    }

    pub fn status(&self) -> TaskStatus {
        self.lock().status
    }

    /// The task without its events.
    pub fn info(&self) -> Value {
        let s = self.lock();
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "deviceId": self.device_id,
            "status": s.status,
            "createdMs": s.created_ms,
            "startedMs": s.started_ms,
            "finishedMs": s.finished_ms,
            "summary": s.summary,
            "acceptsInstructions": self.accepts_instructions,
            "events": s.events.len() as u64 + s.dropped,
        })
    }
}

#[derive(Default)]
pub struct Tasks {
    next: AtomicU64,
    records: Mutex<Vec<Arc<TaskRecord>>>,
    devices: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Tasks {
    pub fn get(&self, id: u64) -> Option<Arc<TaskRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    pub fn all(&self) -> Vec<Arc<TaskRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn device_lock(&self, device_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    /// Queue `task` on `device_id`. It starts once the device's earlier
    /// tasks have finished.
    pub fn submit(
        &self,
        state: &Arc<AppState>,
        task: Box<dyn ConsoleTask>,
        device_id: &str,
    ) -> Result<Arc<TaskRecord>, ApiError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let out_dir = state
            .console
            .runs_dir
            .join(format!("{}-{id}", now_ms() / 1000));
        std::fs::create_dir_all(&out_dir).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("can't create {}: {e}", out_dir.display()),
            )
        })?;
        let (tx, _) = broadcast::channel(1024);
        let (inject, instructions) = mpsc::channel(32);
        let (cancel, cancel_rx) = watch::channel(false);
        let record = Arc::new(TaskRecord {
            id,
            kind: task.kind(),
            title: task.title(),
            device_id: device_id.to_string(),
            out_dir: out_dir.clone(),
            accepts_instructions: task.accepts_instructions(),
            state: Mutex::new(RecordState {
                status: TaskStatus::Queued,
                created_ms: now_ms(),
                started_ms: None,
                finished_ms: None,
                summary: String::new(),
                events: VecDeque::new(),
                dropped: 0,
                next_seq: 0,
            }),
            tx,
            inject,
            cancel,
        });
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(record.clone());
        record.push("status", json!({"status": TaskStatus::Queued}));

        let lock = self.device_lock(device_id);
        let st = state.clone();
        let rec = record.clone();
        let device = device_id.to_string();
        let mut cancel_wait = cancel_rx.clone();
        tokio::spawn(async move {
            // Queued: wait for the device, unless cancelled first.
            let _turn = tokio::select! {
                g = lock.lock_owned() => g,
                _ = cancel_wait.wait_for(|c| *c) => {
                    rec.set_status(TaskStatus::Cancelled, Some("cancelled while queued".into()));
                    return;
                }
            };
            if *cancel_rx.borrow() {
                rec.set_status(TaskStatus::Cancelled, Some("cancelled while queued".into()));
                return;
            }
            let in_process = task.drives_in_process();
            if in_process {
                if let Err((_, e)) = st.control.ensure(st.console.leases.as_ref(), &device).await {
                    rec.set_status(TaskStatus::Error, Some(e));
                    return;
                }
            } else if st.control.release(&device).await {
                // A child `velocity run` takes the lease itself.
                rec.push(
                    "log",
                    json!({"stream": "console", "line": "released the console's control of the device for the run"}),
                );
            }
            st.control.set_task(&device, Some(rec.id));
            rec.set_status(TaskStatus::Running, None);
            let ctx = TaskCtx {
                id: rec.id,
                device_id: device.clone(),
                out_dir: rec.out_dir.clone(),
                project_dir: st.console.project_dir.clone(),
                instructions,
                cancel: cancel_rx,
                record: rec.clone(),
            };
            // A panicking task must not leave the device marked busy.
            let outcome = match tokio::spawn(task.run(ctx)).await {
                Ok(o) => o,
                Err(e) => TaskOutcome::new(TaskStatus::Error, format!("task failed: {e}")),
            };
            st.control.set_task(&device, None);
            rec.set_status(outcome.status, Some(outcome.summary));
            st.poke();
        });
        Ok(record)
    }
}

// ── Suite runs ──────────────────────────────────────────────────────────

/// What to run.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    /// Suite path relative to the project folder.
    pub suite: String,
    pub device_id: String,
    /// `--filter`: a test name glob.
    #[serde(default)]
    pub filter: Option<String>,
}

/// Builds the child process of a suite run.
pub trait SuiteLauncher: Send + Sync {
    /// The command for `suite` (absolute, inside the project) on
    /// `device_id`, writing its artifacts to `out_dir` and live events to
    /// `events_file`.
    fn command(
        &self,
        suite: &Path,
        req: &RunRequest,
        out_dir: &Path,
        events_file: &Path,
    ) -> tokio::process::Command;
}

/// `velocity run` with this binary.
pub struct VelocityBinary {
    pub exe: PathBuf,
}

impl VelocityBinary {
    pub fn current() -> Self {
        Self {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("velocity")),
        }
    }
}

impl SuiteLauncher for VelocityBinary {
    fn command(
        &self,
        suite: &Path,
        req: &RunRequest,
        out_dir: &Path,
        events_file: &Path,
    ) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.exe);
        c.arg("run")
            .arg(suite)
            .args(["-d", &req.device_id])
            .args(["--report", "json,html"])
            .arg("--artifacts-dir")
            .arg(out_dir)
            .args(["--video", "on-failure", "--logs", "on-failure"]);
        if let Some(f) = req.filter.as_deref().filter(|f| !f.is_empty()) {
            c.args(["--filter", f]);
        }
        c.env(velocity_common::live_events::EVENTS_FILE_ENV, events_file)
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0");
        c
    }
}

/// A suite run in a child process.
pub struct SuiteRun {
    pub suite: PathBuf,
    pub req: RunRequest,
    pub launcher: Arc<dyn SuiteLauncher>,
}

/// `s` without ANSI colour codes, at most [`MAX_LINE`] characters.
pub fn clean_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if out.len() >= MAX_LINE {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// New complete lines of the events file since `offset`.
async fn tail(path: &Path, offset: &mut u64, partial: &mut String) -> Vec<Value> {
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return Vec::new();
    };
    if f.seek(std::io::SeekFrom::Start(*offset)).await.is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).await.is_err() {
        return Vec::new();
    }
    *offset += buf.len() as u64;
    partial.push_str(&String::from_utf8_lossy(&buf));
    let mut out = Vec::new();
    while let Some(i) = partial.find('\n') {
        let line: String = partial.drain(..=i).collect();
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            out.push(v);
        }
    }
    out
}

#[async_trait::async_trait]
impl ConsoleTask for SuiteRun {
    fn kind(&self) -> &'static str {
        "run"
    }
    fn title(&self) -> String {
        match &self.req.filter {
            Some(f) if !f.is_empty() => format!("{} ({f})", self.req.suite),
            _ => self.req.suite.clone(),
        }
    }
    async fn run(self: Box<Self>, mut ctx: TaskCtx) -> TaskOutcome {
        let events_file = ctx.out_dir.join("events.jsonl");
        let mut cmd = self
            .launcher
            .command(&self.suite, &self.req, &ctx.out_dir, &events_file);
        cmd.current_dir(&ctx.project_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match velocity_common::process::spawn_group(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                return TaskOutcome::new(TaskStatus::Error, format!("can't start the run: {e}"))
            }
        };
        ctx.emit(
            "log",
            json!({"stream": "console", "line": format!("running {} on {}", self.req.suite, self.req.device_id)}),
        );
        let (line_tx, mut lines) = mpsc::unbounded_channel::<(&'static str, String)>();
        for (name, pipe) in [
            (
                "stdout",
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            ),
            (
                "stderr",
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            ),
        ] {
            if let Some(p) = pipe {
                let tx = line_tx.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(p).lines();
                    while let Ok(Some(l)) = r.next_line().await {
                        if tx.send((name, l)).is_err() {
                            break;
                        }
                    }
                });
            }
        }
        drop(line_tx);
        let mut offset = 0u64;
        let mut partial = String::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut cancelled = false;
        let mut lines_open = true;
        let status = loop {
            tokio::select! {
                st = child.wait() => break st.ok(),
                _ = ctx.cancel.changed() => {
                    if *ctx.cancel.borrow() {
                        cancelled = true;
                        let _ = child.start_kill();
                        break None;
                    }
                }
                l = lines.recv(), if lines_open => match l {
                    Some((stream, line)) => ctx.emit("log", json!({"stream": stream, "line": clean_line(&line)})),
                    None => lines_open = false,
                },
                _ = tick.tick() => {
                    for v in tail(&events_file, &mut offset, &mut partial).await {
                        let kind = v.get("event").and_then(Value::as_str).unwrap_or("event").to_string();
                        ctx.emit(&kind, v);
                    }
                }
            }
        };
        // What is left of the output and the events.
        let drain_until = tokio::time::Instant::now() + Duration::from_millis(500);
        while let Ok(Some((stream, line))) =
            tokio::time::timeout_at(drain_until, lines.recv()).await
        {
            ctx.emit("log", json!({"stream": stream, "line": clean_line(&line)}));
        }
        for v in tail(&events_file, &mut offset, &mut partial).await {
            let kind = v
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("event")
                .to_string();
            ctx.emit(&kind, v);
        }
        drop(child);
        if cancelled {
            return TaskOutcome::new(TaskStatus::Cancelled, "cancelled");
        }
        let results = tokio::fs::read(ctx.out_dir.join("results.json"))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let counts = results.as_ref().map(|r| {
            json!({
                "total": r["total"], "passed": r["passed"], "failed": r["failed"],
                "skipped": r["skipped"],
            })
        });
        if let Some(c) = &counts {
            ctx.emit("summary", c.clone());
        }
        let code = status.and_then(|s| s.code());
        let summary = match &counts {
            Some(c) => format!(
                "{} passed, {} failed of {}",
                c["passed"].as_u64().unwrap_or(0),
                c["failed"].as_u64().unwrap_or(0),
                c["total"].as_u64().unwrap_or(0)
            ),
            None => match code {
                Some(c) => format!("exited with {c}"),
                None => "stopped by a signal".to_string(),
            },
        };
        match code {
            Some(0) => TaskOutcome::new(TaskStatus::Passed, summary),
            Some(1) => TaskOutcome::new(TaskStatus::Failed, summary),
            other => TaskOutcome::new(
                TaskStatus::Error,
                match (other, &counts) {
                    (Some(c), Some(_)) => format!("{summary} (exit {c})"),
                    _ => summary,
                },
            ),
        }
    }
}

// ── Explore sessions ────────────────────────────────────────────────────

/// The files of one explore session, in its task folder.
pub struct ExploreFiles {
    /// Where the test is written (`--out`).
    pub out: PathBuf,
    pub events: PathBuf,
    /// The page's instructions, one line each (`--inject-file`).
    pub inject: PathBuf,
}

/// Builds the child process of an explore session.
pub trait ExploreLauncher: Send + Sync {
    fn command(&self, spec: &ExploreSpec, files: &ExploreFiles) -> tokio::process::Command;
}

impl ExploreLauncher for VelocityBinary {
    fn command(&self, spec: &ExploreSpec, files: &ExploreFiles) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.exe);
        c.arg("explore")
            .args(["--app-id", &spec.app_id, "--device", &spec.device_id])
            .arg("--json")
            .arg("--force")
            .arg("--out")
            .arg(&files.out)
            .arg("--inject-file")
            .arg(&files.inject);
        if let Some(s) = &spec.script {
            c.arg("--script").arg(s);
        }
        if let Some(m) = &spec.model {
            c.args(["--model", m]);
        }
        if let Some(n) = spec.max_steps {
            c.args(["--max-steps", &n.to_string()]);
        }
        if let Some(n) = spec.replay {
            c.args(["--replay", &n.to_string()]);
        }
        if let Some(v) = &spec.verify {
            c.args(["--verify", v]);
        }
        // The goal last, after `--`: a goal that starts with `-` is text,
        // never a flag (security review 05 #8).
        c.arg("--").arg(&spec.goal);
        c.env(velocity_common::live_events::EVENTS_FILE_ENV, &files.events)
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0");
        c
    }
}

/// Explore sessions as `velocity explore` children.
pub struct ExploreChild {
    pub launcher: Arc<dyn ExploreLauncher>,
}

impl ExploreProvider for ExploreChild {
    fn start(&self, spec: ExploreSpec) -> Result<Box<dyn ConsoleTask>, String> {
        Ok(Box::new(ExploreTask {
            spec,
            launcher: self.launcher.clone(),
        }))
    }
}

/// The test's file name: the goal as a slug (`open-network-internet.yaml`),
/// as `velocity explore` names it.
pub fn explore_file_name(goal: &str) -> String {
    let mut slug = String::new();
    for c in goal.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    format!("{}.yaml", if slug.is_empty() { "explore" } else { slug })
}

struct ExploreTask {
    spec: ExploreSpec,
    launcher: Arc<dyn ExploreLauncher>,
}

/// Ctrl-C to a child's process group: explore ends its session as
/// `stopped` and still reports.
fn interrupt(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: plain kill(2) on the group the child leads.
        unsafe {
            libc::kill(-pid, libc::SIGINT);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// A `velocity explore --json` result, as the page shows it.
pub fn explore_result(r: &Value, out_dir: &Path) -> Value {
    let rep = &r["report"];
    let runs = r["replay"]["runs"].as_array().cloned().unwrap_or_default();
    let file = r["files"]
        .as_array()
        .and_then(|f| f.first())
        .and_then(Value::as_str)
        .map(|f| {
            Path::new(f)
                .strip_prefix(out_dir)
                .unwrap_or(Path::new(f))
                .to_string_lossy()
                .to_string()
        });
    json!({
        "outcome": rep["outcome"],
        "reason": rep["reason"],
        "yaml": r["yaml"],
        "file": file,
        "turns": rep["turns"],
        "actions": rep["steps"].as_array().map_or(0, Vec::len),
        "modelCalls": rep["modelCalls"],
        "inputTokens": rep["inputTokens"],
        "outputTokens": rep["outputTokens"],
        "costUsd": rep["costUsd"],
        "replay": runs.iter().map(|x| json!({
            "passed": x["passed"], "ms": x["ms"], "failedStep": x["failedStep"], "error": x["error"],
        })).collect::<Vec<_>>(),
        "replayModelCalls": r["replay"]["modelCalls"],
        "repair": r["repair"],
    })
}

#[async_trait::async_trait]
impl ConsoleTask for ExploreTask {
    fn kind(&self) -> &'static str {
        "explore"
    }
    fn title(&self) -> String {
        self.spec.goal.clone()
    }
    fn accepts_instructions(&self) -> bool {
        true
    }
    async fn run(self: Box<Self>, mut ctx: TaskCtx) -> TaskOutcome {
        let files = ExploreFiles {
            out: ctx.out_dir.join(explore_file_name(&self.spec.goal)),
            events: ctx.out_dir.join("events.jsonl"),
            inject: ctx.out_dir.join("inject.txt"),
        };
        let result_file = ctx.out_dir.join("result.json");
        let stdout = match tokio::fs::write(&files.inject, "").await {
            Ok(()) => tokio::fs::File::create(&result_file).await,
            Err(e) => Err(e),
        };
        let stdout = match stdout {
            Ok(f) => f.into_std().await,
            Err(e) => return TaskOutcome::new(TaskStatus::Error, format!("task folder: {e}")),
        };
        let mut cmd = self.launcher.command(&self.spec, &files);
        cmd.current_dir(&ctx.project_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::piped());
        let mut child = match velocity_common::process::spawn_group(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                return TaskOutcome::new(TaskStatus::Error, format!("can't start explore: {e}"))
            }
        };
        ctx.emit(
            "log",
            json!({"stream": "console", "line": format!("exploring {:?} on {}", self.spec.goal, self.spec.device_id)}),
        );
        let mut lines = child.stderr.take().map(|p| BufReader::new(p).lines());
        let mut offset = 0u64;
        let mut partial = String::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut cancelled = false;
        let mut kill_at: Option<tokio::time::Instant> = None;
        let emit_events = |ctx: &TaskCtx, events: Vec<Value>| {
            for v in events {
                let kind = v.get("event").and_then(Value::as_str).unwrap_or("");
                // The replays' tests and steps fill the steps table; the
                // session's own events are the explore stream.
                if velocity_common::live_events::LiveEvent::TEST_EVENTS.contains(&kind) {
                    ctx.emit(kind, v.clone());
                } else {
                    ctx.emit("explore", v);
                }
            }
        };
        let status = loop {
            tokio::select! {
                st = child.wait() => break st.ok(),
                _ = ctx.cancel.changed(), if !cancelled => {
                    if *ctx.cancel.borrow() {
                        cancelled = true;
                        interrupt(&child);
                        kill_at = Some(tokio::time::Instant::now() + Duration::from_secs(20));
                    }
                }
                Some(text) = ctx.instructions.recv() => {
                    use tokio::io::AsyncWriteExt;
                    let line = text.replace(['\n', '\r'], " ");
                    if let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(&files.inject).await {
                        let _ = f.write_all(format!("{}\n", line.trim()).as_bytes()).await;
                    }
                }
                l = async {
                    match lines.as_mut() {
                        Some(r) => r.next_line().await,
                        None => std::future::pending().await,
                    }
                } => match l {
                    Ok(Some(line)) => ctx.emit("log", json!({"stream": "stderr", "line": clean_line(&line)})),
                    _ => lines = None,
                },
                _ = tick.tick() => {
                    emit_events(&ctx, tail(&files.events, &mut offset, &mut partial).await);
                    if kill_at.is_some_and(|t| tokio::time::Instant::now() >= t) {
                        let _ = child.start_kill();
                    }
                }
            }
        };
        emit_events(&ctx, tail(&files.events, &mut offset, &mut partial).await);
        let result = tokio::fs::read(&result_file)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let shown = result.as_ref().map(|r| explore_result(r, &ctx.out_dir));
        if let Some(r) = &shown {
            ctx.emit("result", r.clone());
        }
        if cancelled {
            return TaskOutcome::new(TaskStatus::Cancelled, "stopped");
        }
        let code = status.and_then(|s| s.code());
        let Some(r) = shown else {
            return TaskOutcome::new(
                TaskStatus::Error,
                format!("velocity explore exited with {code:?} and no result (see the output)"),
            );
        };
        let runs = r["replay"].as_array().map_or(0, Vec::len);
        let passed = r["replay"]
            .as_array()
            .map_or(0, |a| a.iter().filter(|x| x["passed"] == true).count());
        let summary = format!(
            "{}: {}{}",
            r["outcome"].as_str().unwrap_or("?"),
            r["reason"].as_str().unwrap_or(""),
            if runs > 0 {
                format!("; replay {passed}/{runs} passed")
            } else {
                String::new()
            }
        );
        match code {
            Some(0) => TaskOutcome::new(TaskStatus::Passed, summary),
            Some(1 | 4) => TaskOutcome::new(TaskStatus::Failed, summary),
            _ => TaskOutcome::new(TaskStatus::Error, summary),
        }
    }
}

// ── Routes ──────────────────────────────────────────────────────────────

/// `GET /api/tasks`: newest first.
pub async fn list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut all: Vec<Value> = state.tasks.all().iter().map(|r| r.info()).collect();
    all.reverse();
    let available = state.console.explore.available();
    Json(json!({
        "tasks": all,
        "exploreAvailable": available,
        "exploreMessage": (!available).then_some(EXPLORE_UNAVAILABLE),
        "appId": state.app_id().await,
    }))
}

fn not_found(id: u64) -> ApiError {
    (StatusCode::NOT_FOUND, format!("no task {id}"))
}

/// `POST /api/tasks/run`.
pub async fn start_run(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RunRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.device_id.is_empty() || req.device_id.starts_with('-') {
        return Err((StatusCode::BAD_REQUEST, "pick a device".into()));
    }
    if req.filter.as_deref().is_some_and(|f| f.starts_with('-')) {
        return Err((StatusCode::BAD_REQUEST, "bad filter".into()));
    }
    let suite = confine(&state.console.project_dir, &req.suite)
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml")
                })
        })
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!("{:?} is not a suite in the project folder", req.suite),
        ))?;
    let device = req.device_id.clone();
    let task = SuiteRun {
        suite,
        req,
        launcher: state.console.launcher.clone(),
    };
    let rec = state.tasks.submit(&state, Box::new(task), &device)?;
    Ok((StatusCode::ACCEPTED, Json(rec.info())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreRequest {
    pub goal: String,
    pub device_id: String,
    /// The app (default: the console's `--app-id`).
    #[serde(default)]
    pub app_id: Option<String>,
    /// A plan file in the project (`--script`) instead of a model.
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_steps: Option<usize>,
    #[serde(default)]
    pub replay: Option<usize>,
    #[serde(default)]
    pub verify: Option<String>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

/// An optional text field, trimmed; empty is none.
fn plain(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|v| !v.is_empty()).map(String::from)
}

/// `POST /api/tasks/explore`: 501 when explore is not in the build.
pub async fn start_explore(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ExploreRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !state.console.explore.available() {
        return Err((StatusCode::NOT_IMPLEMENTED, EXPLORE_UNAVAILABLE.into()));
    }
    let goal = req.goal.trim();
    if goal.is_empty() || goal.len() > 4000 {
        return Err(bad("describe a goal (up to 4000 characters)"));
    }
    if req.device_id.is_empty() || req.device_id.starts_with('-') {
        return Err(bad("pick a device"));
    }
    let app_id = match plain(req.app_id.as_deref()) {
        Some(a) => a,
        None => state
            .app_id()
            .await
            .ok_or_else(|| bad("which app? give its bundle id / package"))?,
    };
    let model = plain(req.model.as_deref());
    let verify = plain(req.verify.as_deref());
    if app_id.starts_with('-')
        || model.as_deref().is_some_and(|m| m.starts_with('-'))
        || verify.as_deref().is_some_and(|v| v.starts_with('-'))
    {
        return Err(bad("a value may not start with '-'"));
    }
    let script = match plain(req.script.as_deref()) {
        Some(rel) => Some(
            confine(&state.console.project_dir, &rel)
                .filter(|p| p.is_file())
                .ok_or_else(|| bad(format!("{rel:?} is not a plan file in the project folder")))?,
        ),
        None => None,
    };
    if script.is_some() && model.is_some() {
        return Err(bad("give a plan or a model, not both"));
    }
    let spec = ExploreSpec {
        goal: goal.to_string(),
        device_id: req.device_id.clone(),
        app_id,
        script,
        model,
        max_steps: req.max_steps.map(|n| n.clamp(1, 200)),
        replay: req.replay.map(|n| n.min(10)),
        verify,
    };
    let task = state
        .console
        .explore
        .start(spec)
        .map_err(|e| (StatusCode::NOT_IMPLEMENTED, e))?;
    let rec = state.tasks.submit(&state, task, &req.device_id)?;
    Ok((StatusCode::ACCEPTED, Json(rec.info())))
}

#[derive(Deserialize)]
pub struct SaveRequest {
    /// Where in the project, relative (`flows/open-wifi.yaml`).
    pub path: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// `rel` as a new `.yaml` file inside `base`: relative, normal components
/// only, its folder inside `base` once symlinks are resolved (created if
/// missing).
pub fn confine_new(base: &Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    if rel.is_empty() || rel.contains(['\0', '\\']) {
        return None;
    }
    let p = Path::new(rel);
    if !p.components().all(|c| matches!(c, Component::Normal(_)))
        || !p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    {
        return None;
    }
    let base = base.canonicalize().ok()?;
    let name = p.file_name()?;
    // Check the deepest folder that exists before creating any.
    let mut existing = base.join(p.parent()?);
    while !existing.exists() {
        existing = existing.parent()?.to_path_buf();
    }
    if !existing.canonicalize().ok()?.starts_with(&base) {
        return None;
    }
    let dir = base.join(p.parent()?);
    std::fs::create_dir_all(&dir).ok()?;
    let dir = dir.canonicalize().ok()?;
    let full = dir.join(name);
    // Not through a symlink at the file itself.
    if full
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return None;
    }
    dir.starts_with(&base).then_some(full)
}

/// `POST /api/tasks/{id}/save`: write an explore task's YAML into the
/// project folder (a new file unless `overwrite`).
pub async fn save(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
    Json(req): Json<SaveRequest>,
) -> Result<Json<Value>, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    if rec.kind != "explore" || !rec.status().finished() {
        return Err((
            StatusCode::CONFLICT,
            "only a finished explore task has a test to save".into(),
        ));
    }
    let yaml = tokio::fs::read(rec.out_dir.join("result.json"))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|r| r["yaml"].as_str().map(String::from))
        .ok_or((
            StatusCode::CONFLICT,
            "this session wrote no test".to_string(),
        ))?;
    let path = confine_new(&state.console.project_dir, req.path.trim()).ok_or_else(|| {
        bad(format!(
            "{:?} is not a new .yaml path inside the project folder",
            req.path
        ))
    })?;
    if path.exists() && !req.overwrite {
        return Err((StatusCode::CONFLICT, format!("{} exists", req.path.trim())));
    }
    tokio::fs::write(&path, yaml)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rel = path
        .strip_prefix(state.console.project_dir.canonicalize().unwrap_or_default())
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string();
    rec.push("saved", json!({"path": rel}));
    Ok(Json(json!({"ok": true, "path": rel})))
}

/// `GET /api/tasks/{id}`: the task and its events.
pub async fn get(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
) -> Result<Json<Value>, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    let mut info = rec.info();
    info["eventList"] = json!(rec.lock().events.iter().collect::<Vec<_>>());
    Ok(Json(info))
}

#[derive(Deserialize)]
pub struct InjectRequest {
    pub text: String,
}

/// `POST /api/tasks/{id}/inject`.
pub async fn inject(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
    Json(req): Json<InjectRequest>,
) -> Result<Json<Value>, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    if !rec.accepts_instructions {
        return Err((
            StatusCode::CONFLICT,
            "this task takes no instructions".into(),
        ));
    }
    if rec.status().finished() {
        return Err((StatusCode::CONFLICT, "the task has finished".into()));
    }
    let text = req.text.trim();
    if text.is_empty() || text.len() > 4000 {
        return Err((StatusCode::BAD_REQUEST, "empty or too long".into()));
    }
    rec.inject
        .try_send(text.to_string())
        .map_err(|_| (StatusCode::CONFLICT, "too many pending instructions".into()))?;
    rec.push("instruction", json!({"text": text}));
    Ok(Json(json!({"ok": true})))
}

/// `POST /api/tasks/{id}/cancel`.
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
) -> Result<Json<Value>, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    let _ = rec.cancel.send(true);
    Ok(Json(json!({"ok": true})))
}

/// `GET /api/tasks/{id}/events` (WebSocket): every event so far, then the
/// new ones; the socket closes after the task's final status.
pub async fn events(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    Ok(ws.on_upgrade(move |socket| stream_events(socket, rec)))
}

async fn stream_events(mut socket: WebSocket, rec: Arc<TaskRecord>) {
    let (history, mut rx, done) = {
        let s = rec.lock();
        let rx = rec.tx.subscribe();
        (
            s.events.iter().cloned().collect::<Vec<_>>(),
            rx,
            s.status.finished(),
        )
    };
    for ev in history {
        if socket
            .send(Message::Text(json!(ev).to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    if done {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    let end = ev.kind == "status" && ev.data["status"].as_str().is_some_and(|s| !matches!(s, "queued" | "running"));
                    if socket.send(Message::Text(json!(ev).to_string().into())).await.is_err() {
                        return;
                    }
                    if end {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let msg = json!({"seq": -1, "kind": "lagged", "data": {"missed": n}});
                    if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            }
        }
    }
}

/// The files under `dir` (not following symlinks), relative, with sizes.
fn walk(dir: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                let p = e.path();
                let rel = p
                    .strip_prefix(dir)
                    .unwrap_or(&p)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(json!({"path": rel, "size": size}));
                if out.len() >= 5000 {
                    return out;
                }
            }
        }
    }
    out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    out
}

/// `GET /api/tasks/{id}/files`.
pub async fn files(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<u64>,
) -> Result<Json<Value>, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    let dir = rec.out_dir.clone();
    let list = tokio::task::spawn_blocking(move || walk(&dir))
        .await
        .unwrap_or_default();
    Ok(Json(json!({"files": list})))
}

/// The Content-Security-Policy every served artifact gets: a `trace.html`
/// renders text from the device, so even if some of it got through its
/// escaping, its scripts can't call the API (`connect-src 'none'`), post a
/// form, load anything from elsewhere, or be framed.
pub const ARTIFACT_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
style-src 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; \
connect-src 'none'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'";

/// `GET /api/tasks/{id}/files/{*path}`: one artifact of the task's folder.
/// Supports a single `Range` (video seeking).
pub async fn file(
    State(state): State<Arc<AppState>>,
    UrlPath((id, path)): UrlPath<(u64, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let rec = state.tasks.get(id).ok_or_else(|| not_found(id))?;
    let full = confine(&rec.out_dir, &path)
        .filter(|p| p.is_file())
        .ok_or((StatusCode::NOT_FOUND, "no such file".to_string()))?;
    let data = tokio::fs::read(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mime = mime_guess::from_path(&full).first_or_octet_stream();
    let total = data.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| parse_range(r, total));
    let mut resp = match range {
        Some((start, end)) => {
            let mut r = (
                StatusCode::PARTIAL_CONTENT,
                data[start as usize..=end as usize].to_vec(),
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            r
        }
        None => (StatusCode::OK, data).into_response(),
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(ARTIFACT_CSP),
    );
    Ok(resp)
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` within `total` bytes.
pub fn parse_range(r: &str, total: u64) -> Option<(u64, u64)> {
    let spec = r.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (total.saturating_sub(n), total - 1)
        }
        (a, "") => (a.parse().ok()?, total - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(total - 1)),
    };
    (start <= end && end < total).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_parsed_within_the_file() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn log_lines_lose_colours_and_length() {
        assert_eq!(clean_line("\x1b[32m✓\x1b[0m login"), "✓ login");
        assert!(clean_line(&"x".repeat(5000)).len() <= MAX_LINE + 3);
    }
}
