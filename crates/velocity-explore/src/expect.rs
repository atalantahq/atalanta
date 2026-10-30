//! Expected post-conditions (Q-Reflector, arXiv 2608.05587): before acting,
//! the model says what should hold afterwards; code judges the transition
//! against the next screen.
//!
//! The model only *describes* the expectation (a text, or an element number
//! of the screen it acted on). Code checks it against the real elements of
//! the next screen, and a met expectation becomes an assertion built from
//! the element that matched, with the same selector builder as every other
//! assertion. The model's text is never a selector: a text that matches no
//! element is "not met", and nothing is written for it.

use serde::Serialize;
use velocity_common::{Element, Selector};
use velocity_nl::Expectation;

use crate::screen::{assertable, clip, content_selectors, element_name, Observation};

/// How an expectation turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExpectOutcome {
    Met,
    NotMet,
    /// No element number in the list, not a switch, an unreadable form, a
    /// failed action: nothing to judge.
    NotCheckable,
}

/// An assertion a met expectation adds after its step.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostAssertion {
    /// `assertVisible` (true) or `assertNotVisible` (false).
    pub visible: bool,
    pub selector: Selector,
}

/// The judgement on one turn's expectation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectCheck {
    /// What was expected, in words (`"Saved" to appear`).
    pub expected: String,
    pub outcome: ExpectOutcome,
    /// Why it wasn't met or couldn't be checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The assertion it adds to the test (met, and worth asserting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assertion: Option<PostAssertion>,
}

/// Expectations over a run, for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectationCounts {
    pub met: usize,
    pub not_met: usize,
    pub not_checkable: usize,
}

impl ExpectationCounts {
    pub fn add(&mut self, outcome: ExpectOutcome) {
        match outcome {
            ExpectOutcome::Met => self.met += 1,
            ExpectOutcome::NotMet => self.not_met += 1,
            ExpectOutcome::NotCheckable => self.not_checkable += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.met + self.not_met + self.not_checkable
    }
}

impl std::fmt::Display for ExpectationCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "expectations: {} met, {} not met, {} not checkable",
            self.met, self.not_met, self.not_checkable
        )
    }
}

/// The expectation in words, with the element's name from the screen it
/// was given on (`before`).
pub fn describe(exp: &Expectation, before: &Observation) -> String {
    let name = |n: usize| {
        before
            .index
            .get(n)
            .map(|el| format!("{:?}", element_name(el, before.platform())))
            .unwrap_or_else(|| format!("element {n}"))
    };
    match exp {
        Expectation::Appears { text } => format!("{:?} to appear", clip(text, 80)),
        Expectation::Gone { element } => format!("{} to be gone", name(*element)),
        Expectation::Checked { element, value } => format!(
            "{} to be {}",
            name(*element),
            if *value { "checked" } else { "unchecked" }
        ),
        Expectation::Unreadable => "something code can't read".into(),
    }
}

/// Judge `exp`, given on `before`, against `after`. `user_values` (typed
/// data, secrets) are never asserted on.
pub fn check(
    exp: &Expectation,
    before: &Observation,
    after: &Observation,
    user_values: &[String],
) -> ExpectCheck {
    let expected = describe(exp, before);
    let verdict = match exp {
        Expectation::Appears { text } => appears(text, before, after, user_values),
        Expectation::Gone { element } => gone(*element, before, after, user_values),
        Expectation::Checked { element, value } => {
            checked(*element, *value, before, after, user_values)
        }
        Expectation::Unreadable => Verdict::NotCheckable(
            "its form isn't one of appears, gone, checked or unchecked".into(),
        ),
    };
    let (outcome, detail, assertion) = match verdict {
        Verdict::Met(a) => (ExpectOutcome::Met, None, a),
        Verdict::NotMet(why) => (ExpectOutcome::NotMet, Some(why), None),
        Verdict::NotCheckable(why) => (ExpectOutcome::NotCheckable, Some(why), None),
    };
    ExpectCheck {
        expected,
        outcome,
        detail,
        assertion,
    }
}

/// A check that couldn't run at all (the action failed on the device).
pub fn not_checkable(exp: &Expectation, before: &Observation, why: &str) -> ExpectCheck {
    ExpectCheck {
        expected: describe(exp, before),
        outcome: ExpectOutcome::NotCheckable,
        detail: Some(why.into()),
        assertion: None,
    }
}

enum Verdict {
    Met(Option<PostAssertion>),
    NotMet(String),
    NotCheckable(String),
}

