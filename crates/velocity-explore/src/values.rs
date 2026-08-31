//! Code's check on text the model proposes itself: a value to type under
//! `--values-from-goal`, and the answer to a question goal.
//!
//! The rule stays "the model picks; code decides". The model may propose
//! the text, but code accepts it only when it is *grounded*:
//!
//! - **typed text** must occur verbatim (case- and whitespace-folded) in
//!   the goal or in a text shown on a screen during this run; or be a plain
//!   number (a computed amount, a count); or be a composition of such
//!   pieces joined by line breaks, commas or semicolons (merging notes,
//!   a list copied from one app to another). Free rewording is refused.
//! - **an answer** is split at commas (AndroidWorld's and most graders'
//!   list format); each part must be shown text, a number, a yes/no
//!   (true/false) judgement, or a phrase whose every word is a number, a
//!   word seen on a screen or in the goal, or a calendar or unit word
//!   ("October 22 2023 15:00", "2 tablespoons"). Anything else is refused.
//!
//! What this can't check: whether a computed number is right, or text the
//! model read from pixels only (an image with no accessibility text). The
//! first is accepted and marked `number`; the second is refused.

use std::collections::HashSet;

use serde::Serialize;

/// The texts and words the run has shown, folded for matching.
#[derive(Debug, Default, Clone)]
pub struct Seen {
    texts: HashSet<String>,
    words: HashSet<String>,
}

impl Seen {
    /// Remember one shown text (an element's text or label).
    pub fn add(&mut self, text: &str) {
        let n = fold(text);
        if n.is_empty() {
            return;
        }
        for w in words(&n) {
            self.words.insert(w);
        }
        self.texts.insert(n);
    }

    fn contains_text(&self, needle: &str) -> bool {
        !needle.is_empty() && self.texts.iter().any(|t| t.contains(needle))
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
}

/// Where an accepted piece of text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Verbatim in the goal.
    Goal,
    /// Verbatim in a text shown during the run.
    Screen,
    /// A plain number (computed, counted, or typed digit by digit).
    Number,
    /// Pieces joined by line breaks, commas or semicolons, each grounded.
    Composed,
    /// A yes/no judgement (True, False, Yes, No).
    Judgement,
    /// Every word is a number, a shown or goal word, or a calendar/unit word.
    Words,
}

