// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Whether a value passes an `expect` matcher, and how it doesn't.

use atalanta_common::{Checks, JsonType, Matcher};
use serde_json::Value;

/// What is being checked, which changes how some checks read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A JSON value from a singular path.
    Json,
    /// The list a non-singular path matched: `exists` asks whether it
    /// matched anything.
    Nodes,
    /// A header or the body: text, so `content-length: 0` and
    /// `x-cached: true` compare with the number or boolean as written.
    Text,
}

/// Each way `actual` (`None`: nothing there) fails `m`, as "expected …,
/// got …"; empty when it passes.
pub fn failures(m: &Matcher, actual: Option<&Value>, src: Source) -> Vec<String> {
    let mut out = Vec::new();
    match m {
        Matcher::Equals(want) => match actual {
            None => out.push(format!("expected {}, got nothing", show(want))),
            Some(got) if !equal(want, got, src) => {
                out.push(format!("expected {}, got {}", show(want), show(got)))
            }
            Some(_) => {}
        },
        Matcher::Checks(c) => checks(c, actual, src, &mut out),
    }
    out
}

fn checks(c: &Checks, actual: Option<&Value>, src: Source, out: &mut Vec<String>) {
    let present = match (src, actual) {
        (Source::Nodes, Some(Value::Array(nodes))) => !nodes.is_empty(),
        (_, a) => a.is_some(),
    };
    match (c.exists, present, actual) {
        (Some(false), true, Some(got)) => {
            out.push(format!("expected nothing, got {}", show(got)));
            return;
        }
        (Some(false), _, _) => return,
        (Some(true), false, _) => {
            out.push("expected something, got nothing".into());
            return;
        }
        _ => {}
    }
    let Some(got) = actual else {
        out.push("expected a value, got nothing".into());
        return;
    };
    if let Some(t) = c.kind {
        if !t.holds(got) {
            out.push(format!(
                "expected {} {t}, got {} ({})",
                article(t),
                show(got),
                JsonType::of(got)
            ));
        }
    }
    if let Some(want) = &c.eq {
        if !equal(want, got, src) {
            out.push(format!("expected {}, got {}", show(want), show(got)));
        }
    }
    if let Some(not) = &c.ne {
        if equal(not, got, src) {
            out.push(format!("expected anything but {}", show(not)));
        }
    }
    for (bound, op, holds) in [
        (c.gt, ">", (|a, b| a > b) as fn(f64, f64) -> bool),
        (c.gte, ">=", |a, b| a >= b),
        (c.lt, "<", |a, b| a < b),
        (c.lte, "<=", |a, b| a <= b),
    ] {
        let Some(bound) = bound else { continue };
        match number(got, src) {
            Some(n) if holds(n, bound) => {}
            Some(_) => out.push(format!("expected {op} {bound}, got {}", show(got))),
            None => out.push(format!("expected a number {op} {bound}, got {}", show(got))),
        }
    }
    if let Some(part) = &c.contains {
        match contains(part, got) {
            Some(true) => {}
            Some(false) => out.push(format!(
                "expected to contain {}, got {}",
                show(part),
                show(got)
            )),
            None => out.push(format!(
                "expected something that can contain {}, got {}",
                show(part),
                show(got)
            )),
        }
    }
    if let Some(pattern) = &c.matches {
        match (text(got), whole_match(pattern)) {
            (_, Err(e)) => out.push(format!("matches: {e}")),
            (Some(s), Ok(re)) if re.is_match(&s) => {}
            (Some(_), Ok(_)) => {
                out.push(format!("expected to match /{pattern}/, got {}", show(got)))
            }
            (None, Ok(_)) => out.push(format!(
                "expected text matching /{pattern}/, got {}",
                show(got)
            )),
        }
    }
    if let Some(len) = &c.length {
        match length(got) {
            Some(n) => out.extend(
                failures(len, Some(&Value::from(n)), Source::Json)
                    .into_iter()
                    .map(|f| format!("length: {f}")),
            ),
            None => out.push(format!(
                "expected something with a length, got {}",
                show(got)
            )),
        }
    }
    if let Some(options) = &c.one_of {
        if !options.iter().any(|o| equal(o, got, src)) {
            let shown: Vec<String> = options.iter().map(show).collect();
            out.push(format!(
                "expected one of {}, got {}",
                shown.join(", "),
                show(got)
            ));
        }
    }
}

/// A `matches:` pattern as it runs: the whole text, `.` across newlines.
pub fn whole_match(pattern: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(&format!("(?s)^(?:{pattern})$"))
        .map_err(|e| format!("bad regular expression /{pattern}/: {e}"))
}

