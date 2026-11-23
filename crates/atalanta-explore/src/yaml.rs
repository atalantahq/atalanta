// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Turning a session into a test: a plain, deterministic Atalanta suite.
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

use atalanta_common::{Action, AtalantaError, Direction, Key, LaunchArgs, Result, Selector};
use serde_yaml::{Mapping, Value};

use crate::session::{ExploreReport, LaunchMode, RecordedStep, ValueSlot};
use atalanta_nl::GoalOp;

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
    /// The final assertion at this index of [`ExploreReport::final_assertions`]
    /// (not its place among the written ones: a repeat is skipped).
    Final(usize),
}

/// A generated suite: the YAML and where each step came from.
#[derive(Debug, Clone)]
pub struct Generated {
    pub yaml: String,
    pub origins: Vec<Origin>,
    pub assertions: usize,
}

/// A string the generated suite holds as written. Screen text, selectors
/// and goal literals can contain `${NAME}`; the replay interpolates the
/// suite, so each `$` before a `{` is doubled (`env::escape`) or a page
/// showing `${AWS_SECRET_ACCESS_KEY}` would make the replay read that
/// variable (security audit 2026-11, MCP-2).
fn s(v: &str) -> Value {
    Value::String(atalanta_core::env::escape(v))
}

/// A value slot's placeholder (`${NAME}`), which the replay must fill in.
/// Only [`ValueSlot::yaml_text`] of a data or secret slot goes here.
fn placeholder(slot: &ValueSlot) -> Value {
    match slot.kind {
        crate::session::ValueKind::Literal => s(&slot.value),
        _ => Value::String(slot.yaml_text()),
    }
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
    k.name()
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
                Some(slot) => placeholder(slot),
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
                map(vec![("selector", selector_value(selector)), ("text", text)]),
            )
        }
        Action::ClearText { selector } => step("clearText", selector_value(selector)),
        Action::AssertVisible { selector } => step("assertVisible", selector_value(selector)),
        Action::AssertNotVisible { selector } => step("assertNotVisible", selector_value(selector)),
        // Everything the explored swipe had: a scroll inside a picker or a
        // lane stroke replayed as a whole-screen swipe tested a different
        // action (review 08 F4).
        Action::Swipe {
            direction,
            from,
            to,
            within,
        } if direction.is_some() || (from.is_some() && to.is_some()) => {
            let point = |(x, y): (i32, i32)| {
                map(vec![
                    ("x", Value::Number(x.into())),
                    ("y", Value::Number(y.into())),
                ])
            };
            let mut pairs = Vec::new();
            if let Some(d) = direction {
                pairs.push(("direction", dir(*d)));
            }
            if let (Some(a), Some(b)) = (from, to) {
                pairs.push(("from", point(*a)));
                pairs.push(("to", point(*b)));
            }
            if let Some(w) = within {
                pairs.push(("within", selector_value(w)));
            }
            step("swipe", map(pairs))
        }
        Action::ScrollUntilVisible {
            selector,
            direction,
            max_scrolls,
            within,
        } => {
            let mut pairs = vec![
                ("selector", selector_value(selector)),
                ("direction", dir(*direction)),
                ("maxScrolls", Value::Number((*max_scrolls).into())),
            ];
            if let Some(w) = within {
                pairs.push(("within", selector_value(w)));
            }
            step("scrollUntilVisible", map(pairs))
        }
        Action::PressKey { key } => step("pressKey", map(vec![("key", s(key_name(key)))])),
        Action::Wait { ms } => step("wait", map(vec![("ms", Value::Number((*ms).into()))])),
        Action::GoBack => step("goBack", Value::Mapping(Mapping::new())),
        Action::Drag { from, to, .. } => step(
            "drag",
            map(vec![
                ("from", selector_value(from)),
                ("to", selector_value(to)),
            ]),
        ),
        other => return Err(internal(format!("explore can't write {other:?} as a step"))),
    })
}

