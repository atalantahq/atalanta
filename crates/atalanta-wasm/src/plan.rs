// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The playground's plan view and its in-browser runner for API-only suites.
//! JavaScript sends the request (`fetch`); the request is built and the
//! response judged by `atalanta_api::exchange`, the code `atalanta run`
//! uses, so the browser applies the same rules.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use atalanta_api::exchange::{self, Response};
use atalanta_common::{Action, AtalantaError, Platform, Step, TestCase, TestSuite};
use atalanta_core::{
    env, http_only_tests, parse_suite_from_str, resolve_flows, source_map, step_label,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::check;

struct Loaded {
    suite: TestSuite,
    tests: Vec<TestCase>,
    test_lines: Vec<Option<usize>>,
}

fn load(yaml: &str) -> Result<Loaded, String> {
    if let Some(d) = check(yaml).into_iter().find(|d| d.severity == "error") {
        return Err(match d.line {
            Some(line) => format!("line {line}: {}", d.message),
            None => d.message,
        });
    }
    let mut suite = parse_suite_from_str(yaml).map_err(|e| e.to_string())?;
    suite.config.captured_vars = env::captured_names(&suite);
    let tests = resolve_flows(&suite).map_err(|e| e.to_string())?;
    let text = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let locations = source_map::locate(text).unwrap_or_default();
    let test_lines = (0..suite.tests.len())
        .map(|i| locations.tests.get(i)?.at.map(|m| m.line))
        .collect();
    Ok(Loaded {
        suite,
        tests,
        test_lines,
    })
}

fn step_kind(action: &Action) -> &'static str {
    match action {
        Action::Http(_) => "http",
        Action::Wait { .. } => "wait",
        Action::SetConfig { .. } => "setConfig",
        _ => "other",
    }
}

#[derive(Serialize)]
struct PlanStep {
    index: usize,
    line: Option<usize>,
    label: String,
    kind: &'static str,
    /// The step as the suite model serializes it, for the playground's simulation.
    action: Value,
}

#[derive(Serialize)]
struct PlanTest {
    name: String,
    line: Option<usize>,
    steps: Vec<PlanStep>,
}

fn base_url(suite: &TestSuite) -> Option<String> {
    let raw = suite.config.http.as_ref()?.base_url.as_deref()?;
    let none = BTreeSet::new();
    Some(env::interpolate_deferring(raw, &HashMap::new(), &none).unwrap_or_else(|_| raw.into()))
}

pub fn plan(yaml: &str) -> Value {
    let l = match load(yaml) {
        Ok(l) => l,
        Err(error) => {
            return json!({
                "ok": false, "error": error, "appId": null, "platform": null,
                "target": {"kind": null, "url": null, "baseUrl": null},
                "runnable": false, "tests": []
            })
        }
    };
    let runnable = http_only_tests(&l.tests);
    let base = base_url(&l.suite);
    let (platform, kind, url) = match l.suite.config.platform {
        Some(Platform::Web) => ("web", Some("web"), Some(l.suite.app_id.clone())),
        Some(Platform::Android) => ("android", Some("android"), None),
        Some(Platform::Ios) => ("ios", Some("ios"), None),
        None if runnable => ("headless", Some("api"), None),
        None => ("", None, None),
    };
    let tests: Vec<PlanTest> = l
        .tests
        .iter()
        .zip(&l.test_lines)
        .map(|(t, line)| PlanTest {
            name: t.name.clone(),
            line: *line,
            steps: t
                .steps
                .iter()
                .enumerate()
                .map(|(index, s)| PlanStep {
                    index,
                    line: s.source.as_ref().map(|p| p.line).filter(|n| *n > 0),
                    label: step_label(&s.action),
                    kind: step_kind(&s.action),
                    action: serde_json::to_value(&s.action).unwrap_or(Value::Null),
                })
                .collect(),
        })
        .collect();
    json!({
        "ok": true,
        "error": null,
        "appId": (!l.suite.app_id.is_empty()).then_some(&l.suite.app_id),
        "platform": (!platform.is_empty()).then_some(platform),
        "target": {"kind": kind, "url": url, "baseUrl": base},
        "runnable": runnable,
        "tests": tests,
    })
}

fn missing(e: AtalantaError) -> String {
    match e {
        AtalantaError::MissingEnvVars { vars } => {
            let names: Vec<String> = vars
                .iter()
                .map(|v| format!("${{{}}}", v.split(':').next().unwrap_or(v)))
                .collect();
            format!("needs {}, which the browser doesn't have", names.join(", "))
        }
        other => other.to_string(),
    }
}

fn parse_vars(vars_json: &str) -> Result<HashMap<String, String>, String> {
    serde_json::from_str(vars_json).map_err(|e| format!("vars_json: {e}"))
}

