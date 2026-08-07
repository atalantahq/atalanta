use serde::{Deserialize, Serialize};

use crate::types::{Action, Platform};

/// A single step in a test or flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// This step's settle: skip it, or other timings (`settle:` in YAML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle: Option<StepSettle>,
    /// Where the step is written in the suite YAML. A step inlined from a
    /// flow keeps the flow's line, so its failure points at the code to fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourcePos>,
    /// Step modifiers beside the action: `force`, `optional`, `appInFront`.
    #[serde(default, skip_serializing_if = "StepModifiers::is_default")]
    pub modifiers: StepModifiers,
}

/// Per-step switches written beside the action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepModifiers {
    /// `force: true`: act without the actionability checks (stable bounds,
    /// enabled, and on web the hit target). For custom and React Native
    /// views that report `enabled=false` but work (D-3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// `optional: true`: a failure of this step is a warning and the test
    /// goes on (D-12).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// `appInFront: false`: an `assertNotVisible` that may pass with
    /// another app in front (D-2). `None` is the default check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_in_front: Option<bool>,
    /// `checkpoint:` an AI check after this step under `verify:
    /// checkpoints` or `strict`. The claim the model checks; empty means the
    /// natural-language step's own `waitUntil`, else its intent
    /// (`checkpoint: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Set by flow resolution, never written in YAML: the `optional: true`
    /// `runFlow` blocks this inlined step belongs to, outermost first, as
    /// (block id, flow id). The first failure inside such a block skips
    /// the rest of it with one warning and the test goes on after it, as
    /// Maestro's optional subflow does (D-12, review M2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_flows: Vec<(u32, String)>,
}

impl StepModifiers {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// One step's settle: `settle: none` (no waiting before or after it) or
/// `settle: { reactionMs, quietMs, capMs }` (these timings for this step).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepSettle {
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub reaction_ms: Option<u64>,
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    #[serde(default)]
    pub cap_ms: Option<u64>,
}

/// A position in a suite file, 1-based (`tests/app.yaml:42:7`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourcePos {
    /// The suite path as it was given; empty when parsed from a string.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
}

impl SourcePos {
    pub fn new(file: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            file: file.into(),
            line,
            column,
        }
    }
}

/// `file:line`, the form editors and CI logs turn into links. The column is
/// left out: steps are whole lines.
impl std::fmt::Display for SourcePos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file.is_empty() {
            write!(f, "line {}", self.line)
        } else {
            write!(f, "{}:{}", self.file, self.line)
        }
    }
}

/// A reusable flow (sequence of steps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub id: String,
    pub steps: Vec<Step>,
}

/// A test case containing steps to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub isolated: bool,
    /// Tests that must run (and pass) before this one, in the same session on
    /// the same device — for tests that build on another's state. The runner
    /// keeps them together, in order, and skips this test if one fails.
    #[serde(default, rename = "dependsOn")]
    pub depends_on: Vec<String>,
    pub steps: Vec<Step>,
    /// What the test is meant to show (`description:`). The `verify: final`
    /// check asks whether it holds on the last screen; the name stands in
    /// when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// This test's `verify:`, over the suite's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyLevel>,
}

/// Sync engine mode selection.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    /// v1 polling-based sync (default).
    Polling,
    /// v2 native probe sync (requires probe embedded in app).
    Native,
    /// Try native first, fallback to polling if probe unavailable.
    #[default]
    Auto,
    /// No sync — skip all hierarchy polling, rely on explicit waits.
    None,
}

