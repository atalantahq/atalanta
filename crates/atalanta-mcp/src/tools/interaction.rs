use std::sync::Arc;

use serde_json::Value;
use velocity_common::{Direction, Element, Key, PlatformDriver, Result, Selector, VelocityError};

/// The tool call's `selector`, checked: an invalid `textMatches` /
/// `idMatches` pattern is an error, not a lookup that never matches (D-11).
pub(crate) fn parse_selector(args: &Value) -> Result<Selector> {
    let selector_val = args
        .get("selector")
        .ok_or_else(|| VelocityError::Config("Missing 'selector' argument".to_string()))?;

    // The DSL's flat map (`{"role": "button", "name": "Pay", "enabled":
    // true}`), as a suite writes it (S1/S2), and only that: serde's tagged
    // form of `Selector` skipped the DSL's checks, so `{"enabled": true}`
    // tapped the first enabled element (review 02 H2). A ref resolves to
    // the flat form too (`refs::resolve_args`).
    let selector: Selector = velocity_core::parser::selector_from_value(selector_val.clone())
        .map_err(|flat| VelocityError::Config(format!("Invalid selector: {flat}")))?;
    velocity_common::selector_match::check_patterns(&selector)
        .map_err(|e| VelocityError::Config(format!("Invalid selector: {e}")))?;
    if let Some(why) = role_problem(&selector) {
        return Err(VelocityError::Config(format!("Invalid selector: {why}")));
    }
    Ok(selector)
}

/// The element `selector` matches on the live screen. Drivers keep a
/// short-lived tree (2 s on Android and iOS) that the executor invalidates
/// itself; a tool call is a fresh look, so the tree read just after a tap,
/// mid-transition, is never served again to the next call (DV-1).
pub(crate) async fn find_live(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    selector: &Selector,
) -> Result<Element> {
    driver.invalidate_hierarchy_cache().await;
    driver.find_element(device_id, selector).await
}

/// [`find_live`] for an action (`tap`, `type_text`), retrying a miss for up
/// to [`FIND_GRACE`]: field tests on real apps saw them fail "after 0ms"
/// right after a screen change, where a suite step would have waited.
/// Assertions keep their own `timeout_ms` (none: check now).
pub(crate) async fn find_live_for_action(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    selector: &Selector,
) -> Result<Element> {
    let deadline = std::time::Instant::now() + FIND_GRACE;
    loop {
        match find_live(driver, device_id, selector).await {
            Err(velocity_common::VelocityError::ElementNotFound { .. })
                if std::time::Instant::now() + FIND_POLL < deadline =>
            {
                tokio::time::sleep(FIND_POLL).await;
            }
            other => return other,
        }
    }
}

/// How long a tool's lookup keeps trying a selector that isn't there yet.
const FIND_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);
/// How often it looks again in that time.
const FIND_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// An unknown `role:` anywhere in `selector` (S1): it would never match.
fn role_problem(selector: &Selector) -> Option<String> {
    match selector {
        Selector::Role(r) => velocity_core::validator::role_problem(r),
        Selector::Compound(parts) => parts.iter().find_map(role_problem),
        Selector::Index { selector, .. } => role_problem(selector),
        Selector::Relative { target, anchor, .. } => {
            role_problem(target).or_else(|| role_problem(anchor))
        }
        _ => None,
    }
}

pub async fn tap(driver: &Arc<dyn PlatformDriver>, device_id: &str, args: &Value) -> Result<Value> {
    let selector = parse_selector(args)?;
    let element = find_live_for_action(driver, device_id, &selector).await?;
    driver.tap(device_id, &element).await?;
    Ok(serde_json::json!({
        "action": "tap",
        "selector": selector.to_string(),
        "element": {
            "label": element.label,
            "text": element.text,
            "bounds": element.bounds
        }
    }))
}

pub async fn type_text(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let selector = parse_selector(args)?;
    let input = typed_input(args)?;
    let element = find_live_for_action(driver, device_id, &selector).await?;
    driver
        .input_text(device_id, &element, input.value())
        .await?;
    let mut out = serde_json::json!({
        "action": "type_text",
        "selector": selector.to_string(),
        "element": {
            "label": element.label,
            "text": element.text
        }
    });
    // A secret is named, never echoed (mcp-secret).
    match &input {
        Typed::Text(text) => out["text"] = Value::from(text.as_str()),
        Typed::Secret { name, .. } => out["secret"] = Value::from(name.as_str()),
    }
    Ok(out)
}

/// What `type_text` types: literal `text`, or the value of the environment
/// variable `secret` names, which the agent never sees.
pub enum Typed {
    Text(String),
    Secret { name: String, value: String },
}

