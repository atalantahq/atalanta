//! The session recording behind `export_session_flow` and
//! `generate_test_skeleton`.
//!
//! Each successful device action is kept with the selector it really used
//! (a ref is resolved first, so a flow never records one). After an action
//! that changed the screen, the next tree the server reads (before the next
//! action, a snapshot, or the export) decides an `assertVisible` for it: an
//! element of the new screen that the old one didn't have, by a selector
//! that finds only it, preferring an id, then an accessibility label, then a
//! unique text, and never text that looks volatile (times, counts, prices)
//! or that the agent typed.
//!
//! The YAML is built as a `serde_yaml` value and serialized: no value is
//! ever spliced into YAML text.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_yaml::{Mapping, Value as Yaml};
use velocity_common::selector_build::robust_selector;
use velocity_common::selector_match::find_all_ref;
use velocity_common::{Element, Platform, Rect, Selector};

/// Tools whose successful calls become steps.
pub const RECORDED_TOOLS: &[&str] = &[
    "tap",
    "type_text",
    "swipe",
    "press_key",
    "scroll",
    "launch_app",
    "stop_app",
    "assert_visible",
    "assert_not_visible",
];

/// Of those, the ones after which the screen may be a new one.
const SCREEN_CHANGING: &[&str] = &["tap", "press_key", "swipe", "scroll", "launch_app"];

/// One recorded action.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub tool: String,
    pub args: Value,
    /// The `assertVisible` decided for the screen it led to.
    pub assertion: Option<Selector>,
    before: Option<Arc<Element>>,
    /// Its after-screen was seen (and the assertion decided).
    observed: bool,
}

/// A [`Recorder`] as saved between `velocity` agent commands.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RecorderState {
    steps: Vec<RecordedState>,
    platform: Option<Platform>,
    last_tree: Option<Element>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordedState {
    tool: String,
    args: Value,
    assertion: Option<Selector>,
    before: Option<Element>,
    observed: bool,
}

impl Recorder {
    pub fn to_state(&self) -> RecorderState {
        RecorderState {
            steps: self
                .steps
                .iter()
                .map(|r| RecordedState {
                    tool: r.tool.clone(),
                    args: r.args.clone(),
                    assertion: r.assertion.clone(),
                    before: r.before.as_deref().cloned(),
                    observed: r.observed,
                })
                .collect(),
            platform: self.platform,
            last_tree: self.last_tree.as_deref().cloned(),
        }
    }

    pub fn from_state(state: RecorderState) -> Self {
        Self {
            steps: state
                .steps
                .into_iter()
                .map(|r| Recorded {
                    tool: r.tool,
                    args: r.args,
                    assertion: r.assertion,
                    before: r.before.map(Arc::new),
                    observed: r.observed,
                })
                .collect(),
            platform: state.platform,
            last_tree: state.last_tree.map(Arc::new),
        }
    }
}

/// The session's recording.
#[derive(Debug, Default)]
pub struct Recorder {
    pub steps: Vec<Recorded>,
    pub platform: Option<Platform>,
    last_tree: Option<Arc<Element>>,
}

impl Recorder {
    /// The last action's resulting screen hasn't been seen yet, or no
    /// screen has been: the server should read the tree and [`observe`] it.
    ///
    /// [`observe`]: Self::observe
    pub fn wants_tree(&self) -> bool {
        self.last_tree.is_none() || self.pending().is_some()
    }

    fn pending(&self) -> Option<usize> {
        self.steps
            .iter()
            .rposition(|r| !r.observed)
            .filter(|i| *i + 1 == self.steps.len())
    }

    /// The screen now: it is what the last action led to, and what the
    /// next one starts from.
    pub fn observe(&mut self, tree: &Element, screen: Option<&Rect>) {
        let platform = self.platform.unwrap_or(Platform::Android);
        let typed = self.typed_texts();
        if let Some(i) = self.pending() {
            let r = &mut self.steps[i];
            r.observed = true;
            if SCREEN_CHANGING.contains(&r.tool.as_str()) {
                r.assertion = pick_assertion(r.before.as_deref(), tree, screen, platform, &typed);
            }
        }
        self.last_tree = Some(Arc::new(tree.clone()));
    }