/// Sync engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConfig {
    #[serde(default)]
    pub mode: SyncMode,
    /// Deprecated: the settle is time-based now (`quietMs`). Still parsed.
    #[serde(alias = "interval_ms", default = "default_sync_interval")]
    pub interval_ms: u64,
    /// Deprecated: the settle is time-based now (`quietMs`). Still parsed.
    #[serde(alias = "stability_count", default = "default_stability_count")]
    pub stability_count: u32,
    #[serde(alias = "timeout_ms", default = "default_sync_timeout")]
    pub timeout_ms: u64,
    /// The screen counts as settled after this long with no change.
    /// `None`: not set, the platform's default ([`Self::quiet`]). Kept
    /// apart from a value that happens to equal the default, so a value
    /// set on purpose is never swapped for the web's own (NB-8, SUM-4).
    #[serde(alias = "quiet_ms", default, skip_serializing_if = "Option::is_none")]
    pub quiet_ms: Option<u64>,
    /// An action whose screen hasn't changed after this long is taken as
    /// done (some actions change nothing). `None`: the platform's default.
    #[serde(
        alias = "reaction_ms",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reaction_ms: Option<u64>,
    /// Longest wait for a settle; past it the step warns and continues.
    /// `None`: the platform's default.
    #[serde(
        alias = "settle_cap_ms",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub settle_cap_ms: Option<u64>,
    #[serde(default = "default_adaptive")]
    pub adaptive: bool,
    #[serde(alias = "native_port_ios", default = "default_native_port_ios")]
    pub native_port_ios: u16,
    #[serde(alias = "native_port_android", default = "default_native_port_android")]
    pub native_port_android: u16,
    #[serde(
        alias = "probe_connect_timeout_ms",
        default = "default_probe_connect_timeout"
    )]
    pub probe_connect_timeout_ms: u64,
    /// The native probes' timer horizon for this run's connection: a
    /// one-shot timer due further out doesn't keep the app busy (probe
    /// default 1500, EarlGrey's and Detox's). iOS probe only; the Android
    /// probe takes it but tracks no timers.
    #[serde(
        alias = "max_timer_ms",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_timer_ms: Option<u64>,
}

fn default_sync_interval() -> u64 {
    200
}
fn default_stability_count() -> u32 {
    3
}
fn default_sync_timeout() -> u64 {
    10000
}
fn default_quiet() -> u64 {
    150
}
fn default_reaction() -> u64 {
    300
}
fn default_settle_cap() -> u64 {
    3000
}
fn default_adaptive() -> bool {
    true
}
fn default_native_port_ios() -> u16 {
    19400
}
fn default_native_port_android() -> u16 {
    19401
}
fn default_probe_connect_timeout() -> u64 {
    2000
}

impl SyncConfig {
    /// The quiet time: as set, else the default (150 ms).
    pub fn quiet(&self) -> u64 {
        self.quiet_ms.unwrap_or_else(default_quiet)
    }
    /// The reaction window: as set, else the default (300 ms; the web's
    /// is its own, see `velocity_core::native_sync`).
    pub fn reaction(&self) -> u64 {
        self.reaction_ms.unwrap_or_else(default_reaction)
    }
    /// The settle cap: as set, else the default (3 s; the web's is its
    /// own).
    pub fn settle_cap(&self) -> u64 {
        self.settle_cap_ms.unwrap_or_else(default_settle_cap)
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            mode: SyncMode::default(),
            interval_ms: default_sync_interval(),
            stability_count: default_stability_count(),
            timeout_ms: default_sync_timeout(),
            quiet_ms: None,
            reaction_ms: None,
            settle_cap_ms: None,
            adaptive: default_adaptive(),
            native_port_ios: default_native_port_ios(),
            native_port_android: default_native_port_android(),
            probe_connect_timeout_ms: default_probe_connect_timeout(),
            max_timer_ms: None,
        }
    }
}

/// Retry configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub count: u32,
    /// What a test that passed only on retry does to the exit code:
    /// `pass` (default) keeps it green, `fail` exits 1 (D-21, as
    /// `--fail-on-flaky`). Reports show it as flaky either way.
    #[serde(default, rename = "flakyResult", alias = "flaky_result")]
    pub flaky_result: FlakyResult,
}

/// `retry.flakyResult`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlakyResult {
    #[default]
    Pass,
    Fail,
}

/// Artifact collection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactsConfig {
    // The snake_case spellings were silently ignored before (README and
    // example suites used them), so both are accepted.
    #[serde(default = "default_on_failure", alias = "on_failure")]
    pub on_failure: bool,
    #[serde(default = "default_output_dir", alias = "output_dir")]
    pub output_dir: String,
    /// What `screenshot:` steps do: write at once (`always`), keep in
    /// memory and write only if the test fails (`onFailure`), or skip the
    /// capture (`never`).
    #[serde(default)]
    pub screenshots: ScreenshotPolicy,
    /// Capture the screen after every step, written (under
    /// `<outputDir>/steps/<test>/`) only for a test that fails.
    #[serde(default, alias = "step_screenshots")]
    pub step_screenshots: bool,
    /// A video of each test (`off`, `on-failure`, `always`), written as
    /// `traces/<test>/video.mp4` (web: a JPEG screencast).
    #[serde(default)]
    pub video: crate::evidence::CaptureMode,
    /// Android keeps the last this many seconds of video in memory.
    #[serde(default = "default_video_seconds", alias = "video_seconds")]
    pub video_seconds: u32,
    /// The device's logs during each test (logcat, `log stream`, the
    /// browser console), written under `traces/<test>/`.
    #[serde(default)]
    pub logs: crate::evidence::CaptureMode,
}

