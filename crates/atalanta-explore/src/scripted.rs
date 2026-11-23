//! A scripted planner and checker: no model, a fixed plan.
//!
//! For tests, demos and device checks without API keys (`atalanta explore
//! --script plan.json`). The script names targets by their text or label;
//! each turn the planner reads the numbered screen list exactly as a model
//! would and answers with the element's number, so observing, grounding,
//! acting, settling, the assertions and the replay all run for real. Only
//! the model's judgement is replaced.
//!
//! ```json
//! {
//!   "steps": [
//!     { "op": "tap", "target": "Network & internet", "scroll": "down" },
//!     { "op": "assert", "target": "Internet" },
//!     { "op": "done", "outcome": "success", "reason": "the page is open" }
//!   ],
//!   "check": { "holds": true, "evidence": ["Internet"], "reason": "shown" }
//! }
//! ```
//!
//! `scroll` (optional): when the target isn't listed, scroll that way
//! (up to 8 times) instead of giving up.
//!
//! `expect` (optional): the step's expected post-condition, elements named
//! by text as `target` is: `{"appears": "Saved"}`, `{"gone": "Battery"}`,
//! `{"checked": "Wi-Fi"}`, `{"unchecked": "Wi-Fi"}`.

use std::sync::Mutex;

use async_trait::async_trait;
use atalanta_common::{AtalantaError, Result};
use atalanta_nl::checker::{CheckAnswer, CheckRequest, ScreenChecker};
use atalanta_nl::planner::{NaturalPlanner, PlanRequest, PlannedStep, Usage, UsageMeter};
use atalanta_nl::{Expectation, GoalOp, GoalOutcome, GoalRequest, GoalStep};
use serde::Deserialize;

/// One scripted turn.
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptStep {
    pub op: GoalOp,
    /// The element's text or label (exact, else contained).
    #[serde(default)]
    pub target: Option<String>,
    /// A value name for inputText.
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub direction: String,
    #[serde(default)]
    pub ms: u64,
    #[serde(default)]
    pub outcome: GoalOutcome,
    #[serde(default)]
    pub reason: String,
    /// Scroll this way while the target isn't listed.
    #[serde(default)]
    pub scroll: Option<String>,
    /// A raw element number (tests that want a bogus pick).
    #[serde(default)]
    pub element: Option<i64>,
    /// The memo the "model" keeps for its next turns.
    #[serde(default)]
    pub memo: String,
    /// What the step should cause, elements named by text.
    #[serde(default)]
    pub expect: Option<ScriptExpect>,
}

/// A scripted expectation; resolved against the screen list each turn as a
/// model would, so a name not listed sends a number code can't use.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScriptExpect {
    #[serde(default)]
    pub appears: Option<String>,
    #[serde(default)]
    pub gone: Option<String>,
    #[serde(default)]
    pub checked: Option<String>,
    #[serde(default)]
    pub unchecked: Option<String>,
}

impl ScriptExpect {
    fn resolve(&self, screen: &str) -> Option<Expectation> {
        let number = |t: &str| find_in_list(screen, t).unwrap_or(usize::MAX);
        if let Some(text) = &self.appears {
            return Some(Expectation::Appears { text: text.clone() });
        }
        if let Some(t) = &self.gone {
            return Some(Expectation::Gone { element: number(t) });
        }
        if let Some(t) = &self.checked {
            return Some(Expectation::Checked {
                element: number(t),
                value: true,
            });
        }
        self.unchecked.as_ref().map(|t| Expectation::Checked {
            element: number(t),
            value: false,
        })
    }
}

/// The scripted checker's verdict.
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptCheck {
    pub holds: bool,
    /// Texts or labels of the elements cited; mapped to their numbers.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Raw numbers cited as well (tests that cite elements not listed).
    #[serde(default)]
    pub evidence_ids: Vec<i64>,
    /// Log turns cited (an end state shown only by the run log).
    #[serde(default)]
    pub evidence_turns: Vec<i64>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Script {
    pub steps: Vec<ScriptStep>,
    #[serde(default)]
    pub check: Option<ScriptCheck>,
}

impl Script {
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text)
            .map_err(|e| AtalantaError::Config(format!("explore script: {e}")))
    }
}

/// Plays a [`Script`] as a planner. Every turn counts as a call (with no
/// tokens), so "zero model calls on replay" is checkable against it.
pub struct ScriptedPlanner {
    steps: Vec<ScriptStep>,
    next: Mutex<(usize, usize)>,
    usage: UsageMeter,
    /// Every goal request's notes, for tests (injected instructions).
    pub seen_notes: Mutex<Vec<String>>,
    /// Every goal request's history, for tests.
    pub seen_history: Mutex<Vec<String>>,
    /// Whether each goal request carried a screenshot.
    pub seen_shots: Mutex<Vec<bool>>,
    /// Every goal request's memo, for tests.
    pub seen_memos: Mutex<Vec<String>>,
    /// Every goal request's values list, for tests.
    pub seen_values: Mutex<Vec<String>>,
    /// Every goal request's demonstrations, for tests.
    pub seen_demos: Mutex<Vec<String>>,
    /// Whether each goal request asked for the benchmark instructions.
    pub seen_benchmark: Mutex<Vec<bool>>,
    /// Every goal request's screen list, for tests.
    pub seen_screens: Mutex<Vec<String>>,
}

