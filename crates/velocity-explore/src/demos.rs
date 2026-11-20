//! Demonstrations from the project's own tests.
//!
//! LearnAct (Liu et al. 2025, arXiv 2504.13805) showed one demonstration
//! of a similar task taking Gemini-1.5-Pro from 19.3% to 51.7% success on
//! mobile tasks. A Velocity project already holds the best demonstrations
//! there are: tests that pass against this very app. Before a goal run, the
//! one or two tests most like the goal are shown to the model, compactly,
//! as "how similar goals were done in this app".
//!
//! Context only. The model still answers an op and an element number from
//! the current screen list, and code still builds the selector: nothing in
//! a demonstration becomes a selector or a value. Typed text is shown only
//! as its `${NAME}`, never as the text itself, so a demo can't hand the
//! model a value to copy.
//!
//! Retrieval is plain token overlap of the goal with each test's name,
//! description and step text. That is enough here: the candidates are a
//! few dozen tests of one app, written in the app's own words.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use velocity_common::{Action, NaturalKind, Platform, Selector, Step, TestSuite};
use velocity_core::executor::selector_fields;

use crate::screen::clip;

/// Tests shown at most.
pub const MAX_DEMOS: usize = 2;
/// Characters of demonstrations in a prompt at most, all tests together.
pub const DEMO_CHARS: usize = 1500;
/// A test must score this much to be shown: one goal word in its name or
/// description, or two in its steps. One shared step word ("Settings") is
/// not similarity.
const MIN_SCORE: usize = 2;
/// How deep below a demo folder suites are looked for.
const MAX_DEPTH: usize = 3;
/// Suite files read at most, folders listed at most, and the largest file
/// read: run from a home folder by mistake, the search stays cheap.
const MAX_FILES: usize = 400;
const MAX_DIRS: usize = 300;
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Folders that hold no suites of the project's own.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "build", "Pods", "DerivedData"];
/// Words that say nothing about which screen a goal is about.
const STOP: &[&str] = &[
    "a", "an", "and", "app", "as", "at", "be", "by", "click", "do", "for", "from", "go", "in",
    "into", "is", "it", "its", "me", "my", "of", "on", "open", "or", "page", "screen", "see",
    "tap", "test", "that", "the", "then", "this", "to", "up", "with",
];

/// One test chosen as a demonstration.
#[derive(Debug, Clone, PartialEq)]
pub struct Demo {
    /// The suite file it came from.
    pub file: PathBuf,
    /// The test's name.
    pub test: String,
    pub score: usize,
    /// The test as the model sees it: a header line, then one line per step.
    pub lines: Vec<String>,
}

/// The goal's words, folded: lowercase, a plural `s` dropped, stop words out.
fn words(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 2 && !STOP.contains(&w.as_str()))
        .map(|w| {
            let n = w.chars().count();
            if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w.chars().take(n - 1).collect()
            } else {
                w
            }
        })
        .collect()
}

/// The text a selector matches on, for retrieval.
fn selector_text(sel: &Selector, out: &mut String) {
    match sel {
        Selector::Id(t)
        | Selector::Text(t)
        | Selector::TextContains(t)
        | Selector::AccessibilityId(t)
        | Selector::Name(t) => {
            out.push(' ');
            out.push_str(t);
        }
        Selector::Index { selector, .. } => selector_text(selector, out),
        Selector::Compound(parts) => parts.iter().for_each(|p| selector_text(p, out)),
        Selector::Relative { target, anchor, .. } => {
            selector_text(target, out);
            selector_text(anchor, out);
        }
        _ => {}
    }
}

/// The element a step acts on or checks.
fn target(a: &Action) -> Option<&Selector> {
    match a {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    }
}

/// A `${NAME}` placeholder, as written.
fn placeholder(text: &str) -> Option<&str> {
    let t = text.trim();
    let name = t.strip_prefix("${")?.strip_suffix('}')?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(t)
}

