// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::test_types::{TestCase, TestSuite};
use atalanta_common::types::{Action, Selector};

/// Severity of a lint warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Warning,
    Error,
}

/// A lint diagnostic about a selector.
#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    pub severity: LintSeverity,
    pub test_name: String,
    pub step_index: usize,
    pub selector: String,
    pub rule: String,
    pub message: String,
}

/// Lint all selectors (and fixed waits) in a test suite, flows included.
pub fn lint_suite(suite: &TestSuite) -> Vec<LintDiagnostic> {
    let mut diagnostics = Vec::new();
    for flow in &suite.flows {
        lint_steps(&flow.steps, &format!("flow {}", flow.id), &mut diagnostics);
    }
    for test in &suite.tests {
        lint_test(test, &mut diagnostics);
    }
    diagnostics
}

fn lint_test(test: &TestCase, diagnostics: &mut Vec<LintDiagnostic>) {
    lint_steps(&test.steps, &test.name, diagnostics);
}

fn lint_steps(steps: &[atalanta_common::Step], name: &str, diagnostics: &mut Vec<LintDiagnostic>) {
    for (i, step) in steps.iter().enumerate() {
        // Every selector the step uses, `within:` containers included, and
        // every selector nested in them (VEL-207: only the top one was seen).
        let mut all = step_selectors(&step.action);
        match &step.action {
            Action::ScrollUntilVisible {
                within: Some(w), ..
            }
            | Action::Swipe {
                within: Some(w), ..
            } => all.push(w),
            _ => {}
        }
        for selector in all {
            lint_tree(selector, false, name, i, diagnostics);
        }
        if let Action::Wait { ms } = step.action {
            check_fixed_wait(
                ms,
                steps.get(i + 1).map(|s| &s.action),
                name,
                i,
                diagnostics,
            );
        }
    }
}

/// Whether `action` already waits for its element before acting.
pub fn waits_for_its_target(action: &Action) -> bool {
    matches!(
        action,
        Action::Tap { .. }
            | Action::DoubleTap { .. }
            | Action::LongPress { .. }
            | Action::Drag { .. }
            | Action::InputText { .. }
            | Action::ClearText { .. }
            | Action::AssertVisible { .. }
            | Action::AssertText { .. }
            | Action::ScrollUntilVisible { .. }
    )
}

/// Rule 5: fixed waits. Before a step that waits for its own element the
/// wait is pure cost (49 s across one real app's suite); elsewhere it's
/// still a guess that's too long on a fast run and too short on a slow one.
fn check_fixed_wait(
    ms: u64,
    next: Option<&Action>,
    test_name: &str,
    step_index: usize,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let message = if next.is_some_and(waits_for_its_target) {
        format!(
            "wait {ms} ms before a step that already waits for its element; remove it \
             (`atalanta migrate waits` does this)"
        )
    } else {
        format!(
            "fixed wait of {ms} ms; wait for a condition instead (e.g. assertVisible on what \
             should appear)"
        )
    };
    diagnostics.push(LintDiagnostic {
        severity: LintSeverity::Warning,
        test_name: test_name.to_string(),
        step_index,
        selector: String::new(),
        rule: "fixed-wait".to_string(),
        message,
    });
}

/// Extract the selector from an action, if any.
/// Every selector `action` uses: its target, and an assertScreenshot's masks.
pub(crate) fn step_selectors(action: &Action) -> Vec<&Selector> {
    let mut out: Vec<&Selector> = extract_selector(action).into_iter().collect();
    if let Action::AssertScreenshot { mask, crop, .. } = action {
        out.extend(mask);
        if let Some(atalanta_common::ScreenshotCrop::Element(sel)) = crop {
            out.push(sel);
        }
    }
    out
}

fn extract_selector(action: &Action) -> Option<&Selector> {
    match action {
        Action::Tap { selector } => Some(selector),
        Action::DoubleTap { selector } => Some(selector),
        Action::LongPress { selector, .. } => Some(selector),
        Action::Drag { from, .. } => Some(from),
        Action::InputText { selector, .. } => Some(selector),
        Action::ClearText { selector } => Some(selector),
        Action::AssertVisible { selector } => Some(selector),
        Action::AssertNotVisible { selector } => Some(selector),
        Action::AssertText { selector, .. } => Some(selector),
        Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    }
}

