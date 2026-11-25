// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! An `http` step as two pure functions: [`build`] turns the step into the
//! request to send, and [`evaluate`] judges the response against `expect`
//! and takes its `capture:`s. Nothing here does I/O, so `atalanta run` (over
//! reqwest) and the browser playground (over `fetch`) apply the same rules.

use atalanta_common::{AtalantaError, Capture, HttpConfig, HttpExpect, HttpRequest, Result};
use serde_json::Value;

use crate::check::{self, Source};
use crate::path::{Path, Selected};

/// How long a request may take when neither the step nor the suite says.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// The pause between `retry:` attempts when `intervalMs` isn't given.
pub const DEFAULT_RETRY_INTERVAL_MS: u64 = 1_000;

/// The request to send. Header names are lower-case; `url` has the query
/// string; `body` is as it goes on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: u64,
}

/// How long one request may take: the step's `timeoutMs`, else
/// `config.http.timeoutMs`, else 30 s.
pub fn timeout_ms(config: &HttpConfig, step_timeout_ms: Option<u64>) -> u64 {
    step_timeout_ms
        .or(config.timeout_ms)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

/// The request `req` sends under `config`: the method (`GET` by default),
/// the url (after `config.http.baseUrl`, with the query string), the
/// headers (`config.http.headers`, then the step's, which win) and the
/// body with the content type its kind sets.
pub fn build(
    config: &HttpConfig,
    req: &HttpRequest,
    step_timeout_ms: Option<u64>,
) -> Result<Prepared> {
    let method = req.method();
    if method.is_empty() || !method.bytes().all(crate::validate::is_token_byte) {
        return Err(AtalantaError::Config(format!(
            "http: `{method}` is not an HTTP method ({})",
            refused("invalid HTTP method", &method, is_token_char)
        )));
    }
    let resolved = resolve_url(config.base_url.as_deref(), &req.url)?;
    let mut headers = headers(config, req)?;
    let mut url = url::Url::parse(&resolved)
        .map_err(|e| AtalantaError::Config(format!("http: {method} {resolved}: {e}")))?;
    if !req.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&req.query);
    }
    let mut content_type = |value: &str| {
        if !headers.iter().any(|(n, _)| n == "content-type") {
            headers.push(("content-type".to_string(), value.to_string()));
        }
    };
    let body = if let Some(json) = &req.json {
        content_type("application/json");
        Some(json.to_string())
    } else if let Some(text) = &req.body {
        Some(text.clone())
    } else if !req.form.is_empty() {
        content_type("application/x-www-form-urlencoded");
        let form = serde_urlencoded::to_string(&req.form)
            .map_err(|e| AtalantaError::Config(format!("http: {method} {resolved}: {e}")))?;
        Some(form)
    } else {
        None
    };
    Ok(Prepared {
        method,
        url: url.to_string(),
        headers,
        body,
        timeout_ms: timeout_ms(config, step_timeout_ms),
    })
}

/// `config.http.headers`, then the step's (same name, any case: the
/// step's wins).
fn headers(config: &HttpConfig, req: &HttpRequest) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in config.headers.iter().chain(&req.headers) {
        if name.is_empty() || !name.bytes().all(crate::validate::is_token_byte) {
            return Err(AtalantaError::Config(format!(
                "http: bad header name `{name}` ({})",
                refused("invalid HTTP header name", name, is_token_char)
            )));
        }
        let carries = |c: char| c == '\t' || !c.is_ascii_control();
        if !value.chars().all(carries) {
            return Err(AtalantaError::Config(format!(
                "http: header `{name}` has a value HTTP can't carry ({})",
                refused("failed to parse header value", value, carries)
            )));
        }
        let name = name.to_ascii_lowercase();
        out.retain(|(n, _)| *n != name);
        out.push((name, value.clone()));
    }
    Ok(out)
}

/// A url with a scheme as written; any other appended to `base` (one `/`
/// between them, so `/orders` keeps the base's own path).
pub fn resolve_url(base: Option<&str>, url: &str) -> Result<String> {
    let has_scheme = |u: &str| {
        let lower = u.get(..8).unwrap_or(u).to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
    };
    if has_scheme(url) {
        return Ok(url.to_string());
    }
    match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) if has_scheme(b) => {
            let rest = url.trim_start_matches('/');
            let base = b.trim_end_matches('/');
            Ok(if rest.is_empty() {
                base.to_string()
            } else {
                format!("{base}/{rest}")
            })
        }
        Some(b) => Err(AtalantaError::Config(format!(
            "http: config.http.baseUrl `{b}` needs http:// or https://"
        ))),
        None => Err(AtalantaError::Config(format!(
            "http: `{url}` has no http:// or https://, and the suite has no config.http.baseUrl to put before it"
        ))),
    }
}

