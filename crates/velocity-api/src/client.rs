//! Sending an `http` step and judging its response.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use serde_json::Value;
use velocity_common::{Capture, HttpConfig, HttpExpect, HttpRequest, Result, VelocityError};

use crate::check::{self, Source};
use crate::path::{Path, Selected};

/// How long a request may take when neither the step nor the suite says.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// The pause between `retry:` attempts when `intervalMs` isn't given.
pub const DEFAULT_RETRY_INTERVAL_MS: u64 = 1_000;
/// Most of a response body a failure shows (reports keep it short; the
/// whole body is in the debug log).
const BODY_EXCERPT: usize = 512;
/// Largest response body read into memory (a hostile or runaway server
/// could otherwise fill it; security audit 2026-11).
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// The `http` steps of one test: the suite's `config.http` and one cookie
/// jar, so a login's session cookie goes with the requests after it. Made
/// afresh for each test, like the values the test captures.
pub struct HttpSession {
    config: HttpConfig,
    follow: reqwest::Client,
    /// Follows redirects on the request's own origin only: for a request
    /// that carries headers of its own (`crate::guard`).
    follow_same_origin: reqwest::Client,
    no_follow: reqwest::Client,
}

/// A step whose response passed: what it captured, and one line on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Passed {
    pub captured: Vec<(String, String)>,
    pub summary: String,
}

impl HttpSession {
    pub fn new(config: &HttpConfig) -> Result<Self> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let block = crate::guard::metadata_blocked();
        let build = |policy: Policy| {
            let mut b = reqwest::Client::builder()
                .cookie_provider(jar.clone())
                .redirect(policy)
                .danger_accept_invalid_certs(config.insecure)
                .user_agent(concat!("velocity/", env!("CARGO_PKG_VERSION")));
            if block {
                b = b.dns_resolver(Arc::new(crate::guard::FilteringResolver));
            }
            b.build()
                .map_err(|e| VelocityError::Config(format!("http: can't make a client: {e}")))
        };
        Ok(Self {
            config: config.clone(),
            follow: build(crate::guard::redirect_policy(false))?,
            follow_same_origin: build(crate::guard::redirect_policy(true))?,
            no_follow: build(Policy::none())?,
        })
    }

    /// How long one request may take: the step's `timeoutMs`, else
    /// `config.http.timeoutMs`, else 30 s.
    pub fn request_timeout(&self, step_timeout_ms: Option<u64>) -> Duration {
        Duration::from_millis(
            step_timeout_ms
                .or(self.config.timeout_ms)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        )
    }

    /// Send `req` until its expectations pass: once, or with `retry:` up to
    /// `count` more times while they fail or no response comes.
    pub async fn run(&self, req: &HttpRequest, timeout: Duration) -> Result<Passed> {
        let retries = req.retry.as_ref().map_or(0, |r| r.count);
        let pause = Duration::from_millis(
            req.retry
                .as_ref()
                .and_then(|r| r.interval_ms)
                .unwrap_or(DEFAULT_RETRY_INTERVAL_MS),
        );
        let mut attempt = 0;
        loop {
            match self.once(req, timeout).await {
                // A request that can't be built fails the same way again.
                Err(e @ VelocityError::Config(_)) => return Err(e),
                Err(e) if attempt < retries => {
                    attempt += 1;
                    tracing::debug!("http attempt {attempt}/{retries} failed: {e}");
                    tokio::time::sleep(pause).await;
                }
                done => return done,
            }
        }
    }

    async fn once(&self, req: &HttpRequest, timeout: Duration) -> Result<Passed> {
        let (exchange, label) = self.send(req, timeout).await?;
        exchange.judge(req, label)
    }

    /// Send `req` once and read the whole response, judging nothing: what
    /// `--confirm` checks one mined check at a time.
    pub(crate) async fn send(
        &self,
        req: &HttpRequest,
        timeout: Duration,
    ) -> Result<(Exchange, String)> {
        let method_name = req.method();
        let method = reqwest::Method::from_bytes(method_name.as_bytes()).map_err(|e| {
            VelocityError::Config(format!(
                "http: `{method_name}` is not an HTTP method ({})",
                refused(e, &method_name, is_token_char)
            ))
        })?;
        let url = resolve_url(self.config.base_url.as_deref(), &req.url)?;
        let own_headers = !self.config.headers.is_empty() || !req.headers.is_empty();
        let client = if req.follow_redirects == Some(false) {
            &self.no_follow
        } else if own_headers {
            &self.follow_same_origin
        } else {
            &self.follow
        };
        let mut b = client
            .request(method.clone(), &url)
            .timeout(timeout)
            .headers(self.headers(req)?);
        if !req.query.is_empty() {
            b = b.query(&req.query);
        }
        if let Some(json) = &req.json {
            b = b.json(json);
        } else if let Some(text) = &req.body {
            b = b.body(text.clone());
        } else if !req.form.is_empty() {
            b = b.form(&req.form);
        }
        let request = b
            .build()
            .map_err(|e| VelocityError::Config(format!("http: {method} {url}: {}", chain(&e))))?;
        let label = format!("{method} {}", request.url());
        if let Some(why) = crate::guard::refused_url(request.url()) {
            return Err(VelocityError::Config(why));
        }
        tracing::debug!("http {label}");

        let started = Instant::now();
        let unreachable = |e: reqwest::Error| VelocityError::HttpUnreachable {
            request: label.clone(),
            detail: if e.is_timeout() {
                format!("timed out after {} ms", timeout.as_millis())
            } else {
                chain(&e)
            },
        };
        let response = client.execute(request).await.map_err(unreachable)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let mut response = response;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(VelocityError::HttpUnreachable {
                    request: label.clone(),
                    detail: format!(
                        "the response body is over {} MiB; Velocity reads at most that much",
                        MAX_BODY_BYTES / (1024 * 1024)
                    ),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let exchange = Exchange {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            json: None,
        };
        tracing::debug!(
            "http {label} answered {status} in {} ms: {}",
            exchange.elapsed_ms,
            excerpt(&exchange.body)
        );
        Ok((exchange, label))
    }

    /// `config.http.headers`, then the step's (same name, any case: the
    /// step's wins).
    fn headers(&self, req: &HttpRequest) -> Result<HeaderMap> {
        let mut map = HeaderMap::new();
        for (name, value) in self.config.headers.iter().chain(&req.headers) {
            let n = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                VelocityError::Config(format!(
                    "http: bad header name `{name}` ({})",
                    refused(e, name, is_token_char)
                ))
            })?;
            let v = HeaderValue::from_str(value).map_err(|e| {
                VelocityError::Config(format!(
                    "http: header `{name}` has a value HTTP can't carry ({})",
                    refused(e, value, |c| c == '\t' || !c.is_ascii_control())
                ))
            })?;
            map.insert(n, v);
        }
        Ok(map)
    }
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
        Some(b) => Err(VelocityError::Config(format!(
            "http: config.http.baseUrl `{b}` needs http:// or https://"
        ))),
        None => Err(VelocityError::Config(format!(
            "http: `{url}` has no http:// or https://, and the suite has no config.http.baseUrl to put before it"
        ))),
    }
}

