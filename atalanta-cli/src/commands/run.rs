// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use atalanta_common::{
    AtalantaError, DriverMode, Framework, Platform, PlatformDriver, ReportFormat, ResilientDriver,
    RuntimeConfig,
};
use atalanta_core::{parse_suite_with_positions, resolve_flows};
use atalanta_runner::SuiteRunner;
use clap::Args;
use colored::Colorize;

fn parse_driver_mode(value: &str) -> anyhow::Result<DriverMode> {
    match value {
        "device" => Ok(DriverMode::Device),
        "headless" => Ok(DriverMode::Headless),
        other => usage_bail!("Unknown driver mode '{other}'. Use 'device' or 'headless'."),
    }
}

/// `--update-baselines[=missing|changed|all]`.
fn parse_update_baselines(value: &str) -> Result<atalanta_common::BaselineUpdate, String> {
    match atalanta_common::BaselineUpdate::parse(value) {
        Some(atalanta_common::BaselineUpdate::Off) | None => Err(format!(
            "expected `missing` or `changed` (or nothing), got `{value}`"
        )),
        Some(mode) => Ok(mode),
    }
}

fn parse_framework(value: &str) -> anyhow::Result<Framework> {
    match value {
        "native" => Ok(Framework::Native),
        "react_native" | "reactnative" | "rn" => Ok(Framework::ReactNative),
        "flutter" => Ok(Framework::Flutter),
        other => {
            usage_bail!("Unknown framework '{other}'. Use 'native', 'react_native', or 'flutter'.")
        }
    }
}

/// Auto-detect app framework from the app path.
fn detect_framework(app_path: Option<&str>, platform: Platform) -> Framework {
    // Web platform has no app binary; framework is always Native from Atalanta's perspective.
    if platform == Platform::Web {
        return Framework::Native;
    }

    let Some(path) = app_path else {
        return Framework::Native;
    };

    let path_obj = std::path::Path::new(path);

    // Flutter detection: pubspec.yaml in project or libflutter.so in APK
    if path_obj.join("pubspec.yaml").exists() {
        return Framework::Flutter;
    }

    // React Native detection
    if platform == Platform::Android {
        // Check for libreactnativejni.so or index.android.bundle
        if path_obj.join("index.android.bundle").exists() {
            return Framework::ReactNative;
        }
    } else if platform == Platform::Ios {
        // Check for hermes.framework or jsc.framework inside .app
        let hermes = path_obj.join("Frameworks").join("hermes.framework");
        let jsc = path_obj.join("Frameworks").join("JavaScriptCore.framework");
        if hermes.exists() || jsc.exists() {
            return Framework::ReactNative;
        }
    }

    // Check for package.json with react-native dependency (project root)
    if path_obj.join("package.json").exists() {
        if let Ok(content) = std::fs::read_to_string(path_obj.join("package.json")) {
            if content.contains("react-native") {
                return Framework::ReactNative;
            }
        }
    }

    Framework::Native
}

#[derive(Args, Clone)]
#[command(group(clap::ArgGroup::new("sharding").args(["shard", "shard_total"]).multiple(true)))]
pub struct RunArgs {
    /// Test suite YAML to run, or a folder: every *.yaml / *.yml in it, each
    /// reporting into <artifacts-dir>/<file name> (same as --config)
    #[arg(value_name = "PATH")]
    path: Option<String>,

    /// The suite file to run (same as the PATH argument)
    #[arg(short, long, default_value = "atalanta.yaml")]
    config: String,

    /// Set by a folder run for each suite in it: `--tags` / `--filter` that
    /// select nothing in this one suite skip it instead of failing the run.
    #[arg(skip)]
    folder_member: bool,

    /// Target platform: ios, android or web. Auto-detected if omitted.
    #[arg(short, long, value_parser = ["ios", "android", "web"])]
    platform: Option<String>,

    /// Target device ID. Uses first available if omitted. With --parallel: an
    /// id list (`a,b`) or a name/id pattern (`*iPhone*`) limits the device pool.
    #[arg(short, long)]
    device: Option<String>,

    /// Run only tests matching these tags (comma-separated)
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,

    /// Run only tests whose name matches this glob pattern (use * for wildcards)
    #[arg(long)]
    filter: Option<String>,

    /// Run shard M of N (1-based, as Playwright, jest and nextest count):
    /// `--shard 2/4` is the second of four
    #[arg(long, value_name = "M/N", conflicts_with_all = ["shard_index", "shard_total"])]
    shard: Option<String>,

    /// Shard index (0-based, below --shard-total); `--shard M/N` is the
    /// 1-based form
    #[arg(long, requires = "shard_total")]
    shard_index: Option<usize>,

    /// Total number of shards (at least 1)
    #[arg(long, requires = "shard_index",
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    shard_total: Option<usize>,

    /// Balance shards by duration using this history file (or folder holding
    /// atalanta-history.json). Every shard must be given the same file;
    /// without it shards balance the number of tests
    #[arg(long, value_name = "FILE", requires = "sharding")]
    shard_timings: Option<String>,

    /// Merge history other CI jobs uploaded into this run's history first
    /// (a file, a folder holding atalanta-history.json, or a glob such as
    /// `history/*`; repeatable). Flaky flags, durations, --last-failed and
    /// --changed-since then see every job's runs. A pattern matching
    /// nothing is a warning; a file that can't be read is exit 2
    #[arg(long, value_name = "PATH|GLOB")]
    history_from: Vec<String>,

    /// Retries for a failed test (at most 10). With --parallel a retry runs
    /// on another device, from a cold app launch
    #[arg(long)]
    retries: Option<u32>,

    /// Report formats, comma-separated: junit, json, html, github, allure.
    /// `github` (annotations + job summary) is added automatically in GitHub Actions.
    #[arg(long, default_value = "junit", value_parser = check_reports)]
    report: String,

    /// JUnit `classname` of every test: `test` (default) keeps today's
    /// constant `atalanta`; `suite` uses the suite file's stem
    /// (`checkout` for `checkout.yaml`), so CI groups tests by suite. Changing it resets
    /// CI test history once; `suite` becomes the default at 1.0
    #[arg(long, value_name = "MODE", default_value = "test")]
    junit_classname: JunitClassname,

    /// With `--report allure`: remove the Allure results already in
    /// `<artifacts>/allure-results` before writing this run's. Results are
    /// kept by default, so reruns (`--last-failed`) and shards sharing the
    /// directory add up into one report
    #[arg(long)]
    clean_allure: bool,

    /// Run only the tests that failed in the last run (all of them if none did)
    #[arg(long)]
    last_failed: bool,

    /// Skip tests that already passed with the same app build, test, config and
    /// device (`sideEffects: true` tests always run)
    #[arg(long)]
    cache_results: bool,

    /// Early flake detection: re-run tests new to the history up to 10 times after
    /// they pass, and report them as flaky if any re-run fails
    #[arg(long)]
    detect_new_flakes: bool,

    /// Quarantine list [default: atalanta-quarantine.yaml next to the suite]
    #[arg(long)]
    quarantine: Option<String>,

    /// Hand the run to `atalanta daemon` (warm devices); runs here if none is up
    #[arg(long)]
    daemon: bool,

    /// Write a Chrome trace of the run (tests, steps, settles) to FILE; open it in
    /// https://ui.perfetto.dev
    #[arg(long, value_name = "FILE")]
    profile: Option<String>,

    /// Print where the run's time went: per action, its count, total, average,
    /// slowest step and the share spent waiting for the screen to settle
    #[arg(long)]
    profile_steps: bool,

    /// Output directory for artifacts
    #[arg(long, default_value = "./atalanta-results")]
    artifacts_dir: String,

    /// Suite timeout; exceeding it exits 4. Takes a unit (30s, 10m, 500ms);
    /// a bare number is milliseconds
    #[arg(long, value_name = "DURATION", value_parser = super::duration_arg::bare_millis)]
    timeout: Option<std::time::Duration>,

    /// Per-test timeout: an attempt that runs longer fails (and may be
    /// retried) while the rest of the run goes on, unlike --timeout, which
    /// ends the whole run. Takes a unit (90s, 5m); a bare number is
    /// milliseconds
    #[arg(long, value_name = "DURATION", value_parser = super::duration_arg::bare_millis)]
    test_timeout: Option<std::time::Duration>,

    /// Stop on first failure
    #[arg(long)]
    fail_fast: bool,

    /// A test that passed only on retry (flaky) fails the run: exit 1.
    /// Reports still show it as flaky. Same as `retry.flakyResult: fail`
    #[arg(long)]
    fail_on_flaky: bool,

    /// Run the tests that failed last run first (uses atalanta-history.json)
    #[arg(long)]
    failed_first: bool,

    /// Shuffle test order (dependsOn still runs first) to expose order-dependent tests
    #[arg(long)]
    shuffle: bool,

    /// Seed for --shuffle / --jitter, to replay a failing order (printed on every shuffled run)
    #[arg(long)]
    seed: Option<u64>,

    /// Flaky hunting: run the suite N times (shuffled, step jitter, no retries) and report
    /// tests whose outcome varies; exits 1 if any did. With --parallel M, the (test, round)
    /// copies share the device pool, and copies of a test go to different devices
    #[arg(long, value_name = "ROUNDS")]
    hunt: Option<u32>,

    /// With --hunt: for each flaky test, the YAML change for its most likely cause (a
    /// patch-like snippet with its line and reason; the suite file is not edited), then
    /// the hunt again with the changes applied in memory to show whether the flake went
    #[arg(long, requires = "hunt")]
    suggest: bool,

    /// Run copies of tests the history flags as flaky, enough that at least one
    /// passes with this probability (e.g. 0.99); a test passes if any copy does
    /// and is then reported as flaky. Copies go to different devices (--parallel)
    #[arg(long, value_name = "P")]
    flaky_confidence: Option<f64>,

    /// When to keep a test's trace (steps, bounds, settle reads, screen trees) under
    /// <artifacts>/traces/: off, on-failure, always
    #[arg(long, value_name = "MODE", default_value = "on-failure")]
    trace: atalanta_common::TraceMode,

    /// Record each test's screen: off, on-failure (kept for failed tests; the
    /// default, or `artifacts.video`), always. Android keeps the last
    /// `artifacts.videoSeconds` in memory; written to <artifacts>/traces/<test>/
    #[arg(long, value_name = "MODE")]
    video: Option<atalanta_common::CaptureMode>,

    /// Capture each test's device logs (logcat, simulator `log stream`, browser
    /// console): off, on-failure (default, or `artifacts.logs`), always
    #[arg(long, value_name = "MODE")]
    logs: Option<atalanta_common::CaptureMode>,

    /// Stop at the first failing step with the device as it is, print where the
    /// screenshot, trace and video are, and wait: Enter continues, r retries the
    /// step, q aborts. Interactive terminals only (ignored with a warning in CI)
    #[arg(long)]
    pause_on_failure: bool,

    /// Random delay of up to MS before each step (defaults to 250 with --hunt)
    #[arg(long, value_name = "MS")]
    jitter: Option<u64>,

    /// Load the device CPU while running (Android), to expose timing-dependent flakes.
    /// In a hunt, every other round (1st, 3rd, …) is stressed, so a flake that needs the
    /// load can be told from one that doesn't
    #[arg(long)]
    stress: bool,

    /// Run tests in parallel across up to N booted devices
    #[arg(long, value_name = "N")]
    parallel: Option<usize>,

    /// App build (.apk, .app, .ipa) to install on every device of the run before
    /// its first test; skipped where that exact build is installed. Overrides `app:`
    #[arg(long, value_name = "PATH")]
    app: Option<String>,

    /// Boot the devices the run needs first (see `atalanta devices up`): enough for
    /// --parallel N, or one. ATALANTA_AVD / ATALANTA_DEVICE_TYPE pick what to boot,
    /// ATALANTA_EMULATOR_PORT the first emulator port
    #[arg(long)]
    provision: bool,

    /// Change-based selection: run tests affected by changes since this git ref, plus
    /// recently failed, new and stale tests (needs an impact map)
    #[arg(long, value_name = "GIT_REF")]
    changed_since: Option<String>,

    /// Impact map (tests/flows -> source patterns) [default: atalanta-impact.yaml next to the suite]
    #[arg(long)]
    impact_map: Option<String>,

    /// With --changed-since: fail (exit 2) when no test is affected by
    /// the changes (git and the impact map; tests the local history picks
    /// don't count, so every shard decides alike), instead of passing
    #[arg(long, requires = "changed_since")]
    fail_if_none: bool,

    /// Environment variable overrides (KEY=VALUE; several per flag as
    /// A=1,B=2; a comma not followed by KEY= stays in the value)
    #[arg(long = "env")]
    env_overrides: Vec<String>,

    /// Read environment variable overrides from a dotenv file (repeatable):
    /// KEY=VALUE lines, `#` comments, an optional `export `, values in "…"
    /// or '…', nothing expanded. --env wins over a file, a later file over
    /// an earlier one, any file over the environment. No .env is read
    /// unless named here
    #[arg(long = "env-file", value_name = "PATH")]
    env_files: Vec<String>,

    /// Change a setting for this run (repeatable): a named setting
    /// (`atalanta settings`: `android.scroll=semantic`, `ios.launch=wda`) or
    /// any suite config key (`sync.reactionMs=150`, `retry.count=2`)
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Driver mode: device or headless
    #[arg(long, value_parser = ["device", "headless"])]
    driver: Option<String>,