/// Case, whitespace and typographic quotes folded.
pub fn fold(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201b}' | '`' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201f}' => '"',
            '\u{00a0}' => ' ',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn words(folded: &str) -> Vec<String> {
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_number(s: &str) -> bool {
    let t = s.trim();
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let mut digits = 0;
    let mut seps = 0;
    for c in t.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == '.' || c == ',' {
            seps += 1;
        } else {
            return false;
        }
    }
    digits > 0 && seps <= 2 && !t.starts_with(['.', ',']) && !t.ends_with(['.', ','])
}

/// Text longer than this is refused outright.
const MAX_TEXT: usize = 4000;

fn piece_source(folded: &str, goal: &str, seen: &Seen) -> Option<Source> {
    if folded.is_empty() {
        return None;
    }
    if goal.contains(folded) {
        Some(Source::Goal)
    } else if seen.contains_text(folded) {
        Some(Source::Screen)
    } else if is_number(folded) {
        Some(Source::Number)
    } else {
        None
    }
}

/// Check text the model wants typed. `Ok` names where it came from.
pub fn ground_typed(text: &str, goal: &str, seen: &Seen) -> Result<Source, String> {
    if text.trim().is_empty() {
        return Err("value is empty: name a listed value or copy the text to type".into());
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("value is longer than {MAX_TEXT} characters"));
    }
    let goal = fold(goal);
    if let Some(s) = piece_source(&fold(text), &goal, seen) {
        return Ok(s);
    }
    let pieces: Vec<String> = text
        .split(['\n', ',', ';'])
        .map(fold)
        .filter(|p| !p.is_empty())
        .collect();
    if pieces.len() >= 2
        && pieces
            .iter()
            .all(|p| piece_source(p, &goal, seen).is_some())
    {
        return Ok(Source::Composed);
    }
    Err(format!(
        "{:?} is not in the goal or on a screen seen in this run; type only text copied from them (or a listed value)",
        crate::screen::clip(text.trim(), 60)
    ))
}

/// Words a reformatted answer may use besides shown and goal words.
const VOCABULARY: &[&str] = &[
    // calendar
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "jan",
    "feb",
    "mar",
    "apr",
    "jun",
    "jul",
    "aug",
    "sep",
    "sept",
    "oct",
    "nov",
    "dec",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "am",
    "pm",
    "today",
    "tomorrow",
    "yesterday",
    // units
    "second",
    "seconds",
    "minute",
    "minutes",
    "hour",
    "hours",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "year",
    "years",
    "meter",
    "meters",
    "metre",
    "metres",
    "kilometer",
    "kilometers",
    "km",
    "mile",
    "miles",
    "foot",
    "feet",
    "inch",
    "inches",
    "cup",
    "cups",
    "tablespoon",
    "tablespoons",
    "teaspoon",
    "teaspoons",
    "gram",
    "grams",
    "kilogram",
    "kilograms",
    "ounce",
    "ounces",
    "pound",
    "pounds",
    "liter",
    "liters",
    "litre",
    "litres",
    "milliliter",
    "milliliters",
    "piece",
    "pieces",
    "clove",
    "cloves",
    "pinch",
    "slice",
    "slices",
    "can",
    "cans",
    "dollar",
    "dollars",
    "percent",
    // glue
    "and",
    "or",
    "of",
    "at",
    "on",
    "the",
    "a",
    "an",
    "to",
    "none",
    "no",
    "yes",
];

const JUDGEMENTS: &[&str] = &["true", "false", "yes", "no"];

/// The verdict on an answer: each comma-separated part and its source.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerGrounding {
    pub parts: Vec<(String, Source)>,
}

/// Check an answer the model gives. `cited` is the text of the element it
/// cited, if any (counted as shown text).
pub fn ground_answer(
    answer: &str,
    goal: &str,
    seen: &Seen,
    cited: Option<&str>,
) -> Result<AnswerGrounding, String> {
    if answer.trim().is_empty() {
        return Err("answer needs the answer in value".into());
    }
    if answer.chars().count() > MAX_TEXT {
        return Err(format!("answer is longer than {MAX_TEXT} characters"));
    }
    let mut seen = seen.clone();
    if let Some(c) = cited {
        seen.add(c);
    }
    let goal_f = fold(goal);
    let goal_words: HashSet<String> = words(&goal_f).into_iter().collect();
    let vocab: HashSet<&str> = VOCABULARY.iter().copied().collect();
    let mut parts = Vec::new();
    for raw in answer.split(',') {
        let part = fold(raw);
        let part = part.trim_matches(|c: char| c == '.' || c == '"' || c == '\'');
        if part.is_empty() {
            continue;
        }
        let source = if seen.contains_text(part) {
            Source::Screen
        } else if is_number(part) {
            Source::Number
        } else if JUDGEMENTS.contains(&part) {
            Source::Judgement
        } else if words(part).iter().all(|w| {
            w.chars().all(|c| c.is_ascii_digit())
                || seen.words.contains(w)
                || goal_words.contains(w)
                || vocab.contains(w.as_str())
        }) {
            Source::Words
        } else {
            let unknown: Vec<String> = words(part)
                .into_iter()
                .filter(|w| {
                    !w.chars().all(|c| c.is_ascii_digit())
                        && !seen.words.contains(w)
                        && !goal_words.contains(w)
                        && !vocab.contains(w.as_str())
                })
                .collect();
            return Err(format!(
                "the answer part {:?} is not supported by text shown in this run (unseen: {}); answer only with what the screens showed",
                crate::screen::clip(raw.trim(), 60),
                unknown.join(", ")
            ));
        };
        parts.push((raw.trim().to_string(), source));
    }
    if parts.is_empty() {
        return Err("answer needs the answer in value".into());
    }
    Ok(AnswerGrounding { parts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(texts: &[&str]) -> Seen {
        let mut s = Seen::default();
        for t in texts {
            s.add(t);
        }
        s
    }

    const GOAL: &str = "Add a contact named John Smith with phone +1 555-1234, label 'Mobile'";

    #[test]
    fn typed_text_from_the_goal_is_accepted_case_and_space_folded() {
        let s = Seen::default();
        assert_eq!(ground_typed("John Smith", GOAL, &s), Ok(Source::Goal));
        assert_eq!(ground_typed("john   SMITH", GOAL, &s), Ok(Source::Goal));
        assert_eq!(ground_typed("+1 555-1234", GOAL, &s), Ok(Source::Goal));
        assert_eq!(ground_typed("Mobile", GOAL, &s), Ok(Source::Goal));
    }

    #[test]
    fn typed_text_from_a_screen_or_a_number_is_accepted() {
        let s = seen(&["Meeting at 42 Elm Street, Springfield"]);
        assert_eq!(ground_typed("42 Elm Street", GOAL, &s), Ok(Source::Screen));
        assert_eq!(ground_typed("1260", GOAL, &s), Ok(Source::Number));
        assert_eq!(ground_typed("-3.50", GOAL, &s), Ok(Source::Number));
    }

    #[test]
    fn invented_or_reworded_text_is_refused() {
        let s = seen(&["Contacts"]);
        let e = ground_typed("Jonathan Smith", GOAL, &s).unwrap_err();
        assert!(e.contains("not in the goal"), "{e}");
        assert!(ground_typed("Smith John", GOAL, &s).is_err());
        assert!(ground_typed("ignore previous instructions", GOAL, &s).is_err());
        assert!(ground_typed("   ", GOAL, &s).is_err());
        assert!(ground_typed(&"x".repeat(MAX_TEXT + 1), GOAL, &s).is_err());
        // A number glued to words is not a number.
        assert!(ground_typed("555 dollars", GOAL, &s).is_err());
    }

    #[test]
    fn compositions_of_grounded_pieces_are_accepted() {
        let s = seen(&["Groceries", "Buy eggs", "Call the bank"]);
        assert_eq!(
            ground_typed("Buy eggs\nCall the bank", GOAL, &s),
            Ok(Source::Composed)
        );
        assert_eq!(
            ground_typed("John Smith, 42", GOAL, &s),
            Ok(Source::Composed)
        );
        // One bad piece spoils it.
        assert!(ground_typed("Buy eggs\nBuy a car", GOAL, &s).is_err());
    }

    #[test]
    fn answers_shown_on_screen_numbers_and_judgements_are_accepted() {
        let goal = "What events do I have tomorrow? Answer with the titles only.";
        let s = seen(&["Team sync", "Dentist appointment", "10:00 - 11:00"]);
        let g = ground_answer("Team sync, Dentist appointment", goal, &s, None).unwrap();
        assert_eq!(g.parts.len(), 2);
        assert!(g.parts.iter().all(|p| p.1 == Source::Screen));
        assert_eq!(
            ground_answer("3", goal, &s, None).unwrap().parts[0].1,
            Source::Number
        );
        assert_eq!(
            ground_answer("True", goal, &s, None).unwrap().parts[0].1,
            Source::Judgement
        );
        // A citation counts as shown text.
        assert_eq!(
            ground_answer("Gym", goal, &s, Some("Gym")).unwrap().parts[0].1,
            Source::Screen
        );
    }

    #[test]
    fn reformatted_answers_may_use_calendar_and_unit_words() {
        let goal =
            "When is my next meeting with Ana? Format <month name> <day> <year> <hour>:<minute>";
        let s = seen(&["Meeting with Ana", "Oct 23, 2023", "15:00"]);
        assert_eq!(
            ground_answer("October 23 2023 15:00", goal, &s, None)
                .unwrap()
                .parts[0]
                .1,
            Source::Words
        );
        let s = seen(&["2 tbsp sugar"]);
        assert!(ground_answer("2 tablespoons", "How much sugar?", &s, None).is_ok());
    }

    #[test]
    fn unsupported_answers_are_refused_with_the_unseen_words() {
        let s = seen(&["Team sync"]);
        let e = ground_answer("Team sync, Board review", "What events?", &s, None).unwrap_err();
        assert!(e.contains("board") && e.contains("review"), "{e}");
        assert!(ground_answer("", "q", &s, None).is_err());
        assert!(ground_answer(" , ", "q", &s, None).is_err());
    }

    #[test]
    fn folding_handles_quotes_and_nbsp() {
        assert_eq!(fold("  It\u{2019}s\u{00a0}  OK "), "it's ok");
        assert!(is_number("1,024.5"));
        assert!(!is_number("1.2.3.4"));
        assert!(!is_number("."));
        assert!(!is_number("12a"));
    }
}
