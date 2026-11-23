use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use velocity_common::{
    validate_launch_arg_name, Action, Direction, Flow, Key, LaunchArg, LaunchArgs, NaturalKind,
    Result, Selector, SourcePos, Step, SuiteConfig, SuiteResult, TestCase, TestSuite,
    VelocityError,
};

use crate::source_map::{self, ItemLocation, SuiteLocations};

/// YAML files that sit next to suites but aren't suites: the impact map
/// and the quarantine list (read from the suite's folder by default).
pub const NOT_SUITES: &[&str] = &[
    "velocity-impact.yaml",
    "velocity-impact.yml",
    "velocity-quarantine.yaml",
    "velocity-quarantine.yml",
];

/// Whether a folder run (`run <dir>`, `validate <dir>`) treats `path` as a
/// suite: a `.yaml` / `.yml` file that isn't one of [`NOT_SUITES`].
pub fn is_suite_file(path: &std::path::Path) -> bool {
    let yaml = path.extension().is_some_and(|x| x == "yaml" || x == "yml");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    yaml && !NOT_SUITES.contains(&name.as_str())
}

pub fn parse_suite(path: &str) -> Result<TestSuite> {
    parse_suite_with_positions(path).map(|(suite, _)| suite)
}

/// Parse a suite file, and also return the `- name:` line of each test (by
/// name) for reports: JUnit `line`, GitHub annotations, the HTML report.
/// Steps carry their own position in `Step::source`.
pub fn parse_suite_with_positions(path: &str) -> Result<(TestSuite, HashMap<String, SourcePos>)> {
    // Name the file: "No such file or directory" alone didn't (VEL-200).
    let content = std::fs::read_to_string(path)
        .map_err(|e| VelocityError::Config(format!("can't read {path}: {e}")))?;
    let (mut suite, tests) = parse_located(&content, path)?;
    suite.config.suite_dir = std::path::Path::new(path).parent().map(|d| d.to_path_buf());
    Ok((suite, tests))
}

pub fn parse_suite_from_str(yaml: &str) -> Result<TestSuite> {
    parse_located(yaml, "").map(|(suite, _)| suite)
}

fn parse_located(yaml: &str, file: &str) -> Result<(TestSuite, HashMap<String, SourcePos>)> {
    // Editors on Windows save a byte-order mark; YAML read it as part of the
    // first key ("missing field `appId`", VEL-198).
    let yaml = yaml.strip_prefix('\u{feff}').unwrap_or(yaml);
    let raw: RawTestSuite = serde_yaml::from_str(yaml).map_err(|e| {
        // A Maestro flow (`appId:`, `---`, then a list of commands) said
        // only "missing field `tests`" (VEL-218).
        let maestro = yaml.lines().any(|l| l.trim_end() == "---")
            && yaml.lines().any(|l| l.starts_with("- "));
        match yaml_error(e, file) {
            VelocityError::YamlParse {
                file,
                line,
                col,
                message,
            } if maestro && message.contains("missing field `tests`") => {
                VelocityError::YamlParse {
                    file,
                    line,
                    col,
                    message: format!(
                        "{message}\nhint: this looks like a Maestro flow; convert it with `velocity migrate maestro`"
                    ),
                }
            }
            other => other,
        }
    })?;
    let locations = source_map::locate(yaml).unwrap_or_default();
    let tests: HashMap<String, SourcePos> = raw
        .tests
        .iter()
        .zip(&locations.tests)
        .filter_map(|(t, loc)| Some((t.name.clone(), pos(file, loc.at?))))
        .collect();
    Ok((raw.into_test_suite(&locations, file), tests))
}

fn pos(file: &str, m: source_map::Mark) -> SourcePos {
    SourcePos::new(file, m.line, m.column)
}

/// serde_yaml's error, with its location moved out of the message so it
/// reads `tests/app.yaml:12:9: tests[0].steps[1]: unknown action ...`.
fn yaml_error(e: serde_yaml::Error, file: &str) -> VelocityError {
    let msg = with_hint(e.to_string());
    match e.location() {
        Some(loc) => {
            let suffix = format!(" at line {} column {}", loc.line(), loc.column());
            let message = match msg.find(&suffix) {
                Some(at) => format!("{}{}", &msg[..at], &msg[at + suffix.len()..]),
                None => msg,
            };
            VelocityError::YamlParse {
                file: file.into(),
                line: loc.line(),
                col: loc.column(),
                message,
            }
        }
        None if file.is_empty() => VelocityError::Config(msg),
        None => VelocityError::YamlParse {
            file: file.into(),
            line: 0,
            col: 0,
            message: msg,
        },
    }
}

/// Fill in each test result's `source` (its `- name:` line) from the
/// positions `parse_suite_with_positions` returned. Runners don't know the
/// file, so the caller that parsed it does this once, before reporting.
pub fn stamp_sources(result: &mut SuiteResult, tests: &HashMap<String, SourcePos>) {
    for t in &mut result.tests {
        if t.source.is_none() {
            t.source = tests.get(&t.test_name).cloned();
        }
    }
}

/// Point at the usual fix for the usual mistakes.
fn with_hint(msg: String) -> String {
    if msg.contains("unknown field `timeoutMs`") || msg.contains("unknown field `timeout_ms`") {
        format!(
            "{msg}\nhint: timeoutMs belongs to the step, next to the action:\n  - assertVisible: {{ text: \"Done\" }}\n    timeoutMs: 5000"
        )
    } else {
        msg
    }
}

/// The keys the suite, a test and a flow read (field names and aliases),
/// for the unknown-key warnings in [`crate::config_keys`].
pub(crate) fn suite_keys() -> [&'static [&'static str]; 3] {
    use crate::config_keys::fields;
    [
        fields::<RawTestSuite>(),
        fields::<RawTestCase>(),
        fields::<RawFlow>(),
    ]
}

/// Tag carried by tests marked `sideEffects: true`.
pub const SIDE_EFFECTS_TAG: &str = "side-effects";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTestSuite {
    /// Optional for a suite of only `http` steps (the validator says so
    /// for any other).
    #[serde(default)]
    app_id: String,
    #[serde(default)]
    config: SuiteConfig,
    #[serde(default)]
    flows: Vec<RawFlow>,
    tests: Vec<RawTestCase>,
}

impl RawTestSuite {
    fn into_test_suite(self, loc: &SuiteLocations, file: &str) -> TestSuite {
        let none = ItemLocation::default();
        TestSuite {
            app_id: self.app_id,
            config: self.config,
            flows: self
                .flows
                .into_iter()
                .enumerate()
                .map(|(i, f)| f.into_flow(loc.flows.get(i).unwrap_or(&none), file))
                .collect(),
            tests: self
                .tests
                .into_iter()
                .enumerate()
                .map(|(i, t)| t.into_test_case(loc.tests.get(i).unwrap_or(&none), file))
                .collect(),
        }
    }
}

/// Steps with their positions; a step the locator didn't see gets none.
fn located_steps(steps: Vec<DslStep>, loc: &ItemLocation, file: &str) -> Vec<Step> {
    // Index-zipping is only sound when both passes saw the same steps.
    let marks = (loc.steps.len() == steps.len()).then_some(&loc.steps);
    steps
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.into_step(marks.and_then(|m| m.get(i)).map(|m| pos(file, *m))))
        .collect()
}

#[derive(Deserialize)]
struct RawFlow {
    id: String,
    steps: Vec<DslStep>,
}

impl RawFlow {
    fn into_flow(self, loc: &ItemLocation, file: &str) -> Flow {
        Flow {
            id: self.id,
            steps: located_steps(self.steps, loc, file),
        }
    }
}

#[derive(Deserialize)]
struct RawTestCase {
    name: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    isolated: bool,
    #[serde(default, rename = "dependsOn", alias = "depends_on")]
    depends_on: Vec<String>,
    /// Never served from the result cache (it changes something outside
    /// the app: sends an email, places an order). Kept as a tag.
    #[serde(default, rename = "sideEffects")]
    side_effects: bool,
    /// What the test shows; `verify: final` checks it on the last screen.
    #[serde(default)]
    description: Option<String>,
    /// This test's `verify:` level, over the suite's.
    #[serde(default)]
    verify: Option<velocity_common::VerifyLevel>,
    steps: Vec<DslStep>,
}

impl RawTestCase {
    fn into_test_case(self, loc: &ItemLocation, file: &str) -> TestCase {
        let mut tags = self.tags;
        if self.side_effects && !tags.iter().any(|t| t == SIDE_EFFECTS_TAG) {
            tags.push(SIDE_EFFECTS_TAG.to_string());
        }
        TestCase {
            name: self.name,
            tags,
            isolated: self.isolated,
            depends_on: self.depends_on,
            steps: located_steps(self.steps, loc, file),
            description: self.description,
            verify: self.verify,
        }
    }
}

struct DslStep {
    action: Action,
    timeout_ms: Option<u64>,
    settle: Option<velocity_common::StepSettle>,
    modifiers: velocity_common::StepModifiers,
}

impl DslStep {
    fn into_step(self, source: Option<SourcePos>) -> Step {
        Step {
            action: self.action,
            timeout_ms: self.timeout_ms,
            settle: self.settle,
            source,
            modifiers: self.modifiers,
        }
    }
}

impl<'de> Deserialize<'de> for DslStep {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // A step is either a bare natural-language string ("Tap the login button")
        // or a map with a single action key (tap:, inputText:, do:, assert:, …).
        deserializer.deserialize_any(DslStepVisitor)
    }
}

struct DslStepVisitor;