/// Static checks of a matcher: its regexes compile. One with a `${…}` in
/// it is checked when it runs, with the value in.
pub fn validate(m: &Matcher) -> Result<(), String> {
    let Matcher::Checks(c) = m else {
        return Ok(());
    };
    if let Some(p) = c.matches.as_deref().filter(|p| !p.contains("${")) {
        whole_match(p)?;
    }
    for bound in [c.gt, c.gte, c.lt, c.lte].into_iter().flatten() {
        if !bound.is_finite() {
            return Err(format!("{bound} is not a number to compare with"));
        }
    }
    c.length.as_ref().map_or(Ok(()), validate)
}

fn article(t: JsonType) -> &'static str {
    match t {
        JsonType::Integer | JsonType::Object | JsonType::Array => "an",
        _ => "a",
    }
}

/// Equal as JSON (numbers by value), or, for text, as the value is written.
fn equal(want: &Value, got: &Value, src: Source) -> bool {
    match (src, want, got) {
        (Source::Text, Value::Bool(b), Value::String(s)) => s == if *b { "true" } else { "false" },
        // Parsed as JSON writes numbers: "01" and " 1" are not 1, "1.0" is
        // not 1 either (it is written differently).
        (Source::Text, Value::Number(n), Value::String(s)) => s
            .parse::<serde_json::Number>()
            .is_ok_and(|parsed| parsed == *n),
        _ => json_eq(want, got),
    }
}

/// JSON equality with numbers compared by value: 1 and 1.0 are equal.
pub fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => match (x.as_u64(), y.as_u64()) {
                (Some(i), Some(j)) => i == j,
                _ => x.as_f64() == y.as_f64(),
            },
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

/// `contains`: a substring of text; an element of a list (every element,
/// for a list of them); a subset of an object. `None`: `got` can't
/// contain that kind of thing.
fn contains(part: &Value, got: &Value) -> Option<bool> {
    match (got, part) {
        (Value::String(s), Value::String(sub)) => Some(s.contains(sub.as_str())),
        (Value::String(s), Value::Number(_) | Value::Bool(_)) => {
            Some(s.contains(&part.to_string()))
        }
        (Value::Array(items), Value::Array(wanted)) => {
            Some(wanted.iter().all(|w| items.iter().any(|i| subset_eq(w, i))))
        }
        (Value::Array(items), _) => Some(items.iter().any(|i| subset_eq(part, i))),
        (Value::Object(_), Value::Object(_)) => Some(subset_eq(part, got)),
        _ => None,
    }
}

/// `want` is in `got`: objects by subset (every field given, compared the
/// same way), lists by containing each element given, the rest equal.
fn subset_eq(want: &Value, got: &Value) -> bool {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => w
            .iter()
            .all(|(k, v)| g.get(k).is_some_and(|x| subset_eq(v, x))),
        (Value::Array(w), Value::Array(g)) => w.iter().all(|x| g.iter().any(|y| subset_eq(x, y))),
        _ => json_eq(want, got),
    }
}

fn number(v: &Value, src: Source) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if src == Source::Text => s.trim().parse().ok(),
        _ => None,
    }
}

/// The text a `matches:` reads: a string, or a number or boolean as JSON
/// writes it (`$.id: { matches: "\\d+" }`).
fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    }
}

fn length(v: &Value) -> Option<usize> {
    match v {
        Value::String(s) => Some(s.chars().count()),
        Value::Array(a) => Some(a.len()),
        Value::Object(o) => Some(o.len()),
        _ => None,
    }
}