    /// App framework: native, react_native, or flutter. Auto-detected if omitted.
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new([
        clap::builder::PossibleValue::new("native"),
        clap::builder::PossibleValue::new("react_native").aliases(["reactnative", "rn"]),
        clap::builder::PossibleValue::new("flutter"),
    ]))]
    framework: Option<String>,

    /// Write assertScreenshot baselines: bare (or =changed, =all) writes
    /// every missing or changed one and passes; =missing writes only
    /// missing ones and compares the rest. Without it a missing baseline is
    /// written and the step fails.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "changed",
        value_name = "missing|changed",
        value_parser = parse_update_baselines
    )]
    update_baselines: Option<atalanta_common::BaselineUpdate>,

    /// AI verification for every test, over the suite's and the tests'
    /// `verify:`: off (deterministic assertions only), final (the test's
    /// description holds at the end), checkpoints (also after assert: and
    /// checkpoint: steps; failures reported at the end), strict (the first
    /// failed check fails the test). Needs a model key unless off
    #[arg(long, value_name = "off|final|checkpoints|strict", value_parser = parse_verify)]
    verify: Option<atalanta_common::VerifyLevel>,
}

/// `--verify`.
fn parse_verify(value: &str) -> Result<atalanta_common::VerifyLevel, String> {
    value.parse()
}

/// `--junit-classname`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum JunitClassname {
    /// `atalanta` for every test (the default).
    Test,
    /// The suite file's stem.
    Suite,
}

/// The JUnit classname for `suite_file` under `mode`; `None` keeps the
/// reporter's default.
fn junit_classname(mode: JunitClassname, suite_file: &str) -> Option<String> {
    match mode {
        JunitClassname::Test => None,
        JunitClassname::Suite => std::path::Path::new(suite_file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty()),
    }
}

impl RunArgs {
    pub fn profile_path(&self) -> Option<String> {
        self.profile.clone()
    }

    pub fn via_daemon(&self) -> bool {
        self.daemon
    }
}

pub async fn execute(args: RunArgs) -> anyhow::Result<i32> {
    let mut args = args;
    if let Some(path) = args.path.take() {
        args.config = path;
    }

    // A directory runs every suite in it, in name order.
    let path = std::path::Path::new(&args.config);
    if path.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(path)?
            .flatten()
            .map(|e| e.path())
            // Not the impact map or quarantine list next to the suites, and
            // not a folder that happens to be named `x.yaml` (as `validate`).
            .filter(|p| p.is_file() && atalanta_core::is_suite_file(p))
            .collect();
        files.sort();
        if files.is_empty() {
            usage_bail!("no .yaml suites in {}", path.display());
        }
        let mut codes = Vec::new();
        let mut skipped = 0;
        crate::signals::install();
        let seen = crate::signals::count();
        for file in &files {
            let mut one = args.clone();
            one.folder_member = true;
            one.config = file.to_string_lossy().into_owned();
            // Each suite reports into its own folder: they all wrote
            // junit.xml / results.json to the same one, so only the last
            // suite's report survived.
            let stem = file.file_stem().unwrap_or_default().to_string_lossy();
            // `a.yaml` beside `a.yml` share a stem, and so would a folder:
            // those two keep their extension.
            let shared = files
                .iter()
                .filter(|f| f.file_stem() == file.file_stem())
                .count()
                > 1;
            let name = if shared {
                file.file_name().unwrap_or_default().to_string_lossy()
            } else {
                stem
            };
            one.artifacts_dir = format!("{}/{name}", args.artifacts_dir.trim_end_matches('/'));
            // One suite that can't load doesn't stop the others.
            let code = match Box::pin(execute_file(one)).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!(
                        "{} {}: {}",
                        "error:".red().bold(),
                        file.display(),
                        crate::redacted(&e)
                    );
                    super::exit_code_for(&e)
                }
            };
            if code == NOTHING_SELECTED {
                skipped += 1;
            } else {
                codes.push(code);
            }
            // Ctrl-C / SIGTERM stops the folder, not just the one suite;
            // so does a daemon client going away (M-5).
            if crate::signals::count() > seen || super::daemon::client_disconnected() {
                break;
            }
        }
        // `--tags` / `--filter` must match somewhere in the folder, not in
        // every suite: tagging only some suites is normal.
        if codes.is_empty() && skipped > 0 {
            usage_bail!(
                "no test in {} matches the --tags / --filter given",
                path.display()
            );
        }
        return Ok(combined_exit(&codes));
    }
    Box::pin(execute_file(args)).await
}

/// Install the suite's `settings:` and every `--set` (a named setting, or a
/// path into the suite's `config:`), `--set` winning.
/// Parse the suite, apply `--set`, interpolate `${VARS}`, then validate.
/// Validation used to run first, so `text: "${EMPTY}"` passed it and ran
/// as an empty selector, and `runFlow: ${FLOW}` was refused (VEL-196).
pub(crate) fn load_suite(
    path: &str,
    set: &[String],
    env_overrides: &[(String, String)],
) -> anyhow::Result<(
    atalanta_common::TestSuite,
    std::collections::HashMap<String, atalanta_common::SourcePos>,
)> {
    let (mut suite, test_positions) = parse_suite_with_positions(path)?;
    apply_settings(&mut suite, set)?;
    let overrides: std::collections::HashMap<String, String> =
        env_overrides.iter().cloned().collect();
    atalanta_core::env::interpolate_suite(&mut suite, &overrides)?;
    // `tags: [${TEAM}]` too, or `--tags` can never match it (VEL-175).
    for test in &mut suite.tests {
        for tag in &mut test.tags {
            if tag.contains("${") {
                *tag = atalanta_core::env::interpolate(tag, &overrides)?;
            }
        }
    }
    // Capabilities are checked once --platform/--driver are applied.
    atalanta_core::validate_structure_at(&suite, &test_positions)?;
    Ok((suite, test_positions))
}

fn apply_settings(suite: &mut atalanta_common::TestSuite, set: &[String]) -> anyhow::Result<()> {
    apply_settings_with(suite, set, atalanta_common::env::set_settings)
}

/// [`apply_settings`], handing the settings to `install` instead of the
/// process-wide store: the unit tests check what is installed without
/// sharing that store with every other test running at the same time,
/// which made the VEL-041 test flaky (C7).
fn apply_settings_with(
    suite: &mut atalanta_common::TestSuite,
    set: &[String],
    install: impl FnOnce(std::collections::HashMap<String, String>),
) -> anyhow::Result<()> {
    use atalanta_common::settings;
    let mut named = suite
        .config
        .settings
        .as_ref()
        .map(settings::flatten)
        .unwrap_or_default();
    let mut config = serde_json::to_value(&suite.config)?;
    let mut config_changed = false;
    for pair in set {
        let Some((key, value)) = pair.split_once('=') else {
            usage_bail!("--set {pair}: expected KEY=VALUE");
        };
        let key = key.trim();
        if settings::find(key).is_some() {
            named.insert(key.to_string(), value.to_string());
            continue;
        }
        // `android.*`, `ios.*`, `nl.*` are settings, never config paths: a
        // typo there gets the closest setting, not "no config section".
        let prefix = key.split('.').next().unwrap_or("");
        if settings::SETTINGS
            .iter()
            .any(|s| s.key.starts_with(&format!("{prefix}.")))
        {
            let one = std::collections::BTreeMap::from([(key.to_string(), value.to_string())]);
            settings::to_env(&one).map_err(|e| usage_err!("--set {e}"))?;
        }
        set_config_path(&mut config, key, value)?;
        config_changed = true;
    }
    if config_changed {
        let settings_before = suite.config.settings.take();
        let suite_dir = suite.config.suite_dir.take();
        suite.config = serde_json::from_value(config)
            .map_err(|e| usage_err!("--set: the config no longer reads: {e}"))?;
        suite.config.suite_dir = suite_dir;
        if suite.config.settings.is_none() {
            suite.config.settings = settings_before;
        }
    }
    // Always, an empty set too: the previous suite's settings (folder
    // runs, the daemon) must not carry over (VEL-041).
    let env = settings::to_env(&named).map_err(|e| usage_err!("{e}"))?;
    install(env);
    Ok(())
}

/// `config.<a.b.c> = value`, keeping the existing value's JSON type (a
/// number stays a number); a key the config doesn't have is an error.
fn set_config_path(config: &mut serde_json::Value, path: &str, value: &str) -> anyhow::Result<()> {
    let mut cur = config;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| usage_err!("--set {path}: `{part}` isn't a section"))?;
        // Keys are camelCase in YAML and snake_case in the structs' JSON.
        let snake: String = part
            .chars()
            .flat_map(|c| {
                if c.is_uppercase() {
                    vec!['_', c.to_ascii_lowercase()]
                } else {
                    vec![c]
                }
            })
            .collect();
        let name = if obj.contains_key(*part) {
            part.to_string()
        } else {
            snake
        };
        if i + 1 == parts.len() {
            let old = obj.get(&name).ok_or_else(|| {
                usage_err!(
                    "--set {path}: not a setting or config key (`atalanta settings` lists settings)"
                )
            })?;
            let new = match old {
                serde_json::Value::Number(_) => serde_json::Value::Number(
                    value
                        .parse::<i64>()
                        .map(Into::into)
                        .or_else(|_| {
                            value
                                .parse::<f64>()
                                .ok()
                                .and_then(serde_json::Number::from_f64)
                                .ok_or(())
                        })
                        .map_err(|_| usage_err!("--set {path}: `{value}` isn't a number"))?,
                ),
                serde_json::Value::Bool(_) => {
                    serde_json::Value::Bool(match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "on" => true,
                        "false" | "0" | "no" | "off" => false,
                        // Anything else used to read as false, a typo too.
                        _ => usage_bail!("--set {path}: `{value}` isn't true or false"),
                    })
                }
                _ => serde_json::Value::String(value.to_string()),
            };
            obj.insert(name, new);
            return Ok(());
        }
        cur = obj
            .get_mut(&name)
            .ok_or_else(|| usage_err!("--set {path}: no config section `{part}`"))?;
    }
    Ok(())
}

/// The `--profile-steps` table: time per action across the run, slowest
/// first, with how much of it was the post-action settle.
fn step_profile(result: &atalanta_common::SuiteResult) -> String {
    use std::collections::HashMap;
    // action → (count, total ms, settle ms, slowest ms, slowest "test #step")
    let mut by: HashMap<&str, (usize, u128, u128, u128, String)> = HashMap::new();
    let mut total: u128 = 0;
    for t in &result.tests {
        for s in &t.steps {
            let ms = s.duration.as_millis();
            total += ms;
            let e = by.entry(s.action_name.as_str()).or_default();
            e.0 += 1;
            e.1 += ms;
            e.2 += u128::from(s.settle_ms.unwrap_or(0));
            if ms >= e.3 {
                e.3 = ms;
                e.4 = format!("{} #{}", t.test_name, s.step_index + 1);
            }
        }
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.1));
    let mut out = format!(
        "\n  {:<20} {:>5} {:>9} {:>8} {:>7} {:>6}  slowest\n",
        "action", "steps", "total", "avg", "settle", "share"
    );
    for (action, (n, ms, settle, max, at)) in rows {
        let pct = (ms * 100).checked_div(total).unwrap_or(0);
        let settle_pct = (settle * 100).checked_div(ms).unwrap_or(0);
        out.push_str(&format!(
            "  {:<20} {:>5} {:>8.1}s {:>6}ms {:>6}% {:>5}%  {:.1}s ({})\n",
            action.chars().take(20).collect::<String>(),
            n,
            ms as f64 / 1000.0,
            ms / n as u128,
            settle_pct,
            pct,
            max as f64 / 1000.0,
            at
        ));
    }
    out
}

/// The 0-based (index, total) of `--shard M/N` (1-based) or of
/// `--shard-index`/`--shard-total` (0-based). clap refuses both forms
/// together; a malformed or out-of-range `M/N` is a usage error (exit 2).
pub(crate) fn shard_of(
    shard: Option<&str>,
    index: Option<usize>,
    total: Option<usize>,
) -> anyhow::Result<(Option<usize>, Option<usize>)> {
    let Some(spec) = shard else {
        return Ok((index, total));
    };
    if index.is_some() || total.is_some() {
        usage_bail!(
            "--shard {spec}: give either --shard M/N or --shard-index/--shard-total, not both"
        );
    }
    let parsed = spec.split_once('/').and_then(|(m, n)| {
        Some((
            m.trim().parse::<usize>().ok()?,
            n.trim().parse::<usize>().ok()?,
        ))
    });
    match parsed {
        Some((m, n)) if n >= 1 && (1..=n).contains(&m) => Ok((Some(m - 1), Some(n))),
        Some((m, n)) if n >= 1 => usage_bail!(
            "--shard {m}/{n}: shards are numbered 1 to {n} (--shard is 1-based; --shard-index is the 0-based form)"
        ),
        _ => usage_bail!("--shard {spec}: expected M/N with 1 ≤ M ≤ N, e.g. --shard 1/4"),
    }
}

/// The `--device` filter matches a device `list_devices` reports, under
/// (for a `--parallel` pool) the farm's own rule (`atalanta_runner::farm::device_filter_matches`:
/// ignoring case, a single value is a substring or `*` glob of a name or
/// id, a list names exact ids); else exit 3 at once, listing the devices
/// there are. A listing that fails or takes over 15 s skips the check with
/// a warning (the run's own device errors say more than a guess would),
/// and so does a remote adb id (`host:port/serial`) when
/// `ATALANTA_ADB_SERVERS` doesn't list remote servers.
///
/// Only a `--parallel` pool takes the farm's rule. A sequential run uses
/// `--device` as the device itself, so there each part must be a device's
/// exact id or name, ignoring case (review-d7 m8): `emulator-555`, a typo
/// for `emulator-5554`, passed as a substring and then failed every step.
///
/// A sequential run's `--device` given by name (or in another case) is
/// answered with the id the driver knows it by ([`resolve_exact_device`]):
/// the name passed this check and then went to adb as the serial.
async fn check_devices_exist(
    wanted: &str,
    driver: &dyn PlatformDriver,
    pool: bool,
) -> anyhow::Result<Option<String>> {
    check_devices_exist_within(wanted, driver, std::time::Duration::from_secs(15), pool).await
}