impl<'de> Visitor<'de> for DslStepVisitor {
    type Value = DslStep;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a plain-English step string, or a step map with an action key like tap, inputText, do, assert")
    }

    /// A bare string step is a natural-language action, e.g. `- "Tap login"`,
    /// unless it is the name of an action that takes no arguments: `- goBack`,
    /// `- launchApp`, Maestro's `- back`. Those used to become plain-English
    /// steps, which need a configured planner and failed only at run time.
    fn visit_str<E>(self, value: &str) -> std::result::Result<DslStep, E>
    where
        E: de::Error,
    {
        let bare = match value.trim() {
            "launchApp" => Some(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
                arguments: LaunchArgs::default(),
            }),
            "stopApp" => Some(Action::StopApp {
                app_id: String::new(),
            }),
            "goBack" => Some(Action::GoBack),
            "refresh" => Some(Action::Refresh),
            "screenshot" => Some(Action::Screenshot { filename: None }),
            "back" | "pressBack" => Some(Action::PressKey {
                key: velocity_common::Key::Back,
            }),
            "hideKeyboard" => Some(Action::HideKeyboard),
            "clearMocks" => Some(Action::ClearMocks),
            _ => None,
        };
        if let Some(action) = bare {
            return Ok(DslStep {
                action,
                timeout_ms: None,
                settle: None,
                modifiers: Default::default(),
            });
        }
        // `- lanuchApp`, `- hideKeyboard`, `- tap`: one word that names (or
        // nearly names) an action is a mistake, not an instruction for the
        // planner (VEL-061).
        if let Some(msg) = bare_token_error(value.trim()) {
            return Err(de::Error::custom(msg));
        }
        Ok(DslStep {
            action: Action::Natural {
                intent: value.to_string(),
                kind: NaturalKind::Act,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
            },
            timeout_ms: None,
            settle: None,
            modifiers: Default::default(),
        })
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<DslStep, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut action: Option<(String, Action)> = None;
        let mut timeout_ms: Option<u64> = None;
        let mut settle: Option<velocity_common::StepSettle> = None;
        let mut modifiers = velocity_common::StepModifiers::default();
        // Sibling modifiers for natural-language steps.
        let mut data: Option<String> = None;
        let mut wait_until: Option<String> = None;
        // `checkpoint:`; `Some("")` is `true` (the step's own claim).
        let mut checkpoint: Option<String> = None;
        // A modifier given twice kept the last one silently (VEL-194).
        let mut seen: Vec<&'static str> = Vec::new();
        let mut once = |name: &'static str| -> std::result::Result<(), A::Error> {
            if seen.contains(&name) {
                return Err(de::Error::custom(format!(
                    "`{name}` is given twice in one step"
                )));
            }
            seen.push(name);
            Ok(())
        };

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "timeoutMs" | "timeout_ms" => {
                    once("timeoutMs")?;
                    timeout_ms = Some(map.next_value()?);
                }
                "settle" => {
                    once("settle")?;
                    settle = parse_settle(map.next_value::<serde_yaml::Value>()?)
                        .map_err(de::Error::custom)?;
                }
                "data" => {
                    once("data")?;
                    // Accept either a bare string or `{ value: "..." }` (passmark-style).
                    data = Some(parse_data_value(&mut map)?);
                }
                "waitUntil" | "wait_until" => {
                    once("waitUntil")?;
                    wait_until = Some(map.next_value()?);
                }
                "force" => {
                    once("force")?;
                    modifiers.force = flag(&mut map, "force")?;
                }
                "optional" => {
                    once("optional")?;
                    modifiers.optional = flag(&mut map, "optional")?;
                }
                "appInFront" | "app_in_front" => {
                    once("appInFront")?;
                    modifiers.app_in_front = Some(flag(&mut map, "appInFront")?);
                }
                "checkpoint" => {
                    once("checkpoint")?;
                    checkpoint = match map.next_value::<serde_yaml::Value>()? {
                        serde_yaml::Value::Bool(b) => b.then(String::new),
                        serde_yaml::Value::String(claim) if !claim.trim().is_empty() => Some(claim),
                        other => {
                            return Err(de::Error::custom(format!(
                                "`checkpoint` takes true, false or the claim to check, got {}",
                                yaml_kind(&other)
                            )))
                        }
                    };
                }
                action_key => {
                    // Decided by the key alone, so an unknown modifier gets
                    // the same error before or after the action (VEL-217).
                    if !ACTION_KEYS.contains(&action_key) {
                        return Err(de::Error::custom(unknown_step_key(action_key)));
                    }
                    if let Some((first, _)) = &action {
                        return Err(de::Error::custom(format!(
                            "step has two actions, `{first}` and `{action_key}`; put each in its own step"
                        )));
                    }
                    action = Some((action_key.to_string(), parse_action(action_key, &mut map)?));
                }
            }
        }

        let (key, mut action) = action.ok_or_else(|| {
            de::Error::custom(format!(
                "step has no action key (it has only modifiers: {})",
                MODIFIERS_HELP
            ))
        })?;
        // Attach sibling modifiers to natural-language steps; on any other
        // step they were dropped and the check never ran (VEL-194).
        if let Action::Natural {
            data: d,
            wait_until: w,
            ..
        } = &mut action
        {
            *d = data;
            *w = wait_until;
        } else if data.is_some() || wait_until.is_some() {
            let which = if data.is_some() { "data" } else { "waitUntil" };
            return Err(de::Error::custom(format!(
                "`{which}` works only on natural-language steps (do:, assert:), not on `{key}`"
            )));
        }
        // A modifier that does nothing on this action is a mistake, not a
        // no-op (VEL-194's rule for data/waitUntil).
        if modifiers.app_in_front.is_some() && !matches!(action, Action::AssertNotVisible { .. }) {
            return Err(de::Error::custom(format!(
                "`appInFront` works only on `assertNotVisible`, not on `{key}`"
            )));
        }
        // `checkpoint: true` checks a natural-language step's own claim;
        // any other step has none, so it must say what to check.
        if checkpoint.as_deref() == Some("") && !matches!(action, Action::Natural { .. }) {
            return Err(de::Error::custom(format!(
                "`checkpoint: true` works only on natural-language steps; on `{key}`, give the claim to check (checkpoint: \"the cart shows 2 items\")"
            )));
        }
        modifiers.checkpoint = checkpoint;
        if modifiers.force && !takes_force(&action) {
            return Err(de::Error::custom(format!(
                "`force` works only on actions that touch an element (tap, doubleTap, longPress, inputText, clearText, do:), not on `{key}`"
            )));
        }
        Ok(DslStep {
            action,
            timeout_ms,
            settle,
            modifiers,
        })
    }
}

/// Every action key `parse_action` reads, aliases included. The schema
/// test checks the JSON schema against this list (VEL-208).
pub const ACTION_KEYS: &[&str] = &[
    "launchApp",
    "stopApp",
    "setConfig",
    "mockRequest",
    "clearMocks",
    "assertSystemState",
    "tap",
    "doubleTap",
    "longPress",
    "drag",
    "inputText",
    "clearText",
    "assertVisible",
    "assertNotVisible",
    "assertText",
    "scrollUntilVisible",
    "swipe",
    "screenshot",
    "assertScreenshot",
    "saveState",
    "restoreState",
    "pressKey",
    "hideKeyboard",
    "wait",
    "runFlow",
    "tapXY",
    "inputRaw",
    "goto",
    "openLink",
    "goBack",
    "refresh",
    "http",
    "exec",
    "do",
    "act",
    "assert",
    "expect",
];

/// The keys a step may carry beside its action.
pub const MODIFIER_KEYS: &[&str] = &[
    "timeoutMs",
    "settle",
    "data",
    "waitUntil",
    "force",
    "optional",
    "appInFront",
    "checkpoint",
];
const MODIFIERS_HELP: &str =
    "timeoutMs, settle, optional, force, appInFront, checkpoint, and data/waitUntil on natural-language steps";

/// A `true`/`false` step modifier.
fn flag<'de, A: MapAccess<'de>>(map: &mut A, key: &str) -> std::result::Result<bool, A::Error> {
    match map.next_value::<serde_yaml::Value>()? {
        serde_yaml::Value::Bool(b) => Ok(b),
        other => Err(de::Error::custom(format!(
            "`{key}` takes true or false, got {}",
            yaml_kind(&other)
        ))),
    }
}

/// Actions whose element checks `force: true` turns off (D-3).
fn takes_force(action: &Action) -> bool {
    matches!(
        action,
        Action::Tap { .. }
            | Action::DoubleTap { .. }
            | Action::LongPress { .. }
            | Action::InputText { .. }
            | Action::ClearText { .. }
            | Action::Natural {
                kind: NaturalKind::Act,
                ..
            }
    )
}

/// Maestro commands that aren't Velocity's, and what to write instead.
fn maestro_hint(key: &str) -> Option<&'static str> {
    Some(match key {
        "tapOn" => "use `tap`",
        "doubleTapOn" => "use `doubleTap`",
        "longPressOn" => "use `longPress`",
        "takeScreenshot" => "use `screenshot`",
        "eraseText" => "use `clearText`",
        "scroll" => "use `swipe` (or `scrollUntilVisible`)",
        "killApp" => "use `stopApp`",
        "clearState" => "use `launchApp: { clearState: true }`",
        "extendedWaitUntil" => "use `assertVisible` with a step `timeoutMs`",
        "waitForAnimationToEnd" => {
            "Velocity waits for the screen to settle after every step; drop it, or use `wait: { ms: … }`"
        }
        "inputRandomText" | "inputRandomNumber" | "inputRandomEmail" | "inputRandomPersonName" => {
            "use `inputText` with a value (or `${VAR}`)"
        }
        "evalScript" | "runScript" => {
            "Velocity runs no JavaScript; for an HTTP call (Maestro's `http.post` in a script) use an `http` step"
        }
        "assertTrue" | "copyTextFrom" | "pasteText" | "setLocation"
        | "travel" | "repeat" | "retry" | "addMedia" | "clearKeychain" | "startRecording"
        | "stopRecording" => "Velocity has no equivalent step",
        _ => return None,
    })
}

/// `name` is a Maestro command: say so, with the Velocity way.
fn maestro_message(key: &str) -> Option<String> {
    maestro_hint(key).map(|hint| {
        format!("`{key}` is a Maestro command, not Velocity's: {hint} (`velocity migrate maestro` converts whole files)")
    })
}

/// The error for a step key that is neither an action nor a modifier.
fn unknown_step_key(key: &str) -> String {
    if key == "<<" {
        return "YAML merge keys (`<<:`) aren't supported in a step; write the keys out".into();
    }
    if let Some(m) = maestro_message(key) {
        return m;
    }
    let known: Vec<&str> = ACTION_KEYS.iter().chain(MODIFIER_KEYS).copied().collect();
    let near = crate::config_keys::closest(key, &known)
        .map(|k| format!("; did you mean `{k}`?"))
        .unwrap_or_default();
    format!("unknown action `{key}`{near} (step modifiers: {MODIFIERS_HELP})")
}

/// Why a one-word string step is refused, or `None` to keep it as a
/// natural-language step. A word that names an action needing arguments, a
/// Maestro command, a camelCase word, or a near-miss of an action name.
fn bare_token_error(token: &str) -> Option<String> {
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if ACTION_KEYS.contains(&token) {
        return Some(format!(
            "`{token}` needs arguments: write `- {token}: …` (see the README for its fields)"
        ));
    }
    if let Some(m) = maestro_message(token) {
        return Some(m);
    }
    // Only spellings of an action key are refused: a camelCase word
    // (`asertVisible`; at least two lowercase letters before the first
    // capital, so `iPhone` and `eBay` stay words) within two edits, or an
    // all-lowercase word within one (`scrol`). Plain English one-word
    // steps (`Export`, `Refresh`, `Slide`, `alert`) stay natural-language.
    let lead = token.chars().take_while(|c| c.is_ascii_lowercase()).count();
    let camel = lead >= 2 && token.chars().skip(lead).any(|c| c.is_ascii_uppercase());
    let lower = token.chars().all(|c| c.is_ascii_lowercase());
    let max = if camel {
        2
    } else if lower && token.len() >= 5 {
        1
    } else {
        return None;
    };
    let near = crate::config_keys::closest(token, ACTION_KEYS).filter(|k| {
        crate::config_keys::distance(&token.to_lowercase(), &k.to_lowercase()) <= max
    })?;
    let near = format!("; did you mean `{near}`?");
    Some(format!(
        "`{token}` is not an action{near} (a one-word plain-English step must be written `- do: \"{token}\"`)"
    ))
}

