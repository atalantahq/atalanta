//! Code's check on text the model proposes itself: a value to type under
//! `--values-from-goal` / `--values-from-screen`, and the answer to a
//! question goal.
//!
//! The rule stays "the model picks; code decides". The model may propose
//! the text, but code accepts it only when it is *grounded*:
//!
//! - **typed text** must occur verbatim (case- and whitespace-folded) in
//!   the goal (`--values-from-goal`) or in a text shown on a screen during
//!   this run (`--values-from-screen`, a separate opt-in: screen text is
//!   untrusted, the app or a web page wrote it); or be a plain
//!   number (a computed amount, a count); or be a composition of such
//!   pieces joined by line breaks, commas or semicolons (merging notes,
//!   a list copied from one app to another). Free rewording is refused.
//! - **an answer** is split at commas (AndroidWorld's and most graders'
//!   list format), and each part gets a tier:
//!   - *verified*: the part is shown text (whole words of a text a screen
//!     showed, or the whole text; a number shown on a screen included);
//!   - *unverified*: a number no screen showed, accepted only when the goal
//!     asks for a count or a computed amount; a yes/no (true/false) judgement,
//!     accepted only when the goal is a yes/no question; or a phrase whose
//!     every word is a number, a word seen on a screen or in the goal, or a
//!     calendar or unit word ("October 22 2023 15:00", "2 tablespoons"),
//!     with at least one word read from a screen.
//!
//!   Anything else is refused. An unverified part is not a pass: the final
//!   check is told which parts code verified and must judge the others
//!   from the screens itself.
//!
//! What this can't check: whether a computed number or a judgement is right
//! (left to the final check), or text the model read from pixels only. The
//! second is accepted for typing only under `--values-from-screenshots`, and
//! only once a screenshot of a screen with pixel-only content (an image, a
//! canvas, a video, a screen with almost no accessibility tree) went to the
//! model.

use std::collections::HashSet;

use serde::Serialize;

/// The texts and words the run has shown, folded for matching.
#[derive(Debug, Default, Clone)]
pub struct Seen {
    texts: HashSet<String>,
    words: HashSet<String>,
    /// Screenshots sent to the model of a screen whose content the tree
    /// can't read (an image, a canvas, a video, a near-empty tree): the only
    /// screens text may be read from under `--values-from-screenshots`.
    image_shots: usize,
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

    /// A screenshot of a screen with pixel-only content (an image, a
    /// canvas, a video, a near-empty tree) went to the model.
    pub fn add_image_shot(&mut self) {
        self.image_shots += 1;
    }

    /// Whether text may have been read from a screenshot in this run.
    pub fn image_shot_sent(&self) -> bool {
        self.image_shots > 0
    }

    /// Whether `needle` (folded) is a shown text, or a run of whole words in
    /// one: "Ann" is not in "Planning", and "on" is not in "Turn on Wi-Fi"
    /// unless it was shown on its own (a part shorter than 3 characters
    /// must be a whole shown text).
    fn contains_text(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        if self.texts.contains(needle) {
            return true;
        }
        let want = words(needle);
        if want.is_empty() || (short(needle) && !is_number(needle)) {
            return false;
        }
        self.texts.iter().any(|t| {
            let have = words(t);
            have.windows(want.len()).any(|w| w == want.as_slice())
        })
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
}

/// Whether `text` shows `part` as a run of its whole words (folded):
/// "Lunch" in "Lunch at noon", not "Ann" in "Planning".
pub fn shows_words(text: &str, part: &str) -> bool {
    let want = words(&fold(part));
    if want.is_empty() {
        return false;
    }
    let have = words(&fold(text));
    have.windows(want.len()).any(|w| w == want.as_slice())
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
    /// A yes/no judgement (True, False, Yes, No), for a yes/no question.
    Judgement,
    /// Every word is a number, a shown or goal word, or a calendar/unit word.
    Words,
    /// Read by the model from a screenshot of a screen with pixel-only
    /// content (`--values-from-screenshots`): code can't check pixels, so
    /// the model's reading is trusted.
    Screenshot,
    /// Written by the model (`--values-from-model`).
    Model,
}

impl Source {
    /// Whether code itself saw the text (in the goal or on a screen), as
    /// opposed to accepting a number, a judgement, reworded words or a
    /// reading it can't check.
    pub fn verified(self) -> bool {
        matches!(self, Source::Goal | Source::Screen)
    }

