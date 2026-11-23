//! Background tasks (`run_task`, `task_status`, `task_stop`, `task_inject`).
//!
//! A task runs in a tokio task inside the server, so `run_task` answers at
//! once with the task's id, folder and log, and the agent polls
//! `task_status`. What a task does is a [`TaskBody`]:
//!
//! - [`process::SuiteRun`]: a `atalanta run` child process. A child, not the
//!   runner in this process, because the runner prints to stdout, which here
//!   carries the protocol, and so the run takes and releases its device
//!   lease like any other run, and a stop is the Ctrl-C a run already
//!   handles (it writes its reports).
//! - [`process::ExploreRun`]: a `atalanta explore … --json` child
//!   ([`TaskKind::Explore`]), for the same reasons. `task_inject`
//!   instructions arrive on [`TaskContext::instructions`] and are appended
//!   to the child's `--inject-file`.
//!
//! Both children write live events (`atalanta_common::live_events`) to the
//! task's `events.jsonl`, which becomes [`Progress`].
//!
//! On shutdown every running task is stopped and waited for, so each writes
//! its reports and `task.json`.

pub mod inject;
pub mod notify;
pub mod process;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atalanta_common::{AtalantaError, Result};
use serde::Serialize;
use tokio::sync::watch;

pub use inject::{instruction_channel, Instruction, InstructionReceiver, InstructionSender};
pub use notify::Notifier;

/// What kind of task it is (it decides whether instructions are taken).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    /// A suite run.
    Run,
    /// An exploring agent (`atalanta explore`): takes injected instructions.
    Explore,
    /// `atalanta verify-change`: a saved flow replayed, or explored and saved.
    Verify,
    /// `atalanta gate`: the trust gates on a written test.
    Gate,
}

impl TaskKind {
    pub fn takes_instructions(self) -> bool {
        self == Self::Explore
    }
}

/// Where a task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Passed,
    Failed,
    Stopped,
    /// It could not run or finish (a crash, an unusable suite, a timeout).
    Error,
}

impl TaskState {
    pub fn is_done(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// How far a running task got.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Progress {
    /// The test running now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// The step running now, 1-based.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
    /// Steps in that test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<usize>,
    /// What that step does (`tap { id: "ok" }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Test attempts finished so far (a retry counts again).
    pub tests_finished: usize,
    /// Of them, how many failed.
    pub tests_failed: usize,
    /// Tests the run is expected to run, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests_total: Option<usize>,
    /// Explore: `exploring`, `explored` (the session ended), `replaying`
    /// or `repairing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Explore: the planning turn (1-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    /// Explore: the operation the agent chose last (`tap`, `scroll`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// Explore: what it chose it on (the element's text or label).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// One failed test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub test: String,
    /// The failing step (`3: tap { id: "pay" }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<PathBuf>,
    /// The trace `inspect_trace` and `describe_failure` read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<PathBuf>,
}

/// The machine-readable result of a finished task.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    /// Passed only on a retry.
    pub flaky: usize,
    pub skipped: usize,
    pub failures: Vec<Failure>,
    /// Explore tasks: outcome, the YAML written, replays, model calls and
    /// tokens (the counts above are then the replay runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explore: Option<serde_json::Value>,
    /// Gate tasks: `{trusted, gates, file, goal}` from `atalanta gate --json`
    /// (the counts above are then the burn-in replays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<serde_json::Value>,
}