/// The step with environment defaults and captured values filled in, and
/// the suite's `config.http` with its own placeholders filled.
fn bound_step(
    l: &Loaded,
    test_index: usize,
    step_index: usize,
    vars: &HashMap<String, String>,
) -> Result<(Step, atalanta_common::HttpConfig), String> {
    let step = l
        .tests
        .get(test_index)
        .ok_or_else(|| format!("no test {test_index}"))?
        .steps
        .get(step_index)
        .ok_or_else(|| format!("no step {step_index} in test {test_index}"))?;
    let none = HashMap::new();
    let deferred = &l.suite.config.captured_vars;
    let mut config = l.suite.config.http.clone().unwrap_or_default();
    if let Some(base) = config.base_url.as_mut() {
        *base = env::interpolate_deferring(base, &none, deferred).map_err(missing)?;
    }
    for (_, v) in config.headers.iter_mut() {
        *v = env::interpolate_deferring(v, &none, deferred).map_err(missing)?;
    }
    let step = env::interpolate_step(step, &none, deferred).map_err(missing)?;
    let mut names = deferred.clone();
    names.extend(vars.keys().cloned());
    let step = env::bind_step(&step, vars, &names)
        .map_err(|e| e.to_string())?
        .unwrap_or(step);
    Ok((step, config))
}

