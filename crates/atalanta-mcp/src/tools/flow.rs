use std::sync::Arc;

use serde_json::Value;
use velocity_common::{PlatformDriver, Result, TestSuite, VelocityError};
use velocity_core::{parse_suite, resolve_flows, TestExecutor};

fn load_suite(config_path: &Option<String>) -> Result<TestSuite> {
    let path = config_path.as_deref().unwrap_or("velocity.yaml");
    parse_suite(path)
}

/// The suite as `velocity run` executes it: `${VAR}` filled in from the
/// server's environment, which also registers secret-looking values for
/// redaction. Without this `${PASSWORD}` was typed literally (VEL-088).
fn load_runnable_suite(config_path: &Option<String>) -> Result<TestSuite> {
    let mut suite = load_suite(config_path)?;
    velocity_core::env::interpolate_suite(&mut suite, &Default::default())?;
    Ok(suite)
}

pub fn list_flows(config_path: &Option<String>, _args: &Value) -> Result<Value> {
    let suite = load_suite(config_path)?;
    let flows: Vec<Value> = suite
        .flows
        .iter()
        .map(|f| {
            serde_json::json!({
                "id": f.id,
                "step_count": f.steps.len()
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": flows.len(),
        "flows": flows
    }))
}

pub async fn run_flow(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let flow_id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VelocityError::Config("Missing 'id' argument".to_string()))?;

    // `env`: the runFlow parameters, as a test's `runFlow: { flow, env }`.
    let mut env = std::collections::BTreeMap::new();
    if let Some(map) = args.get("env") {
        let map = map.as_object().ok_or_else(|| {
            VelocityError::Config("'env' must be an object of strings".to_string())
        })?;
        for (k, v) in map {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => {
                    return Err(VelocityError::Config(format!(
                        "env {k}: give a string (or a whole number, true or false)"
                    )))
                }
            };
            env.insert(k.clone(), v);
        }
    }

    let suite = load_runnable_suite(config_path)?;
    if !suite.flows.iter().any(|f| f.id == flow_id) {
        return Err(VelocityError::UnknownFlowRef {
            flow_id: flow_id.to_string(),
            test_name: "<mcp>".to_string(),
        });
    }

    // Run it as a call, the way a test's `runFlow` does, so its parameters
    // are filled (from `env`, else the environment) and the flows it runs
    // are inlined; a parameter with no value is an error, never a literal
    // `${NAME}` typed into the app.
    let steps = resolve_flows(&TestSuite {
        tests: vec![velocity_common::TestCase {
            name: format!("flow {flow_id}"),
            tags: vec![],
            isolated: false,
            depends_on: vec![],
            steps: vec![velocity_common::Step {
                action: velocity_common::Action::RunFlow {
                    flow_id: flow_id.to_string(),
                    env,
                },
                timeout_ms: None,
                settle: None,
                source: None,
                modifiers: Default::default(),
            }],
            description: None,
            verify: None,
        }],
        ..suite.clone()
    })?
    .remove(0)
    .steps;
    let app_id = &suite.app_id;
    let mut executor = TestExecutor::new(driver.as_ref(), suite.config.clone(), &suite.app_id);
    let mut results = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        match executor.execute_step(step, device_id, app_id, i).await {
            Ok(result) => {
                let passed = result.status == velocity_common::StepStatus::Passed;
                results.push(serde_json::json!({
                    "step": i,
                    "action": result.action_name,
                    "status": if passed { "passed" } else { "failed" },
                    "duration_ms": result.duration.as_millis()
                }));
                if !passed {
                    break;
                }
            }
            Err(e) => {
                results.push(serde_json::json!({
                    "step": i,
                    "error": e.to_string()
                }));
                break;
            }
        }
    }

    Ok(serde_json::json!({
        "flow_id": flow_id,
        "steps_executed": results.len(),
        "results": results
    }))
}

pub fn list_tests(config_path: &Option<String>, _args: &Value) -> Result<Value> {
    let suite = load_suite(config_path)?;
    let tests: Vec<Value> = suite
        .tests
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "tags": t.tags,
                "step_count": t.steps.len(),
                "isolated": t.isolated
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": tests.len(),
        "tests": tests
    }))
}