/// Lowercase, whitespace collapsed, typographic quotes folded: how a model
/// may retype a text it read.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '‘' | '’' | '`' => '\'',
            '“' | '”' => '"',
            c => c,
        })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `needle` is in `hay` as whole words ("saved" in "contact saved",
/// not in "unsaved").
fn has_words(hay: &str, needle: &str) -> bool {
    if needle.chars().count() < 3 {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    hay.match_indices(needle).any(|(i, _)| {
        !word(hay[..i].chars().next_back()) && !word(hay[i + needle.len()..].chars().next())
    })
}

/// A selector for `el` that also passes on replay as a check of this step:
/// content first (as the screen assertions), then the robust selector;
/// assertable, and matching on `now` but not on `then` (else it would pass
/// without the step doing anything).
fn new_selector(
    el: &Element,
    n: usize,
    now: &Observation,
    then: &Observation,
    user_values: &[String],
) -> Option<Selector> {
    let mut candidates = content_selectors(el, now);
    candidates.extend(now.selector_for(n));
    candidates
        .into_iter()
        .find(|s| assertable(s, user_values) && then.matches(s).is_empty())
}

fn appears(text: &str, before: &Observation, after: &Observation, uv: &[String]) -> Verdict {
    let want = fold(text);
    if want.is_empty() {
        return Verdict::NotCheckable("no text was named".into());
    }
    if uv.iter().any(|v| !v.is_empty() && fold(v) == want) {
        return Verdict::NotCheckable("it names typed data".into());
    }
    let old = before.keys();
    let platform = after.platform();
    // Exact before whole-word, new to this screen before old, top first.
    let mut best: Option<((u8, bool, i32), usize)> = None;
    for n in 0..after.index.len() {
        let Some(el) = after.index.get(n) else {
            continue;
        };
        for t in [&el.label, &el.text].into_iter().flatten() {
            let have = fold(t);
            let rank = if have == want {
                0
            } else if has_words(&have, &want) {
                1
            } else {
                continue;
            };
            let key = (
                rank,
                old.contains(&crate::screen::element_key(el, platform)),
                el.bounds.y,
            );
            if best.is_none_or(|(k, _)| key < k) {
                best = Some((key, n));
            }
        }
    }
    let Some((_, n)) = best else {
        return Verdict::NotMet("no element on the screen now shows it".into());
    };
    let el = after.index.get(n).expect("listed");
    Verdict::Met(
        new_selector(el, n, after, before, uv).map(|selector| PostAssertion {
            visible: true,
            selector,
        }),
    )
}

/// The element `n` of `before` and its selectors, content first.
fn selectors_of(n: usize, before: &Observation) -> Option<(&Element, Vec<Selector>)> {
    let el = before.index.get(n)?;
    let mut candidates = content_selectors(el, before);
    candidates.extend(before.selector_for(n));
    Some((el, candidates))
}

fn gone(n: usize, before: &Observation, after: &Observation, uv: &[String]) -> Verdict {
    let Some((_, candidates)) = selectors_of(n, before) else {
        return Verdict::NotCheckable(format!("element {n} was not in the list"));
    };
    // The most specific selector decides: its content, else its id.
    let Some(first) = candidates.first() else {
        return Verdict::NotCheckable("no selector".into());
    };
    if !after.matches(first).is_empty() {
        return Verdict::NotMet("it is still on the screen".into());
    }
    let assertion = candidates
        .into_iter()
        .find(|s| assertable(s, uv) && after.matches(s).is_empty())
        .map(|selector| PostAssertion {
            visible: false,
            selector,
        });
    Verdict::Met(assertion)
}

fn checked(
    n: usize,
    want: bool,
    before: &Observation,
    after: &Observation,
    uv: &[String],
) -> Verdict {
    let Some((el, candidates)) = selectors_of(n, before) else {
        return Verdict::NotCheckable(format!("element {n} was not in the list"));
    };
    if el.checked.is_none() {
        return Verdict::NotCheckable("it is not a switch or checkbox".into());
    }
    // The same element on the next screen: by the first selector that
    // still finds something there, among its matches the one with a state
    // (an iOS cell and its label share a name; only one is checkable).
    let Some((base, found)) = candidates
        .iter()
        .map(|s| (s, after.matches(s)))
        .find(|(_, m)| !m.is_empty())
    else {
        return Verdict::NotMet("it is no longer on the screen".into());
    };
    let states: Vec<bool> = found.iter().filter_map(|m| m.checked).collect();
    let [state] = states[..] else {
        return Verdict::NotCheckable("it can't be told apart on the screen now".into());
    };
    if state != want {
        return Verdict::NotMet(format!(
            "it is {}",
            if state { "checked" } else { "unchecked" }
        ));
    }
    let assertion = assertable(base, uv)
        .then(|| with_state(base, want))
        .filter(|s| !after.matches(s).is_empty() && before.matches(s).is_empty())
        .map(|selector| PostAssertion {
            visible: true,
            selector,
        });
    Verdict::Met(assertion)
}

/// `base` narrowed to a checked state: `{ text: "Wi-Fi", checked: true }`.
fn with_state(base: &Selector, checked: bool) -> Selector {
    let mut parts = match base {
        Selector::Compound(parts) => parts.clone(),
        other => vec![other.clone()],
    };
    parts.push(Selector::Checked(checked));
    Selector::Compound(parts)
}

/// Whether a selector narrows by state (a checked assertion): it says more
/// than a plain one with the same text, so the writer keeps both.
pub fn has_state(sel: &Selector) -> bool {
    match sel {
        Selector::Compound(parts) => parts.iter().any(has_state),
        Selector::Checked(_)
        | Selector::Selected(_)
        | Selector::Focused(_)
        | Selector::Enabled(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_match_folded_and_by_whole_words() {
        assert_eq!(fold("  Contact\u{2019}s   SAVED "), "contact's saved");
        assert!(has_words("contact saved", "saved"));
        assert!(!has_words("unsaved changes", "saved"));
        assert!(!has_words("ok", "ok"), "too short to look inside");
        let c = ExpectationCounts {
            met: 2,
            not_met: 1,
            not_checkable: 0,
        };
        assert_eq!(c.total(), 3);
        assert_eq!(
            c.to_string(),
            "expectations: 2 met, 1 not met, 0 not checkable"
        );
    }
}