fn internal(msg: String) -> AtalantaError {
    AtalantaError::Internal(anyhow::anyhow!(msg))
}

/// A short label for logs: `tap { text: "Pay" }`.
pub fn action_label(action: &Action) -> String {
    action_label_named(action, None)
}

/// [`action_label`], with an input's value shown by name.
pub fn action_label_named(action: &Action, typed: Option<&ValueSlot>) -> String {
    let sel = |s: &Selector| atalanta_core::executor::selector_fields(s);
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
        Action::Drag { from, to, .. } => format!("drag {{ {} }} to {{ {} }}", sel(from), sel(to)),
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
                    arguments: LaunchArgs::default(),
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
            // The scrolls merge only when they were all in one container
            // (or all of the whole screen), which the merged step keeps.
            let within = |i: &usize| match &report.steps[*i].action {
                Action::Swipe { within, .. } => within.clone(),
                _ => None,
            };
            let first_within = within(&scrolls[0]);
            let one_place = scrolls.iter().all(|i| within(i) == first_within);
            match (
                action_selector(&rec.action).filter(|_| one_place),
                report.steps[scrolls[0]].scroll,
            ) {
                (Some(sel), Some(direction)) => {
                    steps.push(action_value(
                        &Action::ScrollUntilVisible {
                            selector: sel.clone(),
                            direction,
                            max_scrolls: 10,
                            within: first_within,
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
        let mut step = action_value(&rec.action, rec.typed.as_ref())?;
        if rec.force {
            if let Value::Mapping(m) = &mut step {
                m.insert(s("force"), Value::Bool(true));
            }
        }
        steps.push(step);
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
    for (n, sel) in report.final_assertions.iter().enumerate() {
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
        origins.push(Origin::Final(n));
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

/// Every string in `v`, keys included.
fn strings_of(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(t) => out.push(t.clone()),
        Value::Sequence(items) => items.iter().for_each(|i| strings_of(i, out)),
        Value::Mapping(m) => {
            for (k, val) in m {
                strings_of(k, out);
                strings_of(val, out);
            }
        }
        _ => {}
    }
}

/// Parse and validate the generated YAML, and make sure no secret's value
/// made it in.
pub fn check(yaml: &str, report: &ExploreReport) -> Result<()> {
    // The strings of the file as the parser reads them, with the `$` the
    // writer doubled undone: a value with a newline, a tab or a `${` is
    // not in the written text as it is, though it is in the file.
    let read: Vec<String> = serde_yaml::from_str::<Value>(yaml)
        .map(|doc| {
            let mut out = Vec::new();
            strings_of(&doc, &mut out);
            out.into_iter()
                .flat_map(|t| [t.replace("$$", "$"), t])
                .collect()
        })
        .unwrap_or_default();
    // The run's own values, and the ones typed by the steps replayed before
    // the first turn (`--after`'s login: its secrets are not in `values`).
    let typed = report.steps.iter().filter_map(|s| s.typed.as_ref());
    for v in report.values.iter().chain(typed) {
        if v.kind == crate::session::ValueKind::Secret
            && !v.value.is_empty()
            && (yaml.contains(&v.value) || read.iter().any(|t| t.contains(&v.value)))
        {
            return Err(internal(format!(
                "the generated test would contain the value of secret {}",
                v.name
            )));
        }
    }
    let suite = atalanta_core::parse_suite_from_str(yaml)
        .map_err(|e| internal(format!("the generated test doesn't parse: {e}")))?;
    atalanta_core::validate_suite(&suite)
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
                arguments: LaunchArgs::default(),
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
            // `data` is only ever a data/secret slot's `${NAME}`.
            pairs.push(("data", Value::String(d)));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `action` written as a step, parsed back by the suite parser.
    fn round_trip(action: &Action) -> (String, Action) {
        let step = action_value(action, None).unwrap();
        let doc = map(vec![
            ("appId", s("a")),
            (
                "tests",
                Value::Sequence(vec![map(vec![
                    ("name", s("t")),
                    ("steps", Value::Sequence(vec![step])),
                ])]),
            ),
        ]);
        let yaml = serde_yaml::to_string(&doc).unwrap();
        let suite = atalanta_core::parse_suite_from_str(&yaml).unwrap();
        (yaml, suite.tests[0].steps[0].action.clone())
    }

    fn report_with(values: Vec<ValueSlot>) -> ExploreReport {
        ExploreReport {
            goal: "g".into(),
            app_id: "a".into(),
            platform: atalanta_common::Platform::Android,
            launch: LaunchMode::Launch,
            outcome: crate::session::ExploreOutcome::Success,
            reason: String::new(),
            steps: vec![],
            final_assertions: vec![],
            checks: vec![],
            turns: 0,
            model_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            elapsed_ms: 0,
            values,
            answer: None,
            answer_sources: vec![],
            prompt_version: "",
            demos: vec![],
            expectations: Default::default(),
            revisit_turns: 0,
            final_basis: None,
        }
    }

    // The scan for a secret's value runs on the YAML as written, where the
    // writer has quoted it (a single quote is doubled), split it over lines
    // or doubled the `$` before a `{`: the plain text of the value is then
    // not in the file though the value is.
    #[test]
    fn a_secret_the_writer_quotes_or_escapes_is_still_found() {
        use crate::session::ValueKind;
        for secret in [
            "it's-a-secret",
            "say \"hunter2\" now",
            "line one\nline two",
            "pa${ss}word",
            "tab\there",
        ] {
            let tap = Action::Tap {
                selector: Selector::Text(secret.into()),
            };
            let doc = map(vec![
                ("appId", s("a")),
                (
                    "tests",
                    Value::Sequence(vec![map(vec![
                        ("name", s("t")),
                        (
                            "steps",
                            Value::Sequence(vec![action_value(&tap, None).unwrap()]),
                        ),
                    ])]),
                ),
            ]);
            let yaml = serde_yaml::to_string(&doc).unwrap();
            let report = report_with(vec![ValueSlot {
                name: "PASSWORD".into(),
                kind: ValueKind::Secret,
                value: secret.into(),
            }]);
            let err = check(&yaml, &report).expect_err(&format!("{secret:?} in\n{yaml}"));
            assert!(err.to_string().contains("PASSWORD"), "{err}");
            // The same suite with another secret is fine.
            let other = report_with(vec![ValueSlot {
                name: "PIN".into(),
                kind: ValueKind::Secret,
                value: "not-in-the-file".into(),
            }]);
            assert!(check(&yaml, &other).is_ok(), "{yaml}");
        }
    }

    // `--after`'s login secrets are typed by replayed steps and are not among
    // the run's values: the scan has to look at the steps too.
    #[test]
    fn a_secret_typed_by_a_replayed_step_is_scanned_for_too() {
        use crate::session::ValueKind;
        let secret = "totp-481516";
        let typed = RecordedStep {
            turn: 0,
            op: GoalOp::InputText,
            action: Action::InputText {
                selector: Selector::Id("otp".into()),
                text: secret.into(),
            },
            label: String::new(),
            typed: Some(ValueSlot {
                name: "totp:LOGIN".into(),
                kind: ValueKind::Secret,
                value: secret.into(),
            }),
            target: None,
            scroll: None,
            before_title: String::new(),
            after_title: String::new(),
            changed: true,
            ms: 0,
            screen_assertion: None,
            expectation: None,
            post_assertion: None,
            force: false,
        };
        let mut report = report_with(vec![]);
        report.steps = vec![typed];
        // A test that holds the value (here as a selector's text).
        let yaml = format!(
            "appId: a\ntests:\n  - name: t\n    steps:\n      - tap: {{ text: \"{secret}\" }}\n"
        );
        let err = check(&yaml, &report).unwrap_err();
        assert!(err.to_string().contains("totp:LOGIN"), "{err}");
        report.steps.clear();
        assert!(check(&yaml, &report).is_ok());
    }

    #[test]
    fn every_action_explore_writes_parses_back_as_itself() {
        let t = |x: &str| Selector::Text(x.into());
        let actions = [
            Action::Tap { selector: t("a") },
            Action::DoubleTap { selector: t("a") },
            Action::LongPress {
                selector: t("a"),
                duration_ms: Some(1500),
            },
            Action::LongPress {
                selector: t("a"),
                duration_ms: None,
            },
            Action::ClearText { selector: t("a") },
            Action::AssertVisible { selector: t("a") },
            Action::AssertNotVisible { selector: t("a") },
            Action::PressKey { key: Key::Back },
            Action::PressKey { key: Key::Home },
            Action::PressKey { key: Key::Enter },
            Action::Wait { ms: 250 },
            Action::GoBack,
            Action::Drag {
                from: t("a"),
                to: t("b"),
                hold_ms: None,
                move_ms: None,
            },
            Action::LaunchApp {
                app_id: "a".into(),
                clear_state: true,
                arguments: LaunchArgs::default(),
            },
        ];
        for a in &actions {
            let (yaml, back) = round_trip(a);
            // `launchApp` takes the suite's appId; compare the rest as written.
            if let Action::LaunchApp { clear_state, .. } = a {
                assert!(
                    matches!(back, Action::LaunchApp { clear_state: c, .. } if c == *clear_state),
                    "{yaml}"
                );
            } else {
                assert_eq!(&back, a, "{yaml}");
            }
        }
    }

    #[test]
    fn a_swipe_keeps_its_container_and_its_stroke() {
        let within = Action::Swipe {
            direction: Some(Direction::Up),
            from: None,
            to: None,
            within: Some(Selector::Id("hour_picker".into())),
        };
        let (yaml, back) = round_trip(&within);
        assert_eq!(back, within, "{yaml}");
        let lane = Action::Swipe {
            direction: Some(Direction::Right),
            from: Some((100, 400)),
            to: Some((900, 400)),
            within: None,
        };
        let (yaml, back) = round_trip(&lane);
        assert_eq!(back, lane, "{yaml}");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Display".into()),
            direction: Direction::Down,
            max_scrolls: 10,
            within: Some(Selector::Id("list".into())),
        };
        let (yaml, back) = round_trip(&scroll);
        assert_eq!(back, scroll, "{yaml}");
    }

    #[test]
    fn a_literal_or_selector_with_a_placeholder_is_written_escaped() {
        use crate::session::ValueKind;
        // A page showing `${AWS_SECRET_ACCESS_KEY}` (or a goal quoting it)
        // must not make the replay read the variable (audit 2026-11 MCP-2).
        let tap = Action::Tap {
            selector: Selector::Text("${VEL_AUDIT_SECRET_X}".into()),
        };
        let (yaml, back) = round_trip(&tap);
        let Action::Tap {
            selector: Selector::Text(t),
        } = back
        else {
            panic!("{yaml}")
        };
        let none = std::collections::HashMap::new();
        assert_eq!(
            atalanta_core::env::interpolate(&t, &none).unwrap(),
            "${VEL_AUDIT_SECRET_X}",
            "{yaml}"
        );
        let input = Action::InputText {
            selector: Selector::Id("q".into()),
            text: String::new(),
        };
        let literal = ValueSlot {
            name: "TEXT1".into(),
            kind: ValueKind::Literal,
            value: "${keychain:LOGIN_PW}".into(),
        };
        let v = action_value(&input, Some(&literal)).unwrap();
        let text = v["inputText"]["text"].as_str().unwrap().to_string();
        assert_eq!(
            atalanta_core::env::interpolate(&text, &none).unwrap(),
            "${keychain:LOGIN_PW}"
        );
        let secret = ValueSlot {
            name: "PASSWORD".into(),
            kind: ValueKind::Secret,
            value: "hunter2".into(),
        };
        let v = action_value(&input, Some(&secret)).unwrap();
        assert_eq!(v["inputText"]["text"].as_str(), Some("${PASSWORD}"));
    }
}
