//! The `http` step (API testing): a request, what its response must show,
//! and the values to take from it for later steps. `velocity-api` sends
//! it; these are the types the parser, the schema and the executor share.
//!
//! ```yaml
//! - http:
//!     method: POST
//!     url: /orders                       # appended to config.http.baseUrl
//!     headers: { Authorization: "Bearer ${API_TOKEN}" }
//!     json: { sku: "A1", qty: 2 }
//!     expect:
//!       status: 201
//!       json:
//!         "$.id": { type: string }
//!         "$.items": { length: 1 }
//!     capture:
//!       ORDER_ID: "$.id"
//! ```

use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// One `http` step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpRequest {
    /// `GET` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Absolute (`https://…`), or appended to `config.http.baseUrl`.
    pub url: String,
    /// Sent after `config.http.headers`, replacing one of the same name.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "scalar_map")]
    pub headers: Vec<(String, String)>,
    /// Query parameters, added to any already in the url, in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "scalar_map")]
    pub query: Vec<(String, String)>,
    /// A JSON body (sets `content-type: application/json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    /// A text body, sent as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// A form body (`application/x-www-form-urlencoded`).
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "scalar_map")]
    pub form: Vec<(String, String)>,
    /// What the response must show. Without `status`, any status below 400
    /// passes.
    #[serde(default, skip_serializing_if = "HttpExpect::is_empty")]
    pub expect: HttpExpect,
    /// `NAME: <where>`: values for `${NAME}` in the test's later steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "ordered_map")]
    pub capture: Vec<(String, Capture)>,
    /// Send the request again while `expect` fails (a backend that catches
    /// up). Off by default: requests change things.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<HttpRetry>,
    /// Follow redirects (default true); `false` to check a 3xx itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<bool>,
}

impl HttpRequest {
    /// The method, upper-cased: `GET` when none is given.
    pub fn method(&self) -> String {
        self.method
            .as_deref()
            .map_or_else(|| "GET".to_string(), str::to_ascii_uppercase)
    }

    /// Rewrite every string a `${VAR}` may appear in: the url, headers,
    /// query, bodies, and the expected values and JSON paths.
    pub fn map_strings(
        &mut self,
        f: &mut dyn FnMut(&str) -> crate::Result<String>,
    ) -> crate::Result<()> {
        if let Some(m) = self.method.as_mut() {
            *m = f(m)?;
        }
        self.url = f(&self.url)?;
        for (_, v) in self
            .headers
            .iter_mut()
            .chain(self.query.iter_mut())
            .chain(self.form.iter_mut())
        {
            *v = f(v)?;
        }
        if let Some(j) = self.json.as_mut() {
            map_json_strings(j, f)?;
        }
        if let Some(b) = self.body.as_mut() {
            *b = f(b)?;
        }
        self.expect.map_strings(f)
    }
}

/// What a response must show. Every check runs, and a failure lists each
/// one that failed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpExpect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusCheck>,
    /// Header (any case) → matcher on its value. `{ exists: false }` for a
    /// header that must be absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "ordered_map")]
    pub headers: Vec<(String, Matcher)>,
    /// JSONPath (RFC 9535, `$.items[0].id`; `items[0].id` for short) →
    /// matcher on what it selects: the value for a path that names one
    /// place, the list of matches for one with `*`, `..`, a filter, a slice
    /// or a union.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "ordered_map")]
    pub json: Vec<(String, Matcher)>,
    /// Matcher on the whole body as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Matcher>,
    /// The response must arrive within this many milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<u64>,
}

impl HttpExpect {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    fn map_strings(
        &mut self,
        f: &mut dyn FnMut(&str) -> crate::Result<String>,
    ) -> crate::Result<()> {
        for (_, m) in self.headers.iter_mut() {
            m.map_strings(f)?;
        }
        for (path, m) in self.json.iter_mut() {
            *path = f(path)?;
            m.map_strings(f)?;
        }
        if let Some(m) = self.body.as_mut() {
            m.map_strings(f)?;
        }
        Ok(())
    }
}

