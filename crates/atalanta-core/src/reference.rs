// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The DSL reference (`docs/DSL-REFERENCE.md`), generated from the JSON
//! Schema in [`crate::schema`], the parser's config structs and the
//! settings table, so the doc lists every action, selector key, config key
//! and setting the code knows. `atalanta schema --markdown` prints it; the
//! `dsl_reference` test fails when the checked-in file is out of date.

use std::fmt::Write;

use serde_json::Value;

/// One line on what each action does. Every action in
/// [`crate::schema::ACTIONS`] has one (a test checks); it is also the step's
/// `description` in the JSON Schema.
pub fn action_doc(action: &str) -> &'static str {
    match action {
        "launchApp" => "Launch the app (or `appId`); `clearState: true` wipes its data first.",
        "stopApp" => "Stop the app (or `appId`).",
        "setConfig" => "Change named settings or `sync.*` timings for the rest of this test; the test's end restores them.",
        "mockRequest" => "Web only: answer requests matching `url` with a canned response, delay or block them, until `clearMocks` or the test ends.",
        "clearMocks" => "Web only: drop the mocks this test added (the suite's `web.mocks` stay).",
        "assertSystemState" => "Android only: assert media playback (`mediaPlaying`, `mediaTitle`) or call state (`inCall`), polled until the step timeout.",
        "tap" => "Tap the element, after it is visible, enabled, still and not covered (`force: true` skips the checks).",
        "doubleTap" => "Double-tap the element.",
        "longPress" => "Press and hold the element (`durationMs`, default 1000).",
        "drag" => "Reorder: press the `from` element, hold it (`holdMs`, default 700, past the long-press time), move onto the `to` element over `moveMs` (default 900), rest so it drops in place, and lift.",
        "inputText" => "Type `text` into the element (focusing it first).",
        "clearText" => "Clear the element's text.",
        "assertVisible" => "Pass once the element is on screen; fail at the step timeout.",
        "assertNotVisible" => "Pass once the element is gone; the app must be in front unless `appInFront: false`.",
        "assertText" => "The element's text equals `expected` (polled until the step timeout).",
        "scrollUntilVisible" => "Scroll (`direction`, default down; `within` a container) until the element is on screen, up to `maxScrolls`.",
        "swipe" => "Swipe in a `direction` (optionally `within` an element) or `from` one point `to` another.",
        "screenshot" => "Save a screenshot to the artifacts folder.",
        "assertScreenshot" => "Compare the screen with the baseline `atalanta-baselines/<platform>[-headless]/<W>x<H>/<name>.png`; a missing baseline is written and the step fails.",
        "saveState" => "Save the app's data under a name (iOS simulator: data container clone; Android: run-as tar on debuggable builds, else an emulator snapshot).",
        "restoreState" => "Put back app data saved with `saveState`; the app is left stopped.",
        "pressKey" => "Press a key (`back`, `home`, `enter`, …).",
        "hideKeyboard" => "Hide the soft keyboard if one is showing (Android only for now: Escape, then Back if it stays up); a no-op when none is.",
        "wait" => "Sleep `ms` milliseconds. Prefer an assertion: it waits only as long as needed.",
        "runFlow" => "Run the steps of a flow from `flows:` (by id).",
        "tapXY" => "Tap screen coordinates (no element lookup or actionability checks).",
        "inputRaw" => "Type text into whatever has focus.",
        "goto" => "Open a URL: on the web, navigate the page; on a device, open it as a deep link.",
        "openLink" => "Same as `goto` (Maestro's name).",
        "goBack" => "Web only: the browser's back button (on a device use `pressKey: back`).",
        "refresh" => "Web only: reload the page.",
        "exec" => "Run a program on the host (no shell; a string is split on spaces) that `config.exec.allow` lists; check its exit code (`expect.exitCode`, default 0) and output (`expect.contains`), and `capture` its trimmed output as `${NAME}`. Off unless allowed; `ATALANTA_DEVICE_ID` is set for it.",
        "http" => "Send an HTTP request; check `expect` (status, headers, JSONPath values, body, `maxMs`) and `capture` values as `${NAME}` for later steps. Needs no device.",
        "do" | "act" => "Natural-language action: a model picks one action and one element from the screen; the selector is built and checked in code.",
        "assert" | "expect" => "Natural-language assertion, checked against the element tree.",
        _ => "",
    }
}

