// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! JSONPath (RFC 9535) for `expect.json` and captures, through
//! `serde_json_path`.
//!
//! What a path gives is decided by how it is written, never by how many
//! nodes it happens to match: a path that names one place (`$.a.b[0]`, a
//! "singular query" in the RFC) gives that value or nothing; any other
//! (`*`, `..`, a filter, a slice, a union) gives the list of what it
//! matched, empty included. Hurl turns a one-node list into its value,
//! which made `$.items[*].id` mean a string or a list depending on the
//! data (its 8.1 added `--no-jsonpath-coercion` for that).

use serde_json::Value;
use serde_json_path::JsonPath;

/// A parsed path, as written and as run.
#[derive(Debug, Clone)]
pub struct Path {
    written: String,
    path: JsonPath,
    singular: bool,
}

/// What a path selected.
#[derive(Debug, PartialEq)]
pub enum Selected<'a> {
    /// A singular path that matched nothing.
    Missing,
    /// A singular path's value.
    One(&'a Value),
    /// A non-singular path's matches, in document order.
    Many(Vec<&'a Value>),
}

impl Path {
    /// `$.a`, or the short form `a.b[0]` / `[0]` (`$.` / `$` is added).
    pub fn parse(written: &str) -> Result<Path, String> {
        let full = full_form(written);
        let path = JsonPath::parse(&full).map_err(|e| format!("JSONPath `{written}`: {e}"))?;
        Ok(Path {
            written: written.to_string(),
            singular: is_singular(&full),
            path,
        })
    }

    pub fn written(&self) -> &str {
        &self.written
    }

    pub fn select<'a>(&self, doc: &'a Value) -> Selected<'a> {
        let nodes = self.path.query(doc).all();
        match (self.singular, nodes.as_slice()) {
            (true, []) => Selected::Missing,
            (true, [one, ..]) => Selected::One(one),
            (false, _) => Selected::Many(nodes),
        }
    }

    /// The one value a capture takes: a path matching nothing, or several
    /// values, is an error (a filter written to find one item that finds
    /// two would otherwise capture whichever came first).
    pub fn select_one<'a>(&self, doc: &'a Value) -> Result<&'a Value, String> {
        match self.path.query(doc).all().as_slice() {
            [one] => Ok(one),
            [] => Err(format!("`{}` matched nothing", self.written)),
            many => Err(format!(
                "`{}` matched {} values; a capture takes one (index or filter it down)",
                self.written,
                many.len()
            )),
        }
    }
}

fn full_form(written: &str) -> String {
    let p = written.trim();
    if p.starts_with('$') {
        p.to_string()
    } else if p.starts_with('[') {
        format!("${p}")
    } else {
        format!("$.{p}")
    }
}

/// Whether a (parsed, valid) path is a singular query: only names and
/// single indexes, no `..`, `*`, filter (`?`), slice (`:`) or union (`,`).
/// Quoted names are skipped, so `$['a,b']` and `$['x*']` stay singular.
fn is_singular(path: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut in_bracket = false;
    let mut prev = '\0';
    for c in path.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            prev = c;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => in_bracket = true,
            ']' => in_bracket = false,
            '*' | '?' => return false,
            '.' if prev == '.' => return false,
            ':' | ',' if in_bracket => return false,
            _ => {}
        }
        prev = c;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "id": "o-1",
            "items": [{ "sku": "A1", "qty": 2 }, { "sku": "B2", "qty": 1 }],
            "a.b": true,
            "empty": []
        })
    }

    #[test]
    fn the_short_form_is_the_long_one() {
        let d = doc();
        for p in ["$.id", "id", " id "] {
            assert_eq!(
                Path::parse(p).unwrap().select(&d),
                Selected::One(&json!("o-1"))
            );
        }
        assert_eq!(
            Path::parse("items[1].sku").unwrap().select(&d),
            Selected::One(&json!("B2"))
        );
        assert_eq!(
            Path::parse("$['a.b']").unwrap().select(&d),
            Selected::One(&json!(true))
        );
        let root = json!([10, 20]);
        assert_eq!(
            Path::parse("[1]").unwrap().select(&root),
            Selected::One(&json!(20))
        );
        assert_eq!(
            Path::parse("$").unwrap().select(&root),
            Selected::One(&root)
        );
    }

    #[test]
    fn the_way_a_path_is_written_decides_one_or_many() {
        let d = doc();
        assert_eq!(Path::parse("$.nope").unwrap().select(&d), Selected::Missing);
        assert_eq!(
            Path::parse("items[5]").unwrap().select(&d),
            Selected::Missing
        );
        // One match is still a list: the data never changes the shape.
        assert_eq!(
            Path::parse("$.items[?@.sku=='A1'].qty").unwrap().select(&d),
            Selected::Many(vec![&json!(2)])
        );
        assert_eq!(
            Path::parse("$.items[*].sku").unwrap().select(&d),
            Selected::Many(vec![&json!("A1"), &json!("B2")])
        );
        assert_eq!(
            Path::parse("$.empty[*]").unwrap().select(&d),
            Selected::Many(vec![])
        );
        for p in ["$..sku", "$.items[0:1]", "$.items[0,1]", "$.*"] {
            assert!(!is_singular(p), "{p}");
        }
        for p in ["$['a,b']", "$[\"x*\"]", "$.a[0]['b:c']", "$['it\\'s*']"] {
            assert!(is_singular(p), "{p}");
        }
    }

    #[test]
    fn a_capture_takes_exactly_one_value() {
        let d = doc();
        let one = Path::parse("$.items[?@.sku=='B2'].qty").unwrap();
        assert_eq!(one.select_one(&d), Ok(&json!(1)));
        let e = Path::parse("$.items[*].sku")
            .unwrap()
            .select_one(&d)
            .unwrap_err();
        assert!(e.contains("matched 2 values"), "{e}");
        let e = Path::parse("$.nope").unwrap().select_one(&d).unwrap_err();
        assert!(e.contains("matched nothing"), "{e}");
    }

    #[test]
    fn a_bad_path_names_itself() {
        let e = Path::parse("items[").unwrap_err();
        assert!(e.starts_with("JSONPath `items[`:"), "{e}");
    }
}
