//! Live events for whoever supervises a run: the MCP server's `run_task`
//! and the inspector console's Run and Explore panels. One mechanism for
//! both: the supervisor starts `velocity run` (or `velocity explore`) with
//! `VELOCITY_EVENTS_FILE=<path>` and tails that file. Every test and step
//! start and end is appended to it as one JSON line, written at once.
//! Unset (the default), nothing is opened or written.
//!
//! ```text
//! {"event":"testStarted","test":"login","device":"emulator-5554","steps":6,"t":1761360000123}
//! {"event":"stepStarted","test":"login","index":0,"total":6,"step":"launchApp","t":…}
//! {"event":"stepFinished","test":"login","index":0,"total":6,"step":"launchApp","status":"passed","ms":812,"error":null,"t":…}
//! {"event":"testFinished","test":"login","status":"failed","ms":5120,"error":"…","t":…}
//! ```
//!
//! `index` is 0-based. `velocity explore` writes its own events to the same
//! file through [`emit`] (`{"event":"planned","turn":3,"op":"tap",…}`, see
//! `velocity_explore::ExploreEvent`); its replays add the test events above.
//!
//! Best-effort: a file that can't be written never fails the run. Labels and
//! errors are masked like any other output (a step label can quote typed
//! text).

use std::io::Write;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::{StepResult, TestResult};

/// The environment variable naming the events file.
pub const EVENTS_FILE_ENV: &str = "VELOCITY_EVENTS_FILE";

/// The open file, with the path it was opened for (the variable is read on
/// each event, so a test that sets it later still gets its events).
static SINK: Mutex<Option<(String, std::fs::File)>> = Mutex::new(None);

fn path() -> Option<String> {
    let p = crate::env::var(EVENTS_FILE_ENV).ok()?;
    let p = p.trim();
    (!p.is_empty()).then(|| p.to_string())
}

/// Whether events are being written (so callers can skip building them).
pub fn enabled() -> bool {
    path().is_some()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One JSON line, as [`emit`] writes it (with a `t` in Unix ms).
pub fn line(mut event: Value) -> String {
    if let Some(o) = event.as_object_mut() {
        o.entry("t").or_insert(json!(now_ms()));
    }
    let mut s = event.to_string();
    s.push('\n');
    s
}

/// Append `event` to the events file, if there is one.
pub fn emit(mut event: Value) {
    let Some(path) = path() else {
        return;
    };
    // Every writer goes through here, an exploration's own events included:
    // mask registered secrets and secret shapes in every string field, not
    // only in the typed test and step helpers (second-pass review).
    crate::redact::redact_value(&mut event);
    let mut sink = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if sink.as_ref().is_none_or(|(p, _)| *p != path) {
        let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return;
        };
        *sink = Some((path, f));
    }
    if let Some((_, f)) = sink.as_mut() {
        // One write per line, under the lock: lines never interleave. A full
        // disk must not fail the run: the events are a view only. But a
        // supervisor's progress comes from this file, so the first failure
        // is said once (review 03 L4).
        if let Err(e) = f.write_all(line(event).as_bytes()) {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(error = %e, "can't write the live events file; progress stops updating");
            }
        }
    }
}

fn masked(s: &str) -> String {
    crate::redact::redact(s).into_owned()
}

/// The test and step events this module writes, typed: the one schema the
/// writers here and the readers (MCP tasks, the console) share (review 02
/// M4). Lines it doesn't know (an exploration's own events, which
/// `velocity_explore::ExploreEvent` types, or one added later) read as
/// [`LiveEvent::Other`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
#[non_exhaustive]
pub enum LiveEvent {
    TestStarted {
        test: String,
        device: String,
        steps: usize,
    },
    StepStarted {
        test: String,
        index: usize,
        total: usize,
        step: String,
    },
    StepFinished {
        test: String,
        index: usize,
        total: usize,
        step: String,
        status: crate::StepStatus,
        ms: u64,
        error: Option<String>,
    },
    TestFinished {
        test: String,
        status: FinishedStatus,
        /// Absent when the attempt ended in an error before a result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
        error: Option<String>,
    },
    /// Any other line (an exploration's events).
    #[serde(other)]
    Other,
}

impl LiveEvent {
    /// The `event` names of the test and step events, for a reader that
    /// routes them apart from an exploration's own.
    pub const TEST_EVENTS: [&'static str; 4] =
        ["testStarted", "stepStarted", "stepFinished", "testFinished"];

    /// Read one events line; `None` for one that isn't JSON.
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }
}

/// How a test attempt ended, as `testFinished` says: a [`crate::TestStatus`],
/// or `error` when it ended in an error before a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FinishedStatus {
    Passed,
    Failed,
    Skipped,
    Retried,
    Error,
}

impl FinishedStatus {
    /// Whether the attempt counts as failed: anything but passed, passed
    /// on a retry, or skipped. Readers ask this rather than keep their own
    /// list of passing strings, so a status added here is decided here.
    pub fn is_failure(self) -> bool {
        !matches!(self, Self::Passed | Self::Retried | Self::Skipped)
    }
}

impl From<crate::TestStatus> for FinishedStatus {
    fn from(s: crate::TestStatus) -> Self {
        match s {
            crate::TestStatus::Passed => Self::Passed,
            crate::TestStatus::Failed => Self::Failed,
            crate::TestStatus::Skipped => Self::Skipped,
            crate::TestStatus::Retried => Self::Retried,
        }
    }
}

