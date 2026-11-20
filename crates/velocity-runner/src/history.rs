use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use velocity_common::{
    DeviceInfo, Platform, PlatformDriver, ResourceSnapshot, Result, TestResult, TestStatus,
    VelocityError,
};

pub(crate) const HISTORY_FILENAME: &str = "velocity-history.json";

/// Version of the history file this code writes.
///
/// - 1 (no `version` field): outcomes and durations per test only.
/// - 2: adds `byDevice`, outcomes per (test, platform, device model, OS
///   major). `outcomes` and `durations` stay the test-wide aggregate that
///   scheduling and sharding use, so a version-1 file migrates as is: its
///   outcomes are that aggregate, and per-device history starts empty.
/// - 3: adds `failureKinds`, what each recent failed or flaky run failed on
///   ([`FailureKind`]). A version-2 file migrates as is: its failures are
///   of unknown kind. An older Velocity reads a version-3 file and drops
///   the field when it rewrites it, which reads back the same way.
pub const HISTORY_VERSION: u32 = 3;

/// Historical resource baseline for a test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBaseline {
    pub peak_pss_kb: u64,
    pub peak_java_heap_kb: u64,
    pub avg_cpu_percent: f32,
}

/// The old name of [`HistoryOutcome`] (review 06 #10: four types were
/// called `Outcome`).
pub type Outcome = HistoryOutcome;

/// How one run of a test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryOutcome {
    Pass,
    Fail,
    /// Failed, then passed on retry.
    Flaky,
}

/// What a failed run, or the first attempt of a run that passed on retry,
/// failed on. Flip rate alone hides how a test flakes: a test that times
/// out now and then needs a different fix from one whose element is
/// sometimes missing, so flakiness is also scored per kind (Kisaakye et
/// al., "Towards an Actionable Flakiness Score", ICST 2026).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    NotFound,
    Assertion,
    Timeout,
    Crash,
    /// The device or its connection (on a first attempt: a final one is
    /// kept out of history).
    Infrastructure,
    Http,
    Other,
    /// Recorded before kinds were, or a kind a newer Velocity wrote: one
    /// unknown name must not make the whole file unreadable.
    #[serde(other)]
    Unknown,
}

impl FailureKind {
    /// The name history files and reports use.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotFound => "not-found",
            Self::Assertion => "assertion",
            Self::Timeout => "timeout",
            Self::Crash => "crash",
            Self::Infrastructure => "infrastructure",
            Self::Http => "http",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }

    /// The kind of a failure from its message. Results carry the error as
    /// text only, and a step error has its file position and step text in
    /// front (`app.yaml:42: tap { text: "Pay" }: Element not found ...`),
    /// so this looks for each error's own wording anywhere in it, the
    /// least ambiguous first.
    pub fn of_message(message: &str) -> Self {
        let m = message.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
        if has(&["crashed:"]) {
            Self::Crash
        } else if has(&[
            "no device available",
            "not found. available:",
            "failed to boot device",
            "is locked with a pin",
            "wda health check failed",
            "the ios runner can't start",
            "wda session lost",
            "webdriveragent",
            "circuit breaker is open",
            "chrome devtools connection lost",
            "adb connection lost",
            "command failed on device",
            "screen capture failed",
            " is unavailable: ",
        ]) {
            Self::Infrastructure
        } else if has(&[" check failed: ", " checks failed: ", ": no response: "]) {
            Self::Http
        } else if has(&[
            "sync timeout:",
            "step timeout:",
            "suite timeout",
            "timed out",
        ]) {
            Self::Timeout
        } else if has(&["assertion failed"]) {
            Self::Assertion
        } else if has(&["element not found", "is not actionable:"]) {
            Self::NotFound
        } else {
            Self::Other
        }
    }

    /// The kind of `result`'s failure: its error when it failed, its first
    /// failed attempt when it passed on retry.
    pub fn of_result(result: &TestResult) -> Self {
        if result.infrastructure_failure {
            return Self::Infrastructure;
        }
        let message = match result.status {
            TestStatus::Retried => result
                .failed_attempts
                .first()
                .or(result.error_message.as_ref()),
            _ => result.error_message.as_ref(),
        };
        message.map_or(Self::Other, |m| Self::of_message(m))
    }
}

/// Runs needed before a test can be flagged flaky.
const FLAG_MIN_RUNS: usize = 5;
/// Flakiness score that flags a test.
const FLAG_THRESHOLD: f64 = 0.15;
/// Straight passes that clear the flag.
const CLEAR_AFTER: usize = 10;

/// How many recent outcomes to keep per test.
const OUTCOME_WINDOW: usize = 20;
/// Weight of the newest run in the duration estimate. Smoothing keeps one slow
/// retried run from skewing shard balance for the next run.
const DURATION_ALPHA: f64 = 0.3;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestHistory {
    /// File format (see [`HISTORY_VERSION`]); 0 when read from a file
    /// written before versions existed.
    #[serde(default)]
    pub version: u32,
    /// Smoothed duration estimate per test, in ms.
    pub durations: HashMap<String, u64>,
    #[serde(default)]
    pub resource_baselines: HashMap<String, ResourceBaseline>,
    /// Recent outcomes per test, oldest first.
    #[serde(default)]
    pub outcomes: HashMap<String, Vec<HistoryOutcome>>,
    /// When each test last ran (unix seconds).
    #[serde(default)]
    pub last_run: HashMap<String, u64>,
    /// Recent outcomes per test and device profile ([`DeviceProfile`]'s
    /// label, e.g. `iPhone SE / iOS 17`), oldest first. Tells a test that
    /// is flaky everywhere from one that is flaky on one model or OS.
    #[serde(
        default,
        rename = "byDevice",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub by_device: HashMap<String, HashMap<String, Vec<HistoryOutcome>>>,
    /// When each test last ran on each device profile (unix seconds),
    /// keyed like [`by_device`](Self::by_device). Merging shared history
    /// ranks a profile's outcomes by this, not by the test-wide
    /// `last_run`: every matrix job carries the base copy of every other
    /// profile, and only the job that ran a profile has its new outcomes.
    /// Empty in files written before it existed.
    #[serde(
        default,
        rename = "byDeviceLastRun",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub by_device_last_run: HashMap<String, HashMap<String, u64>>,
    /// Per test, the kind of each failed or flaky run in `outcomes`,
    /// oldest first: the last kind goes with the last such run. Runs older
    /// than the first kind (written before kinds were) are of unknown
    /// kind; see [`TestHistory::flakiness_by_kind`].
    #[serde(
        default,
        rename = "failureKinds",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub failure_kinds: HashMap<String, Vec<FailureKind>>,
}

/// What history keys a device by: its platform, model and OS major
/// version. Two simulators of the same model and OS are one profile.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceProfile {
    pub platform: Platform,
    pub model: String,
    pub os_major: Option<u32>,
}

impl DeviceProfile {
    pub fn from_info(info: &DeviceInfo) -> Self {
        Self {
            platform: info.platform,
            model: info.name.trim().to_string(),
            os_major: info.os_version.as_deref().and_then(os_major),
        }
    }

    /// `iPhone SE / iOS 17`, `Pixel 7 / Android 13`: the history key, and
    /// what reports print.
    pub fn label(&self) -> String {
        let os = match self.platform {
            Platform::Ios => "iOS",
            Platform::Android => "Android",
            Platform::Web => "Web",
        };
        match self.os_major {
            Some(v) => format!("{} / {os} {v}", self.model),
            None => format!("{} / {os}", self.model),
        }
    }
}

