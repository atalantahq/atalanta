// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::types::Selector;

// Steps use flow-style selector maps (`{ text: "Pay", index: 1 }`), the
// form the suite parser reads. The old block form (`index:` / `compound:`
// keys, Rust `{:?}` escaping) produced YAML that didn't parse.

/// Generate a YAML test step snippet for a given action and selector.
pub fn tap_yaml(selector: &Selector) -> String {
    format!("- tap: {}", selector_to_yaml(selector))
}

pub fn type_text_yaml(selector: &Selector, text: &str) -> String {
    format!(
        "- inputText: {{ selector: {}, text: {} }}",
        selector_to_yaml(selector),
        literal(text)
    )
}

/// A recorded string as a quoted YAML scalar that loads back as itself: a
/// suite fills `${NAME}` in every string when it loads, so `${` is written
/// `$${` (an app label or typed text never reads a variable).
fn literal(value: &str) -> String {
    yaml_double_quoted(&atalanta_core::env::escape(value))
}

pub fn swipe_yaml(direction: &str) -> String {
    format!("- swipe: {{ direction: {direction} }}")
}

pub fn assert_visible_yaml(selector: &Selector) -> String {
    format!("- assertVisible: {}", selector_to_yaml(selector))
}

/// A complete, runnable suite with one test made of the recorded steps.
pub fn flow_yaml(name: &str, app_id: &str, step_yamls: &[String]) -> String {
    let steps_block: String = step_yamls
        .iter()
        .map(|y| {
            // The UI sends list items (`- tap: …`); a bare step becomes one.
            let y = if y.trim_start().starts_with("- ") {
                y.clone()
            } else {
                y.trim_start()
                    .lines()
                    .enumerate()
                    .map(|(i, l)| {
                        if i == 0 {
                            format!("- {l}")
                        } else {
                            format!("  {l}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            y.lines()
                .map(|line| format!("      {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "appId: {}\n\ntests:\n  - name: {}\n    steps:\n{steps_block}\n",
        yaml_double_quoted(app_id),
        yaml_double_quoted(name)
    )
}

fn yaml_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Other control characters (and NEL, U+2028/9) are not
            // printable YAML: written raw they fail to load or fold into
            // a space, so the saved selector would no longer match.
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn selector_to_yaml(selector: &Selector) -> String {
    format!("{{ {} }}", selector_fields(selector).join(", "))
}

/// The `key: value` pairs of a selector map.
fn selector_fields(selector: &Selector) -> Vec<String> {
    let q = literal;
    match selector {
        Selector::Id(id) => vec![format!("id: {}", q(id))],
        Selector::Text(text) => vec![format!("text: {}", q(text))],
        Selector::TextContains(sub) => vec![format!("textContains: {}", q(sub))],
        Selector::TextMatches(re) => vec![format!("textMatches: {}", q(re))],
        Selector::IdMatches(re) => vec![format!("idMatches: {}", q(re))],
        Selector::AccessibilityId(aid) => vec![format!("accessibilityId: {}", q(aid))],
        Selector::ClassName(cls) => vec![format!("className: {}", q(cls))],
        Selector::Index { selector, index } => {
            let mut f = selector_fields(selector);
            f.push(format!("index: {index}"));
            f
        }
        Selector::Compound(parts) => parts.iter().flat_map(selector_fields).collect(),
        Selector::Css(css) => vec![format!("css: {}", q(css))],
        Selector::XPath(xp) => vec![format!("xpath: {}", q(xp))],
        Selector::Role(r) => vec![format!("role: {}", q(r))],
        Selector::Name(n) => vec![format!("name: {}", q(n))],
        Selector::Checked(b) => vec![format!("checked: {b}")],
        Selector::Focused(b) => vec![format!("focused: {b}")],
        Selector::Selected(b) => vec![format!("selected: {b}")],
        Selector::Enabled(b) => vec![format!("enabled: {b}")],
        Selector::Relative {
            target,
            relation,
            anchor,
        } => {
            let mut f = selector_fields(target);
            f.push(format!("{relation}: {}", selector_to_yaml(anchor)));
            f
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::types::Relation;

    /// S1/S2: role, name and state selectors are written as the DSL keys
    /// and parse back to the same selector.
    #[test]
    fn role_name_and_state_selectors_round_trip() {
        let sel = Selector::Compound(vec![
            Selector::Role("checkbox".into()),
            Selector::Name("Remember \"me\"".into()),
            Selector::Checked(false),
            Selector::Enabled(true),
        ]);
        let yaml = flow_yaml(
            "recorded",
            "com.app",
            &[format!("tap: {}", selector_to_yaml(&sel))],
        );
        let suite = atalanta_core::parser::parse_suite_from_str(&yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            atalanta_common::Action::Tap { selector } => assert_eq!(selector, &sel, "{yaml}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn saved_flows_parse_whether_steps_come_as_list_items_or_bare() {
        let yaml = flow_yaml(
            "recorded",
            "com.app",
            &[
                "- launchApp: {}".into(),
                "tap: { text: \"Battery\" }".into(),
                "inputText:\n  selector: { id: \"q\" }\n  text: \"hi\"".into(),
            ],
        );
        let suite = atalanta_core::parser::parse_suite_from_str(&yaml).unwrap();
        assert_eq!(suite.tests[0].steps.len(), 3, "{yaml}");
    }

    #[test]
    fn a_recorded_flow_is_a_suite_that_parses() {
        let pay = Selector::Index {
            selector: Box::new(Selector::Compound(vec![
                Selector::Text("Pay \"now\"".into()),
                Selector::ClassName("Button".into()),
            ])),
            index: 1,
        };
        let edit = Selector::Relative {
            target: Box::new(Selector::Text("Edit".into())),
            relation: Relation::Below,
            anchor: Box::new(Selector::Text("Shipping".into())),
        };
        let yaml = flow_yaml(
            "checkout: café",
            "com.example",
            &[
                tap_yaml(&pay),
                type_text_yaml(&Selector::Id("email".into()), "a@b.c"),
                assert_visible_yaml(&edit),
                swipe_yaml("up"),
            ],
        );
        let suite = atalanta_core::parser::parse_suite_from_str(&yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        assert_eq!(suite.tests[0].name, "checkout: café");
        assert_eq!(suite.tests[0].steps.len(), 4);
        match &suite.tests[0].steps[0].action {
            atalanta_common::types::Action::Tap { selector } => assert_eq!(selector, &pay),
            other => panic!("{other:?}"),
        }
        match &suite.tests[0].steps[2].action {
            atalanta_common::types::Action::AssertVisible { selector } => {
                assert_eq!(selector, &edit)
            }
            other => panic!("{other:?}"),
        }
    }
    /// V1: a label with a control character or a Unicode line break (a
    /// copied-in U+2028 is common in web text) must come back from the
    /// suite parser as itself: YAML folds a raw NEL/LS/PS inside a quoted
    /// scalar into a space and refuses other control characters.
    #[test]
    fn labels_with_control_and_line_break_characters_round_trip() {
        for text in [
            "Foo\u{2028}Bar",
            "Foo\u{2029}Bar",
            "Foo\u{85}Bar",
            "Foo\u{7}Bar",
            "tab\there",
            "\u{1b}[0m",
        ] {
            let sel = Selector::Text(text.into());
            let yaml = flow_yaml("n", "com.app", &[tap_yaml(&sel)]);
            let suite = atalanta_core::parser::parse_suite_from_str(&yaml)
                .unwrap_or_else(|e| panic!("{text:?}: {e}\n{yaml}"));
            match &suite.tests[0].steps[0].action {
                atalanta_common::types::Action::Tap { selector } => {
                    assert_eq!(selector, &sel, "{text:?}\n{yaml}")
                }
                other => panic!("{other:?}"),
            }
        }
    }
}
