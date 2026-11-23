// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Maestro flow → Atalanta suite.
//!
//! A Maestro file is usually two YAML documents: a header (`appId`, `name`,
//! `tags`) and the list of steps. Steps are either a bare command
//! (`- launchApp`, `- back`) or a one-key map (`- tapOn: "Login"`). Each one
//! is converted to the Atalanta DSL the parser reads, and a `runFlow` of
//! another file is converted too and added to the suite's `flows:`, so the
//! output runs as it is. Anything that can't be converted is reported (with
//! the step's line) and left out; the output is written only when nothing
//! failed outright.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use atalanta_common::{AtalantaError, Result};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::compat::{FileMigrationResult, MigrationIssue, MigrationReport, Severity};

/// Maestro commands with no Atalanta equivalent: reported and skipped.
const UNSUPPORTED_CONSTRUCTS: &[&str] = &[
    "runScript",
    "evalScript",
    "defineVariables",
    "repeat",
    "condition",
    "assertTrue",
    "assertNoDefectsWithAI",
    "copyTextFrom",
    "pasteText",
    "travel",
    "startRecording",
    "stopRecording",
    "addMedia",
    "setAirplaneMode",
    "toggleAirplaneMode",
    "clearKeychain",
    "setOrientation",
    "setPermissions",
    "extractTextWithAI",
];

/// Commands that need no step in Atalanta (its sync engine already waits).
const NOT_NEEDED: &[&str] = &["waitForAnimationToEnd", "hideKeyboard"];

pub struct MaestroMigrator;

impl Default for MaestroMigrator {
    fn default() -> Self {
        Self::new()
    }
}

/// One file's conversion, before it is written.
struct Converted {
    app_id: String,
    name: String,
    tags: Vec<String>,
    steps: Vec<Value>,
    flows: BTreeMap<String, Vec<Value>>,
    issues: Vec<MigrationIssue>,
    skipped: usize,
    /// Host programs the converted `exec` steps run (`setLocation`): the
    /// suite's `config.exec.allow`.
    exec_allow: std::collections::BTreeSet<String>,
}

/// Header keys that are read (or that need nothing in Atalanta).
const KNOWN_HEADER_KEYS: &[&str] = &[
    "appId",
    "name",
    "tags",
    "env",
    "onFlowStart",
    "onFlowComplete",
];

impl MaestroMigrator {
    pub fn new() -> Self {
        Self
    }

    pub fn migrate_directory(&self, input_dir: &str, output_dir: &str) -> Result<MigrationReport> {
        let input_path = Path::new(input_dir);
        if !input_path.is_dir() {
            return Err(AtalantaError::Config(format!(
                "Input directory does not exist: {input_dir}"
            )));
        }
        let output_path = Path::new(output_dir);
        fs::create_dir_all(output_path).map_err(|e| {
            AtalantaError::Internal(anyhow::anyhow!("Failed to create output directory: {e}"))
        })?;

        let mut files: Vec<PathBuf> = fs::read_dir(input_path)
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Failed to read directory: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
            .collect();
        files.sort();

        let mut report = MigrationReport::new();
        let mut used_names = HashSet::new();
        for path in files {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            // flow.yaml and flow.yml must not write the same file (VEL-239).
            let mut name = format!("{stem}.atalanta.yaml");
            if !used_names.insert(name.clone()) {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                name = format!("{stem}.{ext}.atalanta.yaml");
                used_names.insert(name.clone());
            }
            let output = output_path.join(name);
            let result = self.migrate_file(&path.to_string_lossy(), &output.to_string_lossy())?;
            report.add_result(result);
        }
        Ok(report)
    }

    /// `migrate maestro --dry-run`: convert every flow in `input_dir`
    /// (top level, as [`Self::migrate_directory`]) and write nothing.
    pub fn dry_run_directory(&self, input_dir: &str) -> Result<MigrationReport> {
        let input_path = Path::new(input_dir);
        if !input_path.is_dir() {
            return Err(AtalantaError::Config(format!(
                "Input directory does not exist: {input_dir}"
            )));
        }
        let mut files: Vec<PathBuf> = fs::read_dir(input_path)
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Failed to read directory: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
            .collect();
        files.sort();
        let mut report = MigrationReport::new();
        for path in files {
            report.add_result(self.convert_checked(&path.to_string_lossy(), None)?);
        }
        Ok(report)
    }

    pub fn migrate_file(&self, input: &str, output: &str) -> Result<FileMigrationResult> {
        self.convert_checked(input, Some(output))
    }

    /// Convert `input`, check that the result loads, and write it to
    /// `output` when there is one and it does.
    fn convert_checked(&self, input: &str, output: Option<&str>) -> Result<FileMigrationResult> {
        let mut converted = self.convert_file(Path::new(input))?;
        let steps_migrated = converted.steps.len();
        if steps_migrated == 0 {
            // A test with no steps passes without doing anything (a Maestro
            // `config.yaml`, an empty file, a flow whose commands were all
            // skipped).
            converted.issues.push(issue(
                0,
                Severity::Warning,
                "steps",
                "no step was converted: the migrated test is empty and passes without testing anything",
            ));
        }
        let mut success = converted
            .issues
            .iter()
            .all(|i| i.severity != Severity::Error);
        let yaml = build_atalanta_yaml(&converted);
        // Success means the output runs: parse and validate it the way
        // `atalanta run` will (VEL-080).
        if success {
            if let Err(e) = check_output(&yaml) {
                converted.issues.push(issue(
                    0,
                    Severity::Error,
                    "output",
                    &format!("the migrated suite doesn't load in Atalanta, nothing written: {e}"),
                ));
                success = false;
            }
        }
        if let (true, Some(output)) = (success, output) {
            fs::write(output, yaml).map_err(|e| {
                AtalantaError::Internal(anyhow::anyhow!("Failed to write {output}: {e}"))
            })?;
        }
        Ok(FileMigrationResult {
            source_file: input.to_string(),
            output_file: output.filter(|_| success).map(String::from),
            success,
            steps_migrated,
            steps_skipped: converted.skipped,
            issues: converted.issues,
        })
    }

    /// Convert `input` (and the files it `runFlow`s) without writing.
    fn convert_file(&self, input: &Path) -> Result<Converted> {
        let mut flow_ids = HashMap::new();
        let (header, steps, lines) = read_flow(input)?;
        let stem = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("migrated")
            .to_string();
        let mut out = Converted {
            app_id: header
                .get("appId")
                .and_then(Value::as_str)
                .unwrap_or("com.example.app")
                .to_string(),
            name: header
                .get("name")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or(stem),
            tags: header
                .get("tags")
                .and_then(Value::as_sequence)
                .map(|t| {
                    t.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            steps: Vec::new(),
            flows: BTreeMap::new(),
            issues: Vec::new(),
            skipped: 0,
            exec_allow: Default::default(),
        };
        if header.get("appId").and_then(Value::as_str).is_none() {
            out.issues.push(issue(
                lines.header,
                Severity::Warning,
                "appId",
                "the flow has no `appId`: `com.example.app` is a placeholder, set the suite's appId",
            ));
        }
        if !out.tags.iter().any(|t| t == "migrated") {
            out.tags.push("migrated".into());
        }
        let env = header
            .get("env")
            .and_then(Value::as_mapping)
            .map(env_defaults);
        if let Some((ok, left)) = &env {
            let mut note = String::new();
            if !ok.is_empty() {
                note = format!(
                    "Maestro `env:` became defaults: ${{NAME}} is ${{NAME:-value}} for {} (`--env` still overrides)",
                    ok.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
                );
            }
            if !left.is_empty() {
                if !note.is_empty() {
                    note.push_str("; ");
                }
                note.push_str(&format!(
                    "pass {} with `atalanta run --env KEY=VALUE` (the value can't be a default)",
                    left.join(", ")
                ));
            }
            out.issues
                .push(issue(lines.header, Severity::Info, "env", &note));
        } else if header.contains_key("env") {
            out.issues.push(issue(
                lines.header,
                Severity::Info,
                "env",
                "Maestro `env:` values aren't carried over; pass them with `atalanta run --env KEY=VALUE`",
            ));
        }
        let mut visiting = HashSet::from([canonical(input)]);
        let mut unrolled = 0usize;
        let mut ctx = Ctx {
            dir: input.parent().unwrap_or(Path::new(".")).to_path_buf(),
            issues: &mut out.issues,
            flows: &mut out.flows,
            flow_ids: &mut flow_ids,
            visiting: &mut visiting,
            skipped: &mut out.skipped,
            exec_allow: &mut out.exec_allow,
            unrolled: &mut unrolled,
            app_id: out.app_id.clone(),
        };
        out.steps = convert_flow(&header, &steps, &lines, &mut ctx);
        if let Some((defaults, _)) = env.filter(|(ok, _)| !ok.is_empty()) {
            out.steps
                .iter_mut()
                .for_each(|st| apply_env_defaults(st, &defaults));
            for steps in out.flows.values_mut() {
                steps
                    .iter_mut()
                    .for_each(|st| apply_env_defaults(st, &defaults));
            }
        }
        Ok(out)
    }
}

/// Mutable state while converting one file and its sub-flows.
struct Ctx<'a> {
    dir: PathBuf,
    issues: &'a mut Vec<MigrationIssue>,
    flows: &'a mut BTreeMap<String, Vec<Value>>,
    /// The flow id given to each sub-flow file, by canonical path: two files
    /// with the same name are two flows (VEL-017).
    flow_ids: &'a mut HashMap<PathBuf, String>,
    /// Files being converted right now (a `runFlow` cycle guard).
    visiting: &'a mut HashSet<PathBuf>,
    skipped: &'a mut usize,
    exec_allow: &'a mut std::collections::BTreeSet<String>,
    /// Steps produced by unrolling `repeat` blocks so far, across the file
    /// and its sub-flows: nested repeats multiply, so each one counts.
    unrolled: &'a mut usize,
    /// The top flow's `appId`, for commands that default to it
    /// (`setPermissions`).
    app_id: String,
}

/// Line numbers for reporting: the header's, and each step's.
struct Lines {
    header: usize,
    steps: Vec<usize>,
}

/// The header mapping and the step list of a Maestro file (either one or
/// two YAML documents), with each step's line.
fn read_flow(path: &Path) -> Result<(Mapping, Vec<Value>, Lines)> {
    let file = path.to_string_lossy().to_string();
    // A missing, unreadable or non-UTF-8 flow is the user's input, not an
    // internal fault: exit 2, not 5.
    let content = fs::read_to_string(path)
        .map_err(|e| AtalantaError::Config(format!("Failed to read {file}: {e}")))?;
    let mut docs = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(&content) {
        let v = Value::deserialize(doc).map_err(|e| {
            let loc = e.location();
            AtalantaError::YamlParse {
                file: file.clone().into(),
                line: loc.as_ref().map_or(0, |l| l.line()),
                col: loc.as_ref().map_or(0, |l| l.column()),
                message: e.to_string(),
            }
        })?;
        docs.push(v);
    }
    let mut header = Mapping::new();
    let mut steps = Vec::new();
    for d in docs {
        match d {
            Value::Mapping(m) if steps.is_empty() => header.extend(m),
            Value::Sequence(s) => steps.extend(s),
            _ => {}
        }
    }
    Ok((
        header,
        steps,
        Lines {
            header: 1,
            steps: step_lines(&content),
        },
    ))
}

/// The line of each top-level `- ` item of the step list documents (the
/// ones whose first content line is a list item; the header is a mapping).
/// A file may open with `---`, and the header may hold `- ` items at column
/// 0 (`tags:` written without indent): only the step documents count. The
/// items sit at the indent of the document's first one.
fn step_lines(content: &str) -> Vec<usize> {
    let mut out = Vec::new();
    // `Some(indent)` once the current document is known to be a step list.
    let mut list_indent: Option<usize> = None;
    let mut seen_content = false;
    for (i, l) in content.lines().enumerate() {
        let t = l.trim_end();
        if t == "---" || t.starts_with("--- ") {
            list_indent = None;
            seen_content = false;
            continue;
        }
        let body = t.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = t.len() - body.len();
        let is_item = body == "-" || body.starts_with("- ");
        if !seen_content {
            seen_content = true;
            if is_item {
                list_indent = Some(indent);
            }
        }
        if is_item && list_indent == Some(indent) {
            out.push(i + 1);
        }
    }
    out
}

/// `launchApp.arguments` without the names Atalanta rejects (a leading `-`,
/// characters outside `[A-Za-z0-9_.-]`): each one is dropped with a warning,
/// so the rest of the file still converts.
fn launch_arguments(args: Value, line: usize, ctx: &mut Ctx) -> Value {
    let Value::Mapping(m) = args else {
        return args;
    };
    let mut kept = Mapping::new();
    for (k, v) in m {
        let invalid = match &k {
            Value::String(name) => atalanta_common::validate_launch_arg_name(name).err(),
            _ => None,
        };
        match invalid {
            Some(why) => ctx.issues.push(issue(
                line,
                Severity::Warning,
                "launchApp",
                &format!("launchApp argument dropped: {why}"),
            )),
            None => {
                kept.insert(k, v);
            }
        }
    }
    Value::Mapping(kept)
}

/// `text` with control characters (an escape sequence from a hostile flow's
/// command name or key) written out as `\u{1b}`, so a report printed to a
/// terminal can't redraw it.
fn printable(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn issue(line: usize, severity: Severity, construct: &str, message: &str) -> MigrationIssue {
    MigrationIssue {
        line,
        severity,
        message: printable(message),
        maestro_construct: printable(construct),
    }
}

/// A whole flow file: its steps, with the header's `onFlowStart` commands
/// before them and `onFlowComplete` after (VEL-081). Header keys Atalanta
/// doesn't read are reported.
fn convert_flow(header: &Mapping, steps: &[Value], lines: &Lines, ctx: &mut Ctx) -> Vec<Value> {
    for k in header.keys().filter_map(Value::as_str) {
        if !KNOWN_HEADER_KEYS.contains(&k) {
            ctx.issues.push(issue(
                lines.header,
                Severity::Warning,
                k,
                &format!("header key '{k}' isn't carried over"),
            ));
        }
    }
    let hook = |key: &str, ctx: &mut Ctx| -> Vec<Value> {
        match header.get(key) {
            None => Vec::new(),
            Some(Value::Sequence(cmds)) => {
                convert_steps(cmds, &vec![lines.header; cmds.len()], ctx)
            }
            Some(_) => {
                ctx.issues.push(issue(
                    lines.header,
                    Severity::Warning,
                    key,
                    &format!("'{key}' isn't a list of commands; skipped"),
                ));
                Vec::new()
            }
        }
    };
    let mut out = hook("onFlowStart", ctx);
    out.extend(convert_steps(steps, &lines.steps, ctx));
    let complete = hook("onFlowComplete", ctx);
    if !complete.is_empty() {
        ctx.issues.push(issue(
            lines.header,
            Severity::Warning,
            "onFlowComplete",
            "onFlowComplete became the flow's last steps: they run only when the steps before them pass (Maestro runs them always)",
        ));
    }
    out.extend(complete);
    out
}

/// Whether a Maestro step is a `back` press (`back`, or `pressKey: back`).
fn is_back(command: &str, arg: &Value) -> bool {
    let key = || {
        arg.as_str()
            .or_else(|| arg.get("key").and_then(Value::as_str))
            .map(|k| k.trim().eq_ignore_ascii_case("back"))
            .unwrap_or(false)
    };
    command == "back" || (command == "pressKey" && key())
}

fn convert_steps(steps: &[Value], lines: &[usize], ctx: &mut Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    let mut after_input = false;
    let mut after_long_press = false;
    for (i, item) in steps.iter().enumerate() {
        let line = lines.get(i).copied().unwrap_or(0);
        // `- back` and `- back: {}` mean the same.
        let (command, arg) = match item {
            Value::String(s) => (s.as_str(), Value::Null),
            Value::Mapping(m) if m.len() == 1 => match m.iter().next() {
                Some((Value::String(k), v)) => (k.as_str(), v.clone()),
                _ => {
                    ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        "?",
                        "Unrecognised step skipped (its command isn't a name)",
                    ));
                    *ctx.skipped += 1;
                    continue;
                }
            },
            _ => {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    "?",
                    "Unrecognised step skipped",
                ));
                *ctx.skipped += 1;
                continue;
            }
        };
        // `back` straight after typing usually closed the keyboard in
        // Maestro. In Atalanta it is navigation everywhere: history back on
        // the web, which leaves the page (D-9).
        if after_input && is_back(command, &arg) {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                "`back` straight after `inputText`: if it only closed the keyboard, drop it (Atalanta doesn't need hideKeyboard); on the web `back` goes back a page, and `pressKey: backspace` deletes a character",
            ));
        }
        after_input = command.starts_with("input");
        // `longPressOn` then `swipe` is Maestro's drag-to-reorder workaround
        // (issue #1203). Atalanta's `drag` holds and moves one element onto
        // another, which a swipe's screen points can't be turned into.
        if after_long_press && command == "swipe" {
            ctx.issues.push(issue(
                line,
                Severity::Info,
                command,
                "`longPressOn` then `swipe` looks like a drag-to-reorder workaround: kept as a swipe; \
                 Atalanta's `drag: { from: <row>, to: <row> }` picks the row up and drops it on another \
                 (docs/DSL.md#reordering-drag)",
            ));
        }
        after_long_press = command == "longPressOn";
        if command == "repeat" {
            if let Some(body) = unroll_repeat(&arg, line, ctx) {
                out.extend(body);
                continue;
            }
        }
        if command == "retry" {
            out.extend(inline_retry(&arg, line, ctx));
            continue;
        }
        if command == "setLocation" {
            match convert_set_location(&arg, line, ctx) {
                Some(body) => out.extend(body),
                None => *ctx.skipped += 1,
            }
            continue;
        }
        // Device settings Maestro sets itself become `exec` steps with adb /
        // xcrun simctl; what can't be converted falls through to the
        // unsupported report below.
        if let Some(body) = convert_device_command(command, &arg, line, ctx) {
            out.extend(body);
            continue;
        }
        if command == "eraseText" {
            match erase_text(&arg, line, ctx) {
                Some(body) => out.extend(body),
                None => *ctx.skipped += 1,
            }
            continue;
        }
        match convert_step(command, &arg, line, ctx) {
            Some(mut s) => {
                // Maestro's `optional: true` is Atalanta's step-level
                // `optional: true`: a failure is a warning (D-12).
                if arg
                    .as_mapping()
                    .and_then(|m| m.get("optional"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    if let Value::Mapping(m) = &mut s {
                        m.insert(Value::from("optional"), Value::from(true));
                    }
                }
                let times = tap_repeat(command, &arg, line, ctx);
                for i in 0..times {
                    if i > 0 {
                        if let Some(ms) = arg.as_mapping().and_then(|m| m.get("delay")) {
                            if let Some(ms) = as_number(ms, line, "delay", ctx) {
                                out.push(step("wait", map(vec![("ms", Value::from(ms))])));
                            }
                        }
                    }
                    out.push(s.clone());
                }
            }
            None => *ctx.skipped += 1,
        }
    }
    out
}

