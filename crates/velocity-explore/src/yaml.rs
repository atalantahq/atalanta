//! Turning a session into a test: a plain, deterministic Velocity suite.
//!
//! Rules (docs/EXPLORE.md has the long form):
//! - `appId`, one test named after the goal, `description:` = the goal (so
//!   `verify: final` can check it later);
//! - `launchApp` (with `clearState: true` when the session started clean);
//! - each recorded action with the selector code built when it ran (id >
//!   accessibility label > unique text; `index:` only when there were twins);
//! - a run of scrolls before an action becomes one `scrollUntilVisible` on
//!   that action's target, so the test doesn't depend on scroll distances;
//! - after each step that changed the screen, an `assertVisible` on a
//!   stable element new to that screen (no volatile text, no user data);
//! - after a step whose expected post-condition held, its assertion
//!   (`assertVisible`, `assertNotVisible`, or a `checked:` state), built
//!   from the element that matched, deduped with the screen assertion;
//! - the assertions the model proposed, and at the end assertions on the
//!   elements the checker cited for the goal;
//! - typed values: the goal's literals as written, `--data` and `--secret`
//!   values as `${NAME}`. A secret's value is never in the file.
//!
//! Built as a `serde_yaml::Value` (no string splicing) and checked with the
//! suite parser and validator before it is returned.

use serde_yaml::{Mapping, Value};
use velocity_common::{Action, Direction, Key, Result, Selector, VelocityError};

use crate::session::{ExploreReport, LaunchMode, RecordedStep, ValueSlot};
use velocity_nl::GoalOp;

/// Where a generated step came from (for the repair: which recorded step
/// a failing YAML step belongs to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Launch,
    /// The recorded step with this index (an action, a proposed assertion,
    /// or the scrollUntilVisible in front of it).
    Step(usize),
    /// The screen assertion after this recorded step.
    ScreenAssert(usize),
    /// The assertion of this recorded step's met expectation.
    ExpectAssert(usize),
    Final,
}

/// A generated suite: the YAML and where each step came from.
#[derive(Debug, Clone)]
pub struct Generated {
    pub yaml: String,
    pub origins: Vec<Origin>,
    pub assertions: usize,
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(s(k), v);
    }
    Value::Mapping(m)
}

fn dir(d: Direction) -> Value {
    s(match d {
        Direction::Up => "up",
        Direction::Down => "down",
        Direction::Left => "left",
        Direction::Right => "right",
    })
}

/// A selector as its YAML map.
pub fn selector_value(sel: &Selector) -> Value {
    let mut m = Mapping::new();
    fill(sel, &mut m);
    Value::Mapping(m)
}

fn fill(sel: &Selector, m: &mut Mapping) {
    let mut put = |k: &str, v: Value| {
        m.insert(s(k), v);
    };
    match sel {
        Selector::Id(v) => put("id", s(v)),
        Selector::Text(v) => put("text", s(v)),
        Selector::TextContains(v) => put("textContains", s(v)),
        Selector::TextMatches(v) => put("textMatches", s(v)),
        Selector::IdMatches(v) => put("idMatches", s(v)),
        Selector::AccessibilityId(v) => put("accessibilityId", s(v)),
        Selector::ClassName(v) => put("className", s(v)),
        Selector::Css(v) => put("css", s(v)),
        Selector::XPath(v) => put("xpath", s(v)),
        Selector::Role(v) => put("role", s(v)),
        Selector::Name(v) => put("name", s(v)),
        Selector::Checked(b) => put("checked", Value::Bool(*b)),
        Selector::Focused(b) => put("focused", Value::Bool(*b)),
        Selector::Selected(b) => put("selected", Value::Bool(*b)),
        Selector::Enabled(b) => put("enabled", Value::Bool(*b)),
        Selector::Index { selector, index } => {
            fill(selector, m);
            m.insert(s("index"), Value::Number((*index as u64).into()));
        }
        Selector::Compound(parts) => {
            for p in parts {
                fill(p, m);
            }
        }
        Selector::Relative {
            target,
            relation,
            anchor,
        } => {
            fill(target, m);
            m.insert(s(&relation.to_string()), selector_value(anchor));
        }
    }
}

fn key_name(k: &Key) -> &'static str {
    match k {
        Key::Back => "back",
        Key::Home => "home",
        Key::Enter => "enter",
        Key::VolumeUp => "volumeUp",
        Key::VolumeDown => "volumeDown",
        Key::Backspace => "backspace",
        Key::Tab => "tab",
        Key::Escape => "escape",
        Key::Delete => "delete",
        Key::ArrowUp => "arrowUp",
        Key::ArrowDown => "arrowDown",
        Key::ArrowLeft => "arrowLeft",
        Key::ArrowRight => "arrowRight",
    }
}