/// `settle: none | auto | { reactionMs, quietMs, capMs }`.
fn parse_settle(
    v: serde_yaml::Value,
) -> std::result::Result<Option<velocity_common::StepSettle>, String> {
    match v {
        serde_yaml::Value::String(s) if s == "none" || s == "off" => {
            Ok(Some(velocity_common::StepSettle {
                skip: true,
                ..Default::default()
            }))
        }
        serde_yaml::Value::String(s) if s == "auto" => Ok(None),
        serde_yaml::Value::Mapping(m) => {
            let mut out = velocity_common::StepSettle::default();
            for (k, v) in m {
                let key = k.as_str().unwrap_or("");
                let ms = v
                    .as_u64()
                    .ok_or_else(|| format!("settle.{key}: expected milliseconds"))?;
                match key {
                    "reactionMs" | "reaction_ms" => out.reaction_ms = Some(ms),
                    "quietMs" | "quiet_ms" => out.quiet_ms = Some(ms),
                    "capMs" | "cap_ms" => out.cap_ms = Some(ms),
                    other => {
                        return Err(format!(
                            "settle.{other}: unknown (use reactionMs, quietMs, capMs, or `settle: none`)"
                        ))
                    }
                }
            }
            Ok(Some(out))
        }
        other => Err(format!(
            "settle: expected none, auto or a map, got {other:?}"
        )),
    }
}

/// Parse a `data:` value that is either a plain string or `{ value: "..." }`.
fn parse_data_value<'de, A>(map: &mut A) -> std::result::Result<String, A::Error>
where
    A: MapAccess<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wrapped {
        value: String,
    }
    match name_or::<A, Wrapped>(map, "data")? {
        NameOr::Name(s) => Ok(s),
        NameOr::Full(w) => Ok(w.value),
        NameOr::Null => Err(de::Error::custom("data: expected a value")),
    }
}

/// A value written either as a plain string or as a map.
enum NameOr<T> {
    Name(String),
    Full(T),
    Null,
}

/// Read a string-or-map value. Untagged enums said only "data did not
/// match any variant of untagged enum GotoDsl"; the map is now read as its
/// own struct, so serde names the bad field (VEL-217).
fn name_or<'de, A, T>(map: &mut A, what: &str) -> std::result::Result<NameOr<T>, A::Error>
where
    A: MapAccess<'de>,
    T: serde::de::DeserializeOwned,
{
    match map.next_value::<serde_yaml::Value>()? {
        serde_yaml::Value::String(s) => Ok(NameOr::Name(s)),
        serde_yaml::Value::Null => Ok(NameOr::Null),
        v @ serde_yaml::Value::Mapping(_) => serde_yaml::from_value(v)
            .map(NameOr::Full)
            .map_err(|e| de::Error::custom(format!("{what}: {e}"))),
        other => Err(de::Error::custom(format!(
            "{what}: expected a string or a map, got {}",
            yaml_kind(&other)
        ))),
    }
}

/// An action that takes nothing: `goBack:`, `goBack: {}` or a bare
/// `- goBack`. Anything else was accepted and ignored (VEL-208).
fn no_args<'de, A: MapAccess<'de>>(map: &mut A, key: &str) -> std::result::Result<(), A::Error> {
    match map.next_value::<serde_yaml::Value>()? {
        serde_yaml::Value::Null => Ok(()),
        serde_yaml::Value::Mapping(m) if m.is_empty() => Ok(()),
        other => Err(de::Error::custom(format!(
            "`{key}` takes no arguments, got {} (write `- {key}`)",
            yaml_kind(&other)
        ))),
    }
}

/// "a number", "a list": what a YAML value is, for errors.
fn yaml_kind(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "nothing",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a list",
        serde_yaml::Value::Mapping(_) => "a map",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

fn parse_action<'de, A>(key: &str, map: &mut A) -> std::result::Result<Action, A::Error>
where
    A: MapAccess<'de>,
{
    match key {
        "launchApp" => {
            let v: LaunchAppDsl = map.next_value()?;
            let arguments = v
                .arguments
                .unwrap_or_default()
                .into_iter()
                .map(|(name, value)| {
                    validate_launch_arg_name(&name).map_err(|msg| {
                        de::Error::custom(format!("launchApp.arguments.{name}: {msg}"))
                    })?;
                    Ok((name, value))
                })
                .collect::<std::result::Result<_, _>>()?;
            Ok(Action::LaunchApp {
                app_id: v.app_id.unwrap_or_default(),
                clear_state: v.clear_state.unwrap_or(false),
                arguments: LaunchArgs { inner: arguments },
            })
        }
        "stopApp" => {
            let v: StopAppDsl = map.next_value()?;
            Ok(Action::StopApp {
                app_id: v.app_id.unwrap_or_default(),
            })
        }
        "setConfig" => {
            let m: std::collections::BTreeMap<String, serde_yaml::Value> = map.next_value()?;
            let mut values = std::collections::BTreeMap::new();
            for (k, v) in m {
                let v = match v {
                    serde_yaml::Value::Bool(b) => (if b { "1" } else { "0" }).to_string(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::String(s) => s,
                    other => {
                        return Err(de::Error::custom(format!(
                            "setConfig.{k}: expected a scalar, got {other:?}"
                        )))
                    }
                };
                values.insert(k, v);
            }
            Ok(Action::SetConfig { values })
        }
        "mockRequest" => {
            let rule: velocity_common::MockRule = map.next_value()?;
            Ok(Action::MockRequest { rule })
        }
        "clearMocks" => {
            no_args(map, key)?;
            Ok(Action::ClearMocks)
        }
        "http" => {
            let req: velocity_common::HttpRequest = map.next_value()?;
            Ok(Action::Http(Box::new(req)))
        }
        "exec" => {
            let e: velocity_common::exec::ExecStep = map.next_value()?;
            Ok(Action::Exec(Box::new(e)))
        }
        "assertSystemState" => {
            let v: SystemStateDsl = map.next_value()?;
            Ok(Action::AssertSystemState {
                media_playing: v.media_playing,
                media_title: v.media_title,
                in_call: v.in_call,
            })
        }
        "tap" => {
            let v: SelectorDsl = map.next_value()?;
            Ok(Action::Tap {
                selector: v.into_selector().map_err(de::Error::custom)?,
            })
        }
        "doubleTap" => {
            let v: SelectorDsl = map.next_value()?;
            Ok(Action::DoubleTap {
                selector: v.into_selector().map_err(de::Error::custom)?,
            })
        }
        "drag" => {
            let v: DragDsl = map.next_value()?;
            Ok(Action::Drag {
                from: v.from.into_selector().map_err(de::Error::custom)?,
                to: v.to.into_selector().map_err(de::Error::custom)?,
                hold_ms: v.hold_ms,
                move_ms: v.move_ms,
            })
        }
        "longPress" => {
            let v: LongPressDsl = map.next_value()?;
            let duration_ms = v.duration_ms;
            Ok(Action::LongPress {
                selector: v.into_selector().map_err(de::Error::custom)?,
                duration_ms,
            })
        }
        "inputText" => {
            let v: InputTextDsl = map.next_value()?;
            let selector = v.selector.into_selector().map_err(de::Error::custom)?;
            Ok(Action::InputText {
                selector,
                text: v.text,
            })
        }
        "clearText" => {
            let v: SelectorDsl = map.next_value()?;
            Ok(Action::ClearText {
                selector: v.into_selector().map_err(de::Error::custom)?,
            })
        }
        "assertVisible" => {
            let v: SelectorDsl = map.next_value()?;
            Ok(Action::AssertVisible {
                selector: v.into_selector().map_err(de::Error::custom)?,
            })
        }
        "assertNotVisible" => {
            let v: SelectorDsl = map.next_value()?;
            Ok(Action::AssertNotVisible {
                selector: v.into_selector().map_err(de::Error::custom)?,
            })
        }
        "assertText" => {
            let v: AssertTextDsl = map.next_value()?;
            let selector = v.selector.into_selector().map_err(de::Error::custom)?;
            Ok(Action::AssertText {
                selector,
                expected: v.expected,
            })
        }
        "scrollUntilVisible" => {
            let v: ScrollDsl = map.next_value()?;
            let selector = v.selector.into_selector().map_err(de::Error::custom)?;
            let within = v
                .within
                .map(|s| s.into_selector())
                .transpose()
                .map_err(de::Error::custom)?;
            Ok(Action::ScrollUntilVisible {
                selector,
                direction: v.direction.unwrap_or(Direction::Down),
                max_scrolls: v.max_scrolls.unwrap_or(5),
                within,
            })
        }
        "swipe" => {
            let v: SwipeDsl = map.next_value()?;
            let within = v
                .within
                .map(|s| s.into_selector())
                .transpose()
                .map_err(de::Error::custom)?;
            Ok(Action::Swipe {
                direction: v.direction,
                from: v.from.map(|c| (c.x, c.y)),
                to: v.to.map(|c| (c.x, c.y)),
                within,
            })
        }
        "screenshot" => {
            // `screenshot: name` (as Maestro's `takeScreenshot: name`) or
            // `screenshot: { filename: name }`; `.png` is added when missing.
            let filename = match name_or::<A, ScreenshotDsl>(map, key)? {
                NameOr::Name(n) => Some(n),
                NameOr::Full(v) => v.filename,
                NameOr::Null => None,
            }
            .map(|f| {
                if f.contains('.') {
                    f
                } else {
                    format!("{f}.png")
                }
            });
            Ok(Action::Screenshot { filename })
        }
        "assertScreenshot" => Ok(match name_or::<A, AssertScreenshotDsl>(map, key)? {
            NameOr::Null => {
                return Err(de::Error::custom(
                    "assertScreenshot: expected a baseline name",
                ))
            }
            NameOr::Name(name) => Action::AssertScreenshot {
                name,
                threshold: None,
                cluster_px: None,
                mask: Vec::new(),
                tolerance: None,
                comparator: None,
                crop: None,
            },
            NameOr::Full(v) => Action::AssertScreenshot {
                name: v.name,
                threshold: v.threshold,
                cluster_px: v.cluster_px,
                mask: v
                    .mask
                    .into_iter()
                    .map(SelectorDsl::into_selector)
                    .collect::<std::result::Result<_, _>>()
                    .map_err(de::Error::custom)?,
                tolerance: v.tolerance,
                comparator: v.comparator,
                crop: v
                    .crop
                    .map(screenshot_crop)
                    .transpose()
                    .map_err(|e| de::Error::custom(format!("assertScreenshot crop: {e}")))?,
            },
        }),
        "saveState" | "restoreState" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct StateDsl {
                name: String,
            }
            let name = match name_or::<A, StateDsl>(map, key)? {
                NameOr::Name(n) | NameOr::Full(StateDsl { name: n }) => n,
                NameOr::Null => String::new(),
            };
            if name.trim().is_empty() {
                return Err(de::Error::custom("state name must not be empty"));
            }
            Ok(if key == "saveState" {
                Action::SaveState { name }
            } else {
                Action::RestoreState { name }
            })
        }
        "pressKey" => {
            let v: PressKeyDsl = map.next_value()?;
            Ok(Action::PressKey { key: v.key })
        }
        "hideKeyboard" => {
            no_args(map, key)?;
            Ok(Action::HideKeyboard)
        }
        "wait" => {
            let v: WaitDsl = map.next_value()?;
            Ok(Action::Wait { ms: v.ms })
        }
        "runFlow" => {
            let v: RunFlowDsl = map.next_value()?;
            let (flow_id, env) = match v {
                RunFlowDsl::Shorthand(id) => (id, BTreeMap::new()),
                RunFlowDsl::Full(fields) => run_flow_env(fields).map_err(de::Error::custom)?,
            };
            Ok(Action::RunFlow { flow_id, env })
        }
        "tapXY" => {
            let v: TapXYDsl = map.next_value()?;
            Ok(Action::TapXY { x: v.x, y: v.y })
        }
        "inputRaw" => {
            let v: InputRawDsl = map.next_value()?;
            Ok(Action::InputRaw { text: v.text })
        }
        // `openLink` (Maestro's name) opens a deep link on mobile; on web
        // it's the same as `goto`.
        "goto" | "openLink" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct GotoDsl {
                #[serde(alias = "link")]
                url: String,
            }
            let url = match name_or::<A, GotoDsl>(map, key)? {
                NameOr::Name(u) | NameOr::Full(GotoDsl { url: u }) => u,
                NameOr::Null => return Err(de::Error::custom(format!("{key}: expected a url"))),
            };
            Ok(Action::Goto { url })
        }
        "goBack" => {
            no_args(map, key)?;
            Ok(Action::GoBack)
        }
        "refresh" => {
            no_args(map, key)?;
            Ok(Action::Refresh)
        }
        // Natural-language steps: `do:`/`act:` drive the UI, `assert:`/`expect:` check it.
        "do" | "act" => {
            let intent: String = map.next_value()?;
            Ok(Action::Natural {
                intent,
                kind: NaturalKind::Act,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
            })
        }
        "assert" | "expect" => {
            let intent: String = map.next_value()?;
            Ok(Action::Natural {
                intent,
                kind: NaturalKind::Assert,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
            })
        }
        other => Err(de::Error::custom(unknown_step_key(other))),
    }
}