impl Summary {
    /// From a suite result (`results.json`).
    pub fn of(result: &atalanta_common::SuiteResult) -> Self {
        use atalanta_common::{StepStatus, TestStatus};
        let mut s = Summary {
            total: result.tests.len(),
            ..Default::default()
        };
        for t in &result.tests {
            match t.status {
                TestStatus::Passed => s.passed += 1,
                TestStatus::Retried => s.flaky += 1,
                TestStatus::Skipped => s.skipped += 1,
                TestStatus::Failed => {
                    s.failed += 1;
                    let step = t.steps.iter().find(|st| st.status == StepStatus::Failed);
                    s.failures.push(Failure {
                        test: t.test_name.clone(),
                        step: step.map(|st| format!("{}: {}", st.step_index + 1, st.action_name)),
                        error: t
                            .error_message
                            .clone()
                            .or_else(|| step.and_then(|st| st.error_message.clone()))
                            .map(|e| atalanta_common::redact::redact(&e).into_owned()),
                        screenshot: step
                            .and_then(|st| st.screenshot.clone())
                            .or_else(|| t.screenshots.last().cloned()),
                        trace: t.trace.clone(),
                    });
                }
            }
        }
        s
    }
}

/// What `task_status` shows.
#[derive(Debug, Clone, Serialize)]
pub struct TaskInfo {
    pub id: String,
    pub kind: TaskKind,
    pub state: TaskState,
    /// What runs (the suite, or the exploring goal).
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The task's folder: its log, artifacts, traces and `task.json`.
    pub dir: PathBuf,
    pub log: PathBuf,
    /// Artifacts root (`traces/` under it); `inspect_trace` takes the id.
    pub artifacts: PathBuf,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub progress: Progress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Instructions injected so far (explore tasks).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<Instruction>,
}

/// How a body ended.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub state: TaskState,
    pub summary: Option<Summary>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
}

/// What a running body gets.
pub struct TaskContext {
    pub id: String,
    pub dir: PathBuf,
    /// Becomes `true` when the task should stop.
    pub stop: watch::Receiver<bool>,
    /// Injected instructions ([`TaskKind::Explore`] only).
    pub instructions: Option<InstructionReceiver>,
    shared: Shared,
}

impl TaskContext {
    /// Publish how far the task got.
    pub fn set_progress(&self, progress: Progress) {
        if let Some(t) = lock(&self.shared).tasks.get_mut(&self.id) {
            t.info.progress = progress;
        }
    }
}

/// The work a task does.
#[async_trait::async_trait]
pub trait TaskBody: Send + 'static {
    fn kind(&self) -> TaskKind;
    /// Run to the end, or until `ctx.stop` turns true (then end soon,
    /// keeping what was done, with [`TaskState::Stopped`]).
    async fn run(self: Box<Self>, ctx: TaskContext) -> TaskOutcome;
}

/// What the manager needs to start a task.
pub struct TaskMeta {
    pub target: String,
    pub device: Option<String>,
    /// The task's folder (created by [`TaskManager::new_task_dir`]).
    pub dir: PathBuf,
    pub log: PathBuf,
    pub artifacts: PathBuf,
    pub tests_total: Option<usize>,
}

struct Entry {
    info: TaskInfo,
    stop: watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    instructions: Option<InstructionSender>,
}

#[derive(Default)]
struct Inner {
    tasks: BTreeMap<String, Entry>,
    seq: u64,
}

type Shared = Arc<Mutex<Inner>>;

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Inner> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Every task of this server.
pub struct TaskManager {
    shared: Shared,
    root: PathBuf,
    notifiers: Vec<Notifier>,
}

/// The list of files outside the tasks folder that tasks wrote.
const AGENT_WRITTEN: &str = "agent-written.txt";

/// How long shutdown waits for stopped tasks to write their reports.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(40);

impl TaskManager {
    /// Tasks keep their folders under `root`.
    pub fn new(root: PathBuf, notifiers: Vec<Notifier>) -> Self {
        Self {
            shared: Arc::default(),
            root,
            notifiers,
        }
    }

