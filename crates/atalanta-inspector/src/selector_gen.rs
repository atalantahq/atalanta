//! Selector generation, after Playwright's `selectorGenerator.ts`: build
//! candidates from cheapest to dearest, and keep the first that resolves to
//! exactly this element on the current screen.
//!
//! Cost order: a stable id, an accessibility label, exact short text (a
//! `textMatches` with `\d+` for its numbers when they look volatile:
//! prices, counts, dates, S4), role + accessible name where the element has
//! a role (S4), class + text, then
//! the same scoped inside a uniquely matched ancestor (`childOf`), and last
//! an `index`. Ids that look generated (long digit or hex runs, UUIDs) are
//! skipped: they change between builds.

use atalanta_common::selector_match::{find_all_ref, match_count};
use atalanta_common::types::{Element, Rect, Relation, Selector};

/// The best selector for `element` without a tree to check uniqueness.
pub fn generate_selector(element: &Element) -> Selector {
    candidates(element)
        .into_iter()
        .next()
        .unwrap_or_else(|| Selector::ClassName(element.element_type.clone()))
}

/// The cheapest selector that finds exactly `element` in `tree`.
pub fn generate_selector_in(tree: &Element, element: &Element) -> Selector {
    let screen = Some(&tree.bounds).filter(|b| !b.is_empty());
    let unique = |s: &Selector| {
        match_count(tree, s, screen) == 1
            && find_all_ref(tree, s, screen)
                .first()
                .is_some_and(|e| same(e, element))
    };
    let own = candidates(element);
    if let Some(s) = own.iter().find(|s| unique(s)) {
        return s.clone();
    }
    // Scope to the nearest ancestor that has a unique selector of its own.
    for ancestor in ancestors(tree, element).into_iter().rev() {
        let Some(scope) = candidates(ancestor)
            .into_iter()
            .find(|s| unique_any(tree, s, screen))
        else {
            continue;
        };
        for target in &own {
            let scoped = Selector::Relative {
                target: Box::new(target.clone()),
                relation: Relation::ChildOf,
                anchor: Box::new(scope.clone()),
            };
            if unique(&scoped) {
                return scoped;
            }
        }
    }
    // Last resort: position among the matches of the best candidate.
    let base = own
        .into_iter()
        .next()
        .unwrap_or_else(|| Selector::ClassName(element.element_type.clone()));
    let index = find_all_ref(tree, &base, screen)
        .iter()
        .position(|e| same(e, element))
        .unwrap_or(0);
    Selector::Index {
        selector: Box::new(base),
        index,
    }
}

/// Every candidate for `element`, cheapest first, with how many elements on
/// the screen each one matches (Appium Inspector style). `best` (the chosen
/// selector) comes first if it isn't already one of them.
pub fn candidates_with_counts(
    tree: &Element,
    element: &Element,
    best: &Selector,
) -> Vec<(Selector, usize)> {
    let screen = Some(&tree.bounds).filter(|b| !b.is_empty());
    let mut all = candidates(element);
    if !all.contains(best) {
        all.insert(0, best.clone());
    }
    all.into_iter()
        .map(|s| {
            let n = match_count(tree, &s, screen);
            (s, n)
        })
        .collect()
}

/// Candidates for `element` alone, cheapest first.
fn candidates(element: &Element) -> Vec<Selector> {
    let mut out = Vec::new();
    if !element.platform_id.is_empty()
        && !looks_generated(&element.platform_id)
        && !atalanta_common::selector_build::synthetic_id(&element.platform_id)
    {
        // Android ids match by their short form (`login` for `pkg:id/login`).
        let id = element
            .platform_id
            .rsplit_once(":id/")
            .map_or(element.platform_id.as_str(), |(_, short)| short);
        out.push(Selector::Id(id.to_string()));
    }
    let text = element.text.as_deref().filter(|t| !t.trim().is_empty());
    if let Some(label) = element.label.as_deref().filter(|l| !l.trim().is_empty()) {
        if text != Some(label) && label.chars().count() <= 60 {
            out.push(Selector::AccessibilityId(label.to_string()));
        }
    }
    if let Some(text) = text.or(element.label.as_deref()) {
        if text.chars().count() <= 60 {
            let stripped = strip_numbers(text);
            // Only a number changes; "Hello!" stays exact (VEL-261).
            if stripped == text || !text.bytes().any(|b| b.is_ascii_digit()) {
                out.push(Selector::Text(text.to_string()));
            } else if stripped.chars().count() >= 3 {
                // "Order #1234" → textMatches "Order #\d+": the number
                // changes, the rest stays exact (S4). textContains is the
                // looser alternative.
                out.push(Selector::TextMatches(volatile_pattern(text)));
                out.push(Selector::TextContains(stripped));
            } else {
                out.push(Selector::Text(text.to_string()));
            }
        }
        // S4: role + accessible name, where the element has a role.
        if let Some(role) = atalanta_common::role::role_of(element) {
            if let Some(name) = atalanta_common::role::accessible_name(element) {
                if name.chars().count() <= 60 {
                    let named = if name.bytes().any(|b| b.is_ascii_digit())
                        && strip_numbers(&name).chars().count() >= 3
                    {
                        Selector::TextMatches(volatile_pattern(&name))
                    } else {
                        Selector::Name(name)
                    };
                    out.push(Selector::Compound(vec![
                        Selector::Role(role.to_string()),
                        named,
                    ]));
                }
            }
        }
        if !element.element_type.is_empty() {
            out.push(Selector::Compound(vec![
                Selector::ClassName(element.element_type.clone()),
                Selector::Text(text.to_string()),
            ]));
        }
    }
    if out.is_empty() && !element.element_type.is_empty() {
        out.push(Selector::ClassName(element.element_type.clone()));
    }
    out
}