/// The id of the one device a sequential `--device` names, when `wanted` is
/// not already that id as the driver spells it: its name, or its id in
/// another case, ignoring case. `None` for an exact id and a name no listed
/// device has. A list is an error (only a `--parallel` pool takes one: the
/// whole text went to adb as one serial), and so is a name shared by
/// several devices, which lists their ids.
fn resolve_exact_device(
    wanted: &str,
    devices: &[atalanta_common::DeviceInfo],
) -> Result<Option<String>, String> {
    let w = wanted.trim();
    if w.contains(',') {
        return Err("takes one device unless --parallel N (N ≥ 2) runs on several".into());
    }
    if w.is_empty() || devices.iter().any(|d| d.id == w) {
        return Ok(None);
    }
    if let Some(d) = devices.iter().find(|d| d.id.eq_ignore_ascii_case(w)) {
        return Ok(Some(d.id.clone()));
    }
    let named: Vec<&atalanta_common::DeviceInfo> = devices
        .iter()
        .filter(|d| d.name.eq_ignore_ascii_case(w))
        .collect();
    match named.as_slice() {
        [] => Ok(None),
        [d] => Ok(Some(d.id.clone())),
        many => Err(format!(
            "{} devices are named '{w}'; pass one id: {}",
            many.len(),
            many.iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The first part of `wanted` that is no device's exact id or name
/// (ignoring case), if any. Parts `skip` accepts aren't checked.
fn unmatched_exact_device(
    wanted: &str,
    devices: &[atalanta_common::DeviceInfo],
    skip: impl Fn(&str) -> bool,
) -> Option<String> {
    wanted
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && !skip(p))
        .find(|p| {
            !devices
                .iter()
                .any(|d| d.id.eq_ignore_ascii_case(p) || d.name.eq_ignore_ascii_case(p))
        })
        .map(str::to_string)
}

async fn check_devices_exist_within(
    wanted: &str,
    driver: &dyn PlatformDriver,
    limit: std::time::Duration,
    pool: bool,
) -> anyhow::Result<Option<String>> {
    if wanted.trim().is_empty() {
        return Ok(None);
    }
    let skip_warning = |why: String| {
        eprintln!(
            "{} not checking --device {wanted} up front: {why}",
            "warning:".yellow().bold()
        );
    };
    let devices = match tokio::time::timeout(limit, driver.list_devices_strict()).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            skip_warning(format!("listing devices failed ({e})"));
            return Ok(None);
        }
        Err(_) => {
            skip_warning(format!(
                "listing devices took over {}s",
                limit.as_secs_f32()
            ));
            return Ok(None);
        }
    };
    let remote_listed =
        atalanta_common::env::var("ATALANTA_ADB_SERVERS").is_ok_and(|v| !v.trim().is_empty());
    let skip = |id: &str| id.contains('/') && !remote_listed;
    let missing = if pool {
        atalanta_runner::farm::unmatched_device_filter(wanted, &devices, skip)
    } else {
        unmatched_exact_device(wanted, &devices, skip)
    };
    if let Some(missing) = missing {
        let available: Vec<String> = devices.iter().map(|d| d.id.clone()).collect();
        return Err(super::no_device(
            &format!(
                "--device {missing}: no such device (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            ),
            available,
        ));
    }
    if pool {
        return Ok(None);
    }
    resolve_exact_device(wanted, &devices).map_err(|e| usage_err!("--device {wanted}: {e}"))
}

/// The first id of a `--device` list (`a,b` → `a`), or the value itself.
fn first_device(device: &str) -> &str {
    device
        .split(',')
        .map(str::trim)
        .find(|d| !d.is_empty())
        .unwrap_or(device)
}

/// The platform the suite runs on (CLI > suite > the device's kind > iOS),
/// written back into the suite: a suite without `platform:` (a migrated
/// Maestro flow) runs on the platform of the device it was given, and the
/// executor reads `config.platform` (a web run with no `platform:` got the
/// device status-bar mask over the page's own header).
fn resolve_platform(
    suite: &mut atalanta_common::TestSuite,
    cli: Option<Platform>,
    device: Option<&str>,
) -> Platform {
    let platform = cli
        .or(suite.config.platform)
        .or(device.map(|d| Platform::of_device_id(first_device(d))))
        .unwrap_or(Platform::Ios);
    suite.config.platform = Some(platform);
    platform
}

/// The configuration the driver for this run is built from, for the
/// daemon's pool key: only what that driver reads. A device-mode iOS or
/// Android run keyed on the suite's `web:` / `headless:` sections got a
/// second driver (and WDA session) for the same simulator (R-m3).
fn driver_config(
    suite: &atalanta_common::TestSuite,
    platform: Platform,
    mode: DriverMode,
    framework: Framework,
) -> serde_json::Value {
    fn json<T: serde::Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).unwrap_or_default()
    }
    let c = &suite.config;
    match (mode, framework) {
        (DriverMode::Device, _) if platform == Platform::Web => json(&c.web),
        (DriverMode::Device, _) => serde_json::Value::Null,
        // Refused before a driver is built (D-44/45).
        (DriverMode::Headless, Framework::ReactNative) => json(&c.headless),
        (DriverMode::Headless, Framework::Flutter) => {
            serde_json::json!([json(&c.headless), json(&c.flutter)])
        }
        (DriverMode::Headless, Framework::Native) => json(&c.headless),
    }
}

/// How many browsers a web run starts: one per `--parallel` slot (D-48).
fn web_instances(parallel: Option<usize>) -> usize {
    parallel.unwrap_or(1).max(1)
}

/// The exit code for several suites: the most serious outcome, where a
/// failing test outranks a device problem (taking the largest number let a
/// locked device's 3 hide another suite's real failure, 1).
fn combined_exit(codes: &[i32]) -> i32 {
    // Least to most serious; unknown codes count as internal errors.
    const ORDER: [i32; 6] = [0, 3, 4, 1, 2, 5];
    let rank = |c: &i32| ORDER.iter().position(|o| o == c).unwrap_or(ORDER.len());
    codes.iter().copied().max_by_key(rank).unwrap_or(0)
}

/// What [`execute_file`] returns, in a folder run, for a suite where
/// `--tags` / `--filter` select nothing (never a real exit code).
const NOTHING_SELECTED: i32 = -1;

async fn execute_file(mut args: RunArgs) -> anyhow::Result<i32> {
    crate::signals::install();
    // A new suite: the last one's cleanup is over, so a signal now asks
    // this one to stop and clean up rather than exiting at once.
    crate::signals::run_starting();
    println!(
        "{} Loading suite from {}",
        "=>".cyan().bold(),
        args.config.bold()
    );
    super::validate::print_unknown_config_keys(&args.config);
    let RunSetup {
        config: mut runtime_config,
        reports,
        cli_driver_mode,
    } = run_setup(&args)?;

    // Parse, validate, and resolve flows
    // Test lines go into the reports (JUnit `line`, GitHub annotations).
    let (mut suite, test_positions) =
        load_suite(&args.config, &args.set, &runtime_config.env_overrides)?;
    super::validate::print_suite_warnings(&suite, &args.config);

    let artifacts_dir = apply_output_flags(&args, &mut suite, &mut runtime_config);
    merge_shared_history(&args.history_from, &artifacts_dir)?;
    // Change-based selection needs flow usage, so it runs before flows are inlined.
    select_from_history(&args, &suite, &mut runtime_config, &artifacts_dir).await?;

    // Resolve flows (inline runFlow steps)
    suite.tests = resolve_flows(&suite)?;
    // GitHub masks the run's secrets in its own log too, and in later
    // steps' (the step summary, an upload step printing a path). After the
    // flows: a secret-named runFlow parameter is registered as they inline.
    if atalanta_runner::reporter::github::in_github_actions() {
        atalanta_runner::reporter::github::mask_secrets();
    }
    // App text printed during the run can't execute workflow commands;
    // the github report (or this guard's drop) resumes them (R-3).
    let _commands = atalanta_runner::reporter::github::suspend_commands();
    if let Err(e) = check_selection(&suite.tests, &args.tags, args.filter.as_deref()) {
        if !args.folder_member {
            return Err(e);
        }
        println!("{} {e}; skipping this suite", "=>".cyan().bold());
        return Ok(NOTHING_SELECTED);
    }
    let shard_plan = print_shard_plan(&suite.tests, &runtime_config);
    if let Some(why) = atalanta_runner::scheduler::empty_shard(&suite.tests, &runtime_config) {
        usage_bail!("{why}");
    }

    let target = resolve_target(&mut args, &mut suite, &mut runtime_config, cli_driver_mode)?;
    print_baseline_mode(runtime_config.update_baselines);
    let raw_driver = build_driver(target, &suite, args.parallel)?;
    let driver = share_driver(raw_driver, target, &suite, args.parallel).await;
    super::daemon::run_started(&runtime_config.stop);

    // Create artifacts dir
    let _ = std::fs::create_dir_all(&artifacts_dir);
    let run_began = std::time::SystemTime::now();
    if let Some(w) =
        atalanta_runner::artifacts::low_disk_warning(std::path::Path::new(&artifacts_dir))
    {
        eprintln!("{} {w}", "warning:".yellow().bold());
    }
    prepare_devices(&args, target, &suite, &*driver, &mut runtime_config).await?;

    if args.flaky_confidence.is_some() && args.parallel.is_none_or(|n| n <= 1) {
        println!(
            "{} --flaky-confidence copies tests across devices; it needs --parallel N (N ≥ 2). Ignored",
            "=>".yellow().bold()
        );
    }
    // Report properties, taken before the config moves into the run.
    let mut properties = report_properties(&runtime_config);
    let fail_on_flaky = args.fail_on_flaky
        || suite.config.retry.flaky_result == atalanta_common::test_types::FlakyResult::Fail;
    let Finished {
        outcome,
        hunt_code,
        hunt,
        partial,
    } = match run_suite(
        suite,
        runtime_config,
        &driver,
        args.parallel,
        atalanta_runner::hunt::HuntOptions {
            suggest: args.suggest,
            test_sources: test_positions.clone(),
        },
    )
    .await
    {
        Ok(f) => f,
        Err(e) => {
            // The devices still get cleaned up (CPU stress, tree server).
            end_of_run(&*driver, super::daemon::active()).await;
            return Err(e.into());
        }
    };

    let mut result = outcome.result;
    atalanta_core::parser::stamp_sources(&mut result, &test_positions);
    // No report, trace or summary keeps a secret (see atalanta_common::redact).
    atalanta_common::redact::redact_serde(&mut result);
    if args.profile_steps {
        print!("{}", step_profile(&result));
    }
    let exit = match &outcome.termination {
        atalanta_runner::runner::Termination::Completed => result.exit_code_with(fail_on_flaky),
        atalanta_runner::runner::Termination::TimedOut(e) => e.exit_code(),
        atalanta_runner::runner::Termination::Stopped(_) => crate::signals::exit_code(),
    };
    let termination = termination_label(&outcome.termination);
    properties.insert(0, ("atalanta.termination".into(), termination.clone()));
    let report_ctx = ReportContext {
        termination,
        partial,
        hunt,
        shard_plan,
        junit: atalanta_runner::reporter::junit::JunitOptions {
            properties,
            classname: junit_classname(args.junit_classname, &args.config),
        },
    };
    let completed = matches!(
        outcome.termination,
        atalanta_runner::runner::Termination::Completed
    );
    let exit = apply_quarantine(&args, &mut result, exit, completed, fail_on_flaky)?;
    let exit = write_reports(&reports, &args, &result, &artifacts_dir, &report_ctx, exit);
    prune_old_evidence(&artifacts_dir, run_began);

    // Cleanup platform services (the daemon keeps them for the next run).
    // A signal from here on exits at once: the run is over.
    end_of_run(&*driver, super::daemon::active()).await;

    // By severity, like suites: a hunt's flake verdict (1) is not hidden
    // by a last round's device failure (3).
    Ok(combined_exit(&[exit, hunt_code]))
}

/// What the flags alone decide, before the suite is loaded.
struct RunSetup {
    config: RuntimeConfig,
    /// `--report`, see [`parse_reports`].
    reports: Vec<String>,
    /// `--driver`: outranks the suite's `driver:`.
    cli_driver_mode: Option<DriverMode>,
}