/// Lint `selector` and every selector nested in it. A part of a compound is
/// narrowed by the other parts, so it is never "too broad" on its own.
fn lint_tree(
    selector: &Selector,
    in_compound: bool,
    test_name: &str,
    step_index: usize,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    if !(in_compound && matches!(selector, Selector::ClassName(_))) {
        lint_selector(selector, test_name, step_index, diagnostics);
    }
    match selector {
        Selector::Compound(parts) => {
            for p in parts {
                lint_tree(p, true, test_name, step_index, diagnostics);
            }
        }
        Selector::Index { selector, .. } => {
            lint_tree(selector, in_compound, test_name, step_index, diagnostics)
        }
        Selector::Relative { target, anchor, .. } => {
            lint_tree(target, in_compound, test_name, step_index, diagnostics);
            lint_tree(anchor, false, test_name, step_index, diagnostics);
        }
        _ => {}
    }
}

fn lint_selector(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let sel_str = selector.to_string();

    // Rule 1: Platform-specific IDs
    check_platform_specific_id(selector, test_name, step_index, &sel_str, diagnostics);

    // Rule 2: Fragile text selectors with exact match
    check_fragile_text(selector, test_name, step_index, &sel_str, diagnostics);

    // Rule 3: Index-based selectors (fragile)
    check_index_selector(selector, test_name, step_index, &sel_str, diagnostics);

    // Rule 4: Empty or overly broad selectors
    check_broad_selector(selector, test_name, step_index, &sel_str, diagnostics);

    // Rule 5: XPath that walks the tree by position
    check_positional_xpath(selector, test_name, step_index, &sel_str, diagnostics);

    // Rules 6-8 (R7): generated ids, regex habits, volatile numbers.
    check_volatile_text_and_ids(selector, test_name, step_index, &sel_str, diagnostics);

    // Rule 9 (D-35): another engine's selector syntax.
    check_foreign_syntax(selector, test_name, step_index, &sel_str, diagnostics);
}

/// Selector syntax of other engines that Atalanta doesn't speak, and what
/// to write instead: Playwright's chaining (`>>`) and text engine
/// (`text=`), Playwright's `:has-text()` and jQuery's `:contains()`.
const FOREIGN_SYNTAX: &[(&str, &str, &str)] = &[
    (
        ">>",
        "Playwright's `>>` chaining",
        "nest the parts with childOf:",
    ),
    (
        "text=",
        "Playwright's `text=` engine",
        "use text: (or textContains:)",
    ),
    (
        ":has-text(",
        "Playwright's `:has-text()`",
        "use textContains: with childOf:/containsChild:",
    ),
    (
        ":contains(",
        "jQuery's `:contains()`",
        "use textContains: with childOf:/containsChild:",
    ),
];

/// `foreign-syntax` (D-35): a selector written for another engine. In
/// `css:` it is an error (the browser rejects it, so the step can never
/// pass); in a text or id value, which would look for those characters
/// literally, a warning.
fn check_foreign_syntax(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let (value, severity) = match selector {
        Selector::Css(v) => (v, LintSeverity::Error),
        Selector::Text(v)
        | Selector::TextContains(v)
        | Selector::Id(v)
        | Selector::AccessibilityId(v) => (v, LintSeverity::Warning),
        _ => return,
    };
    let lower = value.to_lowercase();
    for (needle, what, instead) in FOREIGN_SYNTAX {
        // `text=` only as an engine prefix, not inside ordinary text.
        let hit = if *needle == "text=" {
            lower.trim_start().starts_with(needle) || lower.contains(&format!(">> {needle}"))
        } else {
            lower.contains(needle)
        };
        if hit {
            diagnostics.push(LintDiagnostic {
                severity,
                test_name: test_name.to_string(),
                step_index,
                selector: sel_str.to_string(),
                rule: "foreign-syntax".to_string(),
                message: format!(
                    "'{value}' uses {what}, which Atalanta selectors don't support; {instead}"
                ),
            });
            return;
        }
    }
}