// DSL helper structs for deserialization

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct SelectorDsl {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    text_contains: Option<String>,
    /// A regex the whole text, label or hint matches (D-11).
    #[serde(default)]
    text_matches: Option<String>,
    /// A regex the whole id matches (D-11).
    #[serde(default)]
    id_matches: Option<String>,
    #[serde(default)]
    accessibility_id: Option<String>,
    #[serde(default)]
    class_name: Option<String>,
    /// The element's role, by Playwright's role names (S1).
    #[serde(default)]
    role: Option<String>,
    /// The accessible name (S1), compared like `text:`.
    #[serde(default)]
    name: Option<String>,
    /// State filters (S2): narrow the other fields, never find alone.
    #[serde(default)]
    checked: Option<bool>,
    #[serde(default)]
    focused: Option<bool>,
    #[serde(default)]
    selected: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    index: Option<usize>,
    /// CSS selector (web only). E.g. `".btn-primary"` or `"#submit"`.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression (web only). E.g. `"//button[text()='Submit']"`.
    #[serde(default)]
    xpath: Option<String>,
    #[serde(default)]
    below: Option<Box<SelectorDsl>>,
    #[serde(default)]
    above: Option<Box<SelectorDsl>>,
    #[serde(default)]
    left_of: Option<Box<SelectorDsl>>,
    #[serde(default)]
    right_of: Option<Box<SelectorDsl>>,
    #[serde(default)]
    contains_child: Option<Box<SelectorDsl>>,
    #[serde(default)]
    child_of: Option<Box<SelectorDsl>>,
}

/// A selector written as the DSL's flat map (`{ role: button, name: Pay,
/// enabled: true }`), as YAML or JSON: what MCP tool calls may send too.
pub fn selector_from_value(v: serde_json::Value) -> std::result::Result<Selector, String> {
    let dsl: SelectorDsl = serde_json::from_value(v).map_err(|e| e.to_string())?;
    dsl.into_selector()
}