/// The first number in an OS version string: `17.2` → 17,
/// `com.apple.CoreSimulator.SimRuntime.iOS-17-2` → 17, `13` → 13.
fn os_major(v: &str) -> Option<u32> {
    let start = v.find(|c: char| c.is_ascii_digit())?;
    let digits: String = v[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Profiles of the driver's devices, by device id. Empty if the driver
/// can't list them (history then records the aggregate only).
pub async fn device_profiles(driver: &dyn PlatformDriver) -> HashMap<String, DeviceProfile> {
    driver
        .list_devices()
        .await
        .map(|devices| {
            devices
                .iter()
                .map(|d| (d.id.clone(), DeviceProfile::from_info(d)))
                .collect()
        })
        .unwrap_or_default()
}

/// Share of runs that flipped (pass↔fail) or passed only on retry; `None`
/// under two runs.
fn flakiness_of(runs: &[HistoryOutcome]) -> Option<f64> {
    if runs.len() < 2 {
        return None;
    }
    let flaky = runs.iter().filter(|o| **o == HistoryOutcome::Flaky).count();
    let flips = runs
        .windows(2)
        .filter(|w| (w[0] == HistoryOutcome::Fail) != (w[1] == HistoryOutcome::Fail))
        .count();
    Some((flaky + flips) as f64 / (runs.len() + runs.len() - 1) as f64)
}

/// The kind of each run in `runs` (`None` for a pass), given `kinds` for
/// the newest failed or flaky ones; older ones are `Unknown`.
fn kinds_per_run(runs: &[HistoryOutcome], kinds: &[FailureKind]) -> Vec<Option<FailureKind>> {
    let failing = runs.iter().filter(|o| **o != HistoryOutcome::Pass).count();
    let mut known = kinds[kinds.len().saturating_sub(failing)..].iter();
    let mut unknown = failing.saturating_sub(kinds.len());
    runs.iter()
        .map(|o| {
            if *o == HistoryOutcome::Pass {
                None
            } else if unknown > 0 {
                unknown -= 1;
                Some(FailureKind::Unknown)
            } else {
                known.next().copied().or(Some(FailureKind::Unknown))
            }
        })
        .collect()
}

/// [`flakiness_of`] split by failure kind: a retried run counts for its
/// first attempt's kind, a flip for the kind of the failed run in it (a
/// flip has exactly one). The shares add up to the overall score. Worst
/// first; kinds that never flaked are left out.
fn flakiness_by_kind_of(runs: &[HistoryOutcome], kinds: &[FailureKind]) -> Vec<(FailureKind, f64)> {
    if runs.len() < 2 {
        return Vec::new();
    }
    let per_run = kinds_per_run(runs, kinds);
    let kind = |i: usize| per_run[i].unwrap_or(FailureKind::Unknown);
    let mut counts: std::collections::BTreeMap<FailureKind, usize> = Default::default();
    for (i, o) in runs.iter().enumerate() {
        if *o == HistoryOutcome::Flaky {
            *counts.entry(kind(i)).or_default() += 1;
        }
    }
    for i in 1..runs.len() {
        let (before, after) = (
            runs[i - 1] == HistoryOutcome::Fail,
            runs[i] == HistoryOutcome::Fail,
        );
        if before != after {
            *counts
                .entry(kind(if before { i - 1 } else { i }))
                .or_default() += 1;
        }
    }
    let total = (runs.len() + runs.len() - 1) as f64;
    let mut out: Vec<(FailureKind, f64)> = counts
        .into_iter()
        .map(|(k, n)| (k, n as f64 / total))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// The flag rule (see [`TestHistory::flagged_flaky`]) on one run list.
fn flagged(runs: &[HistoryOutcome]) -> bool {
    if runs.len() < FLAG_MIN_RUNS {
        return false;
    }
    let cleared = runs.len() >= CLEAR_AFTER
        && runs[runs.len() - CLEAR_AFTER..]
            .iter()
            .all(|o| *o == HistoryOutcome::Pass);
    !cleared && flakiness_of(runs).is_some_and(|f| f >= FLAG_THRESHOLD)
}

impl TestHistory {
    /// Share of recent runs where the result flipped (pass↔fail) or passed
    /// only on retry. `None` until a test has 2+ recorded runs. Flip rate,
    /// not failure rate, is what separates a flaky test from a broken one.
    pub fn flakiness(&self, test: &str) -> Option<f64> {
        flakiness_of(self.outcomes.get(test)?)
    }

    /// [`flakiness`](Self::flakiness) split by what the runs failed on,
    /// worst first; the shares add up to it. Empty under two runs.
    pub fn flakiness_by_kind(&self, test: &str) -> Vec<(FailureKind, f64)> {
        let Some(runs) = self.outcomes.get(test) else {
            return Vec::new();
        };
        let kinds = self.failure_kinds.get(test).map_or(&[][..], Vec::as_slice);
        flakiness_by_kind_of(runs, kinds)
    }

    /// `timeout 25%, not-found 10%`: [`flakiness_by_kind`](Self::flakiness_by_kind)
    /// for a report. `None` when no flaky run has a known kind (history
    /// recorded before kinds were), which would only repeat the total.
    pub fn kind_note(&self, test: &str) -> Option<String> {
        let by_kind = self.flakiness_by_kind(test);
        if by_kind.iter().all(|(k, _)| *k == FailureKind::Unknown) {
            return None;
        }
        let parts: Vec<String> = by_kind
            .iter()
            .map(|(k, f)| format!("{} {:.0}%", k.label(), f * 100.0))
            .collect();
        Some(parts.join(", "))
    }

    /// Flagged as flaky: enough runs to judge (Buildkite/Trunk use a minimum
    /// sample), a flip-or-retry rate of at least 15%, and not cleared by a
    /// clean recent streak — a fixed test drops the flag after
    /// `CLEAR_AFTER` straight passes instead of waiting for the window to
    /// roll over.
    pub fn flagged_flaky(&self, test: &str) -> bool {
        self.outcomes.get(test).is_some_and(|runs| flagged(runs))
    }

    /// Device profiles on which `test` is flagged flaky by the same rule,
    /// with their flakiness and run count, worst first.
    pub fn flaky_profiles(&self, test: &str) -> Vec<(String, f64, usize)> {
        let mut out: Vec<(String, f64, usize)> = self
            .by_device
            .get(test)
            .map(|m| {
                m.iter()
                    .filter(|(_, runs)| flagged(runs))
                    .map(|(p, runs)| (p.clone(), flakiness_of(runs).unwrap_or(0.0), runs.len()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// `flaky only on iPhone SE / iOS 17` when the test is flagged on some
    /// device profiles while others ran it enough times and stayed clean;
    /// `None` when the flakiness doesn't depend on the device (or there is
    /// too little per-device history to say).
    pub fn device_note(&self, test: &str) -> Option<String> {
        let flaky = self.flaky_profiles(test);
        if flaky.is_empty() {
            return None;
        }
        let clean = self
            .by_device
            .get(test)?
            .values()
            .filter(|runs| runs.len() >= FLAG_MIN_RUNS && !flagged(runs))
            .count();
        if clean == 0 {
            return None;
        }
        let names: Vec<&str> = flaky.iter().map(|(p, _, _)| p.as_str()).collect();
        Some(format!("flaky only on {}", names.join(", ")))
    }

    /// Chance that one run of `test` fails (a straight failure, or a first
    /// attempt that needed a retry), from the recent window. `None` with
    /// fewer than `FLAG_MIN_RUNS` runs.
    pub fn fail_rate(&self, test: &str) -> Option<f64> {
        let runs = self.outcomes.get(test)?;
        if runs.len() < FLAG_MIN_RUNS {
            return None;
        }
        let failing = runs.iter().filter(|o| **o != HistoryOutcome::Pass).count();
        Some(failing as f64 / runs.len() as f64)
    }

    /// Duration estimate for `test`: its own, else the median of the known
    /// ones (a new test in a suite of 40 s flows isn't a 5 s test), else 5 s.
    pub fn estimate_ms(&self, test: &str) -> u64 {
        self.durations
            .get(test)
            .copied()
            .unwrap_or_else(|| self.median_ms())
    }

    /// Median of the known durations; 5 s with none. 0 ms entries (a
    /// cached pass recorded by an older Velocity) are not durations.
    pub fn median_ms(&self) -> u64 {
        let mut all: Vec<u64> = self
            .durations
            .values()
            .copied()
            .filter(|&d| d > 0)
            .collect();
        if all.is_empty() {
            return 5000;
        }
        all.sort_unstable();
        all[all.len() / 2]
    }

    /// Whether the most recent run of `test` failed.
    pub fn failed_last(&self, test: &str) -> bool {
        self.outcomes
            .get(test)
            .and_then(|r| r.last())
            .is_some_and(|o| *o == HistoryOutcome::Fail)
    }
}

/// Load test history from a JSON file at the given directory path.
///
/// The file is expected at `{path}/velocity-history.json`. Returns an empty
/// history if the file does not exist.
pub fn load(path: &str) -> Result<TestHistory> {
    let file_path = std::path::Path::new(path).join(HISTORY_FILENAME);
    if !file_path.exists() {
        return Ok(TestHistory::default());
    }
    load_file(&file_path)
}

/// Load a history file by its own path (a folder means its
/// `velocity-history.json`). Unlike [`load`], a missing file is an error.
pub fn load_file(file_path: &std::path::Path) -> Result<TestHistory> {
    let joined;
    let file_path = if file_path.is_dir() {
        joined = file_path.join(HISTORY_FILENAME);
        joined.as_path()
    } else {
        file_path
    };
    let contents = std::fs::read_to_string(file_path).map_err(|e| {
        VelocityError::Config(format!(
            "failed to read history file {}: {e}",
            file_path.display()
        ))
    })?;

    let mut history: TestHistory = serde_json::from_str(&contents).map_err(|e| {
        VelocityError::Config(format!(
            "failed to parse history file {}: {e}",
            file_path.display()
        ))
    })?;
    if history.version > HISTORY_VERSION {
        tracing::warn!(
            "{} is version {} (this Velocity writes {HISTORY_VERSION}); reading what it knows",
            file_path.display(),
            history.version
        );
    }
    migrate(&mut history);
    Ok(history)
}

/// The durations `--shard-timings` balances shards by (D-23): a
/// `velocity-history.json` (or a folder holding one), a JUnit XML report,
/// or a folder of JUnit reports (merged). Every shard must read the same
/// source; the CLI loads it once and a failure is fatal.
pub fn load_timings(path: &std::path::Path) -> Result<TestHistory> {
    if path.is_dir() {
        if path.join(HISTORY_FILENAME).exists() {
            return load_file(path);
        }
        let mut xmls: Vec<std::path::PathBuf> = std::fs::read_dir(path)
            .map_err(|e| VelocityError::Config(format!("{}: {e}", path.display())))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")))
            .collect();
        xmls.sort();
        if xmls.is_empty() {
            return Err(VelocityError::Config(format!(
                "{}: no {HISTORY_FILENAME} and no JUnit .xml report in it",
                path.display()
            )));
        }
        let mut merged = TestHistory::default();
        for xml in xmls {
            merge_max(&mut merged, junit_durations(&xml)?);
        }
        return Ok(merged);
    }
    let is_xml = path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("xml"))
        || first_bytes(path, 4096)
            .iter()
            .copied()
            .find(|c| !c.is_ascii_whitespace())
            == Some(b'<');
    if is_xml {
        let mut h = TestHistory::default();
        merge_max(&mut h, junit_durations(path)?);
        Ok(h)
    } else {
        load_file(path)
    }
}

/// The longer duration wins when a test appears in several reports, so
/// the result doesn't depend on the order they are read in.
fn merge_max(into: &mut TestHistory, from: HashMap<String, u64>) {
    for (name, ms) in from {
        let e = into.durations.entry(name).or_insert(0);
        *e = (*e).max(ms);
    }
}

/// Up to `n` bytes from the start of `path` (none if it can't be read):
/// enough to sniff its format without reading a multi-GB report whole.
fn first_bytes(path: &std::path::Path, n: u64) -> Vec<u8> {
    use std::io::Read;
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(n).read_to_end(&mut buf);
    }
    buf
}

/// The largest JUnit report read for timings. A bigger one (a report with
/// every test's `<system-out>`) is skipped with a warning: timings are a
/// hint, not worth gigabytes of reading.
const MAX_JUNIT_BYTES: u64 = if cfg!(test) { 64 << 10 } else { 64 << 20 };

/// A JUnit `time` (seconds) in ms. Reporters write it in their locale:
/// `1,200.000` (thousands comma), `12,5` (.NET or older Java with a decimal
/// comma), `1.234,5`. The rule:
/// - with both marks, the last one is the decimal point;
/// - a lone comma followed by exactly three digits, after one to three
///   digits that aren't just `0`, is a thousands separator: `1,200` is
///   1200 s, as Java's English `NumberFormat` writes it (review-d7 m10);
/// - any other lone comma is a decimal comma: `12,5`, `0,125`, `1,2345`
///   (it read as 125 s for `12,5`, review d4 m7);
/// - several commas alone are thousands separators.
fn junit_seconds_ms(raw: &str) -> Option<u64> {
    let t = raw.trim();
    let thousands = |c: usize| {
        let (int, frac) = (&t[..c], &t[c + 1..]);
        frac.len() == 3
            && frac.bytes().all(|b| b.is_ascii_digit())
            && (1..=3).contains(&int.len())
            && int.bytes().all(|b| b.is_ascii_digit())
            && int != "0"
    };
    let normal = match (t.rfind(','), t.rfind('.')) {
        (Some(c), Some(d)) if c > d => t.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => t.replace(',', ""),
        (Some(c), None) if t.matches(',').count() == 1 && thousands(c) => t.replace(',', ""),
        (Some(_), None) if t.matches(',').count() == 1 => t.replace(',', "."),
        (Some(_), None) => t.replace(',', ""),
        _ => t.to_string(),
    };
    normal
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * 1000.0).round() as u64)
}

/// `testcase@name` → `time` (seconds) in ms, from a JUnit report. Failed,
/// errored and skipped cases are left out: a failure stops early, so its
/// time says little about the test (Flank's rule), and a skip is free.
/// The report is streamed, not read whole; an attribute that doesn't
/// unescape (an entity XML doesn't define, bytes that aren't UTF-8) is
/// taken as written instead of failing the whole run.
fn junit_durations(path: &std::path::Path) -> Result<HashMap<String, u64>> {
    use quick_xml::events::Event;
    let bad = |e: String| VelocityError::Config(format!("JUnit timings {}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(|e| bad(e.to_string()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size > MAX_JUNIT_BYTES {
        tracing::warn!(
            "JUnit timings {}: {} MB is over the {} MB limit; skipped",
            path.display(),
            size >> 20,
            MAX_JUNIT_BYTES >> 20
        );
        return Ok(HashMap::new());
    }
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(file));
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    // The open testcase: (name, ms, disqualified).
    let mut open: Option<(String, Option<u64>, bool)> = None;
    let mut cases = 0usize;
    loop {
        buf.clear();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| bad(e.to_string()))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.name().as_ref() {
                    b"testcase" => {
                        cases += 1;
                        let mut name = None;
                        let mut ms = None;
                        for a in e.attributes().flatten() {
                            let v = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .map(|v| v.into_owned())
                                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
                            match a.key.as_ref() {
                                b"name" => name = Some(v),
                                b"time" => ms = junit_seconds_ms(&v),
                                _ => {}
                            }
                        }
                        let case = (name.unwrap_or_default(), ms, false);
                        if empty {
                            record_case(&mut out, case);
                        } else {
                            open = Some(case);
                        }
                    }
                    b"failure" | b"error" | b"skipped" => {
                        if let Some(c) = open.as_mut() {
                            c.2 = true;
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"testcase" => {
                if let Some(c) = open.take() {
                    record_case(&mut out, c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if cases == 0 {
        return Err(bad("no <testcase> in it".into()));
    }
    Ok(out)
}

fn record_case(
    out: &mut HashMap<String, u64>,
    (name, ms, disqualified): (String, Option<u64>, bool),
) {
    if disqualified || name.is_empty() {
        return;
    }
    if let Some(ms) = ms {
        let e = out.entry(name).or_insert(0);
        *e = (*e).max(ms);
    }
}

/// Apply `change` to the history saved in folder `path` and save it, under
/// a lock and on a fresh read: runs sharing the folder (shards, the
/// daemon, two terminals) each add their results instead of the last
/// writer replacing the others' with the copy it loaded at its start.
/// Returns the saved history.
pub fn update_saved(path: &str, change: impl FnOnce(&mut TestHistory)) -> Result<TestHistory> {
    let file = std::path::Path::new(path).join(HISTORY_FILENAME);
    velocity_common::fsutil::with_lock(&file, || {
        // A file that can't be read (permissions, a directory) is an
        // error; only one that reads but doesn't parse is moved aside.
        if let Err(e) = std::fs::read(&file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(VelocityError::Config(format!(
                    "failed to read history file {}: {e}",
                    file.display()
                )));
            }
        }
        let mut history = load(path).unwrap_or_else(|e| {
            // Keep what can't be read instead of overwriting it (VEL-183).
            let aside = file.with_extension(format!("json.corrupt-{}", now_secs()));
            let _ = std::fs::rename(&file, &aside);
            tracing::warn!(
                "{e}; moved it to {} and started a new history",
                aside.display()
            );
            TestHistory::default()
        });
        change(&mut history);
        save(path, &history).map(|()| history)
    })
    .map_err(|e| VelocityError::io(format!("failed to lock {}", file.display()), e))?
}

/// Bring a history read from an older file up to [`HISTORY_VERSION`].
/// Version 1 kept outcomes per test only, which is exactly the aggregate;
/// per-device outcomes start empty.
pub fn migrate(history: &mut TestHistory) {
    if history.version < HISTORY_VERSION {
        history.version = HISTORY_VERSION;
    }
    // A hand-edited or corrupt duration (u64::MAX) overflowed the sharder's
    // sums (VEL-182). No test runs for a day.
    for ms in history.durations.values_mut() {
        *ms = (*ms).min(MAX_DURATION_MS);
    }
}

/// Longest duration history keeps for a test.
const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;

/// A test not run for this long is dropped from the history.
const FORGET_AFTER_SECS: u64 = 90 * 24 * 60 * 60;

/// Drop tests that haven't run for `FORGET_AFTER_SECS` and are no longer
/// in the suite (`suite_tests`, every test's name, not only those this run
/// selected): deleted and renamed tests stayed in velocity-history.json
/// forever, and the file only grew (VEL-189). A test still in the suite is
/// kept however long ago it ran: tests outside a `--tags` or shard
/// selection, release-only tags and `skip:`ped tests lost their duration
/// estimates and flake history (m-6). Tests with no run time (older files)
/// are kept.
pub fn forget_stale(history: &mut TestHistory, suite_tests: &std::collections::HashSet<String>) {
    forget_stale_at(history, suite_tests, now_secs());
}

fn forget_stale_at(
    history: &mut TestHistory,
    suite_tests: &std::collections::HashSet<String>,
    now: u64,
) {
    let stale: Vec<String> = history
        .last_run
        .iter()
        .filter(|(name, &at)| {
            now.saturating_sub(at) > FORGET_AFTER_SECS && !suite_tests.contains(*name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    for name in stale {
        history.last_run.remove(&name);
        history.durations.remove(&name);
        history.outcomes.remove(&name);
        history.resource_baselines.remove(&name);
        history.by_device.remove(&name);
        history.by_device_last_run.remove(&name);
        history.failure_kinds.remove(&name);
    }
}

/// Save test history to a JSON file at the given directory path.
pub fn save(path: &str, history: &TestHistory) -> Result<()> {
    let dir = std::path::Path::new(path);
    std::fs::create_dir_all(dir).map_err(|e| {
        VelocityError::io(
            format!("failed to create history directory {}", dir.display()),
            e,
        )
    })?;

    let file_path = dir.join(HISTORY_FILENAME);
    let mut history = history.clone();
    history.version = history.version.max(HISTORY_VERSION);
    let json = serde_json::to_string_pretty(&history)
        .map_err(|e| VelocityError::io("failed to serialize history", e))?;

    // Atomic: a torn file would fail to parse and cost the whole history.
    velocity_common::fsutil::write_private(&file_path, json.as_bytes()).map_err(|e| {
        VelocityError::io(
            format!("failed to write history file {}", file_path.display()),
            e,
        )
    })?;

    Ok(())
}

/// A run where more than this share of tests failed is an outage (backend
/// down, wrong build), not evidence about individual tests (Trunk).
const OUTAGE_FAIL_SHARE: f64 = 0.8;
/// Smallest run the outage rule applies to.
const OUTAGE_MIN_TESTS: usize = 5;

/// Whether `results` look like an outage rather than test failures.
pub fn is_outage(results: &[TestResult]) -> bool {
    let counted: Vec<&TestResult> = results
        .iter()
        .filter(|r| r.status != TestStatus::Skipped && !r.infrastructure_failure)
        .collect();
    if counted.len() < OUTAGE_MIN_TESTS {
        return false;
    }
    let failed = counted
        .iter()
        .filter(|r| r.status == TestStatus::Failed)
        .count();
    failed as f64 / counted.len() as f64 > OUTAGE_FAIL_SHARE
}

/// Update the history with durations and outcomes from the latest results.
/// Skipped tests are left out — a 0ms entry would make the sharder think
/// they're free. An outage run (see [`is_outage`]) records nothing.
pub fn update(history: &mut TestHistory, results: &[TestResult]) {
    update_with_devices(history, results, &HashMap::new());
}

/// [`update`], also recording each outcome under the device profile the
/// test ran on (`profiles` maps device ids, see [`device_profiles`]).
pub fn update_with_devices(
    history: &mut TestHistory,
    results: &[TestResult],
    profiles: &HashMap<String, DeviceProfile>,
) {
    if is_outage(results) {
        tracing::info!("most tests failed: treating the run as an outage; history not updated");
        return;
    }
    for result in results {
        // A device or connection failure says nothing about the test; keep
        // it out of the flakiness window and the duration estimate.
        // Nor does Velocity's own bug (a panic, D-19).
        if result.infrastructure_failure || result.internal_error {
            continue;
        }
        // A cache hit didn't run: recording it added a 0 ms pass, which
        // hid flakiness and made the sharder think the test free (VEL-045).
        if result.from_cache {
            continue;
        }
        let outcome = match result.status {
            TestStatus::Passed => HistoryOutcome::Pass,
            TestStatus::Failed => HistoryOutcome::Fail,
            TestStatus::Retried => HistoryOutcome::Flaky,
            TestStatus::Skipped => continue,
        };
        let now = now_secs();
        // A file last written by a Velocity without `byDeviceLastRun` (a
        // mixed-version matrix, review-fi2 F5): its profiles ran no later
        // than the test's previous run. Written down before this run
        // replaces it, so the rewrite keeps a time for every profile and
        // a merge doesn't take them all for this run's.
        if let Some(&before) = history.last_run.get(&result.test_name) {
            if let Some(known) = history.by_device.get(&result.test_name) {
                let times = history
                    .by_device_last_run
                    .entry(result.test_name.clone())
                    .or_default();
                for profile in known.keys() {
                    times.entry(profile.clone()).or_insert(before);
                }
            }
        }
        history.last_run.insert(result.test_name.clone(), now);
        let runs = history
            .outcomes
            .entry(result.test_name.clone())
            .or_default();
        runs.push(outcome);
        if runs.len() > OUTCOME_WINDOW {
            runs.drain(..runs.len() - OUTCOME_WINDOW);
        }
        let failing = runs.iter().filter(|o| **o != HistoryOutcome::Pass).count();
        if outcome != HistoryOutcome::Pass {
            history
                .failure_kinds
                .entry(result.test_name.clone())
                .or_default()
                .push(FailureKind::of_result(result));
        }
        // Kinds of runs that left the window go with them.
        if let Some(kinds) = history.failure_kinds.get_mut(&result.test_name) {
            kinds.drain(..kinds.len().saturating_sub(failing));
            if kinds.is_empty() {
                history.failure_kinds.remove(&result.test_name);
            }
        }
        if let Some(profile) = result.device.as_ref().and_then(|d| profiles.get(d)) {
            let runs = history
                .by_device
                .entry(result.test_name.clone())
                .or_default()
                .entry(profile.label())
                .or_default();
            runs.push(outcome);
            if runs.len() > OUTCOME_WINDOW {
                runs.drain(..runs.len() - OUTCOME_WINDOW);
            }
            history
                .by_device_last_run
                .entry(result.test_name.clone())
                .or_default()
                .insert(profile.label(), now);
        }

        // Only clean passes update an existing estimate: a failure stops early
        // and a retried run counts every attempt, so either would skew it.
        let duration_ms = result.duration.as_millis() as u64;
        let estimate = match history.durations.get(&result.test_name) {
            Some(&old) if outcome != HistoryOutcome::Pass => old,
            Some(&old) => {
                (DURATION_ALPHA * duration_ms as f64 + (1.0 - DURATION_ALPHA) * old as f64) as u64
            }
            None => duration_ms,
        };
        history.durations.insert(result.test_name.clone(), estimate);

        // Only a clean pass becomes the memory baseline. A run that failed
        // (a memory breach included) or passed on retry would otherwise
        // be the next run's baseline, and the breach would pass then.
        if let (Some(peak), HistoryOutcome::Pass) = (&result.resource_peak, outcome) {
            history.resource_baselines.insert(
                result.test_name.clone(),
                ResourceBaseline {
                    peak_pss_kb: peak.total_pss_kb,
                    peak_java_heap_kb: peak.java_heap_kb,
                    avg_cpu_percent: peak.cpu_percent,
                },
            );
        }
    }
}

/// Lines naming this run's tests that history flags as flaky (overall, or
/// on some device profile), worst first, with what they fail on and where
/// they flake when that depends on the device.
pub fn flaky_report(history: &TestHistory, results: &[TestResult]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut flagged: Vec<(f64, &str, usize)> = results
        .iter()
        .filter(|r| seen.insert(r.test_name.as_str()))
        .filter(|r| {
            history.flagged_flaky(&r.test_name) || !history.flaky_profiles(&r.test_name).is_empty()
        })
        .map(|r| {
            (
                history.flakiness(&r.test_name).unwrap_or(0.0),
                r.test_name.as_str(),
                history.outcomes.get(&r.test_name).map_or(0, Vec::len),
            )
        })
        .collect();
    flagged.sort_by(|a, b| b.0.total_cmp(&a.0));
    flagged
        .into_iter()
        .map(|(score, name, n)| {
            let mut line = format!("{name}: flakiness {:.0}% over {n} runs", score * 100.0);
            if let Some(kinds) = history.kind_note(name) {
                line.push_str(&format!(" ({kinds})"));
            }
            if let Some(note) = history.device_note(name) {
                line.push_str("; ");
                line.push_str(&note);
            }
            line
        })
        .collect()
}

/// Add the device note (`flaky only on iPhone SE / iOS 17`) to the results
/// of tests whose flakiness depends on the device, so the reports (HTML,
/// JUnit, GitHub) say where it flakes.
pub fn note_device_flakiness(history: &TestHistory, results: &mut [TestResult]) {
    for r in results {
        if let Some(note) = history.device_note(&r.test_name) {
            let note = format!("history: {note}");
            if !r.warnings.contains(&note) {
                r.warnings.push(note);
            }
        }
    }
}

/// [`note_device_flakiness`], and for each test history flags as flaky,
/// what its flaky runs failed on (`history: flakiness 35% over 20 runs
/// (timeout 25%, not-found 10%)`), so the reports say how it flakes, not
/// only that it does.
pub fn note_flakiness(history: &TestHistory, results: &mut [TestResult]) {
    note_device_flakiness(history, results);
    for r in results {
        if !history.flagged_flaky(&r.test_name) {
            continue;
        }
        let Some(kinds) = history.kind_note(&r.test_name) else {
            continue;
        };
        let note = format!(
            "history: flakiness {:.0}% over {} runs ({kinds})",
            history.flakiness(&r.test_name).unwrap_or(0.0) * 100.0,
            history.outcomes.get(&r.test_name).map_or(0, Vec::len)
        );
        if !r.warnings.contains(&note) {
            r.warnings.push(note);
        }
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The memory check (`config.performance`): a test whose peak total PSS
/// rose more than `threshold_pct` above its saved baseline fails. Runs on
/// the result before it is cached, reported or recorded, in both runners.
/// Returns whether the test was failed for it.
pub fn apply_memory_check(
    result: &mut TestResult,
    baseline: Option<&ResourceBaseline>,
    threshold_pct: u32,
) -> bool {
    if !matches!(result.status, TestStatus::Passed | TestStatus::Retried) {
        return false;
    }
    let (Some(peak), Some(baseline)) = (&result.resource_peak, baseline) else {
        return false;
    };
    let Some(why) = check_regression(baseline, peak, threshold_pct) else {
        return false;
    };
    result.status = TestStatus::Failed;
    result.error_message = Some(format!(
        "memory regression: {why}, over the {threshold_pct}% allowed (config.performance.pssGrowthThresholdPct)"
    ));
    true
}

/// Whether the peak total PSS exceeds the baseline by more than
/// `threshold_pct`; the reason when it does.
pub fn check_regression(
    baseline: &ResourceBaseline,
    current_peak: &ResourceSnapshot,
    threshold_pct: u32,
) -> Option<String> {
    if baseline.peak_pss_kb == 0 {
        return None;
    }
    let growth_pct =
        ((current_peak.total_pss_kb as f64 / baseline.peak_pss_kb as f64) - 1.0) * 100.0;
    if growth_pct > threshold_pct as f64 {
        Some(format!(
            "PSS grew {growth_pct:.1}% vs baseline ({}KB -> {}KB)",
            baseline.peak_pss_kb, current_peak.total_pss_kb
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use velocity_common::TestStatus;

    fn make_result(name: &str, duration_ms: u64) -> TestResult {
        TestResult {
            test_name: name.to_string(),
            status: TestStatus::Passed,
            duration: Duration::from_millis(duration_ms),
            steps: vec![],
            retries: 0,
            error_message: None,
            screenshots: vec![],
            resource_peak: None,
            warnings: vec![],
            infrastructure_failure: false,
            failed_attempts: vec![],
            source: None,
            trace: None,
            device: None,
            quarantined: None,
            from_cache: false,
            tags: vec![],
            internal_error: false,
            verification: vec![],
            attachments: Vec::new(),
        }
    }

    fn with_pss(name: &str, pss_kb: u64) -> TestResult {
        let mut r = make_result(name, 100);
        r.resource_peak = Some(ResourceSnapshot {
            java_heap_kb: 0,
            native_heap_kb: 0,
            total_pss_kb: pss_kb,
            cpu_percent: 0.0,
            timestamp_ms: 0,
        });
        r
    }

    // NEW-021: a breach fails the test, and the run after it still fails
    // (the breached run never becomes the baseline).
    #[test]
    fn a_memory_breach_fails_and_keeps_failing_until_memory_comes_down() {
        let mut history = TestHistory::default();
        let first = with_pss("t", 100_000);
        update(&mut history, std::slice::from_ref(&first));
        assert_eq!(history.resource_baselines["t"].peak_pss_kb, 100_000);

        for _ in 0..2 {
            let mut r = with_pss("t", 130_000);
            let base = history.resource_baselines.get("t");
            assert!(apply_memory_check(&mut r, base, 15));
            assert_eq!(r.status, TestStatus::Failed);
            let msg = r.error_message.as_deref().unwrap();
            assert!(
                msg.starts_with("memory regression: PSS grew 30.0%"),
                "{msg}"
            );
            update(&mut history, std::slice::from_ref(&r));
            assert_eq!(history.resource_baselines["t"].peak_pss_kb, 100_000);
        }

        let mut ok = with_pss("t", 110_000);
        assert!(!apply_memory_check(
            &mut ok,
            history.resource_baselines.get("t"),
            15
        ));
        assert_eq!(ok.status, TestStatus::Passed);
        update(&mut history, std::slice::from_ref(&ok));
        assert_eq!(history.resource_baselines["t"].peak_pss_kb, 110_000);
    }

    #[test]
    fn the_memory_check_leaves_failures_and_unprofiled_runs_alone() {
        let base = ResourceBaseline {
            peak_pss_kb: 100,
            peak_java_heap_kb: 0,
            avg_cpu_percent: 0.0,
        };
        let mut failed = with_pss("t", 500);
        failed.status = TestStatus::Failed;
        failed.error_message = Some("element not found".into());
        assert!(!apply_memory_check(&mut failed, Some(&base), 15));
        assert_eq!(failed.error_message.as_deref(), Some("element not found"));
        let mut no_peak = make_result("t", 100);
        assert!(!apply_memory_check(&mut no_peak, Some(&base), 15));
        let mut no_base = with_pss("t", 500);
        assert!(!apply_memory_check(&mut no_base, None, 15));
        assert_eq!(no_base.status, TestStatus::Passed);
    }

    // VEL-189: tests not run for 90 days leave the history.
    #[test]
    fn tests_not_run_for_months_are_forgotten() {
        let mut history = TestHistory::default();
        let old = now_secs() - FORGET_AFTER_SECS - 10;
        history.last_run.insert("deleted".into(), old);
        history.durations.insert("deleted".into(), 100);
        history
            .outcomes
            .insert("deleted".into(), vec![HistoryOutcome::Pass]);
        history.durations.insert("no-date".into(), 100);
        update(&mut history, &[make_result("ran", 1500)]);
        let suite: std::collections::HashSet<String> = ["ran".to_string()].into();
        forget_stale(&mut history, &suite);
        assert!(!history.durations.contains_key("deleted"));
        assert!(!history.outcomes.contains_key("deleted"));
        assert!(!history.last_run.contains_key("deleted"));
        assert!(history.durations.contains_key("no-date"));
        assert!(history.durations.contains_key("ran"));
    }

    // m-6: a test still in the suite but not run for 90 days (outside the
    // `--tags`/shard selection, release-only) lost its history.
    #[test]
    fn rarely_run_tests_still_in_the_suite_are_kept() {
        let mut history = TestHistory::default();
        let old = now_secs() - FORGET_AFTER_SECS - 10;
        for name in ["release-only", "deleted"] {
            history.last_run.insert(name.into(), old);
            history.durations.insert(name.into(), 100);
        }
        update(&mut history, &[make_result("ran", 1500)]);
        assert!(
            history.durations.contains_key("release-only"),
            "a run that didn't select it doesn't prune it"
        );
        let suite: std::collections::HashSet<String> =
            ["ran".to_string(), "release-only".to_string()].into();
        forget_stale(&mut history, &suite);
        assert!(history.durations.contains_key("release-only"));
        assert!(!history.durations.contains_key("deleted"));
    }

    /// xorshift64*, for the C8 property tests (fixed seed: a failure
    /// reproduces).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // C8: the JUnit `time` parser on random input never panics, never
    // returns a negative or non-finite time, and reads every plain or
    // English-grouped number exactly.
    #[test]
    fn junit_times_parse_properly_for_random_input() {
        let mut rng = Rng(0xC8_7133);
        let chars: Vec<char> = "0123456789.,- eE+naif\u{e9}".chars().collect();
        for _ in 0..3000 {
            let len = rng.below(14);
            let s: String = (0..len).map(|_| chars[rng.below(chars.len())]).collect();
            if let Some(ms) = junit_seconds_ms(&s) {
                assert!(
                    ms <= u64::MAX / 2 || s.contains(['e', 'E']),
                    "{s:?} -> {ms}"
                );
            }
        }
        for _ in 0..3000 {
            let ms = rng.next() % 10_000_000_000;
            let (secs, frac) = (ms / 1000, ms % 1000);
            assert_eq!(junit_seconds_ms(&format!("{secs}.{frac:03}")), Some(ms));
            // Grouped the way Java's English NumberFormat writes it.
            let digits = secs.to_string();
            let mut grouped = String::new();
            for (i, c) in digits.chars().enumerate() {
                if i > 0 && (digits.len() - i) % 3 == 0 {
                    grouped.push(',');
                }
                grouped.push(c);
            }
            assert_eq!(
                junit_seconds_ms(&format!("{grouped}.{frac:03}")),
                Some(ms),
                "{grouped}.{frac:03}"
            );
            // A decimal comma (.NET, older Java).
            assert_eq!(junit_seconds_ms(&format!("{secs},{frac:03}0")), Some(ms));
        }
    }

    // C8: a JUnit report with random damage is read or refused, never a
    // panic, and no time it reports is longer than the digits in the file
    // allow (the longest, `1,200`, is 1200 s).
    #[test]
    fn damaged_junit_reports_are_read_or_refused() {
        let dir = std::env::temp_dir().join(format!("vel-c8-junit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base: Vec<char> = r#"<?xml version="1.0"?><testsuites><testsuite name="s" tests="3"><testcase name="a &amp; b" time="1.5"/><testcase name="c" time="1,200"><failure message="x"/></testcase><testcase name="d" time="0,25"></testcase><testcase name="e" time="2"><skipped/></testcase></testsuite></testsuites>"#.chars().collect();
        let noise: Vec<char> = "<>/=\"&;! abc".chars().collect();
        let mut rng = Rng(0xC8_0A11);
        for n in 0..300 {
            let mut s = base.clone();
            for _ in 0..1 + rng.below(5) {
                if s.is_empty() {
                    break;
                }
                let at = rng.below(s.len());
                match rng.below(3) {
                    0 => {
                        s.remove(at);
                    }
                    1 => s.insert(at, noise[rng.below(noise.len())]),
                    _ => s.truncate(at.max(1)),
                }
            }
            let text: String = s.into_iter().collect();
            let path = dir.join(format!("r{n}.xml"));
            std::fs::write(&path, &text).unwrap();
            let got = std::panic::catch_unwind(|| junit_durations(&path));
            let Ok(got) = got else {
                panic!("junit_durations panicked on:\n{text}");
            };
            if let Ok(times) = got {
                for ms in times.values() {
                    assert!(*ms <= 1_200_000, "{text} -> {times:?}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-182: a huge duration in the file is clamped on load.
    #[test]
    fn huge_durations_are_clamped_on_load() {
        let dir = std::env::temp_dir().join(format!("vel-hist-huge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(HISTORY_FILENAME),
            format!(r#"{{"durations":{{"a":{},"b":{}}}}}"#, u64::MAX, u64::MAX),
        )
        .unwrap();
        let h = load(dir.to_str().unwrap()).unwrap();
        let total = h.estimate_ms("a").checked_add(h.estimate_ms("b"));
        assert!(total.is_some(), "{:?}", h.durations);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-045: a result-cache hit is not a 0 ms pass.
    #[test]
    fn cache_hits_are_not_recorded() {
        let mut history = TestHistory::default();
        let mut hit = make_result("cached", 0);
        hit.from_cache = true;
        update(&mut history, &[hit, make_result("ran", 1500)]);
        assert!(!history.durations.contains_key("cached"));
        assert!(!history.outcomes.contains_key("cached"));
        assert_eq!(history.durations["ran"], 1500);
    }

    #[test]
    fn test_update_inserts_new_entries() {
        let mut history = TestHistory::default();
        let results = vec![make_result("test_a", 1500), make_result("test_b", 3200)];

        update(&mut history, &results);

        assert_eq!(history.durations["test_a"], 1500);
        assert_eq!(history.durations["test_b"], 3200);
    }

    #[test]
    fn test_update_smooths_existing() {
        let mut history = TestHistory::default();
        history.durations.insert("test_a".to_string(), 1000);

        let results = vec![make_result("test_a", 2000)];
        update(&mut history, &results);

        assert_eq!(history.durations["test_a"], 1300);
    }

    fn with_status(name: &str, status: TestStatus) -> TestResult {
        TestResult {
            status,
            ..make_result(name, 100)
        }
    }

    #[test]
    fn failed_runs_do_not_move_the_estimate() {
        let mut history = TestHistory::default();
        history.durations.insert("t".to_string(), 1000);
        let failed = TestResult {
            status: TestStatus::Failed,
            ..make_result("t", 50)
        };
        update(&mut history, &[failed]);
        assert_eq!(history.durations["t"], 1000);
    }

    #[test]
    fn infrastructure_failures_are_not_recorded() {
        let mut history = TestHistory::default();
        let mut r = with_status("t", TestStatus::Failed);
        r.infrastructure_failure = true;
        update(&mut history, &[r]);
        assert!(history.outcomes.is_empty() && history.durations.is_empty());
    }

    #[test]
    fn skipped_tests_are_not_recorded() {
        let mut history = TestHistory::default();
        update(&mut history, &[with_status("s", TestStatus::Skipped)]);
        assert!(history.durations.is_empty() && history.outcomes.is_empty());
    }

    #[test]
    fn flakiness_counts_flips_and_retries_not_steady_failure() {
        let mut history = TestHistory::default();
        for _ in 0..4 {
            update(&mut history, &[with_status("broken", TestStatus::Failed)]);
            update(&mut history, &[with_status("steady", TestStatus::Passed)]);
        }
        for s in [
            TestStatus::Passed,
            TestStatus::Failed,
            TestStatus::Retried,
            TestStatus::Passed,
        ] {
            update(&mut history, &[with_status("flaky", s)]);
        }
        assert_eq!(history.flakiness("broken"), Some(0.0));
        assert_eq!(history.flakiness("steady"), Some(0.0));
        assert!(history.flakiness("flaky").unwrap() > 0.3);
        assert!(history.failed_last("broken") && !history.failed_last("flaky"));
    }

    #[test]
    fn outcome_window_is_bounded() {
        let mut history = TestHistory::default();
        for _ in 0..(OUTCOME_WINDOW + 5) {
            update(&mut history, &[with_status("t", TestStatus::Passed)]);
        }
        assert_eq!(history.outcomes["t"].len(), OUTCOME_WINDOW);
    }

    #[test]
    fn old_history_files_still_load() {
        let old: TestHistory = serde_json::from_str(r#"{"durations":{"a":10}}"#).unwrap();
        assert_eq!(old.durations["a"], 10);
        assert!(old.outcomes.is_empty());
    }

    #[test]
    fn test_load_nonexistent_returns_empty() {
        let history = load("/tmp/velocity_test_nonexistent_dir_12345").unwrap();
        assert!(history.durations.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("velocity_history_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut history = TestHistory::default();
        history.durations.insert("login_test".to_string(), 4500);
        history.durations.insert("signup_test".to_string(), 6200);

        let dir_str = dir.to_str().unwrap();
        save(dir_str, &history).unwrap();

        let loaded = load(dir_str).unwrap();
        assert_eq!(loaded.durations["login_test"], 4500);
        assert_eq!(loaded.durations["signup_test"], 6200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn with(name: &str, status: TestStatus) -> TestResult {
        TestResult {
            status,
            ..make_result(name, 1000)
        }
    }

    #[test]
    fn an_outage_run_records_nothing() {
        let mut history = TestHistory::default();
        let mut results: Vec<TestResult> = (0..9)
            .map(|i| with(&format!("t{i}"), TestStatus::Failed))
            .collect();
        results.push(with("t9", TestStatus::Passed));
        assert!(is_outage(&results));
        update(&mut history, &results);
        assert!(history.outcomes.is_empty());
        // Four tests is too few to call an outage.
        assert!(!is_outage(&results[..4]));
    }

    #[test]
    fn flaky_flag_needs_enough_runs_and_clears_after_a_clean_streak() {
        let mut h = TestHistory::default();
        let flip = [
            HistoryOutcome::Pass,
            HistoryOutcome::Fail,
            HistoryOutcome::Pass,
            HistoryOutcome::Flaky,
        ];
        h.outcomes.insert("t".into(), flip.to_vec());
        assert!(!h.flagged_flaky("t"), "4 runs is too few");
        h.outcomes.get_mut("t").unwrap().push(HistoryOutcome::Fail);
        assert!(h.flagged_flaky("t"));
        h.outcomes
            .get_mut("t")
            .unwrap()
            .extend(std::iter::repeat_n(HistoryOutcome::Pass, 10));
        assert!(!h.flagged_flaky("t"), "ten straight passes clear it");
        let report = flaky_report(&h, &[with("t", TestStatus::Passed)]);
        assert!(report.is_empty());
    }

    #[test]
    fn unknown_tests_are_estimated_at_the_median() {
        let mut h = TestHistory::default();
        assert_eq!(h.estimate_ms("new"), 5000);
        for (n, ms) in [("a", 20_000), ("b", 40_000), ("c", 60_000)] {
            h.durations.insert(n.into(), ms);
        }
        assert_eq!(h.estimate_ms("new"), 40_000);
        assert_eq!(h.estimate_ms("a"), 20_000);
    }

    fn on(name: &str, status: TestStatus, device: &str) -> TestResult {
        TestResult {
            device: Some(device.into()),
            ..with(name, status)
        }
    }

    fn profiles() -> HashMap<String, DeviceProfile> {
        let p = |platform, model: &str, os: Option<u32>| DeviceProfile {
            platform,
            model: model.into(),
            os_major: os,
        };
        HashMap::from([
            ("se".to_string(), p(Platform::Ios, "iPhone SE", Some(17))),
            (
                "pro".to_string(),
                p(Platform::Ios, "iPhone 15 Pro", Some(17)),
            ),
            (
                "pixel".to_string(),
                p(Platform::Android, "Pixel 7", Some(13)),
            ),
        ])
    }

    #[test]
    fn device_profiles_read_model_and_os_major() {
        let info = |platform, name: &str, os: Option<&str>| DeviceInfo {
            id: "x".into(),
            name: name.into(),
            platform,
            state: velocity_common::DeviceState::Booted,
            os_version: os.map(String::from),
            device_type: Default::default(),
        };
        let se = DeviceProfile::from_info(&info(
            Platform::Ios,
            "iPhone SE (3rd generation)",
            Some("com.apple.CoreSimulator.SimRuntime.iOS-17-2"),
        ));
        assert_eq!(se.label(), "iPhone SE (3rd generation) / iOS 17");
        let pixel = DeviceProfile::from_info(&info(Platform::Android, "Pixel 7", Some("13")));
        assert_eq!(pixel.label(), "Pixel 7 / Android 13");
        let unknown = DeviceProfile::from_info(&info(Platform::Android, "Pixel 7", None));
        assert_eq!(unknown.label(), "Pixel 7 / Android");
    }

    #[test]
    fn outcomes_are_kept_per_device_profile_and_in_the_aggregate() {
        let mut h = TestHistory::default();
        let profiles = profiles();
        for i in 0..6 {
            let se = if i % 2 == 0 {
                TestStatus::Failed
            } else {
                TestStatus::Passed
            };
            update_with_devices(
                &mut h,
                &[on("pay", se, "se"), on("pay", TestStatus::Passed, "pixel")],
                &profiles,
            );
        }
        assert_eq!(h.outcomes["pay"].len(), 12, "aggregate keeps every run");
        assert_eq!(h.by_device["pay"]["iPhone SE / iOS 17"].len(), 6);
        assert_eq!(h.by_device["pay"]["Pixel 7 / Android 13"].len(), 6);
        assert_eq!(h.durations["pay"], 1000, "durations stay test-wide");
        assert_eq!(
            h.device_note("pay").as_deref(),
            Some("flaky only on iPhone SE / iOS 17")
        );
        let report = flaky_report(&h, &[with("pay", TestStatus::Passed)]);
        assert_eq!(report.len(), 1);
        assert!(
            report[0].ends_with("; flaky only on iPhone SE / iOS 17"),
            "{report:?}"
        );
        let mut results = vec![with("pay", TestStatus::Passed)];
        note_device_flakiness(&h, &mut results);
        assert_eq!(
            results[0].warnings,
            vec!["history: flaky only on iPhone SE / iOS 17".to_string()]
        );
    }

    #[test]
    fn a_run_records_when_each_device_profile_ran_the_test() {
        let mut h = TestHistory::default();
        let before = now_secs();
        update_with_devices(&mut h, &[on("pay", TestStatus::Failed, "se")], &profiles());
        let at = h.by_device_last_run["pay"]["iPhone SE / iOS 17"];
        assert!(at >= before && at <= now_secs(), "{at}");
        assert_eq!(
            h.by_device_last_run["pay"].len(),
            1,
            "only the profile that ran"
        );
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("\"byDeviceLastRun\""), "{json}");
        let back: TestHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.by_device_last_run, h.by_device_last_run);
        // A file an older Velocity rewrote (no profile times): the next
        // run keeps a time for every profile it holds, its previous
        // run's, not this run's (review-fi2 F5).
        let mut old: TestHistory = serde_json::from_str(
            r#"{"durations":{},"last_run":{"pay":100},"byDevice":{"pay":{"Pixel 7 / Android 14":["pass"]}}}"#,
        )
        .unwrap();
        update_with_devices(
            &mut old,
            &[on("pay", TestStatus::Passed, "se")],
            &profiles(),
        );
        assert_eq!(old.by_device_last_run["pay"]["Pixel 7 / Android 14"], 100);
        assert!(old.by_device_last_run["pay"]["iPhone SE / iOS 17"] >= before);
        // Files written before the field existed still load.
        let old: TestHistory =
            serde_json::from_str(r#"{"durations":{},"byDevice":{"pay":{"x":["pass"]}}}"#).unwrap();
        assert!(old.by_device_last_run.is_empty());
    }

    #[test]
    fn a_test_flaky_on_every_device_gets_no_device_note() {
        let mut h = TestHistory::default();
        let profiles = profiles();
        for i in 0..6 {
            let s = if i % 2 == 0 {
                TestStatus::Failed
            } else {
                TestStatus::Passed
            };
            update_with_devices(&mut h, &[on("t", s, "se"), on("t", s, "pro")], &profiles);
        }
        assert_eq!(h.flaky_profiles("t").len(), 2);
        assert_eq!(h.device_note("t"), None);
    }

    #[test]
    fn a_version_one_file_migrates_into_the_aggregate() {
        let dir = std::env::temp_dir().join(format!("velocity-hist-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(HISTORY_FILENAME),
            r#"{"durations":{"a":10},"outcomes":{"a":["pass","fail","pass","flaky","fail"]}}"#,
        )
        .unwrap();
        let dir_str = dir.to_str().unwrap();
        let mut h = load(dir_str).unwrap();
        assert_eq!(h.version, HISTORY_VERSION);
        assert_eq!(h.outcomes["a"].len(), 5);
        assert!(h.flagged_flaky("a"));
        assert!(h.by_device.is_empty());
        update_with_devices(&mut h, &[on("a", TestStatus::Passed, "se")], &profiles());
        save(dir_str, &h).unwrap();
        let text = std::fs::read_to_string(dir.join(HISTORY_FILENAME)).unwrap();
        assert!(
            text.contains(&format!("\"version\": {HISTORY_VERSION}")),
            "{text}"
        );
        assert!(text.contains("\"byDevice\""), "{text}");
        let again = load(dir_str).unwrap();
        assert_eq!(again.outcomes["a"].len(), 6);
        assert_eq!(
            again.by_device["a"]["iPhone SE / iOS 17"],
            vec![HistoryOutcome::Pass]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fail_rate_counts_first_attempt_failures() {
        let mut h = TestHistory::default();
        h.outcomes.insert(
            "t".into(),
            vec![
                HistoryOutcome::Pass,
                HistoryOutcome::Fail,
                HistoryOutcome::Pass,
                HistoryOutcome::Flaky,
                HistoryOutcome::Pass,
            ],
        );
        assert_eq!(h.fail_rate("t"), Some(0.4));
        h.outcomes.insert("new".into(), vec![HistoryOutcome::Fail]);
        assert_eq!(h.fail_rate("new"), None, "too few runs to estimate");
    }

    // Runs sharing a folder each add their outcomes; the last writer used
    // to replace the others' with the copy it loaded at its start.
    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = std::env::temp_dir().join(format!("velocity-hist-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_string_lossy().into_owned();
        std::thread::scope(|s| {
            for t in 0..8 {
                let d = d.clone();
                s.spawn(move || {
                    for i in 0..10 {
                        let r = TestResult::failed(
                            &format!("t{t}-{i}"),
                            "x".into(),
                            std::time::Duration::ZERO,
                        );
                        update_saved(&d, |h| update(h, &[r])).unwrap();
                    }
                });
            }
        });
        assert_eq!(load(&d).unwrap().outcomes.len(), 80);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_history_is_moved_aside_not_overwritten() {
        let dir =
            std::env::temp_dir().join(format!("velocity-hist-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HISTORY_FILENAME), "{ not json").unwrap();
        let d = dir.to_string_lossy().into_owned();
        update_saved(&d, |_| {}).unwrap();
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(kept);
        assert!(load(&d).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R-m4: a history that can't be read (here a directory in its place)
    // is an error, not "corrupt": it was moved aside and replaced.
    #[test]
    fn an_unreadable_history_is_an_error_not_moved_aside() {
        let dir =
            std::env::temp_dir().join(format!("velocity-hist-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(HISTORY_FILENAME)).unwrap();
        let d = dir.to_string_lossy().into_owned();
        assert!(update_saved(&d, |_| {}).is_err());
        assert!(dir.join(HISTORY_FILENAME).is_dir(), "moved aside");
        let aside = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(!aside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // D-23: JUnit reports are a timings source. Failed, errored and skipped
    // cases keep no time; a folder of reports merges (longest wins).
    #[test]
    fn junit_reports_give_shard_timings() {
        let dir = std::env::temp_dir().join(format!("vel-junit-timings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.xml");
        std::fs::write(
            &a,
            r#"<?xml version="1.0"?><testsuites><testsuite name="s">
<testcase name="login" classname="velocity" time="12.5"/>
<testcase name="pay" classname="velocity" time="3"><failure message="x"/></testcase>
<testcase name="skip" classname="velocity" time="0"><skipped/></testcase>
<testcase name="search &amp; filter" classname="velocity" time="1,200.000"/>
</testsuite></testsuites>"#,
        )
        .unwrap();
        let h = load_timings(&a).unwrap();
        assert_eq!(h.durations.get("login"), Some(&12_500));
        assert_eq!(h.durations.get("search & filter"), Some(&1_200_000));
        assert!(!h.durations.contains_key("pay"));
        assert!(!h.durations.contains_key("skip"));
        std::fs::write(
            dir.join("b.xml"),
            r#"<testsuite><testcase name="login" time="20"/><testcase name="pay" time="4"/></testsuite>"#,
        )
        .unwrap();
        let h = load_timings(&dir).unwrap();
        assert_eq!(h.durations.get("login"), Some(&20_000));
        assert_eq!(h.durations.get("pay"), Some(&4_000));
        // A broken report is an error, never an empty timings set.
        std::fs::write(&a, "<testsuite><testcase name=").unwrap();
        assert!(load_timings(&a).is_err());
        std::fs::write(&a, "<testsuite></testsuite>").unwrap();
        assert!(load_timings(&a).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Review d4 m7: `12,5` (a decimal comma) read as 125 s; an entity XML
    // doesn't define, or a non-UTF-8 name, rejected the whole report; a
    // huge report was read into memory whole.
    #[test]
    fn junit_timings_survive_locales_entities_and_size() {
        assert_eq!(junit_seconds_ms("12,5"), Some(12_500));
        assert_eq!(junit_seconds_ms("1,200.000"), Some(1_200_000));
        assert_eq!(junit_seconds_ms("1.234,5"), Some(1_234_500));
        assert_eq!(junit_seconds_ms("1,234,567"), Some(1_234_567_000));
        // review-d7 m10: English grouping, as Java's NumberFormat writes
        // 1200 s; other lone commas stay decimal commas.
        assert_eq!(junit_seconds_ms("1,200"), Some(1_200_000));
        assert_eq!(junit_seconds_ms("12,345"), Some(12_345_000));
        assert_eq!(junit_seconds_ms("0,125"), Some(125));
        assert_eq!(junit_seconds_ms("1,25"), Some(1_250));
        assert_eq!(junit_seconds_ms("1,2345"), Some(1_235));
        assert_eq!(junit_seconds_ms("1234,567"), Some(1_234_567));
        assert_eq!(junit_seconds_ms(" 3 "), Some(3_000));
        assert_eq!(junit_seconds_ms("-1"), None);
        assert_eq!(junit_seconds_ms("NaN"), None);
        let dir = std::env::temp_dir().join(format!("vel-junit-m7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.xml");
        let mut xml =
            br#"<testsuite><testcase name="pay &nbsp; now" time="12,5"/><testcase name="caf"#
                .to_vec();
        xml.extend_from_slice(b"\xe9\" time=\"2\"/></testsuite>");
        std::fs::write(&a, &xml).unwrap();
        let h = load_timings(&a).unwrap();
        assert_eq!(h.durations.get("pay &nbsp; now"), Some(&12_500), "{h:?}");
        assert_eq!(h.durations.len(), 2, "{h:?}");
        // Over the size cap: skipped, not read.
        let big = dir.join("big.xml");
        let mut xml = String::from("<testsuite><testcase name=\"slow\" time=\"9\"><system-out>");
        xml.push_str(&"x".repeat(MAX_JUNIT_BYTES as usize));
        xml.push_str("</system-out></testcase></testsuite>");
        std::fs::write(&big, xml).unwrap();
        let h = load_timings(&dir).unwrap();
        assert!(!h.durations.contains_key("slow"));
        assert_eq!(h.durations.get("pay &nbsp; now"), Some(&12_500));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn failed_with(name: &str, msg: &str) -> TestResult {
        TestResult::failed(name, msg.into(), Duration::from_millis(100))
    }

    fn retried_after(name: &str, msg: &str) -> TestResult {
        TestResult {
            status: TestStatus::Retried,
            failed_attempts: vec![msg.into()],
            ..make_result(name, 100)
        }
    }

    #[test]
    fn failures_are_classified_by_their_error() {
        let kind = FailureKind::of_message;
        let cases = [
            (
                VelocityError::ElementNotFound {
                    selector: "text=\"Pay\"".into(),
                    timeout_ms: 5000,
                    screenshot: None,
                    hierarchy_snapshot: None,
                }
                .to_string(),
                FailureKind::NotFound,
            ),
            (
                VelocityError::NotActionable {
                    selector: "id=buy".into(),
                    reason: "covered".into(),
                }
                .to_string(),
                FailureKind::NotFound,
            ),
            (
                VelocityError::AssertionFailed {
                    expected: "Paid".into(),
                    actual: "Pending".into(),
                    selector: "id=status".into(),
                    screenshot: None,
                }
                .to_string(),
                FailureKind::Assertion,
            ),
            (
                VelocityError::StepTimeout {
                    test_name: "t".into(),
                    step_index: 3,
                    timeout_ms: 30_000,
                }
                .to_string(),
                FailureKind::Timeout,
            ),
            (
                VelocityError::SyncTimeout {
                    timeout_ms: 10_000,
                    stable_count: 1,
                    required: 3,
                }
                .to_string(),
                FailureKind::Timeout,
            ),
            (
                VelocityError::AppCrashed {
                    app_id: "com.shop".into(),
                    detail: "NullPointerException".into(),
                }
                .to_string(),
                FailureKind::Crash,
            ),
            (
                VelocityError::AdbConnectionLost {
                    device_id: "emulator-5554".into(),
                    reason: "closed".into(),
                }
                .to_string(),
                FailureKind::Infrastructure,
            ),
            (
                VelocityError::WdaLost {
                    device_id: String::new(),
                    url: "http://localhost:8100".into(),
                    detail: "reset".into(),
                }
                .to_string(),
                FailureKind::Infrastructure,
            ),
            (
                VelocityError::HttpCheckFailed {
                    request: "POST /login".into(),
                    status: 500,
                    failures: vec!["status is 500, expected 200".into()],
                    body: String::new(),
                }
                .to_string(),
                FailureKind::Http,
            ),
            (
                VelocityError::HttpUnreachable {
                    request: "GET /health".into(),
                    detail: "operation timed out".into(),
                }
                .to_string(),
                FailureKind::Http,
            ),
            ("something odd".to_string(), FailureKind::Other),
        ];
        for (msg, want) in cases {
            assert_eq!(kind(&msg), want, "{msg}");
            // A step error carries where the step is written in front.
            let at = format!("tests/app.yaml:42: tap {{ text: \"Pay\" }}: {msg}");
            assert_eq!(kind(&at), want, "{at}");
        }
        // A flaky run is classified by its first failed attempt; a run
        // marked as the device's failure is infrastructure whatever it says.
        let r = retried_after("t", "Element not found: id=x after 5000ms");
        assert_eq!(FailureKind::of_result(&r), FailureKind::NotFound);
        let mut r = failed_with("t", "Assertion failed: expected \"a\", got \"b\"");
        assert_eq!(FailureKind::of_result(&r), FailureKind::Assertion);
        r.infrastructure_failure = true;
        assert_eq!(FailureKind::of_result(&r), FailureKind::Infrastructure);
    }

    const NOT_FOUND: &str = "Element not found: id=pay after 5000ms";
    const TIMEOUT: &str = "Step timeout: step 2 in 'checkout' exceeded 30000ms";

    #[test]
    fn flakiness_is_split_by_failure_kind() {
        let mut h = TestHistory::default();
        let runs = [
            make_result("checkout", 100),
            failed_with("checkout", TIMEOUT),
            make_result("checkout", 100),
            retried_after("checkout", NOT_FOUND),
            make_result("checkout", 100),
            failed_with("checkout", TIMEOUT),
        ];
        for r in runs {
            update(&mut h, &[r]);
        }
        assert_eq!(
            h.failure_kinds["checkout"],
            vec![
                FailureKind::Timeout,
                FailureKind::NotFound,
                FailureKind::Timeout
            ]
        );
        let by_kind = h.flakiness_by_kind("checkout");
        // 11 = 6 runs + 5 transitions; 3 flips into or out of a timeout,
        // one retried pass after an element went missing.
        assert_eq!(
            by_kind,
            vec![
                (FailureKind::Timeout, 3.0 / 11.0),
                (FailureKind::NotFound, 1.0 / 11.0)
            ]
        );
        let sum: f64 = by_kind.iter().map(|(_, f)| f).sum();
        assert!((sum - h.flakiness("checkout").unwrap()).abs() < 1e-9);
        assert_eq!(
            h.kind_note("checkout").as_deref(),
            Some("timeout 27%, not-found 9%")
        );
        assert!(h.flagged_flaky("checkout"));
        let report = flaky_report(&h, &[make_result("checkout", 100)]);
        assert_eq!(
            report,
            vec!["checkout: flakiness 36% over 6 runs (timeout 27%, not-found 9%)".to_string()]
        );
        let mut results = vec![make_result("checkout", 100)];
        note_flakiness(&h, &mut results);
        note_flakiness(&h, &mut results);
        assert_eq!(
            results[0].warnings,
            vec!["history: flakiness 36% over 6 runs (timeout 27%, not-found 9%)".to_string()]
        );
    }

    #[test]
    fn kinds_leave_the_window_with_their_runs() {
        let mut h = TestHistory::default();
        update(&mut h, &[failed_with("t", TIMEOUT)]);
        update(&mut h, &[failed_with("t", NOT_FOUND)]);
        for _ in 0..OUTCOME_WINDOW - 1 {
            update(&mut h, &[make_result("t", 100)]);
        }
        assert_eq!(h.failure_kinds["t"], vec![FailureKind::NotFound]);
        update(&mut h, &[make_result("t", 100)]);
        assert!(!h.failure_kinds.contains_key("t"), "{:?}", h.failure_kinds);
        update(&mut h, &[failed_with("t", TIMEOUT)]);
        let mut suite = std::collections::HashSet::new();
        suite.insert("other".to_string());
        forget_stale_at(&mut h, &suite, now_secs() + FORGET_AFTER_SECS + 10);
        assert!(h.failure_kinds.is_empty());
    }

    #[test]
    fn failure_kinds_survive_a_save_and_load() {
        let dir = std::env::temp_dir().join(format!("velocity-hist-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_string_lossy().into_owned();
        update_saved(&d, |h| {
            update(
                h,
                &[
                    failed_with("pay", TIMEOUT),
                    retried_after("cart", NOT_FOUND),
                ],
            )
        })
        .unwrap();
        let text = std::fs::read_to_string(dir.join(HISTORY_FILENAME)).unwrap();
        assert!(text.contains("\"failureKinds\""), "{text}");
        assert!(text.contains("\"not-found\""), "{text}");
        let h = load(&d).unwrap();
        assert_eq!(h.failure_kinds["pay"], vec![FailureKind::Timeout]);
        assert_eq!(h.failure_kinds["cart"], vec![FailureKind::NotFound]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A version-2 file has no kinds: its failures are of unknown kind, and
    // the runs recorded after it are classified and line up with theirs.
    #[test]
    fn a_version_two_file_loads_with_unknown_kinds() {
        let dir = std::env::temp_dir().join(format!("velocity-hist-v2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(HISTORY_FILENAME),
            r#"{"version":2,"durations":{"a":10},"outcomes":{"a":["pass","fail","pass","flaky"]}}"#,
        )
        .unwrap();
        let d = dir.to_string_lossy().into_owned();
        let mut h = load(&d).unwrap();
        assert_eq!(h.version, HISTORY_VERSION);
        assert!(h.failure_kinds.is_empty());
        assert_eq!(
            h.flakiness_by_kind("a"),
            vec![(FailureKind::Unknown, 3.0 / 7.0)]
        );
        assert_eq!(h.kind_note("a"), None, "nothing known to add");
        update(&mut h, &[failed_with("a", TIMEOUT)]);
        assert_eq!(
            h.flakiness_by_kind("a"),
            vec![
                (FailureKind::Unknown, 3.0 / 9.0),
                (FailureKind::Timeout, 1.0 / 9.0)
            ]
        );
        assert_eq!(
            h.kind_note("a").as_deref(),
            Some("unknown 33%, timeout 11%")
        );
        // A kind a newer Velocity wrote reads as unknown, not as a
        // corrupt file.
        let newer: TestHistory = serde_json::from_str(
            r#"{"version":9,"durations":{},"outcomes":{"a":["fail"]},"failureKinds":{"a":["gremlin"]}}"#,
        )
        .unwrap();
        assert_eq!(newer.failure_kinds["a"], vec![FailureKind::Unknown]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_durations_do_not_pull_the_median_down() {
        let mut h = TestHistory::default();
        for (n, ms) in [("a", 0), ("b", 0), ("c", 0), ("d", 9000), ("e", 11000)] {
            h.durations.insert(n.into(), ms);
        }
        assert_eq!(h.median_ms(), 11000);
    }
}