/// One response, read whole.
pub(crate) struct Exchange {
    pub(crate) status: u16,
    headers: HeaderMap,
    pub(crate) body: String,
    elapsed_ms: u64,
    /// The body parsed as JSON, once something asks.
    json: Option<std::result::Result<Value, String>>,
}

impl Exchange {
    fn judge(mut self, req: &HttpRequest, label: String) -> Result<Passed> {
        let mut failures = self.check(&req.expect);
        let mut captured = Vec::new();
        for (name, from) in &req.capture {
            match self.capture(from) {
                Ok(v) => captured.push((name.clone(), v)),
                Err(e) => failures.push(format!("capture {name}: {e}")),
            }
        }
        if !failures.is_empty() {
            return Err(VelocityError::HttpCheckFailed {
                request: label,
                status: self.status,
                failures,
                body: excerpt(&self.body),
            });
        }
        // A token taken from a login response is as secret as one from
        // the environment.
        for (name, value) in &captured {
            if velocity_common::redact::is_secret_name(name) {
                velocity_common::redact::register_secret(value);
                velocity_common::redact::register_secret_name(name);
            }
        }
        Ok(Passed {
            captured,
            summary: format!("{label} answered {} in {} ms", self.status, self.elapsed_ms),
        })
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
            _ => {}
        }
        for (name, m) in &expect.headers {
            let values: Vec<String> = self
                .headers
                .get_all(name.as_str())
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect();
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
                Ok(text_of(path.select_one(doc)?))
            }
            Capture::Header(name) => self
                .headers
                .get(name.as_str())
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
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
        let body = &self.body;
        self.json
            .get_or_insert_with(|| {
                serde_json::from_str(body).map_err(|e| {
                    let kind = self
                        .headers
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("no content-type");
                    format!("the body is not JSON ({kind}): {e}")
                })
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

/// The start of a body, on one line.
pub(crate) fn excerpt(body: &str) -> String {
    let one_line: String = body
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if one_line.chars().count() <= BODY_EXCERPT {
        return one_line;
    }
    let cut: String = one_line.chars().take(BODY_EXCERPT).collect();
    format!("{cut}…")
}

/// The http crate's refusal of `s` and the first character `ok` rejects,
/// which its errors leave out: a value interpolated from a file often
/// ends in a newline. Never `s` itself (a header value is often a token).
fn refused(e: impl std::fmt::Display, s: &str, ok: impl Fn(char) -> bool) -> String {
    match s.chars().find(|c| !ok(*c)) {
        Some(c) => format!("{e}: {c:?} is not allowed"),
        None => e.to_string(),
    }
}

/// A character of an HTTP token (a method or a header name, RFC 9110).
fn is_token_char(c: char) -> bool {
    c.is_ascii() && crate::validate::is_token_byte(c as u8)
}

/// An error and its causes, without reqwest's repeated url.
fn chain(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut parts = Vec::new();
    let mut cur: Option<&dyn Error> = e.source();
    while let Some(c) = cur {
        let s = c.to_string();
        if !parts.contains(&s) {
            parts.push(s);
        }
        cur = c.source();
    }
    if parts.is_empty() {
        e.to_string()
    } else {
        parts.join(": ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_without_a_scheme_go_after_the_base() {
        let b = Some("https://api.x.dev/v1/");
        assert_eq!(
            resolve_url(b, "/orders").unwrap(),
            "https://api.x.dev/v1/orders"
        );
        assert_eq!(
            resolve_url(b, "orders?a=1").unwrap(),
            "https://api.x.dev/v1/orders?a=1"
        );
        assert_eq!(resolve_url(b, "").unwrap(), "https://api.x.dev/v1");
        assert_eq!(resolve_url(b, "HTTP://other/x").unwrap(), "HTTP://other/x");
        let e = resolve_url(None, "/orders").unwrap_err().to_string();
        assert!(e.contains("no config.http.baseUrl"), "{e}");
        let e = resolve_url(Some("api.x.dev"), "/o")
            .unwrap_err()
            .to_string();
        assert!(e.contains("needs http:// or https://"), "{e}");
    }

    /// The http crate's errors name no character; a token read from a
    /// file with its newline left on failed as "a value HTTP can't carry".
    #[tokio::test]
    async fn a_refused_method_or_header_names_the_character() {
        let refusal = |method: &str, header: (&str, &str)| {
            let config = HttpConfig {
                base_url: Some("http://127.0.0.1:9".into()),
                headers: vec![(header.0.into(), header.1.into())],
                ..Default::default()
            };
            let req: HttpRequest =
                serde_json::from_value(serde_json::json!({"method": method, "url": "/x"})).unwrap();
            async move {
                let session = HttpSession::new(&config).unwrap();
                match session.once(&req, Duration::from_secs(1)).await {
                    Ok(_) => String::new(),
                    Err(e) => e.to_string(),
                }
            }
        };
        let e = refusal("GE T", ("X-Debug", "1")).await;
        assert!(e.contains("`GE T` is not an HTTP method"), "{e}");
        assert!(e.contains("' ' is not allowed"), "{e}");
        let e = refusal("GET", ("Authorization", "Bearer s3cret\n")).await;
        assert!(e.contains("`Authorization` has a value"), "{e}");
        assert!(e.contains(r"'\n' is not allowed"), "{e}");
        assert!(!e.contains("s3cret"), "the value stays out: {e}");
        let e = refusal("GET", ("X Debug", "1")).await;
        assert!(e.contains("bad header name `X Debug`"), "{e}");
        assert!(e.contains("' ' is not allowed"), "{e}");
    }

    #[test]
    fn an_excerpt_is_one_short_line() {
        assert_eq!(excerpt(" {\n  \"a\": 1\n}\n"), "{   \"a\": 1 }");
        let long = excerpt(&"y".repeat(2000));
        assert!(long.ends_with('…') && long.chars().count() == BODY_EXCERPT + 1);
    }
}