/// `retry: {maxRetries, commands}`: the body, once, with a warning. Atalanta
/// retries whole tests (`retry.count`), not blocks; skipping the block
/// emptied the flow and the migration still reported success (VEL-240).
fn inline_retry(arg: &Value, line: usize, ctx: &mut Ctx) -> Vec<Value> {
    let commands = arg
        .as_mapping()
        .and_then(|m| m.get("commands"))
        .and_then(Value::as_sequence);
    let Some(commands) = commands else {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "retry",
            "retry without inline commands (a `file:` retry) can't be converted; skipped",
        ));
        *ctx.skipped += 1;
        return vec![];
    };
    warn_unused(
        arg,
        &["maxRetries", "commands", "label"],
        line,
        "retry",
        ctx,
    );
    let lines = vec![line; commands.len()];
    let body = convert_steps(commands, &lines, ctx);
    ctx.issues.push(issue(
        line,
        Severity::Warning,
        "retry",
        &format!(
            "retry block inlined: its {} step(s) run once; use `retry: {{count: N}}` in the suite config to retry the whole test",
            body.len()
        ),
    ));
    body
}

/// Characters Maestro's `eraseText` deletes when the flow gives no count.
const MAESTRO_ERASE_DEFAULT: u64 = 50;

/// `eraseText` (`charactersToErase`, default 50) deletes that many
/// characters before the cursor of the focused field: the same number of
/// `pressKey: backspace` steps. Over `MAX_UNROLL` it is skipped.
fn erase_text(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Vec<Value>> {
    let count = match arg {
        Value::Null => Some(MAESTRO_ERASE_DEFAULT),
        Value::Mapping(m) => {
            warn_unused(arg, &["charactersToErase", "label"], line, "eraseText", ctx);
            match m.get("charactersToErase") {
                None => Some(MAESTRO_ERASE_DEFAULT),
                Some(v) => as_number(v, line, "charactersToErase", ctx),
            }
        }
        v => as_number(v, line, "charactersToErase", ctx),
    };
    let Some(n) = count else {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "eraseText",
            "eraseText without a usable count; skipped",
        ));
        return None;
    };
    if n > MAX_UNROLL {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "eraseText",
            &format!(
                "eraseText of {n} characters is over {MAX_UNROLL}; skipped (use clearText on the field)"
            ),
        ));
        return None;
    }
    let optional = arg
        .as_mapping()
        .and_then(|m| m.get("optional"))
        .and_then(Value::as_bool)
        == Some(true);
    let mut press = step("pressKey", map(vec![("key", Value::from("backspace"))]));
    if let (true, Value::Mapping(m)) = (optional, &mut press) {
        m.insert(Value::from("optional"), Value::from(true));
    }
    ctx.issues.push(issue(
        line,
        Severity::Info,
        "eraseText",
        &format!(
            "eraseText became {n} `pressKey: backspace` steps; to empty a field, `clearText` on it is shorter"
        ),
    ));
    Some(vec![press; n as usize])
}

/// Maestro's default `scrollUntilVisible` timeout.
const MAESTRO_SCROLL_TIMEOUT_MS: u64 = 20_000;

/// Most `repeat` blocks to unroll; more would bloat the suite.
const MAX_UNROLL: u64 = 100;

/// Most steps all the `repeat` blocks of one file may unroll to. Nested
/// blocks multiply (100 x 100 x 100 is a million steps from a few lines),
/// and `atalanta run` refuses a test of more than 100 000 steps.
const MAX_UNROLLED_STEPS: usize = 100_000;

/// `repeat: { times: N, commands: [...] }` with no `while` is exactly the
/// commands N times over, so it is unrolled. It used to be skipped with a
/// warning, and the converted test then asserted on work it never did (a
/// 10-tap counter test checked "Count: 10" with no taps). A `while` loop
/// depends on the screen at run time and stays unsupported (`None`).
fn unroll_repeat(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Vec<Value>> {
    let m = arg.as_mapping()?;
    if m.contains_key("while") {
        return None;
    }
    let times = match m.get("times")? {
        Value::Number(n) => n.as_u64()?,
        Value::String(t) => t.trim().parse().ok()?,
        _ => return None,
    };
    let commands = m.get("commands")?.as_sequence()?;
    if times > MAX_UNROLL {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "repeat",
            &format!("repeat of {times} is over {MAX_UNROLL}; skipped"),
        ));
        *ctx.skipped += 1;
        return Some(vec![]);
    }
    let lines = vec![line; commands.len()];
    let once = convert_steps(commands, &lines, ctx);
    let total = once.len().saturating_mul(times as usize);
    if ctx.unrolled.saturating_add(total) > MAX_UNROLLED_STEPS {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "repeat",
            &format!(
                "repeat of {times} x {} step(s) would take the file past {MAX_UNROLLED_STEPS} unrolled steps; skipped",
                once.len()
            ),
        ));
        *ctx.skipped += 1;
        return Some(vec![]);
    }
    *ctx.unrolled += total;
    ctx.issues.push(issue(
        line,
        Severity::Info,
        "repeat",
        &format!("repeat unrolled: {} step(s) x {times}", once.len()),
    ));
    Some((0..times).flat_map(|_| once.clone()).collect())
}

/// How many times a tap command runs: Maestro's `tapOn: {.., repeat: N}`
/// taps N times, so the tap is unrolled (VEL-345).
fn tap_repeat(command: &str, arg: &Value, line: usize, ctx: &mut Ctx) -> u64 {
    if !matches!(command, "tapOn" | "doubleTapOn" | "longPressOn") {
        return 1;
    }
    let Some(r) = arg.as_mapping().and_then(|m| m.get("repeat")) else {
        return 1;
    };
    match as_number(r, line, "repeat", ctx) {
        Some(n) if n > MAX_UNROLL => {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                &format!("repeat of {n} is over {MAX_UNROLL}; the tap runs once"),
            ));
            1
        }
        Some(n) if n > 1 => {
            ctx.issues.push(issue(
                line,
                Severity::Info,
                command,
                &format!("repeat unrolled: {n} taps"),
            ));
            n
        }
        _ => 1,
    }
}

/// A number field written as a number or a numeric string. Anything else
/// (`${VAR}`, an expression) can't go into a numeric Atalanta field: it is
/// reported and dropped (VEL-080).
fn as_number(v: &Value, line: usize, field: &str, ctx: &mut Ctx) -> Option<u64> {
    let n = match v {
        Value::Number(n) => n.as_u64(),
        Value::String(t) => t.trim().parse().ok(),
        _ => None,
    };
    if n.is_none() {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            field,
            &format!(
                "'{field}' must be a number in Atalanta, got {}; dropped",
                yaml_inline(v)
            ),
        ));
    }
    n
}

fn yaml_inline(v: &Value) -> String {
    serde_yaml::to_string(v)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Report the keys of a command's map that weren't converted (VEL-081).
fn warn_unused(arg: &Value, known: &[&str], line: usize, command: &str, ctx: &mut Ctx) {
    let Some(m) = arg.as_mapping() else { return };
    for k in m.keys().filter_map(Value::as_str) {
        // Carried over as the step's `optional:` (see `convert_steps`).
        if !known.contains(&k) && k != "optional" {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                &format!("'{k}' of {command} isn't carried over"),
            ));
        }
    }
}