pub async fn run_test(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let test_name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VelocityError::Config("Missing 'name' argument".to_string()))?;

    let suite = load_runnable_suite(config_path)?;
    // Inline `runFlow` steps first, as `velocity run` does: the raw test
    // failed at its first `runFlow` ("flows must be resolved before
    // execution"), so any test built on a shared flow couldn't run here.
    let tests = resolve_flows(&suite)?;
    let test = tests
        .iter()
        .find(|t| t.name == test_name)
        .ok_or_else(|| VelocityError::Config(format!("Test '{test_name}' not found in suite")))?;

    let app_id = &suite.app_id;
    let mut executor = TestExecutor::new(driver.as_ref(), suite.config.clone(), &suite.app_id);
    let result = executor.execute_test(test, device_id, app_id).await?;

    Ok(serde_json::json!({
        "test_name": result.test_name,
        "status": format!("{:?}", result.status).to_lowercase(),
        "duration_ms": result.duration.as_millis(),
        "steps": result.steps.len(),
        "retries": result.retries,
        "error": result.error_message,
        // Kept for a failure; `describe_failure` reads it.
        "trace": result.trace
    }))
}

/// What `generate_test_skeleton` can build from: the session's recorded
/// actions, and the screen now.
#[derive(Default)]
pub struct SkeletonSource<'a> {
    pub recorder: Option<&'a crate::recording::Recorder>,
    pub screen: Option<&'a velocity_common::Element>,
    pub screen_rect: Option<velocity_common::Rect>,
    pub platform: Option<velocity_common::Platform>,
}

