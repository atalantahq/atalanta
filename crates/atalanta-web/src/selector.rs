use atalanta_common::Selector;
use serde_json::{json, Value};

/// Text cap per element when many are captured at once (`find_elements`),
/// so a broad selector can't serialise the page's text once per node.
pub const ALL_TEXT_LIMIT: usize = 10_000;

/// The reason a query's result says its selector is invalid, if it does.
pub fn invalid_selector(v: &Value) -> Option<String> {
    v.get("velInvalid")?.as_str().map(str::to_string)
}

/// A selector as the in-page library's `matchAll` reads it (see
/// `injected.js`), passed as a JSON argument:
/// `{css}`, `{xpath}`, `{text, exact}`, `{and: [...]}`, `{nth, i}`.
///
/// Every part resolves in the page, with one meaning per field: `className`
/// is a CSS class and `id` includes `data-testid` also when a text part
/// rides along. The tree fallback read `className` as the tag name and `id`
/// as the aria-label, so `{text: Save, className: primary}` never matched
/// (VEL-128). `None` for a layout relation, which needs the tree.
pub fn selector_json(selector: &Selector) -> Option<Value> {
    if let Some(css) = selector_to_css(selector) {
        return Some(json!({ "css": css }));
    }
    Some(match selector {
        Selector::XPath(xp) => json!({ "xpath": xp }),
        Selector::Text(t) => json!({ "text": t, "exact": true }),
        Selector::TextContains(t) => json!({ "text": t, "exact": false }),
        // The accessible name, computed in the page (D-W2).
        Selector::AccessibilityId(name) => json!({ "name": name }),
        Selector::Index { selector, index } => json!({
            "nth": selector_json(selector)?,
            // Every list counts its on-screen matches in reading order,
            // as explore counted them when it wrote the index; a CSS list
            // counted every match in document order (review 10 #1).
            "i": index,
        }),
        Selector::Compound(parts) => json!({
            "and": parts.iter().map(selector_json).collect::<Option<Vec<_>>>()?,
        }),
        // S1: the role and the accessible name are computed in the page.
        Selector::Role(r) => json!({ "role": r.trim().to_ascii_lowercase() }),
        Selector::Name(n) => json!({ "accName": n }),
        // S2: state filters, read in the page as `cap` reads them.
        Selector::Checked(b) => json!({ "state": "checked", "value": b }),
        Selector::Focused(b) => json!({ "state": "focused", "value": b }),
        Selector::Selected(b) => json!({ "state": "selected", "value": b }),
        Selector::Enabled(b) => json!({ "state": "enabled", "value": b }),
        _ => return None,
    })
}

/// Tags `className` also matches as a tag (see `selector_to_css`).
const HTML_TAGS: &[&str] = &[
    "a", "button", "canvas", "div", "form", "h1", "h2", "h3", "h4", "h5", "h6", "img", "input",
    "label", "li", "ol", "option", "p", "select", "span", "svg", "table", "td", "textarea", "th",
    "tr", "ul",
];

/// Convert a `Selector` to a CSS query string for `querySelector`/`querySelectorAll`.
///
/// Returns `None` for selectors that cannot be expressed as CSS
/// (Text, TextContains, XPath, Index) — the caller falls back to a JS text search
/// or an Nth pick.
pub fn selector_to_css(selector: &Selector) -> Option<String> {
    match selector {
        Selector::Css(q) => Some(q.clone()),
        Selector::Id(id) => {
            // Match HTML `id` attribute, the common `data-testid`
            // convention (RN-web `testID`), and Flutter web's
            // `Semantics(identifier:)`, which it renders as
            // `flt-semantics-identifier` (Flutter sets the resource id on
            // Android and the accessibilityIdentifier on iOS from it).
            let v = css_string(id);
            Some(format!(
                "[id={v}], [data-testid={v}], [flt-semantics-identifier={v}]"
            ))
        }
        // The computed accessible name (D-W2): aria-labelledby, <label>,
        // alt, title and content too, not CSS-expressible.
        Selector::AccessibilityId(_) => None,
        // A class name, not CSS: `md:block` (Tailwind) is one class, and
        // `btn primary` means both classes, as in a `class` attribute.
        // An HTML tag name also matches the tag: Atalanta's own tree
        // matching reads `className` as the element type, which is the tag
        // on the web, and the selector builder falls back to it for an
        // element with no id, text or label (a bare checkbox). The page
        // read it as a class only, so `className: input` found nothing
        // (MiniWoB rerun, 2026-11-08).
        Selector::ClassName(cls) => {
            let class = cls
                .split_whitespace()
                .map(|c| format!(".{}", css_ident(c)))
                .collect::<String>();
            Some(if HTML_TAGS.contains(&cls.trim()) {
                format!(":is({class}, {})", cls.trim())
            } else {
                class
            })
        }
        Selector::Compound(parts) => {
            // Compound = AND: the element must satisfy *every* part. Each part is
            // wrapped in `:is(...)` and concatenated, which is a same-element
            // intersection in CSS Level 4 (e.g. `:is([id='x']):is(.btn)`).
            // `:is()` also absorbs the comma-lists produced by `Id` above.
            let frags: Vec<String> = parts
                .iter()
                .filter_map(selector_to_css)
                .map(|c| format!(":is({c})"))
                .collect();
            if frags.len() == parts.len() && !frags.is_empty() {
                Some(frags.concat())
            } else {
                // At least one part isn't CSS-expressible (e.g. Text) — the caller
                // must fall back to a tree walk to honour the full AND.
                None
            }
        }
        // These cannot be expressed as CSS; callers use a JS search or Nth pick.
        Selector::Text(_) | Selector::TextContains(_) | Selector::XPath(_) => None,
        // Regexes run in Rust, on the tree (D-11).
        Selector::TextMatches(_) | Selector::IdMatches(_) => None,
        Selector::Index { .. } => None,
        // Layout relations need the captured tree.
        Selector::Relative { .. } => None,
        // Computed in the page (roles, names, states).
        Selector::Role(_)
        | Selector::Name(_)
        | Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => None,
    }
}