    /// The default root: `./atalanta-results/tasks`, or
    /// `~/.atalanta/mcp-tasks` when the server runs in `/` or `$HOME` (as
    /// desktop clients start it).
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cwd = std::env::current_dir().ok();
        match cwd {
            Some(c) if c.parent().is_some() && Some(&c) != home.as_ref() => {
                c.join("atalanta-results").join("tasks")
            }
            _ => home
                .unwrap_or_else(std::env::temp_dir)
                .join(".atalanta")
                .join("mcp-tasks"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Record that a task the agent started writes `path` (an explore
    /// task's `out`), so a later run of it gets the checks a file in the
    /// tasks folder gets, not those of a suite the user saved (review V12).
    /// Kept in the tasks folder, so a server restart doesn't forget it.
    pub fn mark_agent_written(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.root)
            .map_err(|e| AtalantaError::io(format!("task folder {}", self.root.display()), e))?;
        let list = self.root.join(AGENT_WRITTEN);
        let line = path.to_string_lossy().replace('\n', "");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&list)
            .and_then(|mut f| writeln!(f, "{line}"))
            .map_err(|e| AtalantaError::io(list.display(), e))
    }

    /// Whether an agent's task wrote `path`: it is under the tasks folder,
    /// or an explore task named it as its `out` ([`Self::mark_agent_written`]).
    /// `path` is canonical, as `tools::paths::confined` returns it.
    pub fn agent_written(&self, path: &Path) -> bool {
        let Ok(root) = std::fs::canonicalize(&self.root) else {
            return false;
        };
        if path.starts_with(&root) {
            return true;
        }
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        std::fs::read_to_string(root.join(AGENT_WRITTEN)).is_ok_and(|list| {
            list.lines()
                .any(|l| Path::new(l) == path || Path::new(l) == canon)
        })
    }

    /// A fresh id and its folder. The folder is made with `create_dir`: the
    /// root is shared with other servers of the project (each counts from 1),
    /// so an id whose folder exists is taken, and the next number is tried.
    pub fn new_task_dir(&self) -> Result<(String, PathBuf)> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| AtalantaError::io(format!("task folder {}", self.root.display()), e))?;
        loop {
            let id = {
                let mut inner = lock(&self.shared);
                inner.seq += 1;
                format!("t{}-{}", now_ms() / 1000, inner.seq)
            };
            let dir = self.root.join(&id);
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(AtalantaError::io(
                        format!("task folder {}", dir.display()),
                        e,
                    ))
                }
            }
        }
    }

    /// Start `body` in the background; returns at once.
    pub fn spawn(&self, id: String, meta: TaskMeta, body: Box<dyn TaskBody>) -> TaskInfo {
        let kind = body.kind();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (inject_tx, inject_rx) = if kind.takes_instructions() {
            let (tx, rx) = instruction_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let info = TaskInfo {
            id: id.clone(),
            kind,
            state: TaskState::Running,
            target: meta.target,
            device: meta.device,
            dir: meta.dir.clone(),
            log: meta.log,
            artifacts: meta.artifacts,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            progress: Progress {
                tests_total: meta.tests_total,
                ..Default::default()
            },
            summary: None,
            error: None,
            exit_code: None,
            instructions: Vec::new(),
        };
        write_task_file(&info);
        lock(&self.shared).tasks.insert(
            id.clone(),
            Entry {
                info: info.clone(),
                stop: stop_tx,
                handle: None,
                instructions: inject_tx,
            },
        );
        let ctx = TaskContext {
            id: id.clone(),
            dir: meta.dir,
            stop: stop_rx,
            instructions: inject_rx,
            shared: self.shared.clone(),
        };
        let shared = self.shared.clone();
        let notifiers = self.notifiers.clone();
        let handle = tokio::spawn(async move {
            // The body runs as its own task so a panic in it ends the task
            // as `error` instead of leaving it `running` (and its device
            // "busy") until the server restarts (review 03 H1). A child
            // process it started is killed with its process group on drop.
            let outcome = match tokio::spawn(body.run(ctx)).await {
                Ok(o) => o,
                Err(e) => TaskOutcome {
                    state: TaskState::Error,
                    summary: None,
                    error: Some(if e.is_panic() {
                        format!("the task crashed (a Atalanta bug): {}", panic_text(e))
                    } else {
                        "the task was cancelled".to_string()
                    }),
                    exit_code: None,
                },
            };
            let done = {
                let mut inner = lock(&shared);
                let Some(entry) = inner.tasks.get_mut(&id) else {
                    return;
                };
                let i = &mut entry.info;
                i.state = outcome.state;
                i.summary = outcome.summary;
                i.error = outcome
                    .error
                    .map(|e| atalanta_common::redact::redact(&e).into_owned());
                i.exit_code = outcome.exit_code;
                i.finished_at_ms = Some(now_ms());
                i.progress.test = None;
                i.progress.step = None;
                i.progress.label = None;
                entry.instructions = None;
                i.clone()
            };
            let file = write_task_file(&done);
            let value = serde_json::to_value(&done).unwrap_or_default();
            for n in &notifiers {
                n.notify(&value, &file).await;
            }
        });
        if let Some(e) = lock(&self.shared).tasks.get_mut(&info.id) {
            e.handle = Some(handle);
        }
        info
    }

    pub fn status(&self, id: &str) -> Result<TaskInfo> {
        let inner = lock(&self.shared);
        inner
            .tasks
            .get(id)
            .map(|e| e.info.clone())
            .ok_or_else(|| unknown(&inner, id))
    }

    /// Every task, oldest first.
    pub fn list(&self) -> Vec<TaskInfo> {
        lock(&self.shared)
            .tasks
            .values()
            .map(|e| e.info.clone())
            .collect()
    }

    /// The running task on `device`, if any.
    pub fn running_on(&self, device: &str) -> Option<String> {
        lock(&self.shared)
            .tasks
            .values()
            .find(|e| !e.info.state.is_done() && e.info.device.as_deref() == Some(device))
            .map(|e| e.info.id.clone())
    }
}