/// A one-key step map, optionally with a step-level `timeoutMs`.
fn step(action: &str, arg: Value) -> Value {
    let mut m = Mapping::new();
    m.insert(Value::from(action), arg);
    Value::Mapping(m)
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Mapping(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

/// Why an unsupported command was skipped, and what to write instead.
fn unsupported_hint(command: &str, arg: &Value) -> &'static str {
    match command {
        "runScript" | "evalScript" => {
            ": Atalanta runs no JavaScript. An HTTP call the script made becomes an `http` \
             step (its `capture:` replaces `output.*`; docs/DSL.md#http-and-api-tests); a \
             script that seeds data on the host can run as an `exec` step \
             (`exec: { run: \"./seed.sh\", capture: NAME }`, the program listed in \
             config.exec.allow; docs/DSL.md#host-commands-exec)"
        }
        "copyTextFrom" => {
            ": Atalanta can't read text off the screen into a variable. Get the value where \
             the app got it: an `http` step's `capture:` (the order id from the API), an \
             `exec` step's `capture:`, or `--env NAME=value`; then use `${NAME}` \
             (inputText: ${NAME}, assertVisible: ${NAME})"
        }
        "pasteText" => {
            ": it pastes what copyTextFrom read, which doesn't convert; type the value with \
             `inputText: ${NAME}` once it is captured (see the copyTextFrom note)"
        }
        "travel" => {
            ": write one `exec` step per point instead (adb -s ${ATALANTA_DEVICE_ID} emu geo \
             fix LON LAT on an emulator, xcrun simctl location ${ATALANTA_DEVICE_ID} set \
             LAT,LON on a simulator), with a `wait` between them"
        }
        "setPermissions" => {
            ": grant them with an `exec` step (adb -s ${ATALANTA_DEVICE_ID} shell pm grant \
             APP PERMISSION; xcrun simctl privacy ${ATALANTA_DEVICE_ID} grant SERVICE APP), \
             or launchApp with clearState and accept the dialogs"
        }
        "addMedia" | "clearKeychain" | "setAirplaneMode" | "toggleAirplaneMode"
        | "setOrientation" => {
            ": an `exec` step can do it with adb or xcrun simctl on the test's device \
             (${ATALANTA_DEVICE_ID}); docs/DSL.md#host-commands-exec"
        }
        "repeat" if arg.as_mapping().is_some_and(|m| m.contains_key("while")) => {
            ": a `while` loop depends on the screen at run time; write the steps out, or use \
             scrollUntilVisible when it scrolls until something shows"
        }
        _ => "",
    }
}

fn convert_step(command: &str, arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    if UNSUPPORTED_CONSTRUCTS.contains(&command) {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            command,
            &format!(
                "Unsupported Maestro construct '{command}' was skipped{}",
                unsupported_hint(command, arg)
            ),
        ));
        return None;
    }
    if NOT_NEEDED.contains(&command) {
        ctx.issues.push(issue(
            line,
            Severity::Info,
            command,
            &format!(
                "'{command}' isn't needed: Atalanta waits for the screen to settle on its own"
            ),
        ));
        return None;
    }
    let get = |k: &str| arg.as_mapping().and_then(|m| m.get(k)).cloned();
    match command {
        // D-12: a natural-language `assert:`, optional unless the flow
        // says `optional: false` (Maestro's default is optional). Maestro
        // judges a screenshot; Atalanta checks the element tree, so visual
        // claims ("the button is red") may not resolve.
        "assertWithAI" => {
            let assertion = match arg {
                Value::String(a) => Some(a.clone()),
                Value::Mapping(m) => m.get("assertion").and_then(Value::as_str).map(String::from),
                _ => None,
            };
            let Some(assertion) = assertion.filter(|a| !a.trim().is_empty()) else {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    command,
                    "assertWithAI without an assertion was skipped",
                ));
                return None;
            };
            warn_unused(arg, &["assertion", "label"], line, command, ctx);
            let optional = get("optional").and_then(|v| v.as_bool()).unwrap_or(true);
            ctx.issues.push(issue(
                line,
                Severity::Info,
                command,
                &format!(
                    "assertWithAI became a natural-language `assert:`{}; it is checked against \
                     the element tree, not a screenshot (needs ANTHROPIC_API_KEY)",
                    if optional {
                        " with `optional: true`"
                    } else {
                        ""
                    }
                ),
            ));
            let mut s = step("assert", Value::from(assertion));
            if optional {
                if let Value::Mapping(m) = &mut s {
                    m.insert(Value::from("optional"), Value::from(true));
                }
            }
            Some(s)
        }
        "launchApp" => {
            let mut pairs = Vec::new();
            match arg {
                Value::String(app) => pairs.push(("appId", Value::from(app.clone()))),
                Value::Mapping(_) => {
                    if let Some(a) = get("appId") {
                        pairs.push(("appId", a));
                    }
                    if let Some(c) = get("clearState") {
                        pairs.push(("clearState", c));
                    }
                    if let Some(a) = get("arguments") {
                        pairs.push(("arguments", launch_arguments(a, line, ctx)));
                    }
                    warn_unused(
                        arg,
                        &["appId", "clearState", "arguments"],
                        line,
                        command,
                        ctx,
                    );
                }
                _ => {}
            }
            Some(step("launchApp", map(pairs)))
        }
        "stopApp" | "killApp" => {
            if command == "killApp" {
                // Maestro's killApp is the system killing the process
                // (Android) or stopApp (iOS); Atalanta has only the stop.
                ctx.issues.push(issue(
                    line,
                    Severity::Info,
                    command,
                    "killApp became stopApp: the app is force-stopped, so Android's restore \
                     of its saved state after a system kill isn't exercised",
                ));
            }
            Some(step(
                "stopApp",
                match arg {
                    Value::String(app) => map(vec![("appId", Value::from(app.clone()))]),
                    // `stopApp: {appId: x}` stops x, not the suite's app.
                    Value::Mapping(_) => {
                        warn_unused(arg, &["appId"], line, command, ctx);
                        match get("appId") {
                            Some(a) => map(vec![("appId", a)]),
                            None => map(vec![]),
                        }
                    }
                    _ => map(vec![]),
                },
            ))
        }
        "clearState" => {
            ctx.issues.push(issue(
                line,
                Severity::Info,
                command,
                "clearState became launchApp with clearState: true (Atalanta clears on launch)",
            ));
            let mut pairs = vec![("clearState", Value::Bool(true))];
            match arg {
                Value::String(app) => pairs.insert(0, ("appId", Value::from(app.clone()))),
                // `clearState: {appId: x}` clears x, not the suite's app.
                Value::Mapping(_) => {
                    warn_unused(arg, &["appId", "label"], line, command, ctx);
                    if let Some(a) = get("appId") {
                        pairs.insert(0, ("appId", a));
                    }
                }
                _ => {}
            }
            Some(step("launchApp", map(pairs)))
        }
        "tapOn" | "doubleTapOn" | "longPressOn" => {
            if let Some(point) = get("point") {
                // A point beside a selector is relative to that element in
                // Maestro, not a screen position: never a tapXY.
                let selects = arg.as_mapping().is_some_and(|m| {
                    m.keys().filter_map(Value::as_str).any(|k| {
                        !matches!(
                            k,
                            "point"
                                | "repeat"
                                | "delay"
                                | "optional"
                                | "label"
                                | "retryTapIfNoChange"
                                | "waitToSettleTimeoutMs"
                        )
                    })
                });
                if selects {
                    ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        command,
                        "a point inside an element has no Atalanta equivalent; skipped",
                    ));
                    return None;
                }
                return convert_point_tap(command, &point, line, ctx);
            }
            let sel = selector(arg, line, command, ctx)?;
            let action = match command {
                "tapOn" => "tap",
                "doubleTapOn" => "doubleTap",
                _ => "longPress",
            };
            Some(step(action, sel))
        }
        "assertVisible" | "assertNotVisible" => {
            Some(step(command, selector(arg, line, command, ctx)?))
        }
        "extendedWaitUntil" => {
            let (action, target) = match (get("visible"), get("notVisible")) {
                (Some(v), _) => ("assertVisible", v),
                (None, Some(v)) => ("assertNotVisible", v),
                _ => {
                    ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        command,
                        "extendedWaitUntil without visible/notVisible was skipped",
                    ));
                    return None;
                }
            };
            warn_unused(
                arg,
                &["visible", "notVisible", "timeout"],
                line,
                command,
                ctx,
            );
            let sel = selector(&target, line, command, ctx)?;
            let mut s = step(action, sel);
            let timeout = get("timeout").and_then(|t| as_number(&t, line, "timeout", ctx));
            if let (Some(t), Value::Mapping(m)) = (timeout, &mut s) {
                m.insert(Value::from("timeoutMs"), Value::from(t));
            }
            Some(s)
        }
        "inputText"
        | "inputRandomText"
        | "inputRandomEmail"
        | "inputRandomNumber"
        | "inputRandomPersonName" => {
            let literal = match (command, arg) {
                ("inputText", Value::String(t)) => Some(t.clone()),
                ("inputText", Value::Number(n)) => Some(number_text(n, line, ctx)),
                ("inputText", Value::Mapping(m)) => match m.get("text") {
                    Some(Value::String(t)) => Some(t.clone()),
                    Some(Value::Number(n)) => Some(number_text(n, line, ctx)),
                    _ => None,
                },
                _ => None,
            };
            let text = match literal {
                Some(t) => {
                    flag_expressions(&t, line, command, ctx);
                    t
                }
                None => {
                    // A required variable, not a literal "TODO" that would be
                    // typed for real: the run stops until it is set.
                    let var = random_var(command);
                    ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        command,
                        &format!(
                            "{command} has no literal text; the step types ${{{var}}}, which must be set (atalanta run --env {var}=...)"
                        ),
                    ));
                    format!("${{{var}:?{command} from Maestro needs a value}}")
                }
            };
            // Maestro types into whatever has focus, like inputRaw.
            Some(step("inputRaw", map(vec![("text", Value::from(text))])))
        }
        "back" => Some(step("pressKey", map(vec![("key", Value::from("back"))]))),
        "pressKey" => {
            let name = arg
                .as_str()
                .map(String::from)
                .or_else(|| get("key").and_then(|k| k.as_str().map(String::from)))
                .unwrap_or_default();
            let key = match name.to_lowercase().replace(['_', ' '], "").as_str() {
                "enter" => "enter",
                "home" => "home",
                "back" => "back",
                "volumeup" => "volumeUp",
                "volumedown" => "volumeDown",
                "backspace" => "backspace",
                "tab" => "tab",
                "escape" => "escape",
                "delete" => "delete",
                "remotedpadup" | "dpadup" | "arrowup" => "arrowUp",
                "remotedpaddown" | "dpaddown" | "arrowdown" => "arrowDown",
                "remotedpadleft" | "dpadleft" | "arrowleft" => "arrowLeft",
                "remotedpadright" | "dpadright" | "arrowright" => "arrowRight",
                _ => {
                    ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        command,
                        &format!("pressKey '{name}' has no Atalanta key (enter, home, back, volumeUp, volumeDown, backspace, tab, escape, delete, arrowUp/Down/Left/Right); skipped"),
                    ));
                    return None;
                }
            };
            Some(step("pressKey", map(vec![("key", Value::from(key))])))
        }
        "scroll" => Some(step("swipe", map(vec![("direction", Value::from("up"))]))),
        "scrollUntilVisible" => {
            let Some(element) = get("element") else {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    command,
                    "scrollUntilVisible without element was skipped",
                ));
                return None;
            };
            warn_unused(
                arg,
                &["element", "direction", "timeout"],
                line,
                command,
                ctx,
            );
            let sel = selector(&element, line, command, ctx)?;
            let direction = get("direction")
                .and_then(|d| d.as_str().map(str::to_lowercase))
                .unwrap_or_else(|| "down".into());
            // Maestro scrolls until a time budget (`timeout`, 20 s by default)
            // runs out; Atalanta counts scrolls (default 5). Without this a
            // migrated scroll to a far row gave up after 5 scrolls / 6 s where
            // the Maestro flow had 20 s. One scroll per second of budget.
            let timeout_ms = get("timeout")
                .and_then(|t| t.as_u64().or_else(|| t.as_str()?.trim().parse().ok()))
                .unwrap_or(MAESTRO_SCROLL_TIMEOUT_MS);
            let max_scrolls = (timeout_ms / 1000).clamp(5, 100);
            Some(step(
                "scrollUntilVisible",
                map(vec![
                    ("selector", sel),
                    ("direction", Value::from(direction)),
                    ("maxScrolls", Value::from(max_scrolls)),
                ]),
            ))
        }
        "swipe" => {
            warn_unused(
                arg,
                &["direction", "from", "start", "end"],
                line,
                command,
                ctx,
            );
            convert_swipe(arg, line, ctx)
        }
        "takeScreenshot" | "screenshot" => {
            let name = arg
                .as_str()
                .map(String::from)
                .or_else(|| get("path").and_then(|p| p.as_str().map(String::from)));
            Some(step(
                "screenshot",
                match name {
                    Some(n) if n.ends_with(".png") => map(vec![("filename", Value::from(n))]),
                    Some(n) => map(vec![("filename", Value::from(format!("{n}.png")))]),
                    None => map(vec![]),
                },
            ))
        }
        "assertScreenshot" => convert_assert_screenshot(arg, line, ctx),
        "openLink" => {
            // `autoVerify` (Android app-link verification) and `browser`
            // (open in the browser, not the app) have no Atalanta field.
            warn_unused(arg, &["link", "label"], line, command, ctx);
            let link = arg
                .as_str()
                .map(String::from)
                .or_else(|| get("link").and_then(|l| l.as_str().map(String::from)));
            let Some(link) = link.filter(|l| !l.trim().is_empty()) else {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    command,
                    "openLink without a link; skipped",
                ));
                return None;
            };
            flag_expressions(&link, line, command, ctx);
            Some(step("openLink", Value::from(link)))
        }
        "runFlow" => convert_run_flow(arg, line, ctx),
        other => {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                other,
                &format!("Unknown Maestro construct '{other}' was skipped"),
            ));
            None
        }
    }
}

/// Maestro's `thresholdPercentage` when the flow gives none, as a fraction.
const MAESTRO_DEFAULT_THRESHOLD: f64 = 0.95;

/// Large enough that Atalanta's cluster gate never fails a comparison on
/// its own: Maestro judges only the share of differing pixels.
const NO_CLUSTER_GATE: u64 = 1_000_000;

/// `assertScreenshot: home.png` or `{path, thresholdPercentage, cropOn}` →
/// `assertScreenshot: {name, threshold, clusterPx, crop}` (VT-10). Migrated suites
/// lost their screenshot assertions: the command fell to "unknown" (B9).
fn convert_assert_screenshot(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let get = |k: &str| arg.as_mapping().and_then(|m| m.get(k)).cloned();
    let path = arg
        .as_str()
        .map(String::from)
        .or_else(|| get("path").and_then(|p| p.as_str().map(String::from)));
    let Some(path) = path.filter(|p| !p.trim().is_empty()) else {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "assertScreenshot",
            "assertScreenshot without a path; skipped",
        ));
        return None;
    };
    warn_unused(
        arg,
        &["path", "thresholdPercentage", "cropOn", "label"],
        line,
        "assertScreenshot",
        ctx,
    );
    let file = path.rsplit(['/', '\\']).next().unwrap_or(&path);
    // A name that is already a portable file name, so the baseline file
    // Atalanta reads is `<name>.png` and the hint below names it: a raw
    // `home.v2` was read as a hashed `home_v2-….png` (review M-7).
    let stem = file.strip_suffix(".png").unwrap_or(file);
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if name.is_empty() {
        name = "screenshot".into();
    }
    let mut pairs = vec![("name", Value::from(name.clone()))];
    match get("thresholdPercentage") {
        // Maestro's default is 95%; Atalanta's own (0.98) was stricter, so
        // flows that passed at 96% failed after migration (review M-6).
        None => pairs.push(("threshold", Value::from(MAESTRO_DEFAULT_THRESHOLD))),
        Some(v) => match as_number_f64(&v) {
            // Both are the similarity required: Maestro's in percent,
            // Atalanta's as a fraction.
            Some(pct) if (0.0..=100.0).contains(&pct) => {
                pairs.push(("threshold", Value::from(pct / 100.0)))
            }
            _ => {
                pairs.push(("threshold", Value::from(MAESTRO_DEFAULT_THRESHOLD)));
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    "assertScreenshot",
                    &format!(
                        "thresholdPercentage {} isn't a number from 0 to 100; Maestro's default of 95% is used",
                        serde_yaml::to_string(&v).unwrap_or_default().trim()
                    ),
                ))
            }
        },
    }
    pairs.push(("clusterPx", Value::from(NO_CLUSTER_GATE)));
    // Maestro's `cropOn: <selector>` is Atalanta's `crop: <selector>`
    // (VT-12): the comparison covers that element only.
    if let Some(on) = get("cropOn") {
        if let Some(sel) = selector(&on, line, "assertScreenshot", ctx) {
            pairs.push(("crop", sel));
        }
    }
    ctx.issues.push(issue(
        line,
        Severity::Info,
        "assertScreenshot",
        &format!(
            "copy the reference image to Atalanta's baseline: cp {} \
             atalanta-baselines/<platform>/<W>x<H>/{name}.png (<W>x<H>: the device's full \
             screenshot size in pixels, e.g. 1080x2400)",
            sh_quote(&path)
        ),
    ));
    Some(step("assertScreenshot", map(pairs)))
}

