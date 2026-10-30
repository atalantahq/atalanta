//! `snapshot`: the screen as few tokens as an agent can act on.
//!
//! One line per element worth seeing (visible, on screen, with text, a
//! label, a role, an id or a checked state), each with a ref `@e<n>`:
//!
//! ```text
//! # 1080x2400, 9 elements
//! @e1 heading "Settings"
//! @e2 button "Network & internet" #network [disabled]
//! @e5 switch "Wi-Fi" [checked]
//! ```
//!
//! Refs are stable for the session: an element keeps its ref across
//! snapshots while its identity (kind, id, text, label, and which of its
//! twins it is) stays the same, so `mode: "delta"` can say what was added,
//! removed or changed since the last snapshot in a few lines.
//!
//! A ref is only ever a name for an element of the last snapshot: `tap`
//! and friends turn it into a selector built here, in code, from the live
//! tree ([`Snapshots::resolve`]), and refuse it when that element is gone or
//! (for a twin) has moved, asking for a new snapshot. An exported flow
//! records the selector, never the ref.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use velocity_common::selector_build::{robust_selector, synthetic_id};
use velocity_common::{Element, Platform, Rect, Result, Selector, VelocityError};

/// Longest text kept on a line; the rest is cut with `…`.
const MAX_TEXT: usize = 80;

/// What makes an element the same element across snapshots: its kind and
/// id, or, without an id, its kind, text and label; then which of its
/// twins it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Identity {
    kind: String,
    id: String,
    text: String,
    label: String,
    /// Which of its twins (same kind, id, text and label), in tree order.
    nth: usize,
}

/// An element as the last snapshot showed it.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    identity: Identity,
    /// It had twins: it must also be where it was to be the same one.
    twin: bool,
    bounds: Rect,
    line: String,
    json: Value,
}

/// The session's snapshot state.
#[derive(Debug, Default)]
pub struct Snapshots {
    pub platform: Option<Platform>,
    next: u64,
    refs: HashMap<Identity, u64>,
    /// The last snapshot, by ref number.
    last: Option<BTreeMap<u64, Shown>>,
}

/// How to render.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Options {
    pub delta: bool,
    pub json: bool,
    pub bounds: bool,
}

impl Options {
    pub fn from_args(args: &Value) -> Result<Self> {
        let pick = |key: &str, allowed: &[&str], default: &str| -> Result<String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(default.to_string()),
                Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(s.clone()),
                Some(v) => Err(VelocityError::Config(format!(
                    "'{key}' must be one of {}, got {v}",
                    allowed.join(", ")
                ))),
            }
        };
        Ok(Self {
            delta: pick("mode", &["full", "delta"], "full")? == "delta",
            json: pick("format", &["text", "json"], "text")? == "json",
            bounds: args.get("bounds").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// `android.widget.Button` → `Button`.
fn short_kind(t: &str) -> &str {
    t.rsplit(['.', ':']).next().unwrap_or(t)
}

/// The id an app set (its short form), not a driver-made one.
fn app_id(e: &Element) -> Option<&str> {
    let id = e.platform_id.as_str();
    if id.is_empty() || synthetic_id(id) {
        return None;
    }
    Some(id.rsplit_once(":id/").map_or(id, |(_, s)| s))
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Whether `e` is drawn, has an area, and lies on screen.
pub(crate) fn on_screen(e: &Element, screen: Option<&Rect>) -> bool {
    let b = &e.bounds;
    e.visible
        && b.width > 0
        && b.height > 0
        && screen.is_none_or(|s| {
            b.x < s.x + s.width
                && b.x + b.width > s.x
                && b.y < s.y + s.height
                && b.y + b.height > s.y
        })
}

/// Roles an agent acts on even when they carry no text.
const ACTIONABLE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "checkbox",
    "radio",
    "switch",
    "slider",
    "tab",
    "menuitem",
    "option",
    "spinbutton",
];

/// Whether `e` says something or can be acted on: text, a label, a hint,
/// a checked state or an actionable role. Bare layout containers (an id
/// and nothing else) don't.
pub(crate) fn meaningful(e: &Element) -> bool {
    nonempty(e.text.as_deref()).is_some()
        || nonempty(e.label.as_deref()).is_some()
        || nonempty(e.hint.as_deref()).is_some()
        || e.checked.is_some()
        || velocity_common::role::role_of(e).is_some_and(|r| ACTIONABLE_ROLES.contains(&r))
}

/// Whether a snapshot shows `e`.
fn shown(e: &Element, screen: Option<&Rect>) -> bool {
    on_screen(e, screen) && meaningful(e)
}

/// Every shown element with its identity, in tree order.
fn collect<'a>(tree: &'a Element, screen: Option<&Rect>) -> Vec<(Identity, bool, &'a Element)> {
    fn walk<'a>(e: &'a Element, screen: Option<&Rect>, out: &mut Vec<&'a Element>) {
        if shown(e, screen) {
            out.push(e);
        }
        for c in &e.children {
            walk(c, screen, out);
        }
    }
    let mut nodes = Vec::new();
    walk(tree, screen, &mut nodes);
    // An element with an id is that id (its text is its content: a field's
    // value, a counter); one without is its text and label.
    let base = |e: &Element| {
        let id = app_id(e).unwrap_or("").to_string();
        let keyed = !id.is_empty();
        let content = |v: Option<&str>| {
            if keyed {
                String::new()
            } else {
                nonempty(v).unwrap_or("").to_string()
            }
        };
        Identity {
            kind: short_kind(&e.element_type).to_string(),
            text: content(e.text.as_deref()),
            label: content(e.label.as_deref()),
            id,
            nth: 0,
        }
    };
    let mut totals: HashMap<Identity, usize> = HashMap::new();
    for e in &nodes {
        *totals.entry(base(e)).or_default() += 1;
    }
    let mut seen: HashMap<Identity, usize> = HashMap::new();
    nodes
        .into_iter()
        .map(|e| {
            let b = base(e);
            let twin = totals[&b] > 1;
            let n = seen.entry(b.clone()).or_default();
            let identity = Identity { nth: *n, ..b };
            *n += 1;
            (identity, twin, e)
        })
        .collect()
}