/// Check the flags and build the run's config from them, before any
/// device work (VEL-166).
fn run_setup(args: &RunArgs) -> anyhow::Result<RunSetup> {
    // The parallel runner has no early flake detection yet: the flag was
    // silently ignored there, so new flaky tests merged green (N-2).
    if args.detect_new_flakes && args.parallel.is_some_and(|n| n > 1) {
        usage_bail!(
            "--detect-new-flakes doesn't work with --parallel yet: run early flake detection without --parallel"
        );
    }
    let (shard_index, shard_total) =
        shard_of(args.shard.as_deref(), args.shard_index, args.shard_total)?;
    if let Some(p) = args.flaky_confidence {
        if !(p > 0.0 && p < 1.0) {
            usage_bail!("--flaky-confidence must be between 0 and 1 (e.g. 0.99), got {p}");
        }
    }
    atalanta_runner::scheduler::check_shard(&RuntimeConfig {
        shard_index,
        shard_total,
        ..Default::default()
    })?;
    // Loaded once, here; a file that can't be read fails the run (exit 2)
    // instead of this shard quietly balancing by count while the others
    // use timings (D-23).
    let shard_timings_ms = match &args.shard_timings {
        Some(path) => Some(Arc::new(
            atalanta_runner::history::load_timings(std::path::Path::new(path))
                .map_err(|e| AtalantaError::Config(format!("--shard-timings: {e}")))?
                .durations,
        )),
        None => None,
    };
    let platform = parse_platform(args.platform.as_deref())?;
    let reports = parse_reports(&args.report)?;
    let report_format = if reports.first().is_some_and(|r| r == "json") {
        ReportFormat::Json
    } else {
        ReportFormat::Junit
    };
    let env_overrides = env_overrides(&args.env_files, &args.env_overrides)?;
    let update_baselines = match args.update_baselines {
        Some(mode) => mode,
        // Empty is off, as before D-V1 (review-d5 M-2).
        None => atalanta_common::BaselineUpdate::from_env(
            atalanta_common::env::var("ATALANTA_UPDATE_BASELINES")
                .ok()
                .as_deref(),
        )
        .map_err(|e| usage_err!("{e}"))?,
    };
    let cli_driver_mode = args.driver.as_deref().map(parse_driver_mode).transpose()?;

    let config = RuntimeConfig {
        config_path: args.config.clone(),
        platform,
        device_id: args.device.clone(),
        tags: args.tags.clone(),
        test_filter: args.filter.clone(),
        shard_index,
        shard_total,
        retry_count: args.retries,
        report_format,
        artifacts_dir: args.artifacts_dir.clone(),
        suite_timeout_ms: args.timeout.map(super::duration_arg::as_millis_u64),
        test_timeout_ms: args.test_timeout.map(super::duration_arg::as_millis_u64),
        fail_fast: args.fail_fast,
        failed_first: args.failed_first,
        shuffle_seed: (args.shuffle || args.hunt.is_some()).then(|| {
            args.seed.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(1)
            })
        }),
        hunt_rounds: args.hunt,
        jitter_ms: args.jitter.or(args.hunt.map(|_| 250)),
        stress: args.stress,
        only_tests: None,
        post_shard: None,
        detect_new_flakes: args.detect_new_flakes,
        cache_results: args.cache_results,
        env_overrides,
        driver_mode: cli_driver_mode.unwrap_or_default(),
        framework: Framework::default(),
        update_baselines,
        // Fresh per run, so a daemon's next run passes on a baseline this
        // one created (review-d5 M-1).
        created_baselines: Default::default(),
        trace: args.trace,
        verify: args.verify,
        flaky_confidence: args.flaky_confidence,
        shard_timings: args.shard_timings.clone(),
        shard_timings_ms,
        stop: Default::default(),
        // The runner takes its own start (a hunt sets one for all rounds).
        run_started: None,
        state_dir: None,
    };
    Ok(RunSetup {
        config,
        reports,
        cli_driver_mode,
    })
}

/// `--platform`.
fn parse_platform(value: Option<&str>) -> anyhow::Result<Option<Platform>> {
    Ok(match value {
        Some("ios") => Some(Platform::Ios),
        Some("android") => Some(Platform::Android),
        Some("web") => Some(Platform::Web),
        Some(other) => usage_bail!("Unknown platform '{other}'. Use 'ios', 'android', or 'web'."),
        None => None,
    })
}

/// `--report` checked when the command line is parsed, so a typo fails
/// before any device work and `--help` names the formats.
fn check_reports(value: &str) -> Result<String, String> {
    for r in value.split(',').map(|r| r.trim().to_lowercase()) {
        if !matches!(r.as_str(), "junit" | "json" | "html" | "github" | "allure") {
            return Err(format!(
                "unknown report format '{r}': use junit, json, html, github or allure (comma-separated)"
            ));
        }
    }
    Ok(value.to_string())
}

/// `--report`: each format once, in the order given, plus `github` in
/// GitHub Actions.
fn parse_reports(value: &str) -> anyhow::Result<Vec<String>> {
    let mut reports: Vec<String> = Vec::new();
    for r in value.split(',').map(|r| r.trim().to_lowercase()) {
        match r.as_str() {
            "junit" | "json" | "html" | "github" | "allure" => {
                if !reports.contains(&r) {
                    reports.push(r)
                }
            }
            "" => {}
            other => usage_bail!(
                "Unknown report format '{other}'. Use junit, json, html, github or allure (comma-separated)."
            ),
        }
    }
    if atalanta_runner::reporter::github::in_github_actions()
        && !reports.iter().any(|r| r == "github")
    {
        reports.push("github".into());
    }
    Ok(reports)
}

/// `--video`, `--logs` and `--pause-on-failure` onto the suite, and the
/// run's artifacts folder, which this returns.
fn apply_output_flags(
    args: &RunArgs,
    suite: &mut atalanta_common::TestSuite,
    runtime_config: &mut RuntimeConfig,
) -> String {
    if let Some(v) = args.video {
        suite.config.artifacts.video = v;
    }
    if let Some(l) = args.logs {
        suite.config.artifacts.logs = l;
    }
    if args.pause_on_failure {
        use std::io::IsTerminal;
        if args.daemon {
            println!(
                "{} --pause-on-failure is ignored with --daemon (the daemon has no terminal)",
                "warning:".yellow().bold()
            );
        } else if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            atalanta_core::executor::set_pause_on_failure(true);
        } else {
            println!(
                "{} --pause-on-failure needs an interactive terminal; ignored",
                "warning:".yellow().bold()
            );
        }
    }
    // One run, one artifacts folder. Reports and history followed
    // `--artifacts-dir` while failure screenshots, traces and diffs followed
    // the suite's `artifacts.outputDir`, so a run was split across two
    // folders. An explicit flag wins; otherwise a folder the suite names is
    // used for everything.
    let default_dir = "./atalanta-results";
    if args.artifacts_dir != default_dir {
        suite.config.artifacts.output_dir = args.artifacts_dir.clone();
    } else if suite.config.artifacts.output_dir != default_dir {
        runtime_config.artifacts_dir = suite.config.artifacts.output_dir.clone();
    }
    // Reports, `--last-failed` and impact selection use it too: they read
    // the CLI default while the history went to `outputDir` (VEL-033).
    runtime_config.artifacts_dir.clone()
}

/// `--history-from`: history other CI jobs uploaded (C1), merged in before
/// anything reads the history: the scheduler, --last-failed, --changed-since.
fn merge_shared_history(history_from: &[String], artifacts_dir: &str) -> anyhow::Result<()> {
    if history_from.is_empty() {
        return Ok(());
    }
    let (files, unmatched) = atalanta_runner::shared_history::seed(artifacts_dir, history_from)
        .map_err(|e| usage_err!("--history-from: {e}"))?;
    for p in &unmatched {
        println!(
            "{} --history-from {p}: no history file matches (a first run has none)",
            "warning:".yellow().bold()
        );
    }
    if !files.is_empty() {
        println!(
            "{} Merged {} history file(s) from other jobs into {artifacts_dir}",
            "=>".cyan().bold(),
            files.len()
        );
    }
    Ok(())
}

/// `--changed-since` and `--last-failed`. What git and the impact map pick
/// narrows the suite before sharding; what the local history adds is
/// applied after it (R-1).
async fn select_from_history(
    args: &RunArgs,
    suite: &atalanta_common::TestSuite,
    runtime_config: &mut RuntimeConfig,
    artifacts_dir: &str,
) -> anyhow::Result<()> {
    let mut post_shard: Option<std::collections::HashSet<String>> = None;
    if let Some(base) = &args.changed_since {
        let picked = select_impacted(suite, base, args, artifacts_dir).await?;
        runtime_config.only_tests = Some(picked.git);
        post_shard = Some(picked.history.into_iter().collect());
    }
    if args.last_failed {
        let hist = atalanta_runner::history::load(artifacts_dir).unwrap_or_default();
        let failed: Vec<String> = suite
            .tests
            .iter()
            .filter(|t| hist.failed_last(&t.name))
            .map(|t| t.name.clone())
            .collect();
        if failed.is_empty() {
            println!(
                "{} No failures in the last run; running every test",
                "=>".cyan().bold()
            );
        } else {
            println!(
                "{} Re-running {} test(s) that failed last run",
                "=>".cyan().bold(),
                failed.len()
            );
            // With --changed-since: both selections, not the last one
            // given (N-10).
            post_shard
                .get_or_insert_with(Default::default)
                .extend(failed);
        }
    }
    runtime_config.post_shard = post_shard.map(Arc::new);
    Ok(())
}

/// Every shard prints the same `plan=` when they agree on the partition
/// (the same tests, filters and timings): CI can compare them (D-23). The
/// same plan goes into `results.json` (`shard_plan`), where
/// `atalanta results check` compares every shard's.
fn print_shard_plan(
    tests: &[atalanta_common::TestCase],
    runtime_config: &RuntimeConfig,
) -> Option<atalanta_runner::reporter::json::ShardPlanRecord> {
    let (Some(i), Some(n)) = (runtime_config.shard_index, runtime_config.shard_total) else {
        return None;
    };
    let plan = atalanta_runner::scheduler::shard_plan(tests, runtime_config);
    let mine = atalanta_runner::scheduler::plan_tests(
        tests,
        runtime_config,
        &atalanta_runner::TestHistory::default(),
    )
    .len();
    let planned: usize = plan.shards.iter().map(Vec::len).sum();
    println!(
        "{} Shard {}/{n}: {mine} of {planned} tests; plan={} ({})",
        "=>".cyan().bold(),
        i + 1,
        plan.fingerprint,
        if plan.by_timings {
            "balanced by timings"
        } else {
            "balanced by test count"
        }
    );
    Some(atalanta_runner::reporter::json::ShardPlanRecord {
        fingerprint: plan.fingerprint.clone(),
        tests: plan.shards.get(i).cloned().unwrap_or_default(),
        planned_total: planned,
    })
}

/// What a run drives: resolved once the suite is loaded and selected.
#[derive(Clone, Copy)]
struct Target {
    platform: Platform,
    mode: DriverMode,
    framework: Framework,
    /// The selected tests have only http steps (with wait/setConfig): no
    /// device to find, lease, install on or prepare.
    http_only: bool,
}

/// Resolve the platform (written into the suite), the driver mode (CLI >
/// YAML > device) and the framework (CLI > YAML > auto-detect), check the
/// suite against them and resolve `app:` / `--app`. An http-only run drops
/// `--device`.
fn resolve_target(
    args: &mut RunArgs,
    suite: &mut atalanta_common::TestSuite,
    runtime_config: &mut RuntimeConfig,
    cli_driver_mode: Option<DriverMode>,
) -> anyhow::Result<Target> {
    let http_only = atalanta_core::http_only_tests(&atalanta_runner::scheduler::plan_tests(
        &suite.tests,
        runtime_config,
        &atalanta_runner::TestHistory::default(),
    ));
    if http_only && args.device.is_some() {
        println!(
            "{} --device ignored: the selected tests have only http steps",
            "=>".yellow().bold()
        );
        args.device = None;
        runtime_config.device_id = None;
    }

    let effective_platform =
        resolve_platform(suite, runtime_config.platform, args.device.as_deref());

    if http_only {
        println!(
            "{} Platform: {}",
            "=>".cyan().bold(),
            "none (http steps only)".green()
        );
    } else {
        println!(
            "{} Platform: {}",
            "=>".cyan().bold(),
            effective_platform.to_string().green()
        );
    }
    atalanta_core::validator::check_platform_selectors(suite, effective_platform)?;

    let effective_driver_mode = match cli_driver_mode {
        Some(mode) => mode,
        None => suite.config.driver.unwrap_or(DriverMode::Device),
    };
    suite.config.driver = Some(effective_driver_mode);
    // VEL-219 with the platform and driver this run uses (review-s9 M2).
    atalanta_core::check_capabilities(suite)?;

    // `app:` / `--app`: the build the runners install on each device.
    if http_only {
        suite.config.app = None;
    } else {
        super::provision::resolve_app(
            suite,
            args.app.as_deref(),
            &args.config,
            effective_platform,
            effective_driver_mode == DriverMode::Device,
        )?;
    }

    let cli_framework = args.framework.as_deref().map(parse_framework).transpose()?;
    let effective_framework = match cli_framework {
        Some(fw) => fw,
        None => suite.config.framework.unwrap_or_else(|| {
            // Auto-detect from app path
            let app_path = suite
                .config
                .headless
                .as_ref()
                .and_then(|h| h.app_path.as_deref());
            detect_framework(app_path, effective_platform)
        }),
    };
    runtime_config.framework = effective_framework;
    if effective_framework != Framework::Native {
        println!(
            "{} Framework: {}",
            "=>".cyan().bold(),
            effective_framework.to_string().yellow()
        );
    }
    Ok(Target {
        platform: effective_platform,
        mode: effective_driver_mode,
        framework: effective_framework,
        http_only,
    })
}

fn print_baseline_mode(mode: atalanta_common::BaselineUpdate) {
    match mode {
        atalanta_common::BaselineUpdate::Changed => println!(
            "{} {} Baseline update mode — missing and changed snapshots will be overwritten",
            "=>".cyan().bold(),
            "WARNING:".yellow().bold()
        ),
        atalanta_common::BaselineUpdate::Missing => println!(
            "{} Baseline update mode — missing snapshots will be written",
            "=>".cyan().bold()
        ),
        atalanta_common::BaselineUpdate::Off => {}
    }
}

/// The platform driver for `target`; [`share_driver`] wraps it in the
/// resilience layer.
fn build_driver(
    target: Target,
    suite: &atalanta_common::TestSuite,
    parallel: Option<usize>,
) -> anyhow::Result<Arc<dyn PlatformDriver>> {
    if target.mode == DriverMode::Headless && target.platform == Platform::Web {
        usage_bail!(
            "Platform 'web' does not support '--driver headless'. \
            Headless mode is for native/RN/Flutter UI. Omit --driver or use 'device'."
        );
    }
    if target.http_only {
        return Ok(Arc::new(atalanta_api::NoDeviceDriver::new(
            parallel.unwrap_or(1),
            target.platform,
        )));
    }
    Ok(match target.mode {
        DriverMode::Headless => headless_driver(target, suite)?,
        DriverMode::Device => match target.platform {
            Platform::Ios => Arc::new(atalanta_ios::IosDriver::new()),
            Platform::Android => Arc::new(atalanta_android::AndroidDriver::new()),
            Platform::Web => web_driver(suite, parallel)?,
        },
    })
}