impl ScriptedPlanner {
    pub fn new(script: &Script) -> Self {
        Self {
            steps: script.steps.clone(),
            next: Mutex::new((0, 0)),
            usage: UsageMeter::default(),
            seen_notes: Mutex::new(vec![]),
            seen_history: Mutex::new(vec![]),
            seen_shots: Mutex::new(vec![]),
            seen_memos: Mutex::new(vec![]),
            seen_values: Mutex::new(vec![]),
            seen_demos: Mutex::new(vec![]),
            seen_benchmark: Mutex::new(vec![]),
            seen_screens: Mutex::new(vec![]),
        }
    }
}

/// The number of the listed element whose label or text is `target`
/// (exactly, else containing it).
pub fn find_in_list(screen: &str, target: &str) -> Option<usize> {
    let number =
        |line: &str| -> Option<usize> { line.strip_prefix('[')?.split(']').next()?.parse().ok() };
    let quoted = format!("{target:?}");
    let exact = screen
        .lines()
        .find(|l| l.contains(&format!("label={quoted}")) || l.contains(&format!("text={quoted}")));
    exact
        .or_else(|| screen.lines().find(|l| l.contains(target)))
        .and_then(number)
}

#[async_trait]
impl NaturalPlanner for ScriptedPlanner {
    async fn plan(&self, _req: &PlanRequest<'_>) -> Result<PlannedStep> {
        Err(AtalantaError::Config(
            "the scripted planner only plays explore scripts".into(),
        ))
    }

    async fn plan_goal(&self, req: &GoalRequest<'_>) -> Result<GoalStep> {
        self.usage.record(0, 0);
        self.seen_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.notes.to_string());
        self.seen_history
            .lock()
            .unwrap()
            .push(req.history.to_string());
        self.seen_shots
            .lock()
            .unwrap()
            .push(req.screenshot.is_some());
        self.seen_benchmark
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.benchmark);
        self.seen_memos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.memo.to_string());
        self.seen_values
            .lock()
            .unwrap()
            .push(req.values.to_string());
        self.seen_demos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.demos.to_string());
        self.seen_screens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.screen.to_string());
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = self.steps.get(next.0).cloned() else {
            return Ok(GoalStep {
                op: GoalOp::Done,
                element: None,
                value: String::new(),
                direction: String::new(),
                ms: 0,
                outcome: GoalOutcome::GiveUp,
                reason: "the script ran out of steps".into(),
                expect: None,
                memo: String::new(),
            });
        };
        let mut element = s.element.and_then(|e| usize::try_from(e).ok());
        if element.is_none() {
            if let Some(t) = &s.target {
                element = find_in_list(req.screen, t);
                if element.is_none() {
                    if let Some(dir) = s.scroll.as_ref().filter(|_| next.1 < 8) {
                        next.1 += 1;
                        return Ok(GoalStep {
                            op: GoalOp::Scroll,
                            element: None,
                            value: String::new(),
                            direction: dir.clone(),
                            ms: 0,
                            outcome: GoalOutcome::None,
                            reason: format!("looking for {t:?}"),
                            expect: None,
                            memo: String::new(),
                        });
                    }
                    next.0 += 1;
                    next.1 = 0;
                    return Ok(GoalStep {
                        op: GoalOp::Done,
                        element: None,
                        value: String::new(),
                        direction: String::new(),
                        ms: 0,
                        outcome: GoalOutcome::GiveUp,
                        reason: format!("scripted target {t:?} is not on the screen"),
                        expect: None,
                        memo: String::new(),
                    });
                }
            }
        }
        next.0 += 1;
        next.1 = 0;
        Ok(GoalStep {
            op: s.op,
            element,
            value: s.value,
            direction: if s.direction.is_empty() {
                "none".into()
            } else {
                s.direction
            },
            ms: s.ms,
            outcome: s.outcome,
            reason: s.reason,
            expect: s.expect.as_ref().and_then(|e| e.resolve(req.screen)),
            memo: s.memo,
        })
    }

    fn usage(&self) -> Usage {
        self.usage.get()
    }
}

/// Answers checks with the script's verdict, citing elements by text.
pub struct ScriptedChecker {
    check: ScriptCheck,
    usage: UsageMeter,
}

impl ScriptedChecker {
    pub fn new(check: ScriptCheck) -> Self {
        Self {
            check,
            usage: UsageMeter::default(),
        }
    }
}

#[async_trait]
impl ScreenChecker for ScriptedChecker {
    async fn check(&self, req: &CheckRequest<'_>) -> Result<CheckAnswer> {
        self.usage.record(0, 0);
        let mut ids: Vec<i64> = self
            .check
            .evidence
            .iter()
            .filter_map(|t| find_in_list(req.screen, t))
            .map(|n| n as i64)
            .collect();
        ids.extend(&self.check.evidence_ids);
        Ok(CheckAnswer {
            holds: self.check.holds,
            evidence_element_ids: ids,
            evidence_turns: self.check.evidence_turns.clone(),
            reason: self.check.reason.clone(),
        })
    }

    fn usage(&self) -> Usage {
        self.usage.get()
    }
}