/// `text` as a whole-value `textMatches` pattern: every run of digits is
/// `\d+`, everything else literal (escaped). "Due 12/03/2026" →
/// `Due \d+/\d+/\d+`.
fn volatile_pattern(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push_str("\\d+");
            }
            in_digits = true;
            continue;
        }
        in_digits = false;
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The longest run of the text without digits, trimmed.
fn strip_numbers(text: &str) -> String {
    text.split(|c: char| c.is_ascii_digit())
        .map(|p| p.trim_matches(|c: char| !c.is_alphanumeric()))
        .max_by_key(|p| p.chars().count())
        .unwrap_or("")
        .to_string()
}

/// Build-generated ids: 4+ digits in a row, 8+ hex characters in a row, or
/// a UUID.
fn looks_generated(id: &str) -> bool {
    let short = id.rsplit_once(":id/").map_or(id, |(_, s)| s);
    let mut digits = 0;
    let mut hex = 0;
    for c in short.chars() {
        digits = if c.is_ascii_digit() { digits + 1 } else { 0 };
        hex = if c.is_ascii_hexdigit() { hex + 1 } else { 0 };
        if digits >= 4 || (hex >= 8 && short.chars().any(|c| c.is_ascii_digit())) {
            return true;
        }
    }
    false
}

fn unique_any(tree: &Element, s: &Selector, screen: Option<&Rect>) -> bool {
    match_count(tree, s, screen) == 1
}

fn same(a: &Element, b: &Element) -> bool {
    a.bounds == b.bounds && a.element_type == b.element_type && a.platform_id == b.platform_id
}

