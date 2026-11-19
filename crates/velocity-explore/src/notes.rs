//! What the runner tells the model at the start of a turn, as decisions.
//!
//! Each turn's notes are values of [`TurnNote`]: code's decision ("the last
//! action had no visible effect", "this screen closes a loop of 2 turns"),
//! rendered into the prompt's prose in one place ([`TurnNote::render`]) and
//! reported as they are in the `notes` event. Tests assert on the decision,
//! not on a sentence: rewording a note changes one match arm here and no
//! test (review 07 H1).

use serde::Serialize;

/// Why an action that worked is named as having no visible effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NoEffect {
    /// A scroll or swipe: the content did not move.
    Moved,
    /// A tap in a web page, which can set state the page doesn't show.
    WebTap,
    /// Anything else: the screen is unchanged.
    Unchanged,
}

/// One note for the model's next turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[non_exhaustive]
pub enum TurnNote {
    /// An instruction from the operator (`task_inject`, the console).
    #[serde(rename_all = "camelCase")]
    Operator { instruction: String },
    /// Back on a screen seen `turns_ago` turns ago (at `at_turn`), with the
    /// same elements.
    #[serde(rename_all = "camelCase")]
    LoopBack { turns_ago: usize, at_turn: usize },
    /// A screen seen before, longer ago than a loop.
    #[serde(rename_all = "camelCase")]
    SeenBefore { after_turn: usize },
    /// A `back` from this screen already led round, with nothing learned on
    /// the way: one more `back` here is refused.
    BackLedRound,
    /// Code refused the model's last answer.
    Rejected { why: String },
    /// The model said done(success) and the final check didn't confirm it.
    CheckRejected { why: String },
    /// Two assertions already hold on this screen.
    EnoughAsserts,
    /// The model's assertion failed on the device.
    AssertFailed { error: String },
    /// The model's action failed on the device.
    ActionFailed { error: String },
    /// The action worked but changed nothing visible.
    #[serde(rename_all = "camelCase")]
    NoEffect { action: String, why: NoEffect },
    /// `count` actions in a row left the screen as it was (`missed`: some
    /// of them also missed their expectation).
    Streak { count: usize, missed: bool },
    /// The model's expectation for its last action was not met.
    #[serde(rename_all = "camelCase")]
    ExpectedNotMet {
        expected: String,
        action: String,
        detail: Option<String>,
    },
}

impl TurnNote {
    /// The note as the model reads it.
    pub fn render(&self) -> String {
        match self {
            Self::Operator { instruction } => format!("Operator instruction: {instruction}"),
            Self::LoopBack { turns_ago, at_turn } => format!(
                "You are back on a screen you saw {turns_ago} turns ago (at turn {at_turn}), with the same elements: the steps since then led back here without changing it. If that was not the plan, don't repeat them; try another way."
            ),
            Self::SeenBefore { after_turn } => format!(
                "You have seen this screen before (after turn {after_turn}); don't repeat what didn't work."
            ),
            Self::BackLedRound => "Last time you pressed back on this screen, the steps after it led round to a screen you had already seen, with nothing changed and nothing learned. Back is refused here once: use what this screen offers (a button that confirms, applies, saves or continues).".into(),
            Self::Rejected { why } => format!("Your last answer was rejected: {why}. Answer again."),
            Self::CheckRejected { why } => format!(
                "You answered done/success, but an independent check of the goal on this screen did not confirm it ({why}). Keep going, or give up."
            ),
            Self::EnoughAsserts => "Enough assertions on this screen; answer done, or move on.".into(),
            Self::AssertFailed { error } => format!("Your assertion failed: {error}"),
            Self::ActionFailed { error } => {
                format!("Your last action failed on the device: {error}")
            }
            Self::NoEffect { action, why } => match why {
                NoEffect::Moved => format!(
                    "Your last action ({action}) had no visible effect: the content did not move, so it may be at its end. Look elsewhere, or scroll the other way."
                ),
                NoEffect::WebTap => format!(
                    "Your last action ({action}) changed nothing visible. In a web page a tap can set state the page doesn't show (a colour, a tool, a mode): if it was the right element, don't tap it again; go on to the next step."
                ),
                NoEffect::Unchanged => format!(
                    "Your last action ({action}) had no visible effect: the screen is unchanged. Check you picked the right element, or try another way."
                ),
            },
            Self::Streak { count, missed } => format!(
                "Your last {count} actions did not change the screen{}. Try something different, or give up.",
                if *missed {
                    " or missed what they were expected to do"
                } else {
                    ""
                }
            ),
            Self::ExpectedNotMet {
                expected,
                action,
                detail,
            } => format!(
                "You expected {expected} after {action}, but {}: the action did not do what you meant. Read this screen and re-plan from it.",
                detail.as_deref().unwrap_or("it didn't happen")
            ),
        }
    }

    /// Whether the run also reports this note as a `note` event of its own
    /// when it is decided (the loop and effect notes, read live in the
    /// CLI); every note is in the turn's `notes` event either way.
    pub(crate) fn announced(&self) -> bool {
        matches!(
            self,
            Self::LoopBack { .. }
                | Self::NoEffect { .. }
                | Self::Streak { .. }
                | Self::ExpectedNotMet { .. }
        )
    }
}

/// The notes block of a goal turn: one `- ` line per note, redacted.
pub fn render_all(notes: &[TurnNote]) -> String {
    velocity_common::redact::redact(
        &notes
            .iter()
            .map(|n| format!("- {}", n.render()))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_note_renders_to_one_readable_line_and_serializes_with_its_kind() {
        let all = [
            TurnNote::Operator {
                instruction: "use search".into(),
            },
            TurnNote::LoopBack {
                turns_ago: 2,
                at_turn: 3,
            },
            TurnNote::SeenBefore { after_turn: 1 },
            TurnNote::BackLedRound,
            TurnNote::Rejected { why: "x".into() },
            TurnNote::CheckRejected { why: "y".into() },
            TurnNote::EnoughAsserts,
            TurnNote::AssertFailed { error: "e".into() },
            TurnNote::ActionFailed { error: "e".into() },
            TurnNote::NoEffect {
                action: "tap".into(),
                why: NoEffect::WebTap,
            },
            TurnNote::Streak {
                count: 2,
                missed: true,
            },
            TurnNote::ExpectedNotMet {
                expected: "\"Saved\" to appear".into(),
                action: "tap".into(),
                detail: None,
            },
        ];
        for n in &all {
            let line = n.render();
            assert!(!line.is_empty() && !line.contains('\n'), "{line}");
            let json = serde_json::to_value(n).unwrap();
            assert!(json["kind"].is_string(), "{json}");
        }
        assert_eq!(render_all(&all[..2]).lines().count(), 2);
    }
}