/// A value for a message: compact JSON, cut at 120 characters.
pub fn show(v: &Value) -> String {
    const MAX: usize = 120;
    let s = v.to_string();
    if s.chars().count() <= MAX {
        return s;
    }
    let cut: String = s.chars().take(MAX).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(json: &str) -> Matcher {
        serde_json::from_str(json).unwrap()
    }

    fn check(matcher: &str, got: Value) -> Vec<String> {
        failures(&m(matcher), Some(&got), Source::Json)
    }

    #[test]
    fn plain_values_are_equal_by_value() {
        assert!(check("1", json!(1.0)).is_empty());
        assert!(check("[1, \"a\"]", json!([1.0, "a"])).is_empty());
        assert_eq!(check("\"1\"", json!(1)), ["expected \"1\", got 1"]);
        assert_eq!(check("null", json!(false)), ["expected null, got false"]);
        assert_eq!(
            failures(&m("2"), None, Source::Json),
            ["expected 2, got nothing"]
        );
        // u64 beyond i64, and big integers, compare exactly.
        assert!(check("18446744073709551615", json!(u64::MAX)).is_empty());
        assert!(!check("9007199254740993", json!(9007199254740992u64)).is_empty());
    }

    #[test]
    fn every_failed_check_is_reported() {
        let f = check(
            r#"{"type": "string", "gt": 3, "oneOf": [1, 2], "ne": 7}"#,
            json!(7),
        );
        // `gt: 3` passes; the other three fail, each said once.
        assert_eq!(
            f,
            [
                "expected a string, got 7 (integer)",
                "expected anything but 7",
                "expected one of 1, 2, got 7",
            ]
        );
        assert_eq!(
            check(r#"{"gte": 1}"#, json!("5")),
            ["expected a number >= 1, got \"5\""]
        );
    }

    #[test]
    fn contains_is_substring_element_or_subset() {
        assert!(check(r#"{"contains": "ell"}"#, json!("hello")).is_empty());
        assert!(check(r#"{"contains": "a"}"#, json!(["a", "b"])).is_empty());
        assert!(check(r#"{"contains": ["b", "a"]}"#, json!(["a", "b", "c"])).is_empty());
        assert!(check(
            r#"{"contains": {"sku": "B2"}}"#,
            json!([{ "sku": "A1", "q": 1 }, { "sku": "B2", "q": 3 }])
        )
        .is_empty());
        assert!(check(
            r#"{"contains": {"user": {"roles": ["admin"]}}}"#,
            json!({ "id": 1, "user": { "name": "Ada", "roles": ["dev", "admin"] } })
        )
        .is_empty());
        assert_eq!(
            check(r#"{"contains": {"id": 2}}"#, json!({ "id": 1 })),
            ["expected to contain {\"id\":2}, got {\"id\":1}"]
        );
        // A list element is compared whole, not as a substring.
        assert!(!check(r#"{"contains": "a"}"#, json!(["abc"])).is_empty());
        assert_eq!(
            check(r#"{"contains": "x"}"#, json!(5)),
            ["expected something that can contain \"x\", got 5"]
        );
    }

    #[test]
    fn matches_is_the_whole_text() {
        assert!(check(r#"{"matches": "o-\\d+"}"#, json!("o-12")).is_empty());
        assert!(!check(r#"{"matches": "o-\\d+"}"#, json!("no o-12")).is_empty());
        assert!(check(r#"{"matches": "\\d+"}"#, json!(42)).is_empty());
        assert!(check(r#"{"matches": ".*end"}"#, json!("multi\nline end")).is_empty());
        assert!(validate(&m(r#"{"matches": "("}"#))
            .unwrap_err()
            .contains("bad regular expression"));
        assert!(validate(&m(r#"{"matches": "${PATTERN}("}"#)).is_ok());
    }

    #[test]
    fn length_takes_a_matcher() {
        assert!(check(r#"{"length": 2}"#, json!([1, 2])).is_empty());
        assert!(check(r#"{"length": {"gte": 1}}"#, json!({ "a": 1 })).is_empty());
        assert_eq!(
            check(r#"{"length": 3}"#, json!("ab")),
            ["length: expected 3, got 2"]
        );
        assert_eq!(
            check(r#"{"length": 1}"#, json!(4)),
            ["expected something with a length, got 4"]
        );
    }

    #[test]
    fn exists_tells_absent_from_null() {
        let gone = |s: &str| failures(&m(s), None, Source::Json);
        assert!(gone(r#"{"exists": false}"#).is_empty());
        assert_eq!(
            gone(r#"{"exists": true}"#),
            ["expected something, got nothing"]
        );
        assert_eq!(
            gone(r#"{"type": "null"}"#),
            ["expected a value, got nothing"]
        );
        assert!(check(r#"{"exists": true, "type": "null"}"#, Value::Null).is_empty());
        assert_eq!(
            check(r#"{"exists": false}"#, Value::Null),
            ["expected nothing, got null"]
        );
        // A wildcard's list exists when it matched something.
        let nodes = |s: &str, v: Value| failures(&m(s), Some(&v), Source::Nodes);
        assert!(nodes(r#"{"exists": false}"#, json!([])).is_empty());
        assert!(nodes(r#"{"exists": true, "length": 1}"#, json!(["x"])).is_empty());
    }

    #[test]
    fn text_compares_numbers_as_written() {
        let t = |s: &str, v: &str| failures(&m(s), Some(&json!(v)), Source::Text);
        assert!(t("0", "0").is_empty());
        assert!(t("true", "true").is_empty());
        assert!(t(r#"{"lt": 100}"#, "42").is_empty());
        assert_eq!(t("1", "01"), ["expected 1, got \"01\""]);
    }

    #[test]
    fn long_values_are_cut() {
        let s = show(&json!("x".repeat(500)));
        assert!(s.ends_with('…') && s.chars().count() == 121);
    }
}