/// No task `id` (called with the lock held).
fn unknown(inner: &Inner, id: &str) -> AtalantaError {
    let known: Vec<String> = inner.tasks.keys().cloned().collect();
    AtalantaError::Config(if known.is_empty() {
        format!("no task '{id}': none was started in this session")
    } else {
        format!("no task '{id}'; tasks: {}", known.join(", "))
    })
}

impl TaskManager {
    /// Ask a task to stop; it ends as `stopped` once it wrote its reports.
    pub fn stop(&self, id: &str) -> Result<TaskInfo> {
        let inner = lock(&self.shared);
        let e = inner.tasks.get(id).ok_or_else(|| unknown(&inner, id))?;
        if !e.info.state.is_done() {
            let _ = e.stop.send(true);
        }
        Ok(e.info.clone())
    }

    /// Wait (up to `timeout`) until `id` is done.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<TaskInfo> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let info = self.status(id)?;
            if info.state.is_done() || tokio::time::Instant::now() >= deadline {
                return Ok(info);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Hand `text` to a task that takes instructions.
    pub fn inject(&self, id: &str, text: &str) -> Result<TaskInfo> {
        let mut inner = lock(&self.shared);
        if !inner.tasks.contains_key(id) {
            return Err(unknown(&inner, id));
        }
        let e = inner.tasks.get_mut(id).expect("checked");
        if !e.info.kind.takes_instructions() {
            return Err(AtalantaError::Config(format!(
                "task {id} is a {} task: instructions are not supported for this task kind \
                 (they steer explore tasks); stop it with task_stop and start another instead",
                serde_json::to_value(e.info.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default()
            )));
        }
        let instruction = Instruction::new(text);
        let sent = e
            .instructions
            .as_ref()
            .is_some_and(|tx| tx.send(instruction.clone()));
        if !sent {
            return Err(AtalantaError::Config(format!(
                "task {id} has ended ({:?}); it takes no more instructions",
                e.info.state
            )));
        }
        e.info.instructions.push(instruction);
        Ok(e.info.clone())
    }

    /// Stop every running task and wait for each to finish (reports,
    /// `task.json`, notifiers), up to a grace period.
    pub async fn shutdown(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut inner = lock(&self.shared);
            inner
                .tasks
                .values_mut()
                .filter_map(|e| {
                    if !e.info.state.is_done() {
                        let _ = e.stop.send(true);
                    }
                    e.handle.take()
                })
                .collect()
        };
        let all = futures_join_all(handles);
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, all).await;
    }
}