fn default_video_seconds() -> u32 {
    30
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScreenshotPolicy {
    #[default]
    Always,
    #[serde(alias = "on_failure", alias = "onfailure")]
    OnFailure,
    Never,
}

fn default_on_failure() -> bool {
    true
}
fn default_output_dir() -> String {
    "./velocity-results".to_string()
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            on_failure: default_on_failure(),
            output_dir: default_output_dir(),
            screenshots: ScreenshotPolicy::default(),
            step_screenshots: false,
            video: Default::default(),
            video_seconds: default_video_seconds(),
            logs: Default::default(),
        }
    }
}

/// Configuration for automatic system dialog dismissal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DialogConfig {
    /// Whether auto-dismiss is enabled (default: true).
    #[serde(default = "default_dialog_enabled")]
    pub enabled: bool,
    /// Maximum number of dismiss attempts per sync cycle (default: 3).
    #[serde(alias = "max_dismissals", default = "default_max_dismissals")]
    pub max_dismissals: u32,
    /// Additional button labels to treat as dismiss targets.
    #[serde(alias = "custom_dismiss_labels", default)]
    pub custom_dismiss_labels: Vec<String>,
}

fn default_dialog_enabled() -> bool {
    true
}
fn default_max_dismissals() -> u32 {
    3
}

impl Default for DialogConfig {
    fn default() -> Self {
        Self {
            enabled: default_dialog_enabled(),
            max_dismissals: default_max_dismissals(),
            custom_dismiss_labels: Vec::new(),
        }
    }
}

/// What a heal does to the test (D-7, `healing.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealingMode {
    /// Use the healed element and pass, with the heal in the report.
    Auto,
    /// Compute the heal, but fail the step with the proposed patch and the
    /// `velocity heal --accept` command that applies it.
    Report,
    /// No healing.
    Off,
}

/// Configuration for self-healing selectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealingConfig {
    /// Whether self-healing is enabled (default: true).
    #[serde(default = "default_healing_enabled")]
    pub enabled: bool,
    /// `auto`, `report` or `off`. Unset: `report` when CI is detected,
    /// `auto` locally ([`HealingConfig::effective_mode`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<HealingMode>,
    /// Minimum confidence (0.0–1.0) required to accept a healed match (default: 0.8).
    #[serde(
        alias = "confidence_threshold",
        default = "default_confidence_threshold"
    )]
    pub confidence_threshold: f64,
    /// Whether to persist healed selector mappings (default: true).
    #[serde(alias = "persist_healed", default = "default_persist_healed")]
    pub persist_healed: bool,
}

fn default_healing_enabled() -> bool {
    true
}
fn default_confidence_threshold() -> f64 {
    0.8
}
fn default_persist_healed() -> bool {
    true
}

impl Default for HealingConfig {
    fn default() -> Self {
        Self {
            enabled: default_healing_enabled(),
            mode: None,
            confidence_threshold: default_confidence_threshold(),
            persist_healed: default_persist_healed(),
        }
    }
}

impl HealingConfig {
    /// The mode in force: `enabled: false` is `off`; an unset mode is
    /// `report` in CI and `auto` elsewhere.
    pub fn effective_mode(&self, in_ci: bool) -> HealingMode {
        if !self.enabled {
            return HealingMode::Off;
        }
        self.mode.unwrap_or(if in_ci {
            HealingMode::Report
        } else {
            HealingMode::Auto
        })
    }
}

/// Whether actions wait for their target to be actionable and fail when it
/// never is (D-3, `actionability:`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actionability {
    /// Stable bounds (two reads agree), enabled, and on web a hit target;
    /// otherwise the step fails as `NotActionable`.
    #[default]
    Strict,
    /// Wait for those, then act anyway with a warning (the old behaviour).
    Lenient,
}

