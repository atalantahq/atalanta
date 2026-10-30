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
pub fn emit(event: Value) {
    let Some(path) = path() else {
        return;
    };
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
        // disk must not fail the run: the events are a view only.
        let _ = f.write_all(line(event).as_bytes());
    }
}

fn masked(s: &str) -> String {
    crate::redact::redact(s).into_owned()
}

/// A test (or a retry of it) with `steps` steps starts on `device`.
pub fn test_started(test: &str, device: &str, steps: usize) {
    if enabled() {
        emit(json!({"event": "testStarted", "test": test, "device": device, "steps": steps}));
    }
}

/// Step `index` (0-based) of `total` starts.
pub fn step_started(test: &str, index: usize, total: usize, step: &str) {
    if enabled() {
        emit(json!({
            "event": "stepStarted", "test": test, "index": index, "total": total,
            "step": masked(step),
        }));
    }
}

/// The event for a finished step.
pub fn step_finished_event(test: &str, total: usize, r: &StepResult) -> Value {
    json!({
        "event": "stepFinished",
        "test": test,
        "index": r.step_index,
        "total": total,
        "step": masked(&r.action_name),
        "status": r.status,
        "ms": r.duration.as_millis() as u64,
        "error": r.error_message.as_deref().map(masked),
    })
}

pub fn step_finished(test: &str, total: usize, r: &StepResult) {
    if enabled() {
        emit(step_finished_event(test, total, r));
    }
}

/// A test attempt ended: its result, or the error that ended it (status
/// `error`).
pub fn test_finished(test: &str, result: std::result::Result<&TestResult, String>) {
    if !enabled() {
        return;
    }
    emit(match result {
        Ok(r) => json!({
            "event": "testFinished", "test": test, "status": r.status,
            "ms": r.duration.as_millis() as u64,
            "error": r.error_message.as_deref().map(masked),
        }),
        Err(e) => json!({
            "event": "testFinished", "test": test, "status": "error", "error": masked(&e),
        }),
    });
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
    }
}
