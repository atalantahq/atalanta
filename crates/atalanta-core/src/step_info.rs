// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Descriptions of a step/action that stay valid on wasm32: the short label
//! used in error messages (`tap { text: "Pay" }`), and `setConfig`
//! validation (`check_set_config`). Split out of `executor.rs` so the
//! parser/validator path compiles without the rest of the executor
//! (tokio drivers, screenshots, dialogs).

use atalanta_common::{Action, Selector};

/// Whether `setConfig` can change `key` to `value` mid-test: a named
/// setting, or one of the sync timings.
pub fn check_set_config(key: &str, value: &str) -> std::result::Result<(), String> {
    if atalanta_common::settings::find(key).is_some() {
        return Ok(());
    }
    let mut probe = atalanta_common::SyncConfig::default();
    set_sync_field(&mut probe, key, value)
}

/// `sync.timeoutMs`, `sync.reactionMs`, `sync.quietMs`, `sync.settleCapMs`.
pub(crate) fn set_sync_field(
    sync: &mut atalanta_common::SyncConfig,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let field = key.strip_prefix("sync.").ok_or_else(|| {
        format!(
            "setConfig: `{key}` isn't a setting or a sync timing (`atalanta settings` lists settings)"
        )
    })?;
    // The key first: `sync.mode: none` said "`none` isn't milliseconds"
    // when the problem is that `mode` can't change mid-test (VEL-218).
    let ms = |field: &str| -> std::result::Result<u64, String> {
        if !matches!(
            field,
            "timeoutMs"
                | "timeout_ms"
                | "reactionMs"
                | "reaction_ms"
                | "quietMs"
                | "quiet_ms"
                | "settleCapMs"
                | "settle_cap_ms"
        ) {
            return Err(format!(
                "setConfig: sync.{field} can't change mid-test (timeoutMs, reactionMs, quietMs, settleCapMs can)"
            ));
        }
        value
            .trim()
            .parse()
            .map_err(|_| format!("setConfig.{key}: `{value}` isn't milliseconds"))
    };
    let v = ms(field)?;
    match field {
        "timeoutMs" | "timeout_ms" => sync.timeout_ms = v,
        "reactionMs" | "reaction_ms" => sync.reaction_ms = Some(v),
        "quietMs" | "quiet_ms" => sync.quiet_ms = Some(v),
        _ => sync.settle_cap_ms = Some(v),
    }
    Ok(())
}

/// An action as it reads in YAML, short: `tap { text: "Pay" }`.
pub fn step_label(action: &Action) -> String {
    match action_selector(action) {
        Some(sel) => format!("{} {{ {} }}", action_name(action), selector_fields(sel)),
        None => action_name(action),
    }
}

pub(crate) fn action_name(action: &Action) -> String {
    match action {
        Action::LaunchApp { .. } => "launchApp".to_string(),
        Action::StopApp { .. } => "stopApp".to_string(),
        Action::AssertSystemState { .. } => "assertSystemState".to_string(),
        Action::SetConfig { .. } => "setConfig".to_string(),
        Action::MockRequest { .. } => "mockRequest".to_string(),
        Action::ClearMocks => "clearMocks".to_string(),
        Action::Http(req) => format!("http({} {})", req.method(), req.url),
        Action::Exec(e) => format!(
            "exec({})",
            e.run.argv().first().map(String::as_str).unwrap_or("")
        ),
        Action::Tap { .. } => "tap".to_string(),
        Action::DoubleTap { .. } => "doubleTap".to_string(),
        Action::LongPress { .. } => "longPress".to_string(),
        Action::Drag { .. } => "drag".to_string(),
        Action::InputText { .. } => "inputText".to_string(),
        Action::ClearText { .. } => "clearText".to_string(),
        Action::AssertVisible { .. } => "assertVisible".to_string(),
        Action::AssertNotVisible { .. } => "assertNotVisible".to_string(),
        Action::AssertText { .. } => "assertText".to_string(),
        Action::ScrollUntilVisible { .. } => "scrollUntilVisible".to_string(),
        Action::Swipe { .. } => "swipe".to_string(),
        Action::Screenshot { .. } => "screenshot".to_string(),
        Action::AssertScreenshot { name, .. } => format!("assertScreenshot({name})"),
        Action::SaveState { name } => format!("saveState({name})"),
        Action::RestoreState { name } => format!("restoreState({name})"),
        Action::PressKey { .. } => "pressKey".to_string(),
        Action::HideKeyboard => "hideKeyboard".to_string(),
        Action::Wait { .. } => "wait".to_string(),
        Action::RunFlow { .. } => "runFlow".to_string(),
        Action::TapXY { x, y } => format!("tapXY({x},{y})"),
        Action::InputRaw { .. } => "inputRaw".to_string(),
        Action::Goto { url } => format!("goto({url})"),
        Action::GoBack => "goBack".to_string(),
        Action::Refresh => "refresh".to_string(),
        Action::Natural { intent, kind, .. } => {
            let verb = match kind {
                atalanta_common::NaturalKind::Act => "do",
                atalanta_common::NaturalKind::Assert => "assert",
            };
            format!("{verb}({intent})")
        }
    }
}

/// Selectors a step acts on (used to render `{ ... }` after the verb).
pub(crate) fn action_selector(action: &Action) -> Option<&Selector> {
    match action {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::Drag { from: selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    }
}

fn yaml_quoted(v: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control()
                || matches!(
                    c,
                    '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
                ) =>
            {
                let n = u32::from(c);
                if n <= 0xff {
                    let _ = write!(out, "\\x{n:02x}");
                } else {
                    let _ = write!(out, "\\u{n:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A selector's YAML fields: `text: "Pay", index: 1`.
pub fn selector_fields(sel: &Selector) -> String {
    let q = |v: &str| yaml_quoted(v);
    match sel {
        Selector::Id(v) => format!("id: {}", q(v)),
        Selector::Text(v) => format!("text: {}", q(v)),
        Selector::TextContains(v) => format!("textContains: {}", q(v)),
        Selector::TextMatches(v) => format!("textMatches: {}", q(v)),
        Selector::IdMatches(v) => format!("idMatches: {}", q(v)),
        Selector::AccessibilityId(v) => format!("accessibilityId: {}", q(v)),
        Selector::ClassName(v) => format!("className: {}", q(v)),
        Selector::Css(v) => format!("css: {}", q(v)),
        Selector::XPath(v) => format!("xpath: {}", q(v)),
        Selector::Role(v) => format!("role: {}", q(v)),
        Selector::Name(v) => format!("name: {}", q(v)),
        Selector::Checked(b) => format!("checked: {b}"),
        Selector::Focused(b) => format!("focused: {b}"),
        Selector::Selected(b) => format!("selected: {b}"),
        Selector::Enabled(b) => format!("enabled: {b}"),
        Selector::Index { selector, index } => {
            format!("{}, index: {index}", selector_fields(selector))
        }
        Selector::Compound(parts) => parts
            .iter()
            .map(selector_fields)
            .collect::<Vec<_>>()
            .join(", "),
        Selector::Relative {
            target,
            relation,
            anchor,
        } => format!(
            "{}, {relation}: {{ {} }}",
            selector_fields(target),
            selector_fields(anchor)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_label_prints_selector() {
        let action: Action = Action::Tap {
            selector: Selector::Text("Pay".into()),
        };
        assert_eq!(step_label(&action), "tap { text: \"Pay\" }");
    }

    #[test]
    fn check_set_config_accepts_sync_and_settings() {
        assert!(check_set_config("sync.quietMs", "50").is_ok());
        assert!(check_set_config("sync.mode", "none").is_err());
    }
}
