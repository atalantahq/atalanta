//! `velocity explore --after login.yaml` (row 1.5, AI-021): start from a
//! stored flow, typically a login, replayed with no model, then explore.
//!
//! The model never sees the login: its steps run before the first planning
//! turn, their typed values come from `${NAME}`s (the environment,
//! `--env`, `${keychain:NAME}`, `${totp:NAME}`) and are masked everywhere,
//! and the test explore writes starts with the same steps, written with the
//! same `${NAME}`s, never the values.

use std::collections::HashMap;

use velocity_common::{Action, Result, TestCase, VelocityError};
use velocity_nl::goal::GoalOp;

use crate::session::{RecordedStep, ValueKind, ValueSlot};

/// The steps of `test` (default: the first) in the suite `yaml`, with
/// flows inlined, ready to replay before exploring. `overrides` are the
/// `--env` values.
pub fn login_prefix(
    yaml: &str,
    test: Option<&str>,
    overrides: &HashMap<String, String>,
) -> Result<Vec<RecordedStep>> {
    let bad = |m: String| VelocityError::Config(format!("--after: {m}"));
    let raw = velocity_core::parser::parse_suite_from_str(yaml)?;
    let mut filled = raw.clone();
    velocity_core::env::interpolate_suite(&mut filled, overrides)?;
    let pick = |tests: Vec<TestCase>| -> Result<TestCase> {
        match test {
            Some(name) => tests
                .into_iter()
                .find(|t| t.name == name)
                .ok_or_else(|| bad(format!("no test named {name:?}"))),
            None => tests
                .into_iter()
                .next()
                .ok_or_else(|| bad("the file has no test".into())),
        }
    };
    let raw_test = pick(velocity_core::resolver::resolve_flows(&raw)?)?;
    let filled_test = pick(velocity_core::resolver::resolve_flows(&filled)?)?;
    let names = filled.config.captured_vars.clone();
    let mut out = Vec::new();
    for (i, (raw_step, step)) in raw_test.steps.iter().zip(&filled_test.steps).enumerate() {
        // `${totp:…}` is worked out now, right before the replay.
        let step = velocity_core::env::bind_step(step, &HashMap::new(), &names)?
            .unwrap_or_else(|| step.clone());
        let action = step.action.clone();
        let op = match &action {
            Action::Tap { .. } | Action::DoubleTap { .. } => GoalOp::Tap,
            Action::LongPress { .. } => GoalOp::LongPress,
            Action::InputText { .. } => GoalOp::InputText,
            Action::ClearText { .. } => GoalOp::ClearText,
            Action::ScrollUntilVisible { .. } => GoalOp::Scroll,
            Action::Swipe {
                direction: Some(_), ..
            } => GoalOp::Swipe,
            Action::PressKey { .. } => GoalOp::Enter,
            Action::GoBack => GoalOp::Back,
            Action::Wait { .. } => GoalOp::Wait,
            Action::LaunchApp { .. } => GoalOp::Launch,
            Action::AssertVisible { .. } | Action::AssertNotVisible { .. } => GoalOp::Assert,
            other => {
                return Err(bad(format!(
                    "step {} ({}) can't be replayed before exploring; a login flow may tap, \
                     type, scroll, swipe, press keys, wait, launch and assert",
                    i + 1,
                    crate::yaml::action_label(other)
                )))
            }
        };
        let typed = match (&raw_step.action, &action) {
            (Action::InputText { text: written, .. }, Action::InputText { text: value, .. }) => {
                Some(slot(written, value))
            }
            _ => None,
        };
        let label = crate::yaml::action_label_named(&action, typed.as_ref());
        out.push(RecordedStep {
            turn: 0,
            op,
            action,
            label,
            typed,
            target: None,
            scroll: None,
            before_title: String::new(),
            after_title: String::new(),
            changed: true,
            ms: 0,
            screen_assertion: None,
            expectation: None,
            post_assertion: None,
        });
    }
    Ok(out)
}