/// One response, read whole.
pub struct Response {
    pub status: u16,
    /// Names in any case; a header sent twice is two entries.
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub elapsed_ms: u64,
    /// `A to B` (origins) for a redirect that was not followed.
    pub stopped: Option<String>,
    /// The body parsed as JSON, once something asks.
    json: Option<std::result::Result<Value, String>>,
}

/// What [`evaluate`] found: every failed check and every value captured.
#[derive(Debug, Default, PartialEq)]
pub struct Evaluation {
    pub failures: Vec<String>,
    pub captured: Vec<(String, String)>,
}

/// Judge `res` against `req.expect` (every check runs) and take the
/// `capture:`s; a capture that finds nothing is a failure too.
pub fn evaluate(req: &HttpRequest, res: &mut Response) -> Evaluation {
    let mut failures = res.check(&req.expect);
    let mut captured = Vec::new();
    for (name, from) in &req.capture {
        match res.capture(from) {
            Ok(v) => captured.push((name.clone(), v)),
            Err(e) => failures.push(format!("capture {name}: {e}")),
        }
    }
    Evaluation { failures, captured }
}

impl Response {
    pub fn new(status: u16, headers: Vec<(String, String)>, body: String, elapsed_ms: u64) -> Self {
        Self {
            status,
            headers,
            body,
            elapsed_ms,
            stopped: None,
            json: None,
        }
    }

    pub fn stopped(mut self, redirect: Option<String>) -> Self {
        self.stopped = redirect;
        self
    }

    fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn check(&mut self, expect: &HttpExpect) -> Vec<String> {
        let mut out = Vec::new();
        match &expect.status {
            Some(want) if !want.accepts(self.status) => {
                out.push(format!("status: expected {want}, got {}", self.status))
            }
            None if self.status >= 400 => out.push(format!(
                "status: expected below 400 (no expect.status), got {}",
                self.status
            )),
            // Not a pass: the request never reached what it was written for.
            None if self.stopped.is_some() => out.push(format!(
                "status: got {}, a redirect {} that was not followed (a request with headers \
                 or a body follows redirects on its own origin only, and https never to http); \
                 give expect.status to check it",
                self.status,
                self.stopped.as_deref().unwrap_or_default(),
            )),
            _ => {}
        }
        for (name, m) in &expect.headers {
            let values: Vec<&str> = self.header_values(name).collect();
            let got = (!values.is_empty()).then(|| Value::String(values.join(", ")));
            label(
                &mut out,
                &format!("header {name}"),
                check::failures(m, got.as_ref(), Source::Text),
            );
        }
        if !expect.json.is_empty() {
            match self.doc() {
                Err(e) => out.push(e),
                Ok(doc) => {
                    for (written, m) in &expect.json {
                        let fails = match Path::parse(written) {
                            Err(e) => vec![e],
                            Ok(p) => match p.select(doc) {
                                Selected::Missing => check::failures(m, None, Source::Json),
                                Selected::One(v) => check::failures(m, Some(v), Source::Json),
                                Selected::Many(nodes) => {
                                    let list = Value::Array(nodes.into_iter().cloned().collect());
                                    check::failures(m, Some(&list), Source::Nodes)
                                }
                            },
                        };
                        label(&mut out, written, fails);
                    }
                }
            }
        }
        if let Some(m) = &expect.body {
            let body = Value::String(self.body.clone());
            label(
                &mut out,
                "body",
                check::failures(m, Some(&body), Source::Text),
            );
        }
        if let Some(max) = expect.max_ms.filter(|max| self.elapsed_ms > *max) {
            out.push(format!(
                "took {} ms, more than maxMs {max}",
                self.elapsed_ms
            ));
        }
        out
    }

