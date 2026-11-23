//! Tool profiles (ENT-105, plan row 0.5).
//!
//! Every tool definition costs the agent context on every turn, and agent
//! users rank that cost among their top complaints (claude-code #6915,
//! #7328). So `atalanta mcp` serves a **core** profile by default: four
//! tools that cover the work, each expanding into the full tool it stands
//! for. `--profile full` serves all of them, as before.
//!
//! The core tools never do anything a full tool can't: [`expand`] turns a
//! call into the full tool's name and arguments, and the server runs that
//! through its usual path (lease, refs, recording, read-only check,
//! redaction). `act` then appends the screen's changes (a delta snapshot),
//! so the agent sees what its action did without another call (row 2.3a).

use serde_json::{json, Map, Value};

use crate::tool_registry::ToolDefinition;

/// Which tools `tools/list` offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// `observe`, `act`, `run` and `artifacts` (`atalanta mcp`'s default).
    Core,
    /// Every tool (`--profile full`; the library's default).
    #[default]
    Full,
}

impl std::str::FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "core" | "default" => Ok(Profile::Core),
            "full" | "all" => Ok(Profile::Full),
            other => Err(format!("unknown MCP profile {other:?}: core or full")),
        }
    }
}

/// The core profile's own tools (each stands for several full ones).
pub const CORE_TOOLS: &[&str] = &["observe", "act", "run", "artifacts"];

/// Tools listed in both profiles, as they are.
pub const SHARED_TOOLS: &[&str] = &["verify_change"];

/// Tools that can change something outside the app under test: the
/// machine (`diagnose` with `attempt_fix`), and from row 3.2 a shell or
/// `adb` command. The Rule of Two (docs/SECURITY.md) keeps them out of every
/// profile but `full`.
pub const IRREVERSIBLE_TOOLS: &[&str] = &["diagnose"];

/// Whether `profile` lists `tool`.
pub fn lists(profile: Profile, tool: &str) -> bool {
    match profile {
        Profile::Core => CORE_TOOLS.contains(&tool) || SHARED_TOOLS.contains(&tool),
        Profile::Full => !CORE_TOOLS.contains(&tool),
    }
}

/// `act`'s actions and the full tool each one is.
const ACTIONS: &[(&str, &str)] = &[
    ("tap", "tap"),
    ("type", "type_text"),
    ("swipe", "swipe"),
    ("scroll", "scroll"),
    ("press_key", "press_key"),
    ("launch", "launch_app"),
    ("stop", "stop_app"),
    ("wait", "wait_for_element"),
    ("assert_visible", "assert_visible"),
    ("assert_not_visible", "assert_not_visible"),
];

/// `observe`'s and `artifacts`' kinds and their full tools.
const OBSERVE: &[(&str, &str)] = &[
    ("screen", "snapshot"),
    ("screenshot", "screenshot"),
    ("element", "get_element"),
    ("devices", "list_devices"),
    ("a11y", "audit_accessibility"),
];
const ARTIFACTS: &[(&str, &str)] = &[
    ("failure", "describe_failure"),
    ("trace", "inspect_trace"),
    ("flow", "export_session_flow"),
    ("tests", "list_tests"),
    ("flows", "list_flows"),
];

type Table = &'static [(&'static str, &'static str)];

fn names(table: Table) -> Vec<&'static str> {
    table.iter().map(|(k, _)| *k).collect()
}