/// One step as a demo line, or `None` for steps that say nothing about
/// how to move through the app (waits, settings, mocks, HTTP calls).
fn step_line(action: &Action) -> Option<String> {
    let sel = |s: &Selector| clip(&selector_fields(s), 100);
    Some(match action {
        Action::LaunchApp { clear_state, .. } => {
            if *clear_state {
                "launchApp (clean)".into()
            } else {
                "launchApp".into()
            }
        }
        Action::Tap { selector } => format!("tap {{ {} }}", sel(selector)),
        Action::DoubleTap { selector } => format!("doubleTap {{ {} }}", sel(selector)),
        Action::LongPress { selector, .. } => format!("longPress {{ {} }}", sel(selector)),
        Action::ClearText { selector } => format!("clearText {{ {} }}", sel(selector)),
        // The text typed is never shown: only a `${NAME}` says what kind
        // of value went in.
        Action::InputText { selector, text } => format!(
            "inputText {{ {} }} <- {}",
            sel(selector),
            placeholder(text).unwrap_or("(text)")
        ),
        Action::AssertVisible { selector } => format!("assertVisible {{ {} }}", sel(selector)),
        Action::AssertNotVisible { selector } => {
            format!("assertNotVisible {{ {} }}", sel(selector))
        }
        Action::AssertText { selector, .. } => format!("assertText {{ {} }}", sel(selector)),
        Action::ScrollUntilVisible { selector, .. } => {
            format!("scrollUntilVisible {{ {} }}", sel(selector))
        }
        Action::Swipe {
            direction: Some(d), ..
        } => format!("swipe {d:?}").to_lowercase(),
        Action::PressKey { key } => format!("pressKey {key:?}"),
        Action::GoBack => "goBack".into(),
        Action::RunFlow { flow_id } => format!("runFlow {flow_id:?}"),
        Action::Natural {
            intent,
            kind,
            intent_template,
            ..
        } => {
            let verb = match kind {
                NaturalKind::Act => "do",
                NaturalKind::Assert => "assert",
            };
            let intent = intent_template.as_deref().unwrap_or(intent);
            format!("{verb}: {:?}", clip(intent, 100))
        }
        _ => return None,
    })
}

/// A test's steps with its flows inlined (one level: a flow's own
/// `runFlow`s stay as lines).
fn expanded<'a>(steps: &'a [Step], suite: &'a TestSuite) -> Vec<&'a Action> {
    let mut out = Vec::new();
    for s in steps {
        match &s.action {
            Action::RunFlow { flow_id } => match suite.flows.iter().find(|f| &f.id == flow_id) {
                Some(flow) => out.extend(flow.steps.iter().map(|s| &s.action)),
                None => out.push(&s.action),
            },
            a => out.push(a),
        }
    }
    out
}

/// Every suite file under `dir`, nearest first, within the limits
/// (`dirs_left` folders still to list).
fn suite_files(dir: &Path, depth: usize, dirs_left: &mut usize, out: &mut Vec<PathBuf>) {
    if *dirs_left == 0 {
        return;
    }
    *dirs_left -= 1;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut subdirs = Vec::new();
    for path in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                subdirs.push(path);
            }
        } else if velocity_core::parser::is_suite_file(&path)
            && std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_FILE_BYTES)
        {
            out.push(path);
        }
    }
    for d in subdirs {
        suite_files(&d, depth + 1, dirs_left, out);
    }
}