    /// The lower-case name used in events and reports.
    pub fn name(self) -> &'static str {
        match self {
            Source::Goal => "goal",
            Source::Screen => "screen",
            Source::Number => "number",
            Source::Composed => "composed",
            Source::Judgement => "judgement",
            Source::Words => "words",
            Source::Screenshot => "screenshot",
            Source::Model => "model",
        }
    }
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

/// Fewer than 3 letters or digits: too short to ground by words alone.
fn short(s: &str) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() < 3
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
/// A reading from a screenshot is a short piece (a total, a name, a line
/// of a receipt), not a composition.
const MAX_SCREENSHOT_TEXT: usize = 200;

/// Where typed text may be copied from: the flags the run was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CopyFrom {
    /// `--values-from-goal`: the goal text (the user wrote it).
    pub goal: bool,
    /// `--values-from-screen`: text shown on a screen in this run (the app
    /// or a page wrote it, so it is untrusted: a separate opt-in).
    pub screen: bool,
    /// `--values-from-screenshots`: text the model read from a screenshot
    /// sent in this run (an image, a receipt, a video frame). Code can't
    /// check it against the pixels, so it is its own opt-in, and only
    /// after a screenshot of a screen with pixel-only content was sent.
    pub screenshots: bool,
    /// `--values-from-model`: text the model writes itself (a reply, a
    /// message body), for runs where composing it is the task. Nothing
    /// grounds it, so it is the last and widest opt-in.
    pub model: bool,
}

impl CopyFrom {
    /// Whether the model may propose typed text at all.
    pub fn any(self) -> bool {
        self.goal || self.screen || self.screenshots || self.model
    }
}

fn piece_source(folded: &str, goal: &str, seen: &Seen, from: CopyFrom) -> Option<Source> {
    if folded.is_empty() {
        return None;
    }
    if from.goal && goal.contains(folded) {
        Some(Source::Goal)
    } else if from.screen && seen.contains_text(folded) {
        Some(Source::Screen)
    } else if (from.goal || from.screen) && is_number(folded) {
        // A computed number (a total, a count) only where copying is on:
        // under --values-from-screenshots alone a hostile screen could get
        // any digits typed (a PIN, an account number; review 05 #5).
        Some(Source::Number)
    } else if from.screenshots
        && seen.image_shot_sent()
        && folded.chars().count() <= MAX_SCREENSHOT_TEXT
    {
        Some(Source::Screenshot)
    } else if from.model {
        Some(Source::Model)
    } else {
        None
    }
}

