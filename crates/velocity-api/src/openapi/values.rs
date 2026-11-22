//! A value a schema accepts: its example if it has one, else the least
//! that is valid (required fields only, an enum's first value, a value of
//! the right format).

use serde_json::{Map, Value};

use super::spec::Spec;

const MAX_DEPTH: usize = 8;
/// Most items or characters made to reach a `minItems` or `minLength`: a
/// spec's bound can be any number (`minLength: 1e12` would fill the memory).
const MAX_GENERATED: u64 = 4096;

/// For a request: `readOnly` fields are the server's, so never sent.
pub fn example(spec: &Spec, schema: &Value) -> Value {
    value_at(spec, schema, 0)
}

fn value_at(spec: &Spec, schema: &Value, depth: usize) -> Value {
    let s = spec.flatten(schema);
    if depth > MAX_DEPTH || s.is_null() {
        return Value::Null;
    }
    for key in ["example", "default", "const"] {
        if let Some(v) = s.get(key) {
            return v.clone();
        }
    }
    if let Some(v) = s.get("examples").and_then(|e| e.get(0)) {
        return v.clone();
    }
    if let Some(v) = s.get("enum").and_then(|e| e.get(0)) {
        return v.clone();
    }
    if let Some(first) = ["oneOf", "anyOf"]
        .iter()
        .find_map(|k| s.get(*k).and_then(|a| a.get(0)))
    {
        return value_at(spec, first, depth + 1);
    }
    match type_of(&s).as_deref() {
        Some("object") | None if s.get("properties").is_some() => {
            let mut out = Map::new();
            let props = s.get("properties").and_then(Value::as_object);
            for name in required(&s) {
                let Some(p) = props.and_then(|p| p.get(&name)) else {
                    continue;
                };
                if spec.deref(p).get("readOnly") != Some(&Value::Bool(true)) {
                    out.insert(name, value_at(spec, p, depth + 1));
                }
            }
            Value::Object(out)
        }
        Some("object") => Value::Object(Map::new()),
        Some("array") => {
            let n = s.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            // As many as `minItems` asks: fewer is an example the spec rejects.
            let item = s
                .get("items")
                .map_or(Value::Null, |i| value_at(spec, i, depth + 1));
            Value::Array((0..n.min(MAX_GENERATED)).map(|_| item.clone()).collect())
        }
        Some("integer") | Some("number") => number(&s),
        Some("boolean") => Value::Bool(false),
        Some("null") => Value::Null,
        _ => string(&s),
    }
}

/// The schema's `type`; for 3.1's `[string, "null"]`, the one not null.
pub fn type_of(s: &Value) -> Option<String> {
    match s.get("type")? {
        Value::String(t) => Some(t.clone()),
        Value::Array(ts) => ts
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .map(str::to_string),
        _ => None,
    }
}

/// Whether `null` is allowed: 3.0's `nullable`, or `null` among 3.1 types.
pub fn nullable(s: &Value) -> bool {
    s.get("nullable") == Some(&Value::Bool(true))
        || s.get("type")
            .and_then(Value::as_array)
            .is_some_and(|ts| ts.iter().any(|t| t == "null"))
}

pub fn required(s: &Value) -> Vec<String> {
    s.get("required")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn number(s: &Value) -> Value {
    let int = type_of(s).as_deref() == Some("integer");
    let bound = |k: &str| s.get(k).and_then(Value::as_f64);
    // 3.0's `exclusiveMinimum: true` beside `minimum`; 3.1's is the bound.
    let low = match (bound("minimum"), s.get("exclusiveMinimum")) {
        (_, Some(Value::Number(n))) => n.as_f64().map(|n| n + 1.0),
        (Some(m), Some(Value::Bool(true))) => Some(m + 1.0),
        (m, _) => m,
    };
    let mut n = low.unwrap_or(1.0);
    if let Some(max) = bound("maximum").or_else(|| bound("exclusiveMaximum")) {
        n = n.min(max);
    }
    if int {
        Value::from(n.ceil() as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::from(1), Value::Number)
    }
}

fn string(s: &Value) -> Value {
    let format = s.get("format").and_then(Value::as_str).unwrap_or("");
    let fixed = match format {
        "date-time" => Some("2026-01-01T00:00:00Z"),
        "date" => Some("2026-01-01"),
        "time" => Some("00:00:00Z"),
        "email" => Some("user@example.com"),
        "uuid" => Some("00000000-0000-4000-8000-000000000000"),
        "uri" | "url" => Some("https://example.com"),
        "hostname" => Some("example.com"),
        "ipv4" => Some("127.0.0.1"),
        "ipv6" => Some("::1"),
        _ => None,
    };
    if let Some(f) = fixed {
        return Value::String(f.into());
    }
    let min = s
        .get("minLength")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_GENERATED) as usize;
    let max = s
        .get("maxLength")
        .and_then(Value::as_u64)
        .map(|m| m as usize);
    let mut text = "example".to_string();
    while text.len() < min {
        text.push('x');
    }
    if let Some(max) = max {
        text.truncate(max);
    }
    Value::String(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> Spec {
        Spec::parse(
            "openapi: 3.1.0\ninfo: { title: T, version: '1' }\npaths: {}\ncomponents:\n  schemas:\n    \
             Tag: { type: object, required: [label], properties: { label: { type: string, minLength: 9 } } }\n",
        )
        .unwrap()
    }

    #[test]
    fn the_least_valid_value_has_only_required_fields() {
        let schema = json!({
            "type": "object",
            "required": ["name", "status", "born", "id", "tags", "count"],
            "properties": {
                "id": { "type": "string", "readOnly": true },
                "name": { "type": "string" },
                "status": { "type": "string", "enum": ["available", "sold"] },
                "born": { "type": "string", "format": "date-time" },
                "tags": { "type": "array", "minItems": 1, "items": { "$ref": "#/components/schemas/Tag" } },
                "count": { "type": ["integer", "null"], "minimum": 0, "exclusiveMinimum": true },
                "nickname": { "type": "string" }
            }
        });
        assert_eq!(
            example(&spec(), &schema),
            json!({
                "name": "example",
                "status": "available",
                "born": "2026-01-01T00:00:00Z",
                "tags": [{ "label": "examplexx" }],
                "count": 1
            })
        );
        assert_eq!(
            example(&spec(), &json!({ "type": "integer", "example": 7 })),
            json!(7)
        );
    }

    #[test]
    fn a_lower_bound_is_met_and_a_silly_one_is_capped() {
        let spec = spec();
        // `minItems: 5` is five items, not three.
        let five = example(
            &spec,
            &json!({ "type": "array", "minItems": 5, "items": { "type": "integer" } }),
        );
        assert_eq!(five, json!([1, 1, 1, 1, 1]));
        // A bound no one could mean doesn't fill the memory.
        let long = example(
            &spec,
            &json!({ "type": "string", "minLength": 1_000_000_000_000u64 }),
        );
        assert_eq!(long.as_str().map(str::len), Some(MAX_GENERATED as usize));
        let many = example(
            &spec,
            &json!({ "type": "array", "minItems": 1_000_000_000_000u64 }),
        );
        assert_eq!(many.as_array().map(Vec::len), Some(MAX_GENERATED as usize));
    }
}