fn clip(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_TEXT {
        return one_line;
    }
    let mut out: String = one_line.chars().take(MAX_TEXT - 1).collect();
    out.push('…');
    out
}

/// `@e3 button "Pay" label="Pay now" #pay [checked,disabled] @(0,10 100x40)`.
fn render(r: u64, e: &Element, bounds: bool) -> (String, Value) {
    let role = velocity_common::role::role_of(e)
        .map(String::from)
        .unwrap_or_else(|| short_kind(&e.element_type).to_string());
    let mut line = format!("@e{r} {role}");
    let text = nonempty(e.text.as_deref()).map(clip);
    let label = nonempty(e.label.as_deref())
        .map(clip)
        .filter(|l| text.as_ref() != Some(l));
    if let Some(t) = &text {
        line.push_str(&format!(" {t:?}"));
    }
    if let Some(l) = &label {
        line.push_str(&format!(" label={l:?}"));
    }
    let hint = nonempty(e.hint.as_deref())
        .filter(|_| text.is_none())
        .map(clip);
    if let Some(h) = &hint {
        line.push_str(&format!(" hint={h:?}"));
    }
    let id = app_id(e);
    if let Some(id) = id {
        line.push_str(&format!(" #{id}"));
    }
    let mut state = Vec::new();
    match e.checked {
        Some(true) => state.push("checked"),
        Some(false) => state.push("unchecked"),
        None => {}
    }
    if !e.enabled {
        state.push("disabled");
    }
    if e.focused == Some(true) {
        state.push("focused");
    }
    if e.selected == Some(true) {
        state.push("selected");
    }
    if !state.is_empty() {
        line.push_str(&format!(" [{}]", state.join(",")));
    }
    let b = e.bounds;
    if bounds {
        line.push_str(&format!(" @({},{} {}x{})", b.x, b.y, b.width, b.height));
    }
    let mut json = serde_json::json!({ "ref": format!("@e{r}"), "role": role });
    if let Some(t) = text {
        json["text"] = t.into();
    }
    if let Some(l) = label {
        json["label"] = l.into();
    }
    if let Some(h) = hint {
        json["hint"] = h.into();
    }
    if let Some(id) = id {
        json["id"] = id.into();
    }
    if !state.is_empty() {
        json["state"] = state.into();
    }
    if bounds {
        json["bounds"] = serde_json::json!([b.x, b.y, b.width, b.height]);
    }
    (line, json)
}