/// The tests under `dirs` most like `goal`, best first: at most
/// [`MAX_DEMOS`], each scoring at least `MIN_SCORE`. Only suites for
/// `app_id` (or an `${APP_ID}`-style placeholder) on `platform` count.
/// Files that don't parse are skipped: a demo is a nicety, never an error.
pub fn find(dirs: &[PathBuf], app_id: &str, platform: Platform, goal: &str) -> Vec<Demo> {
    let goal_words = words(goal);
    if goal_words.is_empty() {
        return vec![];
    }
    let mut files = Vec::new();
    let mut dirs_left = MAX_DIRS;
    for d in dirs {
        suite_files(d, 0, &mut dirs_left, &mut files);
    }
    // Overlapping folders (`.` and `./tests`) name one file twice.
    let mut seen = BTreeSet::new();
    files.retain(|f| seen.insert(f.canonicalize().unwrap_or_else(|_| f.clone())));
    let mut found: Vec<Demo> = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(suite) = velocity_core::parser::parse_suite_from_str(&text) else {
            continue;
        };
        if suite.app_id != app_id && placeholder(&suite.app_id).is_none() {
            continue;
        }
        if suite.config.platform.is_some_and(|p| p != platform) {
            continue;
        }
        for test in &suite.tests {
            let head = words(&format!(
                "{} {}",
                test.name,
                test.description.as_deref().unwrap_or("")
            ));
            let actions = expanded(&test.steps, &suite);
            let mut step_text = String::new();
            for a in &actions {
                match a {
                    Action::Natural { intent, .. } => {
                        step_text.push(' ');
                        step_text.push_str(intent);
                    }
                    a => {
                        if let Some(s) = target(a) {
                            selector_text(s, &mut step_text);
                        }
                    }
                }
            }
            let body = words(&step_text);
            let score: usize = goal_words
                .iter()
                .map(|w| {
                    if head.contains(w) {
                        2
                    } else {
                        usize::from(body.contains(w))
                    }
                })
                .sum();
            if score < MIN_SCORE {
                continue;
            }
            let lines: Vec<String> = actions.iter().filter_map(|a| step_line(a)).collect();
            if lines.is_empty() {
                continue;
            }
            let shown_file = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut header = format!("Test {:?} ({shown_file})", clip(&test.name, 80));
            if let Some(d) = test.description.as_deref().filter(|d| !d.trim().is_empty()) {
                header.push_str(&format!(": {}", clip(d.trim(), 120)));
            }
            let mut demo_lines = vec![header];
            demo_lines.extend(lines);
            found.push(Demo {
                file: file.clone(),
                test: test.name.clone(),
                score,
                lines: demo_lines,
            });
        }
    }
    // Best score first; among equals the shorter test (less to read, and
    // usually the more direct path), then a stable order.
    found.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.lines.len().cmp(&b.lines.len()))
            .then(a.file.cmp(&b.file))
            .then(a.test.cmp(&b.test))
    });
    // A second test only when it is nearly as close as the first: a
    // weak match is noise the model would read as a hint.
    let best = found.first().map_or(0, |d| d.score);
    found.retain(|d| d.score * 2 >= best);
    found.truncate(MAX_DEMOS);
    found
}

