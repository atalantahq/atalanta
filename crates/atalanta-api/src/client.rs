// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Sending an `http` step and judging its response.

use std::sync::Arc;
use std::time::{Duration, Instant};

use atalanta_common::{AtalantaError, HttpConfig, HttpRequest, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;

use crate::exchange::{self, Response};

pub use crate::exchange::{resolve_url, DEFAULT_RETRY_INTERVAL_MS, DEFAULT_TIMEOUT_MS};
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
                // An API client has no page to refer from: the default sends the
                // previous url (query string and all) to the redirect's host.
                .referer(false)
                .danger_accept_invalid_certs(config.insecure)
                .user_agent(concat!("atalanta/", env!("CARGO_PKG_VERSION")));
            if block {
                b = b.dns_resolver(Arc::new(crate::guard::FilteringResolver));
            }
            b.build()
                .map_err(|e| AtalantaError::Config(format!("http: can't make a client: {e}")))
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
        Duration::from_millis(exchange::timeout_ms(&self.config, step_timeout_ms))
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
                Err(e @ AtalantaError::Config(_)) => return Err(e),
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
        judge(exchange, req, label)
    }

    /// Send `req` once and read the whole response, judging nothing: what
    /// `--confirm` checks one mined check at a time.
    pub(crate) async fn send(
        &self,
        req: &HttpRequest,
        timeout: Duration,
    ) -> Result<(Response, String)> {
        let prepared = exchange::build(&self.config, req, None)?;
        let method = reqwest::Method::from_bytes(prepared.method.as_bytes())
            .map_err(|e| AtalantaError::Config(format!("http: {}: {e}", prepared.method)))?;
        let url = prepared.url.as_str();
        // Headers of its own, or a body, may hold a secret (an API key, a
        // login's password): such a request follows redirects on its own
        // origin only (a 307/308 would replay the body elsewhere).
        let own_headers = !self.config.headers.is_empty()
            || !req.headers.is_empty()
            || req.json.is_some()
            || req.body.is_some()
            || !req.form.is_empty();
        let client = if req.follow_redirects == Some(false) {
            &self.no_follow
        } else if own_headers {
            &self.follow_same_origin
        } else {
            &self.follow
        };
        let mut headers = HeaderMap::new();
        for (name, value) in &prepared.headers {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        }
        let mut b = client
            .request(method.clone(), url)
            .timeout(timeout)
            .headers(headers);
        if let Some(body) = prepared.body {
            b = b.body(body);
        }
        let request = b
            .build()
            .map_err(|e| AtalantaError::Config(format!("http: {method} {url}: {}", chain(&e))))?;
        let label = format!("{method} {}", request.url());
        let sent_to = request.url().clone();
        let follows = !std::ptr::eq(client, &self.no_follow);
        if let Some(why) = crate::guard::refused_url(request.url()) {
            return Err(AtalantaError::Config(why));
        }
        tracing::debug!("http {label}");

        let started = Instant::now();
        let unreachable = |e: reqwest::Error| AtalantaError::HttpUnreachable {
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
        // A redirect that came back as the answer of a client that follows
        // them was stopped by the guard (`crate::guard`).
        let stopped = (follows && matches!(status, 301 | 302 | 303 | 307 | 308))
            .then(|| headers.get(reqwest::header::LOCATION))
            .flatten()
            .and_then(|l| l.to_str().ok())
            .and_then(|l| sent_to.join(l).ok())
            .map(|to| format!("from {} to {}", origin(&sent_to), origin(&to)));
        let mut response = response;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(AtalantaError::HttpUnreachable {
                    request: label.clone(),
                    detail: format!(
                        "the response body is over {} MiB; Atalanta reads at most that much",
                        MAX_BODY_BYTES / (1024 * 1024)
                    ),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let exchange = Response::new(
            status,
            headers
                .iter()
                .map(|(n, v)| {
                    (
                        n.as_str().to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            String::from_utf8_lossy(&bytes).into_owned(),
            started.elapsed().as_millis() as u64,
        )
        .stopped(stopped);
        tracing::debug!(
            "http {label} answered {status} in {} ms: {}",
            exchange.elapsed_ms,
            excerpt(&exchange.body)
        );
        Ok((exchange, label))
    }
}

/// `scheme://host[:port]` of a url.
fn origin(u: &reqwest::Url) -> String {
    u.origin().ascii_serialization()
}

/// The verdict on one response: what it captured, or the checks it failed.
fn judge(mut res: Response, req: &HttpRequest, label: String) -> Result<Passed> {
    let exchange::Evaluation { failures, captured } = exchange::evaluate(req, &mut res);
    // A token taken from a login response is as secret as one from
    // the environment. Registered before a failure is reported too: its
    // message quotes the start of this very body.
    for (name, value) in &captured {
        if atalanta_common::redact::is_secret_name(name) {
            atalanta_common::redact::register_secret(value);
            atalanta_common::redact::register_secret_name(name);
        }
    }
    if !failures.is_empty() {
        return Err(AtalantaError::HttpCheckFailed {
            request: label,
            status: res.status,
            failures,
            body: excerpt(&res.body),
        });
    }
    Ok(Passed {
        captured,
        summary: format!("{label} answered {} in {} ms", res.status, res.elapsed_ms),
    })
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