    pub(crate) fn capture(&mut self, from: &Capture) -> std::result::Result<String, String> {
        match from {
            Capture::Json(written) => {
                let path = Path::parse(written)?;
                let doc = self.doc()?;
                let v = path.select_one(doc)?;
                // serde_json reads an integer beyond u64 as a float, and
                // `${NAME}` would insert its rounded form: say so instead.
                if v.as_f64().is_some_and(|f| {
                    v.is_f64()
                        && f.is_finite()
                        && f.fract() == 0.0
                        && f.abs() >= 18446744073709551616.0
                }) {
                    return Err(format!(
                        "`{written}` is a number too large to capture exactly ({v}); have the API send it as a string"
                    ));
                }
                Ok(text_of(v))
            }
            Capture::Header(name) => self
                .header_values(name)
                .next()
                .map(str::to_string)
                .ok_or_else(|| format!("no `{name}` header")),
            Capture::Regex(pattern) => {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| format!("bad regular expression /{pattern}/: {e}"))?;
                let caps = re
                    .captures(&self.body)
                    .ok_or_else(|| format!("/{pattern}/ found nothing in the body"))?;
                let m = caps
                    .get(1)
                    .or_else(|| caps.get(0))
                    .map_or("", |m| m.as_str());
                Ok(m.to_string())
            }
        }
    }

    fn doc(&mut self) -> std::result::Result<&Value, String> {
        // A byte order mark ahead of the JSON (some servers write one) is
        // allowed by RFC 8259 §8.1 but is no JSON text for serde_json.
        let kind = self
            .header_values("content-type")
            .next()
            .unwrap_or("no content-type")
            .to_string();
        let body = self.body.strip_prefix('\u{feff}').unwrap_or(&self.body);
        self.json
            .get_or_insert_with(|| {
                serde_json::from_str(body)
                    .map_err(|e| format!("the body is not JSON ({kind}): {e}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

fn label(out: &mut Vec<String>, what: &str, failures: Vec<String>) {
    out.extend(failures.into_iter().map(|f| format!("{what}: {f}")));
}

/// A captured value as `${NAME}` inserts it: a string as is, anything else
/// as compact JSON (`42`, `true`, `{"a":1}`).
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The http crate's refusal of `s` and the first character `ok` rejects,
/// which its errors leave out: a value interpolated from a file often
/// ends in a newline. Never `s` itself (a header value is often a token).
pub(crate) fn refused(e: impl std::fmt::Display, s: &str, ok: impl Fn(char) -> bool) -> String {
    match s.chars().find(|c| !ok(*c)) {
        Some(c) => format!("{e}: {c:?} is not allowed"),
        None => e.to_string(),
    }
}

/// A character of an HTTP token (a method or a header name, RFC 9110).
fn is_token_char(c: char) -> bool {
    c.is_ascii() && crate::validate::is_token_byte(c as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: Value) -> HttpRequest {
        serde_json::from_value(v).unwrap()
    }

    fn config() -> HttpConfig {
        HttpConfig {
            base_url: Some("https://api.x.dev/v1/".into()),
            headers: vec![
                ("X-Key".into(), "base".into()),
                ("Accept".into(), "a/b".into()),
            ],
            timeout_ms: Some(5000),
            ..Default::default()
        }
    }

    #[test]
    fn urls_without_a_scheme_go_after_the_base() {
        let b = Some("https://api.x.dev/v1/");
        assert_eq!(resolve_url(b, "/o").unwrap(), "https://api.x.dev/v1/o");
        assert!(resolve_url(None, "/o").is_err());
    }

    #[test]
    fn a_get_has_the_base_headers_and_the_suite_timeout() {
        let p = build(&config(), &req(json!({"url": "/users/1"})), None).unwrap();
        assert_eq!(p.method, "GET");
        assert_eq!(p.url, "https://api.x.dev/v1/users/1");
        assert_eq!(p.body, None);
        assert_eq!(p.timeout_ms, 5000);
        assert!(p.headers.contains(&("x-key".into(), "base".into())));
        let p = build(&config(), &req(json!({"url": "/u"})), Some(70)).unwrap();
        assert_eq!(p.timeout_ms, 70);
    }

    #[test]
    fn a_step_header_replaces_the_base_one_in_any_case() {
        let r = req(json!({"url": "/u", "headers": {"x-KEY": "step"}}));
        let p = build(&config(), &r, None).unwrap();
        let keys: Vec<_> = p.headers.iter().filter(|(n, _)| n == "x-key").collect();
        assert_eq!(keys, [&("x-key".to_string(), "step".to_string())]);
    }

    #[test]
    fn the_query_is_added_to_the_one_in_the_url() {
        let r = req(json!({"url": "/p?a=1", "query": {"userId": 1, "q": "a b&c"}}));
        let p = build(&config(), &r, None).unwrap();
        assert_eq!(p.url, "https://api.x.dev/v1/p?a=1&q=a+b%26c&userId=1");
    }

    #[test]
    fn json_and_form_bodies_set_their_content_type() {
        let r = req(json!({"method": "post", "url": "/p", "json": {"a": 1}}));
        let p = build(&config(), &r, None).unwrap();
        assert_eq!(p.method, "POST");
        assert_eq!(p.body.as_deref(), Some(r#"{"a":1}"#));
        assert!(p
            .headers
            .contains(&("content-type".into(), "application/json".into())));
        let r = req(json!({"method": "POST", "url": "/p", "form": {"a": "x y", "b": "2"}}));
        let p = build(&config(), &r, None).unwrap();
        assert_eq!(p.body.as_deref(), Some("a=x+y&b=2"));
        assert!(p.headers.contains(&(
            "content-type".into(),
            "application/x-www-form-urlencoded".into()
        )));
        let r = req(
            json!({"url": "/p", "json": {}, "headers": {"Content-Type": "application/vnd.x+json"}}),
        );
        let p = build(&config(), &r, None).unwrap();
        let ct: Vec<_> = p
            .headers
            .iter()
            .filter(|(n, _)| n == "content-type")
            .collect();
        assert_eq!(ct.len(), 1);
        assert_eq!(ct[0].1, "application/vnd.x+json");
    }

    #[test]
    fn a_text_body_is_sent_as_written() {
        let r = req(json!({"method": "PUT", "url": "/p", "body": "hello"}));
        let p = build(&config(), &r, None).unwrap();
        assert_eq!(p.body.as_deref(), Some("hello"));
        assert!(!p.headers.iter().any(|(n, _)| n == "content-type"));
    }

    #[test]
    fn bad_methods_and_headers_name_the_character() {
        let e = build(
            &config(),
            &req(json!({"method": "GE T", "url": "/x"})),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("`GE T` is not an HTTP method"), "{e}");
        assert!(e.contains("' ' is not allowed"), "{e}");
        let r = req(json!({"url": "/x", "headers": {"Authorization": "Bearer s3cret\n"}}));
        let e = build(&config(), &r, None).unwrap_err().to_string();
        assert!(e.contains("`Authorization` has a value"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
        let e = build(&HttpConfig::default(), &req(json!({"url": "/x"})), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no config.http.baseUrl"), "{e}");
    }

    fn reply(status: u16, body: &str) -> Response {
        Response::new(
            status,
            vec![
                ("Content-Type".into(), "application/json".into()),
                ("x-token".into(), "t-1".into()),
            ],
            body.into(),
            40,
        )
    }

    #[test]
    fn a_matching_response_passes_and_captures() {
        let r = req(json!({
            "url": "/u",
            "expect": {"status": 200, "headers": {"content-type": {"contains": "json"}},
                       "json": {"id": 7}, "maxMs": 100},
            "capture": {"ID": {"json": "$.id"}, "TOK": {"header": "X-Token"}, "N": {"regex": "\"n\":\\s*(\\d+)"}}
        }));
        let mut res = reply(200, r#"{"id": 7, "n": 12}"#);
        let e = evaluate(&r, &mut res);
        assert!(e.failures.is_empty(), "{:?}", e.failures);
        assert_eq!(
            e.captured,
            [
                ("ID".to_string(), "7".to_string()),
                ("N".to_string(), "12".to_string()),
                ("TOK".to_string(), "t-1".to_string())
            ]
        );
    }

    #[test]
    fn every_failed_check_is_listed() {
        let r = req(json!({
            "url": "/u",
            "expect": {"status": 201, "json": {"id": 8}, "maxMs": 10},
            "capture": {"X": {"json": "$.nope"}}
        }));
        let mut res = reply(200, r#"{"id": 7}"#);
        let f = evaluate(&r, &mut res).failures;
        assert_eq!(f.len(), 4, "{f:?}");
        assert!(f[0].contains("status: expected 201, got 200"), "{f:?}");
        assert!(f.iter().any(|m| m.starts_with("took 40 ms")), "{f:?}");
        assert!(f.iter().any(|m| m.starts_with("capture X:")), "{f:?}");
    }

    #[test]
    fn without_expect_status_a_4xx_fails_and_a_non_json_body_is_said() {
        let r = req(json!({"url": "/u", "expect": {"json": {"a": 1}}}));
        let mut res = reply(404, "<html>");
        let f = evaluate(&r, &mut res).failures;
        assert!(f[0].contains("expected below 400"), "{f:?}");
        assert!(
            f[1].contains("the body is not JSON (application/json)"),
            "{f:?}"
        );
    }
}
