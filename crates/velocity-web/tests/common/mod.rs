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

/// Skips the calling test when there is no Chrome.
#[macro_export]
macro_rules! require_chrome {
    () => {
        if !common::chrome_available() {
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

/// Polls `s`'s text until it equals `want` (pages update asynchronously).
pub async fn wait_text(d: &WebDriver, s: &str, want: &str) -> String {
    let mut got = String::new();
    for _ in 0..60 {
        got = text(d, s).await;
        if got == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    got
}