/// An action as a YAML step. `typed` replaces an inputText's real value
/// with the slot's YAML text (`${NAME}` for data and secrets).
pub fn action_value(action: &Action, typed: Option<&ValueSlot>) -> Result<Value> {
    let step = |name: &str, arg: Value| map(vec![(name, arg)]);
    Ok(match action {
        Action::LaunchApp { clear_state, .. } => step(
            "launchApp",
            if *clear_state {
                map(vec![("clearState", Value::Bool(true))])
            } else {
                Value::Mapping(Mapping::new())
            },
        ),
        Action::Tap { selector } => step("tap", selector_value(selector)),
        Action::DoubleTap { selector } => step("doubleTap", selector_value(selector)),
        Action::LongPress {
            selector,
            duration_ms,
        } => {
            let mut pairs = vec![("selector", selector_value(selector))];
            if let Some(ms) = duration_ms {
                pairs.push(("durationMs", Value::Number((*ms).into())));
            }
            step("longPress", map(pairs))
        }
        Action::InputText { selector, .. } => {
            let text = match typed {
                Some(slot) => slot.yaml_text(),
                // A recorded input always has its slot; never fall back to
                // a raw value that could be a secret.
                None => {
                    return Err(internal(format!(
                        "inputText into {selector} has no value slot"
                    )))
                }
            };
            step(
                "inputText",
                map(vec![
                    ("selector", selector_value(selector)),
                    ("text", s(&text)),
                ]),
            )
        }
        Action::ClearText { selector } => step("clearText", selector_value(selector)),
        Action::AssertVisible { selector } => step("assertVisible", selector_value(selector)),
        Action::AssertNotVisible { selector } => step("assertNotVisible", selector_value(selector)),
        Action::Swipe {
            direction: Some(d), ..
        } => step("swipe", map(vec![("direction", dir(*d))])),
        Action::ScrollUntilVisible {
            selector,
            direction,
            ..
        } => step(
            "scrollUntilVisible",
            map(vec![
                ("selector", selector_value(selector)),
                ("direction", dir(*direction)),
            ]),
        ),
        Action::PressKey { key } => step("pressKey", map(vec![("key", s(key_name(key)))])),
        Action::Wait { ms } => step("wait", map(vec![("ms", Value::Number((*ms).into()))])),
        Action::GoBack => step("goBack", Value::Mapping(Mapping::new())),
        other => return Err(internal(format!("explore can't write {other:?} as a step"))),
    })
}

fn internal(msg: String) -> VelocityError {
    VelocityError::Internal(anyhow::anyhow!(msg))
}

/// A short label for logs: `tap { text: "Pay" }`.
pub fn action_label(action: &Action) -> String {
    action_label_named(action, None)
}

/// [`action_label`], with an input's value shown by name.
pub fn action_label_named(action: &Action, typed: Option<&ValueSlot>) -> String {
    let sel = |s: &Selector| velocity_core::executor::selector_fields(s);
    match action {
        Action::Tap { selector } => format!("tap {{ {} }}", sel(selector)),
        Action::LongPress { selector, .. } => format!("longPress {{ {} }}", sel(selector)),
        Action::ClearText { selector } => format!("clearText {{ {} }}", sel(selector)),
        Action::InputText { selector, .. } => format!(
            "inputText {{ {} }} <- {}",
            sel(selector),
            typed.map(|t| t.name.as_str()).unwrap_or("?")
        ),
        Action::AssertVisible { selector } => format!("assertVisible {{ {} }}", sel(selector)),
        Action::AssertNotVisible { selector } => {
            format!("assertNotVisible {{ {} }}", sel(selector))
        }
        Action::Swipe {
            direction: Some(d), ..
        } => format!("swipe {d:?}").to_lowercase(),
        Action::PressKey { key } => format!("pressKey {}", key_name(key)),
        Action::LaunchApp { clear_state, .. } => {
            if *clear_state {
                "launchApp (clean)".into()
            } else {
                "launchApp".into()
            }
        }
        Action::Wait { ms } => format!("wait {ms} ms"),
        Action::GoBack => "goBack".into(),
        other => format!("{other:?}"),
    }
}

fn action_selector(a: &Action) -> Option<&Selector> {
    match a {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector } => Some(selector),
        _ => None,
    }
}

