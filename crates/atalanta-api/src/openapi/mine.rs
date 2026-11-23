//! `expect.json` checks read from a success response's schema, without
//! running anything (SATORI, ASE 2025, infers its oracles the same way):
//! what the spec promises about every response, so a check can only fail
//! where the implementation and the spec disagree. Only required fields
//! are checked; an optional one may be absent from a correct response.

use atalanta_common::{Checks, JsonType, Matcher};
use serde_json::Value;

use super::spec::Spec;
use super::values::{nullable, required, type_of};

/// The checks for `schema`, at most `cap` of them, and how many more the
/// schema implied.
pub struct Mined {
    pub checks: Vec<(String, Matcher)>,
    pub left_out: usize,
}

pub fn mine(spec: &Spec, schema: &Value, cap: usize) -> Mined {
    let root = spec.flatten(schema);
    let mut all = Vec::new();
    if let Some(m) = field(&root, false) {
        all.push(("$".to_string(), m));
    }
    // The top-level fields, then one level down: enough to catch a wrong
    // shape without a wall of checks.
    let mut nested = Vec::new();
    for (name, prop) in required_props(spec, &root) {
        let path = child("$", &name);
        if let Some(m) = field(&prop, true) {
            all.push((path.clone(), m));
        }
        for (sub, subprop) in required_props(spec, &prop) {
            if let Some(m) = field(&subprop, true) {
                nested.push((child(&path, &sub), m));
            }
        }
    }
    all.extend(nested);
    let left_out = all.len().saturating_sub(cap);
    all.truncate(cap);
    Mined {
        checks: all,
        left_out,
    }
}

/// The required properties of an object schema, flattened, in the order
/// `required` lists them (serde_json keeps `properties` sorted, not as
/// written; `writeOnly` ones never come back).
fn required_props(spec: &Spec, s: &Value) -> Vec<(String, Value)> {
    let Some(props) = s.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    required(s)
        .into_iter()
        .filter_map(|name| props.get(&name).map(|p| (name, spec.flatten(p))))
        .filter(|(_, p)| p.get("writeOnly") != Some(&Value::Bool(true)))
        .collect()
}

/// What one schema promises about the value at its path.
fn field(s: &Value, is_required: bool) -> Option<Matcher> {
    let null_ok = nullable(s);
    let mut c = Checks {
        exists: is_required.then_some(true),
        ..Default::default()
    };
    // `type: [string, integer]` promises neither: only a single type is a check.
    let one_type = s
        .get("type")
        .and_then(Value::as_array)
        .is_none_or(|ts| ts.iter().filter(|t| *t != "null").count() <= 1);
    let kind = type_of(s).filter(|_| one_type).and_then(|t| json_type(&t));
    if !null_ok {
        c.kind = kind;
    }
    if let Some(options) = s.get("enum").and_then(Value::as_array) {
        let mut options = options.clone();
        if null_ok && !options.contains(&Value::Null) {
            options.push(Value::Null);
        }
        c.one_of = Some(options);
    } else if let Some(v) = s.get("const") {
        c.eq = Some(v.clone());
    }
    // A bound or a format holds for the value, not for a null in its place.
    if !null_ok {
        bounds(s, &mut c);
        c.length = length(s, kind);
        if kind.is_none() || kind == Some(JsonType::String) {
            c.matches = s
                .get("format")
                .and_then(Value::as_str)
                .and_then(format_regex)
                .map(str::to_string);
        }
    }
    (c != Checks::default()).then(|| Matcher::Checks(Box::new(c)))
}

fn json_type(t: &str) -> Option<JsonType> {
    Some(match t {
        "string" => JsonType::String,
        "number" => JsonType::Number,
        "integer" => JsonType::Integer,
        "boolean" => JsonType::Boolean,
        "object" => JsonType::Object,
        "array" => JsonType::Array,
        "null" => JsonType::Null,
        _ => return None,
    })
}

/// `minimum`/`maximum` (3.0's boolean `exclusive…` beside them, or 3.1's
/// numeric one on its own).
fn bounds(s: &Value, c: &mut Checks) {
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    let flag = |k: &str| s.get(k) == Some(&Value::Bool(true));
    match (num("minimum"), num("exclusiveMinimum")) {
        (_, Some(x)) => c.gt = Some(x),
        (Some(m), None) if flag("exclusiveMinimum") => c.gt = Some(m),
        (Some(m), None) => c.gte = Some(m),
        _ => {}
    }
    match (num("maximum"), num("exclusiveMaximum")) {
        (_, Some(x)) => c.lt = Some(x),
        (Some(m), None) if flag("exclusiveMaximum") => c.lt = Some(m),
        (Some(m), None) => c.lte = Some(m),
        _ => {}
    }
}

