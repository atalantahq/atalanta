use serde::{Deserialize, Serialize};

/// Report format for test results output.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReportFormat {
    #[default]
    Junit,
    Json,
}

/// Driver mode — how tests are executed.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DriverMode {
    /// Execute against a real emulator/simulator/device.
    #[default]
    Device,
    /// CPU-only headless rendering (no emulator needed).
    Headless,
}

/// Detected app framework.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Framework {
    /// Native iOS/Android (UIKit, Storyboard, Android XML).
    #[default]
    Native,
    /// React Native (JS bundle + bridge).
    #[serde(alias = "reactnative", alias = "rn")]
    ReactNative,
    /// Flutter (Dart + RenderObject tree).
    Flutter,
}

impl std::fmt::Display for Framework {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native => write!(f, "native"),
            Self::ReactNative => write!(f, "react_native"),
            Self::Flutter => write!(f, "flutter"),
        }
    }
}

/// When a test's trace (steps, bounds, settle reads, the trees it read)
/// is written to `<artifacts>/traces/<test>/`. It is always recorded in
/// memory; this only decides whether it is kept (Playwright's
/// `retain-on-failure`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TraceMode {
    /// Not recorded at all.
    Off,
    /// Kept for failed attempts (a retried test keeps its failed attempt).
    #[default]
    OnFailure,
    /// Kept for every test, passing ones too.
    Always,
}

impl std::str::FromStr for TraceMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "off" => Ok(Self::Off),
            "on-failure" | "retain-on-failure" => Ok(Self::OnFailure),
            "always" | "on" => Ok(Self::Always),
            other => Err(format!(
                "unknown trace mode '{other}'; use off, on-failure or always"
            )),
        }
    }
}

/// Runtime configuration merged from CLI args and config file.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config_path: String,
    pub platform: Option<crate::types::Platform>,
    pub device_id: Option<String>,
    pub tags: Vec<String>,
    pub test_filter: Option<String>,
    pub shard_index: Option<usize>,
    pub shard_total: Option<usize>,
    pub retry_count: Option<u32>,
    pub report_format: ReportFormat,
    pub artifacts_dir: String,
    pub suite_timeout_ms: Option<u64>,
    /// `--test-timeout`: the most one attempt of one test may take. Past
    /// it that attempt fails (and may be retried); the run goes on.
    pub test_timeout_ms: Option<u64>,
    pub fail_fast: bool,
    /// Run tests that failed last time first (sequential runs). Fastest
    /// feedback, and with `fail_fast` the quickest red build.
    pub failed_first: bool,
    /// Shuffle test order with this seed (dependencies still run first).
    pub shuffle_seed: Option<u64>,
    /// Flaky-hunting rounds: run the suite this many times, shuffled, with
    /// step jitter and no retries, and report tests whose outcome varies.
    pub hunt_rounds: Option<u32>,
    /// Max random delay injected before each step (flaky hunting), in ms.
    pub jitter_ms: Option<u64>,
    /// Load the device's CPU while testing (flaky hunting; Android only).
    pub stress: bool,
    /// Change-based selection: run only these tests (plus their
    /// dependencies). Derived from git and the impact map only, so it is
    /// the same on every CI shard.
    pub only_tests: Option<Vec<String>>,
    /// History-derived selection (`--last-failed`; the "failed recently",
    /// "new" and "stale" reasons of `--changed-since`), applied after
    /// sharding (R-1): each shard's local history differs, so filtering
    /// before the partition dropped tests one shard chose but another owned.
    /// When set, the shard owns tests from the full filtered suite and runs
    /// those in `only_tests` or here.
    pub post_shard: Option<std::sync::Arc<std::collections::HashSet<String>>>,
    /// Early flake detection: re-run tests new to the history a few times
    /// after they pass.
    pub detect_new_flakes: bool,
    /// `--cache-results`: skip tests that passed with nothing changed.
    pub cache_results: bool,
    pub env_overrides: Vec<(String, String)>,
    pub driver_mode: DriverMode,
    pub framework: Framework,
    /// `--update-baselines[=missing]`: which assertScreenshot baselines
    /// the run writes.
    pub update_baselines: BaselineUpdate,
    /// The baselines this run created (D-V1): fresh per run, shared by its
    /// devices and retries.
    pub created_baselines: CreatedBaselines,
    /// `--trace`: when traces are written.
    pub trace: TraceMode,
    /// `--verify`: the AI verification level for every test, over the
    /// tests' and the suite's `verify:`. `None`: theirs.
    pub verify: Option<crate::VerifyLevel>,
    /// `--flaky-confidence`: run enough copies of tests the history flags
    /// as flaky that at least one passes with this probability (0..1).
    pub flaky_confidence: Option<f64>,
    /// `--shard-timings`: a history file every shard reads, for
    /// duration-balanced sharding. Without it shards balance test counts
    /// (each shard's own history differs, so it can't be used).
    pub shard_timings: Option<String>,
    /// The durations of `shard_timings`, loaded once by the caller (D-23):
    /// the scheduler uses these and never re-reads the file, so a second
    /// read failing can't make one shard fall back to another algorithm.
    pub shard_timings_ms: Option<std::sync::Arc<std::collections::HashMap<String, u64>>>,
    /// Asks the run to stop early (Ctrl-C / SIGTERM). The runners stop at
    /// once and report what didn't run as skipped, so a stopped run still
    /// writes its reports and history.
    pub stop: StopHandle,
    /// When this run started: traces written before it that a pass in
    /// this run supersedes are stale (VEL-205). `None`: the runner takes
    /// the time it starts. Set once by callers that make several runner
    /// calls for one run (a sequential flaky hunt), so a failed round
    /// stays current when a later round passes.
    pub run_started: Option<std::time::SystemTime>,
    /// Where self-healing keeps its files (healed mappings, element
    /// snapshots, rejected heals); `None` is `.velocity/` in the directory
    /// the run starts in. Tests point it at a temp dir, so they never write
    /// into the source tree.
    pub state_dir: Option<std::path::PathBuf>,
}

