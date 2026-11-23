// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! An OpenAPI 3 document read into JSON, its local `$ref`s followed, and
//! its operations in the order they are written.

use atalanta_common::StatusCheck;
use serde_json::{Map, Value};

static NOTHING: Value = Value::Null;

/// How deep `$ref` chains and `allOf` nesting are followed; a schema that
/// refers to itself stops here.
const MAX_DEPTH: usize = 16;

/// Most `allOf` schemas merged for one [`Spec::flatten`] call.
const MAX_PARTS: usize = 2_000;

pub struct Spec {
    doc: Value,
}

/// One method on one path.
#[derive(Debug, Clone)]
pub struct Operation {
    /// Upper case.
    pub method: String,
    pub path: String,
    pub id: Option<String>,
    pub params: Vec<Param>,
    pub body: Option<Body>,
    pub success: Option<Success>,
    /// The security schemes of the operation's own `security` (its first
    /// alternative); `None` when it inherits the document's.
    pub security: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    /// `path`, `query`, `header` or `cookie`.
    pub location: String,
    pub required: bool,
    pub schema: Value,
    pub example: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Body {
    /// `None` for a body that isn't JSON (its media type).
    pub json: Option<Value>,
    pub media_type: String,
    pub example: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Success {
    pub status: StatusCheck,
    pub schema: Option<Value>,
}

impl Spec {
    /// YAML or JSON (JSON is YAML). Swagger 2.0 is refused: its bodies and
    /// responses are shaped differently.
    pub fn parse(text: &str) -> Result<Spec, String> {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(text).map_err(|e| format!("not YAML or JSON: {e}"))?;
        let doc = to_json(yaml);
        match doc.get("openapi") {
            Some(v) if text_of(v).starts_with('3') => Ok(Spec { doc }),
            _ if doc.get("swagger").is_some() => Err(
                "this is Swagger 2.0; convert it to OpenAPI 3 first (swagger2openapi, or editor.swagger.io)"
                    .into(),
            ),
            _ => Err("no `openapi: 3.x` field: not an OpenAPI 3 document".into()),
        }
    }

    /// `info.title` and `info.version`.
    pub fn title(&self) -> String {
        let info = self.doc.get("info");
        let field = |k: &str| info.and_then(|i| i.get(k)).map(text_of);
        match (field("title"), field("version")) {
            (Some(t), Some(v)) => format!("{t} {v}"),
            (Some(t), None) => t,
            _ => "the API".into(),
        }
    }

    /// `servers[0].url` with its variables' defaults filled in. A relative
    /// one (`/v1`) has no host, so it isn't a base url on its own.
    pub fn server(&self) -> Option<String> {
        let s = self.doc.get("servers")?.get(0)?;
        let mut url = s.get("url")?.as_str()?.to_string();
        if let Some(Value::Object(vars)) = s.get("variables") {
            for (name, var) in vars {
                if let Some(d) = var.get("default") {
                    url = url.replace(&format!("{{{name}}}"), &text_of(d));
                }
            }
        }
        Some(url)
    }

    pub fn security_schemes(&self) -> Vec<(String, Value)> {
        self.doc
            .pointer("/components/securitySchemes")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), self.deref(v).clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The document's `security`: the schemes of its first alternative.
    pub fn global_security(&self) -> Vec<String> {
        first_requirement(self.doc.get("security")).unwrap_or_default()
    }

    /// `v` with its `$ref` chain followed: a local `#/…` one only; a remote
    /// or broken one reads as nothing known.
    pub fn deref<'a>(&'a self, v: &'a Value) -> &'a Value {
        let mut cur = v;
        for _ in 0..MAX_DEPTH {
            let Some(r) = cur.get("$ref").and_then(Value::as_str) else {
                return cur;
            };
            match r.strip_prefix('#').and_then(|p| self.doc.pointer(p)) {
                Some(target) => cur = target,
                None => {
                    tracing::warn!("openapi: `$ref: {r}` is not a local reference; left out");
                    return &NOTHING;
                }
            }
        }
        &NOTHING
    }

    /// A schema with its `$ref` followed and its `allOf` parts merged
    /// (properties, required, and the first part's other keywords).
    /// Property schemas are left as written: callers flatten them as they
    /// go down.
    pub fn flatten(&self, schema: &Value) -> Value {
        let mut budget = MAX_PARTS;
        self.flatten_at(schema, 0, &mut budget)
    }

    fn flatten_at(&self, schema: &Value, depth: usize, budget: &mut usize) -> Value {
        let s = self.deref(schema);
        let Some(parts) = s.get("allOf").and_then(Value::as_array) else {
            return s.clone();
        };
        // `allOf` of eight refs to a schema whose `allOf` is eight refs to
        // the next, sixteen deep, is 8^16 parts: the budget ends it.
        if depth >= MAX_DEPTH || *budget == 0 {
            return Value::Null;
        }
        *budget -= 1;
        let mut out: Map<String, Value> = s
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| *k != "allOf")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for part in parts {
            let Value::Object(p) = self.flatten_at(part, depth + 1, budget) else {
                continue;
            };
            for (k, v) in p {
                match (k.as_str(), out.get_mut(&k)) {
                    ("properties", Some(Value::Object(have))) => {
                        if let Value::Object(more) = v {
                            have.extend(more);
                        }
                    }
                    ("required", Some(Value::Array(have))) => {
                        for r in v.as_array().into_iter().flatten() {
                            if !have.contains(r) {
                                have.push(r.clone());
                            }
                        }
                    }
                    (_, Some(_)) => {}
                    (_, None) => {
                        out.insert(k, v);
                    }
                }
            }
        }
        Value::Object(out)
    }

    /// Every operation, in the order the document lists them.
    pub fn operations(&self) -> Vec<Operation> {
        let Some(paths) = self.doc.get("paths").and_then(Value::as_object) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (path, item) in paths {
            let item = self.deref(item);
            let shared = self.params(item.get("parameters"));
            for method in ["get", "post", "put", "patch", "delete", "head", "options"] {
                let Some(op) = item.get(method) else { continue };
                let mut params = self.params(op.get("parameters"));
                // An operation's own parameter replaces the path's one of
                // the same name and place.
                for p in &shared {
                    if !params
                        .iter()
                        .any(|q| q.name == p.name && q.location == p.location)
                    {
                        params.push(p.clone());
                    }
                }
                out.push(Operation {
                    method: method.to_ascii_uppercase(),
                    path: path.clone(),
                    id: op.get("operationId").map(text_of),
                    params,
                    body: op.get("requestBody").and_then(|b| self.body(b)),
                    success: op.get("responses").and_then(|r| self.success(r)),
                    security: first_requirement(op.get("security")),
                });
            }
        }
        out
    }

    fn params(&self, list: Option<&Value>) -> Vec<Param> {
        let mut out = Vec::new();
        for p in list.and_then(Value::as_array).into_iter().flatten() {
            let p = self.deref(p);
            let (Some(name), Some(location)) = (
                p.get("name").and_then(Value::as_str),
                p.get("in").and_then(Value::as_str),
            ) else {
                continue;
            };
            let schema = p.get("schema").cloned().unwrap_or(Value::Null);
            let example = p
                .get("example")
                .cloned()
                .or_else(|| first_example(p.get("examples"), self))
                .or_else(|| self.deref(&schema).get("example").cloned());
            out.push(Param {
                name: name.to_string(),
                location: location.to_string(),
                required: location == "path" || p.get("required") == Some(&Value::Bool(true)),
                schema,
                example,
            });
        }
        out
    }

    fn body(&self, body: &Value) -> Option<Body> {
        let content = self.deref(body).get("content")?.as_object()?;
        let json = content.iter().find(|(t, _)| is_json(t));
        let (media_type, media) = json.or_else(|| content.iter().next())?;
        let example = media
            .get("example")
            .cloned()
            .or_else(|| first_example(media.get("examples"), self));
        Some(Body {
            json: is_json(media_type).then(|| media.get("schema").cloned().unwrap_or(Value::Null)),
            media_type: media_type.clone(),
            example,
        })
    }

    /// The lowest documented 2xx (else `2XX`) and its JSON schema.
    fn success(&self, responses: &Value) -> Option<Success> {
        let responses = responses.as_object()?;
        let code = responses
            .keys()
            .filter_map(|k| k.parse::<u16>().ok())
            .filter(|c| (200..300).contains(c))
            .min();
        let (status, key) = match code {
            Some(c) => (StatusCheck::Code(c), c.to_string()),
            None if responses.contains_key("2XX") => (StatusCheck::Class(2), "2XX".into()),
            None if responses.contains_key("2xx") => (StatusCheck::Class(2), "2xx".into()),
            None => return None,
        };
        let response = self.deref(&responses[&key]);
        let schema = response
            .get("content")
            .and_then(Value::as_object)
            .and_then(|c| c.iter().find(|(t, _)| is_json(t)))
            .and_then(|(_, m)| m.get("schema").cloned());
        Some(Success { status, schema })
    }
}

/// `application/json`, `application/problem+json`, `text/json`, … and
/// `*/*` (a spec that won't say usually means JSON).
fn is_json(media_type: &str) -> bool {
    let t = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    t.ends_with("/json") || t.ends_with("+json") || t == "*/*"
}

fn first_requirement(security: Option<&Value>) -> Option<Vec<String>> {
    let list = security?.as_array()?;
    Some(
        list.first()
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
    )
}

/// The first of an `examples:` map's `value`s.
fn first_example(examples: Option<&Value>, spec: &Spec) -> Option<Value> {
    let (_, first) = examples?.as_object()?.iter().next()?;
    spec.deref(first).get("value").cloned()
}

/// A scalar as text (`version: 1.0` is a number to YAML).
pub(crate) fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// YAML into JSON. Keys become strings: `200:` is a number to YAML, and a
/// status code is a key in every `responses:`.
fn to_json(v: serde_yaml::Value) -> Value {
    use serde_yaml::Value as Y;
    match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(u) = n.as_u64() {
                u.into()
            } else {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(Value::Null, Value::Number)
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(items.into_iter().map(to_json).collect()),
        Y::Mapping(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let key = match to_json(k) {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, to_json(v))
                })
                .collect(),
        ),
        Y::Tagged(t) => to_json(t.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_followed_and_all_of_is_merged() {
        let spec = Spec::parse(
            "openapi: 3.0.3\ninfo: { title: T, version: 1 }\npaths: {}\ncomponents:\n  schemas:\n    \
             Base: { type: object, required: [id], properties: { id: { type: string } } }\n    \
             Alias: { $ref: '#/components/schemas/Base' }\n    \
             Pet: { allOf: [ { $ref: '#/components/schemas/Alias' }, { required: [name], properties: { name: { type: string } } } ] }\n    \
             Loop: { $ref: '#/components/schemas/Loop' }\n",
        )
        .unwrap();
        let pet = spec.flatten(&serde_json::json!({ "$ref": "#/components/schemas/Pet" }));
        assert_eq!(pet["required"], serde_json::json!(["id", "name"]));
        assert_eq!(pet["type"], "object");
        assert!(pet["properties"]["name"].is_object() && pet["properties"]["id"].is_object());
        // A cycle and a remote reference read as nothing known.
        assert!(spec
            .flatten(&serde_json::json!({ "$ref": "#/components/schemas/Loop" }))
            .is_null());
        assert!(spec
            .flatten(&serde_json::json!({ "$ref": "other.yaml#/X" }))
            .is_null());
        assert_eq!(spec.title(), "T 1");
    }

    #[test]
    fn an_all_of_that_fans_out_is_cut_off_not_run_forever() {
        // nine levels of `allOf` over eight refs to the next: 8^9 merges.
        let mut text = String::from(
            "openapi: 3.0.3\ninfo: { title: T, version: 1 }\npaths: {}\ncomponents:\n  schemas:\n",
        );
        let next = |i: usize| format!("{{ $ref: '#/components/schemas/L{i}' }}");
        for i in 0..9 {
            let refs = vec![next(i + 1); 8].join(", ");
            text.push_str(&format!("    L{i}: {{ allOf: [ {refs} ] }}\n"));
        }
        text.push_str("    L9: { type: object, properties: { id: { type: string } } }\n");
        let spec = Spec::parse(&text).unwrap();
        let started = std::time::Instant::now();
        let _ = spec.flatten(&serde_json::json!({ "$ref": "#/components/schemas/L0" }));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn swagger_two_and_other_documents_are_refused() {
        assert!(Spec::parse("swagger: '2.0'\n")
            .err()
            .unwrap()
            .contains("Swagger 2.0"));
        assert!(Spec::parse("a: 1\n")
            .err()
            .unwrap()
            .contains("not an OpenAPI 3"));
    }
}