fn length(s: &Value, kind: Option<JsonType>) -> Option<Matcher> {
    let (lo, hi) = match kind {
        Some(JsonType::Array) => ("minItems", "maxItems"),
        Some(JsonType::String) => ("minLength", "maxLength"),
        _ => return None,
    };
    let get = |k: &str| s.get(k).and_then(Value::as_u64);
    match (get(lo).filter(|n| *n > 0), get(hi)) {
        (None, None) => None,
        (Some(a), Some(b)) if a == b => Some(Matcher::Equals(a.into())),
        (a, b) => Some(Matcher::Checks(Box::new(Checks {
            gte: a.map(|n| n as f64),
            lte: b.map(|n| n as f64),
            ..Default::default()
        }))),
    }
}

/// Loose on purpose: a check that rejects a valid value is worse than one
/// that lets a doubtful one through (a date-time without its offset still
/// passes).
fn format_regex(format: &str) -> Option<&'static str> {
    Some(match format {
        "uuid" => "[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
        "date-time" => {
            r"\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?([Zz]|[+-]\d{2}(:?\d{2})?)?"
        }
        "date" => r"\d{4}-\d{2}-\d{2}",
        "email" => r"[^@\s]+@[^@\s]+",
        "uri" | "url" => r"[A-Za-z][A-Za-z0-9+.\-]*:\S*",
        _ => return None,
    })
}

/// `$.name`, or `$['odd-name']` for a name a dotted path can't hold.
fn child(parent: &str, name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("{parent}.{name}")
    } else {
        format!(
            "{parent}['{}']",
            name.replace('\\', "\\\\").replace('\'', "\\'")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_fields_get_what_the_schema_promises() {
        let spec =
            Spec::parse("openapi: 3.0.3\ninfo: { title: T, version: '1' }\npaths: {}\n").unwrap();
        let schema = json!({
            "type": "object",
            "required": ["id", "status", "age", "tags", "owner", "x-trace"],
            "properties": {
                "id": { "type": "string", "format": "uuid" },
                "status": { "type": "string", "enum": ["a", "b"] },
                "age": { "type": "integer", "minimum": 0, "maximum": 30, "exclusiveMaximum": true },
                "tags": { "type": "array", "minItems": 1 },
                "note": { "type": "string" },
                "owner": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string", "nullable": true } } },
                "x-trace": { "type": "string", "nullable": true, "format": "uuid" }
            }
        });
        let m = mine(&spec, &schema, 100);
        let paths: Vec<&str> = m.checks.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "$",
                "$.id",
                "$.status",
                "$.age",
                "$.tags",
                "$.owner",
                "$['x-trace']",
                "$.owner.name"
            ]
        );
        let yaml = |i: usize| serde_json::to_value(&m.checks[i].1).unwrap();
        assert_eq!(yaml(0), json!({ "type": "object" }));
        assert!(yaml(1)["matches"].as_str().unwrap().len() > 10);
        assert_eq!(yaml(2)["oneOf"], json!(["a", "b"]));
        assert_eq!(
            yaml(3),
            json!({ "exists": true, "type": "integer", "gte": 0.0, "lt": 30.0 })
        );
        assert_eq!(yaml(4)["length"], json!({ "gte": 1.0 }));
        // Nullable: no type and no format, only that it is there.
        assert_eq!(yaml(6), json!({ "exists": true }));
        assert_eq!(yaml(7), json!({ "exists": true }));
        // A union of types promises no one of them.
        let union = mine(
            &spec,
            &json!({ "type": "object", "required": ["v"], "properties": { "v": { "type": ["string", "integer"] } } }),
            100,
        );
        let v = union.checks.iter().find(|(p, _)| p == "$.v").unwrap();
        assert_eq!(
            serde_json::to_value(&v.1).unwrap(),
            json!({ "exists": true })
        );
        let capped = mine(&spec, &schema, 3);
        assert_eq!((capped.checks.len(), capped.left_out), (3, 5));
    }
}