impl SelectorDsl {
    fn into_selector(self) -> std::result::Result<Selector, String> {
        // CSS and XPath selectors are web-only and stand alone: any other
        // field beside them was dropped, so `{ text: Pay, css: .cancel }`
        // matched any `.cancel` (VEL-194).
        let web = match (self.css, self.xpath) {
            (Some(_), Some(_)) => return Err("css and xpath can't be combined; use one".into()),
            (Some(q), None) => Some(("css", Selector::Css(q))),
            (None, Some(q)) => Some(("xpath", Selector::XPath(q))),
            (None, None) => None,
        };
        if let Some((kind, sel)) = web {
            let others: Vec<&str> = [
                ("id", self.id.is_some()),
                ("text", self.text.is_some()),
                ("textContains", self.text_contains.is_some()),
                ("textMatches", self.text_matches.is_some()),
                ("idMatches", self.id_matches.is_some()),
                ("accessibilityId", self.accessibility_id.is_some()),
                ("className", self.class_name.is_some()),
                ("role", self.role.is_some()),
                ("name", self.name.is_some()),
                ("checked", self.checked.is_some()),
                ("focused", self.focused.is_some()),
                ("selected", self.selected.is_some()),
                ("enabled", self.enabled.is_some()),
                ("index", self.index.is_some()),
                ("below", self.below.is_some()),
                ("above", self.above.is_some()),
                ("leftOf", self.left_of.is_some()),
                ("rightOf", self.right_of.is_some()),
                ("containsChild", self.contains_child.is_some()),
                ("childOf", self.child_of.is_some()),
            ]
            .into_iter()
            .filter_map(|(name, given)| given.then_some(name))
            .collect();
            if !others.is_empty() {
                return Err(format!(
                    "{kind} can't be combined with {}; put the whole condition in the {kind} expression",
                    others.join(", ")
                ));
            }
            return Ok(sel);
        }

        // Every field given must hold (it used to keep the first and drop
        // the rest silently: `{ text: "Pay", className: "Button" }` matched
        // any "Pay").
        let mut parts = Vec::new();
        if let Some(id) = self.id {
            parts.push(Selector::Id(id));
        }
        if let Some(text) = self.text {
            parts.push(Selector::Text(text));
        }
        if let Some(sub) = self.text_contains {
            parts.push(Selector::TextContains(sub));
        }
        if let Some(re) = self.text_matches {
            parts.push(Selector::TextMatches(re));
        }
        if let Some(re) = self.id_matches {
            parts.push(Selector::IdMatches(re));
        }
        if let Some(aid) = self.accessibility_id {
            parts.push(Selector::AccessibilityId(aid));
        }
        if let Some(cls) = self.class_name {
            parts.push(Selector::ClassName(cls));
        }
        if let Some(role) = self.role {
            parts.push(Selector::Role(role));
        }
        if let Some(name) = self.name {
            parts.push(Selector::Name(name));
        }
        // State filters narrow what the fields above find (S2).
        let filters: Vec<Selector> = [
            self.checked.map(Selector::Checked),
            self.focused.map(Selector::Focused),
            self.selected.map(Selector::Selected),
            self.enabled.map(Selector::Enabled),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() && !filters.is_empty() {
            return Err("checked, focused, selected and enabled narrow a selector; add one of: id, text, textContains, textMatches, idMatches, accessibilityId, className, role, name".to_string());
        }
        parts.extend(filters);
        let mut base = match parts.len() {
            0 => return Err("selector must have at least one of: id, text, textContains, textMatches, idMatches, accessibilityId, className, role, name, css, xpath".to_string()),
            1 => parts.remove(0),
            _ => Selector::Compound(parts),
        };
        for (relation, anchor) in [
            (velocity_common::Relation::Below, self.below),
            (velocity_common::Relation::Above, self.above),
            (velocity_common::Relation::LeftOf, self.left_of),
            (velocity_common::Relation::RightOf, self.right_of),
            (
                velocity_common::Relation::ContainsChild,
                self.contains_child,
            ),
            (velocity_common::Relation::ChildOf, self.child_of),
        ] {
            if let Some(anchor) = anchor {
                base = Selector::Relative {
                    target: Box::new(base),
                    relation,
                    anchor: Box::new(anchor.into_selector()?),
                };
            }
        }

        Ok(match self.index {
            Some(idx) => Selector::Index {
                selector: Box::new(base),
                index: idx,
            },
            None => base,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct LaunchAppDsl {
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    clear_state: Option<bool>,
    #[serde(default)]
    arguments: Option<BTreeMap<String, LaunchArg>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct SystemStateDsl {
    #[serde(default)]
    media_playing: Option<bool>,
    #[serde(default)]
    media_title: Option<String>,
    #[serde(default)]
    in_call: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct StopAppDsl {
    #[serde(default)]
    app_id: Option<String>,
}

/// `longPress: { selector: {…}, durationMs }`, or the selector's own
/// fields inline: `longPress: { text: Edit, below: {…}, durationMs }`.
/// Inline took only six fields, so `below:` or `css:` was "unknown field"
/// there and nowhere else (VEL-221).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DragDsl {
    from: SelectorDsl,
    to: SelectorDsl,
    hold_ms: Option<u64>,
    move_ms: Option<u64>,
}

struct LongPressDsl {
    selector: std::result::Result<SelectorDsl, String>,
    duration_ms: Option<u64>,
}

impl LongPressDsl {
    fn into_selector(self) -> std::result::Result<Selector, String> {
        self.selector?.into_selector()
    }
}

impl<'de> Deserialize<'de> for LongPressDsl {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let mut m = serde_yaml::Mapping::deserialize(d)?;
        let duration_ms = match m.remove("durationMs") {
            None => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| {
                de::Error::custom(format!(
                    "longPress.durationMs: expected milliseconds, got {}",
                    yaml_kind(&v)
                ))
            })?),
        };
        let selector = match m.remove("selector") {
            Some(_) if !m.is_empty() => Err(
                "longPress: give the selector either under `selector:` or inline, not both".into(),
            ),
            Some(sel) => {
                serde_yaml::from_value(sel).map_err(|e| format!("longPress.selector: {e}"))
            }
            None => serde_yaml::from_value(serde_yaml::Value::Mapping(m))
                .map_err(|e| format!("longPress: {e}")),
        };
        Ok(LongPressDsl {
            selector,
            duration_ms,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct InputTextDsl {
    selector: SelectorDsl,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct AssertTextDsl {
    selector: SelectorDsl,
    expected: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct ScrollDsl {
    selector: SelectorDsl,
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    max_scrolls: Option<u32>,
    #[serde(default)]
    within: Option<SelectorDsl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coords {
    x: i32,
    y: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct SwipeDsl {
    #[serde(default)]
    direction: Option<Direction>,
    #[serde(default)]
    from: Option<Coords>,
    #[serde(default)]
    to: Option<Coords>,
    #[serde(default)]
    within: Option<SelectorDsl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct ScreenshotDsl {
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct AssertScreenshotDsl {
    name: String,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    cluster_px: Option<usize>,
    #[serde(default)]
    mask: Vec<SelectorDsl>,
    #[serde(default)]
    tolerance: Option<u8>,
    #[serde(default)]
    comparator: Option<velocity_common::Comparator>,
    #[serde(default)]
    crop: Option<serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CropRectDsl {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// `crop:` is a rect (`{x, y, width, height}`) or a selector.
fn screenshot_crop(
    v: serde_yaml::Value,
) -> std::result::Result<velocity_common::ScreenshotCrop, String> {
    let rect_keys = ["x", "y", "width", "height"];
    let is_rect = v.as_mapping().is_some_and(|m| {
        m.keys()
            .any(|k| k.as_str().is_some_and(|k| rect_keys.contains(&k)))
    });
    if is_rect {
        let r: CropRectDsl = serde_yaml::from_value(v).map_err(|e| e.to_string())?;
        if r.width <= 0 || r.height <= 0 {
            return Err("a rect needs a positive width and height".into());
        }
        return Ok(velocity_common::ScreenshotCrop::Rect(
            velocity_common::Rect {
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
            },
        ));
    }
    let sel: SelectorDsl = serde_yaml::from_value(v).map_err(|e| e.to_string())?;
    sel.into_selector()
        .map(velocity_common::ScreenshotCrop::Element)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct PressKeyDsl {
    key: Key,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct WaitDsl {
    ms: u64,
}

/// `runFlow` accepts both the string shorthand (`runFlow: login`) and the
/// map form (`runFlow: { flow: login, env: { USER: "qa" } }`).
#[derive(Deserialize)]
#[serde(untagged)]
enum RunFlowDsl {
    Shorthand(String),
    Full(std::collections::BTreeMap<String, serde_yaml::Value>),
}

/// The flow a `runFlow: { flow: …, env: { … } }` map names. `env` are
/// parameters bound for that call only. Any other key is an error.
fn run_flow_env(
    fields: std::collections::BTreeMap<String, serde_yaml::Value>,
) -> std::result::Result<(String, std::collections::BTreeMap<String, String>), String> {
    let mut id = None;
    let mut env = std::collections::BTreeMap::new();
    for (key, value) in fields {
        match key.as_str() {
            "flowId" | "flow" => match value {
                serde_yaml::Value::String(v) => {
                    if id.is_some() {
                        return Err("runFlow: give flow or flowId, not both".into());
                    }
                    id = Some(v);
                }
                other => return Err(format!("runFlow.{key} must be a flow name, got {other:?}")),
            },
            "env" => match value {
                serde_yaml::Value::Mapping(m) => {
                    for (k, v) in m {
                        let name = match k {
                            serde_yaml::Value::String(s) => s,
                            other => {
                                return Err(format!(
                                    "runFlow.env key must be a string, got {other:?}"
                                ))
                            }
                        };
                        let val = match v {
                            serde_yaml::Value::String(s) => s,
                            serde_yaml::Value::Number(n) if n.is_i64() || n.is_u64() => {
                                n.to_string()
                            }
                            serde_yaml::Value::Number(n) => {
                                return Err(format!(
                                    "runFlow env {name}: write {n} in quotes (\"{n}\") so it is \
                                     typed exactly as written"
                                ))
                            }
                            serde_yaml::Value::Bool(b) => b.to_string(),
                            other => {
                                return Err(format!(
                                    "runFlow.env.{name} must be a string, got {other:?}"
                                ))
                            }
                        };
                        env.insert(name, val);
                    }
                }
                other => return Err(format!("runFlow.env must be a map, got {other:?}")),
            },
            "when" => {
                return Err(
                    "runFlow: `when:` isn't supported (the flow would always run); \
                     put the steps that depend on it in their own test"
                        .into(),
                )
            }
            other => {
                return Err(format!(
                    "runFlow: unknown field `{other}` (expected `flow` or `env`)"
                ))
            }
        }
    }
    id.ok_or_else(|| "runFlow: missing `flow`".to_string())
        .map(|id| (id, env))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TapXYDsl {
    x: i32,
    y: i32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRawDsl {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3: `web.parallelMode` takes `contexts` or `processes`; anything
    /// else is a parse error, not a silent default.
    #[test]
    fn web_parallel_mode_parses() {
        let suite = |mode: &str| {
            format!(
                "appId: x\nconfig:\n  platform: web\n  web:\n    parallelMode: {mode}\ntests:\n  - name: t\n    steps:\n      - tap: {{ id: a }}\n"
            )
        };
        use velocity_common::test_types::WebParallelMode;
        let mode = |m: &str| {
            parse_suite_from_str(&suite(m))
                .unwrap()
                .config
                .web
                .unwrap()
                .parallel_mode
        };
        assert_eq!(mode("processes"), Some(WebParallelMode::Processes));
        assert_eq!(mode("contexts"), Some(WebParallelMode::Contexts));
        assert!(parse_suite_from_str(&suite("threads")).is_err());
    }

    #[test]
    fn step_settle_and_set_config_parse() {
        let y = r#"
appId: x
tests:
  - name: t
    steps:
      - tap: { id: "a" }
        settle: none
      - tap: { id: "b" }
        settle: { quietMs: 40, capMs: 300 }
      - tap: { id: "c" }
        settle: auto
      - setConfig: { android.scroll: semantic, ios.fullSource: true, sync.timeoutMs: 4000 }
"#;
        let suite = parse_suite_from_str(y).unwrap();
        let steps = &suite.tests[0].steps;
        assert!(steps[0].settle.as_ref().unwrap().skip);
        let s1 = steps[1].settle.as_ref().unwrap();
        assert_eq!(
            (s1.skip, s1.quiet_ms, s1.cap_ms, s1.reaction_ms),
            (false, Some(40), Some(300), None)
        );
        assert!(steps[2].settle.is_none());
        match &steps[3].action {
            Action::SetConfig { values } => {
                assert_eq!(values["android.scroll"], "semantic");
                assert_eq!(values["ios.fullSource"], "1");
                assert_eq!(values["sync.timeoutMs"], "4000");
            }
            other => panic!("{other:?}"),
        }
        let bad = y.replace("settle: none", "settle: sometimes");
        assert!(parse_suite_from_str(&bad).is_err());
    }

    fn minimal_suite() -> &'static str {
        r#"
appId: com.example.app
tests:
  - name: "basic test"
    steps:
      - tap: { id: "login_button" }
      - inputText: { selector: { id: "email_field" }, text: "user@example.com" }
      - assertVisible: { text: "Welcome" }
      - wait: { ms: 500 }
"#
    }

    #[test]
    fn parse_run_flow_shorthand_and_map_forms() {
        let yaml = r#"
appId: com.example.app
flows:
  - id: login
    steps:
      - tap: { id: "login_button" }
tests:
  - name: "shorthand"
    steps:
      - runFlow: login
  - name: "map form"
    steps:
      - runFlow: { flowId: "login" }
"#;
        let suite = parse_suite_from_str(yaml).expect("both runFlow forms must parse");
        for test in &suite.tests {
            match &test.steps[0].action {
                Action::RunFlow { flow_id, env } => {
                    assert_eq!(flow_id, "login");
                    assert!(env.is_empty());
                }
                other => panic!("expected RunFlow, got {other:?}"),
            }
        }
    }

    #[test]
    fn run_flow_rejects_keys_it_would_ignore() {
        // VEL-070: `when:` was dropped and the flow always ran.
        let suite = |step: &str| {
            format!(
                "appId: a\nflows:\n  - id: login\n    steps:\n      - tap: {{ id: b }}\ntests:\n  - name: t\n    steps:\n      - {step}\n"
            )
        };
        let err = parse_suite_from_str(&suite(
            "runFlow: { flowId: login, when: { visible: { text: Login } } }",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("`when:` isn't supported"), "{err}");
        let err = parse_suite_from_str(&suite("runFlow: { flowid: login }"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `flowid`"), "{err}");
        assert!(parse_suite_from_str(&suite("runFlow: { flow: login }")).is_ok());
    }

    #[test]
    fn run_flow_env_refuses_a_non_integer_number_and_both_flow_keys() {
        let suite = |step: &str| {
            format!(
                "appId: a\nflows:\n  - id: login\n    steps:\n      - tap: {{ id: b }}\ntests:\n  - name: t\n    steps:\n      - {step}\n"
            )
        };
        let err = parse_suite_from_str(&suite("runFlow: { flow: login, env: { AMOUNT: 1.50 } }"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("AMOUNT") && err.contains("in quotes"), "{err}");
        let ok = parse_suite_from_str(&suite(
            "runFlow: { flow: login, env: { A: \"1.50\", N: 42, B: true, PIN: 0042 } }",
        ))
        .unwrap();
        match &ok.tests[0].steps[0].action {
            Action::RunFlow { env, .. } => {
                assert_eq!(env["A"], "1.50");
                assert_eq!(env["N"], "42");
                assert_eq!(env["B"], "true");
                // serde_yaml reads a leading-zero scalar as a string, so it survives.
                assert_eq!(env["PIN"], "0042");
            }
            other => panic!("expected RunFlow, got {other:?}"),
        }
        let err = parse_suite_from_str(&suite("runFlow: { flow: login, flowId: login }"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("give flow or flowId, not both"), "{err}");
    }

    #[test]
    fn run_flow_map_form_carries_env() {
        let yaml = r#"
appId: com.test
flows:
  - id: login
    steps:
      - tap: { id: "btn" }
tests:
  - name: t
    steps:
      - runFlow: { flow: login, env: { USER: qa@example.com, PIN: "1234" } }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::RunFlow { flow_id, env } => {
                assert_eq!(flow_id, "login");
                assert_eq!(env.get("USER"), Some(&"qa@example.com".to_string()));
                assert_eq!(env.get("PIN"), Some(&"1234".to_string()));
            }
            other => panic!("expected RunFlow, got {:?}", other),
        }
    }

    #[test]
    fn a_byte_order_mark_and_a_missing_file_are_handled() {
        // VEL-198: a BOM made a valid suite fail; VEL-200: which file?
        let yaml = format!("\u{feff}{}", minimal_suite());
        assert_eq!(
            parse_suite_from_str(&yaml).unwrap().app_id,
            "com.example.app"
        );
        let err = parse_suite("no/such/suite.yaml").unwrap_err().to_string();
        assert!(err.contains("no/such/suite.yaml"), "{err}");
    }

    #[test]
    fn parse_minimal_suite() {
        let suite = parse_suite_from_str(minimal_suite()).unwrap();
        assert_eq!(suite.app_id, "com.example.app");
        assert_eq!(suite.tests.len(), 1);
        assert_eq!(suite.tests[0].steps.len(), 4);
    }

    #[test]
    fn parse_tap_selector() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - tap: { id: "btn" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::Tap { selector } => {
                assert_eq!(*selector, Selector::Id("btn".to_string()));
            }
            other => panic!("expected Tap, got {:?}", other),
        }
    }

    #[test]
    fn parse_text_contains_selector() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - assertVisible: { textContains: "partial" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::AssertVisible { selector } => {
                assert_eq!(*selector, Selector::TextContains("partial".to_string()));
            }
            other => panic!("expected AssertVisible, got {:?}", other),
        }
    }

    #[test]
    fn parse_indexed_selector() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - tap: { className: "Button", index: 2 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::Tap { selector } => {
                assert_eq!(
                    *selector,
                    Selector::Index {
                        selector: Box::new(Selector::ClassName("Button".to_string())),
                        index: 2,
                    }
                );
            }
            other => panic!("expected Tap, got {:?}", other),
        }
    }

    #[test]
    fn parse_input_text() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - inputText: { selector: { id: "field" }, text: "hello" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::InputText { selector, text } => {
                assert_eq!(*selector, Selector::Id("field".to_string()));
                assert_eq!(text, "hello");
            }
            other => panic!("expected InputText, got {:?}", other),
        }
    }

    #[test]
    fn launch_app_arguments_keep_their_types() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - launchApp:
          arguments:
            isE2E: true
            retries: 3
            ratio: 1.5
            server: "https://staging.example.com"
            pin: "0042"
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        let Action::LaunchApp { arguments, .. } = &suite.tests[0].steps[0].action else {
            panic!("expected LaunchApp");
        };
        let a = &arguments.inner;
        assert_eq!(a["isE2E"], LaunchArg::Bool(true));
        assert_eq!(a["retries"], LaunchArg::Int(3));
        assert_eq!(a["ratio"], LaunchArg::Float(1.5));
        assert_eq!(
            a["server"],
            LaunchArg::Str("https://staging.example.com".into())
        );
        assert_eq!(
            a["pin"],
            LaunchArg::Str("0042".into()),
            "a quoted number stays text"
        );
    }

    #[test]
    fn a_launch_argument_name_starting_with_a_dash_is_refused() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - launchApp:
          arguments:
            "-flag": true
"#;
        let e = parse_suite_from_str(yaml).unwrap_err().to_string();
        assert!(e.contains("must not start with '-'"), "{e}");
    }

    #[test]
    fn parse_launch_stop_app() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - launchApp: { appId: "com.other", clearState: true }
      - stopApp: { appId: "com.other" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::LaunchApp {
                app_id,
                clear_state,
                ..
            } => {
                assert_eq!(app_id, "com.other");
                assert!(*clear_state);
            }
            other => panic!("expected LaunchApp, got {:?}", other),
        }
        match &suite.tests[0].steps[1].action {
            Action::StopApp { app_id } => {
                assert_eq!(app_id, "com.other");
            }
            other => panic!("expected StopApp, got {:?}", other),
        }
    }

    #[test]
    fn parse_run_flow() {
        let yaml = r#"
appId: com.test
flows:
  - id: login
    steps:
      - tap: { id: "btn" }
tests:
  - name: t
    steps:
      - runFlow: { flowId: "login" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.flows.len(), 1);
        match &suite.tests[0].steps[0].action {
            Action::RunFlow { flow_id, env } => {
                assert_eq!(flow_id, "login");
                assert!(env.is_empty());
            }
            other => panic!("expected RunFlow, got {:?}", other),
        }
    }

    #[test]
    fn parse_scroll_until_visible() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - scrollUntilVisible: { selector: { text: "Footer" }, direction: up, maxScrolls: 10 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::ScrollUntilVisible {
                selector,
                direction,
                max_scrolls,
                ..
            } => {
                assert_eq!(*selector, Selector::Text("Footer".to_string()));
                assert_eq!(*direction, Direction::Up);
                assert_eq!(*max_scrolls, 10);
            }
            other => panic!("expected ScrollUntilVisible, got {:?}", other),
        }
    }

    #[test]
    fn parse_swipe_direction() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - swipe: { direction: left }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::Swipe {
                direction,
                from,
                to,
                ..
            } => {
                assert_eq!(*direction, Some(Direction::Left));
                assert!(from.is_none());
                assert!(to.is_none());
            }
            other => panic!("expected Swipe, got {:?}", other),
        }
    }

    #[test]
    fn parse_assert_text() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - assertText: { selector: { id: "title" }, expected: "Hello World" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::AssertText { selector, expected } => {
                assert_eq!(*selector, Selector::Id("title".to_string()));
                assert_eq!(expected, "Hello World");
            }
            other => panic!("expected AssertText, got {:?}", other),
        }
    }

    #[test]
    fn parse_press_key_and_screenshot() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - pressKey: { key: back }
      - screenshot: { filename: "after_back.png" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::PressKey { key } => assert_eq!(*key, Key::Back),
            other => panic!("expected PressKey, got {:?}", other),
        }
        match &suite.tests[0].steps[1].action {
            Action::Screenshot { filename } => {
                assert_eq!(filename.as_deref(), Some("after_back.png"));
            }
            other => panic!("expected Screenshot, got {:?}", other),
        }
    }

    #[test]
    fn parse_step_with_timeout() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - tap: { id: "btn" }
        timeoutMs: 5000
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.tests[0].steps[0].timeout_ms, Some(5000));
    }

    #[test]
    fn parse_config() {
        let yaml = r#"
appId: com.test
config:
  platform: ios
  sync:
    intervalMs: 100
    stabilityCount: 5
    timeoutMs: 20000
tests:
  - name: t
    steps:
      - wait: { ms: 100 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.config.sync.interval_ms, 100);
        assert_eq!(suite.config.sync.stability_count, 5);
    }

    #[test]
    fn parse_unknown_action_fails() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - unknownAction: { id: "btn" }
"#;
        let result = parse_suite_from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_selector_fails() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - tap: {}
"#;
        let result = parse_suite_from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn parse_tags() {
        let yaml = r#"
appId: com.test
tests:
  - name: tagged test
    tags: [smoke, login]
    steps:
      - wait: { ms: 100 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.tests[0].tags, vec!["smoke", "login"]);
    }

    #[test]
    fn parse_long_press_with_nested_selector() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - longPress: { selector: { id: "item" }, durationMs: 1500 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::LongPress {
                selector,
                duration_ms,
            } => {
                assert_eq!(*selector, Selector::Id("item".to_string()));
                assert_eq!(*duration_ms, Some(1500));
            }
            other => panic!("expected LongPress, got {:?}", other),
        }
    }

    #[test]
    fn parse_double_tap() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - doubleTap: { accessibilityId: "heart" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::DoubleTap { selector } => {
                assert_eq!(*selector, Selector::AccessibilityId("heart".to_string()));
            }
            other => panic!("expected DoubleTap, got {:?}", other),
        }
    }

    #[test]
    fn parse_clear_text() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - clearText: { id: "email" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::ClearText { selector } => {
                assert_eq!(*selector, Selector::Id("email".to_string()));
            }
            other => panic!("expected ClearText, got {:?}", other),
        }
    }

    #[test]
    fn bare_action_names_are_actions_not_natural_language() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - launchApp
      - back
      - goBack
      - stopApp
      - screenshot
      - screenshot: home
      - screenshot: { filename: about }
      - "Tap the login button"
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        let a: Vec<&Action> = suite.tests[0].steps.iter().map(|s| &s.action).collect();
        assert!(matches!(
            a[0],
            Action::LaunchApp {
                clear_state: false,
                ..
            }
        ));
        assert!(matches!(
            a[1],
            Action::PressKey {
                key: velocity_common::Key::Back
            }
        ));
        assert!(matches!(a[2], Action::GoBack));
        assert!(matches!(a[3], Action::StopApp { .. }));
        assert!(matches!(a[4], Action::Screenshot { filename: None }));
        assert_eq!(
            a[5],
            &Action::Screenshot {
                filename: Some("home.png".into())
            }
        );
        assert_eq!(
            a[6],
            &Action::Screenshot {
                filename: Some("about.png".into())
            }
        );
        assert!(
            matches!(a[7], Action::Natural { .. }),
            "a sentence stays natural language"
        );
    }

    #[test]
    fn parse_natural_bare_string_step() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - "Tap the login button"
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::Natural {
                intent,
                kind,
                data,
                data_name: None,
                wait_until,
                intent_template: None,
            } => {
                assert_eq!(intent, "Tap the login button");
                assert_eq!(*kind, NaturalKind::Act);
                assert!(data.is_none());
                assert!(wait_until.is_none());
            }
            other => panic!("expected Natural, got {:?}", other),
        }
    }

    #[test]
    fn parse_natural_do_with_wait_until_and_data() {
        // passmark-style: a `do:` action with a `waitUntil` condition and `data`.
        let yaml = r#"
appId: com.test
tests:
  - name: cart
    steps:
      - do: "Add product to cart"
        waitUntil: "My Cart is visible"
      - do: "Select color"
        data: { value: "White" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        match &suite.tests[0].steps[0].action {
            Action::Natural {
                intent,
                kind,
                wait_until,
                ..
            } => {
                assert_eq!(intent, "Add product to cart");
                assert_eq!(*kind, NaturalKind::Act);
                assert_eq!(wait_until.as_deref(), Some("My Cart is visible"));
            }
            other => panic!("expected Natural, got {:?}", other),
        }
        match &suite.tests[0].steps[1].action {
            Action::Natural { data, .. } => assert_eq!(data.as_deref(), Some("White")),
            other => panic!("expected Natural, got {:?}", other),
        }
    }

    #[test]
    fn parse_natural_assert_and_expect() {
        let yaml = r#"
appId: com.test
tests:
  - name: t
    steps:
      - assert: "You can see My Cart with the T-Shirt"
      - expect: "Order total is correct"
      - data: "plain string form"
        do: "Enter the coupon"
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        assert!(matches!(
            &suite.tests[0].steps[0].action,
            Action::Natural {
                kind: NaturalKind::Assert,
                ..
            }
        ));
        assert!(matches!(
            &suite.tests[0].steps[1].action,
            Action::Natural {
                kind: NaturalKind::Assert,
                ..
            }
        ));
        // `data:` given as a plain string (not `{ value: ... }`) also works.
        match &suite.tests[0].steps[2].action {
            Action::Natural { data, kind, .. } => {
                assert_eq!(*kind, NaturalKind::Act);
                assert_eq!(data.as_deref(), Some("plain string form"));
            }
            other => panic!("expected Natural, got {:?}", other),
        }
    }

    #[test]
    fn unknown_keys_in_an_action_are_errors_with_a_hint() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - assertVisible: { text: "Done", timeoutMs: 500 }
"#;
        let err = parse_suite_from_str(yaml).unwrap_err().to_string();
        assert!(err.contains("unknown field `timeoutMs`"), "{err}");
        assert!(err.contains("belongs to the step"), "{err}");

        let typo = r#"
appId: com.example
tests:
  - name: t
    steps:
      - tap: { txet: "Go" }
"#;
        let err = parse_suite_from_str(typo).unwrap_err().to_string();
        assert!(err.contains("unknown field `txet`"), "{err}");
    }

    #[test]
    fn open_link_parses_in_both_forms() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - openLink: "loyalty://rewards"
      - openLink: { link: "https://example.com/r/1" }
      - goto: { url: "https://example.com" }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        let urls: Vec<String> = suite.tests[0]
            .steps
            .iter()
            .map(|s| match &s.action {
                Action::Goto { url } => url.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            urls,
            [
                "loyalty://rewards",
                "https://example.com/r/1",
                "https://example.com"
            ]
        );
    }

    #[test]
    fn selectors_with_several_fields_and_relations() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - tap: { text: "Pay", className: "Button" }
      - tap: { text: "Edit", below: { text: "Shipping" } }
      - tap: { text: "Add", childOf: { className: "Cell", containsChild: { text: "Coffee" } }, index: 0 }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        let sel = |i: usize| match &suite.tests[0].steps[i].action {
            Action::Tap { selector } => selector.to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(sel(0), r#"compound(text="Pay" AND className="Button")"#);
        assert_eq!(sel(1), r#"text="Edit" below(text="Shipping")"#);
        assert_eq!(
            sel(2),
            r#"text="Add" childOf(className="Cell" containsChild(text="Coffee"))[0]"#
        );
    }

    /// S1/S2: `role:` + `name:` and the state filters parse into one
    /// compound, in every place a selector goes (inline longPress, a
    /// relation's anchor); a state alone, or beside css, is refused.
    #[test]
    fn role_name_and_state_keys_parse() {
        let yaml = r#"
appId: com.example
tests:
  - name: t
    steps:
      - tap: { role: button, name: "Pay" }
      - assertVisible: { text: "Remember me", checked: true, enabled: true }
      - tap: { role: tab, selected: false, index: 1 }
      - longPress: { role: listitem, name: "Coffee", durationMs: 800 }
      - tap: { role: button, rightOf: { role: textbox, focused: true } }
"#;
        let suite = parse_suite_from_str(yaml).unwrap();
        let sel = |i: usize| match &suite.tests[0].steps[i].action {
            Action::Tap { selector }
            | Action::AssertVisible { selector }
            | Action::LongPress { selector, .. } => selector.to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(sel(0), r#"compound(role="button" AND name="Pay")"#);
        assert_eq!(
            sel(1),
            r#"compound(text="Remember me" AND checked=true AND enabled=true)"#
        );
        assert_eq!(sel(2), r#"compound(role="tab" AND selected=false)[1]"#);
        assert_eq!(sel(3), r#"compound(role="listitem" AND name="Coffee")"#);
        assert_eq!(
            sel(4),
            r#"role="button" rightOf(compound(role="textbox" AND focused=true))"#
        );
        for bad in [
            "{ checked: true }",
            "{ enabled: false, index: 0 }",
            "{ css: \".x\", checked: true }",
            "{ text: x, checked: maybe }",
        ] {
            let yaml = format!("appId: a\ntests:\n  - name: t\n    steps:\n      - tap: {bad}\n");
            assert!(parse_suite_from_str(&yaml).is_err(), "{bad}");
        }
    }

    #[test]
    fn steps_carry_their_line_and_flow_steps_keep_the_flow_line_when_inlined() {
        let yaml = r#"appId: com.example
flows:
  - id: login
    steps:
      - inputText: { selector: { id: "email" }, text: "a@b.c" }
      - tap: { text: "Log in" }
tests:
  - name: checkout
    steps:
      - launchApp
      - runFlow: login
      - tap: { text: "Pay" }
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.yaml");
        std::fs::write(&path, yaml).unwrap();
        let path = path.to_string_lossy().into_owned();
        let (suite, tests) = parse_suite_with_positions(&path).unwrap();
        assert_eq!(tests["checkout"], SourcePos::new(path.as_str(), 8, 5));
        let lines: Vec<usize> = suite.tests[0]
            .steps
            .iter()
            .map(|s| s.source.as_ref().unwrap().line)
            .collect();
        assert_eq!(lines, vec![10, 11, 12]);
        assert_eq!(suite.tests[0].steps[2].source.as_ref().unwrap().file, path);

        let inlined = crate::resolve_flows(&suite).unwrap();
        let lines: Vec<usize> = inlined[0]
            .steps
            .iter()
            .map(|s| s.source.as_ref().unwrap().line)
            .collect();
        assert_eq!(lines, vec![10, 5, 6, 12]);
    }

    #[test]
    fn parse_errors_point_at_file_line_and_column() {
        let yaml = "appId: x\ntests:\n  - name: t\n    steps:\n      - launchApp\n      - tapp: { text: \"Pay\" }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, yaml).unwrap();
        let path = path.to_string_lossy().into_owned();
        let err = parse_suite(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with(&format!("{path}:6:")),
            "expected file:line first, got {msg}"
        );
        assert!(msg.contains("tapp"), "{msg}");
        assert!(
            !msg.contains(" at line "),
            "location should not repeat: {msg}"
        );
    }

    #[test]
    fn regex_selectors_parse_and_an_invalid_pattern_is_a_validation_error() {
        // D-11.
        let yaml = "appId: x\ntests:\n  - name: t\n    steps:\n      - tap: { textMatches: \"Item \\\\d+\", below: { idMatches: \"row_.*\" } }\n      - assertVisible: { textMatches: \"Hello ${NAME}\" }\n";
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(
            suite.tests[0].steps[0].action,
            Action::Tap {
                selector: Selector::Relative {
                    target: Box::new(Selector::TextMatches(r"Item \d+".into())),
                    relation: velocity_common::Relation::Below,
                    anchor: Box::new(Selector::IdMatches("row_.*".into())),
                }
            }
        );
        crate::validate_suite(&suite).unwrap();
        let bad =
            "appId: x\ntests:\n  - name: t\n    steps:\n      - tap: { textMatches: \"[oops\" }\n";
        let err = crate::validate_suite(&parse_suite_from_str(bad).unwrap()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains("textMatches \"[oops\" is not a valid regular expression"),
            "{err}"
        );
        let css =
            "appId: x\ntests:\n  - name: t\n    steps:\n      - tap: { css: a, textMatches: b }\n";
        assert!(parse_suite_from_str(css).is_err());
    }

    #[test]
    fn validation_errors_name_the_step_and_its_line() {
        let yaml = "appId: x\ntests:\n  - name: t\n    steps:\n      - launchApp\n      - tap: { text: \"  \" }\n";
        let suite = parse_suite_from_str(yaml).unwrap();
        let err = crate::validate_suite(&suite).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.to_string(),
            "line 6: tap { text: \"  \" }: Invalid selector in step 1 of 't': selector value is empty"
        );
    }

    #[test]
    fn stamp_sources_fills_test_lines_into_results() {
        let mut tests = HashMap::new();
        tests.insert("a".to_string(), SourcePos::new("s.yaml", 3, 5));
        let mut result = SuiteResult {
            total: 2,
            passed: 0,
            failed: 2,
            skipped: 0,
            retried: 0,
            duration: std::time::Duration::ZERO,
            tests: vec![
                velocity_common::TestResult::failed("a", "x".into(), Default::default()),
                velocity_common::TestResult::failed("b", "x".into(), Default::default()),
            ],
            shard_index: None,
            shard_total: None,
        };
        stamp_sources(&mut result, &tests);
        assert_eq!(
            result.tests[0].source.as_ref().unwrap().to_string(),
            "s.yaml:3"
        );
        assert!(result.tests[1].source.is_none());
    }

    #[test]
    fn driver_and_framework_typos_fail_at_parse_time() {
        let yaml = |extra: &str| {
            format!("appId: a\nconfig:\n  {extra}\ntests:\n  - name: t\n    steps:\n      - back\n")
        };
        let ok = parse_suite_from_str(&yaml("framework: rn")).unwrap();
        assert_eq!(
            ok.config.framework,
            Some(velocity_common::config::Framework::ReactNative)
        );
        assert!(parse_suite_from_str(&yaml("driver: headles")).is_err());
        assert!(parse_suite_from_str(&yaml("framework: swiftui")).is_err());
    }

    /// One step under a test, parsed; the error text when it fails.
    fn one_step(step: &str) -> std::result::Result<Action, String> {
        let yaml = format!("appId: a\ntests:\n  - name: t\n    steps:\n      - {step}\n");
        parse_suite_from_str(&yaml)
            .map(|s| s.tests[0].steps[0].action.clone())
            .map_err(|e| e.to_string())
    }

    fn one_full_step(step: &str) -> std::result::Result<Step, String> {
        let yaml = format!("appId: a\ntests:\n  - name: t\n    steps:\n      - {step}\n");
        parse_suite_from_str(&yaml)
            .map(|s| s.tests[0].steps[0].clone())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn checkpoint_takes_a_claim_or_true_on_natural_steps() {
        let s = one_full_step("assert: \"the cart is open\"\n        checkpoint: true").unwrap();
        assert_eq!(s.modifiers.checkpoint.as_deref(), Some(""));
        let s =
            one_full_step("tap: { text: Buy }\n        checkpoint: \"the cart is open\"").unwrap();
        assert_eq!(s.modifiers.checkpoint.as_deref(), Some("the cart is open"));
        let s = one_full_step("do: \"tap Buy\"\n        checkpoint: false").unwrap();
        assert_eq!(s.modifiers.checkpoint, None);
        let e = one_full_step("tap: { text: Buy }\n        checkpoint: true").unwrap_err();
        assert!(e.contains("give the claim"), "{e}");
        let e = one_full_step("tap: { text: Buy }\n        checkpoint: 3").unwrap_err();
        assert!(e.contains("`checkpoint` takes"), "{e}");
    }

    #[test]
    fn verify_description_and_ai_config_parse() {
        use velocity_common::VerifyLevel;
        let yaml = "appId: a\nconfig:\n  verify: checkpoints\n  ai:\n    provider: openai\n    model: gpt-5-mini\n    baseUrl: http://localhost:11434/v1\n    apiKeyEnv: OLLAMA_KEY\n    images: false\ntests:\n  - name: t\n    description: the cart shows 2 items\n    verify: strict\n    steps:\n      - tap: { text: Buy }\n";
        let suite = parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.config.verify, Some(VerifyLevel::Checkpoints));
        let ai = suite.config.ai.as_ref().unwrap();
        assert_eq!(ai.provider.as_deref(), Some("openai"));
        assert_eq!(ai.api_key_env.as_deref(), Some("OLLAMA_KEY"));
        assert_eq!(ai.images, Some(false));
        assert_eq!(suite.tests[0].verify, Some(VerifyLevel::Strict));
        assert_eq!(
            suite.tests[0].description.as_deref(),
            Some("the cart shows 2 items")
        );
        assert!(crate::config_keys::unknown_keys(yaml).is_empty());
        // A bad level is an error, an unknown ai key is named.
        let bad = yaml.replace("verify: strict", "verify: always");
        assert!(parse_suite_from_str(&bad).is_err());
        let typo = yaml.replace("apiKeyEnv", "apiKey");
        assert!(
            parse_suite_from_str(&typo).is_err()
                || !crate::config_keys::unknown_keys(&typo).is_empty()
        );
        // Unset: off, and the CLI wins over the test and the suite.
        let plain = parse_suite_from_str("appId: a\ntests:\n  - name: t\n    steps: []\n").unwrap();
        assert_eq!(plain.config.verify, None);
        assert_eq!(
            VerifyLevel::effective(None, None, plain.config.verify),
            VerifyLevel::Off
        );
        assert_eq!(
            VerifyLevel::effective(
                Some(VerifyLevel::Off),
                Some(VerifyLevel::Strict),
                Some(VerifyLevel::Final)
            ),
            VerifyLevel::Off
        );
        assert_eq!(VerifyLevel::EXPLORE_DEFAULT, VerifyLevel::Final);
        assert_eq!(
            "Checkpoints".parse::<VerifyLevel>(),
            Ok(VerifyLevel::Checkpoints)
        );
    }

    #[test]
    fn step_modifiers_force_optional_and_app_in_front_parse() {
        // D-3, D-12, D-2.
        let s = one_full_step("tap: { text: Pay }\n        force: true").unwrap();
        assert!(s.modifiers.force);
        let s = one_full_step("do: \"tap Pay\"\n        force: true").unwrap();
        assert!(s.modifiers.force);
        let s = one_full_step("optional: true\n        assert: \"a banner\"").unwrap();
        assert!(s.modifiers.optional);
        let s =
            one_full_step("assertNotVisible: { text: Error }\n        appInFront: false").unwrap();
        assert_eq!(s.modifiers.app_in_front, Some(false));
        let s = one_full_step("tap: { text: Pay }").unwrap();
        assert!(s.modifiers.is_default());
        // On a step it does nothing for, a modifier is an error.
        let err = one_full_step("assertVisible: { text: a }\n        force: true").unwrap_err();
        assert!(err.contains("`force` works only"), "{err}");
        let err = one_full_step("tap: { text: a }\n        appInFront: false").unwrap_err();
        assert!(
            err.contains("`appInFront` works only on `assertNotVisible`"),
            "{err}"
        );
        let err = one_full_step("tap: { text: a }\n        optional: yes please").unwrap_err();
        assert!(err.contains("`optional` takes true or false"), "{err}");
        let err = one_full_step("tap: { text: a }\n        force: true\n        force: false")
            .unwrap_err();
        assert!(err.contains("given twice"), "{err}");
    }

    #[test]
    fn one_word_action_typos_and_maestro_names_are_errors() {
        // VEL-061: these became natural-language steps.
        let err = one_step("lanuchApp").unwrap_err();
        assert!(err.contains("did you mean `launchApp`"), "{err}");
        let err = one_step("tapOn: { text: Go }").unwrap_err();
        assert!(err.contains("use `tap`"), "{err}");
        let err = one_step("tap").unwrap_err();
        assert!(err.contains("needs arguments"), "{err}");
        let err = one_step("scrollUntilVisibel").unwrap_err();
        assert!(err.contains("did you mean `scrollUntilVisible`"), "{err}");
        let err = one_step("inputtext").unwrap_err();
        assert!(err.contains("did you mean `inputText`"), "{err}");
        let err = one_step("asertVisible: { text: a }").unwrap_err();
        assert!(err.contains("did you mean `assertVisible`"), "{err}");
        // Words and sentences stay plain English; `do:` says so explicitly.
        for ok in [
            "Continue",
            "login",
            // Review-s9 M1: dictionary words near short action names.
            "Export",
            "Refresh",
            "Alert",
            "Insert",
            "Slide",
            "Photo",
            "React",
            "Await",
            "Opening",
            "export",
            "iPhone",
            "iCloud",
            "eBay",
            "\"Tap the login button\"",
            "do: hideKeyboard",
        ] {
            assert!(
                matches!(one_step(ok), Ok(Action::Natural { .. })),
                "{ok}: {:?}",
                one_step(ok)
            );
        }
        assert_eq!(one_step("clearMocks"), Ok(Action::ClearMocks));
    }

    #[test]
    fn dropped_modifiers_and_web_selector_combinations_are_errors() {
        // VEL-194: each of these parsed, and part of it was ignored.
        let err = one_step("tap: { text: Buy }\n        waitUntil: the cart shows 1").unwrap_err();
        assert!(
            err.contains("`waitUntil` works only on natural-language steps"),
            "{err}"
        );
        let err = one_step("tap: { text: Buy }\n        data: x").unwrap_err();
        assert!(err.contains("`data` works only"), "{err}");
        let err = one_step("tap: { text: Buy }\n        timeoutMs: 1\n        timeout_ms: 2")
            .unwrap_err();
        assert!(err.contains("`timeoutMs` is given twice"), "{err}");
        let err = one_step("tap: { text: Pay, css: .cancel }").unwrap_err();
        assert!(err.contains("css can't be combined with text"), "{err}");
        let err = one_step("tap: { xpath: //a, below: { text: b } }").unwrap_err();
        assert!(err.contains("xpath can't be combined with below"), "{err}");
        assert_eq!(
            one_step("tap: { css: .cancel }"),
            Ok(Action::Tap {
                selector: Selector::Css(".cancel".into())
            })
        );
        assert!(matches!(
            one_step("do: add to cart\n        data: x\n        waitUntil: the cart shows 1"),
            Ok(Action::Natural {
                data: Some(_),
                wait_until: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn unknown_modifiers_and_untagged_forms_name_the_problem() {
        // VEL-217: order-dependent errors, and untagged-enum type names.
        let before = one_step("flaky: true\n        tap: { text: a }").unwrap_err();
        let after = one_step("tap: { text: a }\n        flaky: true").unwrap_err();
        for err in [&before, &after] {
            assert!(err.contains("unknown action `flaky`"), "{err}");
            assert!(err.contains("timeoutMs, settle"), "{err}");
        }
        let err = one_step("tap: { text: a }\n        retry: 3").unwrap_err();
        assert!(err.contains("Maestro"), "{err}");
        let err = one_step("tap: { text: a }\n        doubleTap: { text: b }").unwrap_err();
        assert!(err.contains("two actions, `tap` and `doubleTap`"), "{err}");
        for (step, field) in [
            ("openLink: { uri: x }", "`uri`"),
            ("screenshot: { file: a }", "`file`"),
            ("assertScreenshot: { name: a, masks: [] }", "`masks`"),
            ("saveState: { nam: a }", "`nam`"),
            ("do: x\n        data: { val: 1 }", "`val`"),
        ] {
            let err = one_step(step).unwrap_err();
            assert!(
                err.contains(&format!("unknown field {field}")),
                "{step}: {err}"
            );
            assert!(
                !err.contains("untagged") && !err.contains("Dsl"),
                "{step}: {err}"
            );
        }
        let err = one_step("saveState: [a]").unwrap_err();
        assert!(
            err.contains("expected a string or a map, got a list"),
            "{err}"
        );
        // The forms that worked still do.
        assert_eq!(
            one_step("openLink: { link: x }"),
            Ok(Action::Goto { url: "x".into() })
        );
        assert_eq!(
            one_step("saveState: { name: a }"),
            Ok(Action::SaveState { name: "a".into() })
        );
        assert_eq!(
            one_step("screenshot:"),
            Ok(Action::Screenshot { filename: None })
        );
    }

    #[test]
    fn long_press_takes_every_selector_field_inline() {
        // VEL-221: `below:` and `css:` were "unknown field" on longPress only.
        let a = one_step("longPress: { text: Edit, below: { text: Shipping }, durationMs: 900 }")
            .unwrap();
        match a {
            Action::LongPress {
                selector: Selector::Relative { relation, .. },
                duration_ms,
            } => {
                assert_eq!(relation, velocity_common::Relation::Below);
                assert_eq!(duration_ms, Some(900));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            one_step("longPress: { css: .row }"),
            Ok(Action::LongPress {
                selector: Selector::Css(_),
                ..
            })
        ));
        let a = one_step("longPress: { selector: { id: x }, durationMs: 5 }").unwrap();
        assert_eq!(
            a,
            Action::LongPress {
                selector: Selector::Id("x".into()),
                duration_ms: Some(5)
            }
        );
        let err = one_step("longPress: { selector: { id: x }, text: y }").unwrap_err();
        assert!(err.contains("not both"), "{err}");
        let err = one_step("longPress: { txet: y }").unwrap_err();
        assert!(err.contains("unknown field `txet`"), "{err}");
    }

    #[test]
    fn misleading_validation_messages_are_fixed() {
        // VEL-218.
        let err = parse_suite_from_str("appId: com.x\n---\n- launchApp\n- tapOn: Login\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("velocity migrate maestro"), "{err}");
        let validate = |step: &str| {
            let yaml = format!("appId: a\ntests:\n  - name: t\n    steps:\n      - {step}\n");
            crate::validate_suite(&parse_suite_from_str(&yaml).unwrap())
                .unwrap_err()
                .to_string()
        };
        for step in [
            "do: \"  \"",
            "assertScreenshot: { name: a, threshold: 2 }",
            "setConfig: { sync.mode: none }",
        ] {
            let err = validate(step);
            assert!(!err.contains("Invalid selector"), "{step}: {err}");
            assert!(!err.contains("isn't milliseconds"), "{step}: {err}");
        }
        assert!(validate("setConfig: { sync.mode: none }").contains("can't change mid-test"));
        // dependsOn errors say where the test is.
        let (suite, tests) = {
            let yaml = "appId: a\ntests:\n  - name: t\n    dependsOn: [zz]\n    steps: [goBack]\n";
            let dir = std::env::temp_dir().join(format!("vel218-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("s.yaml");
            std::fs::write(&path, yaml).unwrap();
            let out = parse_suite_with_positions(&path.to_string_lossy()).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            out
        };
        let err = crate::validate_suite_at(&suite, &tests)
            .unwrap_err()
            .to_string();
        assert!(err.contains("s.yaml:3") && err.contains("'zz'"), "{err}");
    }

    #[test]
    fn assert_screenshot_takes_tolerance_comparator_and_crop() {
        // D-V2 and VT-12.
        let suite = parse_suite_from_str(
            "appId: a\ntests:\n  - name: t\n    steps:\n      - assertScreenshot: { name: a, tolerance: 4, crop: { id: card } }\n      - assertScreenshot: { name: b, comparator: perceptual, crop: { x: 1, y: 2, width: 30, height: 40 } }\n",
        )
        .unwrap();
        let shots: Vec<_> = suite.tests[0]
            .steps
            .iter()
            .map(|s| match &s.action {
                Action::AssertScreenshot {
                    tolerance,
                    comparator,
                    crop,
                    ..
                } => (*tolerance, *comparator, crop.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shots[0],
            (
                Some(4),
                None,
                Some(velocity_common::ScreenshotCrop::Element(Selector::Id(
                    "card".into()
                )))
            )
        );
        assert_eq!(
            shots[1],
            (
                None,
                Some(velocity_common::Comparator::Perceptual),
                Some(velocity_common::ScreenshotCrop::Rect(
                    velocity_common::Rect {
                        x: 1,
                        y: 2,
                        width: 30,
                        height: 40
                    }
                ))
            )
        );
        for bad in [
            "{ name: a, tolerance: 256 }",
            "{ name: a, comparator: fuzzy }",
            "{ name: a, crop: { x: 0, y: 0, width: 0, height: 5 } }",
            "{ name: a, crop: { x: 0, y: 0, width: 5 } }",
        ] {
            let y = format!(
                "appId: a\ntests:\n  - name: t\n    steps:\n      - assertScreenshot: {bad}\n"
            );
            assert!(parse_suite_from_str(&y).is_err(), "{bad}");
        }
        // tolerance is the strict rule's.
        let y = "appId: a\ntests:\n  - name: t\n    steps:\n      - assertScreenshot: { name: a, tolerance: 4, comparator: perceptual }\n";
        let suite = parse_suite_from_str(y).unwrap();
        let err = crate::validator::validate_suite(&suite)
            .unwrap_err()
            .to_string();
        assert!(err.contains("tolerance"), "{err}");
    }

    #[test]
    fn hide_keyboard_parses_both_forms() {
        let a = one_step("hideKeyboard").unwrap();
        assert!(matches!(a, velocity_common::Action::HideKeyboard));
        let a = one_step("hideKeyboard: {}").unwrap();
        assert!(matches!(a, velocity_common::Action::HideKeyboard));
    }
}