pub fn prepare_step(yaml: &str, test_index: usize, step_index: usize, vars_json: &str) -> Value {
    let run = || -> Result<Value, String> {
        let vars = parse_vars(vars_json)?;
        let l = load(yaml)?;
        let (step, config) = bound_step(&l, test_index, step_index, &vars)?;
        match &step.action {
            Action::Http(req) => {
                let p =
                    exchange::build(&config, req, step.timeout_ms).map_err(|e| e.to_string())?;
                let headers: BTreeMap<_, _> = p.headers.into_iter().collect();
                let retry = req.retry.as_ref().map(|r| {
                    json!({
                        "count": r.count,
                        "intervalMs": r.interval_ms.unwrap_or(exchange::DEFAULT_RETRY_INTERVAL_MS),
                    })
                });
                Ok(json!({
                    "kind": "http",
                    "request": {
                        "method": p.method, "url": p.url, "headers": headers,
                        "body": p.body, "timeoutMs": p.timeout_ms,
                    },
                    "retry": retry,
                }))
            }
            Action::Wait { ms } => Ok(json!({"kind": "wait", "ms": ms})),
            Action::SetConfig { .. } => {
                Ok(json!({"kind": "skip", "reason": "setConfig has no effect in the browser"}))
            }
            other => Err(format!("{} needs a device", step_label(other))),
        }
    };
    run().unwrap_or_else(|message| json!({"kind": "error", "message": message}))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseIn {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    elapsed_ms: u64,
}

pub fn check_step(
    yaml: &str,
    test_index: usize,
    step_index: usize,
    vars_json: &str,
    response_json: &str,
) -> Value {
    let run = || -> Result<exchange::Evaluation, String> {
        let vars = parse_vars(vars_json)?;
        let r: ResponseIn =
            serde_json::from_str(response_json).map_err(|e| format!("response_json: {e}"))?;
        let l = load(yaml)?;
        let (step, _) = bound_step(&l, test_index, step_index, &vars)?;
        let Action::Http(req) = &step.action else {
            return Err(format!("{} is not an http step", step_label(&step.action)));
        };
        let mut res = Response::new(
            r.status,
            r.headers.into_iter().collect(),
            r.body,
            r.elapsed_ms,
        );
        Ok(exchange::evaluate(req, &mut res))
    };
    match run() {
        Ok(exchange::Evaluation { failures, captured }) => {
            let captured: BTreeMap<_, _> = captured.into_iter().collect();
            json!({"passed": failures.is_empty(), "failures": failures, "captured": captured})
        }
        Err(message) => json!({"passed": false, "failures": [message], "captured": {}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../examples/{name}/atalanta.yaml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    const CHAIN: &str = r#"
appId: x
config:
  http:
    baseUrl: ${BASE:-https://api.test}
tests:
  - name: chain
    steps:
      - http:
          method: POST
          url: /orders
          json: { item: pen }
          expect: { status: 201 }
          capture: { ORDER_ID: { json: $.id } }
      - http:
          url: /orders/${ORDER_ID}
      - wait: { ms: 5 }
"#;

    #[test]
    fn the_api_example_plan() {
        let p = plan(&example("api-example"));
        assert_eq!(p["ok"], true, "{p}");
        assert_eq!(p["runnable"], true);
        assert_eq!(p["platform"], "headless");
        assert_eq!(p["target"]["kind"], "api");
        assert_eq!(
            p["target"]["baseUrl"],
            "https://jsonplaceholder.typicode.com"
        );
        let tests = p["tests"].as_array().unwrap();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0]["steps"].as_array().unwrap().len(), 1);
        assert_eq!(tests[1]["steps"].as_array().unwrap().len(), 2);
        assert_eq!(tests[0]["line"], 10);
        assert_eq!(tests[0]["steps"][0]["line"], 12);
        assert_eq!(tests[0]["steps"][0]["label"], "http(GET /users/1)");
        assert_eq!(tests[1]["steps"][1]["line"], 34);
    }

    #[test]
    fn device_examples_are_not_runnable() {
        let a = plan(&example("android-example"));
        assert_eq!(a["ok"], true, "{a}");
        assert_eq!(a["runnable"], false);
        assert_eq!(a["target"]["kind"], "android");
        let w = plan(&example("web-example"));
        assert_eq!(w["runnable"], false);
        assert_eq!(w["target"]["kind"], "web");
        assert!(w["target"]["url"].as_str().unwrap().starts_with("data:"));
        assert_eq!(w["tests"][0]["steps"][0]["kind"], "other");
    }

    #[test]
    fn a_broken_suite_is_not_ok() {
        let p = plan("appId: x\ntests: 5");
        assert_eq!(p["ok"], false);
        assert!(p["error"].as_str().is_some());
    }

    #[test]
    fn the_first_api_step_is_prepared() {
        let p = prepare_step(&example("api-example"), 0, 0, "{}");
        assert_eq!(p["kind"], "http");
        assert_eq!(p["request"]["method"], "GET");
        assert_eq!(
            p["request"]["url"],
            "https://jsonplaceholder.typicode.com/users/1"
        );
        assert_eq!(p["request"]["timeoutMs"], 10000);
        assert!(p["request"]["body"].is_null());
        let post = prepare_step(&example("api-example"), 1, 0, "{}");
        assert_eq!(post["request"]["method"], "POST");
        assert_eq!(
            post["request"]["headers"]["content-type"],
            "application/json"
        );
        assert!(post["request"]["body"]
            .as_str()
            .unwrap()
            .contains("Hello from Atalanta"));
    }

    #[test]
    fn a_response_is_checked_with_the_shared_rules() {
        let y = example("api-example");
        let ok = r#"{"status":200,"headers":{"Content-Type":"application/json"},"body":"{\"id\":1,\"name\":\"Leanne Graham\",\"email\":\"a@b.c\",\"address\":{\"geo\":{\"lat\":\"1\"}}}","elapsedMs":12}"#;
        let r = check_step(&y, 0, 0, "{}", ok);
        assert_eq!(r["passed"], true, "{r}");
        let bad = r#"{"status":500,"headers":{},"body":"oops","elapsedMs":1}"#;
        let r = check_step(&y, 0, 0, "{}", bad);
        assert_eq!(r["passed"], false);
        assert!(r["failures"][0]
            .as_str()
            .unwrap()
            .contains("status: expected 200, got 500"));
    }

    #[test]
    fn a_capture_flows_into_the_next_step() {
        let created = r#"{"status":201,"headers":{},"body":"{\"id\": 42}","elapsedMs":3}"#;
        let r = check_step(CHAIN, 0, 0, "{}", created);
        assert_eq!(r["passed"], true, "{r}");
        assert_eq!(r["captured"]["ORDER_ID"], "42");
        let p = prepare_step(CHAIN, 0, 1, r#"{"ORDER_ID":"42"}"#);
        assert_eq!(p["request"]["url"], "https://api.test/orders/42", "{p}");
        let early = prepare_step(CHAIN, 0, 1, "{}");
        assert_eq!(early["kind"], "error");
        assert_eq!(
            prepare_step(CHAIN, 0, 2, "{}"),
            json!({"kind": "wait", "ms": 5})
        );
        assert_eq!(prepare_step(CHAIN, 0, 9, "{}")["kind"], "error");
    }

    #[test]
    fn a_missing_variable_is_an_error_and_a_default_is_used() {
        let y = "appId: x\nconfig:\n  http: { baseUrl: \"https://a.b\" }\ntests:\n  - name: t\n    steps:\n      - http: { url: \"/x?k=${API_KEY_NOT_SET_XYZ}\" }\n";
        let p = prepare_step(y, 0, 0, "{}");
        assert_eq!(p["kind"], "error");
        assert_eq!(
            p["message"],
            "needs ${API_KEY_NOT_SET_XYZ}, which the browser doesn't have"
        );
        let r = check_step(y, 0, 0, "{}", r#"{"status":200,"headers":{},"body":""}"#);
        assert_eq!(r["passed"], false);
    }
}