/// `docs/DSL-REFERENCE.md`.
pub fn dsl_reference() -> String {
    let schema = crate::schema::suite_schema();
    let defs = &schema["$defs"];
    let mut out = String::new();
    out.push_str(
        "# Atalanta DSL reference\n\n\
         <!-- Generated from crates/atalanta-core/src/schema.rs by `atalanta schema --markdown`.\n     \
         Do not edit by hand: `cargo test -p atalanta-core --test dsl_reference` fails when this\n     \
         file is out of date; `ATALANTA_BLESS=1` on that test rewrites it. -->\n\n\
         Every key a suite file can use. For how the pieces fit together, with examples, see\n\
         [DSL.md](DSL.md). Editors can validate suites with the JSON Schema from\n\
         `atalanta schema -o atalanta.schema.json`.\n\n",
    );

    out.push_str("## Suite file\n\n");
    props_list(&mut out, &schema, &["config", "tests", "flows"]);
    out.push_str("\n### `tests[]`\n\n");
    props_list(&mut out, &schema["properties"]["tests"]["items"], &[]);
    out.push_str("\n### `flows[]`\n\n");
    props_list(&mut out, &schema["properties"]["flows"]["items"], &[]);

    out.push_str(
        "\n## Steps\n\n\
         A step is a map with one action key and optional modifiers, or a plain-English string \
         (a natural-language step).\n\n\
         Modifiers:\n\n",
    );
    for (key, text) in modifiers(defs) {
        let _ = writeln!(out, "- `{key}`: {text}");
    }

    out.push_str("\n## Actions\n\n");
    for action in crate::schema::ACTIONS {
        let variant = action_variant(defs, action);
        let arg = &variant["properties"][*action];
        let _ = writeln!(out, "### `{action}`\n\n{}\n", action_doc(action));
        let _ = writeln!(out, "Argument: {}.", shape(arg));
        let sub = described_props(arg);
        if !sub.is_empty() {
            out.push('\n');
            for (k, s, d) in sub {
                let _ = writeln!(out, "- `{k}` ({s}){}", dash(&d));
            }
        }
        out.push('\n');
    }

    out.push_str(
        "## Selectors\n\n\
         A selector is a map. `css` or `xpath` (web only) stand alone; otherwise at least one of \
         `id`, `text`, `textContains`, `textMatches`, `idMatches`, `accessibilityId`, \
         `className`, `role` or `name` is needed, and the other keys narrow it. Every key given \
         must match.\n\n",
    );
    let native = &defs["selector"]["oneOf"][2]["properties"];
    for (k, v) in native.as_object().into_iter().flatten() {
        let _ = writeln!(
            out,
            "- `{k}` ({}){}",
            shape(v),
            dash(&description(v).unwrap_or_else(|| relation_doc(k).to_string()))
        );
    }
    for k in ["css", "xpath"] {
        let _ = writeln!(out, "- `{k}` (string) - Web only; alone in its selector.");
    }

    out.push_str("\n## `config`\n\n");
    let config = &schema["properties"]["config"];
    for (k, v) in config["properties"].as_object().into_iter().flatten() {
        let _ = writeln!(
            out,
            "- `{k}` ({}){}",
            shape(v),
            dash(&description(v).unwrap_or_default())
        );
    }
    for (section, v) in config["properties"].as_object().into_iter().flatten() {
        let listed: Vec<&String> = v["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
            .collect();
        let parsed = crate::config_keys::section_fields(section).unwrap_or(&[]);
        if listed.is_empty() && parsed.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n### `config.{section}`\n");
        props_list(&mut out, v, &[]);
        // Keys the parser reads that the schema leaves undescribed.
        let extra: Vec<&&str> = parsed
            .iter()
            .filter(|k| {
                !k.contains('_')
                    && !listed.iter().any(|l| l == *k)
                    && !crate::config_keys::is_renamed(section, k)
            })
            .collect();
        if !extra.is_empty() {
            let keys: Vec<String> = extra.iter().map(|k| format!("`{k}`")).collect();
            let _ = writeln!(out, "- Also read: {}.", keys.join(", "));
        }
    }

    out.push_str(
        "\n## Settings\n\n\
         Named settings go under `config: settings:` (flat `android.scroll: semantic` or nested), \
         per run as `atalanta run --set key=value`, per test with `setConfig`, or as the \
         environment variable. `atalanta settings` prints this table with the current values.\n\n\
         | Setting | Values | Default | Variable | What it does |\n\
         |---|---|---|---|---|\n",
    );
    for s in atalanta_common::settings::SETTINGS {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | `{}` | {} |",
            s.key,
            cell(s.values),
            if s.default.is_empty() { "-" } else { s.default },
            s.env,
            cell(s.help)
        );
    }
    out
}

