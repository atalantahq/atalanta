//! Instructions an agent injects into a running task (`task_inject`).
//!
//! Only tasks that steer themselves read them: `velocity explore` takes an
//! [`InstructionReceiver`] through its [`super::TaskContext`] and folds what
//! arrives into its next plan. A plain suite run has no use for them, and
//! `task_inject` says so instead of queueing words nobody reads.
//!
//! The instruction is the agent's text, passed through as text: whoever
//! consumes it must treat it as a goal to plan from, never as a selector or
//! a value (the rule `velocity-nl` follows).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::mpsc;

/// One injected instruction.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Instruction {
    pub text: String,
    /// When the server received it, in ms since the Unix epoch.
    pub received_at_ms: u64,
}

impl Instruction {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            received_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        }
    }
}

/// The server's end: `task_inject` sends here.
#[derive(Debug, Clone)]
pub struct InstructionSender(mpsc::UnboundedSender<Instruction>);

/// The task's end.
#[derive(Debug)]
pub struct InstructionReceiver(mpsc::UnboundedReceiver<Instruction>);

/// A connected pair.
pub fn instruction_channel() -> (InstructionSender, InstructionReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    (InstructionSender(tx), InstructionReceiver(rx))
}

impl InstructionSender {
    /// `false` when the task no longer listens (it ended).
    pub fn send(&self, instruction: Instruction) -> bool {
        self.0.send(instruction).is_ok()
    }
}

impl InstructionReceiver {
    /// The next instruction, if one is waiting.
    pub fn try_recv(&mut self) -> Option<Instruction> {
        self.0.try_recv().ok()
    }

    /// Wait for the next instruction; `None` once the server dropped its end.
    pub async fn recv(&mut self) -> Option<Instruction> {
        self.0.recv().await
    }

    /// Everything waiting now, oldest first.
    pub fn drain(&mut self) -> Vec<Instruction> {
        std::iter::from_fn(|| self.try_recv()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn instructions_arrive_in_order() {
        let (tx, mut rx) = instruction_channel();
        assert!(tx.send(Instruction::new("open settings")));
        assert!(tx.send(Instruction::new("then wifi")));
        let got: Vec<String> = rx.drain().into_iter().map(|i| i.text).collect();
        assert_eq!(got, ["open settings", "then wifi"]);
        assert!(rx.try_recv().is_none());
        drop(rx);
        assert!(!tx.send(Instruction::new("late")), "nobody listens");
    }
}