/// The core tools' definitions. Kept short on purpose: their size is
/// measured in CI (`core_profile_schema_budget`).
pub fn core_definitions() -> Vec<ToolDefinition> {
    let selector = json!({ "type": "object", "description": "{\"id\": …} or {\"text\": …}" });
    let reference =
        json!({ "type": "string", "description": "\"@e12\" from observe (instead of selector)" });
    vec![
        ToolDefinition {
            name: "observe".into(),
            description: "Read the device; changes nothing. what=screen (default): a line per element, ref e.g. @e12; mode=delta: changes only; within/find narrow it. screenshot: a PNG path. element; devices.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "what": { "type": "string", "enum": names(OBSERVE) },
                    "mode": { "type": "string", "enum": ["full", "delta"] },
                    "within": { "type": "string" },
                    "find": { "type": "string" },
                    "inline": { "type": "boolean", "description": "screenshot: the image in the reply too" },
                    "selector": selector,
                    "ref": reference,
                }
            }),
        },
        ToolDefinition {
            name: "act".into(),
            description: "Do one thing, wait (max 3 s) for the screen to settle, say what changed. Target: ref or selector. type: text or secret (env var name). launch/stop: app_id. Failure: see error.retry.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": names(ACTIONS) },
                    "ref": reference,
                    "selector": selector,
                    "text": { "type": "string" },
                    "secret": { "type": "string" },
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "key": { "type": "string" },
                    "app_id": { "type": "string" },
                    "timeout_ms": { "type": "integer" },
                    "clear_state": { "type": "boolean" },
                    "until_visible": { "type": "object" },
                    "max_scrolls": { "type": "integer" },
                },
                "required": ["action"]
            }),
        },
        ToolDefinition {
            name: "run".into(),
            description: "Background; returns task_id at once: poll it. Start: suite (path), yaml (your suite, runs on device) or goal (an AI explores; costs model calls). task_id: status (wait_ms), stop, instruction.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "suite": { "type": "string" },
                    "yaml": { "type": "string" },
                    "filter": { "type": "string" },
                    "goal": { "type": "string" },
                    "app_id": { "type": "string" },
                    "after": { "type": "string", "description": "A login suite to run first, with no model" },
                    "task_id": { "type": "string" },
                    "wait_ms": { "type": "integer" },
                    "stop": { "type": "boolean" },
                    "instruction": { "type": "string" },
                }
            }),
        },
        ToolDefinition {
            name: "artifacts".into(),
            description: "Read run output; changes nothing. kind=failure (default): why the last test failed. trace; flow: this session as a YAML test; tests/flows: the suite's.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": names(ARTIFACTS) },
                    "task_id": { "type": "string" },
                    "test": { "type": "string" },
                    "step": { "type": "integer" },
                    "name": { "type": "string" },
                }
            }),
        },
    ]
}

/// A core call as the full tool it stands for: (tool, arguments).
pub fn expand(name: &str, args: &Value) -> Result<(String, Value), String> {
    let mut rest: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    let mut pick = |key: &str, table: Table, default: Option<&str>| {
        let given = rest
            .remove(key)
            .and_then(|v| v.as_str().map(str::to_string));
        let wanted = given
            .as_deref()
            .or(default)
            .ok_or_else(|| format!("{name}: `{key}` is required ({})", names(table).join(", ")))?;
        table
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(wanted))
            .map(|(_, tool)| tool.to_string())
            .ok_or_else(|| {
                format!(
                    "{name}: unknown {key} {wanted:?}; one of {}",
                    names(table).join(", ")
                )
            })
    };
    let tool = match name {
        "observe" => pick("what", OBSERVE, Some("screen"))?,
        "act" => pick("action", ACTIONS, None)?,
        "artifacts" => pick("kind", ARTIFACTS, Some("failure"))?,
        "run" => {
            let has_task = rest.get("task_id").is_some();
            let stop = rest
                .remove("stop")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_task {
                if stop || rest.contains_key("instruction") {
                    return Err(
                        "run: stop and instruction need the task_id of a running task".into(),
                    );
                }
                "run_task".into()
            } else if stop {
                "task_stop".into()
            } else if rest.contains_key("instruction") {
                "task_inject".into()
            } else {
                "task_status".into()
            }
        }
        other => return Err(format!("{other} is not a core tool")),
    };
    Ok((tool, Value::Object(rest)))
}

/// The core call that does what a full tool does, for the error a client
/// gets when it calls a full tool under the core profile.
pub fn core_equivalent(full: &str) -> Option<String> {
    for (tool, key, table) in [
        ("act", "action", ACTIONS),
        ("observe", "what", OBSERVE),
        ("artifacts", "kind", ARTIFACTS),
    ] {
        if let Some((k, _)) = table.iter().find(|(_, t)| *t == full) {
            return Some(format!("{tool} {{\"{key}\": \"{k}\", …}}"));
        }
    }
    match full {
        "run_task" => Some("run {\"suite\": …} or run {\"goal\": …}".into()),
        "task_status" => Some("run {\"task_id\": …}".into()),
        "task_stop" => Some("run {\"task_id\": …, \"stop\": true}".into()),
        "task_inject" => Some("run {\"task_id\": …, \"instruction\": …}".into()),
        _ => None,
    }
}