/// A test from the session: its recorded actions (with their assertions)
/// when there are any; else a launch and an assertion on the current
/// screen, with its actionable elements listed as comments to build on;
/// else a placeholder. The description only ever goes into comments.
pub fn generate_test_skeleton(
    config_path: &Option<String>,
    args: &Value,
    from: SkeletonSource<'_>,
) -> Result<Value> {
    use crate::recording::{one, pick_assertion, selector_yaml, suite};
    use serde_yaml::{Mapping, Value as Yaml};

    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("New test");

    // The loaded suite's app when none is given (it said com.example.app
    // even with a suite open).
    let suite_app = config_path
        .as_ref()
        .and_then(|_| load_suite(config_path).ok())
        .map(|s| s.app_id);
    let app_id = args
        .get("app_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(suite_app)
        .or_else(|| {
            from.recorder
                .and_then(|r| r.launched_app())
                .map(String::from)
        })
        .unwrap_or_else(|| "com.example.app".to_string());

    let sanitized_name = description
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != ' ', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_");
    // A comment line is whatever YAML breaks lines on: `lines()` split only
    // on `\n`, so a lone CR, NEL or LS/PS ended the comment early (m11).
    let comment = |prefix: &str, text: &str| {
        text.split(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
            .map(|l| format!("{prefix}{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let name = if sanitized_name.is_empty() {
        "generated_test"
    } else {
        sanitized_name.as_str()
    };
    let launch = || one("launchApp", Yaml::Mapping(Mapping::new()));
    let mut notes: Vec<String> = Vec::new();
    let (steps, source) = match (from.recorder, from.screen) {
        (Some(rec), _) if !rec.is_empty() => {
            let (mut steps, _) = rec.steps_yaml();
            if rec.steps.first().is_none_or(|r| r.tool != "launch_app") {
                steps.insert(0, launch());
            }
            (steps, "recorded")
        }
        (_, Some(tree)) => {
            let platform = from.platform.unwrap_or(velocity_common::Platform::Android);
            let rect = from.screen_rect;
            let mut steps = vec![launch()];
            if let Some(sel) = pick_assertion(None, tree, rect.as_ref(), platform, &[]) {
                steps.push(one("assertVisible", Yaml::Mapping(selector_yaml(&sel))));
            }
            let mut snaps = crate::snapshot::Snapshots::default();
            snaps.platform = Some(platform);
            // An unfiltered snapshot can't fail.
            let listing = snaps
                .take(tree, rect.as_ref(), Default::default())
                .unwrap_or(Value::Null);
            notes.push("On screen now (selectors to build steps from):".into());
            notes.extend(
                listing
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .skip(1)
                    .take(25)
                    .map(|l| format!("  {l}")),
            );
            (steps, "screen")
        }
        _ => {
            let mut placeholder = Mapping::new();
            placeholder.insert("text".into(), "Expected Element".into());
            let mut launch_clear = Mapping::new();
            launch_clear.insert("clearState".into(), true.into());
            (
                vec![
                    one("launchApp", Yaml::Mapping(launch_clear)),
                    one("assertVisible", Yaml::Mapping(placeholder)),
                ],
                "placeholder",
            )
        }
    };
    let n = steps.len();
    let mut header = format!(
        "# Auto-generated test for:\n{}\n# TODO: add the steps it still needs.\n",
        comment("# ", description)
    );
    for note in &notes {
        header.push_str(&comment("# ", note));
        header.push('\n');
    }
    let skeleton = format!("{header}{}", suite(&app_id, name, &["generated"], steps));

    Ok(serde_json::json!({
        "description": description,
        "app_id": app_id,
        "test_name": sanitized_name,
        "source": source,
        "steps": n,
        "yaml": skeleton
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skeleton_is_a_valid_suite() {
        let out = generate_test_skeleton(
            &None,
            &serde_json::json!({
                "description": "Log in, see welcome!",
                "app_id": "com.velocity.lab"
            }),
            Default::default(),
        )
        .unwrap();
        let yaml = out["yaml"].as_str().unwrap();
        let suite = velocity_core::parser::parse_suite_from_str(yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        velocity_core::validate_suite(&suite).unwrap();
        assert_eq!(suite.app_id, "com.velocity.lab");
        assert_eq!(suite.tests[0].name, "log_in_see_welcome");
        assert_eq!(suite.tests[0].steps.len(), 2);
    }

    // VEL-087: a description with newlines and YAML, and an app id with
    // YAML syntax, can't change the suite.
    #[test]
    fn the_skeleton_quotes_what_it_interpolates() {
        let out = generate_test_skeleton(
            &None,
            &serde_json::json!({
                "description": "Pay\ntests: []\n- name: injected: {x",
                "app_id": "com.app # not a comment: {"
            }),
            Default::default(),
        )
        .unwrap();
        let yaml = out["yaml"].as_str().unwrap();
        let suite = velocity_core::parser::parse_suite_from_str(yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        velocity_core::validate_suite(&suite).unwrap();
        assert_eq!(suite.app_id, "com.app # not a comment: {");
        assert_eq!(suite.tests.len(), 1);
        assert_eq!(suite.tests[0].steps.len(), 2);
    }

    // m11: YAML (libyaml) also breaks lines on CR, NEL and LS/PS.
    #[test]
    fn the_skeleton_comment_survives_every_yaml_line_break() {
        for sep in ["\r", "\r\n", "\u{85}", "\u{2028}", "\u{2029}"] {
            let description = format!("Pay{sep}tests: []{sep}appId: evil");
            let out = generate_test_skeleton(
                &None,
                &serde_json::json!({ "description": description, "app_id": "com.app" }),
                Default::default(),
            )
            .unwrap();
            let yaml = out["yaml"].as_str().unwrap();
            let suite = velocity_core::parser::parse_suite_from_str(yaml)
                .unwrap_or_else(|e| panic!("{sep:?}: {e}\n{yaml}"));
            velocity_core::validate_suite(&suite).unwrap();
            assert_eq!(suite.app_id, "com.app", "{sep:?}");
            assert_eq!(suite.tests.len(), 1, "{sep:?}");
            assert_eq!(suite.tests[0].steps.len(), 2, "{sep:?}");
        }
    }

    // VEL-088: run_test/run_flow see the suite with `${VAR}` filled in.
    #[test]
    fn runnable_suite_is_interpolated() {
        let d = std::env::temp_dir().join(format!("vel-mcp-interp-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("velocity.yaml");
        std::fs::write(
            &path,
            "appId: ${VEL088_APP:-com.interpolated}\ntests:\n- name: t\n  steps:\n  - launchApp: {}\n",
        )
        .unwrap();
        let cfg = Some(path.to_string_lossy().to_string());
        let suite = load_runnable_suite(&cfg).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(suite.app_id, "com.interpolated");
    }
}