impl Snapshots {
    /// Snapshot `tree`; with `delta`, only what changed since the last one.
    pub fn take(&mut self, tree: &Element, screen: Option<&Rect>, opts: Options) -> Value {
        let mut now: BTreeMap<u64, Shown> = BTreeMap::new();
        for (identity, twin, e) in collect(tree, screen) {
            let r = match self.refs.get(&identity) {
                Some(r) => *r,
                None => {
                    self.next += 1;
                    self.refs.insert(identity.clone(), self.next);
                    self.next
                }
            };
            let (line, json) = render(r, e, opts.bounds);
            now.insert(
                r,
                Shown {
                    identity,
                    twin,
                    bounds: e.bounds,
                    line,
                    json,
                },
            );
        }
        let size = screen.map(|s| format!("{}x{}, ", s.width, s.height));
        let previous = self.last.replace(now);
        let now = self.last.as_ref().expect("just set");
        let header = format!("# {}{} elements", size.unwrap_or_default(), now.len());
        match previous.filter(|_| opts.delta) {
            None => {
                let note = opts
                    .delta
                    .then_some("no earlier snapshot in this session: full snapshot");
                if opts.json {
                    let mut v = serde_json::json!({
                        "mode": "full",
                        "count": now.len(),
                        "elements": now.values().map(|s| s.json.clone()).collect::<Vec<_>>(),
                    });
                    if let Some(n) = note {
                        v["note"] = n.into();
                    }
                    v
                } else {
                    let mut lines = vec![header];
                    if let Some(n) = note {
                        lines.push(format!("# {n}"));
                    }
                    lines.extend(now.values().map(|s| s.line.clone()));
                    Value::String(lines.join("\n"))
                }
            }
            Some(before) => {
                let added: Vec<&Shown> = now
                    .iter()
                    .filter(|(r, _)| !before.contains_key(r))
                    .map(|(_, s)| s)
                    .collect();
                let removed: Vec<(&u64, &Shown)> = before
                    .iter()
                    .filter(|(r, _)| !now.contains_key(r))
                    .collect();
                let changed: Vec<&Shown> = now
                    .iter()
                    .filter(|(r, s)| before.get(r).is_some_and(|b| b.line != s.line))
                    .map(|(_, s)| s)
                    .collect();
                let same = now.len() - added.len() - changed.len();
                if opts.json {
                    serde_json::json!({
                        "mode": "delta",
                        "count": now.len(),
                        "unchanged": same,
                        "added": added.iter().map(|s| s.json.clone()).collect::<Vec<_>>(),
                        "removed": removed.iter().map(|(r, _)| format!("@e{r}")).collect::<Vec<_>>(),
                        "changed": changed.iter().map(|s| s.json.clone()).collect::<Vec<_>>(),
                    })
                } else if added.is_empty() && removed.is_empty() && changed.is_empty() {
                    Value::String(format!("{header}; no change since the last snapshot"))
                } else {
                    let mut lines = vec![format!(
                        "{header}; delta: +{} -{} ~{} ({same} unchanged)",
                        added.len(),
                        removed.len(),
                        changed.len()
                    )];
                    lines.extend(added.iter().map(|s| format!("+ {}", s.line)));
                    lines.extend(removed.iter().map(|(_, s)| format!("- {}", s.line)));
                    lines.extend(changed.iter().map(|s| format!("~ {}", s.line)));
                    Value::String(lines.join("\n"))
                }
            }
        }
    }

    /// The selector `@eN` stands for on `tree` now: built in code for the
    /// element the last snapshot showed under that ref, or an error asking
    /// for a new snapshot when it is gone, or a twin of it has moved in.
    pub fn resolve(&self, r: &str, tree: &Element, screen: Option<&Rect>) -> Result<Selector> {
        let n: u64 = r
            .trim()
            .strip_prefix("@e")
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                VelocityError::Config(format!("'{r}' is not a snapshot ref (they look like @e12)"))
            })?;
        let Some(last) = &self.last else {
            return Err(VelocityError::Config(format!(
                "ref {r}: no snapshot taken yet in this session; call snapshot first"
            )));
        };
        let stale = |why: &str| {
            VelocityError::Config(format!(
                "ref {r} is stale: {why}; call snapshot (mode \"delta\" is cheap) and use a current ref"
            ))
        };
        let Some(seen) = last.get(&n) else {
            return Err(stale("it is not on the last snapshot"));
        };
        let node = collect(tree, screen)
            .into_iter()
            .find(|(id, _, _)| *id == seen.identity)
            .map(|(_, _, e)| e)
            .ok_or_else(|| stale("that element is no longer on screen"))?;
        if seen.twin && node.bounds != seen.bounds {
            return Err(stale(
                "it has identical twins and the screen moved, so it may be another one now",
            ));
        }
        let platform = self.platform.unwrap_or(Platform::Android);
        let selector = robust_selector(node, tree, screen, platform);
        let hit = crate::refs::pick(tree, &selector, screen);
        if !hit.is_some_and(|h| std::ptr::eq(h, node)) {
            return Err(stale("no selector finds exactly that element"));
        }
        Ok(selector)
    }
}