fn emit_typed(event: &LiveEvent) {
    if let Ok(v) = serde_json::to_value(event) {
        emit(v);
    }
}

/// A test (or a retry of it) with `steps` steps starts on `device`.
pub fn test_started(test: &str, device: &str, steps: usize) {
    if enabled() {
        emit_typed(&LiveEvent::TestStarted {
            test: test.to_string(),
            device: device.to_string(),
            steps,
        });
    }
}

/// Step `index` (0-based) of `total` starts.
pub fn step_started(test: &str, index: usize, total: usize, step: &str) {
    if enabled() {
        emit_typed(&LiveEvent::StepStarted {
            test: test.to_string(),
            index,
            total,
            step: masked(step),
        });
    }
}

/// The event for a finished step.
pub fn step_finished_event(test: &str, total: usize, r: &StepResult) -> Value {
    serde_json::to_value(LiveEvent::StepFinished {
        test: test.to_string(),
        index: r.step_index,
        total,
        step: masked(&r.action_name),
        status: r.status,
        ms: r.duration.as_millis() as u64,
        error: r.error_message.as_deref().map(masked),
    })
    .unwrap_or(Value::Null)
}

pub fn step_finished(test: &str, total: usize, r: &StepResult) {
    if enabled() {
        emit(step_finished_event(test, total, r));
    }
}

/// The event for a finished test attempt: its result, or the error that
/// ended it (status `error`).
pub fn test_finished_event(
    test: &str,
    result: std::result::Result<&TestResult, String>,
) -> LiveEvent {
    match result {
        Ok(r) => LiveEvent::TestFinished {
            test: test.to_string(),
            status: r.status.into(),
            ms: Some(r.duration.as_millis() as u64),
            error: r.error_message.as_deref().map(masked),
        },
        Err(e) => LiveEvent::TestFinished {
            test: test.to_string(),
            status: FinishedStatus::Error,
            ms: None,
            error: Some(masked(&e)),
        },
    }
}

/// A test attempt ended: its result, or the error that ended it (status
/// `error`).
pub fn test_finished(test: &str, result: std::result::Result<&TestResult, String>) {
    if enabled() {
        emit_typed(&test_finished_event(test, result));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StepStatus;
    use std::time::Duration;

    #[test]
    fn a_step_event_is_one_json_line_with_a_time() {
        let r = StepResult {
            step_index: 2,
            action_name: "tap \"Pay\"\nnow".into(),
            status: StepStatus::Failed,
            duration: Duration::from_millis(42),
            screenshot: None,
            error_message: Some("not found".into()),
            resource_delta: None,
            settle_ms: None,
            source: None,
        };
        let l = line(step_finished_event("checkout", 5, &r));
        assert!(l.ends_with('\n') && l.matches('\n').count() == 1, "{l}");
        let v: Value = serde_json::from_str(l.trim()).unwrap();
        assert_eq!(v["event"], "stepFinished");
        assert_eq!(v["index"], 2);
        assert_eq!(v["total"], 5);
        assert_eq!(v["status"], "failed");
        assert_eq!(v["ms"], 42);
        assert_eq!(v["error"], "not found");
        assert!(v["t"].as_u64().unwrap() > 0);
        // ...and reads back as the same typed event.
        match LiveEvent::parse(l.trim()) {
            Some(LiveEvent::StepFinished { index, status, .. }) => {
                assert_eq!((index, status), (2, StepStatus::Failed));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_test_attempt_that_errored_is_a_failure_with_no_time() {
        let e = test_finished_event("pay", Err("device gone".into()));
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["event"], "testFinished");
        assert_eq!(v["status"], "error");
        assert!(v.get("ms").is_none(), "{v}");
        let LiveEvent::TestFinished { status, .. } = e else {
            panic!("{e:?}")
        };
        assert!(status.is_failure());
        for (s, failed) in [
            (crate::TestStatus::Passed, false),
            (crate::TestStatus::Retried, false),
            (crate::TestStatus::Skipped, false),
            (crate::TestStatus::Failed, true),
        ] {
            assert_eq!(FinishedStatus::from(s).is_failure(), failed, "{s:?}");
        }
    }

    #[test]
    fn an_exploration_line_reads_as_other() {
        let l = r#"{"event":"planned","turn":3,"op":"tap","t":1}"#;
        assert_eq!(LiveEvent::parse(l), Some(LiveEvent::Other));
        let l = r#"{"event":"testStarted","test":"a","device":"d","steps":2,"t":1}"#;
        assert!(matches!(
            LiveEvent::parse(l),
            Some(LiveEvent::TestStarted { steps: 2, .. })
        ));
    }

    #[test]
    fn second_pass_emit_masks_secrets_in_explore_events() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("events.jsonl");
        let mut env = crate::env::settings_snapshot();
        env.insert(EVENTS_FILE_ENV.into(), file.to_string_lossy().into_owned());
        let before = crate::env::settings_snapshot();
        crate::env::set_settings(env);
        crate::redact::register_secret("sp-live-secret-9917");
        emit(json!({"event":"planned","turn":1,"op":"type","element":"field sp-live-secret-9917"}));
        crate::env::set_settings(before);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("sp-live-secret-9917"), "{text}");
    }
}