/// `volatile-id`: an id a build generated; `regex-in-text`: a `text:` that
/// reads like a regular expression (Maestro's `text:` is one, Atalanta's is
/// exact); `digits-in-text`: a price, count or time that changes with the
/// data.
fn check_volatile_text_and_ids(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let mut push = |rule: &str, message: String| {
        diagnostics.push(LintDiagnostic {
            severity: LintSeverity::Warning,
            test_name: test_name.to_string(),
            step_index,
            selector: sel_str.to_string(),
            rule: rule.to_string(),
            message,
        })
    };
    match selector {
        Selector::Id(id) | Selector::AccessibilityId(id)
            if atalanta_common::selector_match::is_volatile_id(id) =>
        {
            push(
                "volatile-id",
                format!(
                    "'{id}' looks generated (a long number, hex run or UUID) and changes \
                     between builds; use a stable id, the label or the text"
                ),
            )
        }
        Selector::Text(text) if looks_like_regex(text) => push(
            "regex-in-text",
            format!(
                "text '{text}' looks like a regular expression, but text: matches exactly; \
                 use textMatches for a pattern, or textContains with the fixed part"
            ),
        ),
        Selector::Text(text) if volatile_number(text) => push(
            "digits-in-text",
            format!(
                "text '{text}' holds a price, count or time that changes with the data; \
                 use textContains with the stable part"
            ),
        ),
        _ => {}
    }
}

/// Regex constructs that never occur in ordinary UI text: `.*`, `.+`,
/// `\d`/`\w`/`\s`, a leading `^`, `(a|b)` alternation and `[0-9]`-style
/// classes. "Mr. Smith" and "Price ($)" are not flagged.
fn looks_like_regex(text: &str) -> bool {
    [".*", ".+", "\\d", "\\w", "\\s", "\\.", "(?i)"]
        .iter()
        .any(|p| text.contains(p))
        || text.starts_with('^')
        || (text.contains('(') && text.contains('|') && text.contains(')'))
        || text.split('[').skip(1).any(|p| {
            p.split_once(']')
                .is_some_and(|(class, _)| class.len() >= 3 && class.chars().nth(1) == Some('-'))
        })
}

/// A price (`$12.99`, `12,50 €`), a count (`3 items`) or a clock time
/// (`10:45`).
fn volatile_number(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let currency = |c: char| "$€£¥₹".contains(c);
    chars.iter().enumerate().any(|(i, &c)| {
        if !c.is_ascii_digit() {
            return false;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let after_number = chars[i..]
            .iter()
            .skip_while(|c| c.is_ascii_digit() || matches!(c, '.' | ','))
            .copied()
            .collect::<String>();
        prev.is_some_and(currency)
            || after_number.trim_start().starts_with(currency)
            || (next == Some(':') && chars.get(i + 2).is_some_and(|c| c.is_ascii_digit()))
            || ["item", "items", "result", "results", "new", "unread"]
                .iter()
                .any(|w| {
                    after_number.trim_start().to_lowercase().starts_with(*w)
                        && after_number.starts_with(' ')
                })
    })
}

fn check_platform_specific_id(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    if let Selector::Id(id) = selector {
        // Android resource IDs contain colons/slashes
        if id.contains(':') || id.contains('/') {
            diagnostics.push(LintDiagnostic {
                severity: LintSeverity::Warning,
                test_name: test_name.to_string(),
                step_index,
                selector: sel_str.to_string(),
                rule: "platform-specific-id".to_string(),
                message: format!(
                    "ID '{id}' appears platform-specific (contains ':' or '/'). \
                     Use accessibilityId for cross-platform compatibility."
                ),
            });
        }
    }
}

fn check_fragile_text(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    if let Selector::Text(text) = selector {
        // Characters, not bytes: 17 CJK characters are 51 bytes (VEL-207).
        if text.chars().count() > 50 {
            diagnostics.push(LintDiagnostic {
                severity: LintSeverity::Warning,
                test_name: test_name.to_string(),
                step_index,
                selector: sel_str.to_string(),
                rule: "long-text-selector".to_string(),
                message: "Text selector is very long (>50 chars). Consider using \
                          textContains or accessibilityId for stability."
                    .to_string(),
            });
        }
    }
}

fn check_index_selector(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    if let Selector::Index { .. } = selector {
        diagnostics.push(LintDiagnostic {
            severity: LintSeverity::Warning,
            test_name: test_name.to_string(),
            step_index,
            selector: sel_str.to_string(),
            rule: "index-selector".to_string(),
            message: "Index-based selectors are fragile \u{2014} they break when element \
                      order changes. Prefer id or accessibilityId."
                .to_string(),
        });
    }
}

/// `//FrameLayout[2]/LinearLayout[3]` names an element by where it sits,
/// so any layout change moves it; `[@text='Pay']` predicates are fine.
fn check_positional_xpath(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let Selector::XPath(x) = selector else {
        return;
    };
    let positional = x.split('[').skip(1).any(|p| {
        let inner = p.split(']').next().unwrap_or("").trim();
        !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit())
            || inner.starts_with("position()")
            || inner == "last()"
    });
    if positional {
        diagnostics.push(LintDiagnostic {
            severity: LintSeverity::Warning,
            test_name: test_name.to_string(),
            step_index,
            selector: sel_str.to_string(),
            rule: "positional-xpath".to_string(),
            message: "XPath picks the element by position ([n]); any layout change moves it. \
                      Match an attribute instead, e.g. [@text='…'] or an id."
                .to_string(),
        });
    }
}