/// Whether `sel` names the same text as one of `asserted`: a cell and its
/// label both called "About" are one assertion, not two.
fn same_content(sel: &Selector, asserted: &[Selector]) -> bool {
    let names = content_of(sel);
    !names.is_empty()
        && asserted
            .iter()
            .any(|a| content_of(a).iter().any(|n| names.contains(n)))
}

/// The texts and names a selector matches on.
fn content_of(sel: &Selector) -> Vec<String> {
    match sel {
        Selector::Text(t)
        | Selector::TextContains(t)
        | Selector::AccessibilityId(t)
        | Selector::Name(t) => vec![t.clone()],
        Selector::Compound(parts) => parts.iter().flat_map(content_of).collect(),
        Selector::Index { selector, .. } => content_of(selector),
        Selector::Relative { target, .. } => content_of(target),
        _ => vec![],
    }
}

/// Options for the generated test.
#[derive(Debug, Clone, Default)]
pub struct GenOptions {
    /// The test's name (default: the goal).
    pub name: Option<String>,
}

/// Write the session as a suite. Fails if the result doesn't parse and
/// validate (a bug here, never written to disk).
pub fn generate(report: &ExploreReport, opts: &GenOptions) -> Result<Generated> {
    let mut steps: Vec<Value> = Vec::new();
    let mut origins: Vec<Origin> = Vec::new();
    let mut assertions = 0;
    match report.launch {
        LaunchMode::Clean | LaunchMode::Launch => {
            steps.push(action_value(
                &Action::LaunchApp {
                    app_id: report.app_id.clone(),
                    clear_state: report.launch == LaunchMode::Clean,
                },
                None,
            )?);
            origins.push(Origin::Launch);
        }
        LaunchMode::Attach => {}
    }
    // Assertions written since the last action, to skip repeats.
    let mut asserted: Vec<Selector> = Vec::new();
    let mut scrolls: Vec<usize> = Vec::new();
    let flush_scrolls = |scrolls: &mut Vec<usize>,
                         steps: &mut Vec<Value>,
                         origins: &mut Vec<Origin>|
     -> Result<()> {
        for i in scrolls.drain(..) {
            steps.push(action_value(&report.steps[i].action, None)?);
            origins.push(Origin::Step(i));
        }
        Ok(())
    };
    for (i, rec) in report.steps.iter().enumerate() {
        if rec.op == GoalOp::Scroll {
            scrolls.push(i);
            continue;
        }
        if !scrolls.is_empty() {
            match (
                action_selector(&rec.action),
                report.steps[scrolls[0]].scroll,
            ) {
                (Some(sel), Some(direction)) => {
                    steps.push(action_value(
                        &Action::ScrollUntilVisible {
                            selector: sel.clone(),
                            direction,
                            max_scrolls: 10,
                            within: None,
                        },
                        None,
                    )?);
                    origins.push(Origin::Step(i));
                    scrolls.clear();
                }
                _ => flush_scrolls(&mut scrolls, &mut steps, &mut origins)?,
            }
        }
        if let Action::AssertVisible { selector } = &rec.action {
            if asserted.contains(selector) || same_content(selector, &asserted) {
                continue;
            }
            asserted.push(selector.clone());
            assertions += 1;
        } else {
            asserted.clear();
        }
        steps.push(action_value(&rec.action, rec.typed.as_ref())?);
        origins.push(Origin::Step(i));
        if let Some(sel) = &rec.screen_assertion {
            steps.push(action_value(
                &Action::AssertVisible {
                    selector: sel.clone(),
                },
                None,
            )?);
            origins.push(Origin::ScreenAssert(i));
            asserted.push(sel.clone());
            assertions += 1;
        }
        if let Some(post) = &rec.post_assertion {
            let sel = &post.selector;
            // A state assertion (`checked: true`) says more than a plain
            // one on the same text, so only an identical one is a repeat.
            let repeat = asserted.contains(sel)
                || (post.visible && !crate::expect::has_state(sel) && same_content(sel, &asserted));
            if !repeat {
                let action = if post.visible {
                    Action::AssertVisible {
                        selector: sel.clone(),
                    }
                } else {
                    Action::AssertNotVisible {
                        selector: sel.clone(),
                    }
                };
                steps.push(action_value(&action, None)?);
                origins.push(Origin::ExpectAssert(i));
                if post.visible {
                    asserted.push(sel.clone());
                }
                assertions += 1;
            }
        }
    }
    flush_scrolls(&mut scrolls, &mut steps, &mut origins)?;
    for sel in &report.final_assertions {
        // The same element by another selector (a cell and its label
        // both named "About") proves nothing new.
        if asserted.contains(sel) || same_content(sel, &asserted) {
            continue;
        }
        asserted.push(sel.clone());
        steps.push(action_value(
            &Action::AssertVisible {
                selector: sel.clone(),
            },
            None,
        )?);
        origins.push(Origin::Final);
        assertions += 1;
    }

    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| report.goal.trim().to_string());
    let test = map(vec![
        ("name", s(&name)),
        ("description", s(report.goal.trim())),
        ("steps", Value::Sequence(steps)),
    ]);
    let doc = map(vec![
        ("appId", s(&report.app_id)),
        ("tests", Value::Sequence(vec![test])),
    ]);
    let yaml = serde_yaml::to_string(&doc).map_err(|e| internal(format!("writing YAML: {e}")))?;
    check(&yaml, report)?;
    Ok(Generated {
        yaml,
        origins,
        assertions,
    })
}

