//! `HttpSession` against a real HTTP server on 127.0.0.1: what goes on the
//! wire, the cookie jar, captures, every failed check at once, `retry:`,
//! redirects, timeouts and a server that isn't there.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use velocity_api::HttpSession;
use velocity_common::{HttpConfig, HttpRequest, VelocityError};

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A test API: one request per connection, answered by `route`.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let flaky = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let flaky = flaky.clone();
            std::thread::spawn(move || handle(conn, &flaky));
        }
    });
    format!("http://{addr}")
}

fn handle(mut conn: TcpStream, flaky: &AtomicUsize) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':').unwrap();
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    let req = Request {
        method,
        target,
        headers,
        body: String::from_utf8(body).unwrap(),
    };
    let (status, extra, body) = route(&req, flaky);
    let mut out = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&body);
    let _ = conn.write_all(out.as_bytes());
}

type Reply = (u16, Vec<(&'static str, String)>, String);

fn json_reply(status: u16, v: Value) -> Reply {
    (
        status,
        vec![("content-type", "application/json".into())],
        v.to_string(),
    )
}

trait TapBody {
    fn tap_body(self, body: &str) -> Self;
}

impl TapBody for Reply {
    fn tap_body(mut self, body: &str) -> Self {
        self.2 = body.to_string();
        self
    }
}

fn route(req: &Request, flaky: &AtomicUsize) -> Reply {
    let path = req.target.split('?').next().unwrap();
    match (req.method.as_str(), path) {
        ("POST", "/login") => {
            let creds: Value = serde_json::from_str(&req.body).unwrap_or_default();
            if creds != json!({ "user": "ada", "password": "pw" }) {
                return json_reply(401, json!({ "error": "bad credentials" }));
            }
            let (s, mut h, b) = json_reply(
                200,
                json!({ "token": "tok-0123456789abcdef", "user": { "name": "Ada", "roles": ["dev", "admin"] } }),
            );
            h.push(("set-cookie", "sid=abc123; Path=/".into()));
            (s, h, b)
        }
        ("GET", "/me") => match req.header("cookie") {
            Some(c) if c.contains("sid=abc123") => json_reply(200, json!({ "name": "Ada" })),
            _ => json_reply(401, json!({ "error": "no session" })),
        },
        ("POST", "/orders") => {
            let (s, mut h, b) = json_reply(
                201,
                json!({ "id": "o-7", "items": [{ "sku": "A1", "qty": 2 }, { "sku": "B2", "qty": 1 }], "total": 12.5 }),
            );
            h.push(("location", "/orders/o-7".into()));
            (s, h, b)
        }
        ("GET", "/flaky") => {
            if flaky.fetch_add(1, Ordering::SeqCst) < 2 {
                json_reply(503, json!({ "ready": false }))
            } else {
                json_reply(200, json!({ "ready": true }))
            }
        }
        ("GET", "/redirect") => (302, vec![("location", "/me".into())], String::new()),
        (_, "/hop") => {
            let to = req.target.split_once("to=").map_or("/", |(_, t)| t);
            (307, vec![("location", to.to_string())], String::new())
        }
        ("GET", "/slow") => {
            std::thread::sleep(Duration::from_millis(400));
            json_reply(200, json!({}))
        }
        ("GET", "/bom") => (
            200,
            vec![("content-type", "application/json".into())],
            "\u{feff}{\"id\": 7}".into(),
        ),
        ("POST", "/session") => json_reply(200, json!({ "sid": "zq9-sid-5f3a7c1e8b2d4096" })),
        ("GET", "/big") => json_reply(200, json!({ "ok": true }))
            .tap_body(r#"{"id": 12345678901234567890123, "small": 9007199254740993}"#),
        ("GET", "/text") => (
            200,
            vec![("content-type", "text/plain".into())],
            "hello world".into(),
        ),
        _ => json_reply(
            if path == "/echo" { 200 } else { 404 },
            json!({
                "method": req.method,
                "target": req.target,
                "contentType": req.header("content-type"),
                "auth": req.header("authorization"),
                "client": req.header("x-client"),
                "referer": req.header("referer"),
                "body": req.body,
            }),
        ),
    }
}

fn step(yaml: &str) -> HttpRequest {
    serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{e}\n{yaml}"))
}

fn session(base: &str) -> HttpSession {
    HttpSession::new(&HttpConfig {
        base_url: Some(base.to_string()),
        headers: vec![
            ("Authorization".into(), "Bearer suite-token".into()),
            ("X-Client".into(), "velocity".into()),
        ],
        ..Default::default()
    })
    .unwrap()
}

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_login_captures_its_token_and_its_cookie_goes_with_the_next_request() {
    let s = session(&serve());
    let passed = s
        .run(
            &step(
                "method: POST\nurl: /login\njson: { user: ada, password: pw }\n\
                 expect:\n  status: 200\n  headers: { content-type: { contains: json } }\n\
                 \x20 json:\n    user.name: Ada\n    $.user.roles: { contains: admin, length: 2 }\n\
                 capture:\n  ACCESS_TOKEN: token\n  FIRST_ROLE: \"$.user.roles[0]\"\n  ROLES: user.roles\n",
            ),
            T,
        )
        .await
        .unwrap();
    assert_eq!(
        passed.captured,
        vec![
            (
                "ACCESS_TOKEN".to_string(),
                "tok-0123456789abcdef".to_string()
            ),
            ("FIRST_ROLE".to_string(), "dev".to_string()),
            ("ROLES".to_string(), "[\"dev\",\"admin\"]".to_string()),
        ]
    );
    assert!(
        passed.summary.starts_with("POST http://127.0.0.1:"),
        "{}",
        passed.summary
    );
    // A secret-named capture is masked from now on.
    assert!(!velocity_common::redact::redact("t=tok-0123456789abcdef").contains("0123456789"));
    // The session cookie came along.
    s.run(
        &step("url: /me\nexpect: { status: 200, json: { name: Ada } }"),
        T,
    )
    .await
    .unwrap();
    // Another test's session starts without it.
    let fresh = session(&s_base(&passed.summary));
    let e = fresh.run(&step("url: /me"), T).await.unwrap_err();
    assert!(e.to_string().contains("answered 401"), "{e}");
}

/// The server's base url, out of a summary line.
fn s_base(summary: &str) -> String {
    let url = summary.split_whitespace().nth(1).unwrap();
    url.split('/').take(3).collect::<Vec<_>>().join("/")
}

#[tokio::test]
async fn every_failed_check_is_listed_with_the_body() {
    let s = session(&serve());
    let e = s
        .run(
            &step(
                "method: POST\nurl: /orders\nexpect:\n  status: 200\n  headers: { location: /orders/o-8 }\n\
                 \x20 json:\n    id: o-7\n    total: { gt: 20 }\n    $.items[*].sku: { contains: C3 }\n    $.missing: { exists: true }\n\
                 \x20 maxMs: 60000\ncapture: { NOPE: \"$.nope\" }\n",
            ),
            T,
        )
        .await
        .unwrap_err();
    let VelocityError::HttpCheckFailed {
        request,
        status,
        failures,
        body,
    } = &e
    else {
        panic!("{e:?}")
    };
    assert!(request.starts_with("POST http://127.0.0.1:") && request.ends_with("/orders"));
    assert_eq!(*status, 201);
    assert_eq!(
        failures,
        &[
            "status: expected 200, got 201",
            "header location: expected \"/orders/o-8\", got \"/orders/o-7\"",
            "total: expected > 20, got 12.5",
            "$.items[*].sku: expected to contain \"C3\", got [\"A1\",\"B2\"]",
            "$.missing: expected something, got nothing",
            "capture NOPE: `$.nope` matched nothing",
        ]
    );
    assert!(body.starts_with("{\"id\":\"o-7\""), "{body}");
    assert_eq!(e.exit_code(), 1);
    assert!(
        e.to_string()
            .contains("answered 201: 6 checks failed: status: expected 200"),
        "{e}"
    );
}

#[tokio::test]
async fn without_expect_status_an_error_status_fails() {
    let s = session(&serve());
    let e = s.run(&step("url: /nothing-here"), T).await.unwrap_err();
    assert!(
        e.to_string()
            .contains("status: expected below 400 (no expect.status), got 404"),
        "{e}"
    );
    s.run(&step("url: /nothing-here\nexpect: { status: 4xx }"), T)
        .await
        .unwrap();
}

#[tokio::test]
async fn what_is_sent_is_what_was_written() {
    let s = session(&serve());
    let echo = |yaml: &str| step(&format!("url: /echo?first=1\n{yaml}"));
    // Query after the url's own, in order; the step's header wins over the
    // suite's of the same name (any case).
    s.run(
        &echo(
            "query: { b: 2, a: x y }\nheaders: { authorization: Bearer step-token }\n\
             expect:\n  json:\n    target: /echo?first=1&b=2&a=x+y\n    auth: Bearer step-token\n    client: velocity\n",
        ),
        T,
    )
    .await
    .unwrap();
    s.run(
        &echo("method: put\nform: { n: 1, s: a&b }\nexpect: { json: { method: PUT, contentType: application/x-www-form-urlencoded, body: n=1&s=a%26b } }"),
        T,
    )
    .await
    .unwrap();
    s.run(
        &echo("method: PATCH\njson: { a: [1, true] }\nexpect: { json: { contentType: application/json, body: '{\"a\":[1,true]}' } }"),
        T,
    )
    .await
    .unwrap();
    s.run(
        &echo("method: POST\nbody: raw text\nheaders: { Content-Type: text/plain }\nexpect: { json: { body: raw text, contentType: text/plain } }"),
        T,
    )
    .await
    .unwrap();
    // A body that isn't JSON fails a JSON check, saying so; text checks work.
    let e = s
        .run(&step("url: /text\nexpect: { json: { a: 1 } }"), T)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("the body is not JSON (text/plain)"),
        "{e}"
    );
    let p = s
        .run(
            &step("url: /text\nexpect: { body: { matches: \"hello \\\\w+\" } }\ncapture: { WHO: { regex: \"hello (\\\\w+)\" } }"),
            T,
        )
        .await
        .unwrap();
    assert_eq!(p.captured, vec![("WHO".to_string(), "world".to_string())]);
}

#[tokio::test]
async fn retry_sends_again_until_the_expectations_pass() {
    let s = session(&serve());
    let flaky = "url: /flaky\nexpect: { json: { ready: true } }\n";
    // Without retry it fails at once; with it, the third request passes.
    assert!(s.run(&step(flaky), T).await.is_err());
    s.run(
        &step(&format!("{flaky}retry: {{ count: 3, intervalMs: 10 }}")),
        T,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn redirects_are_followed_unless_the_step_checks_them() {
    let s = session(&serve());
    s.run(&step("url: /redirect\nexpect: { status: 401 }"), T)
        .await
        .unwrap();
    s.run(
        &step("url: /redirect\nfollowRedirects: false\nexpect: { status: 302, headers: { location: /me } }"),
        T,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_redirect_to_another_origin_does_not_carry_the_suites_headers() {
    // Audit 2026-11 API-1: reqwest keeps custom headers (X-Api-Key) across
    // hosts, so a request with headers of its own stops at a cross-origin
    // hop and the step sees the 3xx.
    let a = serve();
    let b = serve();
    let s = session(&a);
    s.run(
        &step(&format!(
            "url: /hop?to={b}/echo
expect: {{ status: 307 }}"
        )),
        T,
    )
    .await
    .unwrap();
    // The same origin is followed, headers and all.
    s.run(
        &step(&format!(
            "url: /hop?to={a}/echo
expect: {{ status: 200, json: {{ client: velocity }} }}"
        )),
        T,
    )
    .await
    .unwrap();
    // No headers of its own: followed anywhere.
    let bare = HttpSession::new(&HttpConfig {
        base_url: Some(a.clone()),
        ..Default::default()
    })
    .unwrap();
    bare.run(
        &step(&format!(
            "url: /hop?to={b}/echo
expect: {{ status: 200 }}"
        )),
        T,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn no_answer_is_a_test_failure_not_the_devices() {
    let s = session(&serve());
    let e = s
        .run(&step("url: /slow"), Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, VelocityError::HttpUnreachable { detail, .. } if detail == "timed out after 100 ms"),
        "{e:?}"
    );
    assert_eq!(e.exit_code(), 1);
    assert!(!e.is_transient());
    // A port nobody listens on.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let e = HttpSession::new(&HttpConfig::default())
        .unwrap()
        .run(&step(&format!("url: http://{closed}/x")), T)
        .await
        .unwrap_err();
    assert!(matches!(e, VelocityError::HttpUnreachable { .. }), "{e:?}");
    // No base url for a relative one: a configuration error, never retried.
    let e = HttpSession::new(&HttpConfig::default())
        .unwrap()
        .run(&step("url: /x\nretry: { count: 50, intervalMs: 1000 }"), T)
        .await
        .unwrap_err();
    assert_eq!(e.exit_code(), 2, "{e}");
}

#[tokio::test]
async fn a_json_body_with_a_byte_order_mark_is_json() {
    let s = session(&serve());
    let p = s
        .run(
            &step("url: /bom\nexpect: { json: { id: 7 } }\ncapture: { ID: id }"),
            T,
        )
        .await
        .unwrap();
    assert_eq!(p.captured, vec![("ID".to_string(), "7".to_string())]);
}

#[tokio::test]
async fn a_failed_step_does_not_print_the_secret_it_captured() {
    // The failure message quotes the start of the body: the token that
    // `capture` took from it must be masked there too.
    let s = session(&serve());
    let e = s
        .run(
            &step("method: POST\nurl: /session\nexpect: { status: 201 }\ncapture: { SESSION_TOKEN: sid }"),
            T,
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("zq9-sid-5f3a7c1e8b2d4096"), "{e}");
    let shown = velocity_common::redact::redact(&e.to_string()).into_owned();
    assert!(!shown.contains("zq9-sid-5f3a7c1e8b2d4096"), "{shown}");
}

#[tokio::test]
async fn a_redirect_carries_no_referer() {
    let a = serve();
    let bare = HttpSession::new(&HttpConfig {
        base_url: Some(a.clone()),
        ..Default::default()
    })
    .unwrap();
    bare.run(
        &step("url: '/hop?to=/echo'\nexpect: { status: 200, json: { referer: null } }"),
        T,
    )
    .await
    .unwrap();
}

/// A server that counts its connections and answers 200.
fn counting() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let n = hits.clone();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            n.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim_end().is_empty() {
                    break;
                }
            }
            let _ =
                conn.write_all(b"HTTP/1.1 200 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    (format!("http://{addr}"), hits)
}

#[tokio::test]
async fn a_login_body_is_not_replayed_to_another_origin() {
    // A 307 replays its body: a login's password with no headers at all.
    let (elsewhere, hits) = counting();
    let a = serve();
    let bare = HttpSession::new(&HttpConfig {
        base_url: Some(a.clone()),
        ..Default::default()
    })
    .unwrap();
    let hop = format!(
        "method: POST\nurl: '/hop?to={elsewhere}/steal'\njson: {{ user: ada, password: pw }}\n"
    );
    let e = bare.run(&step(&hop), T).await.unwrap_err().to_string();
    assert!(e.contains("not followed"), "{e}");
    assert!(e.contains(&format!("from {a} to {elsewhere}")), "{e}");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the body reached the other origin"
    );
    // A step that tests the redirect itself passes; so does a bare GET that
    // carries nothing to protect.
    bare.run(&step(&format!("{hop}expect: {{ status: 307 }}")), T)
        .await
        .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    bare.run(&step(&format!("url: '/hop?to={elsewhere}/x'")), T)
        .await
        .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_redirect_the_guard_stopped_is_not_a_pass() {
    let (elsewhere, hits) = counting();
    let s = session(&serve());
    let e = s
        .run(&step(&format!("url: '/hop?to={elsewhere}/x'")), T)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("answered 307") && e.contains("not followed"),
        "{e}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    // `followRedirects: false` asks for the 3xx: that one passes.
    s.run(
        &step(&format!(
            "url: '/hop?to={elsewhere}/x'\nfollowRedirects: false"
        )),
        T,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_number_beyond_u64_is_refused_not_rounded() {
    let s = session(&serve());
    let e = s
        .run(&step("url: /big\ncapture: { ID: id }"), T)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("too large to capture exactly"), "{e}");
    let p = s
        .run(&step("url: /big\ncapture: { N: small }"), T)
        .await
        .unwrap();
    assert_eq!(p.captured[0].1, "9007199254740993");
}

#[tokio::test]
async fn max_ms_fails_a_slow_answer_and_passes_a_fast_one() {
    // `every_failed_check_is_listed_with_the_body` only gives it room.
    let s = session(&serve());
    let e = s
        .run(&step("url: /slow\nexpect: { maxMs: 50 }"), T)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("more than maxMs 50"), "{e}");
    s.run(&step("url: /slow\nexpect: { maxMs: 5000 }"), T)
        .await
        .unwrap();
}