impl Typed {
    pub fn value(&self) -> &str {
        match self {
            Self::Text(t) => t,
            Self::Secret { value, .. } => value,
        }
    }
}

/// Whether `name` can be a `${NAME}` in a suite.
fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `text` or `secret` (exactly one). A secret's value comes from the
/// server's environment (or `.velocity` settings) and is registered for
/// masking, so it can't come back in a result, an error or a log. Only
/// names listed in `VELOCITY_SECRETS`, or used as secrets by a suite in
/// this process, can be read.
pub fn typed_input(args: &Value) -> Result<Typed> {
    let text = args.get("text").and_then(|v| v.as_str());
    let secret = args.get("secret").and_then(|v| v.as_str());
    match (text, secret) {
        (Some(_), Some(_)) => Err(VelocityError::Config(
            "type_text takes 'text' or 'secret', not both".into(),
        )),
        (Some(t), None) => Ok(Typed::Text(t.to_string())),
        (None, Some(name)) => {
            if !is_var_name(name) {
                return Err(VelocityError::Config(format!(
                    "'secret' names an environment variable (letters, digits, _): {name:?}"
                )));
            }
            // Review MAJOR 4: any variable of the server's environment
            // could be typed into a page that sends it home.
            if !velocity_common::redact::is_allowed_secret_name(name) {
                return Err(VelocityError::Config(format!(
                    "secret {name} is not an allowed secret: list it in VELOCITY_SECRETS \
                     (the `secrets` setting) for type_text to read it"
                )));
            }
            let value = velocity_common::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    VelocityError::Config(format!(
                        "secret {name} is not set in the MCP server's environment"
                    ))
                })?;
            velocity_common::redact::register_secret(&value);
            Ok(Typed::Secret {
                name: name.to_string(),
                value,
            })
        }
        (None, None) => Err(VelocityError::Config(
            "Missing 'text' (or 'secret': the name of an environment variable) argument"
                .to_string(),
        )),
    }
}

pub async fn swipe(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let direction_str = args
        .get("direction")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VelocityError::Config("Missing 'direction' argument".to_string()))?;

    let direction: Direction =
        serde_json::from_value(Value::String(normalize_direction(direction_str))).map_err(|e| {
            VelocityError::Config(format!(
                "Invalid direction '{direction_str}': {e}. Use: up, down, left, right"
            ))
        })?;

    driver.swipe(device_id, direction).await?;
    Ok(serde_json::json!({
        "action": "swipe",
        "direction": direction_str
    }))
}

/// `platform` is the device's, when known: iOS has no Escape key, and an
/// agent's `escape` there presses back, as it did before D-9, with a note
/// saying so (review-d6 m3).
pub async fn press_key(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    platform: Option<velocity_common::Platform>,
    args: &Value,
) -> Result<Value> {
    let key_str = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| VelocityError::Config("Missing 'key' argument".to_string()))?;

    let key: Key = serde_json::from_value(Value::String(normalize_key(key_str))).map_err(|e| {
        VelocityError::Config(format!(
            "Invalid key '{key_str}': {e}. Use: back, home, enter, volumeUp, volumeDown, backspace, tab, escape, delete, arrowUp, arrowDown, arrowLeft, arrowRight"
        ))
    })?;

    if key == Key::Escape && platform == Some(velocity_common::Platform::Ios) {
        driver.press_key(device_id, Key::Back).await?;
        return Ok(serde_json::json!({
            "action": "press_key",
            "key": key_str,
            "pressed": "back",
            "note": "iOS has no Escape key: pressed back (the navigation bar's back button) instead"
        }));
    }
    driver.press_key(device_id, key).await?;
    Ok(serde_json::json!({
        "action": "press_key",
        "key": key_str
    }))
}

/// The finger's direction that scrolls content `content` (`down` shows
/// what is below: the finger moves up), as `scrollUntilVisible` scrolls.
pub(crate) fn finger_for(content: &str) -> &'static str {
    match content {
        "up" => "down",
        "left" => "right",
        "right" => "left",
        _ => "up",
    }
}

/// `"UP"`, `" Down "`, `"swipe_left"` → `up` / `down` / `left`: models
/// phrase directions loosely, and a near-miss used to be an error.
pub(crate) fn normalize_direction(s: &str) -> String {
    let t = s.trim().to_lowercase();
    let t = t
        .strip_prefix("swipe_")
        .or_else(|| t.strip_prefix("scroll_"))
        .unwrap_or(&t);
    t.to_string()
}