/// `--driver headless`: the Flutter bridge, or Atalanta's own renderer.
fn headless_driver(
    target: Target,
    suite: &atalanta_common::TestSuite,
) -> anyhow::Result<Arc<dyn PlatformDriver>> {
    Ok(match target.framework {
        // D-44/45: the RN headless sidecar (a stub) was removed.
        // Reached when `--framework`/auto-detection picks RN after
        // `check_capabilities` ran on the YAML.
        Framework::ReactNative => {
            usage_bail!("{}", atalanta_core::validator::RN_NO_HEADLESS)
        }
        Framework::Flutter => {
            // headless.width/height/devicePixelRatio, the flutter:
            // block, semantics by default (D-15) and the 3.19 SDK
            // floor (D-41): a config error, exit 2.
            let (flutter_config, warnings) =
                atalanta_flutter_bridge::FlutterBridgeConfig::from_suite(&suite.config)
                    .map_err(|e| usage_err!("{e}"))?;
            for w in warnings {
                eprintln!("{} {w}", "warning:".yellow().bold());
            }
            println!(
                "{} Driver: {} ({}x{}, port {})",
                "=>".cyan().bold(),
                "flutter headless".yellow(),
                flutter_config.width,
                flutter_config.height,
                flutter_config.port,
            );
            Arc::new(atalanta_flutter_bridge::FlutterDriver::new(
                target.platform,
                flutter_config,
            ))
        }
        Framework::Native => {
            // headless.width/height/density (or devicePixelRatio,
            // D-46)/fontScale (D-47)/appPath/initialLayout.
            let headless_config =
                atalanta_headless::HeadlessConfig::from_yaml(suite.config.headless.as_ref())
                    .map_err(|e| usage_err!("{e}"))?;
            println!(
                "{} Driver: {} ({}x{})",
                "=>".cyan().bold(),
                "headless".yellow(),
                headless_config.width,
                headless_config.height
            );
            Arc::new(atalanta_headless::HeadlessDriver::new(
                target.platform,
                headless_config,
            ))
        }
    })
}

/// Chrome over CDP, set up from the suite's `web:` section.
fn web_driver(
    suite: &atalanta_common::TestSuite,
    parallel: Option<usize>,
) -> anyhow::Result<Arc<dyn PlatformDriver>> {
    let mut web_config = atalanta_web::WebDriverConfig {
        // `--parallel N`: N browsers, `web-0` … `web-{N-1}` (D-48).
        instances: web_instances(parallel),
        ..Default::default()
    };
    if let Some(ref wc) = suite.config.web {
        // A device profile first: `width`/`height` override its
        // viewport (D-W3).
        if let Some(ref name) = wc.device {
            web_config.apply_device(atalanta_common::web_devices::lookup(name)?);
        }
        if let Some(ref url) = wc.start_url {
            web_config.start_url = Some(url.clone());
        }
        if let Some(w) = wc.width {
            web_config.width = w;
        }
        if let Some(h) = wc.height {
            web_config.height = h;
        }
        if let Some(ref p) = wc.chrome_path {
            web_config.chrome_path = Some(p.clone());
        }
        if let Some(hl) = wc.headless {
            web_config.headless = hl;
        }
        if let Some(mode) = wc.parallel_mode {
            web_config.parallel_mode = mode;
        }
        web_config.extra_args = wc.extra_args.clone();
        web_config.mocks = wc.mocks.clone();
        web_config
            .mocks
            .extend(wc.block.iter().map(|url| atalanta_common::MockRule {
                url: url.clone(),
                block: true,
                ..Default::default()
            }));
    }
    println!(
        "{} Driver: {} ({}x{}, headless={}{})",
        "=>".cyan().bold(),
        "chrome-cdp".yellow(),
        web_config.width,
        web_config.height,
        web_config.headless,
        match (web_config.instances, web_config.parallel_mode) {
            (1, _) => String::new(),
            (n, atalanta_web::driver::ParallelMode::Processes) => {
                format!(", {n} browsers")
            }
            (n, atalanta_web::driver::ParallelMode::Contexts) => {
                format!(", {n} browsers (contexts of one Chrome)")
            }
        } + &web_config
            .device
            .map(|d| format!(", device={}", d.name))
            .unwrap_or_default(),
    );
    Ok(Arc::new(atalanta_web::WebDriver::new(web_config)))
}

/// `raw_driver` in the resilience layer: automatic retries on transient
/// failures, circuit breaking on sustained ones. Inside `atalanta daemon`
/// the driver (and its warm sessions) outlives the run; a no-device one has
/// nothing to keep warm.
async fn share_driver(
    raw_driver: Arc<dyn PlatformDriver>,
    target: Target,
    suite: &atalanta_common::TestSuite,
    parallel: Option<usize>,
) -> Arc<ResilientDriver> {
    if target.http_only {
        return Arc::new(ResilientDriver::new(raw_driver));
    }
    super::daemon::share(
        super::daemon::pool_key(
            target.platform,
            target.mode,
            target.framework,
            &serde_json::json!([
                driver_config(suite, target.platform, target.mode, target.framework),
                // A pooled web driver has as many browsers as it was made
                // with: a `--parallel 4` run can't use a one-browser one.
                (target.platform == Platform::Web).then(|| web_instances(parallel)),
            ]),
        ),
        Arc::new(ResilientDriver::new(raw_driver)),
    )
    .await
}

/// Get the devices ready: `--provision` boots missing ones, `--device` is
/// checked, and the device to bootstrap is picked and prepared. A
/// sequential run then uses that same device.
async fn prepare_devices(
    args: &RunArgs,
    target: Target,
    suite: &atalanta_common::TestSuite,
    driver: &dyn PlatformDriver,
    runtime_config: &mut RuntimeConfig,
) -> anyhow::Result<()> {
    // `--provision` boots missing devices while the app build is hashed.
    super::provision::before_run(
        args.provision && args.device.is_none() && !target.http_only,
        target.platform,
        args.parallel.unwrap_or(1).max(1),
        driver,
        atalanta_runner::install::build_for(suite, Some(target.platform)),
    )
    .await?;

    // `--device` names a device the driver can see, checked now: a typo
    // used to fail later as a device error deep in the run (D-25).
    let mut device = args.device.clone();
    if let (Some(wanted), DriverMode::Device, false) = (
        args.device.as_deref(),
        target.mode,
        target.platform == Platform::Web,
    ) {
        if let Some(id) =
            check_devices_exist(wanted, driver, args.parallel.is_some_and(|n| n > 1)).await?
        {
            println!("{} Device {wanted} is {id}", "=>".cyan().bold());
            runtime_config.device_id = Some(id.clone());
            device = Some(id);
        }
    }

    let boot_device_id = boot_device(device.as_deref(), driver).await?;
    if runtime_config.device_id.is_none()
        && !boot_device_id.is_empty()
        && args.parallel.is_none_or(|n| n <= 1)
    {
        runtime_config.device_id = Some(boot_device_id.clone());
    }
    if !boot_device_id.is_empty() {
        prepare_under_lease(driver, &boot_device_id, target).await?;
    }
    Ok(())
}

/// The device to bootstrap (empty if there is none): a booted one nobody
/// else holds. The sequential runner then uses this same device (it used to
/// pick its own, which could differ, and `first()` could be a shut-down
/// simulator).
async fn boot_device(device: Option<&str>, driver: &dyn PlatformDriver) -> anyhow::Result<String> {
    Ok(match device {
        // A list (`--parallel 2 --device simA,simB`): bootstrap the first;
        // the parallel runner prepares each device it leases.
        Some(id) => first_device(id).to_string(),
        None => {
            let mut devices = driver.list_devices().await?;
            devices.sort_by_key(|d| d.state != atalanta_common::DeviceState::Booted);
            devices
                .iter()
                .find(|d| atalanta_runner::lease::try_lock(&d.id).is_ok())
                .or(devices.first())
                .map(|d| d.id.clone())
                .unwrap_or_default()
        }
    })
}

/// Bootstrap the platform (e.g. auto-start WDA on iOS), under the device's
/// lease: preparing a device another Atalanta process holds stopped its
/// WebDriverAgent and started another runner on its port. The runner
/// takes the lease again for the run itself.
async fn prepare_under_lease(
    driver: &dyn PlatformDriver,
    boot_device_id: &str,
    target: Target,
) -> anyhow::Result<()> {
    let prepare_lease = match atalanta_runner::lease::lock_waiting(boot_device_id).await {
        Ok(lock) => lock,
        Err(atalanta_runner::lease::LeaseError::Held(holder)) => {
            return Err(atalanta_common::AtalantaError::ServiceUnavailable {
                service: format!("Device {boot_device_id}"),
                detail: format!(
                    "in use by another Atalanta process ({holder}). \
                     Wait for it to finish or pick another device with --device."
                ),
            }
            .into());
        }
        Err(atalanta_runner::lease::LeaseError::Io(e)) => {
            return Err(atalanta_common::AtalantaError::ServiceUnavailable {
                service: format!("Device {boot_device_id}"),
                detail: e,
            }
            .into());
        }
    };
    if !target.http_only {
        println!(
            "{} Preparing {} driver...",
            "=>".cyan().bold(),
            target.platform.to_string().green()
        );
    }
    driver.prepare(boot_device_id).await?;
    drop(prepare_lease);
    Ok(())
}

/// The JUnit properties of the run's shard and shuffle seed.
fn report_properties(runtime_config: &RuntimeConfig) -> Vec<(String, String)> {
    let mut properties: Vec<(String, String)> = Vec::new();
    if let (Some(i), Some(n)) = (runtime_config.shard_index, runtime_config.shard_total) {
        properties.push(("atalanta.shard".into(), format!("{}/{n}", i + 1)));
    }
    if let Some(seed) = runtime_config.shuffle_seed {
        properties.push(("atalanta.seed".into(), seed.to_string()));
    }
    properties
}

/// How [`run_suite`] ended.
struct Finished {
    outcome: atalanta_runner::runner::RunOutcome,
    /// A hunt's verdict; 0 for any other run.
    hunt_code: i32,
    /// A hunt's flaky tests and their divergence, for the JSON report.
    hunt: Option<serde_json::Value>,
    /// The stopped run didn't return within [`STOP_GRACE`]: the results
    /// are what had finished by then (C2).
    partial: bool,
}

/// Run the suite: a hunt, the parallel runner or the sequential one.
/// Ctrl-C / SIGTERM stop the run but still clean up devices (tree server,
/// WDA, CPU stress, animation settings) before exiting.
async fn run_suite(
    suite: atalanta_common::TestSuite,
    runtime_config: RuntimeConfig,
    driver: &Arc<ResilientDriver>,
    parallel: Option<usize>,
    hunt_options: atalanta_runner::hunt::HuntOptions,
) -> Result<Finished, AtalantaError> {
    let stop = runtime_config.stop.clone();
    let runtime_shard = (runtime_config.shard_index, runtime_config.shard_total);
    let run = async {
        if runtime_config.hunt_rounds.is_some() {
            let report = match parallel.filter(|&n| n > 1) {
                Some(n) => {
                    let shared: Arc<dyn PlatformDriver> = driver.clone();
                    atalanta_runner::hunt::hunt_parallel_with(
                        suite,
                        runtime_config,
                        shared,
                        n,
                        &hunt_options,
                    )
                    .await?
                }
                None => {
                    Box::pin(atalanta_runner::hunt::hunt_with(
                        suite,
                        runtime_config,
                        &**driver,
                        &hunt_options,
                    ))
                    .await?
                }
            };
            let code = report.exit_code();
            let hunt = Some(report.json());
            Ok::<_, atalanta_common::AtalantaError>((
                atalanta_runner::runner::RunOutcome {
                    result: report.last,
                    termination: match stop.reason() {
                        Some(r) => atalanta_runner::runner::Termination::Stopped(r.to_string()),
                        None => atalanta_runner::runner::Termination::Completed,
                    },
                },
                code,
                hunt,
            ))
        } else if let Some(n) = parallel.filter(|&n| n > 1) {
            let shared: Arc<dyn PlatformDriver> = driver.clone();
            atalanta_runner::ParallelRunner::run_outcome(suite, runtime_config, shared, n)
                .await
                .map(|o| (o, 0, None))
        } else {
            Box::pin(SuiteRunner::run_outcome(suite, runtime_config, &**driver))
                .await
                .map(|o| (o, 0, None))
        }
    };
    atalanta_runner::live::reset();
    tokio::pin!(run);
    let seen = crate::signals::count();
    let mut partial = false;
    let outcome = tokio::select! {
        r = &mut run => r,
        signal = crate::signals::after(seen) => {
            // The runner stops at once and returns what ran, so the reports
            // still get written, the tests not run marked skipped (VEL-188).
            eprintln!(
                "\n{} {signal} received — stopping, writing reports and cleaning up devices (again to skip)",
                "=>".yellow().bold()
            );
            stop.stop(format!("interrupted by {signal}"));
            match tokio::time::timeout(STOP_GRACE, &mut run).await {
                Ok(r) => r,
                Err(_) => {
                    // The run is stuck (a driver call that can't be
                    // cancelled): report what finished so far instead of
                    // nothing (C2, N-9). Cleanup still runs below.
                    let why = format!(
                        "interrupted by {signal} (the run did not stop within {}s)",
                        STOP_GRACE.as_secs()
                    );
                    eprintln!(
                        "{} the run did not stop within {}s; writing partial reports",
                        "=>".yellow().bold(),
                        STOP_GRACE.as_secs()
                    );
                    partial = true;
                    let mut result = atalanta_runner::live::partial(&why);
                    result.shard_index = runtime_shard.0;
                    result.shard_total = runtime_shard.1;
                    Ok((
                        atalanta_runner::runner::RunOutcome {
                            result,
                            termination: atalanta_runner::runner::Termination::Stopped(why),
                        },
                        0,
                        None,
                    ))
                }
            }
        }
    };
    let (outcome, hunt_code, hunt) = outcome?;
    Ok(Finished {
        outcome,
        hunt_code,
        hunt,
        partial,
    })
}