/// The step schema for `action`.
fn action_variant<'a>(defs: &'a Value, action: &str) -> &'a Value {
    defs["step"]["oneOf"]
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|v| v["required"].get(0).and_then(Value::as_str) == Some(action))
        })
        .unwrap_or(&Value::Null)
}

/// Each modifier once, with the actions it applies to.
fn modifiers(defs: &Value) -> Vec<(String, String)> {
    let mut seen: Vec<(String, String, Vec<&str>)> = Vec::new();
    for action in crate::schema::ACTIONS {
        let props = &action_variant(defs, action)["properties"];
        for (k, v) in props.as_object().into_iter().flatten() {
            if k == action || k.contains('_') {
                continue;
            }
            match seen.iter_mut().find(|(key, ..)| key == k) {
                Some((_, _, on)) => on.push(action),
                None => seen.push((
                    k.clone(),
                    format!("{}{}", shape(v), dash(&description(v).unwrap_or_default())),
                    vec![action],
                )),
            }
        }
    }
    let all = crate::schema::ACTIONS.len();
    seen.into_iter()
        .map(|(k, text, on)| {
            let on = if on.len() == all {
                String::new()
            } else {
                let names: Vec<String> = on.iter().map(|a| format!("`{a}`")).collect();
                format!(" Only on {}.", names.join(", "))
            };
            (k, format!("{text}.{on}").replace("..", "."))
        })
        .collect()
}

fn relation_doc(key: &str) -> &'static str {
    match key {
        "below" => "Nearest match below this element",
        "above" => "Nearest match above this element",
        "leftOf" => "Nearest match left of this element",
        "rightOf" => "Nearest match right of this element",
        "containsChild" => "A match with this element inside it",
        "childOf" => "A match inside this element",
        _ => "",
    }
}

/// `- key (shape) - description` for each property; `skip` keys get just
/// their shape (they have sections of their own).
fn props_list(out: &mut String, obj: &Value, skip: &[&str]) {
    let required: Vec<&str> = obj["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (k, v) in obj["properties"].as_object().into_iter().flatten() {
        let req = if required.contains(&k.as_str()) {
            ", required"
        } else {
            ""
        };
        let deprecated = if v["deprecated"] == Value::Bool(true) {
            " Deprecated."
        } else {
            ""
        };
        let shape = if skip.contains(&k.as_str()) {
            "see below".to_string()
        } else {
            shape(v)
        };
        let _ = writeln!(
            out,
            "- `{k}` ({shape}{req}){}{deprecated}",
            dash(&description(v).unwrap_or_default())
        );
    }
}

/// An argument's properties that say something more than their shape,
/// looking through `oneOf`.
fn described_props(v: &Value) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut visit = |o: &Value| {
        for (k, p) in o["properties"].as_object().into_iter().flatten() {
            if k.contains('_') || out.iter().any(|(n, ..): &(String, _, _)| n == k) {
                continue;
            }
            out.push((k.clone(), shape(p), description(p).unwrap_or_default()));
        }
    };
    visit(v);
    for alt in v["oneOf"].as_array().into_iter().flatten() {
        visit(alt);
    }
    // Inline selector fields (longPress) are the selector's own keys.
    if out.iter().any(|(k, ..)| k == "text") && out.iter().any(|(k, ..)| k == "durationMs") {
        out.retain(|(k, ..)| k == "selector" || k == "durationMs");
        out.push((
            "…".into(),
            "selector keys".into(),
            "or the selector's keys inline, beside `durationMs`".into(),
        ));
    }
    out
}