/// Whether `act`'s full tool may change the screen (so a delta follows).
pub fn changes_screen(full: &str) -> bool {
    crate::refs::SCREEN_CHANGING_TOOLS.contains(&full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_calls_expand_to_the_full_tools() {
        let (t, a) = expand("act", &json!({"action": "tap", "ref": "@e3"})).unwrap();
        assert_eq!((t.as_str(), a), ("tap", json!({"ref": "@e3"})));
        let (t, a) = expand(
            "act",
            &json!({"action": "type", "ref": "@e1", "secret": "PW"}),
        )
        .unwrap();
        assert_eq!(
            (t.as_str(), a),
            ("type_text", json!({"ref": "@e1", "secret": "PW"}))
        );
        assert_eq!(expand("observe", &json!({})).unwrap().0, "snapshot");
        assert_eq!(
            expand("observe", &json!({"what": "screenshot"})).unwrap().0,
            "screenshot"
        );
        assert_eq!(
            expand("artifacts", &json!({})).unwrap().0,
            "describe_failure"
        );
        assert_eq!(
            expand("run", &json!({"goal": "open settings"})).unwrap().0,
            "run_task"
        );
        assert_eq!(
            expand("run", &json!({"task_id": "t1"})).unwrap().0,
            "task_status"
        );
        let (t, a) = expand("run", &json!({"task_id": "t1", "stop": true})).unwrap();
        assert_eq!((t.as_str(), a), ("task_stop", json!({"task_id": "t1"})));
        assert_eq!(
            expand("run", &json!({"task_id": "t1", "instruction": "go back"}))
                .unwrap()
                .0,
            "task_inject"
        );
    }

    #[test]
    fn bad_core_calls_say_what_is_allowed() {
        let e = expand("act", &json!({})).unwrap_err();
        assert!(
            e.contains("`action` is required") && e.contains("tap"),
            "{e}"
        );
        let e = expand("act", &json!({"action": "explode"})).unwrap_err();
        assert!(
            e.contains("unknown action") && e.contains("press_key"),
            "{e}"
        );
        assert!(expand("run", &json!({"stop": true})).is_err());
    }

    #[test]
    fn every_full_tool_a_core_tool_reaches_is_named_in_the_hint() {
        assert_eq!(
            core_equivalent("tap").unwrap(),
            "act {\"action\": \"tap\", …}"
        );
        assert!(core_equivalent("task_stop").unwrap().contains("stop"));
        assert!(core_equivalent("open_inspector").is_none());
    }

    // Row 2.1 (NEW-002), the Rule of Two: the default profile reads untrusted
    // screen text (A) and can type secrets by name (B), so it lists no tool
    // with an irreversible effect outside the app (C).
    #[test]
    fn only_the_full_profile_lists_a_tool_that_changes_the_machine() {
        for t in IRREVERSIBLE_TOOLS {
            assert!(!lists(Profile::Core, t), "{t} in the core profile");
            assert!(lists(Profile::Full, t), "{t} missing from full");
            assert!(core_equivalent(t).is_none(), "a core call reaches {t}");
        }
        for (_, table) in [
            ("act", ACTIONS),
            ("observe", OBSERVE),
            ("artifacts", ARTIFACTS),
        ] {
            for (_, full) in table {
                assert!(
                    !IRREVERSIBLE_TOOLS.contains(full),
                    "{full} reachable from core"
                );
            }
        }
    }

    #[test]
    fn profiles_parse() {
        assert_eq!("core".parse::<Profile>().unwrap(), Profile::Core);
        assert_eq!("FULL".parse::<Profile>().unwrap(), Profile::Full);
        assert!("tiny".parse::<Profile>().is_err());
    }
}