/// How the run ended, in the reports (D-20): `completed`, `timeout`,
/// `interrupted:SIGTERM`.
fn termination_label(termination: &atalanta_runner::runner::Termination) -> String {
    match termination {
        atalanta_runner::runner::Termination::Completed => "completed".to_string(),
        atalanta_runner::runner::Termination::TimedOut(_) => "timeout".to_string(),
        atalanta_runner::runner::Termination::Stopped(_) => match crate::signals::first() {
            Some(sig) => format!("interrupted:{}", sig.sig_name()),
            None => "interrupted".to_string(),
        },
    }
}

/// Known failures from the quarantine list don't fail the run — only
/// when every failure is one of them. The run's exit code after the list.
fn apply_quarantine(
    args: &RunArgs,
    result: &mut atalanta_common::SuiteResult,
    exit: i32,
    completed: bool,
    fail_on_flaky: bool,
) -> anyhow::Result<i32> {
    let quarantine_path = args
        .quarantine
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(&args.config)
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(atalanta_runner::quarantine::FILE_NAME)
        });
    let Some(q) = atalanta_runner::quarantine::Quarantine::load(&quarantine_path)? else {
        return Ok(exit);
    };
    let verdict = q.apply(result);
    for (test, date) in &verdict.expired {
        println!(
            "{} quarantine entry for '{test}' expired on {date}; it no longer forgives anything (fix the test, or renew the entry in {})",
            "warning:".yellow().bold(),
            quarantine_path.display()
        );
    }
    for (test, reason) in &verdict.forgiven {
        println!(
            "  {} {test} failed as known ({reason})",
            "quarantined".yellow()
        );
    }
    if verdict.all_forgiven() && completed {
        // A flaky pass still fails a `--fail-on-flaky` run (D-21),
        // unless the list knows it (A-11).
        let exit = SuiteResultExt::flaky_exit(&*result, fail_on_flaky);
        println!(
            "{} every failure is quarantined; exiting {exit}",
            "=>".cyan().bold()
        );
        return Ok(exit);
    }
    if verdict.unforgiven == 0 && completed && exit == 1 && fail_on_flaky {
        // Only flaky passes failed the run: the known ones don't (A-11).
        return Ok(SuiteResultExt::flaky_exit(&*result, fail_on_flaky));
    }
    Ok(exit)
}

/// Write every report in `reports`: one that can't be written no longer
/// skips the others and the device cleanup (VEL-034). `exit` combined with
/// 5 for each that failed.
/// `ATALANTA_ARTIFACTS_MAX_BYTES`: drop the oldest evidence of earlier
/// runs (traces, screenshots) once the artifacts dir is over the cap.
fn prune_old_evidence(artifacts_dir: &str, run_began: std::time::SystemTime) {
    let Some(max) = atalanta_runner::artifacts::max_evidence_bytes() else {
        return;
    };
    let pruned = atalanta_runner::artifacts::prune_evidence(
        std::path::Path::new(artifacts_dir),
        max,
        run_began,
    );
    if pruned.removed > 0 {
        println!(
            "{} Removed {} old evidence item(s) ({:.1} MB) to keep {artifacts_dir} under ATALANTA_ARTIFACTS_MAX_BYTES",
            "=>".cyan().bold(),
            pruned.removed,
            pruned.freed as f64 / (1u64 << 20) as f64
        );
    }
}

fn write_reports(
    reports: &[String],
    args: &RunArgs,
    result: &atalanta_common::SuiteResult,
    artifacts_dir: &str,
    report_ctx: &ReportContext,
    exit: i32,
) -> i32 {
    let mut report_codes = vec![exit];
    for format in reports {
        if format == "allure" && args.clean_allure {
            atalanta_runner::reporter::allure::clean_dir(&format!(
                "{artifacts_dir}/allure-results"
            ));
        }
        match write_report(format, result, artifacts_dir, &args.config, report_ctx) {
            Ok(Some(path)) => {
                println!("{} Report: {}", "=>".cyan().bold(), path.underline())
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "{} {format} report: {}",
                    "error:".red().bold(),
                    atalanta_common::redact::redact(&e.to_string())
                );
                // Atalanta couldn't write its output (a full disk, a bad
                // path): an internal error, 5, never "config" (D-20).
                report_codes.push(5);
            }
        }
    }
    // Row 2.2: failures.md on every run, whatever --report says: each
    // failure with its step, error, log tail and the files that show it.
    match atalanta_runner::reporter::failures::write(result, artifacts_dir) {
        Ok(path)
            if result
                .tests
                .iter()
                .any(|t| t.status == atalanta_common::TestStatus::Failed) =>
        {
            println!("{} Failures: {}", "=>".cyan().bold(), path.underline())
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("{} failures.md: {e}", "error:".red().bold());
            report_codes.push(5);
        }
    }
    combined_exit(&report_codes)
}

/// The end of a run: clean the devices up, or under `atalanta daemon`
/// (which keeps its drivers and services for the next run) only undo
/// what must not outlive the run: the frozen status bar (review-d5 m-4).
async fn end_of_run(driver: &dyn atalanta_common::PlatformDriver, daemon: bool) {
    if daemon {
        driver.unfreeze_status_bar().await;
    } else {
        crate::signals::cleaning_up();
        driver.cleanup().await;
    }
}

/// What the reports say about the run besides its results.
struct ReportContext {
    termination: String,
    /// The run was dropped after the stop grace: the results are what
    /// had finished by then (C2).
    partial: bool,
    /// A hunt's verdicts: the JSON report's `hunt` field.
    hunt: Option<serde_json::Value>,
    /// A sharded run's partition: the JSON report's `shard_plan` field.
    shard_plan: Option<atalanta_runner::reporter::json::ShardPlanRecord>,
    junit: atalanta_runner::reporter::junit::JunitOptions,
}

/// The exit code of a run whose failures are all forgiven.
trait SuiteResultExt {
    fn flaky_exit(&self, fail_on_flaky: bool) -> i32;
}

impl SuiteResultExt for atalanta_common::SuiteResult {
    fn flaky_exit(&self, fail_on_flaky: bool) -> i32 {
        // A flaky test the quarantine list knows doesn't count (A-11).
        let flaky = self
            .tests
            .iter()
            .any(|t| t.status == atalanta_common::TestStatus::Retried && t.quarantined.is_none());
        i32::from(fail_on_flaky && flaky)
    }
}

/// Write one report; the path written, if it is a file.
fn write_report(
    format: &str,
    result: &atalanta_common::SuiteResult,
    artifacts_dir: &str,
    suite_file: &str,
    ctx: &ReportContext,
) -> atalanta_common::Result<Option<String>> {
    let path = match format {
        "junit" => {
            let path = format!("{artifacts_dir}/junit.xml");
            atalanta_runner::reporter::junit::write_junit_with(result, &path, &ctx.junit)?;
            path
        }
        "json" => {
            let path = format!("{artifacts_dir}/results.json");
            atalanta_runner::reporter::json::write_json_report(
                result,
                &path,
                &atalanta_runner::reporter::json::JsonExtras {
                    termination: Some(&ctx.termination),
                    hunt: ctx.hunt.as_ref(),
                    shard_plan: ctx.shard_plan.as_ref(),
                },
            )?;
            // The farm view beside it (per-device busy time, retries).
            // Best effort: the results above are what CI depends on.
            let metrics = format!("{artifacts_dir}/run-metrics.json");
            if let Err(e) =
                atalanta_runner::reporter::metrics::write(result, result.duration, &metrics)
            {
                tracing::warn!("{e}");
            }
            path
        }
        "html" => {
            let path = format!("{artifacts_dir}/report.html");
            atalanta_runner::write_html(result, &path)?;
            path
        }
        "github" => {
            atalanta_runner::reporter::github::report_with(
                result,
                suite_file,
                &atalanta_runner::reporter::github::RunEnd {
                    termination: ctx.termination.clone(),
                    partial: ctx.partial,
                },
            )?;
            return Ok(None);
        }
        "allure" => {
            let path = format!("{artifacts_dir}/allure-results");
            let suite = std::path::Path::new(suite_file)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "atalanta".into());
            atalanta_runner::reporter::allure::write_allure(result, &path, &suite)?;
            path
        }
        _ => return Ok(None),
    };
    Ok(Some(path))
}

/// How long a stopped run gets to return its results before it is
/// dropped (cleanup still runs).
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// `--tags` / `--filter` that match no test are a usage error (exit 2): a
/// typo used to run nothing and pass. The message lists the suite's tags.
fn check_selection(
    tests: &[atalanta_common::TestCase],
    tags: &[String],
    filter: Option<&str>,
) -> anyhow::Result<()> {
    if tags.is_empty() && filter.is_none() {
        return Ok(());
    }
    let mut picked = atalanta_runner::filter_by_tags(tests, tags);
    if let Some(pattern) = filter {
        picked = atalanta_runner::filter_by_name(&picked, pattern);
    }
    if !picked.is_empty() || tests.is_empty() {
        return Ok(());
    }
    let mut known: Vec<&str> = tests
        .iter()
        .flat_map(|t| t.tags.iter().map(String::as_str))
        .collect();
    known.sort_unstable();
    known.dedup();
    let mut asked = Vec::new();
    if !tags.is_empty() {
        asked.push(format!("--tags {}", tags.join(",")));
    }
    if let Some(f) = filter {
        asked.push(format!("--filter {f}"));
    }
    usage_bail!(
        "no test matches {} (the suite's tags: {})",
        asked.join(" "),
        if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        }
    )
}

/// Whether `path` (from the repository root) is Atalanta's own output: its
/// `.atalanta/` state or the run's artifacts directory.
fn is_atalanta_output(path: &str, artifacts_prefix: &str) -> bool {
    path.split('/').any(|part| part == ".atalanta") || path.starts_with(artifacts_prefix)
}

/// Run `git` with `args` (under a deadline: a wedged fsmonitor hook or
/// credential prompt hung the run, VEL-352) and split its `-z` output on
/// NUL. Paths come back as they are, never C-quoted (VEL-035).
async fn git_z(args: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["-c", "core.quotePath=false"]).args(args);
    let out =
        atalanta_common::process::output_with_timeout(&mut cmd, std::time::Duration::from_secs(60))
            .await
            .map_err(|e| usage_err!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        usage_bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(split_z(&out.stdout))
}