/// A request to stop a run early, shared by every clone. The first
/// [`stop`](Self::stop) wins; its reason goes into the skipped tests.
#[derive(Clone, Default)]
pub struct StopHandle(std::sync::Arc<StopInner>);

#[derive(Default)]
struct StopInner {
    reason: std::sync::OnceLock<String>,
    notify: tokio::sync::Notify,
}

impl StopHandle {
    /// Ask the run to stop, for `reason` ("interrupted by Ctrl-C").
    pub fn stop(&self, reason: impl Into<String>) {
        let _ = self.0.reason.set(reason.into());
        self.0.notify.notify_waiters();
    }

    /// Why the run was asked to stop, if it was.
    pub fn reason(&self) -> Option<&str> {
        self.0.reason.get().map(String::as_str)
    }

    /// Resolves with the reason once the run is asked to stop.
    pub async fn stopped(&self) -> String {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(r) = self.reason() {
                return r.to_string();
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for StopHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StopHandle").field(&self.reason()).finish()
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            config_path: "velocity.yaml".to_string(),
            platform: None,
            device_id: None,
            tags: Vec::new(),
            test_filter: None,
            shard_index: None,
            shard_total: None,
            retry_count: None,
            report_format: ReportFormat::default(),
            artifacts_dir: "./velocity-results".to_string(),
            suite_timeout_ms: None,
            test_timeout_ms: None,
            fail_fast: false,
            failed_first: false,
            shuffle_seed: None,
            hunt_rounds: None,
            jitter_ms: None,
            stress: false,
            only_tests: None,
            post_shard: None,
            detect_new_flakes: false,
            cache_results: false,
            env_overrides: Vec::new(),
            driver_mode: DriverMode::default(),
            framework: Framework::default(),
            update_baselines: BaselineUpdate::Off,
            created_baselines: CreatedBaselines::default(),
            trace: TraceMode::default(),
            verify: None,
            flaky_confidence: None,
            shard_timings: None,
            shard_timings_ms: None,
            stop: StopHandle::default(),
            run_started: None,
            state_dir: None,
        }
    }
}

/// Which `assertScreenshot` baselines a run writes (D-V1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaselineUpdate {
    /// Compare only. A missing baseline is written and the step fails
    /// ("review and commit it"), on a laptop and in CI alike.
    #[default]
    Off,
    /// `--update-baselines=missing`: write missing baselines and pass;
    /// existing ones are compared as usual.
    Missing,
    /// `--update-baselines` (`=changed`, `=all`): write every baseline that
    /// is missing or no longer matches, and pass.
    Changed,
}

impl From<bool> for BaselineUpdate {
    /// `true` is the plain `--update-baselines`.
    fn from(on: bool) -> Self {
        if on {
            Self::Changed
        } else {
            Self::Off
        }
    }
}

impl BaselineUpdate {
    /// Whether the run writes any baseline (and so skips the result cache).
    pub fn is_on(self) -> bool {
        self != Self::Off
    }