/// A YAML number (or a numeric string) as f64.
fn as_number_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A Maestro selector (a string, or a map) as a Atalanta selector map.
fn selector(value: &Value, line: usize, command: &str, ctx: &mut Ctx) -> Option<Value> {
    match value {
        Value::String(text) => {
            let (key, value) = text_value(text, line, command, ctx);
            Some(map(vec![(key, value)]))
        }
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, v) in m {
                let Some(key) = k.as_str() else { continue };
                match key {
                    "id" => match v.as_str() {
                        Some(id) => {
                            let (key, value) = id_value(id, line, command, ctx);
                            out.insert(Value::from(key), value);
                        }
                        None => {
                            out.insert(Value::from(key), v.clone());
                        }
                    },
                    "index" => match as_number(v, line, "index", ctx) {
                        Some(n) => {
                            out.insert(Value::from(key), Value::from(n));
                        }
                        // Dropping it would silently retarget the step to
                        // the first twin: refuse instead.
                        None => {
                            if let Some(last) = ctx.issues.last_mut() {
                                last.severity = Severity::Error;
                            }
                        }
                    },
                    "text" => {
                        // `text: 123` and `text: true` are text in Maestro.
                        let t = match v {
                            Value::String(t) => Some(t.clone()),
                            Value::Number(n) => Some(n.to_string()),
                            Value::Bool(b) => Some(b.to_string()),
                            _ => None,
                        };
                        let Some(t) = t else {
                            // Dropping it would widen the match to whatever
                            // else the selector says: refuse the step.
                            ctx.issues.push(issue(
                                line,
                                Severity::Warning,
                                command,
                                "selector `text` isn't a string; step skipped",
                            ));
                            return None;
                        };
                        let (key, value) = text_value(&t, line, command, ctx);
                        out.insert(Value::from(key), value);
                    }
                    "below" | "above" | "leftOf" | "rightOf" | "containsChild" | "childOf" => {
                        // An anchor that can't be converted would leave the
                        // step matching the first such element anywhere:
                        // skip the step, as the anchor's own warning says.
                        let anchor = selector(v, line, command, ctx)?;
                        out.insert(Value::from(key), anchor);
                    }
                    // Maestro's state filters are Atalanta's (S2).
                    "enabled" | "checked" | "focused" | "selected" => match v.as_bool() {
                        Some(b) => {
                            out.insert(Value::from(key), Value::from(b));
                        }
                        None => ctx.issues.push(issue(
                            line,
                            Severity::Warning,
                            command,
                            &format!("'{key}' must be true or false; dropped"),
                        )),
                    },
                    // Handled by the command (taps are unrolled).
                    "repeat" | "delay" => {}
                    // Carried over as the step's `optional:`.
                    "optional" => {}
                    // Options of the command, not the selector.
                    "retryTapIfNoChange" | "waitToSettleTimeoutMs" | "label" => {
                        ctx.issues.push(issue(
                            line,
                            Severity::Info,
                            command,
                            &format!("'{key}' isn't carried over"),
                        ));
                    }
                    other => {
                        ctx.issues.push(issue(
                            line,
                            Severity::Warning,
                            command,
                            &format!(
                                "selector field '{other}' has no Atalanta equivalent; dropped"
                            ),
                        ));
                    }
                }
            }
            if out.is_empty() {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    command,
                    "no usable selector; step skipped",
                ));
                return None;
            }
            // `index` or a state filter alone isn't a selector in Atalanta.
            let modifier = |k: &Value| {
                matches!(
                    k.as_str(),
                    Some("index" | "enabled" | "checked" | "focused" | "selected")
                )
            };
            if out.keys().all(modifier) {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    command,
                    "an index or state without a selector can't be converted; step skipped",
                ));
                return None;
            }
            Some(Value::Mapping(out))
        }
        _ => {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                "no usable selector; step skipped",
            ));
            None
        }
    }
}

/// Maestro matches text as a regular expression; Atalanta's `text:` is
/// exact. Plain text carries over as `text:`; a real pattern becomes
/// `textMatches:`, which has Maestro's semantics (D-11).
fn text_value(text: &str, line: usize, command: &str, ctx: &mut Ctx) -> (&'static str, Value) {
    flag_expressions(text, line, command, ctx);
    pattern_or_literal(text, "text", "textMatches", line, command, ctx)
}

/// Maestro's `id:` is a regular expression too: a real pattern becomes
/// `idMatches:` (D-11).
fn id_value(id: &str, line: usize, command: &str, ctx: &mut Ctx) -> (&'static str, Value) {
    pattern_or_literal(id, "id", "idMatches", line, command, ctx)
}

fn pattern_or_literal(
    value: &str,
    exact: &'static str,
    regex: &'static str,
    line: usize,
    command: &str,
    ctx: &mut Ctx,
) -> (&'static str, Value) {
    if let Some(literal) = regex_literal(value) {
        return (exact, Value::from(literal));
    }
    // Never write a pattern `atalanta validate` rejects (review MAJOR 6):
    // Maestro falls back to the plain text when its regex doesn't compile
    // (`+44 20 …`, `* Email`, `Done :)`), and so does this.
    let checked = atalanta_core::validator::without_env_refs(value);
    if let Err(why) = atalanta_common::selector_match::compile_pattern(&checked) {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            command,
            &format!(
                "'{value}' is not a valid regular expression ({why}); Maestro matches it as                  plain text, so it is written as {exact}:"
            ),
        ));
        return (exact, Value::from(unescape_meta(value)));
    }
    // Brackets that only group literal text (`Price (USD)`) mean the text.
    if !regex_intent(value) {
        return (exact, Value::from(unescape_meta(value)));
    }
    ctx.issues.push(issue(
        line,
        Severity::Info,
        command,
        &format!(
            "'{value}' is a regular expression in Maestro; written as {regex}: (whole \
             match, ignoring case)"
        ),
    ));
    (regex, Value::from(value.to_string()))
}

/// Whether a pattern that compiles uses regex syntax for what it matches,
/// not just as punctuation: a class escape (`\d`), a class (`[0-9]`), a
/// quantifier (`.*`, `+`, `?`, `{2}`), an alternation or an anchor. Bare
/// parentheses and a lone `.` don't count (`Price (USD)`, `a.b`).
fn regex_intent(text: &str) -> bool {
    const META: &str = "\\.*+?()[]{}|^$";
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' if chars.peek() == Some(&'{') => {
                for d in chars.by_ref() {
                    if d == '}' {
                        break;
                    }
                }
            }
            '\\' => match chars.next() {
                Some(e) if META.contains(e) => {}
                Some(_) => return true,
                None => {}
            },
            '(' | ')' | '.' => {}
            c if "*+?[]{}|^$".contains(c) => return true,
            _ => {}
        }
    }
    false
}

/// `text` with escaped metacharacters unescaped (`\(` → `(`): the plain
/// text a Maestro pattern stands for when it isn't used as one.
fn unescape_meta(text: &str) -> String {
    const META: &str = "\\.*+?()[]{}|^$";
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|e| META.contains(*e)) {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// The literal text a Maestro regex matches, when it is one: escaped
/// metacharacters are unescaped (`\[Beta\]` → `[Beta]`). `None` when any
/// unescaped metacharacter or class (`\d`) makes it a real pattern (VEL-241).
/// `${VAR}` references are kept as they are.
fn regex_literal(text: &str) -> Option<String> {
    const META: &str = "\\.*+?()[]{}|^$";
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' if chars.peek() == Some(&'{') => {
                out.push(c);
                for d in chars.by_ref() {
                    out.push(d);
                    if d == '}' {
                        break;
                    }
                }
            }
            '\\' => match chars.next() {
                Some(e) if META.contains(e) => out.push(e),
                _ => return None,
            },
            // A '.' also matches itself, so exact text still finds it.
            '.' => out.push(c),
            c if META.contains(c) => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Maestro's `${...}` is JavaScript; Atalanta reads `${NAME}` as an
/// environment variable. Anything but a plain name can't carry over (VEL-082).
fn flag_expressions(text: &str, line: usize, command: &str, ctx: &mut Ctx) {
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let expr = &rest[start + 2..start + len];
        let plain = !expr.is_empty()
            && !expr.starts_with(|c: char| c.is_ascii_digit())
            && expr.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !plain {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                &format!(
                    "'${{{expr}}}' is a JavaScript expression in Maestro; Atalanta reads ${{...}} as an environment variable, so set it with --env or rewrite the step"
                ),
            ));
        }
        rest = &rest[start + len..];
    }
}

/// A YAML number typed as text. A float is not what the flow wrote
/// (`1.50` is `1.5`, `1e3` is `1000.0`): the text is typed as the number
/// prints, with a warning to quote it in the flow.
fn number_text(n: &serde_yaml::Number, line: usize, ctx: &mut Ctx) -> String {
    let text = n.to_string();
    if n.is_f64() {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "inputText",
            &format!(
                "inputText: the number was read as {text}, which may not be the digits written (a trailing zero, an exponent); quote the text in the flow and check it"
            ),
        ));
    }
    text
}

/// The variable an `inputRandom*` command types instead.
fn random_var(command: &str) -> &'static str {
    match command {
        "inputRandomEmail" => "RANDOM_EMAIL",
        "inputRandomNumber" => "RANDOM_NUMBER",
        "inputRandomPersonName" => "RANDOM_PERSON_NAME",
        "inputRandomText" => "RANDOM_TEXT",
        _ => "INPUT_TEXT",
    }
}

/// `tapOn: { point: "100,200" }` → `tapXY`; percentages can't be converted.
fn convert_point_tap(command: &str, point: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let parsed = point.as_str().and_then(|p| {
        let (x, y) = p.split_once(',')?;
        Some((x.trim().parse::<i64>().ok()?, y.trim().parse::<i64>().ok()?))
    });
    match (command, parsed) {
        ("tapOn", Some((x, y))) => Some(step("tapXY", map(vec![("x", x.into()), ("y", y.into())]))),
        _ => {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                command,
                "a percentage point (or a double-tap/long-press at a point) can't be converted: \
                 Atalanta's tapXY takes pixels; skipped",
            ));
            None
        }
    }
}

fn convert_swipe(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let direction = |d: &str| map(vec![("direction", Value::from(d.to_lowercase()))]);
    match arg {
        Value::String(d) => Some(step("swipe", direction(d))),
        Value::Mapping(m) => {
            if let Some(d) = m.get("direction").and_then(Value::as_str) {
                let mut swipe = direction(d);
                // `from: <selector>` swipes across that element (Maestro: from
                // its middle, in the direction). Dropping it swiped the middle
                // of the screen, so a carousel above the fold never moved.
                if let Some(from) = m.get("from") {
                    // Without it the swipe would move the middle of the
                    // screen: skip the swipe instead.
                    let sel = selector(from, line, "swipe", ctx)?;
                    if let Value::Mapping(sw) = &mut swipe {
                        sw.insert(Value::from("within"), sel);
                    }
                }
                return Some(step("swipe", swipe));
            }
            let point = |k: &str| -> Option<(i64, i64)> {
                let (x, y) = m.get(k)?.as_str()?.split_once(',')?;
                Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
            };
            let percent = |k: &str| -> Option<(f64, f64)> {
                let (x, y) = m.get(k)?.as_str()?.split_once(',')?;
                let p = |s: &str| s.trim().strip_suffix('%')?.trim().parse::<f64>().ok();
                Some((p(x)?, p(y)?))
            };
            match (point("start"), point("end")) {
                (Some((x1, y1)), Some((x2, y2))) => Some(step(
                    "swipe",
                    map(vec![
                        ("from", map(vec![("x", x1.into()), ("y", y1.into())])),
                        ("to", map(vec![("x", x2.into()), ("y", y2.into())])),
                    ]),
                )),
                _ => match (percent("start"), percent("end")) {
                    // The direction of the finger's dominant movement (VEL-346).
                    (Some((x1, y1)), Some((x2, y2))) if (x2 - x1).abs() != (y2 - y1).abs() => {
                        let (dx, dy) = (x2 - x1, y2 - y1);
                        let d = if dx.abs() > dy.abs() {
                            if dx < 0.0 {
                                "left"
                            } else {
                                "right"
                            }
                        } else if dy < 0.0 {
                            "up"
                        } else {
                            "down"
                        };
                        ctx.issues.push(issue(
                            line,
                            Severity::Info,
                            "swipe",
                            &format!("percentage swipe became a direction swipe ({d})"),
                        ));
                        Some(step("swipe", direction(d)))
                    }
                    _ => {
                        ctx.issues.push(issue(
                            line,
                            Severity::Warning,
                            "swipe",
                            "swipe without a direction or usable start/end points; skipped",
                        ));
                        None
                    }
                },
            }
        }
        _ => Some(step("swipe", direction("up"))),
    }
}

/// `setLocation: { latitude, longitude }` → two `exec` steps, one per
/// platform, each `optional: true` because only one of them applies to the
/// device the test runs on: `adb emu geo fix` (an Android emulator) and
/// `xcrun simctl location … set` (an iOS simulator). `adb` and `xcrun` go
/// into `config.exec.allow`. Real devices can't be moved this way.
fn convert_set_location(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Vec<Value>> {
    let coord = |k: &str| -> Option<String> {
        let v = arg.as_mapping()?.get(k)?;
        let n = match v {
            Value::Number(n) => n.as_f64(),
            Value::String(t) => t.trim().parse::<f64>().ok(),
            _ => None,
        }?;
        n.is_finite().then(|| n.to_string())
    };
    let (Some(lat), Some(lon)) = (coord("latitude"), coord("longitude")) else {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "setLocation",
            "setLocation needs numeric latitude and longitude; skipped",
        ));
        return None;
    };
    warn_unused(arg, &["latitude", "longitude"], line, "setLocation", ctx);
    let exec = |run: Vec<&str>| {
        let mut m = Mapping::new();
        m.insert(
            Value::from("exec"),
            map(vec![(
                "run",
                Value::Sequence(run.into_iter().map(Value::from).collect()),
            )]),
        );
        m.insert(Value::from("optional"), Value::from(true));
        Value::Mapping(m)
    };
    let dev = "${ATALANTA_DEVICE_ID}";
    let ll = format!("{lat},{lon}");
    ctx.exec_allow.insert("adb".into());
    ctx.exec_allow.insert("xcrun".into());
    ctx.issues.push(issue(
        line,
        Severity::Info,
        "setLocation",
        "setLocation became two optional `exec` steps (adb emu geo fix for an Android \
         emulator, xcrun simctl location for an iOS simulator; the one for the other platform \
         fails as a warning), with adb and xcrun in config.exec.allow. Real devices can't be \
         moved this way; keep the one your suite needs",
    ));
    Some(vec![
        exec(vec!["adb", "-s", dev, "emu", "geo", "fix", &lon, &lat]),
        exec(vec!["xcrun", "simctl", "location", dev, "set", &ll]),
    ])
}

/// `text` as one word for a POSIX shell.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// An `exec` step running `run`, `optional: true` when it applies to one
/// platform only (the other platform's step then fails as a warning).
fn exec_step(run: &[&str], optional: bool) -> Value {
    let mut m = Mapping::new();
    m.insert(
        Value::from("exec"),
        map(vec![(
            "run",
            Value::Sequence(run.iter().map(|a| Value::from(*a)).collect()),
        )]),
    );
    if optional {
        m.insert(Value::from("optional"), Value::from(true));
    }
    Value::Mapping(m)
}

/// Maestro's permission name → (Android permission, `simctl privacy`
/// service). `None` where the platform has no single equivalent.
fn permission_names(name: &str) -> Option<(Option<&'static str>, Option<&'static str>)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "camera" => (Some("android.permission.CAMERA"), Some("camera")),
        "contacts" => (Some("android.permission.READ_CONTACTS"), Some("contacts")),
        "location" => (
            Some("android.permission.ACCESS_FINE_LOCATION"),
            Some("location"),
        ),
        "microphone" => (Some("android.permission.RECORD_AUDIO"), Some("microphone")),
        "calendar" => (Some("android.permission.READ_CALENDAR"), Some("calendar")),
        "phone" => (Some("android.permission.CALL_PHONE"), None),
        "sms" => (Some("android.permission.READ_SMS"), None),
        "storage" => (Some("android.permission.READ_EXTERNAL_STORAGE"), None),
        "notifications" => (Some("android.permission.POST_NOTIFICATIONS"), None),
        "photos" | "medialibrary" => (Some("android.permission.READ_MEDIA_IMAGES"), Some("photos")),
        "reminders" => (None, Some("reminders")),
        "motion" => (None, Some("motion")),
        _ => return None,
    })
}