/// The demonstrations as the prompt shows them, within `cap` characters:
/// a test that doesn't fit whole is cut at a step, with the rest counted.
/// Empty when nothing fits.
pub fn render(demos: &[Demo], cap: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    // Room kept for the "… N more steps" line.
    const MORE: usize = 30;
    for demo in demos {
        let Some((header, steps)) = demo.lines.split_first() else {
            continue;
        };
        let header_len = header.chars().count() + 1;
        // A header with no step under it shows nothing.
        let first = steps.first().map_or(0, |s| s.chars().count() + 3);
        if used + header_len + first + MORE > cap {
            break;
        }
        out.push_str(header);
        out.push('\n');
        used += header_len;
        for (i, step) in steps.iter().enumerate() {
            let len = step.chars().count() + 3;
            let last = i + 1 == steps.len();
            if used + len + if last { 0 } else { MORE } > cap {
                let n = steps.len() - i;
                let more = format!("- … {n} more step{}\n", if n == 1 { "" } else { "s" });
                used += more.chars().count();
                out.push_str(&more);
                break;
            }
            out.push_str("- ");
            out.push_str(step);
            out.push('\n');
            used += len;
        }
    }
    velocity_common::redact::redact(out.trim_end()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, yaml: &str) {
        std::fs::write(dir.join(name), yaml).unwrap();
    }

    const WIFI: &str = r#"
appId: com.android.settings
tests:
  - name: Turn on Wi-Fi
    description: open Network & internet and turn Wi-Fi on
    steps:
      - launchApp
      - tap: { text: "Network & internet" }
      - tap: { text: "Internet" }
      - inputText: { selector: { id: "search" }, text: "hunter2" }
      - inputText: { selector: { id: "ssid" }, text: "${SSID}" }
      - assertVisible: { text: "Wi-Fi" }
"#;

    const DISPLAY: &str = r#"
appId: com.android.settings
tests:
  - name: Dark theme
    steps:
      - launchApp
      - tap: { text: "Display" }
      - tap: { text: "Dark theme" }
  - name: Battery saver from the network page
    steps:
      - tap: { text: "Battery" }
      - tap: { text: "Network & internet" }
"#;

    const OTHER_APP: &str = r#"
appId: com.example.other
tests:
  - name: Turn on Wi-Fi in another app
    description: open Network & internet and turn Wi-Fi on
    steps:
      - tap: { text: "Wi-Fi" }
"#;

    #[test]
    fn the_most_similar_tests_of_this_app_rank_first() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "wifi.yaml", WIFI);
        write(dir.path(), "display.yaml", DISPLAY);
        write(dir.path(), "other.yaml", OTHER_APP);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        write(dir.path(), "sub/broken.yaml", "appId: [unclosed");
        write(dir.path(), "notes.txt", WIFI);
        let found = find(
            &[dir.path().to_path_buf()],
            "com.android.settings",
            Platform::Android,
            "Turn Wi-Fi on from Network & internet",
        );
        let names: Vec<&str> = found.iter().map(|d| d.test.as_str()).collect();
        // The name and description match; the battery test shares only
        // "network" and "internet" (far behind), the other app's test and
        // the dark theme test (no goal word) are out.
        assert_eq!(names, ["Turn on Wi-Fi"], "{found:?}");

        // Two close matches: both, the closer first.
        let found = find(
            &[dir.path().to_path_buf(), dir.path().join(".")],
            "com.android.settings",
            Platform::Android,
            "Network & internet",
        );
        let names: Vec<&str> = found.iter().map(|d| d.test.as_str()).collect();
        assert_eq!(
            names,
            ["Turn on Wi-Fi", "Battery saver from the network page"],
            "{found:?}"
        );
        assert!(found[0].score > found[1].score);

        let found = find(
            &[dir.path().to_path_buf()],
            "com.android.settings",
            Platform::Android,
            "switch to the dark theme in Display",
        );
        assert_eq!(found[0].test, "Dark theme");

        // Nothing similar: no demo rather than a random one.
        assert!(find(
            &[dir.path().to_path_buf()],
            "com.android.settings",
            Platform::Android,
            "add a calendar event",
        )
        .is_empty());
        assert!(find(&[], "com.android.settings", Platform::Android, "wifi").is_empty());
    }

    #[test]
    fn typed_text_is_shown_only_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "wifi.yaml", WIFI);
        let found = find(
            &[dir.path().to_path_buf()],
            "com.android.settings",
            Platform::Android,
            "turn on wifi",
        );
        let text = render(&found, DEMO_CHARS);
        assert!(
            text.contains("inputText { id: \"ssid\" } <- ${SSID}"),
            "{text}"
        );
        assert!(
            text.contains("inputText { id: \"search\" } <- (text)"),
            "{text}"
        );
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.starts_with("Test \"Turn on Wi-Fi\" (wifi.yaml): open Network"));
        assert!(
            text.contains("- tap { text: \"Network & internet\" }"),
            "{text}"
        );
    }

    #[test]
    fn the_rendering_keeps_to_its_cap() {
        let long = Demo {
            file: "a.yaml".into(),
            test: "long".into(),
            score: 4,
            lines: std::iter::once("Test \"long\" (a.yaml)".to_string())
                .chain((0..200).map(|i| format!("tap {{ text: \"Ünïcödé row {i}\" }}")))
                .collect(),
        };
        let second = Demo {
            test: "second".into(),
            lines: vec![
                "Test \"second\" (b.yaml)".into(),
                "tap { text: \"x\" }".into(),
            ],
            ..long.clone()
        };
        let text = render(&[long.clone(), second], DEMO_CHARS);
        assert!(
            text.chars().count() <= DEMO_CHARS,
            "{}",
            text.chars().count()
        );
        assert!(text.contains("more steps"), "{text}");
        // The first test took the budget; the second didn't fit.
        assert!(!text.contains("second"), "{text}");
        // A cap too small for a header and one step shows nothing.
        assert_eq!(render(&[long], 20), "");
    }

    #[test]
    fn words_fold_case_plurals_and_stop_words() {
        let w = words("Open the Contacts app and ADD a contact, Wi-Fi");
        assert!(w.contains("contact") && w.contains("add") && w.contains("wi"));
        assert!(!w.contains("the") && !w.contains("open") && !w.contains("contacts"));
        assert_eq!(placeholder(" ${EMAIL} "), Some("${EMAIL}"));
        assert_eq!(placeholder("${A} and ${B}"), None);
    }
}