fn description(v: &Value) -> Option<String> {
    v["description"].as_str().map(str::to_string)
}

fn dash(d: &str) -> String {
    if d.is_empty() {
        String::new()
    } else {
        format!(" - {}", d.trim_end_matches('.'))
    }
}

/// A Markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// A short human form of a schema: `selector`, `string`, `one of a, b`, …
fn shape(v: &Value) -> String {
    if v.get("$ref").and_then(Value::as_str) == Some("#/$defs/selector") {
        return "selector".into();
    }
    if v.get("$ref").and_then(Value::as_str) == Some("#/$defs/step") {
        return "step".into();
    }
    if let Some(values) = v.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = values
            .iter()
            .map(|x| match x {
                Value::String(s) => format!("`{s}`"),
                other => format!("`{other}`"),
            })
            .collect();
        return format!("one of {}", names.join(", "));
    }
    if let Some(alts) = v
        .get("oneOf")
        .or_else(|| v.get("anyOf"))
        .and_then(Value::as_array)
    {
        if v.get("type").is_none() {
            let mut shapes: Vec<String> = Vec::new();
            for a in alts {
                let s = shape(a);
                if !shapes.contains(&s) {
                    shapes.push(s);
                }
            }
            return shapes.join(" or ");
        }
    }
    match v.get("type") {
        Some(Value::String(t)) => match t.as_str() {
            "null" => "nothing".into(),
            "array" => format!("list of {}", shape(&v["items"])),
            "object" => match v.get("properties").and_then(Value::as_object) {
                // Long key lists (an inline selector, a config section) are
                // listed underneath instead.
                Some(p) if p.len() > 10 => "map (keys below)".into(),
                Some(p) if !p.is_empty() => {
                    let req: Vec<&str> = v["required"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let keys: Vec<String> = p
                        .keys()
                        .filter(|k| !k.contains('_'))
                        .map(|k| {
                            if req.contains(&k.as_str()) {
                                format!("`{k}`")
                            } else {
                                format!("`{k}`?")
                            }
                        })
                        .collect();
                    format!("map: {}", keys.join(", "))
                }
                _ if v.get("additionalProperties").is_some_and(Value::is_object) => {
                    "map of key to value".into()
                }
                _ => "map".into(),
            },
            "integer" => match v.get("minimum").and_then(Value::as_i64) {
                Some(0) => "integer ≥ 0".into(),
                Some(n) => format!("integer ≥ {n}"),
                None => "integer".into(),
            },
            other => other.into(),
        },
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "any value".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_line() {
        for a in crate::schema::ACTIONS {
            assert!(!action_doc(a).is_empty(), "no doc line for `{a}`");
        }
    }

    #[test]
    fn the_reference_names_every_action_selector_key_and_setting() {
        let doc = dsl_reference();
        for a in crate::schema::ACTIONS {
            assert!(doc.contains(&format!("### `{a}`")), "{a}");
        }
        for k in [
            "id",
            "text",
            "textContains",
            "textMatches",
            "idMatches",
            "accessibilityId",
            "className",
            "role",
            "name",
            "checked",
            "focused",
            "selected",
            "enabled",
            "index",
            "css",
            "xpath",
            "below",
            "above",
            "leftOf",
            "rightOf",
            "containsChild",
            "childOf",
        ] {
            assert!(doc.contains(&format!("- `{k}` (")), "selector key {k}");
        }
        for s in atalanta_common::settings::SETTINGS {
            assert!(doc.contains(&format!("| `{}` |", s.key)), "{}", s.key);
        }
        for section in [
            "sync",
            "retry",
            "artifacts",
            "dialog",
            "healing",
            "web",
            "nl",
            "ai",
        ] {
            assert!(
                doc.contains(&format!("### `config.{section}`")),
                "{section}"
            );
            for k in crate::config_keys::section_fields(section).unwrap() {
                if !k.contains('_') {
                    assert!(doc.contains(&format!("`{k}`")), "config.{section}.{k}");
                }
            }
        }
    }
}