/// Check text the model wants typed. `Ok` names where it came from.
pub fn ground_typed(text: &str, goal: &str, seen: &Seen, from: CopyFrom) -> Result<Source, String> {
    if text.trim().is_empty() {
        return Err("value is empty: name a listed value or copy the text to type".into());
    }
    if text.chars().count() > MAX_TEXT {
        return Err(format!("value is longer than {MAX_TEXT} characters"));
    }
    let goal = fold(goal);
    if let Some(s) = piece_source(&fold(text), &goal, seen, from) {
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
            .all(|p| piece_source(p, &goal, seen, from).is_some())
    {
        return Ok(Source::Composed);
    }
    let place = match (from.goal, from.screen) {
        (true, true) => "in the goal or on a screen seen in this run",
        (true, false) => "in the goal (text on screens may not be copied in this run)",
        (false, _) => {
            "on a screen seen in this run (text from the goal may not be copied in this run)"
        }
    };
    Err(format!(
        "{:?} is not {place}; type only text copied from there (or a listed value)",
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

/// Words that make a goal ask for a count or a computed amount, the only
/// goals a number no screen showed may answer.
const QUANTITY_CUES: &[&str] = &[
    "how many",
    "how much",
    "number of",
    "count",
    "total",
    "sum",
    "add up",
    "average",
    "difference",
    "calculate",
    "compute",
    "percentage",
    "how long",
    "how far",
    "how old",
];

/// Whether the goal asks for a count or a computed amount.
fn asks_for_quantity(goal_folded: &str) -> bool {
    let padded = format!(" {} ", words(goal_folded).join(" "));
    QUANTITY_CUES
        .iter()
        .any(|c| padded.contains(&format!(" {c} ")))
}

/// Whether the goal is a yes/no question: it says so ("yes or no",
/// "true or false"), or one of its questions starts with an auxiliary verb
/// ("Is Bluetooth on?", "Do I have a meeting today?").
fn asks_yes_no(goal_folded: &str) -> bool {
    const AUX: &[&str] = &[
        "is", "are", "was", "were", "do", "does", "did", "has", "have", "had", "can", "could",
        "will", "would", "should",
    ];
    let w = words(goal_folded).join(" ");
    if ["yes or no", "true or false", "yes no", "true false"]
        .iter()
        .any(|c| format!(" {w} ").contains(&format!(" {c} ")))
    {
        return true;
    }
    goal_folded
        .split(['.', '!', ';', '\n'])
        .filter(|q| q.contains('?'))
        .flat_map(|q| q.split('?'))
        .filter_map(|q| words(q).into_iter().next())
        .any(|first| AUX.contains(&first.as_str()))
}

/// The verdict on an answer: each comma-separated part and its source.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerGrounding {
    pub parts: Vec<(String, Source)>,
}

impl AnswerGrounding {
    /// The parts code saw on a screen.
    pub fn verified(&self) -> Vec<String> {
        self.parts
            .iter()
            .filter(|(_, s)| s.verified())
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// The parts code accepted without seeing them (a number, a judgement,
    /// reworded words), with their source: the final check judges these.
    pub fn unverified(&self) -> Vec<(String, Source)> {
        self.parts
            .iter()
            .filter(|(_, s)| !s.verified())
            .cloned()
            .collect()
    }
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
    let quantity = asks_for_quantity(&goal_f);
    let yes_no = asks_yes_no(&goal_f);
    let mut parts = Vec::new();
    for raw in answer.split(',') {
        let part = fold(raw);
        let part = part.trim_matches(|c: char| c == '.' || c == '"' || c == '\'');
        if part.is_empty() {
            continue;
        }
        let unseen_words = || -> Vec<String> {
            words(part)
                .into_iter()
                .filter(|w| {
                    !w.chars().all(|c| c.is_ascii_digit())
                        && !seen.words.contains(w)
                        && !goal_words.contains(w)
                        && !vocab.contains(w.as_str())
                })
                .collect()
        };
        let clipped = crate::screen::clip(raw.trim(), 60);
        let source = if seen.contains_text(part) {
            Source::Screen
        } else if is_number(part) {
            if !quantity {
                return Err(format!(
                    "the number {clipped:?} was not shown on a screen in this run, and the goal asks for no count or total; answer with what a screen showed"
                ));
            }
            Source::Number
        } else if JUDGEMENTS.contains(&part) {
            if !yes_no {
                return Err(format!(
                    "{clipped:?} is a yes/no answer, but the goal is not a yes/no question; answer with what a screen showed"
                ));
            }
            Source::Judgement
        } else if unseen_words().is_empty() {
            // Reworded: every word is a number, a calendar or unit word, or
            // was seen; at least one must come from a screen, or the part is
            // the goal's own words played back.
            if short(part) || !words(part).iter().any(|w| seen.words.contains(w)) {
                return Err(format!(
                    "the answer part {clipped:?} is too short, or uses no text shown in this run (only the goal's words); answer only with what the screens showed"
                ));
            }
            Source::Words
        } else {
            return Err(format!(
                "the answer part {clipped:?} is not supported by text shown in this run (unseen: {}); answer only with what the screens showed",
                unseen_words().join(", ")
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

    const BOTH: CopyFrom = CopyFrom {
        goal: true,
        screen: true,
        screenshots: false,
        model: false,
    };

    #[test]
    fn screenshot_text_needs_its_flag_and_a_sent_screenshot() {
        let mut s = seen(&["Gallery"]);
        let pixels = CopyFrom {
            goal: false,
            screen: true,
            screenshots: true,
            model: false,
        };
        // No screenshot of an image screen went to the model yet: refused.
        assert!(ground_typed("Dress", GOAL, &s, pixels).is_err());
        s.add_image_shot();
        assert_eq!(
            ground_typed("Dress", GOAL, &s, pixels),
            Ok(Source::Screenshot)
        );
        // Text the tree shows is still credited to the screen.
        assert_eq!(
            ground_typed("Gallery", GOAL, &s, pixels),
            Ok(Source::Screen)
        );
        // Without the flag, a sent screenshot changes nothing.
        let no = CopyFrom {
            screenshots: false,
            ..pixels
        };
        assert!(ground_typed("Dress", GOAL, &s, no).is_err());
        // A reading is a short piece, not a composition.
        assert!(ground_typed(&"word ".repeat(60), GOAL, &s, pixels).is_err());
    }

    // AI-092: screen text is its own opt-in; the goal flag alone refuses it.
    #[test]
    fn screen_text_needs_its_own_flag() {
        let s = seen(&["42 Elm Street", "Buy eggs"]);
        let goal_only = CopyFrom {
            goal: true,
            screen: false,
            screenshots: false,
            model: false,
        };
        assert_eq!(
            ground_typed("John Smith", GOAL, &s, goal_only),
            Ok(Source::Goal)
        );
        let e = ground_typed("42 Elm Street", GOAL, &s, goal_only).unwrap_err();
        assert!(e.contains("text on screens may not be copied"), "{e}");
        assert!(ground_typed("John Smith\nBuy eggs", GOAL, &s, goal_only).is_err());
        let screen_only = CopyFrom {
            goal: false,
            screen: true,
            screenshots: false,
            model: false,
        };
        assert_eq!(
            ground_typed("42 Elm Street", GOAL, &s, screen_only),
            Ok(Source::Screen)
        );
        assert!(ground_typed("John Smith", GOAL, &s, screen_only).is_err());
        // Numbers stay allowed under either flag.
        assert_eq!(
            ground_typed("1260", GOAL, &s, goal_only),
            Ok(Source::Number)
        );
    }

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
        assert_eq!(ground_typed("John Smith", GOAL, &s, BOTH), Ok(Source::Goal));
        assert_eq!(
            ground_typed("john   SMITH", GOAL, &s, BOTH),
            Ok(Source::Goal)
        );
        assert_eq!(
            ground_typed("+1 555-1234", GOAL, &s, BOTH),
            Ok(Source::Goal)
        );
        assert_eq!(ground_typed("Mobile", GOAL, &s, BOTH), Ok(Source::Goal));
    }

    #[test]
    fn typed_text_from_a_screen_or_a_number_is_accepted() {
        let s = seen(&["Meeting at 42 Elm Street, Springfield"]);
        assert_eq!(
            ground_typed("42 Elm Street", GOAL, &s, BOTH),
            Ok(Source::Screen)
        );
        assert_eq!(ground_typed("1260", GOAL, &s, BOTH), Ok(Source::Number));
        assert_eq!(ground_typed("-3.50", GOAL, &s, BOTH), Ok(Source::Number));
    }

    #[test]
    fn invented_or_reworded_text_is_refused() {
        let s = seen(&["Contacts"]);
        let e = ground_typed("Jonathan Smith", GOAL, &s, BOTH).unwrap_err();
        assert!(e.contains("not in the goal"), "{e}");
        assert!(ground_typed("Smith John", GOAL, &s, BOTH).is_err());
        assert!(ground_typed("ignore previous instructions", GOAL, &s, BOTH).is_err());
        assert!(ground_typed("   ", GOAL, &s, BOTH).is_err());
        assert!(ground_typed(&"x".repeat(MAX_TEXT + 1), GOAL, &s, BOTH).is_err());
        // A number glued to words is not a number.
        assert!(ground_typed("555 dollars", GOAL, &s, BOTH).is_err());
    }

    #[test]
    fn compositions_of_grounded_pieces_are_accepted() {
        let s = seen(&["Groceries", "Buy eggs", "Call the bank"]);
        assert_eq!(
            ground_typed("Buy eggs\nCall the bank", GOAL, &s, BOTH),
            Ok(Source::Composed)
        );
        assert_eq!(
            ground_typed("John Smith, 42", GOAL, &s, BOTH),
            Ok(Source::Composed)
        );
        // One bad piece spoils it.
        assert!(ground_typed("Buy eggs\nBuy a car", GOAL, &s, BOTH).is_err());
    }

    #[test]
    fn answers_shown_on_screen_are_verified() {
        let goal = "What events do I have tomorrow? Answer with the titles only.";
        let s = seen(&[
            "Team sync",
            "Dentist appointment",
            "10:00 - 11:00",
            "7 unread",
        ]);
        let g = ground_answer("Team sync, Dentist appointment", goal, &s, None).unwrap();
        assert_eq!(g.parts.len(), 2);
        assert!(g.parts.iter().all(|p| p.1 == Source::Screen));
        assert!(g.unverified().is_empty());
        // A number a screen showed is shown text, whatever the goal asks.
        assert_eq!(
            ground_answer("7", goal, &s, None).unwrap().parts[0].1,
            Source::Screen
        );
        // A citation counts as shown text.
        assert_eq!(
            ground_answer("Gym", goal, &s, Some("Gym")).unwrap().parts[0].1,
            Source::Screen
        );
    }

    #[test]
    fn an_unshown_number_needs_a_goal_that_asks_for_a_count_or_total() {
        let s = seen(&["Buy milk", "Call Ana"]);
        assert!(ground_answer("3", "What is due today?", &s, None).is_err());
        for goal in [
            "How many tasks are due today?",
            "What is the total of my expenses?",
        ] {
            let g = ground_answer("3", goal, &s, None).unwrap();
            assert_eq!(g.parts[0].1, Source::Number, "{goal}");
            assert_eq!(
                g.unverified().len(),
                1,
                "a computed number is left to the check"
            );
        }
    }

    #[test]
    fn a_yes_or_no_needs_a_yes_no_question() {
        let s = seen(&["Bluetooth", "Wi-Fi"]);
        assert!(ground_answer("yes", "Which network am I on?", &s, None).is_err());
        assert!(ground_answer("True", "What events do I have tomorrow?", &s, None).is_err());
        for goal in [
            "Is Bluetooth on?",
            "Check the settings. Do I have Wi-Fi enabled?",
            "Answer true or false: the alarm is set",
        ] {
            let g = ground_answer("Yes", goal, &s, None).unwrap();
            assert_eq!(g.parts[0].1, Source::Judgement, "{goal}");
            assert!(!g.parts[0].1.verified());
        }
    }

    #[test]
    fn shown_text_means_whole_words_not_a_substring() {
        let s = seen(&["Planning", "Turn on Wi-Fi"]);
        let goal = "Who is the organiser? Answer with the name";
        assert!(ground_answer("Ann", goal, &s, None).is_err());
        assert!(ground_answer("on", goal, &s, None).is_err());
        assert_eq!(
            ground_answer("Wi-Fi", goal, &s, None).unwrap().parts[0].1,
            Source::Screen
        );
        let typed = CopyFrom {
            screen: true,
            ..CopyFrom::default()
        };
        assert!(ground_typed("lann", GOAL, &s, typed).is_err());
        assert_eq!(ground_typed("turn on", GOAL, &s, typed), Ok(Source::Screen));
    }

    #[test]
    fn the_goals_own_words_are_no_answer() {
        // Both candidates are named in the goal; neither was on a screen.
        let goal = "Which contact lives in Springfield, Ana Lima or Ben Cole?";
        let s = seen(&["Contacts", "Search"]);
        assert!(ground_answer("Ana Lima", goal, &s, None).is_err());
        let s = seen(&["Ana Lima", "Springfield"]);
        assert_eq!(
            ground_answer("Ana Lima", goal, &s, None).unwrap().parts[0].1,
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