/// `setPermissions`, `clearKeychain`, `addMedia`, `setAirplaneMode` and
/// `setOrientation` as `exec` steps on `${ATALANTA_DEVICE_ID}` (review 15
/// §2: each is one adb or simctl call). `None` when the command isn't one
/// of these or its argument can't be converted: the caller reports it.
fn convert_device_command(
    command: &str,
    arg: &Value,
    line: usize,
    ctx: &mut Ctx,
) -> Option<Vec<Value>> {
    let dev = "${ATALANTA_DEVICE_ID}";
    let (steps, tools, note): (Vec<Value>, &[&str], String) = match command {
        "setPermissions" => {
            let m = arg.as_mapping()?;
            let app = m
                .get("appId")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| ctx.app_id.clone());
            let perms = m.get("permissions")?.as_mapping()?;
            // `adb shell` hands its arguments to the device's shell, so an
            // id like `a; reboot` would run as a command. Package and
            // bundle ids never need more than these characters.
            if app.is_empty()
                || !app
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                ctx.issues.push(issue(
                        line,
                        Severity::Warning,
                        command,
                        &format!(
                            "setPermissions of app id '{app}' skipped: it has characters an app id never has, and the converted `adb shell` step would pass them to the device's shell"
                        ),
                    ));
                *ctx.skipped += 1;
                return Some(vec![]);
            }
            let mut steps = Vec::new();
            let mut unmapped = Vec::new();
            for (k, v) in perms {
                let (Some(name), Some(state)) = (k.as_str(), v.as_str()) else {
                    unmapped.push(yaml_inline(k));
                    continue;
                };
                let Some((android, ios)) = permission_names(name) else {
                    unmapped.push(name.to_string());
                    continue;
                };
                let (pm, privacy) = match state.to_ascii_lowercase().as_str() {
                    "allow" => ("grant", "grant"),
                    "deny" => ("revoke", "revoke"),
                    "unset" => ("revoke", "reset"),
                    _ => {
                        unmapped.push(format!("{name}: {state}"));
                        continue;
                    }
                };
                if let Some(a) = android {
                    steps.push(exec_step(
                        &["adb", "-s", dev, "shell", "pm", pm, &app, a],
                        true,
                    ));
                }
                if let Some(i) = ios {
                    steps.push(exec_step(
                        &["xcrun", "simctl", "privacy", dev, privacy, i, &app],
                        true,
                    ));
                }
            }
            if steps.is_empty() {
                return None;
            }
            let mut note = "setPermissions became optional `exec` steps (pm grant/revoke on \
                            Android, simctl privacy on an iOS simulator; the other platform's \
                            step fails as a warning)"
                .to_string();
            if !unmapped.is_empty() {
                note.push_str(&format!("; not converted: {}", unmapped.join(", ")));
            }
            (steps, &["adb", "xcrun"], note)
        }
        "clearKeychain" => (
            vec![exec_step(
                &["xcrun", "simctl", "keychain", dev, "reset"],
                false,
            )],
            &["xcrun"],
            "clearKeychain became `xcrun simctl keychain reset` (iOS simulators only)".into(),
        ),
        "addMedia" => {
            let files: Vec<String> = match arg {
                Value::String(f) => vec![f.clone()],
                Value::Sequence(v) => v
                    .iter()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect(),
                Value::Mapping(m) => m
                    .get("files")
                    .and_then(Value::as_sequence)?
                    .iter()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect(),
                _ => return None,
            };
            if files.is_empty() {
                return None;
            }
            let mut steps = Vec::new();
            for f in &files {
                let name = Path::new(f).file_name()?.to_string_lossy().into_owned();
                let remote = format!("/sdcard/Pictures/{name}");
                steps.push(exec_step(&["adb", "-s", dev, "push", f, &remote], true));
                steps.push(exec_step(
                    &[
                        "adb",
                        "-s",
                        dev,
                        "shell",
                        "am",
                        "broadcast",
                        "-a",
                        "android.intent.action.MEDIA_SCANNER_SCAN_FILE",
                        "-d",
                        // The device's shell reads this argument.
                        &sh_quote(&format!("file://{remote}")),
                    ],
                    true,
                ));
            }
            let mut ios = vec!["xcrun", "simctl", "addmedia", dev];
            ios.extend(files.iter().map(String::as_str));
            steps.push(exec_step(&ios, true));
            (
                steps,
                &["adb", "xcrun"],
                "addMedia became optional `exec` steps (adb push + a media scan on Android, \
                 simctl addmedia on an iOS simulator). The paths are as the flow wrote them: \
                 relative to where `atalanta run` starts"
                    .into(),
            )
        }
        "setAirplaneMode" => {
            let on = match arg {
                Value::String(s) => match s.to_ascii_lowercase().as_str() {
                    "enabled" | "true" | "on" => true,
                    "disabled" | "false" | "off" => false,
                    _ => return None,
                },
                Value::Bool(b) => *b,
                _ => return None,
            };
            let mode = if on { "enable" } else { "disable" };
            (
                vec![exec_step(
                    &[
                        "adb",
                        "-s",
                        dev,
                        "shell",
                        "cmd",
                        "connectivity",
                        "airplane-mode",
                        mode,
                    ],
                    false,
                )],
                &["adb"],
                "setAirplaneMode became `adb shell cmd connectivity airplane-mode` (Android 9+; \
                 iOS simulators have no airplane mode)"
                    .into(),
            )
        }
        "setOrientation" => {
            let o = arg
                .as_str()
                .or_else(|| arg.get("orientation").and_then(Value::as_str))?;
            let rotation = match o.to_ascii_uppercase().as_str() {
                "PORTRAIT" => "0",
                "LANDSCAPE_LEFT" | "LANDSCAPE" => "1",
                "UPSIDE_DOWN" => "2",
                "LANDSCAPE_RIGHT" => "3",
                _ => return None,
            };
            (
                vec![
                    exec_step(
                        &[
                            "adb",
                            "-s",
                            dev,
                            "shell",
                            "settings",
                            "put",
                            "system",
                            "accelerometer_rotation",
                            "0",
                        ],
                        false,
                    ),
                    exec_step(
                        &[
                            "adb",
                            "-s",
                            dev,
                            "shell",
                            "settings",
                            "put",
                            "system",
                            "user_rotation",
                            rotation,
                        ],
                        false,
                    ),
                ],
                &["adb"],
                "setOrientation became `adb shell settings put system user_rotation` (Android \
                 only: an iOS simulator can't be rotated from the command line)"
                    .into(),
            )
        }
        _ => return None,
    };
    for t in tools {
        ctx.exec_allow.insert((*t).into());
    }
    ctx.issues.push(issue(
        line,
        // What wasn't converted is a loss, not a note.
        if note.contains("; not converted: ") {
            Severity::Warning
        } else {
            Severity::Info
        },
        command,
        &format!("{note}; {} in config.exec.allow", tools.join(" and ")),
    ));
    Some(steps)
}

/// Maestro header `env:` as defaults: every `${NAME}` the flow uses
/// becomes `${NAME:-value}`, so the flow runs as it did in Maestro and
/// `--env NAME=…` still overrides it. A value that can't sit inside
/// `${…:-…}` (it has `}` or `${`) is left for `--env`, and named.
fn env_defaults(env: &Mapping) -> (Vec<(String, String)>, Vec<String>) {
    let mut ok = Vec::new();
    let mut left = Vec::new();
    for (k, v) in env {
        let Some(name) = k.as_str() else { continue };
        let value = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => {
                left.push(name.to_string());
                continue;
            }
        };
        if value.contains('}') || value.contains("${") {
            left.push(name.to_string());
        } else {
            ok.push((name.to_string(), value));
        }
    }
    (ok, left)
}

/// Rewrite `${NAME}` to `${NAME:-value}` in every string under `v`.
fn apply_env_defaults(v: &mut Value, defaults: &[(String, String)]) {
    match v {
        Value::String(s) => {
            for (name, value) in defaults {
                let plain = format!("${{{name}}}");
                if s.contains(&plain) {
                    *s = s.replace(&plain, &format!("${{{name}:-{value}}}"));
                }
            }
        }
        Value::Sequence(items) => items
            .iter_mut()
            .for_each(|i| apply_env_defaults(i, defaults)),
        Value::Mapping(m) => m
            .iter_mut()
            .for_each(|(_, i)| apply_env_defaults(i, defaults)),
        _ => {}
    }
}

/// A `runFlow`'s `when:` as a gate step at the start of the flow: an
/// `assertVisible` / `assertNotVisible` for `visible:` / `notVisible:`.
/// `None` for a condition Atalanta can't check (`platform:`, `true:` with
/// a JavaScript expression).
fn when_gate(when: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let m = when.as_mapping()?;
    if m.len() != 1 {
        return None;
    }
    let (k, v) = m.iter().next()?;
    let action = match k.as_str()? {
        "visible" => "assertVisible",
        "notVisible" => "assertNotVisible",
        _ => return None,
    };
    Some(step(action, selector(v, line, "runFlow", ctx)?))
}

/// `runFlow: login.yaml` → `runFlow: login`, with login.yaml converted into
/// the suite's `flows:` so the suite runs on its own.
///
/// `when:` becomes an optional block (`optional: true` on the `runFlow`:
/// the first step that fails skips the rest of the flow and the test goes
/// on). For `visible:` / `notVisible:` the flow is wrapped in one that
/// first asserts the condition, so when it doesn't hold nothing in the flow
/// runs, as in Maestro; it waits up to the element timeout to find out,
/// where Maestro checks once. Other conditions (`platform:`, `true:`) can't
/// be checked: the flow is attempted and any failure in it is a warning.
fn convert_run_flow(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let when = arg.as_mapping().and_then(|m| m.get("when")).cloned();
    // Maestro's `env:` becomes the call's parameters, on the outermost
    // runFlow (a `when:` wrapper passes them on to the flow it runs).
    let env = arg.as_mapping().and_then(|m| m.get("env")).cloned();
    let call = convert_run_flow_file(arg, line, ctx)?;
    let Some(when) = when else {
        return Some(with_env(call, env));
    };
    let flow_id = call
        .as_mapping()
        .and_then(|m| m.get("runFlow"))
        .and_then(Value::as_str)
        .map(String::from)?;
    let gated = match when_gate(&when, line, ctx) {
        Some(gate) => {
            let id = unique_flow_id(&format!("{flow_id}_when"), ctx.flows);
            ctx.flows.insert(
                id.clone(),
                vec![gate, step("runFlow", Value::from(flow_id))],
            );
            ctx.issues.push(issue(
                line,
                Severity::Info,
                "runFlow",
                &format!(
                    "runFlow `when: {}` became flow '{id}': it checks the condition first and \
                     runs as an optional block, so the flow is skipped when the condition \
                     doesn't hold (waiting up to the element timeout to find out)",
                    yaml_inline(&when)
                ),
            ));
            id
        }
        None => {
            ctx.issues.push(issue(
                line,
                Severity::Warning,
                "runFlow",
                &format!(
                    "runFlow `when: {}` can't be checked in Atalanta (only visible/notVisible \
                     convert); the flow runs as an optional block: it is attempted every \
                     time and a failure in it is a warning, not a test failure",
                    yaml_inline(&when)
                ),
            ));
            flow_id
        }
    };
    let mut m = Mapping::new();
    m.insert(Value::from("runFlow"), Value::from(gated));
    m.insert(Value::from("optional"), Value::from(true));
    Some(with_env(Value::Mapping(m), env))
}

/// `runFlow: id` with Maestro's `env:` as `runFlow: { flow: id, env }`.
fn with_env(mut step: Value, env: Option<Value>) -> Value {
    let Some(env) = env else { return step };
    if let Some(m) = step.as_mapping_mut() {
        if let Some(Value::String(id)) = m.get("runFlow").cloned() {
            let mut call = Mapping::new();
            call.insert(Value::from("flow"), Value::from(id));
            call.insert(Value::from("env"), env);
            m.insert(Value::from("runFlow"), Value::Mapping(call));
        }
    }
    step
}

fn convert_run_flow_file(arg: &Value, line: usize, ctx: &mut Ctx) -> Option<Value> {
    let file = match arg {
        Value::String(s) => Some(s.clone()),
        Value::Mapping(m) => {
            warn_unused(
                arg,
                &["file", "when", "commands", "env", "label"],
                line,
                "runFlow",
                ctx,
            );
            if m.contains_key("commands") {
                ctx.issues.push(issue(
                    line,
                    Severity::Warning,
                    "runFlow",
                    "inline runFlow commands aren't supported; skipped",
                ));
                return None;
            }
            m.get("file").and_then(|v| v.as_str()).map(String::from)
        }
        _ => None,
    };
    let Some(file) = file else {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "runFlow",
            "runFlow without a file; skipped",
        ));
        return None;
    };
    if file.contains("${") {
        // A parameterised path can't be read at migration time. Keep the
        // call as a placeholder flow that stops the run until it is
        // converted by hand, rather than refusing the whole file.
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "runFlow",
            &format!(
                "runFlow of a parameterised path '{file}' can't be converted; it runs a placeholder flow that stops the run until you replace it"
            ),
        ));
        let id = unique_flow_id("parameterised_flow", ctx.flows);
        let stop = step(
            "assertVisible",
            map(vec![(
                "text",
                Value::from(
                    "${MAESTRO_RUNFLOW:?a runFlow with a parameterised path from Maestro needs converting by hand}",
                ),
            )]),
        );
        ctx.flows.insert(id.clone(), vec![stop]);
        return Some(step("runFlow", Value::from(id)));
    }
    let path = ctx.dir.join(&file);
    let key = canonical(&path);
    if ctx.visiting.contains(&key) {
        ctx.issues.push(issue(
            line,
            Severity::Error,
            "runFlow",
            &format!("runFlow cycle through '{file}'"),
        ));
        return None;
    }
    // A file used twice is converted once; two files with the same name are
    // two flows (VEL-017).
    if let Some(id) = ctx.flow_ids.get(&key) {
        return Some(step("runFlow", Value::from(id.clone())));
    }
    let (header, steps, lines) = match read_flow(&path) {
        Ok(f) => f,
        Err(e) => {
            // A `runFlow` of a flow that isn't in the suite fails validation,
            // so the file can't be migrated as it is.
            ctx.issues.push(issue(
                line,
                Severity::Error,
                "runFlow",
                &format!(
                    "'{file}' can't be read next to the flow ({e}); add it, or remove the runFlow"
                ),
            ));
            return None;
        }
    };
    if header.contains_key("env") {
        // Only the top flow's `env:` becomes defaults.
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "env",
            &format!(
                "the `env:` of '{file}' isn't carried over (only the top flow's becomes defaults); pass its values with `runFlow: {{ env }}` or `--env`"
            ),
        ));
    }
    let id = unique_flow_id(&file, ctx.flows);
    ctx.visiting.insert(key.clone());
    // Reserve the id first, so a nested flow can't take it.
    ctx.flow_ids.insert(key.clone(), id.clone());
    ctx.flows.insert(id.clone(), Vec::new());
    let mut sub = Ctx {
        dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        issues: &mut *ctx.issues,
        flows: &mut *ctx.flows,
        flow_ids: &mut *ctx.flow_ids,
        visiting: &mut *ctx.visiting,
        skipped: &mut *ctx.skipped,
        exec_allow: &mut *ctx.exec_allow,
        unrolled: &mut *ctx.unrolled,
        app_id: ctx.app_id.clone(),
    };
    let converted = convert_flow(&header, &steps, &lines, &mut sub);
    if converted.is_empty() {
        ctx.issues.push(issue(
            line,
            Severity::Warning,
            "runFlow",
            &format!("'{file}' has no step that converts: the runFlow does nothing"),
        ));
    }
    ctx.flows.insert(id.clone(), converted);
    ctx.visiting.remove(&key);
    Some(step("runFlow", Value::from(id)))
}