/// How a natural-language step's `data` reaches the model (D-30).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NlDataPolicy {
    /// The value is sent as written; secrets are masked.
    #[default]
    Verbatim,
    /// Every value is described (`<value, 5 chars>`), never sent.
    Describe,
}

/// Natural-language step settings (`config.nl`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NlConfig {
    /// A `goto` from a natural-language step may leave the current page's
    /// origin (default: false, D-17).
    #[serde(default, alias = "allow_cross_origin")]
    pub allow_cross_origin: bool,
    /// `verbatim` (default) or `describe` (D-30).
    #[serde(default, alias = "data_policy")]
    pub data_policy: NlDataPolicy,
}

/// How much an AI checker verifies a run (`verify:`, `--verify`).
///
/// The checker is a model, so it is never on by default for YAML runs:
/// cached replays use deterministic assertions only, and CI pays for no
/// model call it didn't ask for. `velocity explore` starts from
/// [`VerifyLevel::EXPLORE_DEFAULT`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyLevel {
    /// No AI check; deterministic assertions only.
    #[default]
    Off,
    /// After the last step, check that the test's `description` (or name)
    /// holds on the final screen.
    Final,
    /// `final`, plus a check after every `assert:` step and every step
    /// with `checkpoint:`. A failed check is recorded and the test goes on;
    /// it fails at the end with every failed check listed.
    Checkpoints,
    /// `checkpoints`, but the first failed or unknown check fails the test
    /// at once.
    Strict,
}

impl VerifyLevel {
    /// The level `velocity explore` runs at unless told otherwise.
    pub const EXPLORE_DEFAULT: VerifyLevel = VerifyLevel::Final;

    /// Every level's name, in order.
    pub const NAMES: [&'static str; 4] = ["off", "final", "checkpoints", "strict"];

    /// Whether the end-of-test goal check runs.
    pub fn checks_final(self) -> bool {
        self >= VerifyLevel::Final
    }

    /// Whether `assert:` steps and `checkpoint:` steps are checked.
    pub fn checks_checkpoints(self) -> bool {
        self >= VerifyLevel::Checkpoints
    }

    /// Whether a failed check stops the test at once.
    pub fn is_strict(self) -> bool {
        self == VerifyLevel::Strict
    }

    /// The level in force: the CLI's, else the test's, else the suite's.
    pub fn effective(
        cli: Option<VerifyLevel>,
        test: Option<VerifyLevel>,
        suite: Option<VerifyLevel>,
    ) -> VerifyLevel {
        cli.or(test).or(suite).unwrap_or_default()
    }
}

impl std::fmt::Display for VerifyLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(Self::NAMES[*self as usize])
    }
}

impl std::str::FromStr for VerifyLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "final" => Ok(Self::Final),
            "checkpoints" => Ok(Self::Checkpoints),
            "strict" => Ok(Self::Strict),
            other => Err(format!(
                "verify level {other:?} is not one of {}",
                Self::NAMES.join(", ")
            )),
        }
    }
}

/// Which model service plans and checks natural-language steps
/// (`config.ai`). Every field is optional: the environment
/// (`VELOCITY_NL_PROVIDER`, `VELOCITY_NL_MODEL`, ...) wins over it, and
/// unset fields fall back to the provider's defaults. Keys are only ever
/// named by environment variable, never written here. `provider` is a
/// preset name (`anthropic`, `openai`, `azure-openai`, `gemini`,
/// `bedrock`, `vertex`, `openrouter`, `groq`, `ollama`, … see
/// `velocity_nl::provider::PRESETS`); `model` may carry it as
/// `provider:model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AiConfig {
    /// `anthropic` or `openai` (any OpenAI-compatible endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model for single steps and checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The model for goal-level planning (`velocity explore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore_model: Option<String>,
    /// The API base, e.g. `http://localhost:11434/v1` for Ollama.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The name of the environment variable that holds the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Send screenshots to the checker (default: the provider's). Set
    /// false for text-only models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    /// Bedrock's AWS region or Vertex's location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Vertex's Google Cloud project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Azure OpenAI's `api-version` (the deployments URL); unset uses the
    /// `/openai/v1` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
}