/// `-z` output: NUL-terminated entries (a trailing newline for commands
/// that ignore `-z`, like `rev-parse`).
fn split_z(out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    let parts: Vec<&str> = if out.contains(&0) {
        text.split('\0').collect()
    } else {
        text.lines().collect()
    };
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// A `--changed-since` value git can only read as a commit: one starting
/// with `-` would be taken as an option (VEL-036).
fn check_base(base: &str) -> anyhow::Result<()> {
    if base.is_empty() || base.starts_with('-') {
        usage_bail!("--changed-since {base:?}: expected a git commit, branch or tag");
    }
    Ok(())
}

/// What `--changed-since` picked: from git and the impact map (the same on
/// every shard), and from the local history (applied after sharding, R-1).
struct Impacted {
    git: Vec<String>,
    history: Vec<String>,
}

/// The commit this branch forked from `base` at (`git merge-base`). In a
/// shallow clone there is often none: say how to fetch the history.
async fn merge_base(base: &str) -> anyhow::Result<String> {
    if let Ok(found) = git_z(&["merge-base", "--end-of-options", base, "HEAD"]).await {
        if let Some(commit) = found.into_iter().next() {
            return Ok(commit);
        }
    }
    let shallow = git_z(&["rev-parse", "--is-shallow-repository"])
        .await
        .ok()
        .and_then(|o| o.into_iter().next())
        .is_some_and(|s| s.trim() == "true");
    if shallow {
        usage_bail!(
            "--changed-since {base}: this is a shallow clone, so git can't find where HEAD forked from {base}. \
             Fetch the full history: `fetch-depth: 0` on actions/checkout, or `git fetch --unshallow`"
        );
    }
    usage_bail!(
        "--changed-since {base}: {base} and HEAD have no common ancestor. \
         If this is a CI checkout, fetch the full history (`fetch-depth: 0` on actions/checkout)"
    )
}

/// Paths changed between `from` and the working tree (`HEAD`) or between
/// `from` and `HEAD` (a merge-base commit), from the repository root.
async fn diff_names(from: &str) -> anyhow::Result<Vec<String>> {
    let range;
    let rev = if from == "HEAD" {
        "HEAD"
    } else {
        range = format!("{from}..HEAD");
        range.as_str()
    };
    let args = |no_relative: bool| {
        let mut a = vec![
            "-c",
            "diff.relative=false",
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
        ];
        if no_relative {
            a.push("--no-relative");
        }
        a.extend(["--end-of-options", rev]);
        a
    };
    match git_z(&args(true)).await {
        Ok(v) => Ok(v),
        // git before 2.28 has no `--no-relative`; the config does the same.
        Err(e) if e.to_string().contains("no-relative") => git_z(&args(false)).await,
        Err(e) => Err(e),
    }
}

/// Pick the tests to run for changes since `base` (see `atalanta_runner::impact`).
async fn select_impacted(
    suite: &atalanta_common::TestSuite,
    base: &str,
    args: &RunArgs,
    artifacts_dir: &str,
) -> anyhow::Result<Impacted> {
    let suite_dir = std::path::Path::new(&args.config)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let map_path = args
        .impact_map
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| suite_dir.join("atalanta-impact.yaml"));
    let mapping = if map_path.exists() {
        atalanta_core::impact::ImpactMapping::load(&map_path)?
    } else {
        atalanta_core::impact::ImpactMapping::empty()
    };

    check_base(base)?;
    git_z(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{base}^{{commit}}"),
    ])
    .await
    .map_err(|_| usage_err!("--changed-since {base}: not a commit in this repository"))?;
    // What this branch changed since it forked from `base` (`base...HEAD`:
    // the merge base, so upstream-only commits on `base` don't count,
    // D-24), uncommitted edits (`HEAD` against the working tree, staged or
    // not), and new files. All paths from the repository root, wherever
    // the run starts: `diff --no-relative` (a user's `diff.relative=true`
    // printed them relative to the cwd, so root mappings missed, N-6);
    // `ls-files` needs `--full-name`, and `:/` to look beyond the working
    // directory. `--no-renames`: a rename lists its old path too, so a
    // mapping on the old path still selects its tests.
    let fork = merge_base(base).await?;
    let mut changed = diff_names(&fork).await?;
    changed.extend(diff_names("HEAD").await?);
    changed.extend(
        git_z(&[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
            ":/",
        ])
        .await?,
    );
    // What Atalanta itself writes (healing snapshots, reports, history) is
    // not a change to the app.
    let prefix = git_z(&["rev-parse", "--show-prefix"])
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    let artifacts = format!(
        "{prefix}{}/",
        artifacts_dir.trim_start_matches("./").trim_end_matches('/')
    );
    changed.retain(|p| !is_atalanta_output(p, &artifacts));
    changed.sort();
    changed.dedup();

    // A mapping that matches no file in the repository is a typo or a
    // moved folder: its tests would never be selected by a change.
    if let Ok(tracked) = git_z(&["ls-files", "-z", "--full-name", ":/"]).await {
        for (owner, pattern) in atalanta_runner::impact::dead_patterns(&mapping, &tracked) {
            println!(
                "{} impact map: {owner} pattern `{pattern}` matches no file in the repository",
                "warning:".yellow().bold()
            );
        }
    }

    let history = atalanta_runner::history::load(artifacts_dir).unwrap_or_default();
    let sel = atalanta_runner::impact::select(
        &suite.tests,
        &atalanta_runner::impact::flows_used_in(&suite.tests, &suite.flows),
        &mapping,
        &changed,
        &args.config,
        &history,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    if let Some(why) = &sel.run_all {
        println!("{} Impact: running all tests ({why})", "=>".cyan().bold());
        return Ok(Impacted {
            git: suite.tests.iter().map(|t| t.name.clone()).collect(),
            history: Vec::new(),
        });
    }
    println!(
        "{} Impact: {} of {} tests for {} changed file(s) since {base}",
        "=>".cyan().bold(),
        sel.reasons.len(),
        suite.tests.len(),
        changed.len()
    );
    // `--fail-if-none` looks only at what git and the impact map select:
    // the history picks differ per shard (each has its own history), so
    // one shard ran a "stale" test and passed while another exited 2
    // (review d4 m6).
    let git_picks = sel
        .reasons
        .keys()
        .filter(|n| !sel.from_history.contains(*n))
        .count();
    if git_picks == 0 && args.fail_if_none {
        usage_bail!(
            "--changed-since {base}: no test is affected by these changes (--fail-if-none)"
        );
    }
    if sel.reasons.is_empty() {
        // An empty selection passes unless asked otherwise (D-18).
        println!(
            "{} No test is affected by these changes; nothing to run",
            "=>".cyan().bold()
        );
    }
    for (name, why) in &sel.reasons {
        println!("     {name} — {why}");
    }
    if sel.unmapped > 0 {
        println!(
            "{} {} unmapped test(s) selected (map them in atalanta-impact.yaml, or set `impact: {{ unmapped: skip }}`, to skip them safely)",
            "=>".cyan().bold(),
            sel.unmapped
        );
    }
    let history: Vec<String> = sel.from_history.iter().cloned().collect();
    let git = sel
        .reasons
        .into_keys()
        .filter(|n| !sel.from_history.contains(n))
        .collect();
    Ok(Impacted { git, history })
}

/// The run's variable overrides: every `--env-file` (in order), then
/// every `--env`. They are collected into one map where a later pair wins,
/// so `--env` outranks the files and a later file an earlier one; any of
/// them outranks the process environment, and a secret-looking name is
/// masked whichever it came from (`atalanta_core::env`).
pub(crate) fn env_overrides(
    files: &[String],
    flags: &[String],
) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for path in files {
        out.extend(super::env_file::read(path)?);
    }
    out.extend(parse_env_overrides(flags).map_err(|e| usage_err!("--env {e}"))?);
    Ok(out)
}

/// `--env` values as (key, value). The flag splits on commas (`--env
/// A=1,B=2`), so a fragment without `=` is the rest of the value before it:
/// `--env PRICES=1,2` is `PRICES` = `1,2`, not `1` (VEL-S04).
pub(crate) fn parse_env_overrides(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    // Each `--env` on its own: a fragment joins only the value before it
    // in the same flag. `--env A=1 --env foo` made `A` = `1,foo` (m-8).
    for flag in raw {
        let mut current: Option<(String, String)> = None;
        for fragment in flag.split(',') {
            match (fragment.split_once('='), current.as_mut()) {
                (Some((k, v)), _) if !k.is_empty() => {
                    out.extend(current.take());
                    current = Some((k.to_string(), v.to_string()));
                }
                (_, Some((_, v))) => {
                    v.push(',');
                    v.push_str(fragment);
                }
                (_, None) => return Err(format!("`{flag}`: `{fragment}` is not KEY=VALUE")),
            }
        }
        out.extend(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review-d5 m-4: a daemon run (the driver outlives it) still undoes
    /// the status-bar freeze at its end; a plain run cleans up as before.
    #[tokio::test]
    async fn the_end_of_a_daemon_run_unfreezes_the_status_bar() {
        use atalanta_common::{
            DeviceInfo, Direction, Element, Key, PlatformDriver, Result, Selector,
        };
        /// Records the two calls the end of a run makes; nothing else is
        /// called.
        #[derive(Default)]
        struct Calls(std::sync::Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl PlatformDriver for Calls {
            async fn unfreeze_status_bar(&self) {
                self.0.lock().unwrap().push("unfreeze");
            }
            async fn cleanup(&self) {
                self.0.lock().unwrap().push("cleanup");
            }
            async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
                unreachable!()
            }
            async fn boot_device(&self, _: &str) -> Result<()> {
                unreachable!()
            }
            async fn shutdown_device(&self, _: &str) -> Result<()> {
                unreachable!()
            }
            async fn install_app(&self, _: &str, _: &str) -> Result<()> {
                unreachable!()
            }
            async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
                unreachable!()
            }
            async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
                unreachable!()
            }
            async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
                unreachable!()
            }
            async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
                unreachable!()
            }
            async fn get_hierarchy(&self, _: &str) -> Result<Element> {
                unreachable!()
            }
            async fn tap(&self, _: &str, _: &Element) -> Result<()> {
                unreachable!()
            }
            async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
                unreachable!()
            }
            async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
                unreachable!()
            }
            async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
                unreachable!()
            }
            async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
                unreachable!()
            }
            async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
                unreachable!()
            }
            async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
                unreachable!()
            }
            async fn press_key(&self, _: &str, _: Key) -> Result<()> {
                unreachable!()
            }
            async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
                unreachable!()
            }
            async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
                unreachable!()
            }
            async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
                unreachable!()
            }
            async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
                unreachable!()
            }
        }
        let d = Calls::default();
        end_of_run(&d, true).await;
        assert_eq!(*d.0.lock().unwrap(), ["unfreeze"]);
    }
    use clap::Parser;

    /// VEL-196: the suite is validated after `${VARS}` are filled in.
    #[test]
    fn a_suite_is_validated_after_interpolation() {
        let dir = std::env::temp_dir().join(format!("vel196-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.yaml");
        std::fs::write(
            &path,
            "appId: x\nflows:\n  - id: login\n    steps: [goBack]\ntests:\n  - name: t\n    steps:\n      - tap: { text: \"${VEL196_LABEL}\" }\n      - runFlow: \"${VEL196_FLOW}\"\n",
        )
        .unwrap();
        let path = path.to_string_lossy().to_string();
        let env = |label: &str| {
            vec![
                ("VEL196_LABEL".to_string(), label.to_string()),
                ("VEL196_FLOW".to_string(), "login".to_string()),
            ]
        };
        // `runFlow: ${FLOW}` names a real flow once interpolated.
        load_suite(&path, &[], &env("Pay")).unwrap();
        // An empty value is an empty selector, refused before the run.
        let err = load_suite(&path, &[], &env("")).unwrap_err().to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("empty"), "{err}");
    }

    /// D-V1: `--update-baselines` keeps meaning "missing and changed";
    /// `=missing` writes only missing ones.
    #[test]
    fn verify_takes_a_level() {
        use atalanta_common::VerifyLevel;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            run: RunArgs,
        }
        let parse = |args: &[&str]| {
            let mut argv = vec!["x"];
            argv.extend_from_slice(args);
            Cli::try_parse_from(argv).map(|c| c.run.verify)
        };
        assert_eq!(parse(&[]).unwrap(), None);
        assert_eq!(
            parse(&["--verify", "strict"]).unwrap(),
            Some(VerifyLevel::Strict)
        );
        assert_eq!(parse(&["--verify=off"]).unwrap(), Some(VerifyLevel::Off));
        assert!(parse(&["--verify", "maybe"]).is_err());
    }

    #[test]
    fn update_baselines_takes_an_optional_mode() {
        use atalanta_common::BaselineUpdate;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            run: RunArgs,
        }
        let mode = |args: &[&str]| {
            let mut argv = vec!["x"];
            argv.extend_from_slice(args);
            Cli::try_parse_from(argv).map(|c| c.run.update_baselines)
        };
        assert_eq!(mode(&[]).unwrap(), None);
        assert_eq!(
            mode(&["--update-baselines"]).unwrap(),
            Some(BaselineUpdate::Changed)
        );
        assert_eq!(
            mode(&["--update-baselines=missing"]).unwrap(),
            Some(BaselineUpdate::Missing)
        );
        assert_eq!(
            mode(&["--update-baselines=all"]).unwrap(),
            Some(BaselineUpdate::Changed)
        );
        // The bare flag doesn't swallow the suite path after it.
        let cli = Cli::try_parse_from(["x", "--update-baselines", "suite.yaml"]).unwrap();
        assert_eq!(cli.run.path.as_deref(), Some("suite.yaml"));
        assert_eq!(cli.run.update_baselines, Some(BaselineUpdate::Changed));
        assert!(mode(&["--update-baselines=sometimes"]).is_err());
        assert!(mode(&["--update-baselines=off"]).is_err());
    }

    /// VEL-S04: a comma inside a value stays in the value.
    #[test]
    fn env_values_keep_their_commas() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            run: RunArgs,
        }
        let cli = Cli::try_parse_from(["x", "--env", "A=1,2", "--env", "B=x,C=y"]).unwrap();
        assert_eq!(
            parse_env_overrides(&cli.run.env_overrides).unwrap(),
            vec![
                ("A".to_string(), "1,2".to_string()),
                ("B".to_string(), "x".to_string()),
                ("C".to_string(), "y".to_string()),
            ]
        );
    }

    /// m-8: a fragment joined the value of an earlier `--env`, and a lone
    /// key was dropped without a word.
    #[test]
    fn env_fragments_stay_within_their_flag() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            run: RunArgs,
        }
        let parse = |args: &[&str]| {
            let mut argv = vec!["x"];
            argv.extend_from_slice(args);
            let cli = Cli::try_parse_from(argv).unwrap();
            parse_env_overrides(&cli.run.env_overrides)
        };
        assert!(parse(&["--env", "A=1", "--env", "foo"]).is_err());
        assert!(parse(&["--env", "FOO"]).is_err());
        assert_eq!(
            parse(&["--env", "A=1", "--env", "B=2,3"]).unwrap(),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2,3".to_string()),
            ]
        );
    }

    /// `--env` outranks every `--env-file`, a later file an earlier one;
    /// all of them end in the one map `load_suite` interpolates from.
    #[test]
    fn env_files_rank_below_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.env"), dir.path().join("b.env"));
        std::fs::write(&a, "API=from-a\nUSER=ada\nTOKEN=a\n").unwrap();
        std::fs::write(&b, "export API=\"from-b\"\n").unwrap();
        let files = [a.display().to_string(), b.display().to_string()];
        let pairs = env_overrides(&files, &["TOKEN=flag".to_string()]).unwrap();
        let map: std::collections::HashMap<String, String> = pairs.into_iter().collect();
        assert_eq!(map["API"], "from-b");
        assert_eq!(map["USER"], "ada");
        assert_eq!(map["TOKEN"], "flag");

        // A bad line: exit 2, with the file and line.
        std::fs::write(&b, "# ok\nA=1\nnot a pair\n").unwrap();
        let e = env_overrides(&files, &[]).unwrap_err();
        assert_eq!(crate::commands::exit_code_for(&e), 2);
        assert!(
            e.to_string().ends_with("b.env:3: expected KEY=VALUE"),
            "{e}"
        );
    }

    use super::is_atalanta_output;

    #[test]
    fn the_effective_platform_is_written_into_the_suite() {
        // M-3: a suite without `platform:` run on device `web` reached the
        // executor with no platform, which then masked a device status bar
        // over the page's header.
        use atalanta_common::Platform;
        let yaml = "appId: x\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n";
        let mut suite = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        assert_eq!(suite.config.platform, None);
        let p = super::resolve_platform(&mut suite, None, Some("web"));
        assert_eq!(p, Platform::Web);
        assert_eq!(suite.config.platform, Some(Platform::Web));
        // The CLI wins over the suite.
        let p = super::resolve_platform(&mut suite, Some(Platform::Android), None);
        assert_eq!(p, Platform::Android);
        assert_eq!(suite.config.platform, Some(Platform::Android));
    }

    /// VEL-041: suite A's `settings:` stayed installed for suite B of the
    /// same folder run (and the daemon's next run).
    #[test]
    ///
    /// C7: it used to read the process-wide store back, which every other
    /// test calling `load_suite` (and so `apply_settings`) overwrote
    /// concurrently, so it failed now and then. It now checks what each
    /// suite installs, through its own sink.
    fn a_suites_settings_do_not_leak_into_the_next_suite() {
        let a = "appId: x\nconfig:\n  settings:\n    android:\n      scroll: semantic\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n";
        let b = "appId: x\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n";
        let mut installed: Vec<std::collections::HashMap<String, String>> = Vec::new();
        let mut suite = atalanta_core::parser::parse_suite_from_str(a).unwrap();
        super::apply_settings_with(&mut suite, &[], |env| installed.push(env)).unwrap();
        let mut suite = atalanta_core::parser::parse_suite_from_str(b).unwrap();
        super::apply_settings_with(&mut suite, &[], |env| installed.push(env)).unwrap();
        assert_eq!(installed.len(), 2, "every suite installs its settings");
        assert_eq!(
            installed[0]
                .get("ATALANTA_ANDROID_SCROLL")
                .map(String::as_str),
            Some("semantic")
        );
        // Suite B installs an empty set, replacing A's: it sees the default.
        assert_eq!(installed[1], Default::default(), "suite B sees the default");
    }

    #[test]
    fn atalanta_state_and_artifacts_are_not_app_changes() {
        let artifacts = "e2e/atalanta-results/";
        assert!(is_atalanta_output(".atalanta/snapshots.json", artifacts));
        assert!(is_atalanta_output(
            "e2e/.atalanta/healed-selectors.json",
            artifacts
        ));
        assert!(is_atalanta_output(
            "e2e/atalanta-results/junit.xml",
            artifacts
        ));
        assert!(!is_atalanta_output("src/net/wifi.ts", artifacts));
        assert!(!is_atalanta_output("atalanta-results/x", artifacts));
    }
}