    /// A successful action, with its resolved arguments.
    pub fn record(&mut self, tool: &str, args: &Value) {
        // An action's own screen is settled once the next one starts.
        if let Some(i) = self.pending() {
            self.steps[i].observed = true;
        }
        self.steps.push(Recorded {
            tool: tool.to_string(),
            args: args.clone(),
            assertion: None,
            before: self.last_tree.clone(),
            observed: false,
        });
    }

    pub fn clear(&mut self) {
        self.steps.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The app the recording launched first, if it did.
    pub fn launched_app(&self) -> Option<&str> {
        self.steps
            .iter()
            .find(|r| r.tool == "launch_app")
            .and_then(|r| r.args.get("app_id"))
            .and_then(Value::as_str)
    }

    /// The last screen seen.
    pub fn last_tree(&self) -> Option<&Element> {
        self.last_tree.as_deref()
    }

    fn typed_texts(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|r| r.tool == "type_text")
            .filter_map(|r| r.args.get("text").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// The steps as YAML values, and how many of them came from actions.
    pub fn steps_yaml(&self) -> (Vec<Yaml>, usize) {
        let mut out = Vec::new();
        let mut actions = 0;
        for r in &self.steps {
            let Some(step) = step_yaml(&r.tool, &r.args) else {
                continue;
            };
            actions += 1;
            out.push(step);
            if let Some(a) = &r.assertion {
                out.push(one("assertVisible", Yaml::Mapping(selector_yaml(a))));
            }
        }
        (out, actions)
    }

    /// The recording as a runnable suite: `(yaml, actions recorded)`.
    pub fn suite_yaml(&self, app_id: &str, name: &str) -> (String, usize) {
        let (mut steps, n) = self.steps_yaml();
        let launches_first = self.steps.first().is_some_and(|r| r.tool == "launch_app");
        if !launches_first {
            steps.insert(0, one("launchApp", Yaml::Mapping(Mapping::new())));
        }
        let yaml = suite(app_id, name, &["recorded"], steps);
        (
            format!(
                "# Recorded from an MCP session by export_session_flow. Each assertVisible \
                 checks the screen an action led to; review them.\n{yaml}"
            ),
            n,
        )
    }
}

/// `{ key: value }`.
pub fn one(key: &str, value: Yaml) -> Yaml {
    let mut m = Mapping::new();
    m.insert(Yaml::String(key.to_string()), value);
    Yaml::Mapping(m)
}

/// A suite with one test.
pub fn suite(app_id: &str, name: &str, tags: &[&str], steps: Vec<Yaml>) -> String {
    let mut test = Mapping::new();
    test.insert("name".into(), name.into());
    test.insert(
        "tags".into(),
        Yaml::Sequence(tags.iter().map(|t| Yaml::from(*t)).collect()),
    );
    test.insert("steps".into(), Yaml::Sequence(steps));
    let mut root = Mapping::new();
    root.insert("appId".into(), app_id.into());
    root.insert("tests".into(), Yaml::Sequence(vec![Yaml::Mapping(test)]));
    serde_yaml::to_string(&Yaml::Mapping(root)).unwrap_or_default()
}

/// A selector as a suite writes it (`{id: ok}`, `{text: Buy, index: 1}`).
/// Its strings are escaped for interpolation: a suite fills `${NAME}` in
/// every string when it loads, so an app label `Pay ${amount}` is written
/// `Pay $${amount}`, still matches that label, and reads no variable.
pub fn selector_yaml(s: &Selector) -> Mapping {
    let mut m = Mapping::new();
    fill(&mut m, s, true);
    m
}

/// How a string value is written: escaped in a suite, as is in a tool's
/// arguments (which nothing interpolates).
fn value(v: &str, escape: bool) -> Yaml {
    if escape {
        velocity_core::env::escape(v).into()
    } else {
        v.into()
    }
}

fn fill(m: &mut Mapping, s: &Selector, esc: bool) {
    let mut put = |k: &str, v: Yaml| {
        m.insert(Yaml::String(k.to_string()), v);
    };
    match s {
        Selector::Id(v) => put("id", value(v, esc)),
        Selector::Text(v) => put("text", value(v, esc)),
        Selector::TextContains(v) => put("textContains", value(v, esc)),
        Selector::TextMatches(v) => put("textMatches", value(v, esc)),
        Selector::IdMatches(v) => put("idMatches", value(v, esc)),
        Selector::AccessibilityId(v) => put("accessibilityId", value(v, esc)),
        Selector::ClassName(v) => put("className", value(v, esc)),
        Selector::Css(v) => put("css", value(v, esc)),
        Selector::XPath(v) => put("xpath", value(v, esc)),
        Selector::Role(v) => put("role", value(v, esc)),
        Selector::Name(v) => put("name", value(v, esc)),
        Selector::Checked(b) => put("checked", (*b).into()),
        Selector::Focused(b) => put("focused", (*b).into()),
        Selector::Selected(b) => put("selected", (*b).into()),
        Selector::Enabled(b) => put("enabled", (*b).into()),
        Selector::Index { selector, index } => {
            fill(m, selector, esc);
            m.insert("index".into(), Yaml::from(*index as u64));
        }
        Selector::Compound(parts) => {
            for p in parts {
                fill(m, p, esc);
            }
        }
        Selector::Relative {
            target,
            relation,
            anchor,
        } => {
            fill(m, target, esc);
            m.insert(
                Yaml::String(relation.to_string()),
                Yaml::Mapping({
                    let mut a = Mapping::new();
                    fill(&mut a, anchor, esc);
                    a
                }),
            );
        }
    }
}

/// The tool call's selector, parsed.
fn selector_of(args: &Value, key: &str) -> Option<Selector> {
    velocity_core::parser::selector_from_value(args.get(key)?.clone()).ok()
}

/// A selector in the flat form tools take, as is: what a ref resolves to,
/// and what a tool call parses back to the same selector. Suites get
/// [`selector_yaml`], which escapes `${`.
pub fn selector_json(s: &Selector) -> Value {
    let mut m = Mapping::new();
    fill(&mut m, s, false);
    serde_json::to_value(Yaml::Mapping(m)).unwrap_or(Value::Null)
}

/// A recorded tool call as a step, or `None` when it can't be one.
pub fn step_yaml(tool: &str, args: &Value) -> Option<Yaml> {
    let sel = || selector_of(args, "selector").map(|s| Yaml::Mapping(selector_yaml(&s)));
    let s = |k: &str| args.get(k).and_then(Value::as_str);
    Some(match tool {
        "tap" => one("tap", sel()?),
        "assert_visible" => one("assertVisible", sel()?),
        "assert_not_visible" => one("assertNotVisible", sel()?),
        "type_text" => {
            // A secret is exported by name: the flow reads it from the
            // environment when it runs, and the value is never written.
            let text = match s("secret") {
                Some(name) => format!("${{{name}}}"),
                // Typed as written: a literal `${HOME}` must not read the
                // environment when the flow runs.
                None => velocity_core::env::escape(s("text")?),
            };
            let mut m = Mapping::new();
            m.insert("selector".into(), sel()?);
            m.insert("text".into(), text.into());
            one("inputText", Yaml::Mapping(m))
        }
        "swipe" => {
            let d = crate::tools::interaction::normalize_direction(s("direction")?);
            one("swipe", one("direction", d.into()))
        }
        "press_key" => {
            let k = crate::tools::interaction::normalize_key(s("key")?);
            one("pressKey", one("key", k.into()))
        }
        "scroll" => {
            let d =
                crate::tools::interaction::normalize_direction(s("direction").unwrap_or("down"));
            match selector_of(args, "until_visible") {
                Some(target) => {
                    let mut m = Mapping::new();
                    m.insert("selector".into(), Yaml::Mapping(selector_yaml(&target)));
                    m.insert("direction".into(), d.into());
                    if let Some(n) = args.get("max_scrolls").and_then(Value::as_u64) {
                        m.insert("maxScrolls".into(), n.into());
                    }
                    one("scrollUntilVisible", Yaml::Mapping(m))
                }
                // Content down is a finger up.
                None => one(
                    "swipe",
                    one(
                        "direction",
                        crate::tools::interaction::finger_for(&d).into(),
                    ),
                ),
            }
        }
        "launch_app" | "stop_app" => {
            let mut m = Mapping::new();
            if let Some(a) = s("app_id") {
                m.insert("appId".into(), a.into());
            }
            if tool == "launch_app"
                && args.get("clear_state").and_then(Value::as_bool) == Some(true)
            {
                m.insert("clearState".into(), true.into());
            }
            one(
                if tool == "launch_app" {
                    "launchApp"
                } else {
                    "stopApp"
                },
                Yaml::Mapping(m),
            )
        }
        _ => return None,
    })
}

/// Text that changes between runs: times, dates, counts, prices.
pub fn volatile(t: &str) -> bool {
    let t = t.trim();
    let chars: Vec<char> = t.chars().collect();
    if chars.len() < 2 || chars.len() > 60 {
        return true;
    }
    // Any number: a count, a time, a date, a price, a version. A label
    // with a digit is rarely the one stable thing on a screen.
    chars.iter().any(|c| c.is_ascii_digit())
}

/// System ids every screen has: they say nothing about which screen it is.
fn generic_id(id: &str) -> bool {
    id.starts_with("android:id/")
        || id.contains("systemui")
        || id.starts_with("com.android.launcher")
        // UIKit's navigation bar back button: every pushed screen has one.
        || id == "BackButton"
}

/// Whether `text` on `e` is its children's text joined (a web `<body>`, an
/// iOS cell): a replay matches that element by its own text, not by what
/// its descendants say, so it makes a brittle assertion.
fn aggregates(e: &Element, text: &str) -> bool {
    fn any_inside(e: &Element, text: &str) -> bool {
        e.children.iter().any(|c| {
            [c.text.as_deref(), c.label.as_deref()]
                .into_iter()
                .flatten()
                .any(|t| !t.trim().is_empty() && t.trim() != text.trim() && text.contains(t.trim()))
                || any_inside(c, text)
        })
    }
    any_inside(e, text)
}

/// The `assertVisible` for `after`: an element `before` didn't have, by a
/// selector that finds only it there and nothing on `before`.
pub fn pick_assertion(
    before: Option<&Element>,
    after: &Element,
    screen: Option<&Rect>,
    platform: Platform,
    typed: &[String],
) -> Option<Selector> {
    fn walk<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
        out.push(e);
        for c in &e.children {
            walk(c, out);
        }
    }
    let mut nodes = Vec::new();
    walk(after, &mut nodes);
    let sig = |e: &Element| (e.platform_id.clone(), e.text.clone(), e.label.clone());
    let old: HashSet<_> = before
        .map(|b| {
            let mut v = Vec::new();
            walk(b, &mut v);
            v.into_iter().map(sig).collect()
        })
        .unwrap_or_default();
    let usable_text = |t: &str| {
        !volatile(t)
            && !typed
                .iter()
                .any(|x| !x.is_empty() && (t.contains(x.as_str()) || x.contains(t)))
            && velocity_common::redact::redact(t) == t
    };
    let mut best: Option<(u8, Selector)> = None;
    for e in nodes {
        if !crate::snapshot::on_screen(e, screen) || old.contains(&sig(e)) {
            continue;
        }
        // A bare layout container proves little about the screen.
        if !crate::snapshot::meaningful(e) {
            continue;
        }
        let selector = robust_selector(e, after, screen, platform);
        let rank = match &selector {
            Selector::Id(id) if !generic_id(&e.platform_id) && !generic_id(id) => 0,
            Selector::AccessibilityId(l) if usable_text(l) && !aggregates(e, l) => 1,
            Selector::Text(t) if usable_text(t) && !aggregates(e, t) => 2,
            _ => continue,
        };
        if best.as_ref().is_some_and(|(r, _)| *r <= rank) {
            continue;
        }
        let only_it = find_all_ref(after, &selector, screen);
        if only_it.len() != 1 || !std::ptr::eq(only_it[0], e) {
            continue;
        }
        if before.is_some_and(|b| !find_all_ref(b, &selector, None).is_empty()) {
            continue;
        }
        best = Some((rank, selector));
    }
    best.map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: &str, text: &str, y: i32) -> Element {
        Element {
            platform_id: id.into(),
            text: (!text.is_empty()).then(|| text.into()),
            element_type: "android.widget.TextView".into(),
            bounds: Rect {
                x: 0,
                y,
                width: 300,
                height: 40,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        }
    }

    fn tree(kids: Vec<Element>) -> Element {
        Element {
            children: kids,
            element_type: "FrameLayout".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            visible: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_container_whose_text_joins_its_children_is_not_asserted() {
        let mut body = el("", "Sign in Email Continue", 0);
        body.children = vec![el("", "Sign in", 10), el("", "Continue", 60)];
        let after = tree(vec![body]);
        let picked = pick_assertion(None, &after, None, Platform::Web, &[]);
        assert_eq!(picked, Some(Selector::Text("Sign in".into())), "{picked:?}");
    }

    #[test]
    fn volatile_text_is_recognised() {
        for t in [
            "12:45",
            "3 unread",
            "$9.99",
            "1,234",
            "5%",
            "x",
            "2026/10/25",
        ] {
            assert!(volatile(t), "{t}");
        }
        for t in ["Network & internet", "Sign in", "Welcome back"] {
            assert!(!volatile(t), "{t}");
        }
    }

    #[test]
    fn the_assertion_prefers_a_new_id_then_a_label_then_text() {
        let before = tree(vec![
            el("com.app:id/title", "Home", 0),
            el("", "Sign in", 100),
        ]);
        // The title id exists on both screens: it can't tell them apart.
        let after = tree(vec![
            el("com.app:id/title", "Profile", 0),
            el("", "12:45", 50),
            el("", "Edit profile", 100),
            Element {
                label: Some("Avatar".into()),
                ..el("com.app:id/avatar", "", 200)
            },
            el("com.app:id/bare_container", "", 300),
        ]);
        let a = pick_assertion(Some(&before), &after, None, Platform::Android, &[]);
        assert_eq!(a, Some(Selector::Id("com.app:id/avatar".into())));
        let no_id = tree(vec![
            el("com.app:id/title", "Profile", 0),
            el("", "12:45", 50),
            el("", "Edit profile", 100),
        ]);
        let a = pick_assertion(Some(&before), &no_id, None, Platform::Android, &[]);
        assert_eq!(a, Some(Selector::Text("Edit profile".into())));
        // Nothing new: no assertion.
        assert_eq!(
            pick_assertion(Some(&before), &before, None, Platform::Android, &[]),
            None
        );
        // Typed text is not an assertion.
        let typed = tree(vec![el("", "qa@velocity.dev", 0)]);
        assert_eq!(
            pick_assertion(
                Some(&tree(vec![])),
                &typed,
                None,
                Platform::Android,
                &["qa@velocity.dev".into()]
            ),
            None
        );
    }

    #[test]
    fn literal_typed_text_is_exported_so_it_reads_no_variable() {
        let y = step_yaml(
            "type_text",
            &serde_json::json!({ "selector": { "id": "q" }, "text": "${AWS_SECRET_ACCESS_KEY}" }),
        )
        .unwrap();
        let text = y["inputText"]["text"].as_str().unwrap().to_string();
        assert_eq!(text, "$${AWS_SECRET_ACCESS_KEY}");
        assert_eq!(
            velocity_core::env::interpolate(&text, &Default::default()).unwrap(),
            "${AWS_SECRET_ACCESS_KEY}"
        );
    }

    /// A recorded selector holding `${` is escaped in the suite (which
    /// interpolates every selector string when it loads), so the loaded
    /// selector is the recorded one and reads no variable; a ref's selector
    /// (`selector_json`) stays as is, so a tool call parses it back
    /// unchanged.
    #[test]
    fn a_recorded_selector_with_a_placeholder_is_exported_literally() {
        let raw = "Pay ${AWS_SECRET_ACCESS_KEY}";
        let sel = Selector::Text(raw.into());
        let y = step_yaml("tap", &serde_json::json!({ "selector": { "text": raw } })).unwrap();
        let written = y["tap"]["text"].as_str().unwrap().to_string();
        assert_eq!(written, "Pay $${AWS_SECRET_ACCESS_KEY}");
        let yaml = suite("app", "t", &[], vec![y]);
        let mut parsed = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
        velocity_core::env::interpolate_suite(&mut parsed, &Default::default()).unwrap();
        let back = format!("{:?}", parsed.tests[0].steps[0]);
        assert!(back.contains(&format!("{raw:?}")), "{back}");
        // The ref form round-trips through a tool call untouched.
        let json = selector_json(&sel);
        assert_eq!(json["text"], raw);
        assert_eq!(
            velocity_core::parser::selector_from_value(json).unwrap(),
            sel
        );
    }

    #[test]
    fn a_recording_exports_a_suite_with_assertions_that_parses() {
        let mut r = Recorder::default();
        let home = tree(vec![el("", "Network & internet", 100)]);
        r.observe(&home, None);
        r.record(
            "tap",
            &serde_json::json!({ "selector": { "text": "Network & internet" } }),
        );
        assert!(r.wants_tree());
        let net = tree(vec![el("", "Internet", 100), el("", "SIMs", 200)]);
        r.observe(&net, None);
        assert!(!r.wants_tree());
        r.record(
            "type_text",
            &serde_json::json!({ "selector": { "id": "pw" }, "secret": "LOGIN_PW" }),
        );
        r.record("press_key", &serde_json::json!({ "key": "Return" }));
        r.observe(&home, None);
        r.record(
            "scroll",
            &serde_json::json!({ "direction": "down", "until_visible": { "text": "About" } }),
        );
        r.record("tap", &serde_json::json!({ "bogus": 1 }));
        let (yaml, n) = r.suite_yaml("com.android.settings", "net: \"quoted\"\n- injected");
        assert_eq!(n, 4, "{yaml}");
        let suite = velocity_core::parser::parse_suite_from_str(&yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        velocity_core::validate_suite(&suite).unwrap();
        assert_eq!(suite.tests.len(), 1);
        assert_eq!(suite.tests[0].name, "net: \"quoted\"\n- injected");
        // launchApp, tap, assertVisible Internet, inputText, pressKey,
        // assertVisible (home again), scrollUntilVisible.
        assert_eq!(suite.tests[0].steps.len(), 7, "{yaml}");
        assert!(yaml.contains("text: Internet"), "{yaml}");
        assert!(yaml.contains("${LOGIN_PW}"), "{yaml}");
    }

    #[test]
    fn every_selector_shape_round_trips_through_the_parser() {
        use velocity_common::Relation;
        let shapes = vec![
            Selector::Id("a".into()),
            Selector::Index {
                selector: Box::new(Selector::Text("Buy: now".into())),
                index: 2,
            },
            Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("Pay".into()),
            ]),
            Selector::Relative {
                target: Box::new(Selector::Text("Edit".into())),
                relation: Relation::Below,
                anchor: Box::new(Selector::Id("title".into())),
            },
            Selector::AccessibilityId("Close #1 'x'".into()),
        ];
        for s in shapes {
            let v = serde_yaml::to_value(selector_yaml(&s)).unwrap();
            let json: Value = serde_json::to_value(&v).unwrap();
            let back = velocity_core::parser::selector_from_value(json.clone())
                .unwrap_or_else(|e| panic!("{e}: {json}"));
            assert_eq!(back, s, "{json}");
        }
    }
}
