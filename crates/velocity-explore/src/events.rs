//! What an explore session reports while it runs, and how instructions
//! reach it from outside (the MCP server's `task_inject`, a console).
//!
//! The library never prints: the CLI turns these events into its live
//! lines, the MCP server into task progress (its stdout carries JSON-RPC).

use serde::Serialize;

/// One thing that happened in a session. Every event carries `atMs`, the
/// milliseconds since the session started. Serialized with an `event` tag
/// (`{"event":"acted","atMs":1234,…}`) for JSON lines and MCP progress.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ExploreEvent {
    /// A step from the start of the run was replayed without the model
    /// (the launch, or the prefix a repair starts from).
    #[serde(rename_all = "camelCase")]
    Prefix {
        at_ms: u64,
        step: usize,
        action: String,
        ok: bool,
        ms: u64,
    },
    /// The screen was read.
    #[serde(rename_all = "camelCase")]
    Observed {
        at_ms: u64,
        turn: usize,
        title: String,
        elements: usize,
        hash: String,
        /// The turn this screen was first seen at, when it isn't new.
        seen_at: Option<usize>,
        screenshot_sent: bool,
    },
    /// The model chose an operation.
    #[serde(rename_all = "camelCase")]
    Planned {
        at_ms: u64,
        turn: usize,
        op: String,
        element: Option<usize>,
        target: Option<String>,
        reason: String,
        model_ms: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Code refused the model's choice; the model is told why.
    #[serde(rename_all = "camelCase")]
    Rejected {
        at_ms: u64,
        turn: usize,
        why: String,
    },
    /// The action ran on the device.
    #[serde(rename_all = "camelCase")]
    Acted {
        at_ms: u64,
        turn: usize,
        action: String,
        ok: bool,
        ms: u64,
        settle_ms: Option<u64>,
        changed: bool,
        error: Option<String>,
        /// The model's expected post-condition and how code judged it on
        /// the screen the action led to.
        #[serde(skip_serializing_if = "Option::is_none")]
        expectation: Option<crate::expect::ExpectCheck>,
    },
    /// An assertion the model proposed, built and run by code.
    #[serde(rename_all = "camelCase")]
    Asserted {
        at_ms: u64,
        turn: usize,
        assertion: String,
        ok: bool,
    },
    /// The model answered a question goal and code accepted the answer.
    #[serde(rename_all = "camelCase")]
    Answered {
        at_ms: u64,
        turn: usize,
        answer: String,
        /// Where each comma-separated part was grounded (`screen`,
        /// `number`, `judgement`, `words`, …).
        sources: Vec<String>,
    },
    /// An AI check ran (the goal at the end; checkpoints).
    #[serde(rename_all = "camelCase")]
    Checked {
        at_ms: u64,
        claim: String,
        outcome: String,
        evidence: Vec<String>,
        reason: String,
    },
    /// An instruction from outside was taken in at a planning turn.
    #[serde(rename_all = "camelCase")]
    Injected {
        at_ms: u64,
        turn: usize,
        instruction: String,
    },
    /// The run noticed something the model is told about (stuck, loop).
    #[serde(rename_all = "camelCase")]
    Note {
        at_ms: u64,
        turn: usize,
        note: String,
    },
    /// One replay of the generated test finished (no model calls).
    #[serde(rename_all = "camelCase")]
    ReplayRun {
        at_ms: u64,
        run: usize,
        passed: bool,
        ms: u64,
        steps: usize,
        /// The failing step (0-based in the test's steps).
        failed_step: Option<usize>,
        error: Option<String>,
        model_calls: u64,
    },
    /// The replay failed; one repair round starts.
    #[serde(rename_all = "camelCase")]
    Repair {
        at_ms: u64,
        /// What the repair does: drop an unstable assertion, or explore on
        /// from the recorded step before the failure.
        action: String,
        from_step: Option<usize>,
        reason: String,
    },
    /// The session ended.
    #[serde(rename_all = "camelCase")]
    Finished {
        at_ms: u64,
        outcome: crate::session::Outcome,
        reason: String,
        turns: usize,
        actions: usize,
        model_calls: u64,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
        /// How the model's expected post-conditions turned out.
        expectations: crate::expect::ExpectationCounts,
    },
}

/// Where events go: a callback. The CLI prints them; an MCP adapter
/// forwards them as task progress.
pub type EventSink = Box<dyn FnMut(&ExploreEvent) + Send>;

/// Instructions from outside, taken at the next planning turn.
///
/// An MCP task's `InstructionReceiver` (fed by `task_inject`) adapts with a
/// three-line impl; tokio and std channels work as they are.
pub trait InstructionSource: Send {
    /// Every instruction waiting now, oldest first. Never blocks.
    fn drain(&mut self) -> Vec<String>;
}

impl InstructionSource for tokio::sync::mpsc::UnboundedReceiver<String> {
    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(s) = self.try_recv() {
            out.push(s);
        }
        out
    }
}

impl InstructionSource for std::sync::mpsc::Receiver<String> {
    fn drain(&mut self) -> Vec<String> {
        self.try_iter().collect()
    }
}