fn check_broad_selector(
    selector: &Selector,
    test_name: &str,
    step_index: usize,
    sel_str: &str,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    if let Selector::ClassName(cls) = selector {
        let broad_types = ["View", "UIView", "FrameLayout", "LinearLayout", "div"];
        if broad_types.iter().any(|b| cls == *b) {
            diagnostics.push(LintDiagnostic {
                severity: LintSeverity::Warning,
                test_name: test_name.to_string(),
                step_index,
                selector: sel_str.to_string(),
                rule: "broad-class-selector".to_string(),
                message: format!(
                    "Class selector '{cls}' is very broad and will match many elements. \
                     Consider adding an id or combining with other selectors."
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_engine_syntax_is_flagged() {
        // D-35.
        let rules = |sel: Selector| {
            let mut d = Vec::new();
            lint_selector(&sel, "t", 0, &mut d);
            d.into_iter()
                .filter(|d| d.rule == "foreign-syntax")
                .map(|d| d.severity)
                .collect::<Vec<_>>()
        };
        for css in [
            "#list >> text=Buy",
            "button:has-text(\"Buy\")",
            "li:contains('Tea')",
            "text=Sign in",
        ] {
            assert_eq!(
                rules(Selector::Css(css.into())),
                [LintSeverity::Error],
                "{css}"
            );
        }
        assert_eq!(
            rules(Selector::Text("text=Sign in".into())),
            [LintSeverity::Warning]
        );
        assert_eq!(
            rules(Selector::Id("form >> #email".into())),
            [LintSeverity::Warning]
        );
        for fine in ["Context=menu", "Plain text", "a > b", "next=1"] {
            assert!(rules(Selector::Text(fine.into())).is_empty(), "{fine}");
        }
        assert!(rules(Selector::Css("ul > li.item".into())).is_empty());
    }

    #[test]
    fn positional_xpath_is_flagged_and_attribute_xpath_is_not() {
        let rules = |x: &str| {
            let mut d = Vec::new();
            lint_selector(&Selector::XPath(x.into()), "t", 0, &mut d);
            d.into_iter().map(|d| d.rule).collect::<Vec<_>>()
        };
        assert_eq!(
            rules("//FrameLayout[2]/LinearLayout[3]"),
            ["positional-xpath"]
        );
        assert_eq!(rules("(//button)[last()]"), ["positional-xpath"]);
        assert!(rules("//button[@text='Pay']").is_empty());
    }

    #[test]
    fn test_platform_specific_id() {
        let selector = Selector::Id("com.example:id/button".to_string());
        let mut diags = Vec::new();
        lint_selector(&selector, "test1", 0, &mut diags);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, "platform-specific-id");
    }

    #[test]
    fn test_long_text_selector() {
        let long_text = "a".repeat(51);
        let selector = Selector::Text(long_text);
        let mut diags = Vec::new();
        lint_selector(&selector, "test1", 0, &mut diags);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, "long-text-selector");
    }

    #[test]
    fn test_index_selector() {
        let selector = Selector::Index {
            selector: Box::new(Selector::ClassName("Button".to_string())),
            index: 2,
        };
        let mut diags = Vec::new();
        lint_selector(&selector, "test1", 0, &mut diags);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, "index-selector");
    }

    #[test]
    fn test_broad_class_selector() {
        let selector = Selector::ClassName("UIView".to_string());
        let mut diags = Vec::new();
        lint_selector(&selector, "test1", 0, &mut diags);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, "broad-class-selector");
    }

    #[test]
    fn test_good_selector_no_warnings() {
        let selector = Selector::AccessibilityId("login_button".to_string());
        let mut diags = Vec::new();
        lint_selector(&selector, "test1", 0, &mut diags);
        assert!(diags.is_empty());
    }

    #[test]
    fn nested_selectors_masks_and_within_are_linted() {
        // VEL-207.
        let yaml = r#"
appId: a
tests:
  - name: t
    steps:
      - tap: { text: "Buy", below: { id: "com.app:id/header" } }
      - scrollUntilVisible: { selector: { text: "Row" }, within: { xpath: "//list[2]" } }
      - assertVisible: { text: "日本語のとても長いテキストです日本語のとても長いテキストです" }
"#;
        let suite = crate::parser::parse_suite_from_str(yaml).unwrap();
        let rules: Vec<String> = lint_suite(&suite).into_iter().map(|d| d.rule).collect();
        assert!(
            rules.contains(&"platform-specific-id".to_string()),
            "{rules:?}"
        );
        assert!(rules.contains(&"positional-xpath".to_string()), "{rules:?}");
        assert!(
            !rules.contains(&"long-text-selector".to_string()),
            "{rules:?}"
        );
    }

    #[test]
    fn volatile_ids_regex_habits_and_data_numbers_are_flagged() {
        // R7: three new rules.
        let rules = |sel: Selector| {
            let mut d = Vec::new();
            lint_tree(&sel, false, "t", 0, &mut d);
            d.into_iter().map(|d| d.rule).collect::<Vec<_>>()
        };
        let has = |sel: Selector, rule: &str| rules(sel).iter().any(|r| r == rule);
        assert!(has(Selector::Id("btn_8f3a9c2e1b".into()), "volatile-id"));
        assert!(has(
            Selector::AccessibilityId("3f2504e0-4f89-11d3-9a0c-0305e82c3301".into()),
            "volatile-id"
        ));
        assert!(has(
            Selector::Id("headless:id/node_17".into()),
            "volatile-id"
        ));
        assert!(!has(Selector::Id("login_button".into()), "volatile-id"));
        assert!(!has(Selector::Id("step2".into()), "volatile-id"));
        for re in [
            "Order .*",
            "^Welcome",
            "(Yes|No)",
            "Item [0-9]",
            "Total \\d+",
        ] {
            assert!(has(Selector::Text(re.into()), "regex-in-text"), "{re}");
        }
        for plain in ["Mr. Smith", "Price ($)", "Save", "A [beta] feature"] {
            assert!(
                !has(Selector::Text(plain.into()), "regex-in-text"),
                "{plain}"
            );
        }
        for n in ["$12.99", "12,50 €", "3 items", "10:45"] {
            assert!(has(Selector::Text(n.into()), "digits-in-text"), "{n}");
        }
        for plain in ["Step 1", "Level 2 unlocked", "Checkout"] {
            assert!(
                !has(Selector::Text(plain.into()), "digits-in-text"),
                "{plain}"
            );
        }
        // Nested parts are linted too.
        assert!(has(
            Selector::Compound(vec![
                Selector::Text("$4.50".into()),
                Selector::ClassName("TextView".into())
            ]),
            "digits-in-text"
        ));
    }
}