#[cfg(test)]
mod folder_tests {
    use super::combined_exit;

    #[test]
    fn the_step_profile_ranks_actions_by_time() {
        use atalanta_common::{StepResult, StepStatus, SuiteResult, TestResult, TestStatus};
        use std::time::Duration;
        let step = |i: usize, name: &str, ms: u64, settle: u64| StepResult {
            step_index: i,
            action_name: name.into(),
            status: StepStatus::Passed,
            duration: Duration::from_millis(ms),
            screenshot: None,
            error_message: None,
            resource_delta: None,
            settle_ms: Some(settle),
            source: None,
        };
        let mut t = TestResult::failed("login", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.steps = vec![
            step(0, "tap", 300, 200),
            step(1, "launchApp", 2000, 300),
            step(2, "tap", 500, 400),
        ];
        let r = SuiteResult {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: Duration::ZERO,
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let table = super::step_profile(&r);
        let lines: Vec<&str> = table.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(lines[1].trim_start().starts_with("launchApp"), "{table}");
        assert!(
            lines[2].contains("tap") && lines[2].contains("75%") && lines[2].contains("login #3"),
            "{table}"
        );
    }

    #[test]
    fn set_reaches_config_keys_keeping_their_types() {
        let mut c = serde_json::json!({ "sync": { "reaction_ms": 300, "mode": "auto" }, "retry": { "count": 0 }, "batch": false });
        super::set_config_path(&mut c, "sync.reactionMs", "150").unwrap();
        super::set_config_path(&mut c, "retry.count", "2").unwrap();
        super::set_config_path(&mut c, "batch", "true").unwrap();
        assert_eq!(c["sync"]["reaction_ms"], 150);
        assert_eq!(c["retry"]["count"], 2);
        assert_eq!(c["batch"], true);
        assert!(super::set_config_path(&mut c, "sync.reactionMz", "1").is_err());
        assert!(super::set_config_path(&mut c, "sync.reactionMs", "fast").is_err());
    }

    /// `--set flag=TRUE` or `=on` used to turn a boolean off without a word:
    /// only `true`, `1` and `yes` were true, so everything else, a typo
    /// included, was false.
    #[test]
    fn set_reads_booleans_the_way_people_write_them_and_refuses_the_rest() {
        let mut c = serde_json::json!({ "batch": false });
        for yes in ["true", "TRUE", "True", "1", "yes", "on"] {
            c["batch"] = false.into();
            super::set_config_path(&mut c, "batch", yes).unwrap();
            assert_eq!(c["batch"], true, "{yes}");
        }
        for no in ["false", "FALSE", "0", "no", "off"] {
            c["batch"] = true.into();
            super::set_config_path(&mut c, "batch", no).unwrap();
            assert_eq!(c["batch"], false, "{no}");
        }
        c["batch"] = true.into();
        let e = super::set_config_path(&mut c, "batch", "flase").unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2, "{e}");
        assert_eq!(c["batch"], true, "a refused value changes nothing");
    }

    /// `--device Pixel_8` (a name, or an id in another case) passed the
    /// up-front check and then went to adb as the serial. A sequential run
    /// now takes the id the driver knows the device by.
    #[test]
    fn a_sequential_device_name_resolves_to_its_id() {
        use atalanta_common::{DeviceInfo, DeviceState, DeviceType, Platform};
        let dev = |id: &str, name: &str| DeviceInfo {
            id: id.into(),
            name: name.into(),
            platform: Platform::Android,
            state: DeviceState::Booted,
            os_version: None,
            device_type: DeviceType::Emulator,
        };
        let devices = [
            dev("emulator-5554", "Pixel_7"),
            dev("emulator-5556", "Pixel_8"),
            dev("R5CT1", "Pixel_8"),
        ];
        let r = |w: &str| super::resolve_exact_device(w, &devices);
        assert_eq!(r("Pixel_7"), Ok(Some("emulator-5554".into())));
        assert_eq!(r("pixel_7"), Ok(Some("emulator-5554".into())));
        assert_eq!(r("EMULATOR-5556"), Ok(Some("emulator-5556".into())));
        assert_eq!(r("emulator-5554"), Ok(None), "an exact id stays as it is");
        assert_eq!(r("nothing"), Ok(None));
        let list = r("emulator-5554,emulator-5556").unwrap_err();
        assert!(list.contains("--parallel"), "a list is a pool's: {list}");
        let two = r("Pixel_8").unwrap_err();
        assert!(
            two.contains("emulator-5556") && two.contains("R5CT1"),
            "{two}"
        );
    }

    // A-10: `--junit-classname suite` names the suite file's stem; the
    // default keeps `atalanta`.
    #[test]
    fn junit_classname_is_opt_in() {
        use clap::Parser;
        let a = Parse::try_parse_from(["x.yaml"]).unwrap().args;
        assert_eq!(a.junit_classname, super::JunitClassname::Test);
        assert_eq!(
            super::junit_classname(a.junit_classname, "tests/checkout.yaml"),
            None
        );
        let a = Parse::try_parse_from(["--junit-classname", "suite"])
            .unwrap()
            .args;
        assert_eq!(
            super::junit_classname(a.junit_classname, "tests/checkout.yaml").as_deref(),
            Some("checkout")
        );
        assert!(Parse::try_parse_from(["--junit-classname", "file"]).is_err());
    }

    // D-21: `--fail-on-flaky` and `retry.flakyResult: fail`.
    #[test]
    fn fail_on_flaky_is_a_flag_and_a_suite_setting() {
        use clap::Parser;
        assert!(
            Parse::try_parse_from(["--fail-on-flaky"])
                .unwrap()
                .args
                .fail_on_flaky
        );
        let yaml = "appId: x\nconfig:\n  retry:\n    count: 2\n    flakyResult: fail\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n";
        let suite = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        assert_eq!(
            suite.config.retry.flaky_result,
            atalanta_common::test_types::FlakyResult::Fail
        );
        assert!(atalanta_core::config_keys::unknown_config_keys(yaml).is_empty());
        let mut c = serde_json::to_value(&suite.config).unwrap();
        super::set_config_path(&mut c, "retry.flakyResult", "pass").unwrap();
        assert_eq!(c["retry"]["flakyResult"], "pass");
    }

    #[test]
    fn the_first_of_a_device_list_bootstraps() {
        assert_eq!(super::first_device("simA, simB"), "simA");
        assert_eq!(super::first_device("emulator-5554"), "emulator-5554");
    }

    #[test]
    fn a_test_failure_outranks_a_device_failure_across_suites() {
        assert_eq!(combined_exit(&[0, 0]), 0);
        assert_eq!(combined_exit(&[0, 3]), 3);
        assert_eq!(combined_exit(&[3, 1, 0]), 1);
        assert_eq!(combined_exit(&[1, 2]), 2);
        assert_eq!(combined_exit(&[]), 0);
    }

    #[derive(clap::Parser)]
    #[command(no_binary_name = true)]
    struct Parse {
        #[command(flatten)]
        args: super::RunArgs,
    }

    // VEL-003: shard flags that would run nothing or everything are usage
    // errors (exit 2), not a green run.
    #[test]
    fn bad_shard_flags_are_usage_errors() {
        use clap::Parser;
        for bad in [
            &["--shard-index", "0", "--shard-total", "0"][..],
            &["--shard-index", "1"],
            &["--shard-total", "3"],
        ] {
            let e = Parse::try_parse_from(bad).err().expect("rejected");
            assert_eq!(e.exit_code(), 2, "{bad:?}: {e}");
        }
        assert!(Parse::try_parse_from(["--shard-index", "1", "--shard-total", "2"]).is_ok());
    }

    // D-22: `--shard M/N` is 1-based; the 0-based flags stay as an alias;
    // both together, or an M outside 1..=N, is exit 2.
    #[test]
    fn shard_m_of_n_is_one_based() {
        use clap::Parser;
        let of = |argv: &[&str]| {
            let a = Parse::try_parse_from(argv).unwrap().args;
            super::shard_of(a.shard.as_deref(), a.shard_index, a.shard_total)
        };
        assert_eq!(of(&["--shard", "1/3"]).unwrap(), (Some(0), Some(3)));
        assert_eq!(of(&["--shard", "3/3"]).unwrap(), (Some(2), Some(3)));
        assert_eq!(
            of(&["--shard-index", "2", "--shard-total", "3"]).unwrap(),
            (Some(2), Some(3))
        );
        for bad in ["0/3", "4/3", "1/0", "x", "1-3", "/3"] {
            let e = of(&["--shard", bad]).unwrap_err();
            assert_eq!(super::super::exit_code_for(&e), 2, "{bad}: {e}");
        }
        let e =
            Parse::try_parse_from(["--shard", "1/2", "--shard-index", "0", "--shard-total", "2"])
                .err()
                .expect("both forms rejected");
        assert_eq!(e.exit_code(), 2);
        // --shard-timings goes with either form.
        assert!(Parse::try_parse_from(["--shard", "1/2", "--shard-timings", "h.json"]).is_ok());
        assert!(Parse::try_parse_from(["--shard-timings", "h.json"]).is_err());
    }

    // VEL-027: a usage error exits 2 and an unclassified error 5 — both
    // used to exit 1, the code for failing tests.
    #[test]
    fn usage_errors_exit_2_and_unknown_errors_5() {
        let usage = super::super::usage("Unknown platform 'symbian'".into());
        assert_eq!(super::super::exit_code_for(&usage), 2);
        let wrapped = usage.context("loading suite");
        assert_eq!(super::super::exit_code_for(&wrapped), 2);
        let io = anyhow::Error::from(std::io::Error::other("disk"));
        assert_eq!(super::super::exit_code_for(&io), 5);
        assert_eq!(
            super::parse_driver_mode("x")
                .map_err(|e| super::super::exit_code_for(&e))
                .err(),
            Some(2)
        );
    }

    // `mcp` with no booted device and `signing` with no connected iPhone
    // are device errors (3), not internal ones (5).
    #[test]
    fn no_device_is_a_device_error() {
        let e = super::super::no_device("No booted device found", vec!["sim-1".into()]);
        assert_eq!(super::super::exit_code_for(&e), 3);
        assert_eq!(e.to_string(), "No booted device found");
    }

    // VEL-175: a --tags / --filter typo that selects nothing is exit 2.
    #[test]
    fn a_selection_that_matches_nothing_is_a_usage_error() {
        let t = |name: &str, tags: &[&str]| atalanta_common::TestCase {
            name: name.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            isolated: false,
            depends_on: vec![],
            steps: vec![],
            description: None,
            verify: None,
        };
        let tests = vec![t("login", &["smoke"]), t("pay", &["slow"])];
        assert!(super::check_selection(&tests, &[], None).is_ok());
        assert!(super::check_selection(&tests, &["smoke".into()], None).is_ok());
        assert!(super::check_selection(&tests, &[], Some("log*")).is_ok());
        let e = super::check_selection(&tests, &["smkoe".into()], None).unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2);
        assert!(e.to_string().contains("slow, smoke"), "{e}");
        let e = super::check_selection(&tests, &["smoke".into()], Some("pay")).unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2);
    }
}