/// Configuration for opt-in resource profiling (heap, PSS, CPU).
/// Disabled by default — zero overhead when off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerformanceConfig {
    /// Whether resource profiling is enabled (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Sampling interval in milliseconds during long-running actions (default: 500).
    #[serde(alias = "interval_ms", default = "default_perf_interval")]
    pub interval_ms: u64,
    /// Percentage threshold for heap growth to flag as regression (default: 15).
    #[serde(
        alias = "heap_growth_threshold_pct",
        default = "default_heap_growth_threshold"
    )]
    pub heap_growth_threshold_pct: u32,
}

fn default_perf_interval() -> u64 {
    500
}
fn default_heap_growth_threshold() -> u32 {
    15
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_ms: default_perf_interval(),
            heap_growth_threshold_pct: default_heap_growth_threshold(),
        }
    }
}

/// Headless driver YAML configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessYamlConfig {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub density: Option<f32>,
    /// Physical pixels per logical pixel (D-46). Native headless: the
    /// same as `density` (set one of them). Flutter: the test surface is
    /// `width` x `height` physical px at this ratio; unset keeps the
    /// bridge's default surface (dpr 1).
    #[serde(alias = "device_pixel_ratio", default)]
    pub device_pixel_ratio: Option<f64>,
    /// Text size multiplier for native headless renders, like the system
    /// font-size setting (D-47). Default 1.0.
    #[serde(alias = "font_scale", default)]
    pub font_scale: Option<f32>,
    #[serde(alias = "app_path", default)]
    pub app_path: Option<String>,
    #[serde(alias = "initial_layout", default)]
    pub initial_layout: Option<String>,
}

/// The removed React Native headless sidecar's settings (D-44/45). Still
/// parsed so old suites load; `suite_warnings` says the block is ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReactNativeYamlConfig {
    /// Path to the JS bundle (e.g., index.android.bundle).
    #[serde(alias = "bundle_path", default)]
    pub bundle_path: Option<String>,
    /// Root component name.
    #[serde(default)]
    pub component: Option<String>,
    /// Sidecar TCP port (default: 19500).
    #[serde(default)]
    pub port: Option<u16>,
    /// Native module mocks (module_name -> { method: return_value }).
    #[serde(alias = "native_mocks", default)]
    pub native_mocks: std::collections::HashMap<String, serde_json::Value>,
}

/// Flutter headless configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlutterYamlConfig {
    /// Path to the Flutter project root.
    #[serde(alias = "project_path", default)]
    pub project_path: Option<String>,
    /// Dart target file (default: lib/main.dart).
    #[serde(default)]
    pub target: Option<String>,
    /// Directory for golden file baselines.
    #[serde(alias = "golden_dir", default)]
    pub golden_dir: Option<String>,
    /// TCP port for Dart test process (default: 19600).
    #[serde(default)]
    pub port: Option<u16>,
    /// The tree headless Flutter exports: `semantics` (the default, what
    /// the device drivers see) or `widgets` (deprecated opt-out: every
    /// widget, `text:` on `Text` widgets, `id:` on a `ValueKey`).
    #[serde(default)]
    pub hierarchy: Option<String>,
}

/// Web browser driver configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebConfig {
    /// Starting URL to open when launching the browser session.
    #[serde(alias = "start_url", default)]
    pub start_url: Option<String>,
    /// Viewport width in CSS pixels (default: 1280).
    #[serde(default)]
    pub width: Option<u32>,
    /// Viewport height in CSS pixels (default: 800).
    #[serde(default)]
    pub height: Option<u32>,
    /// Path to a Chrome/Chromium binary. Uses PATH detection if absent.
    #[serde(alias = "chrome_path", default)]
    pub chrome_path: Option<String>,
    /// Run browser headless (default: true).
    #[serde(default)]
    pub headless: Option<bool>,
    /// Extra Chrome launch arguments (e.g. ["--disable-web-security"]).
    #[serde(alias = "extra_args", default)]
    pub extra_args: Vec<String>,
    /// Requests answered without the network, for every test.
    #[serde(default)]
    pub mocks: Vec<crate::types::MockRule>,
    /// URL patterns refused (analytics, ads, third-party widgets).
    #[serde(default)]
    pub block: Vec<String>,
    /// A mobile-web profile (`"Pixel 7"`, `"iPhone 15"`, `"iPad"`; see
    /// [`crate::web_devices`]): its viewport, pixel ratio, user agent,
    /// mobile layout and touch. `width`/`height` still override the
    /// viewport. An unknown name is a config error (D-W3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Web-only settle timings (SUM-4): on the web they win over `sync.*`,
    /// so a cross-platform suite can tune each side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle: Option<WebSettle>,
    /// What each of `--parallel N`'s browsers is (W3): `contexts` (the
    /// default: browser contexts in one Chrome, each with its own cookies,
    /// storage and mocks) or `processes` (a Chrome process each).
    #[serde(
        alias = "parallel_mode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parallel_mode: Option<WebParallelMode>,
}