/// `status: 201`, `status: [200, 204]` or `status: 2xx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusCheck {
    Code(u16),
    AnyOf(Vec<u16>),
    /// The hundreds digit: `2` for `2xx`.
    Class(u16),
}

impl StatusCheck {
    pub fn accepts(&self, status: u16) -> bool {
        match self {
            Self::Code(c) => *c == status,
            Self::AnyOf(cs) => cs.contains(&status),
            Self::Class(c) => status / 100 == *c,
        }
    }
}

impl fmt::Display for StatusCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(c) => write!(f, "{c}"),
            Self::AnyOf(cs) => {
                let cs: Vec<String> = cs.iter().map(u16::to_string).collect();
                write!(f, "one of {}", cs.join(", "))
            }
            Self::Class(c) => write!(f, "{c}xx"),
        }
    }
}

impl Serialize for StatusCheck {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Code(c) => s.serialize_u16(*c),
            Self::AnyOf(cs) => cs.serialize(s),
            Self::Class(c) => s.serialize_str(&format!("{c}xx")),
        }
    }
}

impl<'de> Deserialize<'de> for StatusCheck {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        const WANT: &str = "a status (201), a list of them ([200, 204]) or a class (2xx)";
        fn code<E: de::Error>(n: u64) -> Result<u16, E> {
            u16::try_from(n)
                .ok()
                .filter(|c| (100..=599).contains(c))
                .ok_or_else(|| E::custom(format!("status {n} is not an HTTP status (100–599)")))
        }
        impl<'de> Visitor<'de> for V {
            type Value = StatusCheck;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(WANT)
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<StatusCheck, E> {
                code(n).map(StatusCheck::Code)
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<StatusCheck, E> {
                code(u64::try_from(n).map_err(|_| E::custom(WANT))?).map(StatusCheck::Code)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<StatusCheck, E> {
                let t = s.trim().to_ascii_lowercase();
                if let Some(d) = t.strip_suffix("xx").filter(|d| d.len() == 1) {
                    return match d.parse::<u16>() {
                        Ok(c) if (1..=5).contains(&c) => Ok(StatusCheck::Class(c)),
                        _ => Err(E::custom(format!("status `{s}`: the class is 1xx to 5xx"))),
                    };
                }
                t.parse::<u64>()
                    .map_err(|_| E::custom(format!("status `{s}`: expected {WANT}")))
                    .and_then(code)
                    .map(StatusCheck::Code)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StatusCheck, A::Error> {
                let mut out = Vec::new();
                while let Some(n) = seq.next_element::<u64>()? {
                    out.push(code(n)?);
                }
                if out.is_empty() {
                    return Err(de::Error::custom("status: an empty list accepts nothing"));
                }
                Ok(StatusCheck::AnyOf(out))
            }
        }
        d.deserialize_any(V)
    }
}

/// An expected value: a plain value (string, number, true/false, null, a
/// list) must equal it; a map lists checks that must all pass. Compare
/// with an object through `eq: { … }`, or `contains: { … }` for a subset.
#[derive(Debug, Clone, PartialEq)]
pub enum Matcher {
    /// Never an object (that is [`Matcher::Checks`]).
    Equals(Value),
    Checks(Box<Checks>),
}

/// The checks of a matcher map, each optional; all given must pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checks {
    /// Equal (numbers by value: 1 and 1.0 are equal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<f64>,
    /// A substring of a string; an element of a list (an object element
    /// matches by subset); for an object, a subset of its fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<Value>,
    /// A regular expression the whole string must match, as `textMatches`
    /// (`.*` around it for a part).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    /// Whether anything is there at all (`false`: the path selects
    /// nothing, the header is absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<JsonType>,
    /// The length of a string (characters), list or object (fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Matcher>,
    /// Equal to one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
}

/// A JSON value's type, for `type:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonType {
    String,
    Number,
    /// A number with no fractional part.
    Integer,
    Boolean,
    Object,
    Array,
    Null,
}

