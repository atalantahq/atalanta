//! Shared fixtures for the headless-Chrome tests: a tiny HTTP server on
//! 127.0.0.1 serving fixed pages, and a driver launched against it.
//!
//! The tests drive the CLI's own headless Chrome (a fresh profile, never the
//! user's). Without a Chrome binary they skip: set `VELOCITY_TEST_WEB=0` to
//! skip them explicitly.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use velocity_common::{Element, PlatformDriver, Selector};
use velocity_web::{WebDriver, WebDriverConfig};

/// Serves `pages` (path -> HTML) on an ephemeral 127.0.0.1 port, for the life
/// of the test process. Returns the base URL.
pub fn serve(pages: &[(&str, &str)]) -> String {
    let pages: Arc<HashMap<String, String>> = Arc::new(
        pages
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                // Drain the headers.
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/");
                let path = path.split(['?', '#']).next().unwrap_or("/");
                // `/slow…` answers after 3 s (a slow navigation).
                if path.starts_with("/slow") {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                let (status, body) = match pages.get(path) {
                    Some(b) => ("200 OK", b.as_str()),
                    None => ("404 Not Found", "not found"),
                };
                // Scripts as scripts: a service worker is refused otherwise.
                let kind = if path.ends_with(".js") {
                    "application/javascript"
                } else if path.ends_with(".json") {
                    "application/json"
                } else {
                    "text/html; charset=utf-8"
                };
                let _ = write!(
                    &stream,
                    "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// The Chrome to run these tests with: `CHROME_PATH` or `CHROME` when set,
/// else the usual install locations (snap's too).
pub fn chrome_path() -> Option<String> {
    if matches!(
        std::env::var("VELOCITY_TEST_WEB").as_deref(),
        Ok("0") | Ok("false")
    ) {
        return None;
    }
    let from_env = ["CHROME_PATH", "CHROME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|p| !p.is_empty());
    if let Some(p) = from_env {
        return std::path::Path::new(&p).exists().then_some(p);
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ]
    .iter()
    .find(|p| std::path::Path::new(p).exists())
    .map(|p| p.to_string())
}

/// Whether a Chrome binary is around to run these tests.
pub fn chrome_available() -> bool {
    chrome_path().is_some()
}

/// Polls `check` (every 50 ms, up to 3 s) until it holds; whether it did.
pub async fn eventually<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..60 {
        if check().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// Polls `check` (every 50 ms) until it holds or `limit` of wall-clock
/// time has passed; whether it did. For waits on something outside the
/// test's own process (a killed Chrome going away, a garbage collection),
/// where a loaded machine stretches the wait: [`eventually`] counts tries,
/// not time.
pub async fn eventually_for<F, Fut>(limit: std::time::Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let end = std::time::Instant::now() + limit;
    loop {
        if check().await {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Waits (as [`eventually`]) until `s` is found, in the page or one of
/// its frames: a frame that is still loading, instead of a fixed sleep
/// (review 07 M9). Whether it was.
pub async fn wait_found(d: &WebDriver, s: &str) -> bool {
    eventually(|| async { d.find_element("web", &css(s)).await.is_ok() }).await
}

/// Whether a missing Chrome must fail the tests instead of skipping them:
/// on a CI server, or with `VELOCITY_TEST_STRICT=1`, unless
/// `VELOCITY_TEST_WEB=0` turned the web tests off on purpose. A runner
/// image that lost Chrome turned ~150 tests into vacuous passes
/// (review 07 M6).
pub fn chrome_required() -> bool {
    if matches!(
        std::env::var("VELOCITY_TEST_WEB").as_deref(),
        Ok("0") | Ok("false")
    ) {
        return false;
    }
    let strict =
        std::env::var("VELOCITY_TEST_STRICT").is_ok_and(|v| velocity_common::env::ci_flag(&v));
    strict || velocity_common::env::in_ci_with(|k| std::env::var(k).ok())
}

/// Skips the calling test when there is no Chrome; fails it instead when
/// Chrome is required ([`chrome_required`]).
#[macro_export]
macro_rules! require_chrome {
    () => {
        if !common::chrome_available() {
            if common::chrome_required() {
                panic!(
                    "no Chrome binary on a CI server (or with VELOCITY_TEST_STRICT=1): \
                     install Chrome, set CHROME_PATH, or VELOCITY_TEST_WEB=0 to skip"
                );
            }
            eprintln!("skipped: no Chrome binary (or VELOCITY_TEST_WEB=0)");
            return;
        }
    };
}

/// A prepared headless driver, sandbox off for CI containers.
pub async fn driver(mocks: Vec<velocity_common::MockRule>) -> WebDriver {
    let d = WebDriver::new(WebDriverConfig {
        mocks,
        chrome_path: chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    });
    d.prepare("web").await.expect("Chrome starts");
    d
}

pub fn css(s: &str) -> Selector {
    Selector::Css(s.into())
}

pub async fn find(d: &WebDriver, s: &str) -> Element {
    d.find_element("web", &css(s))
        .await
        .unwrap_or_else(|e| panic!("find {s}: {e}"))
}

/// The live text of the element `s` matches.
pub async fn text(d: &WebDriver, s: &str) -> String {
    let el = find(d, s).await;
    d.get_element_text("web", &el).await.unwrap()
}

/// Polls `s`'s text until it equals `want` (pages update asynchronously,
/// and a page still being replaced has no `s` for a moment: that is waited
/// out, not a failure). The last text read; a lookup that never worked
/// panics with its error.
pub async fn wait_text(d: &WebDriver, s: &str, want: &str) -> String {
    let mut got = None;
    let mut last_err = None;
    for _ in 0..60 {
        let read = match d.find_element("web", &css(s)).await {
            Ok(el) => d.get_element_text("web", &el).await,
            Err(e) => Err(e),
        };
        match read {
            Ok(t) => {
                let done = t == want;
                got = Some(t);
                if done {
                    break;
                }
            }
            Err(e) => last_err = Some(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    match (got, last_err) {
        (Some(t), _) => t,
        (None, Some(e)) => panic!("find {s}: {e}"),
        (None, None) => String::new(),
    }
}
