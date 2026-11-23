// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Tools that run one suite step through the executor, so they behave
//! exactly as the same step in a suite: `launch_app`, `stop_app`, `scroll`
//! (until an element is visible) and `assert_not_visible`.

use std::sync::Arc;

use atalanta_common::{
    Action, AtalantaError, Direction, LaunchArgs, PlatformDriver, Result, Step, StepStatus,
};
use atalanta_core::TestExecutor;
use serde_json::Value;

/// The loaded suite's config and app id, if a suite is loaded.
fn suite(config_path: &Option<String>) -> Option<atalanta_common::TestSuite> {
    config_path
        .as_deref()
        .and_then(|p| atalanta_core::parse_suite(p).ok())
}

/// `app_id`, else the loaded suite's.
fn app_id(config_path: &Option<String>, args: &Value) -> Result<String> {
    args.get("app_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| suite(config_path).map(|s| s.app_id))
        .ok_or_else(|| {
            AtalantaError::Config(
                "'app_id' is required (no suite is loaded: start the server with -c suite.yaml to default it)"
                    .into(),
            )
        })
}

/// Run `action` as one step; a failed step is an error with its message.
async fn run_step(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    app_id: &str,
    action: Action,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    // With no app to check, an absence can't require the app in front (D-2).
    let app_in_front = app_id.is_empty().then_some(false);
    // Without a suite, a failed step's screenshot goes to a scratch folder,
    // not the server's working directory.
    let config = suite(config_path).map(|s| s.config).unwrap_or_else(|| {
        let mut c = atalanta_common::SuiteConfig::default();
        c.artifacts.output_dir = std::env::temp_dir()
            .join("atalanta-mcp")
            .to_string_lossy()
            .into_owned();
        c
    });
    let mut executor = TestExecutor::new(driver.as_ref(), config, app_id);
    let step = Step {
        action,
        timeout_ms,
        settle: None,
        source: None,
        modifiers: atalanta_common::StepModifiers {
            app_in_front,
            ..Default::default()
        },
    };
    let r = executor.execute_step(&step, device_id, app_id, 0).await?;
    if r.status == StepStatus::Failed {
        let (kind, crashed) = executor.last_failure();
        return Err(AtalantaError::StepFailed {
            kind: kind.unwrap_or(atalanta_common::ErrorKind::Unknown),
            crashed,
            message: r
                .error_message
                .unwrap_or_else(|| format!("{} failed", r.action_name)),
        });
    }
    Ok(serde_json::json!({
        "action": r.action_name,
        "status": "passed",
        "duration_ms": r.duration.as_millis() as u64,
        "settle_ms": r.settle_ms,
    }))
}

fn direction(args: &Value, key: &str, default: Direction) -> Result<Direction> {
    match args.get(key).and_then(Value::as_str) {
        None => Ok(default),
        Some(d) => {
            serde_json::from_value(Value::String(super::interaction::normalize_direction(d)))
                .map_err(|_| {
                    AtalantaError::Config(format!(
                        "Invalid {key} '{d}'. Use: up, down, left, right"
                    ))
                })
        }
    }
}

pub async fn launch_app(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let app = app_id(config_path, args)?;
    let clear_state = args
        .get("clear_state")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let action = Action::LaunchApp {
        app_id: app.clone(),
        clear_state,
        arguments: LaunchArgs::default(),
    };
    let mut out = run_step(driver, device_id, config_path, &app, action, None).await?;
    out["app_id"] = app.into();
    Ok(out)
}

pub async fn stop_app(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let app = app_id(config_path, args)?;
    driver.stop_app(device_id, &app).await?;
    Ok(serde_json::json!({ "action": "stopApp", "app_id": app }))
}

/// `direction` is where the content goes (`down` shows what is below), as
/// in `scrollUntilVisible`. With `until_visible` it scrolls until that
/// element is on screen, up to `max_scrolls`; without, one scroll.
pub async fn scroll(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let content = direction(args, "direction", Direction::Down)?;
    let Some(target) = args.get("until_visible") else {
        let finger = super::interaction::finger_for(&format!("{content:?}").to_lowercase());
        let finger: Direction = serde_json::from_value(Value::String(finger.into()))
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("{e}")))?;
        driver.swipe(device_id, finger).await?;
        return Ok(
            serde_json::json!({ "action": "scroll", "direction": format!("{content:?}").to_lowercase() }),
        );
    };
    let selector = super::interaction::parse_selector(&serde_json::json!({ "selector": target }))?;
    let max_scrolls = args
        .get("max_scrolls")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 50) as u32;
    let app = app_id(config_path, args).unwrap_or_default();
    let action = Action::ScrollUntilVisible {
        selector,
        direction: content,
        max_scrolls,
        within: None,
    };
    run_step(driver, device_id, config_path, &app, action, None).await
}

pub async fn assert_not_visible(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    config_path: &Option<String>,
    args: &Value,
) -> Result<Value> {
    let selector = super::interaction::parse_selector(args)?;
    let timeout = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5_000)
        .min(30_000);
    let app = app_id(config_path, args).unwrap_or_default();
    let shown = selector.to_string();
    let action = Action::AssertNotVisible { selector };
    let mut out = run_step(driver, device_id, config_path, &app, action, Some(timeout)).await?;
    out["selector"] = shown.into();
    out["visible"] = false.into();
    Ok(out)
}