/// Whether `v` is a snapshot ref (`@e12`).
pub fn is_snapshot_ref(v: &str) -> bool {
    v.trim()
        .strip_prefix("@e")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(kind: &str, id: &str, text: &str, y: i32) -> Element {
        Element {
            platform_id: id.into(),
            text: (!text.is_empty()).then(|| text.into()),
            element_type: kind.into(),
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
            element_type: "android.widget.FrameLayout".into(),
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

    fn screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        }
    }

    fn text(v: Value) -> String {
        v.as_str().unwrap().to_string()
    }

    #[test]
    fn a_full_snapshot_is_one_line_per_element() {
        let mut s = Snapshots::default();
        let mut wifi = el(
            "android.widget.Switch",
            "com.android.settings:id/switch",
            "Wi-Fi",
            100,
        );
        wifi.checked = Some(true);
        let mut off = el("android.widget.Button", "", "Pay", 200);
        off.enabled = false;
        let t = tree(vec![
            el("android.widget.TextView", "", "Settings", 0),
            wifi,
            off,
            el("android.view.View", "", "", 300), // nothing to show
            el("android.widget.FrameLayout", "com.app:id/content", "", 350), // a bare container
        ]);
        let out = text(s.take(&t, Some(&screen()), Options::default()));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "# 1080x2400, 3 elements", "{out}");
        assert_eq!(lines[1], "@e1 TextView \"Settings\"");
        assert!(
            lines[2].starts_with("@e2 ") && lines[2].ends_with("\"Wi-Fi\" #switch [checked]"),
            "{out}"
        );
        assert!(lines[3].ends_with("\"Pay\" [disabled]"), "{out}");
    }

    #[test]
    fn delta_reports_added_removed_and_changed_with_stable_refs() {
        let mut s = Snapshots::default();
        let one = tree(vec![
            el("TextView", "", "Inbox", 0),
            el("Button", "compose", "Compose", 100),
            el("TextView", "", "3 unread", 200),
        ]);
        s.take(&one, None, Options::default());
        let mut compose = el("Button", "compose", "Compose", 100);
        compose.focused = Some(true);
        let two = tree(vec![
            el("TextView", "", "Inbox", 0),
            compose,
            el("TextView", "", "Sent", 300),
        ]);
        let delta = Options {
            delta: true,
            ..Default::default()
        };
        let out = text(s.take(&two, None, delta));
        assert!(out.contains("delta: +1 -1 ~1 (1 unchanged)"), "{out}");
        assert!(out.contains("+ @e4 TextView \"Sent\""), "{out}");
        assert!(out.contains("- @e3 TextView \"3 unread\""), "{out}");
        assert!(
            out.contains("~ @e2 button \"Compose\" #compose [focused]"),
            "{out}"
        );
        let again = text(s.take(&two, None, delta));
        assert!(
            again.ends_with("no change since the last snapshot"),
            "{again}"
        );
        let json = s.take(
            &two,
            None,
            Options {
                delta: true,
                json: true,
                ..Default::default()
            },
        );
        assert_eq!(json["unchanged"], 3);
    }

    #[test]
    fn refs_resolve_in_code_and_go_stale() {
        let mut s = Snapshots::default();
        let t = tree(vec![
            el("Button", "ok", "OK", 0),
            el("Button", "", "Buy", 100),
            el("Button", "", "Buy", 200),
        ]);
        s.take(&t, None, Options::default());
        assert_eq!(
            s.resolve("@e1", &t, None).unwrap(),
            Selector::Id("ok".into())
        );
        let second = s.resolve("@e3", &t, None).unwrap();
        assert!(
            matches!(second, Selector::Index { index: 1, .. }),
            "{second:?}"
        );
        // A unique element may move (a scroll) and keep its ref.
        let moved = tree(vec![el("Button", "ok", "OK", 50)]);
        assert!(s.resolve("@e1", &moved, None).is_ok());
        // A twin that moved could be another row now.
        let scrolled = tree(vec![
            el("Button", "ok", "OK", 0),
            el("Button", "", "Buy", 50),
            el("Button", "", "Buy", 150),
        ]);
        let e = s.resolve("@e3", &scrolled, None).unwrap_err();
        assert!(e.to_string().contains("twins"), "{e}");
        // Gone, unknown, malformed.
        let gone = tree(vec![]);
        assert!(s
            .resolve("@e1", &gone, None)
            .unwrap_err()
            .to_string()
            .contains("no longer on screen"));
        assert!(s
            .resolve("@e99", &t, None)
            .unwrap_err()
            .to_string()
            .contains("not on the last snapshot"));
        assert!(s.resolve("e1", &t, None).is_err());
        assert!(is_snapshot_ref("@e12") && !is_snapshot_ref("e12") && !is_snapshot_ref("@e"));
    }

    #[test]
    fn options_are_checked() {
        assert!(Options::from_args(&serde_json::json!({ "mode": "diff" })).is_err());
        let o = Options::from_args(
            &serde_json::json!({ "mode": "delta", "format": "json", "bounds": true }),
        )
        .unwrap();
        assert!(o.delta && o.json && o.bounds);
    }
}
