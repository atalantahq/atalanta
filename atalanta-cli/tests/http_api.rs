//! `atalanta run` of an API suite end to end, against an HTTP server on
//! 127.0.0.1: no device, no `appId`, captures flowing between steps and
//! through a flow, every failed check in the report, a secret captured
//! from a login masked, and `--parallel`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn atalanta() -> &'static str {
    env!("CARGO_BIN_EXE_atalanta")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vhttp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A shop API: `POST /login` (a token and a session cookie), `POST
/// /orders` (needs the token), `GET /orders/<id>` (needs the cookie).
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || handle(conn));
        }
    });
    format!("http://{addr}")
}

fn handle(mut conn: TcpStream) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut words = line.split_whitespace();
    let (method, path) = (
        words.next().unwrap_or_default().to_string(),
        words.next().unwrap_or_default().to_string(),
    );
    let (mut len, mut auth, mut cookie) = (0usize, String::new(), String::new());
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        let h = h.trim_end().to_string();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':').unwrap();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = v.trim().parse().unwrap(),
            "authorization" => auth = v.trim().to_string(),
            "cookie" => cookie = v.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    let token = "tok-5ecret-0123456789";
    let (status, extra, out) = match (method.as_str(), path.as_str()) {
        ("POST", "/login") => (
            200,
            "set-cookie: sid=s1; Path=/\r\n",
            format!(r#"{{"token":"{token}"}}"#),
        ),
        ("POST", "/orders") if auth == format!("Bearer {token}") => {
            (201, "", r#"{"id":"o-42","total":12.5}"#.to_string())
        }
        ("GET", "/orders/o-42") if cookie.contains("sid=s1") => {
            (200, "", r#"{"id":"o-42","status":"paid"}"#.to_string())
        }
        ("GET", "/orders/o-42") => (401, "", r#"{"error":"no session"}"#.to_string()),
        _ => (404, "", r#"{"error":"not found"}"#.to_string()),
    };
    let _ = write!(
        conn,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{out}",
        out.len()
    );
}

const SUITE: &str = r#"
config:
  http:
    baseUrl: "${SHOP_API}"
flows:
  - id: login
    steps:
      - http:
          method: POST
          url: /login
          json: { user: ada }
          capture: { SHOP_TOKEN: token }
tests:
  - name: order is paid
    steps:
      - runFlow: login
      - http:
          method: POST
          url: /orders
          headers: { Authorization: "Bearer ${SHOP_TOKEN}" }
          expect: { status: 201, json: { total: { gt: 10 } } }
          capture: { ORDER_ID: id }
      - http:
          url: "/orders/${ORDER_ID}"
          expect: { json: { status: paid } }
  - name: order is shipped
    steps:
      - runFlow: login
      - http:
          url: /orders/o-42
          expect:
            status: 200
            json:
              status: shipped
              $.carrier: { exists: true }
  - name: without a session
    steps:
      - http: { url: /orders/o-42, expect: { status: 401 } }
"#;

fn run(dir: &Path, api: &str, extra: &[&str]) -> Output {
    Command::new(atalanta())
        .args(["run", "api.yaml", "--report", "json,junit"])
        .args(extra)
        .current_dir(dir)
        .env("SHOP_API", api)
        .env("ATALANTA_LEASE_DIR", dir.join("leases"))
        .env_remove("GITHUB_ACTIONS")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// (status, error message) of each test in a results.json.
fn outcomes(dir: &Path) -> Vec<(String, String, String)> {
    let raw = std::fs::read_to_string(dir.join("atalanta-results/results.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["test_name"].as_str().unwrap_or_default().to_string(),
                t["status"].as_str().unwrap_or_default().to_lowercase(),
                t["error_message"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn an_api_suite_runs_without_a_device() {
    let dir = scratch("seq");
    std::fs::write(dir.join("api.yaml"), SUITE).unwrap();
    let out = run(&dir, &serve(), &[]);
    let all = text(&out);
    // One test fails its checks: exit 1, not a device or config error.
    assert_eq!(out.status.code(), Some(1), "{all}");
    assert!(all.contains("Platform: none (http steps only)"), "{all}");
    assert!(!all.contains("Preparing"), "{all}");
    let o = outcomes(&dir);
    let get = |name: &str| {
        o.iter()
            .find(|t| t.0 == name)
            .unwrap_or_else(|| panic!("{name}: {o:?}"))
    };
    assert_eq!(get("order is paid").1, "passed", "{all}");
    assert_eq!(get("without a session").1, "passed", "{all}");
    let (_, status, error) = get("order is shipped");
    assert_eq!(status, "failed");
    // Every failed check, with the start of the body.
    assert!(
        error.contains("answered 200: 2 checks failed: status: expected \"shipped\", got \"paid\"; $.carrier: expected something, got nothing; body: {\"id\":\"o-42\",\"status\":\"paid\"}"),
        "{error}"
    );
    // The token captured from the login is a secret: no report keeps it.
    let reports = std::fs::read_to_string(dir.join("atalanta-results/results.json")).unwrap()
        + &std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap()
        + &all;
    assert!(!reports.contains("5ecret-0123456789"), "the token leaked");
}

#[test]
fn an_api_suite_runs_in_parallel_without_devices() {
    let dir = scratch("par");
    std::fs::write(dir.join("api.yaml"), SUITE).unwrap();
    let out = run(&dir, &serve(), &["--parallel", "2"]);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(1), "{all}");
    let o = outcomes(&dir);
    assert_eq!(o.len(), 3, "{all}");
    let passed = o.iter().filter(|t| t.1 == "passed").count();
    assert_eq!(passed, 2, "{o:?}\n{all}");
}

#[test]
fn a_ui_suite_still_needs_its_app_id() {
    let dir = scratch("appid");
    std::fs::write(
        dir.join("api.yaml"),
        "tests:\n  - name: t\n    steps:\n      - tap: { text: Pay }\n",
    )
    .unwrap();
    let out = run(&dir, "http://127.0.0.1:9", &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("the suite has no `appId`"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_typo_in_a_step_is_found_before_anything_is_sent() {
    let dir = scratch("typo");
    std::fs::write(
        dir.join("api.yaml"),
        "tests:\n  - name: t\n    steps:\n      - http: { url: /x, expect: { json: { \"$.a[\": 1 } } }\n",
    )
    .unwrap();
    let out = run(&dir, "http://127.0.0.1:9", &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("expect.json: JSONPath `$.a[`"),
        "{}",
        text(&out)
    );
}

/// `--env-file` (Maestro #2817): its values outrank the process
/// environment, `--env` outranks them, a secret-named one is masked like
/// an `--env` one, and a malformed line or a missing file is exit 2.
#[test]
fn env_files_feed_the_suite() {
    const ENV_SUITE: &str = r#"
config:
  http:
    baseUrl: "${SHOP_API}"
tests:
  - name: without a session
    steps:
      - http: { url: /orders/o-42, expect: { status: 401 } }
  - name: the error is the password
    steps:
      - http: { url: /orders/o-42, expect: { json: { error: "${SHOP_PASSWORD}" } } }
"#;
    let dir = scratch("envfile");
    std::fs::write(dir.join("api.yaml"), ENV_SUITE).unwrap();
    let api = serve();
    std::fs::write(
        dir.join("shop.env"),
        format!(
            "# the local shop\nexport SHOP_API=\"{api}\"  # not the env's\nSHOP_PASSWORD='hunter2-from-file'\n"
        ),
    )
    .unwrap();
    // The environment's SHOP_API (nothing listens on port 9) loses.
    let dead = "http://127.0.0.1:9";
    let out = run(&dir, dead, &["--env-file", "shop.env"]);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(1), "{all}");
    let o = outcomes(&dir);
    assert_eq!(o[0].1, "passed", "{o:?}\n{all}");
    assert_eq!(o[1].1, "failed", "{o:?}\n{all}");
    assert!(o[1].2.contains("[redacted]"), "{o:?}");
    let reports = std::fs::read_to_string(dir.join("atalanta-results/results.json")).unwrap()
        + &std::fs::read_to_string(dir.join("atalanta-results/junit.xml")).unwrap()
        + &all;
    assert!(
        !reports.contains("hunter2-from-file"),
        "the password leaked"
    );

    // `--env` outranks the file.
    let out = run(
        &dir,
        &api,
        &[
            "--env-file",
            "shop.env",
            "--env",
            &format!("SHOP_API={dead}"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(outcomes(&dir)[0].1, "failed", "{}", text(&out));

    std::fs::write(dir.join("bad.env"), "A=1\nnot a pair\n").unwrap();
    let out = run(&dir, &api, &["--env-file", "bad.env"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("--env-file bad.env:2: expected KEY=VALUE"),
        "{}",
        text(&out)
    );

    let out = run(&dir, &api, &["--env-file", "missing.env"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("--env-file missing.env: "),
        "{}",
        text(&out)
    );
}