async fn futures_join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
    for h in handles {
        let _ = h.await;
    }
}

/// What a panicked body said, redacted later with the rest of the error.
fn panic_text(e: tokio::task::JoinError) -> String {
    let payload = e.into_panic();
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".into())
}

/// `<dir>/task.json`, best-effort.
fn write_task_file(info: &TaskInfo) -> PathBuf {
    let path = info.dir.join("task.json");
    if let Ok(json) = serde_json::to_vec_pretty(info) {
        // Best-effort, but said: a notifier is handed this path, and
        // task_status still answers from memory (review 03 L4).
        if let Err(e) = atalanta_common::fsutil::write_atomic(&path, &json) {
            tracing::warn!("task {}: writing {}: {e}", info.id, path.display());
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body that runs until stopped, or ends with `state` after `ms`.
    struct Fake {
        kind: TaskKind,
        ms: u64,
        state: TaskState,
    }

    #[async_trait::async_trait]
    impl TaskBody for Fake {
        fn kind(&self) -> TaskKind {
            self.kind
        }
        async fn run(self: Box<Self>, mut ctx: TaskContext) -> TaskOutcome {
            ctx.set_progress(Progress {
                test: Some("t".into()),
                step: Some(1),
                steps: Some(2),
                ..Default::default()
            });
            let mut got = Vec::new();
            let sleep = tokio::time::sleep(Duration::from_millis(self.ms));
            tokio::pin!(sleep);
            let state = loop {
                tokio::select! {
                    _ = &mut sleep => break self.state,
                    _ = ctx.stop.changed() => break TaskState::Stopped,
                    Some(i) = async { match ctx.instructions.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => got.push(i.text),
                }
            };
            TaskOutcome {
                state,
                summary: Some(Summary {
                    total: got.len(),
                    ..Default::default()
                }),
                error: None,
                exit_code: None,
            }
        }
    }

    fn manager(dir: &Path, notifiers: Vec<Notifier>) -> TaskManager {
        TaskManager::new(dir.to_path_buf(), notifiers)
    }

    fn start(m: &TaskManager, body: Fake) -> TaskInfo {
        let (id, dir) = m.new_task_dir().unwrap();
        let meta = TaskMeta {
            target: "suite.yaml".into(),
            device: Some("emulator-5554".into()),
            log: dir.join("run.log"),
            artifacts: dir.join("artifacts"),
            dir,
            tests_total: None,
        };
        m.spawn(id, meta, Box::new(body))
    }

    /// A body that panics.
    struct Panics;

    #[async_trait::async_trait]
    impl TaskBody for Panics {
        fn kind(&self) -> TaskKind {
            TaskKind::Run
        }
        async fn run(self: Box<Self>, _ctx: TaskContext) -> TaskOutcome {
            panic!("boom in the body")
        }
    }

    // Review 03 H1: a panicking body used to stay `running` and keep its
    // device busy until the server restarted.
    #[tokio::test]
    async fn a_panicking_task_ends_as_an_error_and_frees_its_device() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manager(tmp.path(), vec![]);
        let (id, dir) = m.new_task_dir().unwrap();
        let meta = TaskMeta {
            target: "suite.yaml".into(),
            device: Some("emulator-5554".into()),
            log: dir.join("run.log"),
            artifacts: dir.join("artifacts"),
            dir: dir.clone(),
            tests_total: None,
        };
        let info = m.spawn(id, meta, Box::new(Panics));
        let done = m.wait(&info.id, Duration::from_secs(10)).await.unwrap();
        assert_eq!(done.state, TaskState::Error, "{done:?}");
        assert!(done.finished_at_ms.is_some());
        let err = done.error.unwrap();
        assert!(
            err.contains("crashed") && err.contains("boom in the body"),
            "{err}"
        );
        assert_eq!(m.running_on("emulator-5554"), None);
        assert!(dir.join("task.json").exists());
    }

    // Two servers started in one project share the tasks folder, and each
    // numbers its tasks from 1: in the same second they made the same id and
    // one folder (one `events.jsonl`, one log) for two runs.
    #[test]
    fn two_servers_on_one_tasks_folder_never_share_a_task_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (manager(tmp.path(), vec![]), manager(tmp.path(), vec![]));
        let mut ids = std::collections::HashSet::new();
        for _ in 0..20 {
            for m in [&a, &b] {
                let (id, dir) = m.new_task_dir().unwrap();
                assert!(dir.is_dir());
                assert!(ids.insert(id.clone()), "{id} was made twice");
            }
        }
    }

    #[tokio::test]
    async fn a_task_runs_in_the_background_and_reports_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let jsonl = tmp.path().join("done.jsonl");
        let m = manager(tmp.path(), vec![Notifier::Jsonl(jsonl.clone())]);
        let info = start(
            &m,
            Fake {
                kind: TaskKind::Run,
                ms: 300,
                state: TaskState::Passed,
            },
        );
        assert_eq!(info.state, TaskState::Running, "returned at once");
        assert_eq!(m.running_on("emulator-5554"), Some(info.id.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(m.status(&info.id).unwrap().progress.step, Some(1));
        let done = m.wait(&info.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.state, TaskState::Passed);
        assert!(done.finished_at_ms.is_some());
        // The notifier ran after the state was final.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let line = std::fs::read_to_string(&jsonl).unwrap();
        assert!(line.contains("\"state\":\"passed\""), "{line}");
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(done.dir.join("task.json")).unwrap()).unwrap();
        assert_eq!(file["state"], "passed");
        assert!(m.status("nope").is_err());
    }

    #[tokio::test]
    async fn stop_and_shutdown_end_tasks_as_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manager(tmp.path(), vec![]);
        let slow = || Fake {
            kind: TaskKind::Run,
            ms: 60_000,
            state: TaskState::Passed,
        };
        let a = start(&m, slow());
        let b = start(&m, slow());
        m.stop(&a.id).unwrap();
        assert_eq!(
            m.wait(&a.id, Duration::from_secs(5)).await.unwrap().state,
            TaskState::Stopped
        );
        m.shutdown().await;
        assert_eq!(m.status(&b.id).unwrap().state, TaskState::Stopped);
    }

    #[tokio::test]
    async fn instructions_reach_explore_tasks_only() {
        let tmp = tempfile::tempdir().unwrap();
        let m = manager(tmp.path(), vec![]);
        let run = start(
            &m,
            Fake {
                kind: TaskKind::Run,
                ms: 60_000,
                state: TaskState::Passed,
            },
        );
        let e = m.inject(&run.id, "go to settings").unwrap_err();
        assert!(
            e.to_string().contains("not supported for this task kind"),
            "{e}"
        );
        let explore = start(
            &m,
            Fake {
                kind: TaskKind::Explore,
                ms: 400,
                state: TaskState::Passed,
            },
        );
        m.inject(&explore.id, "open wifi").unwrap();
        m.inject(&explore.id, "then back").unwrap();
        let done = m.wait(&explore.id, Duration::from_secs(5)).await.unwrap();
        assert_eq!(done.summary.unwrap().total, 2, "both instructions arrived");
        assert_eq!(done.instructions.len(), 2);
        assert!(m.inject(&explore.id, "late").is_err());
        m.shutdown().await;
    }
}
