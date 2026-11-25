// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta validate` for one suite, on a string, for the playground.

use std::collections::HashMap;

mod plan;

use atalanta_common::{AtalantaError, SourcePos};
use atalanta_core::{
    config_keys, parse_suite_from_str, resolve_flows, source_map, validate_suite_at, validator,
};
use serde::Serialize;
use wasm_bindgen::prelude::wasm_bindgen;

#[derive(Debug, Serialize, PartialEq)]
pub struct Diagnostic {
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub severity: &'static str,
    pub message: String,
}

impl Diagnostic {
    fn new(severity: &'static str, line: usize, column: usize, message: String) -> Self {
        Self {
            line: (line > 0).then_some(line),
            column: (line > 0 && column > 0).then_some(column),
            severity,
            message,
        }
    }

    fn error(e: &AtalantaError) -> Self {
        match e {
            AtalantaError::YamlParse {
                line, col, message, ..
            } => Self::new("error", *line, *col, message.clone()),
            AtalantaError::AtSource { pos, step, inner } => {
                Self::new("error", pos.line, pos.column, format!("{step}: {inner}"))
            }
            other => {
                let text = other.to_string();
                let text = text.strip_prefix("Configuration error: ").unwrap_or(&text);
                Self::new("error", 0, 0, text.to_string())
            }
        }
    }

    fn warning(message: String) -> Self {
        Self::new("warning", 0, 0, message)
    }
}

pub fn check(yaml: &str) -> Vec<Diagnostic> {
    let suite = match parse_suite_from_str(yaml) {
        Ok(s) => s,
        Err(e) => return vec![Diagnostic::error(&e)],
    };
    let yaml_text = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let locations = source_map::locate(yaml_text).unwrap_or_default();
    let tests: HashMap<String, SourcePos> = suite
        .tests
        .iter()
        .zip(&locations.tests)
        .filter_map(|(t, loc)| {
            let at = loc.at?;
            Some((t.name.clone(), SourcePos::new("", at.line, at.column)))
        })
        .collect();

    let mut out = Vec::new();
    if let Err(e) = validate_suite_at(&suite, &tests) {
        out.push(Diagnostic::error(&e));
    } else if let Err(e) = resolve_flows(&suite) {
        out.push(Diagnostic::error(&e));
    }
    out.extend(
        config_keys::unknown_keys(yaml_text)
            .into_iter()
            .map(Diagnostic::warning),
    );
    out.extend(
        validator::suite_warnings(&suite)
            .into_iter()
            .map(Diagnostic::warning),
    );
    out
}

fn to_json(items: &[Diagnostic]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

/// A JSON array of `{ line, column, severity, message }`.
#[wasm_bindgen]
pub fn validate(yaml: &str) -> String {
    let items = std::panic::catch_unwind(|| check(yaml)).unwrap_or_else(|_| {
        vec![Diagnostic::new(
            "error",
            0,
            0,
            "the validator failed on this input".to_string(),
        )]
    });
    to_json(&items)
}

fn guarded(f: impl FnOnce() -> serde_json::Value + std::panic::UnwindSafe) -> String {
    let value = std::panic::catch_unwind(f).unwrap_or_else(|_| {
        serde_json::json!({"ok": false, "kind": "error", "passed": false,
            "error": "failed on this input", "message": "failed on this input",
            "failures": ["failed on this input"], "captured": {}, "tests": []})
    });
    value.to_string()
}

/// The suite as a plan: its tests and steps (flows expanded), the target
/// and whether the playground can run it.
#[wasm_bindgen]
pub fn plan(yaml: &str) -> String {
    guarded(|| plan::plan(yaml))
}

/// What to send for one `http` step (or `wait`), with the values earlier
/// steps captured (`vars_json`: `{ "NAME": "value" }`).
#[wasm_bindgen]
pub fn prepare_step(yaml: &str, test_index: usize, step_index: usize, vars_json: &str) -> String {
    guarded(|| plan::prepare_step(yaml, test_index, step_index, vars_json))
}

/// The response (`{ status, headers, body, elapsedMs }`) judged against the
/// step's `expect`, with the values its `capture:` takes.
#[wasm_bindgen]
pub fn check_step(
    yaml: &str,
    test_index: usize,
    step_index: usize,
    vars_json: &str,
    response_json: &str,
) -> String {
    guarded(|| plan::check_step(yaml, test_index, step_index, vars_json, response_json))
}

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(yaml: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(&validate(yaml)).unwrap()
    }

    fn errors(yaml: &str) -> Vec<serde_json::Value> {
        run(yaml)
            .into_iter()
            .filter(|d| d["severity"] == "error")
            .collect()
    }

    #[test]
    fn the_api_example_has_no_errors() {
        let yaml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/api-example/atalanta.yaml"
        ))
        .unwrap();
        assert!(errors(&yaml).is_empty(), "{:?}", run(&yaml));
    }

    #[test]
    fn a_syntax_error_is_one_error_with_a_line() {
        let e = errors("appId: x\ntests: [\n  - name: a\n    steps: {");
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0]["line"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn an_unknown_step_key_is_an_error_on_its_line() {
        let yaml =
            "appId: x\ntests:\n  - name: a\n    steps:\n      - launchApp\n      - bogusStep: 1\n";
        let e = errors(yaml);
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0]["line"], 6, "{e:?}");
    }

    #[test]
    fn a_check_beyond_the_schema_is_an_error_with_a_line() {
        let yaml = "appId: x\ntests:\n  - name: a\n    steps:\n      - launchApp\n      - setConfig: { key: nope.nope, value: \"1\" }\n";
        let e = errors(yaml);
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0]["line"], 6, "{e:?}");
    }

    #[test]
    fn odd_input_does_not_panic() {
        for s in [
            "",
            "\u{0}",
            "{{{{",
            "- - - -",
            "tests: 5",
            "\u{feff}",
            "a: &a [*a]",
        ] {
            let _ = validate(s);
        }
    }

    #[test]
    fn version_is_the_crate_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