/// The file's stem, or — when another flow already has it — the stem with
/// its folder (`b_login`), then a number.
fn unique_flow_id(file: &str, flows: &BTreeMap<String, Vec<Value>>) -> String {
    let p = Path::new(file);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("flow");
    if !flows.contains_key(stem) {
        return stem.to_string();
    }
    if let Some(dir) = p
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
    {
        let id = format!("{dir}_{stem}");
        if !flows.contains_key(&id) {
            return id;
        }
    }
    (2..)
        .map(|n| format!("{stem}_{n}"))
        .find(|id| !flows.contains_key(id))
        .unwrap_or_default()
}

/// Parse and validate migrated YAML the way `atalanta run` loads a suite.
fn check_output(yaml: &str) -> Result<()> {
    let suite = atalanta_core::parser::parse_suite_from_str(yaml)?;
    atalanta_core::validate_suite(&suite)?;
    // `atalanta run` inlines the flows too, and refuses what is too deep or
    // expands to too many steps: a file that only fails there isn't a
    // successful migration.
    atalanta_core::resolve_flows(&suite).map(|_| ())
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn build_atalanta_yaml(c: &Converted) -> String {
    let mut doc = Mapping::new();
    doc.insert("appId".into(), Value::from(c.app_id.clone()));
    if !c.exec_allow.is_empty() {
        let allow: Vec<Value> = c.exec_allow.iter().cloned().map(Value::from).collect();
        doc.insert(
            "config".into(),
            map(vec![("exec", map(vec![("allow", Value::Sequence(allow))]))]),
        );
    }
    if !c.flows.is_empty() {
        let flows: Vec<Value> = c
            .flows
            .iter()
            .map(|(id, steps)| {
                map(vec![
                    ("id", Value::from(id.clone())),
                    ("steps", Value::Sequence(steps.clone())),
                ])
            })
            .collect();
        doc.insert("flows".into(), Value::Sequence(flows));
    }
    let test = map(vec![
        ("name", Value::from(c.name.clone())),
        (
            "tags",
            Value::Sequence(c.tags.iter().cloned().map(Value::from).collect()),
        ),
        ("steps", Value::Sequence(c.steps.clone())),
    ]);
    doc.insert("tests".into(), Value::Sequence(vec![test]));
    let body = serde_yaml::to_string(&Value::Mapping(doc)).unwrap_or_default();
    format!("# Migrated from Maestro by `atalanta migrate maestro`. Review the warnings it printed.\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vel-mig-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Convert `files` (name → Maestro YAML) and parse + validate the main
    /// one's output with Atalanta's own parser.
    fn migrate(
        name: &str,
        files: &[(&str, &str)],
    ) -> (FileMigrationResult, atalanta_common::TestSuite) {
        let d = dir(name);
        for (f, body) in files {
            fs::write(d.join(f), body).unwrap();
        }
        let out = d.join("out.atalanta.yaml");
        let result = MaestroMigrator::new()
            .migrate_file(
                &d.join(files[0].0).to_string_lossy(),
                &out.to_string_lossy(),
            )
            .unwrap();
        let yaml = fs::read_to_string(&out).unwrap_or_default();
        let suite = atalanta_core::parser::parse_suite_from_str(&yaml)
            .unwrap_or_else(|e| panic!("migrated output doesn't parse: {e}\n{yaml}"));
        atalanta_core::validate_suite(&suite)
            .unwrap_or_else(|e| panic!("migrated output doesn't validate: {e}\n{yaml}"));
        let _ = fs::remove_dir_all(&d);
        (result, suite)
    }

    #[test]
    fn the_repo_fixtures_migrate_to_suites_that_parse() {
        let root = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/maestro_compat"
        );
        for f in ["login.yaml", "checkout.yaml"] {
            let body = fs::read_to_string(format!("{root}/{f}")).unwrap();
            let (result, suite) = migrate(f, &[(f, &body)]);
            assert!(result.success, "{f}: {:?}", result.issues);
            assert!(!suite.tests[0].steps.is_empty(), "{f}");
        }
    }

    /// D-9: `back` right after typing is flagged (it closed the keyboard
    /// in Maestro; Atalanta's back navigates, on the web through history),
    /// and Maestro's Backspace/Tab/D-pad keys now have Atalanta keys.
    #[test]
    fn back_after_input_text_is_flagged_and_editing_keys_map() {
        let (result, suite) = migrate(
            "back-after-input",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp\n- tapOn: \"Email\"\n- inputText: \"a@b.c\"\n- back\n- tapOn: \"Go\"\n- back\n- pressKey: Backspace\n- pressKey: Tab\n- pressKey: Remote Dpad Up\n",
            )],
        );
        let flagged: Vec<usize> = result
            .issues
            .iter()
            .filter(|i| i.message.contains("straight after `inputText`"))
            .map(|i| i.line)
            .collect();
        assert_eq!(flagged.len(), 1, "{:?}", result.issues);
        let keys: Vec<atalanta_common::Key> = suite.tests[0]
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                atalanta_common::Action::PressKey { key } => Some(key.clone()),
                _ => None,
            })
            .collect();
        use atalanta_common::Key;
        assert_eq!(
            keys,
            [Key::Back, Key::Back, Key::Backspace, Key::Tab, Key::ArrowUp]
        );
    }

    // VEL-240: a `retry` block was skipped, emptying the flow, and the
    // migration still said success.
    #[test]
    fn launch_app_arguments_carry_over() {
        let (result, suite) = migrate(
            "args",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp:\n    arguments:\n      isE2E: true\n      server: \"https://staging\"\n- tapOn: \"Go\"\n",
            )],
        );
        assert!(result.success);
        let atalanta_common::Action::LaunchApp { arguments, .. } = &suite.tests[0].steps[0].action
        else {
            panic!(
                "expected LaunchApp, got {:?}",
                suite.tests[0].steps[0].action
            );
        };
        assert_eq!(arguments.inner.len(), 2, "{arguments:?}");
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.message.contains("arguments")),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn a_launch_argument_atalanta_rejects_is_dropped_with_a_warning() {
        let (result, suite) = migrate(
            "badarg",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp:\n    arguments:\n      isE2E: true\n      \"-FIRDebugEnabled\": true\n- tapOn: \"Go\"\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let atalanta_common::Action::LaunchApp { arguments, .. } = &suite.tests[0].steps[0].action
        else {
            panic!("expected LaunchApp");
        };
        assert_eq!(arguments.inner.len(), 1, "{arguments:?}");
        assert!(arguments.inner.contains_key("isE2E"), "{arguments:?}");
        assert!(warned(&result, "-FIRDebugEnabled"), "{:?}", result.issues);
    }

    #[test]
    fn a_runflow_env_becomes_the_calls_parameters() {
        let (result, suite) = migrate(
            "flowenv",
            &[
                (
                    "flow.yaml",
                    "appId: com.shop\n---\n- runFlow:\n    file: login.yaml\n    env:\n      USER: qa@example.com\n",
                ),
                (
                    "login.yaml",
                    "appId: com.shop\n---\n- inputText: ${USER}\n",
                ),
            ],
        );
        assert!(result.success, "{:?}", result.issues);
        assert!(
            !result.issues.iter().any(|i| i.message.contains("env")),
            "{:?}",
            result.issues
        );
        let tests = atalanta_core::resolve_flows(&suite).unwrap();
        let typed: Vec<String> = tests[0]
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                // A bare Maestro `inputText` types into what has focus.
                atalanta_common::Action::InputRaw { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(typed, ["qa@example.com"]);
    }

    #[test]
    fn a_long_press_then_swipe_points_at_drag() {
        let (result, _) = migrate(
            "dragish",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- longPressOn: \"Milk\"\n- swipe:\n    start: 50%,30%\n    end: 50%,60%\n- swipe:\n    direction: UP\n",
            )],
        );
        assert!(result.success);
        let notes: Vec<_> = result
            .issues
            .iter()
            .filter(|i| i.message.contains("drag-to-reorder"))
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "only the swipe right after the long press: {:?}",
            result.issues
        );
    }

    #[test]
    fn a_retry_block_is_inlined_once_with_a_warning() {
        let (result, suite) = migrate(
            "retry",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp\n- retry:\n    maxRetries: 3\n    commands:\n      - tapOn: \"Pay\"\n      - assertVisible: \"Paid\"\n",
            )],
        );
        assert!(result.success);
        let names: Vec<String> = suite.tests[0]
            .steps
            .iter()
            .map(|s| format!("{:?}", s.action))
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names[1].starts_with("Tap") && names[2].starts_with("AssertVisible"));
        assert!(result
            .issues
            .iter()
            .any(|i| i.message.contains("retry block inlined")));
    }

    // VT-10 / B9: assertScreenshot fell to "unknown construct" and was lost.
    #[test]
    fn assert_screenshot_converts_in_all_three_forms() {
        let (result, suite) = migrate(
            "shots",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp\n- assertScreenshot: shots/home.png\n- assertScreenshot:\n    path: cart\n    thresholdPercentage: 97.5\n- assertScreenshot:\n    path: menu.png\n    cropOn:\n      id: menu\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let shots: Vec<_> = suite.tests[0]
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                atalanta_common::Action::AssertScreenshot {
                    name,
                    threshold,
                    cluster_px,
                    ..
                } => Some((name.clone(), *threshold, *cluster_px)),
                _ => None,
            })
            .collect();
        assert_eq!(shots.len(), 3, "{shots:?}");
        assert_eq!(shots[0].0, "home");
        // Review M-6: no thresholdPercentage is Maestro's 95%.
        assert_eq!(shots[0].1, Some(0.95));
        assert_eq!(shots[1], ("cart".into(), Some(0.975), Some(1_000_000)));
        assert_eq!(shots[2].0, "menu");
        // review-d5 m-5: cropOn is crop:, no longer "not supported".
        let crop = suite.tests[0].steps.iter().find_map(|s| match &s.action {
            atalanta_common::Action::AssertScreenshot { crop, .. } => crop.clone(),
            _ => None,
        });
        assert_eq!(
            crop,
            Some(atalanta_common::ScreenshotCrop::Element(
                atalanta_common::Selector::Id("menu".into())
            ))
        );
        assert!(!result.issues.iter().any(|i| i.message.contains("cropOn")));
        // review-d5 m-5: the hint names the sized layout (D-V3).
        assert!(result.issues.iter().any(|i| i
            .message
            .contains("atalanta-baselines/<platform>/<W>x<H>/home.png")));
    }

    // Review M-7: the hint named `home.v2.png`, but Atalanta reads a
    // cleaned, hashed file for that name; the name is now clean.
    #[test]
    fn a_screenshot_name_is_the_baseline_file_the_hint_names() {
        let (result, suite) = migrate(
            "shots2",
            &[(
                "flow.yaml",
                "appId: com.shop\n---\n- launchApp\n- assertScreenshot: shots/home.v2.png\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let name = suite.tests[0]
            .steps
            .iter()
            .find_map(|s| match &s.action {
                atalanta_common::Action::AssertScreenshot { name, .. } => Some(name.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(name, "home_v2");
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(result.issues.iter().any(|i| i
            .message
            .contains("atalanta-baselines/<platform>/<W>x<H>/home_v2.png")));
    }

    #[test]
    fn two_documents_bare_commands_and_keys_convert() {
        let (result, suite) = migrate(
            "basic",
            &[(
                "flow.yaml",
                "appId: com.shop\nname: Checkout\ntags: [smoke]\n---\n- launchApp\n- tapOn: \"Cart\"\n- tapOn:\n    id: \"pay\"\n    below: \"Total\"\n- inputText: \"4242\"\n- pressKey: Enter\n- back\n- swipe: UP\n- takeScreenshot: cart\n- waitForAnimationToEnd\n- extendedWaitUntil:\n    visible: \"Paid\"\n    timeout: 10000\n",
            )],
        );
        assert!(result.success);
        let t = &suite.tests[0];
        assert_eq!(t.name, "Checkout");
        assert!(t.tags.contains(&"smoke".to_string()));
        let names: Vec<String> = t.steps.iter().map(|s| format!("{:?}", s.action)).collect();
        assert!(names[0].starts_with("LaunchApp"));
        assert!(names[2].contains("Relative"), "{}", names[2]);
        assert!(names[3].starts_with("InputRaw"));
        assert!(names[4].contains("Enter"));
        assert_eq!(t.steps.last().unwrap().timeout_ms, Some(10000));
        assert_eq!(
            result.steps_skipped, 1,
            "waitForAnimationToEnd isn't needed"
        );
    }

    #[test]
    fn run_flow_files_become_suite_flows_and_cycles_are_caught() {
        let (result, suite) = migrate(
            "flows",
            &[
                (
                    "main.yaml",
                    "appId: a\n---\n- runFlow: login.yaml\n- tapOn: \"Home\"\n",
                ),
                ("login.yaml", "appId: a\n---\n- tapOn: \"Log in\"\n"),
            ],
        );
        assert!(result.success);
        assert_eq!(suite.flows.len(), 1);
        assert_eq!(suite.flows[0].id, "login");

        let d = dir("cycle");
        fs::write(d.join("a.yaml"), "appId: a\n---\n- runFlow: b.yaml\n").unwrap();
        fs::write(d.join("b.yaml"), "appId: a\n---\n- runFlow: a.yaml\n").unwrap();
        let r = MaestroMigrator::new()
            .migrate_file(
                &d.join("a.yaml").to_string_lossy(),
                &d.join("o.yaml").to_string_lossy(),
            )
            .unwrap();
        assert!(!r.success);
        assert!(r.issues.iter().any(|i| i.message.contains("cycle")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_counted_repeat_is_unrolled_and_a_while_is_not() {
        let (_, suite) = migrate(
            "repeat",
            &[(
                "f.yaml",
                "appId: a\n---\n- repeat:\n    times: 3\n    commands:\n      - tapOn:\n          id: \"b\"\n- repeat:\n    while:\n      notVisible: \"x\"\n    commands:\n      - tapOn: \"y\"\n- tapOn: \"z\"\n",
            )],
        );
        let steps: Vec<String> = suite.tests[0]
            .steps
            .iter()
            .map(|s| format!("{:?}", s.action))
            .collect();
        assert_eq!(steps.len(), 4, "{steps:?}");
        assert!(
            steps[..3].iter().all(|s| s.contains("Id(\"b\")")),
            "{steps:?}"
        );
        assert!(steps[3].contains("\"z\""), "{steps:?}");
    }

    #[test]
    fn kill_app_becomes_stop_app() {
        let (result, suite) = migrate(
            "killapp",
            &[(
                "f.yaml",
                "appId: a\n---\n- launchApp\n- killApp\n- killApp: other.app\n- killApp:\n    appId: third.app\n",
            )],
        );
        let s = actions(&suite);
        assert_eq!(s.len(), 4, "{s:?}");
        assert!(s[1].starts_with("StopApp"), "{s:?}");
        assert!(
            s[2].contains("other.app") && s[3].contains("third.app"),
            "{s:?}"
        );
        assert!(!result
            .issues
            .iter()
            .any(|i| i.severity == Severity::Warning));
    }

    #[test]
    fn a_repeat_over_the_cap_and_a_while_are_skipped_with_a_reason() {
        let (result, suite) = migrate(
            "repeatcap",
            &[(
                "f.yaml",
                &format!(
                    "appId: a\n---\n- repeat:\n    times: {}\n    commands:\n      - tapOn: \"b\"\n- repeat:\n    while:\n      visible: \"More\"\n    commands:\n      - tapOn: \"More\"\n- tapOn: \"z\"\n",
                    MAX_UNROLL + 1
                ),
            )],
        );
        assert_eq!(actions(&suite).len(), 1);
        assert!(warned(&result, "is over"));
        assert!(warned(&result, "depends on the screen at run time"));
        assert_eq!(result.steps_skipped, 2);
    }

    #[test]
    fn scripts_are_skipped_pointing_at_the_http_step() {
        let (result, _) = migrate(
            "scripts",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn: \"Go\"\n- runScript: seed.js\n- evalScript: ${output.x = 1}\n",
            )],
        );
        for construct in ["runScript", "evalScript"] {
            let i = result
                .issues
                .iter()
                .find(|i| i.maestro_construct == construct)
                .unwrap();
            assert_eq!(i.severity, Severity::Warning);
            assert!(i.message.contains("`http` step"), "{}", i.message);
        }
    }

    #[test]
    fn set_location_becomes_one_exec_step_per_platform() {
        let (result, suite) = migrate(
            "setloc",
            &[(
                "f.yaml",
                "appId: a\n---\n- setLocation:\n    latitude: 52.37\n    longitude: \"4.89\"\n- tapOn: \"Go\"\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let a = actions(&suite);
        assert!(
            a[0].contains("adb") && a[0].contains("4.89") && a[0].contains("52.37"),
            "{}",
            a[0]
        );
        assert!(
            a[1].contains("simctl") && a[1].contains("52.37,4.89"),
            "{}",
            a[1]
        );
        assert!(suite.tests[0].steps[0].modifiers.optional);
        assert!(suite.tests[0].steps[1].modifiers.optional);
        let allow = &suite.config.exec.as_ref().unwrap().allow;
        assert!(
            allow.iter().any(|p| p == "adb") && allow.iter().any(|p| p == "xcrun"),
            "{allow:?}"
        );
    }

    /// Review 15 §2: device settings Maestro sets itself are one adb or
    /// simctl call each, so they convert instead of being skipped.
    #[test]
    fn device_settings_become_exec_steps() {
        let (result, suite) = migrate(
            "devcmds",
            &[(
                "f.yaml",
                "appId: com.example.app\n---\n- setPermissions:\n    permissions:\n      camera: allow\n      location: deny\n- clearKeychain\n- setOrientation: LANDSCAPE_LEFT\n- setAirplaneMode: enabled\n- addMedia:\n    - ./img/cat.png\n- tapOn: \"Go\"\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let a = actions(&suite).join("\n");
        assert!(
            a.contains("\"pm\", \"grant\", \"com.example.app\", \"android.permission.CAMERA\""),
            "{a}"
        );
        assert!(
            a.contains("\"privacy\", \"${ATALANTA_DEVICE_ID}\", \"revoke\", \"location\""),
            "{a}"
        );
        assert!(a.contains("\"keychain\""), "{a}");
        assert!(a.contains("\"user_rotation\", \"1\""), "{a}");
        assert!(a.contains("\"airplane-mode\", \"enable\""), "{a}");
        assert!(a.contains("\"addmedia\"") && a.contains("cat.png"), "{a}");
        assert!(
            !result
                .issues
                .iter()
                .any(|i| i.severity == Severity::Warning),
            "{:?}",
            result.issues
        );
        let allow = &suite.config.exec.as_ref().unwrap().allow;
        assert!(allow.iter().any(|p| p == "adb") && allow.iter().any(|p| p == "xcrun"));
    }

    #[test]
    fn header_env_becomes_defaults_that_env_still_overrides() {
        let (result, suite) = migrate(
            "envdef",
            &[(
                "f.yaml",
                "appId: a\nenv:\n  VEL_MIG_USER: alice\n  VEL_MIG_ODD: \"a}b\"\n---\n- tapOn: \"${VEL_MIG_USER}\"\n- inputText: \"${VEL_MIG_ODD}\"\n",
            )],
        );
        assert!(result.success, "{:?}", result.issues);
        let a = actions(&suite).join("\n");
        assert!(a.contains("${VEL_MIG_USER:-alice}"), "{a}");
        assert!(a.contains("${VEL_MIG_ODD}"), "left for --env: {a}");
        let note = result
            .issues
            .iter()
            .find(|i| i.maestro_construct == "env")
            .unwrap();
        assert!(note.message.contains("VEL_MIG_ODD"), "{}", note.message);
    }

    #[test]
    fn a_when_visible_flow_is_gated_on_the_condition_and_optional() {
        let (result, suite) = migrate(
            "whenvis",
            &[
                (
                    "f.yaml",
                    "appId: a\n---\n- runFlow:\n    when:\n      visible: \"Allow\"\n    file: allow.yaml\n- tapOn: \"Go\"\n",
                ),
                ("allow.yaml", "appId: a\n---\n- tapOn: \"Allow\"\n"),
            ],
        );
        assert!(result.success, "{:?}", result.issues);
        assert!(!warned(&result, "always runs"), "{:?}", result.issues);
        assert!(suite.tests[0].steps[0].modifiers.optional);
        let tests = atalanta_core::resolver::resolve_flows(&suite).unwrap();
        let steps = &tests[0].steps;
        // The first inlined step is the gate, inside one optional block.
        let first = format!("{:?}", steps[0].action);
        assert!(
            first.contains("AssertVisible") && first.contains("Allow"),
            "{first}"
        );
        assert!(
            !steps[0].modifiers.optional_flows.is_empty(),
            "{:?}",
            steps[0]
        );
        assert!(format!("{:?}", steps[1].action).contains("Allow"));
        assert!(steps.last().unwrap().modifiers.optional_flows.is_empty());
    }

    #[test]
    fn a_when_platform_flow_runs_as_an_optional_block_with_a_warning() {
        let (result, suite) = migrate(
            "whenplat",
            &[
                (
                    "f.yaml",
                    "appId: a\n---\n- runFlow:\n    when:\n      platform: Android\n    file: a.yaml\n",
                ),
                ("a.yaml", "appId: a\n---\n- tapOn: \"Only on Android\"\n"),
            ],
        );
        assert!(warned(&result, "can't be checked"), "{:?}", result.issues);
        let tests = atalanta_core::resolver::resolve_flows(&suite).unwrap();
        assert!(!tests[0].steps[0].modifiers.optional_flows.is_empty());
    }

    #[test]
    fn copy_text_from_says_how_to_get_the_value_instead() {
        let (result, _) = migrate(
            "copytext",
            &[(
                "f.yaml",
                "appId: a\n---\n- copyTextFrom: { id: \"order\" }\n- pasteText\n- tapOn: \"Go\"\n",
            )],
        );
        assert!(warned(&result, "`capture:`"), "{:?}", result.issues);
        assert!(warned(&result, "inputText: ${NAME}"), "{:?}", result.issues);
    }

    #[test]
    fn scroll_budget_becomes_max_scrolls() {
        let (_, suite) = migrate(
            "scrollbudget",
            &[(
                "f.yaml",
                "appId: a\n---\n- scrollUntilVisible:\n    element: \"Far\"\n- scrollUntilVisible:\n    element: \"Near\"\n    timeout: 8000\n",
            )],
        );
        let s: Vec<String> = suite.tests[0]
            .steps
            .iter()
            .map(|s| format!("{:?}", s.action))
            .collect();
        assert!(s[0].contains("max_scrolls: 20"), "{}", s[0]);
        assert!(s[1].contains("max_scrolls: 8"), "{}", s[1]);
    }

    #[test]
    fn swipe_from_an_element_becomes_within() {
        let (_, suite) = migrate(
            "swipefrom",
            &[(
                "f.yaml",
                "appId: a\n---\n- swipe:\n    from:\n      id: \"pager\"\n    direction: LEFT\n",
            )],
        );
        let s = format!("{:?}", suite.tests[0].steps[0].action);
        assert!(
            s.contains("Left") && s.contains("within: Some(Id(\"pager\"))"),
            "{s}"
        );
    }

    #[test]
    fn unsupported_steps_are_reported_with_their_line() {
        let (result, _) = migrate(
            "unsup",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn: \"Go\"\n- runScript: x.js\n",
            )],
        );
        let i = result
            .issues
            .iter()
            .find(|i| i.maestro_construct == "runScript")
            .unwrap();
        assert_eq!(i.line, 4);
    }

    fn warned(r: &FileMigrationResult, needle: &str) -> bool {
        r.issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.message.contains(needle))
    }

    fn actions(suite: &atalanta_common::TestSuite) -> Vec<String> {
        suite.tests[0]
            .steps
            .iter()
            .map(|s| format!("{:?}", s.action))
            .collect()
    }

    // VEL-017: flows/a/login.yaml and flows/b/login.yaml stay two flows.
    #[test]
    fn same_named_sub_flows_in_different_folders_stay_apart() {
        let d = dir("samestem");
        fs::create_dir_all(d.join("flows/a")).unwrap();
        fs::create_dir_all(d.join("flows/b")).unwrap();
        fs::write(
            d.join("main.yaml"),
            "appId: a\n---\n- runFlow: flows/a/login.yaml\n- runFlow: flows/b/login.yaml\n- runFlow: flows/a/login.yaml\n",
        )
        .unwrap();
        fs::write(
            d.join("flows/a/login.yaml"),
            "appId: a\n---\n- tapOn: \"A\"\n",
        )
        .unwrap();
        fs::write(
            d.join("flows/b/login.yaml"),
            "appId: a\n---\n- tapOn: \"B\"\n",
        )
        .unwrap();
        let out = d.join("o.yaml");
        let r = MaestroMigrator::new()
            .migrate_file(
                &d.join("main.yaml").to_string_lossy(),
                &out.to_string_lossy(),
            )
            .unwrap();
        assert!(r.success, "{:?}", r.issues);
        let suite = atalanta_core::parser::parse_suite_from_str(&fs::read_to_string(&out).unwrap())
            .unwrap();
        let _ = fs::remove_dir_all(&d);
        assert_eq!(suite.flows.len(), 2);
        let ids: Vec<String> = actions(&suite)
            .iter()
            .map(|a| a.split('"').nth(1).unwrap_or_default().to_string())
            .collect();
        assert_eq!(ids, ["login", "b_login", "login"]);
        let flow = |id: &str| format!("{:?}", suite.flows.iter().find(|f| f.id == id).unwrap());
        assert!(flow("login").contains("\"A\"") && flow("b_login").contains("\"B\""));
    }

    // VEL-080: numeric strings are coerced, `${VAR}` numbers are reported,
    // and success means the output loads.
    #[test]
    fn numeric_strings_are_coerced_and_output_is_checked() {
        let (r, suite) = migrate(
            "numstr",
            &[(
                "f.yaml",
                "appId: a\n---\n- extendedWaitUntil:\n    visible: \"Paid\"\n    timeout: \"10000\"\n- extendedWaitUntil:\n    visible: \"Done\"\n    timeout: ${T}\n- tapOn:\n    text: \"Row\"\n    index: \"1\"\n",
            )],
        );
        assert!(r.success, "{:?}", r.issues);
        assert_eq!(suite.tests[0].steps[0].timeout_ms, Some(10000));
        assert_eq!(suite.tests[0].steps[1].timeout_ms, None);
        assert!(warned(&r, "must be a number"), "{:?}", r.issues);
        assert!(actions(&suite)[2].contains("index: 1"));
        assert!(
            check_output("appId: a\ntests:\n- name: t\n  steps:\n  - runFlow: nope\n").is_err()
        );
    }

    // S2: Maestro's `enabled` / `checked` / `focused` / `selected` carry
    // over as Atalanta's state filters (they were dropped with a warning,
    // so `tapOn: { text: Next, enabled: true }` tapped a disabled Next).
    #[test]
    fn maestro_state_keys_carry_over() {
        let (r, suite) = migrate(
            "states",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn:\n    text: \"Next\"\n    enabled: true\n- assertVisible:\n    id: \"remember\"\n    checked: false\n    focused: false\n    selected: true\n- tapOn:\n    enabled: true\n",
            )],
        );
        assert!(r.success, "{:?}", r.issues);
        let a = actions(&suite);
        assert!(a[0].contains("Enabled(true)"), "{a:?}");
        assert!(
            a[1].contains("Checked(false)")
                && a[1].contains("Focused(false)")
                && a[1].contains("Selected(true)"),
            "{a:?}"
        );
        assert_eq!(a.len(), 2, "a state alone is skipped: {a:?}");
        assert!(warned(&r, "state without a selector"), "{:?}", r.issues);
        assert!(!warned(&r, "no Atalanta equivalent"), "{:?}", r.issues);
    }

    // VEL-081: header hooks become steps; dropped keys are warnings.
    #[test]
    fn hooks_become_steps_and_dropped_keys_are_reported() {
        let (r, suite) = migrate(
            "hooks",
            &[(
                "f.yaml",
                "appId: a\nonFlowStart:\n  - tapOn: \"Setup\"\nonFlowComplete:\n  - tapOn: \"Teardown\"\n---\n- launchApp:\n    appId: a\n    permissions: { all: deny }\n- stopApp:\n    appId: com.other\n- tapOn: \"Main\"\n",
            )],
        );
        let a = actions(&suite);
        assert!(a[0].contains("Setup"), "{a:?}");
        assert!(a.last().unwrap().contains("Teardown"), "{a:?}");
        assert!(a[2].contains("com.other"), "{a:?}");
        assert!(warned(&r, "'permissions' of launchApp"), "{:?}", r.issues);
        assert!(warned(&r, "onFlowComplete"), "{:?}", r.issues);
    }

    // VEL-238 / VEL-345: `optional` is a warning; `repeat: N` taps N times.
    #[test]
    fn tap_repeat_is_unrolled_and_optional_is_kept() {
        let (r, suite) = migrate(
            "taprepeat",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn:\n    id: qty_plus\n    repeat: 3\n    delay: 200\n- tapOn:\n    text: Promo\n    optional: true\n",
            )],
        );
        let a = actions(&suite);
        assert_eq!(
            a.iter().filter(|s| s.contains("qty_plus")).count(),
            3,
            "{a:?}"
        );
        assert_eq!(
            a.iter().filter(|s| s.contains("Wait { ms: 200 }")).count(),
            2,
            "{a:?}"
        );
        // D-12: `optional: true` is the step's `optional: true` now.
        assert!(!warned(&r, "'optional'"), "{:?}", r.issues);
        let promo = suite.tests[0]
            .steps
            .iter()
            .find(|s| format!("{:?}", s.action).contains("Promo"))
            .unwrap();
        assert!(promo.modifiers.optional);
        assert!(!suite.tests[0].steps[0].modifiers.optional);
    }

    // D-12: assertWithAI is a natural-language assert, optional unless
    // the flow says `optional: false`.
    #[test]
    fn assert_with_ai_becomes_an_optional_assert() {
        let (r, suite) = migrate(
            "assertai",
            &[(
                "f.yaml",
                "appId: a\n---\n- assertWithAI:\n    assertion: \"The cart shows 2 items\"\n- assertWithAI:\n    assertion: \"A login button is shown\"\n    optional: false\n- assertVisible:\n    text: Done\n    optional: true\n",
            )],
        );
        let steps = &suite.tests[0].steps;
        assert_eq!(steps.len(), 3, "{:?}", r.issues);
        assert!(matches!(
            &steps[0].action,
            atalanta_common::Action::Natural { intent, kind: atalanta_common::NaturalKind::Assert, .. }
                if intent == "The cart shows 2 items"
        ));
        assert!(steps[0].modifiers.optional);
        assert!(!steps[1].modifiers.optional);
        assert!(steps[2].modifiers.optional);
        assert!(
            !r.issues.iter().any(|i| i.message.contains("skipped")),
            "{:?}",
            r.issues
        );
    }

    // VEL-346: a right-to-left percentage swipe is a left swipe.
    #[test]
    fn percentage_swipe_keeps_its_direction() {
        let (_, suite) = migrate(
            "pctswipe",
            &[(
                "f.yaml",
                "appId: a\n---\n- swipe:\n    start: \"90%, 50%\"\n    end: \"10%, 50%\"\n- swipe:\n    start: \"50%, 20%\"\n    end: \"50%, 80%\"\n",
            )],
        );
        let a = actions(&suite);
        assert!(a[0].contains("Left"), "{a:?}");
        assert!(a[1].contains("Down"), "{a:?}");
    }

    // VEL-241 / VEL-082: escaped brackets are literal text; real patterns
    // and JavaScript expressions are reported.
    #[test]
    fn regex_and_expression_detection() {
        let (r, suite) = migrate(
            "regex",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn: \"\\\\[Beta\\\\] Feature\"\n- tapOn: \"Item \\\\d+\"\n- inputText: ${maestro.copiedText}\n- inputText: ${USER}\n",
            )],
        );
        let a = actions(&suite);
        assert!(a[0].contains("\"[Beta] Feature\""), "{a:?}");
        // D-11: a real pattern keeps its meaning as textMatches.
        assert!(a[1].contains("TextMatches(\"Item \\\\d+\")"), "{a:?}");
        assert!(!warned(&r, "Item \\d+"), "{:?}", r.issues);
        assert!(!warned(&r, "[Beta]"), "{:?}", r.issues);
        assert!(warned(&r, "maestro.copiedText"), "{:?}", r.issues);
        assert!(!warned(&r, "${USER}"), "{:?}", r.issues);
    }

    // Review MAJOR 6: text that doesn't compile as a regex (Maestro then
    // matches it as plain text) or only uses brackets as punctuation stays
    // `text:`; the migrated suite always validates.
    #[test]
    fn text_that_is_not_a_real_pattern_stays_text() {
        let (r, suite) = migrate(
            "notregex",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn: \"+44 20 7946 0958\"\n- tapOn: \"* Email\"\n- tapOn: \"Done :)\"\n- tapOn: \"Price (USD)\"\n- tapOn: \"[Beta\"\n- tapOn:\n    id: \"row(\"\n- tapOn: \"Item [0-9]+\"\n",
            )],
        );
        assert!(r.success, "{:?}", r.issues);
        let a = actions(&suite);
        assert!(a[0].contains("Text(\"+44 20 7946 0958\")"), "{a:?}");
        assert!(a[1].contains("Text(\"* Email\")"), "{a:?}");
        assert!(a[2].contains("Text(\"Done :)\")"), "{a:?}");
        assert!(a[3].contains("Text(\"Price (USD)\")"), "{a:?}");
        assert!(a[4].contains("Text(\"[Beta\")"), "{a:?}");
        assert!(a[5].contains("Id(\"row(\")"), "{a:?}");
        assert!(a[6].contains("TextMatches(\"Item [0-9]+\")"), "{a:?}");
        assert!(warned(&r, "+44 20"), "{:?}", r.issues);
        assert!(!warned(&r, "Price (USD)"), "{:?}", r.issues);
        // Every selector the migration wrote passes `atalanta validate`.
        atalanta_core::validate_suite(&suite).unwrap();
    }

    // D-11: Maestro's `id:` is a regex too; a plain id stays `id:`.
    #[test]
    fn regex_ids_become_id_matches() {
        let (_, suite) = migrate(
            "idre",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn:\n    id: \"row_.*\"\n- tapOn:\n    id: \"login\"\n- assertVisible:\n    text: \"Total: .*\"\n",
            )],
        );
        let a = actions(&suite);
        assert!(a[0].contains("IdMatches(\"row_.*\")"), "{a:?}");
        assert!(a[1].contains("Id(\"login\")"), "{a:?}");
        assert!(a[2].contains("TextMatches(\"Total: .*\")"), "{a:?}");
    }

    // No literal "TODO" is ever typed.
    #[test]
    fn random_input_types_a_required_variable_not_todo() {
        let (r, suite) = migrate(
            "random",
            &[("f.yaml", "appId: a\n---\n- inputRandomEmail\n")],
        );
        let a = actions(&suite);
        assert!(!a[0].contains("TODO"), "{a:?}");
        assert!(a[0].contains("RANDOM_EMAIL"), "{a:?}");
        assert!(warned(&r, "RANDOM_EMAIL"), "{:?}", r.issues);
    }

    // Review MINOR 16: a parameterised runFlow path is a Warning and a
    // placeholder, not an Error that writes nothing.
    #[test]
    fn a_parameterised_run_flow_becomes_a_placeholder() {
        let (r, suite) = migrate(
            "paramflow",
            &[(
                "f.yaml",
                "appId: a\n---\n- runFlow: ${FLOW}\n- tapOn: \"OK\"\n",
            )],
        );
        assert!(r.success, "{:?}", r.issues);
        assert!(warned(&r, "parameterised"), "{:?}", r.issues);
        let a = actions(&suite);
        assert!(a[0].contains("parameterised_flow"), "{a:?}");
        let flow = format!("{:?}", suite.flows);
        assert!(flow.contains("MAESTRO_RUNFLOW"), "{flow}");
    }

    // Review MINOR 17: a non-numeric index can't be dropped silently.
    #[test]
    fn a_non_numeric_index_is_an_error() {
        let d = dir("idxvar");
        fs::write(
            d.join("f.yaml"),
            "appId: a\n---\n- tapOn:\n    text: Item\n    index: ${IDX}\n",
        )
        .unwrap();
        let out = d.join("out.atalanta.yaml");
        let r = MaestroMigrator::new()
            .migrate_file(&d.join("f.yaml").to_string_lossy(), &out.to_string_lossy())
            .unwrap();
        let written = out.exists();
        let _ = fs::remove_dir_all(&d);
        assert!(!r.success, "{:?}", r.issues);
        assert!(!written);
        assert!(r
            .issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.message.contains("index")));
    }

    // VEL-239: flow.yaml and flow.yml don't overwrite each other.
    #[test]
    fn same_stem_files_in_a_directory_write_two_outputs() {
        let d = dir("dirstem");
        fs::write(d.join("flow.yaml"), "appId: a\n---\n- tapOn: \"Y\"\n").unwrap();
        fs::write(d.join("flow.yml"), "appId: a\n---\n- tapOn: \"M\"\n").unwrap();
        let out = d.join("out");
        let rep = MaestroMigrator::new()
            .migrate_directory(&d.to_string_lossy(), &out.to_string_lossy())
            .unwrap();
        let written: HashSet<_> = rep
            .results
            .iter()
            .filter_map(|r| r.output_file.clone())
            .collect();
        let _ = fs::remove_dir_all(&d);
        assert_eq!(written.len(), 2, "{written:?}");
    }

    #[test]
    fn an_absolute_point_tap_becomes_tap_xy_and_others_are_noted() {
        let (result, suite) = migrate(
            "pointtap",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn:\n    point: \"100, 200\"\n- tapOn:\n    point: \"50%,50%\"\n- tapOn:\n    id: \"card\"\n    point: \"10,10\"\n- longPressOn:\n    point: \"5,5\"\n",
            )],
        );
        assert_eq!(actions(&suite), ["TapXY { x: 100, y: 200 }"]);
        assert!(warned(&result, "percentage point"), "{:?}", result.issues);
        assert!(
            warned(&result, "point inside an element"),
            "{:?}",
            result.issues
        );
        assert_eq!(result.steps_skipped, 3);
    }

    #[test]
    fn an_absolute_swipe_keeps_its_points() {
        let (_, suite) = migrate(
            "swipeabs",
            &[(
                "f.yaml",
                "appId: a\n---\n- swipe:\n    start: \"100,800\"\n    end: \"100, 200\"\n",
            )],
        );
        let s = &actions(&suite)[0];
        assert!(
            s.contains("from: Some((100, 800))") && s.contains("to: Some((100, 200))"),
            "{s}"
        );
    }

    #[test]
    fn erase_text_becomes_backspaces() {
        let (result, suite) = migrate(
            "erase",
            &[(
                "f.yaml",
                "appId: a\n---\n- eraseText:\n    charactersToErase: 3\n- eraseText: 2\n- eraseText\n- eraseText:\n    charactersToErase: 500\n",
            )],
        );
        let a = actions(&suite);
        assert_eq!(a.len(), 3 + 2 + 50, "{a:?}");
        assert!(
            a.iter().all(|s| s == "PressKey { key: Backspace }"),
            "{a:?}"
        );
        assert!(warned(&result, "over 100"), "{:?}", result.issues);
        assert!(!result
            .issues
            .iter()
            .any(|i| i.message.contains("Unsupported")));
    }

    #[test]
    fn open_link_forms_convert_and_unknown_options_are_reported() {
        let (result, suite) = migrate(
            "openlink",
            &[(
                "f.yaml",
                "appId: a\n---\n- openLink: \"myapp://home\"\n- openLink:\n    link: \"https://x.test/p\"\n    autoVerify: true\n    browser: true\n- openLink:\n    autoVerify: true\n",
            )],
        );
        let a = actions(&suite);
        assert_eq!(a.len(), 2, "{a:?}");
        assert!(a[0].contains("myapp://home") && a[1].contains("https://x.test/p"));
        assert!(
            warned(&result, "'browser' of openLink"),
            "{:?}",
            result.issues
        );
        assert!(warned(&result, "'autoVerify' of openLink"));
        assert!(warned(&result, "openLink without a link"));
    }

    #[test]
    fn stop_app_and_clear_state_forms_keep_their_app() {
        let (_, suite) = migrate(
            "stopclear",
            &[(
                "f.yaml",
                "appId: a\n---\n- stopApp\n- stopApp: com.other\n- stopApp:\n    appId: com.third\n- clearState\n- clearState: com.other\n- clearState:\n    appId: com.third\n",
            )],
        );
        let a = actions(&suite);
        assert!(
            a[1].contains("com.other") && a[2].contains("com.third"),
            "{a:?}"
        );
        assert!(a[3].contains("LaunchApp") && a[3].contains("clear_state: true"));
        assert!(a[4].contains("\"com.other\"") && a[4].contains("clear_state: true"));
        assert!(a[5].contains("\"com.third\"") && a[5].contains("clear_state: true"));
    }

    #[test]
    fn input_text_types_into_the_focus_and_animation_waits_are_dropped() {
        let (result, suite) = migrate(
            "inputraw",
            &[(
                "f.yaml",
                "appId: a\n---\n- inputText: \"hello\"\n- inputText:\n    text: 42\n- waitForAnimationToEnd\n- waitForAnimationToEnd:\n    timeout: 3000\n",
            )],
        );
        assert_eq!(
            actions(&suite),
            ["InputRaw { text: \"hello\" }", "InputRaw { text: \"42\" }"]
        );
        let noted = result
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Info && i.message.contains("isn't needed"))
            .count();
        assert_eq!(noted, 2, "{:?}", result.issues);
    }

    #[test]
    fn scroll_until_visible_options_and_state_filters_carry_over() {
        let (result, suite) = migrate(
            "scrollopts",
            &[(
                "f.yaml",
                "appId: a\n---\n- scrollUntilVisible:\n    element:\n      text: \"Agree\"\n      enabled: true\n    direction: UP\n    timeout: 10000\n    speed: 40\n- assertVisible:\n    id: \"terms\"\n    checked: true\n",
            )],
        );
        let a = actions(&suite);
        assert!(
            a[0].contains("Up")
                && a[0].contains("max_scrolls: 10")
                && a[0].contains("Enabled(true)"),
            "{a:?}"
        );
        assert!(a[1].contains("Checked(true)"), "{a:?}");
        assert!(warned(&result, "'speed' of scrollUntilVisible"));
    }

    #[test]
    fn maestro_only_device_commands_are_unsupported_not_unknown() {
        let (result, _) = migrate(
            "devicecmds",
            &[(
                "f.yaml",
                "appId: a\n---\n- tapOn: \"Go\"\n- toggleAirplaneMode\n- setOrientation: SIDEWAYS\n",
            )],
        );
        // What `convert_device_command` can't express is still reported,
        // never mistaken for an unknown command.
        assert!(warned(
            &result,
            "Unsupported Maestro construct 'toggleAirplaneMode'"
        ));
        assert!(warned(
            &result,
            "Unsupported Maestro construct 'setOrientation'"
        ));
    }
}