impl JsonType {
    pub const ALL: [&'static str; 7] = [
        "string", "number", "integer", "boolean", "object", "array", "null",
    ];

    pub fn of(v: &Value) -> JsonType {
        match v {
            Value::String(_) => JsonType::String,
            Value::Number(n) if n.is_i64() || n.is_u64() => JsonType::Integer,
            Value::Number(_) => JsonType::Number,
            Value::Bool(_) => JsonType::Boolean,
            Value::Object(_) => JsonType::Object,
            Value::Array(_) => JsonType::Array,
            Value::Null => JsonType::Null,
        }
    }

    /// Whether `v` is of this type (an integer is also a number).
    pub fn holds(self, v: &Value) -> bool {
        let actual = Self::of(v);
        actual == self
            || (self == JsonType::Number && actual == JsonType::Integer)
            || (self == JsonType::Integer
                && v.as_f64()
                    .is_some_and(|f| f.fract() == 0.0 && f.is_finite()))
    }
}

impl fmt::Display for JsonType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let i = *self as usize;
        f.write_str(Self::ALL[i])
    }
}

impl Matcher {
    fn map_strings(
        &mut self,
        f: &mut dyn FnMut(&str) -> crate::Result<String>,
    ) -> crate::Result<()> {
        match self {
            Self::Equals(v) => map_json_strings(v, f),
            Self::Checks(c) => {
                for v in [&mut c.eq, &mut c.ne, &mut c.contains]
                    .into_iter()
                    .flatten()
                {
                    map_json_strings(v, f)?;
                }
                for v in c.one_of.iter_mut().flatten() {
                    map_json_strings(v, f)?;
                }
                if let Some(m) = c.matches.as_mut() {
                    *m = f(m)?;
                }
                if let Some(l) = c.length.as_mut() {
                    l.map_strings(f)?;
                }
                Ok(())
            }
        }
    }
}

impl Serialize for Matcher {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Equals(v) => v.serialize(s),
            Self::Checks(c) => c.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Matcher {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Object(m) if m.is_empty() => Err(de::Error::custom(
                "an empty matcher checks nothing (`eq: {}` compares with an empty object)",
            )),
            v @ Value::Object(_) => serde_json::from_value::<Checks>(v)
                .map(|c| Matcher::Checks(Box::new(c)))
                .map_err(|e| {
                    de::Error::custom(format!(
                        "{e} (a map is a list of checks; compare with an object through `eq: {{ … }}` or `contains: {{ … }}`)"
                    ))
                }),
            v => Ok(Matcher::Equals(v)),
        }
    }
}

/// Where a captured value comes from: a JSONPath into the body (the plain
/// string form), a response header, or a regex's first group (else its
/// whole match) in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture {
    Json(String),
    Header(String),
    Regex(String),
}

impl Serialize for Capture {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (k, v) = match self {
            Self::Json(p) => return s.serialize_str(p),
            Self::Header(h) => ("header", h),
            Self::Regex(r) => ("regex", r),
        };
        let mut m = s.serialize_map(Some(1))?;
        m.serialize_entry(k, v)?;
        m.end()
    }
}

impl<'de> Deserialize<'de> for Capture {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            json: Option<String>,
            header: Option<String>,
            regex: Option<String>,
        }
        match Value::deserialize(d)? {
            Value::String(p) => Ok(Capture::Json(p)),
            v @ Value::Object(_) => {
                let f: Full = serde_json::from_value(v).map_err(de::Error::custom)?;
                match (f.json, f.header, f.regex) {
                    (Some(p), None, None) => Ok(Capture::Json(p)),
                    (None, Some(h), None) => Ok(Capture::Header(h)),
                    (None, None, Some(r)) => Ok(Capture::Regex(r)),
                    _ => Err(de::Error::custom(
                        "a capture takes one of `json`, `header` or `regex`",
                    )),
                }
            }
            _ => Err(de::Error::custom(
                "a capture is a JSONPath (\"$.id\") or a map with `json`, `header` or `regex`",
            )),
        }
    }
}