    /// `--update-baselines` / `VELOCITY_UPDATE_BASELINES` values: `missing`,
    /// `changed`/`all`/`true`/`1`, `off`/`false`/`0`. An empty value is
    /// none of them (the bare flag gets `changed` from clap).
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "missing" => Some(Self::Missing),
            "changed" | "all" | "true" | "1" => Some(Self::Changed),
            "off" | "none" | "false" | "0" => Some(Self::Off),
            _ => None,
        }
    }

    /// `VELOCITY_UPDATE_BASELINES`: unset or empty is off (as before D-V1,
    /// so CI templating that leaves it blank never rewrites baselines);
    /// only an explicit value turns updating on, `1`/`true` meaning the
    /// bare flag. `Err` for a value that isn't one (exit 2).
    pub fn from_env(v: Option<&str>) -> std::result::Result<Self, String> {
        match v.map(str::trim) {
            None | Some("") => Ok(Self::Off),
            Some(v) => Self::parse(v).ok_or_else(|| {
                format!("VELOCITY_UPDATE_BASELINES={v}: expected missing, changed, 1 or 0")
            }),
        }
    }
}

/// The baselines a run wrote because they were missing (D-V1): they fail
/// for the rest of that run, so a retry (or another device of a parallel
/// run) never passes on a file nobody reviewed. One per run, shared by
/// every executor of the run (clones share it), and fresh for the next
/// run, so a long-lived process (`velocity daemon`, MCP) passes once the
/// baseline is committed. Keyed by absolute canonical path, so two
/// projects' same relative path never meet.
#[derive(Clone, Default)]
pub struct CreatedBaselines(
    std::sync::Arc<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>>,
);

impl CreatedBaselines {
    fn key(path: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
    }

    fn set(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<std::path::PathBuf>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `path` was just written as a missing baseline.
    pub fn note(&self, path: &std::path::Path) {
        self.set().insert(Self::key(path));
    }

    /// Whether this run wrote `path` as a missing baseline.
    pub fn contains(&self, path: &std::path::Path) -> bool {
        self.set().contains(&Self::key(path))
    }

    /// `path` was (re)written by an update mode: it is what was asked for.
    pub fn forget(&self, path: &std::path::Path) {
        self.set().remove(&Self::key(path));
    }

    /// How many baselines this run created.
    pub fn len(&self) -> usize {
        self.set().len()
    }

    /// Whether this run created none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for CreatedBaselines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CreatedBaselines")
            .field(&self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review-d5 M-2: an empty `VELOCITY_UPDATE_BASELINES` is off, as it was
    /// before D-V1; only an explicit value turns updating on.
    #[test]
    fn update_baselines_env_values() {
        let env = |v: Option<&str>| BaselineUpdate::from_env(v);
        assert_eq!(env(None), Ok(BaselineUpdate::Off));
        assert_eq!(env(Some("")), Ok(BaselineUpdate::Off));
        assert_eq!(env(Some("  ")), Ok(BaselineUpdate::Off));
        assert_eq!(env(Some("0")), Ok(BaselineUpdate::Off));
        assert_eq!(env(Some("false")), Ok(BaselineUpdate::Off));
        assert_eq!(env(Some("missing")), Ok(BaselineUpdate::Missing));
        assert_eq!(env(Some("changed")), Ok(BaselineUpdate::Changed));
        assert_eq!(env(Some("all")), Ok(BaselineUpdate::Changed));
        assert_eq!(env(Some("1")), Ok(BaselineUpdate::Changed));
        assert_eq!(env(Some("true")), Ok(BaselineUpdate::Changed));
        assert!(env(Some("sometimes")).is_err());
        // The shared parser (flag and env) has no meaning for "".
        assert_eq!(BaselineUpdate::parse(""), None);
    }

    /// review-d5 M-1: the registry is per run (clones share it, a new one
    /// is empty) and keyed by canonical path.
    #[test]
    fn created_baselines_are_per_run_and_canonical() {
        let dir = std::env::temp_dir().join(format!("vel-created-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let run = CreatedBaselines::default();
        let device2 = run.clone();
        run.note(&file);
        assert!(device2.contains(&file));
        assert!(device2.contains(&dir.join(".").join("a.png")));
        assert!(!CreatedBaselines::default().contains(&file));
        assert!(!RuntimeConfig::default().created_baselines.contains(&file));
        run.forget(&file);
        assert!(!device2.contains(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
