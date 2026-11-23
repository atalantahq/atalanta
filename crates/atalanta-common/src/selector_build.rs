//! The selector NL steps and MCP refs record for an element: one builder, so
//! both pick the same, most stable selector (P-7).

use crate::selector_match::find_all_in_tree;
use crate::{Element, Platform, Rect, Selector};

/// The element's developer-assigned id, when the platform has a stable one.
///
/// Android's `platform_id` is the resource-id and iOS's is the accessibility
/// identifier; both survive reruns. Web's is a per-page-load node id, so it is
/// never offered as a selector (NB-2); the page's own `id` attribute
/// (`dom_id`) is offered instead. Neither is an id a driver made up: a
/// headless tree-path key or a UUID re-targets as soon as a view is added.
pub fn stable_id(el: &Element, platform: Platform) -> Option<&str> {
    if platform == Platform::Web {
        // The page's own `id` attribute is the developer's, like a
        // resource-id; the node id in `platform_id` is not.
        // `data-testid` is what the page's `id:` query also matches.
        return el
            .dom_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| el.test_id.as_deref().filter(|s| !s.is_empty()));
    }
    let id = el.platform_id.as_str();
    let placeholder = matches!(id, "" | "hierarchy" | "ios_hierarchy" | "root");
    (!placeholder && !synthetic_id(id)).then_some(id)
}

/// An id the driver made up rather than one the app set: headless's
/// tree-path key (`headless:node#0.2`, formerly `headless:id/node_N`), or a
/// UUID.
pub fn synthetic_id(id: &str) -> bool {
    if id.starts_with("headless:node#") {
        return true;
    }
    if let Some(n) = id.strip_prefix("headless:id/node_") {
        return !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    }
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == 5
        && parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The most stable selector that finds `target` in `root`.
///
/// Candidates, most stable first: developer id, accessibility label, exact
/// text. The first one that matches `target` and no earlier element wins; if
/// every candidate also matches earlier elements, the best one is used with an
/// index. Coordinates are never used — they break on any layout change.
///
/// `screen` must be the rect the driver matches with: an index counts only the
/// matches the driver will see (VEL-018), and visibility applies inside
/// matching, so a hidden child can't hide its visible twin (VEL-089). When the
/// candidate has twins, the index is explicit even when it is 0 (VEL-077).
/// Values are used exactly as the element carries them, as drivers compare
/// them exactly (VEL-095).
pub fn robust_selector(
    target: &Element,
    root: &Element,
    screen: Option<&Rect>,
    platform: Platform,
) -> Selector {
    let mut candidates = Vec::new();
    if let Some(id) = stable_id(target, platform) {
        candidates.push(Selector::Id(id.to_string()));
    }
    // The accessibility label on every platform. On the web that is
    // `aria-label`, which a `text:` selector never matches in the page
    // (it reads visible text): `tap: { text: "Add Green tea" }` for
    // `<button aria-label="Add Green tea">Add to cart</button>` found
    // nothing and waited out its timeout (pilot, 2026-11-07).
    if let Some(label) = non_empty(&target.label) {
        candidates.push(Selector::AccessibilityId(label.to_string()));
    }
    if let Some(text) = non_empty(&target.text) {
        if Some(text) != non_empty(&target.label) {
            candidates.push(Selector::Text(text.to_string()));
        }
    }
    // S1: role + accessible name, when the type maps to a role. After the
    // plain values, so an id or a unique label still wins; it picks a twin
    // apart where a label and a button share their text ("Pay"). Not on the
    // web, whose name is computed in the page, not from the capture.
    if platform != Platform::Web {
        if let (Some(role), Some(name)) = (
            crate::role::role_of(target),
            crate::role::accessible_name(target),
        ) {
            candidates.push(Selector::Compound(vec![
                Selector::Role(role.to_string()),
                Selector::Name(name),
            ]));
        }
    }
    // On the web, an element with nothing else to go by (an icon) by its
    // class words, which the page resolves (`.trash`), before its bare tag.
    if candidates.is_empty() && platform == Platform::Web {
        if let Some(cls) = target
            .css_class
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            candidates.push(Selector::ClassName(cls.to_string()));
        }
    }
    if candidates.is_empty() {
        candidates.push(Selector::ClassName(target.element_type.clone()));
    }

    let mut fallback = None;
    for sel in candidates {
        // Drivers only match visible, non-empty elements, so index the same way.
        let mut matches = Vec::new();
        find_all_in_tree(root, &sel, screen, &mut matches);
        matches.retain(|m| m.visible && !m.bounds.is_empty());
        match matches.iter().position(|m| same_element(m, target)) {
            Some(0) if matches.len() == 1 => return sel,
            Some(i) if fallback.is_none() => fallback = Some((sel, i)),
            _ => {}
        }
    }
    match fallback {
        // A bare selector with twins resolves differently per driver (tree
        // order vs screen order), so even the first twin gets `index: 0`.
        Some((sel, index)) => Selector::Index {
            selector: Box::new(sel),
            index,
        },
        // The target didn't match any of its own selectors — only possible for
        // drivers whose tree matching differs (e.g. web). Use the best guess.
        None => stable_id(target, platform)
            .map(|id| Selector::Id(id.to_string()))
            .or_else(|| non_empty(&target.text).map(|t| Selector::Text(t.to_string())))
            .or_else(|| non_empty(&target.label).map(|l| Selector::AccessibilityId(l.to_string())))
            .unwrap_or_else(|| Selector::ClassName(target.element_type.clone())),
    }
}

fn same_element(a: &Element, b: &Element) -> bool {
    a.bounds == b.bounds
        && a.element_type == b.element_type
        && a.text == b.text
        && a.label == b.label
        && a.platform_id == b.platform_id
}

/// The value as the element carries it (not trimmed: drivers match exactly),
/// when it isn't blank.
fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(ty: &str, text: &str, y: i32) -> Element {
        Element {
            element_type: ty.into(),
            text: Some(text.into()),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        }
    }

    /// S1: a label and a button share "Pay": the button's selector is
    /// `{role: button, name: Pay}`, not an index that breaks when the label
    /// moves.
    #[test]
    fn role_and_name_pick_a_twin_apart() {
        let root = Element {
            element_type: "FrameLayout".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            visible: true,
            children: vec![el("TextView", "Pay", 0), el("Button", "Pay", 100)],
            ..Default::default()
        };
        let sel = robust_selector(&root.children[1], &root, None, Platform::Android);
        assert_eq!(
            sel,
            Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("Pay".into())
            ])
        );
        // A unique text still wins.
        let sel = robust_selector(&root.children[0], &root, None, Platform::Android);
        assert_eq!(
            sel,
            Selector::Index {
                selector: Box::new(Selector::Text("Pay".into())),
                index: 0
            }
        );
    }
}