/// Parse and validate the generated YAML, and make sure no secret's value
/// made it in.
pub fn check(yaml: &str, report: &ExploreReport) -> Result<()> {
    for v in &report.values {
        if v.kind == crate::session::ValueKind::Secret
            && !v.value.is_empty()
            && yaml.contains(&v.value)
        {
            return Err(internal(format!(
                "the generated test would contain the value of secret {}",
                v.name
            )));
        }
    }
    let suite = velocity_core::parse_suite_from_str(yaml)
        .map_err(|e| internal(format!("the generated test doesn't parse: {e}")))?;
    velocity_core::validate_suite(&suite)
        .map_err(|e| internal(format!("the generated test doesn't validate: {e}")))
}

/// The same session as plain-English steps (`--keep-nl`): one `do:` per
/// action and one `assert:` per assertion, for teams that want steps the
/// planner re-resolves (and heals) when the UI changes. Values stay
/// `data: ${NAME}`. Replaying it needs a model; nothing is cached yet.
pub fn generate_nl(report: &ExploreReport, opts: &GenOptions) -> Result<String> {
    let mut steps: Vec<Value> = Vec::new();
    if report.launch != LaunchMode::Attach {
        steps.push(action_value(
            &Action::LaunchApp {
                app_id: report.app_id.clone(),
                clear_state: report.launch == LaunchMode::Clean,
            },
            None,
        )?);
    }
    let q = |t: &Option<String>| format!("\"{}\"", t.as_deref().unwrap_or("it").replace('"', "'"));
    for rec in &report.steps {
        let RecordedStep { target, typed, .. } = rec;
        let (key, text, data) = match (&rec.action, typed) {
            (Action::Tap { .. }, _) => ("do", format!("Tap {}", q(target)), None),
            (Action::LongPress { .. }, _) => ("do", format!("Long-press {}", q(target)), None),
            (Action::ClearText { .. }, _) => {
                ("do", format!("Clear the text in {}", q(target)), None)
            }
            (Action::InputText { .. }, Some(slot)) => match slot.kind {
                crate::session::ValueKind::Literal => (
                    "do",
                    format!("Type \"{}\" into {}", slot.value, q(target)),
                    None,
                ),
                _ => (
                    "do",
                    format!("Type the value into {}", q(target)),
                    Some(slot.yaml_text()),
                ),
            },
            (Action::AssertVisible { .. }, _) => {
                ("assert", format!("{} is visible", q(target)), None)
            }
            (Action::Swipe { .. }, _) if rec.scroll.is_some() => (
                "do",
                format!(
                    "Scroll {}",
                    format!("{:?}", rec.scroll.unwrap_or(Direction::Down)).to_lowercase()
                ),
                None,
            ),
            (Action::PressKey { key: Key::Back }, _) => ("do", "Go back".to_string(), None),
            (other, _) => {
                steps.push(action_value(other, typed.as_ref())?);
                continue;
            }
        };
        let mut pairs = vec![(key, s(&text))];
        if let Some(d) = data {
            pairs.push(("data", s(&d)));
        }
        steps.push(map(pairs));
    }
    steps.push(map(vec![("assert", s(report.goal.trim()))]));
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| report.goal.trim().to_string());
    let doc = map(vec![
        ("appId", s(&report.app_id)),
        (
            "tests",
            Value::Sequence(vec![map(vec![
                ("name", s(&format!("{name} (natural language)"))),
                ("description", s(report.goal.trim())),
                ("steps", Value::Sequence(steps)),
            ])]),
        ),
    ]);
    let yaml = serde_yaml::to_string(&doc).map_err(|e| internal(format!("writing YAML: {e}")))?;
    check(&yaml, report)?;
    Ok(yaml)
}