/// `web.parallelMode` (W3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebParallelMode {
    /// One Chrome process; each device is a browser context in it
    /// (`Target.createBrowserContext`): its own cookies, storage, cache and
    /// mocks, at a fraction of a process's memory and start-up.
    #[default]
    Contexts,
    /// One Chrome process (and profile) per device, as before W3.
    Processes,
}

/// `web.settle: { reactionMs, quietMs, capMs }`: the page settle's timings
/// on the web. Unset ones fall back to `sync.*`, then to the web defaults
/// (30 ms reaction, `sync`'s 150 ms quiet, 1 s cap).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSettle {
    #[serde(alias = "reaction_ms", default)]
    pub reaction_ms: Option<u64>,
    #[serde(alias = "quiet_ms", default)]
    pub quiet_ms: Option<u64>,
    #[serde(alias = "cap_ms", alias = "settleCapMs", default)]
    pub cap_ms: Option<u64>,
}

/// The app build Velocity installs before the first test (`app:`): one
/// path, or one per platform —
/// `app: { android: build/app.apk, ios: build/App.app }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AppSource {
    Path(String),
    PerPlatform(AppPaths),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppPaths {
    /// An `.apk`.
    #[serde(default)]
    pub android: Option<String>,
    /// An `.app` bundle (simulator or device build) or an `.ipa` (device).
    #[serde(default)]
    pub ios: Option<String>,
}

impl AppSource {
    /// The build for `platform`. A single path serves whichever platform
    /// runs; a per-platform map needs to know which one.
    pub fn for_platform(&self, platform: Option<Platform>) -> Option<&str> {
        match self {
            Self::Path(p) => Some(p),
            Self::PerPlatform(paths) => match platform? {
                Platform::Android => paths.android.as_deref(),
                Platform::Ios => paths.ios.as_deref(),
                Platform::Web => None,
            },
        }
    }
}

/// Suite-level configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SuiteConfig {
    #[serde(default)]
    pub platform: Option<Platform>,
    /// App build to install on every device before the first test, skipped
    /// where the same build is already installed (`--app` overrides it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppSource>,
    /// Driver mode: "device" (default) or "headless".
    #[serde(default)]
    pub driver: Option<crate::config::DriverMode>,
    /// App framework: auto-detected if omitted. Values: "native", "react_native", "flutter".
    #[serde(default)]
    pub framework: Option<crate::config::Framework>,
    /// Headless-specific configuration.
    #[serde(default)]
    pub headless: Option<HeadlessYamlConfig>,
    /// React Native bridge configuration.
    #[serde(default, alias = "reactNative")]
    pub react_native: Option<ReactNativeYamlConfig>,
    /// Flutter bridge configuration.
    #[serde(default)]
    pub flutter: Option<FlutterYamlConfig>,
    /// Web browser configuration.
    #[serde(default)]
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    /// `assertScreenshot` capture settings.
    #[serde(default)]
    pub screenshots: ScreenshotsConfig,
    #[serde(default)]
    pub dialog: DialogConfig,
    #[serde(default)]
    pub healing: HealingConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// `strict` (default) or `lenient` actionability checks (D-3).
    #[serde(default)]
    pub actionability: Actionability,
    /// Natural-language step settings.
    #[serde(default)]
    pub nl: NlConfig,
    /// AI checks for the run (`off` by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyLevel>,
    /// The model service natural-language steps and checks use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
    /// How `launchApp: { clearState: true }` resets the app.
    #[serde(default)]
    pub reset: ResetConfig,
    /// Batching: consecutive tests on a device share one warm executor, and
    /// a test whose first step is a plain `launchApp` (no clearState) right
    /// after a passing test of the same app skips that launch.
    #[serde(default)]
    pub batch: bool,
    /// What every `http` step starts from: `baseUrl`, `headers`,
    /// `timeoutMs`, `insecure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<crate::http::HttpConfig>,
    /// Driver and runtime settings by name (`velocity settings` lists them),
    /// e.g. `android: { scroll: semantic }`. `--set` overrides them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    /// The accessibility audit of the screens a test reads (off by default).
    #[serde(default)]
    pub accessibility: AccessibilityConfig,
    /// The suite file's directory, set when the suite is parsed from a file
    /// (never from YAML): `assertScreenshot` baselines live next to the
    /// suite, not in whatever directory the run started from.
    #[serde(skip)]
    pub suite_dir: Option<std::path::PathBuf>,
    /// The names `http` steps capture, set when the suite is interpolated
    /// (never from YAML): their `${NAME}`s are filled in as the test runs.
    #[serde(skip)]
    pub captured_vars: std::collections::BTreeSet<String>,
}