/// `"Return"`, `"KEYCODE_ENTER"`, `"ENTER"`, `"volume_up"`, `"VolumeUp"` →
/// the names `Key` deserializes (`enter`, `volumeUp`, …).
pub(crate) fn normalize_key(s: &str) -> String {
    let t: String = s
        .trim()
        .to_lowercase()
        .trim_start_matches("keycode_")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    match t.as_str() {
        "enter" | "return" | "submit" | "done" | "go" | "search" => "enter",
        "back" => "back",
        "escape" | "esc" => "escape",
        "home" => "home",
        "volumeup" => "volumeUp",
        "volumedown" => "volumeDown",
        "backspace" | "del" => "backspace",
        "tab" => "tab",
        "delete" | "forwarddel" | "forwarddelete" => "delete",
        "arrowup" | "dpadup" => "arrowUp",
        "arrowdown" | "dpaddown" => "arrowDown",
        "arrowleft" | "dpadleft" => "arrowLeft",
        "arrowright" | "dpadright" => "arrowRight",
        _ => return s.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod lenient_tests {
    use super::*;

    #[test]
    fn loose_direction_and_key_names_are_accepted() {
        assert_eq!(normalize_direction(" UP "), "up");
        assert_eq!(normalize_direction("swipe_left"), "left");
        for k in ["Return", "KEYCODE_ENTER", "ENTER", "enter"] {
            assert_eq!(normalize_key(k), "enter");
        }
        assert_eq!(normalize_key("volume_up"), "volumeUp");
        assert_eq!(normalize_key("VolumeDown"), "volumeDown");
        // D-9: Escape is its own key now, not Back.
        assert_eq!(normalize_key("Esc"), "escape");
        assert_eq!(normalize_key("KEYCODE_DEL"), "backspace");
        assert_eq!(normalize_key("ArrowUp"), "arrowUp");
        assert_eq!(normalize_key("KEYCODE_DPAD_LEFT"), "arrowLeft");
        assert_eq!(
            normalize_key("weird"),
            "weird",
            "unknown keys pass through to the error"
        );
    }
}

#[cfg(test)]
mod regex_selector_tests {
    use super::*;

    #[test]
    fn regex_selectors_parse_and_a_bad_pattern_is_refused() {
        // D-11: `{"textMatches": …}` reaches the matcher; a pattern that
        // can't compile is an error, not a lookup that never matches.
        let ok = parse_selector(&serde_json::json!({ "selector": { "textMatches": "Item \\d+" } }));
        assert_eq!(ok.unwrap(), Selector::TextMatches("Item \\d+".into()));
        let bad = parse_selector(&serde_json::json!({ "selector": { "idMatches": "(" } }));
        assert!(
            bad.unwrap_err()
                .to_string()
                .contains("not a valid regular expression"),
            "a bad pattern is an error"
        );
    }

    // Review 02 H2: serde's tagged form was tried first and let a lone
    // state filter (or `compound`/`relative` maps no suite can write)
    // through; the DSL's own rules decide now.
    #[test]
    fn a_lone_state_filter_and_the_tagged_form_are_refused() {
        for sel in [
            serde_json::json!({ "checked": true }),
            serde_json::json!({ "enabled": true }),
            serde_json::json!({ "compound": [{ "text": "Pay" }, { "enabled": true }] }),
        ] {
            let err = parse_selector(&serde_json::json!({ "selector": sel }));
            let msg = err.expect_err(&sel.to_string()).to_string();
            assert!(!msg.contains("tagged form"), "{msg}");
        }
        assert_eq!(
            parse_selector(&serde_json::json!({ "selector": { "text": "Pay", "enabled": true } }))
                .unwrap(),
            Selector::Compound(vec![Selector::Text("Pay".into()), Selector::Enabled(true)])
        );
    }
}

#[cfg(test)]
mod role_selector_tests {
    use super::*;

    /// S1/S2: a tool call may send the DSL's flat map, as a suite writes
    /// it; an unknown role is refused with the nearest one.
    #[test]
    fn flat_role_name_and_state_selectors_parse() {
        let s = parse_selector(&serde_json::json!({
            "selector": { "role": "button", "name": "Pay", "enabled": true }
        }))
        .unwrap();
        assert_eq!(
            s,
            Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("Pay".into()),
                Selector::Enabled(true),
            ])
        );
        // A lone field is still a selector in the flat form.
        let s = parse_selector(&serde_json::json!({ "selector": { "role": "link" } })).unwrap();
        assert_eq!(s, Selector::Role("link".into()));
        let bad = parse_selector(&serde_json::json!({ "selector": { "role": "buton" } }));
        let msg = bad.unwrap_err().to_string();
        assert!(msg.contains("did you mean \"button\""), "{msg}");
        // A state alone is not a selector.
        assert!(parse_selector(
            &serde_json::json!({ "selector": { "text": "x", "checked": "yes" } })
        )
        .is_err());
    }
}