/// How a typed value is written back: `${NAME}` as it was written (a
/// secret when the name looks like one, or comes from the keychain or a
/// TOTP), else the literal.
fn slot(written: &str, value: &str) -> ValueSlot {
    let name = written
        .strip_prefix("${")
        .and_then(|r| r.strip_suffix('}'))
        .filter(|n| !n.contains("${") && !n.contains('}'));
    match name {
        Some(n) => {
            let secret = n.starts_with("keychain:")
                || n.starts_with("totp:")
                || velocity_common::redact::is_secret_name(n);
            if secret {
                velocity_common::redact::register_secret(value);
            }
            ValueSlot {
                name: n.to_string(),
                kind: if secret {
                    ValueKind::Secret
                } else {
                    ValueKind::Data
                },
                value: value.to_string(),
            }
        }
        None if written.contains("${") => {
            // Text around a placeholder: kept as written, never the value.
            ValueSlot {
                name: written.to_string(),
                kind: ValueKind::Literal,
                value: written.to_string(),
            }
        }
        None => ValueSlot {
            name: format!("{written:?}"),
            kind: ValueKind::Literal,
            value: value.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = r#"
appId: com.app
flows:
  - id: open-login
    steps:
      - tap: { id: sign_in }
tests:
  - name: login
    steps:
      - launchApp: { clearState: true }
      - runFlow: open-login
      - inputText: { selector: { id: email }, text: "${VEL15_EMAIL}" }
      - inputText: { selector: { id: password }, text: "${VEL15_PASSWORD}" }
      - inputText: { selector: { id: otp }, text: "${totp:VEL15_TOTP}" }
      - tap: { text: Continue }
      - assertVisible: { text: Home }
"#;

    fn env() -> HashMap<String, String> {
        [
            ("VEL15_EMAIL", "qa@example.com"),
            ("VEL15_PASSWORD", "correct horse battery"),
            ("VEL15_TOTP", "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into()
    }

    #[test]
    fn the_login_replays_with_values_and_writes_names() {
        let steps = login_prefix(LOGIN, None, &env()).unwrap();
        let labels: Vec<_> = steps.iter().map(|s| s.op).collect();
        assert_eq!(
            labels,
            [
                GoalOp::Launch,
                GoalOp::Tap,
                GoalOp::InputText,
                GoalOp::InputText,
                GoalOp::InputText,
                GoalOp::Tap,
                GoalOp::Assert
            ]
        );
        // Replayed with the values…
        let Action::InputText { text, .. } = &steps[3].action else {
            panic!()
        };
        assert_eq!(text, "correct horse battery");
        let Action::InputText { text: code, .. } = &steps[4].action else {
            panic!()
        };
        assert!(
            code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()),
            "{code}"
        );
        // …written back by name, and the secret ones masked.
        let yaml: Vec<String> = steps
            .iter()
            .map(|s| {
                serde_yaml::to_string(
                    &crate::yaml::action_value(&s.action, s.typed.as_ref()).unwrap(),
                )
                .unwrap()
            })
            .collect();
        let all = yaml.join("");
        assert!(
            all.contains("${VEL15_EMAIL}") && all.contains("${VEL15_PASSWORD}"),
            "{all}"
        );
        assert!(all.contains("${totp:VEL15_TOTP}"), "{all}");
        assert!(
            !all.contains("correct horse") && !all.contains(code.as_str()),
            "{all}"
        );
        assert_eq!(steps[3].typed.as_ref().unwrap().kind, ValueKind::Secret);
        assert_eq!(steps[2].typed.as_ref().unwrap().kind, ValueKind::Data);
        let masked = velocity_common::redact::redact("pw correct horse battery").into_owned();
        assert!(!masked.contains("correct horse"), "{masked}");
        for s in &steps {
            assert!(!s.label.contains("correct horse"), "{}", s.label);
        }
    }

    #[test]
    fn a_missing_secret_or_an_unreplayable_step_is_a_config_error() {
        let e = login_prefix(LOGIN, None, &HashMap::new()).unwrap_err();
        assert_eq!(e.exit_code(), 2, "{e}");
        let http =
            "appId: a\ntests:\n  - name: t\n    steps:\n      - http: { url: \"https://x.dev\" }\n";
        let e = login_prefix(http, None, &HashMap::new()).unwrap_err();
        assert!(
            e.to_string().contains("can't be replayed before exploring"),
            "{e}"
        );
        let e = login_prefix(LOGIN, Some("nope"), &env()).unwrap_err();
        assert!(e.to_string().contains("no test named"), "{e}");
    }
}