/// `retry: { count: 5, intervalMs: 1000 }`: up to `count` more requests,
/// `intervalMs` apart (default 1000), while the expectations fail or the
/// server can't be reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpRetry {
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

/// `config.http`: what every `http` step of the suite starts from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpConfig {
    /// Prefix of a step url without a scheme (`/orders`).
    #[serde(default, alias = "base_url", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Sent with every request (a step's header of the same name wins).
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "scalar_map")]
    pub headers: Vec<(String, String)>,
    /// How long a request may take (default 30000); a step's `timeoutMs`
    /// overrides it.
    #[serde(default, alias = "timeout_ms", skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Accept any TLS certificate (a dev server's self-signed one). Never
    /// against a server you don't control.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

/// Rewrite every string inside a JSON value (keys stay).
fn map_json_strings(
    v: &mut Value,
    f: &mut dyn FnMut(&str) -> crate::Result<String>,
) -> crate::Result<()> {
    match v {
        Value::String(s) => *s = f(s)?,
        Value::Array(a) => {
            for x in a {
                map_json_strings(x, f)?;
            }
        }
        Value::Object(o) => {
            for x in o.values_mut() {
                map_json_strings(x, f)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

/// A map kept in the order it was written (checks report in that order;
/// query parameters are sent in it), with a key given twice refused.
mod ordered_map {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(
        v: &[(String, T)],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_map(v.iter().map(|(k, v)| (k, v)))
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<Vec<(String, T)>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Vec<(String, T)>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(String, T)> = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, T>()? {
                    if out.iter().any(|(seen, _)| seen == &k) {
                        return Err(de::Error::custom(format!("`{k}` is given twice")));
                    }
                    out.push((k, v));
                }
                Ok(out)
            }
        }
        d.deserialize_map(V(PhantomData))
    }
}

/// [`ordered_map`] of strings that also takes numbers and booleans as
/// written (`page: 2`, `X-Debug: true`).
mod scalar_map {
    use super::*;

    struct Scalar(String);

    impl<'de> Deserialize<'de> for Scalar {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            match Value::deserialize(d)? {
                Value::String(s) => Ok(Scalar(s)),
                Value::Number(n) => Ok(Scalar(n.to_string())),
                Value::Bool(b) => Ok(Scalar(b.to_string())),
                other => Err(de::Error::custom(format!(
                    "expected a string, number or true/false, got {other}"
                ))),
            }
        }
    }

    pub fn serialize<S: Serializer>(v: &[(String, String)], s: S) -> Result<S::Ok, S::Error> {
        super::ordered_map::serialize(v, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
        let pairs: Vec<(String, Scalar)> = super::ordered_map::deserialize(d)?;
        Ok(pairs.into_iter().map(|(k, Scalar(v))| (k, v)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(yaml: &str) -> Result<HttpRequest, String> {
        serde_yaml::from_str(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn a_request_reads_in_the_order_it_was_written() {
        let r = req(
            "url: /orders\nmethod: post\nquery: { page: 2, sort: name }\nheaders: { X-Debug: true }\n\
             expect:\n  status: 2xx\n  json:\n    \"$.b\": 1\n    \"$.a\": { gt: 0, type: integer }\n\
             capture:\n  ID: \"$.id\"\n  LOC: { header: location }\n",
        )
        .unwrap();
        assert_eq!(r.method(), "POST");
        assert_eq!(
            r.query,
            vec![("page".into(), "2".into()), ("sort".into(), "name".into())]
        );
        assert_eq!(r.headers, vec![("X-Debug".into(), "true".into())]);
        assert_eq!(r.expect.status, Some(StatusCheck::Class(2)));
        let paths: Vec<&str> = r.expect.json.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["$.b", "$.a"]);
        assert_eq!(r.expect.json[0].1, Matcher::Equals(1.into()));
        assert_eq!(
            r.capture,
            vec![
                ("ID".into(), Capture::Json("$.id".into())),
                ("LOC".into(), Capture::Header("location".into()))
            ]
        );
        // And back: what the explore writer and the result cache see.
        let again: HttpRequest = serde_yaml::from_str(&serde_yaml::to_string(&r).unwrap()).unwrap();
        assert_eq!(again, r);
    }

    #[test]
    fn statuses_take_a_code_a_list_or_a_class() {
        let s = |y: &str| serde_yaml::from_str::<StatusCheck>(y).map_err(|e| e.to_string());
        assert_eq!(s("201"), Ok(StatusCheck::Code(201)));
        assert_eq!(s("\"204\""), Ok(StatusCheck::Code(204)));
        assert_eq!(s("[200, 204]"), Ok(StatusCheck::AnyOf(vec![200, 204])));
        assert_eq!(s("4XX"), Ok(StatusCheck::Class(4)));
        assert!(s("700").unwrap_err().contains("not an HTTP status"));
        assert!(s("9xx").unwrap_err().contains("1xx to 5xx"));
        assert!(s("[]").unwrap_err().contains("empty list"));
        assert!(StatusCheck::Class(2).accepts(204) && !StatusCheck::Class(2).accepts(301));
    }

    #[test]
    fn a_map_is_checks_and_a_typo_says_how_to_compare_an_object() {
        let m = |y: &str| serde_yaml::from_str::<Matcher>(y).map_err(|e| e.to_string());
        assert_eq!(m("[1, 2]"), Ok(Matcher::Equals(serde_json::json!([1, 2]))));
        assert_eq!(m("null"), Ok(Matcher::Equals(Value::Null)));
        let Ok(Matcher::Checks(c)) = m("{ length: { gte: 1 }, type: array }") else {
            panic!()
        };
        assert_eq!(c.kind, Some(JsonType::Array));
        assert!(matches!(c.length, Some(Matcher::Checks(_))));
        let e = m("{ name: Ada }").unwrap_err();
        assert!(
            e.contains("unknown field `name`") && e.contains("eq: {"),
            "{e}"
        );
        assert!(m("{}").unwrap_err().contains("empty matcher"));
        assert!(m("{ type: text }").unwrap_err().contains("unknown variant"));
    }

    #[test]
    fn captures_and_maps_refuse_what_is_ambiguous() {
        let e = req("url: x\ncapture: { A: { json: \"$.a\", header: b } }").unwrap_err();
        assert!(e.contains("one of `json`, `header` or `regex`"), "{e}");
        let e = req("url: x\nquery: { a: 1, a: 2 }").unwrap_err();
        assert!(e.contains("twice") || e.contains("duplicate"), "{e}");
        let e = req("url: x\nheaders: { A: [1] }").unwrap_err();
        assert!(e.contains("expected a string, number or true/false"), "{e}");
        assert!(req("url: x\nbodyy: a")
            .unwrap_err()
            .contains("unknown field `bodyy`"));
    }

    #[test]
    fn integers_are_numbers_too() {
        assert!(JsonType::Number.holds(&serde_json::json!(3)));
        assert!(JsonType::Integer.holds(&serde_json::json!(3.0)));
        assert!(!JsonType::Integer.holds(&serde_json::json!(3.5)));
        assert!(!JsonType::String.holds(&serde_json::json!(3)));
        assert_eq!(JsonType::Boolean.to_string(), "boolean");
    }

    #[test]
    fn every_string_is_rewritten_but_keys_stay() {
        let mut r = req(
            "url: \"${B}/x\"\nheaders: { A: \"${T}\" }\njson: { \"${K}\": [\"${V}\"] }\n\
             expect: { json: { \"$.a[?@.id=='${ID}']\": { contains: \"${C}\" } } }",
        )
        .unwrap();
        r.map_strings(&mut |s| Ok(s.replace("${", "<").replace('}', ">")))
            .unwrap();
        assert_eq!(r.url, "<B>/x");
        assert_eq!(r.headers[0].1, "<T>");
        assert_eq!(r.json, Some(serde_json::json!({ "${K}": ["<V>"] })));
        assert_eq!(r.expect.json[0].0, "$.a[?@.id=='<ID>']");
    }
}