/// `s` as a quoted CSS string (`"..."`), for an attribute value.
fn css_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\0' => out.push('\u{FFFD}'),
            c if c.is_control() => out.push_str(&format!("\\{:x} ", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `s` as a CSS identifier, like the browser's `CSS.escape`.
fn css_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let leading_digit = c.is_ascii_digit() && (i == 0 || (i == 1 && chars[0] == '-'));
        match c {
            '\0' => out.push('\u{FFFD}'),
            c if c.is_control() || leading_digit => out.push_str(&format!("\\{:x} ", c as u32)),
            '-' if i == 0 && chars.len() == 1 => out.push_str("\\-"),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() => out.push(c),
            c => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::Selector;

    #[test]
    fn test_css_passthrough() {
        assert_eq!(
            selector_to_css(&Selector::Css(".btn".to_string())).unwrap(),
            ".btn"
        );
    }

    #[test]
    fn test_id_selector() {
        let css = selector_to_css(&Selector::Id("submit".to_string())).unwrap();
        assert_eq!(
            css,
            r#"[id="submit"], [data-testid="submit"], [flt-semantics-identifier="submit"]"#
        );
    }

    #[test]
    fn test_accessibility_id_is_the_accessible_name() {
        // D-W2: computed in the page, not `[aria-label=…]`.
        assert!(selector_to_css(&Selector::AccessibilityId("Login".to_string())).is_none());
        assert_eq!(
            selector_json(&Selector::AccessibilityId("Login".to_string())),
            Some(json!({ "name": "Login" }))
        );
    }

    #[test]
    fn test_class_name() {
        let css = selector_to_css(&Selector::ClassName("Button".to_string())).unwrap();
        assert_eq!(css, ".Button");
        // A tag name matches the tag too, as tree matching does.
        let css = selector_to_css(&Selector::ClassName("input".to_string())).unwrap();
        assert_eq!(css, ":is(.input, input)");
    }

    #[test]
    fn test_text_returns_none() {
        assert!(selector_to_css(&Selector::Text("Hello".to_string())).is_none());
    }

    #[test]
    fn test_xpath_returns_none() {
        assert!(selector_to_css(&Selector::XPath("//button".to_string())).is_none());
    }

    #[test]
    fn test_index_returns_none() {
        assert!(selector_to_css(&Selector::Index {
            selector: Box::new(Selector::Css(".btn".to_string())),
            index: 0
        })
        .is_none());
    }

    #[test]
    fn test_compound_is_and_intersection() {
        // Compound must AND its parts, not OR them.
        let css = selector_to_css(&Selector::Compound(vec![
            Selector::ClassName("btn".to_string()),
            Selector::Id("login".to_string()),
        ]))
        .unwrap();
        assert_eq!(
            css,
            r#":is(.btn):is([id="login"], [data-testid="login"], [flt-semantics-identifier="login"])"#
        );
    }

    #[test]
    fn test_compound_with_non_css_part_returns_none() {
        // If any part can't be CSS (Text), the whole thing must fall back.
        assert!(selector_to_css(&Selector::Compound(vec![
            Selector::ClassName("btn".to_string()),
            Selector::Text("Login".to_string()),
        ]))
        .is_none());
    }

    #[test]
    fn selectors_are_data_not_code() {
        // A needle with quotes is a JSON value, never script text.
        let v = selector_json(&Selector::Text(r#"say "hi"'); alert(1)//"#.into())).unwrap();
        assert_eq!(v["text"], r#"say "hi"'); alert(1)//"#);
        assert_eq!(v["exact"], true);
    }

    #[test]
    fn every_part_of_a_compound_resolves_in_the_page() {
        let v = selector_json(&Selector::Compound(vec![
            Selector::Text("Save".into()),
            Selector::ClassName("primary".into()),
        ]))
        .unwrap();
        assert_eq!(
            v,
            json!({"and": [{"text": "Save", "exact": true}, {"css": ".primary"}]})
        );
        let v = selector_json(&Selector::Index {
            selector: Box::new(Selector::TextContains("Sa".into())),
            index: 2,
        })
        .unwrap();
        assert_eq!(v, json!({"nth": {"text": "Sa", "exact": false}, "i": 2}));
        let v = selector_json(&Selector::Index {
            selector: Box::new(Selector::Css(".row".into())),
            index: 0,
        })
        .unwrap();
        assert_eq!(v, json!({"nth": {"css": ".row"}, "i": 0}));
    }

    #[test]
    fn a_relation_needs_the_tree() {
        let rel = Selector::Relative {
            target: Box::new(Selector::Text("x".into())),
            relation: atalanta_common::Relation::Below,
            anchor: Box::new(Selector::Text("y".into())),
        };
        assert!(selector_json(&rel).is_none());
        assert!(
            selector_json(&Selector::Compound(vec![rel, Selector::Text("x".into())])).is_none()
        );
    }
}