/// Ancestors of `element` from the root down (excluding the element).
fn ancestors<'a>(tree: &'a Element, element: &Element) -> Vec<&'a Element> {
    fn walk<'a>(node: &'a Element, target: &Element, path: &mut Vec<&'a Element>) -> bool {
        if same(node, target) {
            return true;
        }
        path.push(node);
        for c in &node.children {
            if walk(c, target, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    walk(tree, element, &mut path);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: &str, text: &str, kind: &str, y: i32, children: Vec<Element>) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: (!text.is_empty()).then(|| text.to_string()),
            element_type: kind.into(),
            bounds: Rect {
                x: 0,
                y,
                width: 300,
                height: 40,
            },
            enabled: true,
            visible: true,
            children,
            ..Default::default()
        }
    }

    fn screen(children: Vec<Element>) -> Element {
        let mut root = el("", "", "hierarchy", 0, children);
        root.bounds.height = 2000;
        root
    }

    #[test]
    fn prefers_a_stable_short_id() {
        let target = el("com.app:id/login", "Log in", "Button", 100, vec![]);
        let tree = screen(vec![target.clone()]);
        assert_eq!(
            generate_selector_in(&tree, &target),
            Selector::Id("login".into())
        );
    }

    // VEL-258: a headless id-less view's key (`headless:node#0.1`, formerly
    // `headless:id/node_1`) is made up by the driver; never propose it.
    #[test]
    fn skips_synthetic_headless_ids() {
        for id in ["headless:node#0.1", "headless:id/node_1"] {
            let target = el(id, "Pay", "Button", 100, vec![]);
            let tree = screen(vec![target.clone()]);
            assert_eq!(
                generate_selector_in(&tree, &target),
                Selector::Text("Pay".into()),
                "{id}"
            );
        }
    }

    // VEL-261: punctuation around text is kept as an exact `text:`.
    #[test]
    fn text_with_punctuation_stays_exact() {
        let target = el("", "Hello!", "TextView", 100, vec![]);
        let tree = screen(vec![target.clone()]);
        assert_eq!(
            generate_selector_in(&tree, &target),
            Selector::Text("Hello!".into())
        );
    }

    #[test]
    fn skips_generated_ids_and_strips_numbers_from_text() {
        let target = el(
            "com.app:id/view_83412",
            "Order #1234",
            "TextView",
            100,
            vec![],
        );
        let tree = screen(vec![target.clone()]);
        assert_eq!(
            generate_selector_in(&tree, &target),
            Selector::TextMatches(r"Order \#\d+".into())
        );
        assert!(looks_generated("a1b2c3d4e5"));
        assert!(!looks_generated("checkout_button"));
    }

    #[test]
    fn scopes_a_repeated_button_to_its_row() {
        let row = |id: &str, title: &str, y: i32| {
            el(
                id,
                "",
                "Cell",
                y,
                vec![
                    el("", title, "TextView", y, vec![]),
                    el("", "Add", "Button", y + 1, vec![]),
                ],
            )
        };
        let tree = screen(vec![
            row("row_tea", "Tea", 0),
            row("row_coffee", "Coffee", 100),
        ]);
        let target = tree.children[1].children[1].clone();
        let sel = generate_selector_in(&tree, &target);
        assert_eq!(sel.to_string(), r#"text="Add" childOf(id="row_coffee")"#);
        // It resolves to the right button.
        let found = atalanta_common::selector_match::find_in_tree(&tree, &sel, None).unwrap();
        assert_eq!(found.bounds.y, 101);
    }

    #[test]
    fn falls_back_to_an_index_when_nothing_else_is_unique() {
        let tree = screen(vec![
            el("", "Add", "Button", 0, vec![]),
            el("", "Add", "Button", 100, vec![]),
        ]);
        let target = tree.children[1].clone();
        let sel = generate_selector_in(&tree, &target);
        assert_eq!(sel.to_string(), r#"text="Add"[1]"#);
    }

    /// S4: a volatile text (a count, a date, a time) becomes a whole-value
    /// `textMatches` that keeps the words exact and lets the numbers
    /// change; it still finds the element after they do.
    #[test]
    fn volatile_text_becomes_text_matches() {
        for (text, pattern, later) in [
            ("Order #1234", r"Order \#\d+", "Order #99"),
            ("Due 12/03/2026", r"Due \d+/\d+/\d+", "Due 1/4/2027"),
            ("3 items in cart", r"\d+ items in cart", "12 items in cart"),
            (
                "Updated 10:45 (x2.5)",
                r"Updated \d+:\d+ \(x\d+\.\d+\)",
                "Updated 9:05 (x1.0)",
            ),
        ] {
            let target = el("", text, "TextView", 100, vec![]);
            let tree = screen(vec![target.clone()]);
            let sel = generate_selector_in(&tree, &target);
            assert_eq!(sel, Selector::TextMatches(pattern.into()), "{text}");
            assert!(atalanta_common::selector_match::check_patterns(&sel).is_ok());
            let moved = screen(vec![el("", later, "TextView", 100, vec![])]);
            assert!(
                atalanta_common::selector_match::find_in_tree(&moved, &sel, None).is_some(),
                "{text} -> {later}"
            );
        }
        // A bare number is too little to go on: it stays exact.
        let target = el("", "42", "TextView", 100, vec![]);
        let tree = screen(vec![target.clone()]);
        assert_eq!(
            generate_selector_in(&tree, &target),
            Selector::Text("42".into())
        );
    }

    /// S4: where text alone is ambiguous (a label and a button both say
    /// "Pay"), the element's role + name picks it, before a container or an
    /// index; the candidates list offers it too.
    #[test]
    fn role_and_name_are_generated_where_a_role_exists() {
        let tree = screen(vec![
            el("", "Pay", "TextView", 0, vec![]),
            el("", "Pay", "android.widget.Button", 100, vec![]),
        ]);
        let target = tree.children[1].clone();
        let sel = generate_selector_in(&tree, &target);
        assert_eq!(
            sel,
            Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("Pay".into())
            ])
        );
        let all = candidates_with_counts(&tree, &target, &sel);
        assert!(all.iter().any(|(s, n)| *s == sel && *n == 1));
        // A static label has no role: no such candidate.
        assert!(!candidates(&tree.children[0])
            .iter()
            .any(|s| matches!(s, Selector::Compound(p) if matches!(p[0], Selector::Role(_)))));
    }
}