/// `accessibility:` in the suite config: audit each distinct screen a test
/// reads with the rules in [`crate::a11y`], from trees the run reads anyway.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityConfig {
    /// Audit the screens and report findings as test warnings.
    #[serde(default)]
    pub audit: bool,
    /// Fail a test on an error-severity finding (`audit` must be on).
    #[serde(default)]
    pub fail: bool,
    /// Android pixels per dp for the 48 dp target size (2.625 at 420 dpi).
    /// Unset: the screen's short side is taken as 411 dp (a phone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density: Option<f64>,
}

/// `screenshots:` in the suite config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScreenshotsConfig {
    /// Before the first `assertScreenshot` on a device, freeze its status
    /// bar (D-V4): Android SystemUI demo mode (12:00, full battery, full
    /// signal, no notifications) or `simctl status_bar override` on iOS
    /// simulators, both undone when the run ends. Where it took, the status
    /// bar is compared instead of masked. Off (default): the bar is masked.
    #[serde(default, alias = "freeze_status_bar")]
    pub freeze_status_bar: bool,
}

/// How an app is returned to a clean state.
///
/// Cheapest first: `clear` wipes data (`pm clear` / the iOS data container);
/// `reinstall` also resets permissions and anything outside the data dir;
/// `snapshot` restores an emulator snapshot — the whole device, ~10 s. A
/// strategy that can't run (no snapshot, no app path) falls back down the
/// ladder, ending at `clear`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetStrategy {
    #[default]
    Clear,
    Reinstall,
    Snapshot,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetConfig {
    #[serde(default)]
    pub strategy: ResetStrategy,
    /// App binary for `reinstall` (.apk / .app).
    #[serde(default, alias = "app_path")]
    pub app_path: Option<String>,
    /// Emulator snapshot name for `snapshot` (default `velocity-clean`).
    #[serde(default)]
    pub snapshot: Option<String>,
}

/// A complete test suite parsed from a velocity.yaml file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSuite {
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(default)]
    pub config: SuiteConfig,
    #[serde(default)]
    pub flows: Vec<Flow>,
    pub tests: Vec<TestCase>,
}

#[cfg(test)]
mod decision_config_tests {
    use super::*;

    #[test]
    fn healing_mode_defaults_to_report_in_ci_and_auto_locally() {
        let c = HealingConfig::default();
        assert_eq!(c.effective_mode(true), HealingMode::Report);
        assert_eq!(c.effective_mode(false), HealingMode::Auto);
        let c: HealingConfig = serde_json::from_str(r#"{"mode": "auto"}"#).unwrap();
        assert_eq!(c.effective_mode(true), HealingMode::Auto);
        let c: HealingConfig =
            serde_json::from_str(r#"{"enabled": false, "mode": "auto"}"#).unwrap();
        assert_eq!(c.effective_mode(false), HealingMode::Off);
    }

    #[test]
    fn actionability_and_nl_settings_parse() {
        let c: SuiteConfig = serde_json::from_str(
            r#"{"actionability": "lenient", "nl": {"allowCrossOrigin": true, "dataPolicy": "describe"}}"#,
        )
        .unwrap();
        assert_eq!(c.actionability, Actionability::Lenient);
        assert!(c.nl.allow_cross_origin);
        assert_eq!(c.nl.data_policy, NlDataPolicy::Describe);
        let c = SuiteConfig::default();
        assert_eq!(c.actionability, Actionability::Strict);
        assert_eq!(c.nl, NlConfig::default());
    }
}
