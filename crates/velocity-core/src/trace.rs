//! Per-test traces: what each step did and saw.
//!
//! The executor already reads trees, bounds and settle timings on every
//! step and used to throw them away. A [`TraceRecorder`] keeps them in
//! memory while a test runs and writes them only when the test fails (or
//! always, with `--trace always`), Playwright's `retain-on-failure`:
//!
//! ```text
//! <artifacts>/traces/<test>/steps.json        one attempt's steps
//! <artifacts>/traces/<test>/trees/<hash>.json each distinct screen, once
//! ```
//!
//! Trees are stored once per content hash, so a screen that didn't change
//! across ten steps costs one file. Recording costs no device calls: trees
//! are taken from the driver's hierarchy cache right after a lookup used it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use velocity_common::{
    Element, Platform, Rect, Selector, SourcePos, StepResult, StepStatus, TestStatus, TraceMode,
};

/// Bumped when the files' shape changes incompatibly.
pub const TRACE_VERSION: u32 = 1;

/// One attempt of one test, as written to `steps*.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestTrace {
    pub version: u32,
    pub test: String,
    pub device: String,
    #[serde(default)]
    pub platform: Option<Platform>,
    pub attempt: u32,
    pub status: TestStatus,
    pub steps: Vec<StepTrace>,
    /// The video and logs kept with this attempt (paths relative to the
    /// trace folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<TraceEvidence>,
}

/// Evidence files next to a trace, for the viewer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceEvidence {
    /// `video.mp4`, relative to the trace folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// Test time (ms) of the video's first frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_offset_ms: Option<i64>,
    /// `screencast.json` (web), relative to the trace folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screencast: Option<String>,
    /// Log files, relative to the trace folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// `logs*.json`: the timed log lines the viewer shows per step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_lines: Option<String>,
    /// Crashes and ANRs the logs showed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What one step did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepTrace {
    pub index: usize,
    /// `tap { text: "Pay" }`.
    pub label: String,
    /// When the step started, in milliseconds since the test started (the
    /// clock the video and log times use).
    #[serde(default)]
    pub start_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourcePos>,
    #[serde(default)]
    pub status: Option<StepStatus>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub settle_ms: u64,
    /// Tree reads the settle took.
    #[serde(default)]
    pub settle_reads: u32,
    /// The element the selector matched (the last match, for a step that
    /// looked more than once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<TracedElement>,
    /// For a failed step with a selector: the closest elements on its last
    /// tree, by the healing score, best first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nearest: Vec<TracedElement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Hashes of the trees this step read, in order (`trees/<hash>.json`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trees: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<PathBuf>,
}

/// An element, flattened for the trace (no children).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracedElement {
    /// `text="Pay"`, the form a selector would use.
    pub describe: String,
    pub element_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub bounds: Rect,
    /// Similarity to the step's selector (0–1), for `nearest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

impl TracedElement {
    pub fn from_element(el: &Element, score: Option<f64>) -> Self {
        Self {
            describe: crate::healing::describe_element(el),
            element_type: el.element_type.clone(),
            id: el.platform_id.clone(),
            text: el.text.clone(),
            label: el.label.clone(),
            bounds: el.bounds,
            score,
        }
    }
}

/// How many near matches a failed step keeps.
pub const NEAREST: usize = 5;

/// Collects one test's trace in memory.
pub struct TraceRecorder {
    mode: TraceMode,
    steps: Vec<StepTrace>,
    trees: HashMap<String, Element>,
    /// Extra name part for concurrent copies of one test (hunt rounds,
    /// preventive copies), so they don't overwrite each other.
    tag: Option<String>,
    /// Start of the run this recorder belongs to: a pass writes it into
    /// [`PASSED_MARKER`], and only traces written before it are stale.
    /// Per run, not per process: the daemon serves many runs (review
    /// f5-f6 MAJOR-1).
    run_started: std::time::SystemTime,
    /// When the running test started.
    test_started: std::time::Instant,
    /// Evidence for the running test, set before [`Self::finish`].
    evidence: Option<(TraceEvidence, Vec<velocity_common::evidence::LogLine>)>,
    /// The `trace*.html` the last [`Self::finish`] wrote.
    last_html: Option<PathBuf>,
}

impl TraceRecorder {
    /// A recorder for a run starting now.
    pub fn new(mode: TraceMode) -> Self {
        Self {
            mode,
            steps: Vec::new(),
            trees: HashMap::new(),
            tag: None,
            run_started: std::time::SystemTime::now(),
            test_started: std::time::Instant::now(),
            evidence: None,
            last_html: None,
        }
    }

    /// Set the start of the run (the runner's, shared by every executor
    /// and retry of that run).
    pub fn with_run_started(mut self, at: std::time::SystemTime) -> Self {
        self.set_run_started(at);
        self
    }

    pub fn set_run_started(&mut self, at: std::time::SystemTime) {
        self.run_started = at;
    }

    pub fn run_started(&self) -> std::time::SystemTime {
        self.run_started
    }

    pub fn mode(&self) -> TraceMode {
        self.mode
    }

    pub fn enabled(&self) -> bool {
        self.mode != TraceMode::Off
    }

    pub fn set_tag(&mut self, tag: Option<String>) {
        self.tag = tag;
    }

    pub fn begin_test(&mut self) {
        self.steps.clear();
        self.trees.clear();
        self.evidence = None;
        self.last_html = None;
        self.test_started = std::time::Instant::now();
    }

    /// When the running test started ([`Self::begin_test`]).
    pub fn test_started(&self) -> std::time::Instant {
        self.test_started
    }

    /// What every file of this test's attempt adds to its name: the
    /// copy's tag and `-retryN` (`steps-retry1.json`, `video-retry1.mp4`).
    pub fn file_suffix(&self, attempt: u32) -> String {
        let mut s = String::new();
        if let Some(tag) = &self.tag {
            s.push('-');
            s.push_str(&sanitize(tag));
        }
        if attempt > 0 {
            s.push_str(&format!("-retry{attempt}"));
        }
        s
    }

    /// The video and logs kept for the running test, for its trace and
    /// `trace.html`.
    pub fn set_evidence(
        &mut self,
        evidence: TraceEvidence,
        lines: Vec<velocity_common::evidence::LogLine>,
    ) {
        self.evidence = Some((evidence, lines));
    }

    /// Set a step's screenshot after the fact (step screenshots are
    /// written only once the test has failed).
    pub fn set_screenshot(&mut self, index: usize, path: PathBuf) {
        if let Some(step) = self.steps.iter_mut().rev().find(|s| s.index == index) {
            if step.screenshot.is_none() {
                step.screenshot = Some(path);
            }
        }
    }

    /// The `trace*.html` the last [`Self::finish`] wrote.
    pub fn last_html(&self) -> Option<&Path> {
        self.last_html.as_deref()
    }

    /// Write `trace.html` for the steps so far without ending the test
    /// (`--pause-on-failure`). Returns its path.
    pub fn snapshot_html(
        &self,
        root: &Path,
        test: &str,
        device: &str,
        platform: Option<Platform>,
        attempt: u32,
    ) -> Option<PathBuf> {
        let trace = TestTrace {
            version: TRACE_VERSION,
            test: test.to_string(),
            device: device.to_string(),
            platform,
            attempt,
            status: TestStatus::Failed,
            steps: self.steps.clone(),
            evidence: self.evidence.as_ref().map(|(e, _)| e.clone()),
        };
        let lines = self
            .evidence
            .as_ref()
            .map(|(_, l)| l.as_slice())
            .unwrap_or(&[]);
        let dir = trace_dir(root, test);
        let path = dir.join(format!("trace{}.html", self.file_suffix(attempt)));
        let html = crate::trace_html::render(&trace, &self.trees, lines, &dir);
        std::fs::create_dir_all(&dir)
            .and_then(|_| write_atomic(&path, html.as_bytes()))
            .ok()?;
        Some(path)
    }

    pub fn begin_step(&mut self, index: usize, label: String, selector: Option<Selector>) {
        if !self.enabled() {
            return;
        }
        let start_ms = self.test_started.elapsed().as_millis() as i64;
        self.steps.push(StepTrace {
            index,
            label,
            selector,
            start_ms,
            ..Default::default()
        });
    }

    fn current(&mut self) -> Option<&mut StepTrace> {
        if !self.enabled() {
            return None;
        }
        self.steps.last_mut()
    }

    /// Keep a tree the step read; stored once per content.
    pub fn record_tree(&mut self, tree: &Element) {
        if !self.enabled() {
            return;
        }
        let hash = tree_hash(tree);
        self.trees
            .entry(hash.clone())
            .or_insert_with(|| tree.clone());
        if let Some(step) = self.current() {
            if step.trees.last() != Some(&hash) {
                step.trees.push(hash);
            }
        }
    }

    pub fn record_match(&mut self, el: &Element) {
        if let Some(step) = self.current() {
            step.matched = Some(TracedElement::from_element(el, None));
        }
    }

    pub fn record_settle_reads(&mut self, reads: u32) {
        if let Some(step) = self.current() {
            step.settle_reads += reads;
        }
    }

    /// Close the running step with its result, and the heals and warnings
    /// it added.
    pub fn end_step(&mut self, result: &StepResult, heals: Vec<String>, warnings: Vec<String>) {
        let Some(step) = self.current() else {
            return;
        };
        step.status = Some(result.status);
        step.duration_ms = result.duration.as_millis() as u64;
        step.settle_ms = result.settle_ms.unwrap_or(0);
        step.source = result.source.clone();
        step.error = result.error_message.clone();
        step.screenshot = result.screenshot.clone();
        step.heals = heals;
        step.warnings = warnings;
    }

    /// The last tree the running step (or an earlier one) read.
    pub fn last_tree(&self) -> Option<&Element> {
        self.steps
            .iter()
            .rev()
            .find_map(|s| s.trees.last())
            .and_then(|h| self.trees.get(h))
    }

    /// For a failed step with a selector and no match: the nearest
    /// elements on the last tree it read.
    pub fn fill_nearest(&mut self, platform: Option<Platform>) {
        let Some(step) = self.steps.last() else {
            return;
        };
        let (Some(sel), true) = (
            step.selector.clone(),
            step.status == Some(StepStatus::Failed),
        ) else {
            return;
        };
        let Some(tree) = self.last_tree() else {
            return;
        };
        let nearest: Vec<TracedElement> =
            crate::healing::closest_elements(&sel, tree, platform, NEAREST)
                .iter()
                .map(|(el, score)| TracedElement::from_element(el, Some(*score)))
                .collect();
        if let Some(step) = self.steps.last_mut() {
            step.nearest = nearest;
        }
    }

    /// Whether a test that ended with `status` keeps its trace.
    pub fn keeps(&self, status: TestStatus) -> bool {
        match self.mode {
            TraceMode::Off => false,
            TraceMode::OnFailure => status == TestStatus::Failed,
            TraceMode::Always => true,
        }
    }

    /// Write the trace under `<root>/traces/<test>/` if the mode keeps it.
    /// Returns the steps file. Best-effort: a trace that can't be written
    /// must not fail the test.
    pub fn finish(
        &mut self,
        root: &Path,
        test: &str,
        device: &str,
        platform: Option<Platform>,
        attempt: u32,
        status: TestStatus,
    ) -> Option<PathBuf> {
        let started = self.run_started;
        self.last_html = None;
        let evidence = self.evidence.take();
        if !self.keeps(status) {
            // A pass under `onFailure` keeps no trace, so an older failure's
            // was still there and `describe_failure` reported it as the
            // current one (VEL-205). Mark the old traces as superseded.
            if self.enabled() && status == TestStatus::Passed {
                let dir = trace_dir(root, test);
                if dir.is_dir() {
                    // When this run started: failures written since are
                    // this run's own (an earlier copy or retry) and stay
                    // current (review-s9 M3).
                    let ms = started
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let _ = std::fs::write(dir.join(PASSED_MARKER), ms.to_string());
                }
            }
            self.steps.clear();
            return None;
        }
        let trace = TestTrace {
            version: TRACE_VERSION,
            test: test.to_string(),
            device: device.to_string(),
            platform,
            attempt,
            status,
            steps: std::mem::take(&mut self.steps),
            evidence: None,
        };
        let mut trace = trace;
        let dir = trace_dir(root, test);
        let suffix = self.file_suffix(attempt);
        let name = format!("steps{suffix}.json");
        let _ = std::fs::remove_file(dir.join(PASSED_MARKER));
        let lines = match evidence {
            Some((mut ev, lines)) => {
                if !lines.is_empty() {
                    let file = format!("logs{suffix}.json");
                    let _ = std::fs::create_dir_all(&dir);
                    if redacted_json(&lines, false)
                        .and_then(|b| write_atomic(&dir.join(&file), &b))
                        .is_ok()
                    {
                        ev.log_lines = Some(file);
                    }
                }
                trace.evidence = Some(ev);
                lines
            }
            None => Vec::new(),
        };
        match write_trace(&dir, &name, &trace, &self.trees) {
            Ok(path) => {
                // The viewer, next to the steps: one self-contained file.
                let html = crate::trace_html::render(&trace, &self.trees, &lines, &dir);
                let html_path = dir.join(format!("trace{suffix}.html"));
                match write_atomic(&html_path, html.as_bytes()) {
                    Ok(()) => self.last_html = Some(html_path),
                    Err(e) => tracing::warn!("trace.html for '{test}' not written: {e}"),
                }
                Some(path)
            }
            Err(e) => {
                tracing::warn!("trace for '{test}' not written: {e}");
                None
            }
        }
    }
}

/// Written in a test's trace folder when the test passed after the traces
/// there were written.
pub const PASSED_MARKER: &str = "passed";

/// Whether a later run passed the test, so the failure `steps_file`
/// records is no longer the test's current state (VEL-205). A failure
/// from the passing run itself (one copy of a flake hunt, a retry, another
/// device) is never stale: the marker holds that run's start, and only
/// traces written before it are superseded.
pub fn is_stale(steps_file: &Path) -> bool {
    let Some(dir) = steps_file.parent() else {
        return false;
    };
    let marker = dir.join(PASSED_MARKER);
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let (Some(passed), Some(steps)) = (modified(&marker), modified(steps_file)) else {
        return false;
    };
    if passed < steps {
        return false;
    }
    match std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(ms) => steps < std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms),
        // A marker from before run starts were recorded.
        None => true,
    }
}

/// `<root>/traces/<test>`.
pub fn trace_dir(root: &Path, test: &str) -> PathBuf {
    root.join("traces").join(folder_name(test))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A folder name for test `s`: kept as is only when it is short and made
/// of lower-case ASCII letters, digits, `-` and `_`; any other name (a
/// capital, a space, a colon, a non-ASCII letter) is the cleaned name cut
/// to [`MAX_NAME`] bytes plus a hash of the original. So `a:b` and `a/b`,
/// `a b` and `a_b` (review-s8 m-h), `Login` and `login` (one folder on a
/// case-insensitive file system), or two long names sharing a prefix never
/// share a folder, and a long name never fails to write (VEL-206). The
/// name stays a pure function of the test name, so `trace_dir` (and MCP's
/// `describe_failure`) find a trace with no registry.
fn folder_name(s: &str) -> String {
    let clean = sanitize(s);
    let plain = !s.is_empty()
        && s.len() <= MAX_NAME
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if plain {
        return clean;
    }
    let mut cut = clean.len().min(MAX_NAME);
    while !clean.is_char_boundary(cut) {
        cut -= 1;
    }
    // FNV-1a: stable across runs and Rust versions.
    let hash = s.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    format!("{}-{:08x}", &clean[..cut], hash as u32)
}

/// Longest cleaned test name kept in a trace folder name.
const MAX_NAME: usize = 100;

fn write_trace(
    dir: &Path,
    name: &str,
    trace: &TestTrace,
    trees: &HashMap<String, Element>,
) -> std::io::Result<PathBuf> {
    let tree_dir = dir.join("trees");
    std::fs::create_dir_all(&tree_dir)?;
    let used: std::collections::HashSet<&String> =
        trace.steps.iter().flat_map(|s| &s.trees).collect();
    for hash in used {
        let Some(tree) = trees.get(hash) else {
            continue;
        };
        let path = tree_dir.join(format!("{hash}.json"));
        // Same hash, same content: an existing file (another attempt or a
        // concurrent copy) is already right.
        if !path.exists() {
            write_atomic(&path, &redacted_json(tree, false)?)?;
        }
    }
    let path = dir.join(name);
    // Traces quote what steps typed: secrets are masked like in reports.
    write_atomic(&path, &redacted_json(trace, true)?)?;
    Ok(path)
}

/// `value` as JSON with secrets masked (`velocity_common::redact`).
fn redacted_json<T: serde::Serialize>(value: &T, pretty: bool) -> std::io::Result<Vec<u8>> {
    let mut v = serde_json::to_value(value)?;
    velocity_common::redact::redact_value(&mut v);
    Ok(if pretty {
        serde_json::to_vec_pretty(&v)?
    } else {
        serde_json::to_vec(&v)?
    })
}

use velocity_common::fsutil::write_atomic;

/// Read a trace back, with the trees it refers to.
pub fn load_trace(steps_file: &Path) -> std::io::Result<(TestTrace, HashMap<String, Element>)> {
    let trace: TestTrace = serde_json::from_slice(&std::fs::read(steps_file)?)?;
    let tree_dir = steps_file.parent().unwrap_or(Path::new(".")).join("trees");
    let mut trees = HashMap::new();
    for hash in trace.steps.iter().flat_map(|s| &s.trees) {
        if trees.contains_key(hash) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(tree_dir.join(format!("{hash}.json"))) {
            if let Ok(tree) = serde_json::from_slice::<Element>(&bytes) {
                trees.insert(hash.clone(), tree);
            }
        }
    }
    Ok((trace, trees))
}

/// What an agent needs to fix a failed test: the step, its selector and
/// line, the error, and what was on screen instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureDescription {
    pub test: String,
    pub device: String,
    pub attempt: u32,
    pub step_index: usize,
    pub step: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourcePos>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<TracedElement>,
    /// Closest elements on the last tree the step read, best first.
    pub closest: Vec<TracedElement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heals: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<PathBuf>,
    /// Number of elements on that tree; 0 when no tree was recorded.
    pub tree_elements: usize,
}

/// Describe the failed step of a trace. `None` if no step failed.
pub fn describe_failure(
    trace: &TestTrace,
    trees: &HashMap<String, Element>,
    limit: usize,
) -> Option<FailureDescription> {
    let pos = trace
        .steps
        .iter()
        .rposition(|s| s.status == Some(StepStatus::Failed))?;
    let step = &trace.steps[pos];
    // The step's own last tree, else the closest earlier one.
    let tree = trace.steps[..=pos]
        .iter()
        .rev()
        .find_map(|s| s.trees.last())
        .and_then(|h| trees.get(h));
    let closest = match (&step.selector, tree) {
        (Some(sel), Some(tree)) => {
            crate::healing::closest_elements(sel, tree, trace.platform, limit)
                .iter()
                .map(|(el, score)| TracedElement::from_element(el, Some(*score)))
                .collect()
        }
        _ => step.nearest.iter().take(limit).cloned().collect(),
    };
    Some(FailureDescription {
        test: trace.test.clone(),
        device: trace.device.clone(),
        attempt: trace.attempt,
        step_index: step.index,
        step: step.label.clone(),
        selector: step.selector.as_ref().map(|s| s.to_string()),
        source: step.source.clone(),
        error: step.error.clone(),
        matched: step.matched.clone(),
        closest,
        heals: step.heals.clone(),
        warnings: step.warnings.clone(),
        screenshot: step.screenshot.clone(),
        tree_elements: tree.map(count_elements).unwrap_or(0),
    })
}

fn count_elements(el: &Element) -> usize {
    1 + el.children.iter().map(count_elements).sum::<usize>()
}

/// FNV-1a over the tree's content: stable across runs and platforms, which
/// `DefaultHasher` doesn't promise.
pub fn tree_hash(tree: &Element) -> String {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    hash_element(tree, &mut h);
    format!("{:016x}", h.0)
}

struct Fnv(u64);

impl Fnv {
    fn bytes(&mut self, b: &[u8]) {
        for byte in b {
            self.0 ^= *byte as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        // Field separator, so ("ab","c") and ("a","bc") differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
    }
}

fn hash_element(el: &Element, h: &mut Fnv) {
    h.bytes(el.platform_id.as_bytes());
    h.bytes(el.label.as_deref().unwrap_or("\u{0}").as_bytes());
    h.bytes(el.text.as_deref().unwrap_or("\u{0}").as_bytes());
    h.bytes(el.element_type.as_bytes());
    for v in [el.bounds.x, el.bounds.y, el.bounds.width, el.bounds.height] {
        h.bytes(&v.to_le_bytes());
    }
    h.bytes(&[el.enabled as u8, el.visible as u8]);
    h.bytes(&(el.children.len() as u64).to_le_bytes());
    for c in &el.children {
        hash_element(c, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn el(id: &str, text: &str, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: Some(text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn screen(children: Vec<Element>) -> Element {
        Element {
            children,
            text: None,
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            ..el("", "", 0)
        }
    }

    fn step_result(index: usize, status: StepStatus, error: Option<&str>) -> StepResult {
        StepResult {
            step_index: index,
            action_name: "tap".into(),
            status,
            duration: Duration::from_millis(12),
            screenshot: None,
            error_message: error.map(String::from),
            resource_delta: None,
            settle_ms: Some(5),
            source: Some(SourcePos::new("app.yaml", 10 + index, 9)),
        }
    }

    #[test]
    fn equal_trees_hash_equal_and_any_change_shows() {
        let a = screen(vec![el("pay", "Pay", 100)]);
        let b = screen(vec![el("pay", "Pay", 100)]);
        let c = screen(vec![el("pay", "Pay now", 100)]);
        assert_eq!(tree_hash(&a), tree_hash(&b));
        assert_ne!(tree_hash(&a), tree_hash(&c));
        assert_eq!(tree_hash(&a).len(), 16);
    }

    #[test]
    fn a_failed_test_writes_steps_and_each_tree_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.begin_test();
        let home = screen(vec![el("cart", "Cart", 100), el("pay", "Pay now", 300)]);
        rec.begin_step(0, "tap { text: \"Cart\" }".into(), None);
        rec.record_tree(&home);
        rec.record_match(&home.children[0]);
        rec.record_settle_reads(3);
        rec.end_step(&step_result(0, StepStatus::Passed, None), vec![], vec![]);
        rec.begin_step(
            1,
            "tap { text: \"Pay\" }".into(),
            Some(Selector::Text("Pay".into())),
        );
        rec.record_tree(&home);
        rec.end_step(
            &step_result(1, StepStatus::Failed, Some("Element not found")),
            vec![],
            vec!["slow".into()],
        );
        rec.fill_nearest(None);
        let path = rec
            .finish(
                dir.path(),
                "check out",
                "emulator-5554",
                None,
                0,
                TestStatus::Failed,
            )
            .unwrap();
        let folder = trace_dir(dir.path(), "check out");
        assert_eq!(path, folder.join("steps.json"));
        let trees: Vec<_> = std::fs::read_dir(folder.join("trees")).unwrap().collect();
        assert_eq!(trees.len(), 1, "the unchanged screen is stored once");

        let (trace, trees) = load_trace(&path).unwrap();
        assert_eq!(trace.steps.len(), 2);
        assert_eq!(trace.steps[0].settle_reads, 3);
        assert_eq!(trace.steps[0].matched.as_ref().unwrap().bounds.y, 100);
        assert_eq!(trace.steps[1].source.as_ref().unwrap().line, 11);
        assert_eq!(
            trace.steps[1].nearest[0].text.as_deref(),
            Some("Pay now"),
            "{:?}",
            trace.steps[1].nearest
        );

        let d = describe_failure(&trace, &trees, 3).unwrap();
        assert_eq!(d.step_index, 1);
        assert_eq!(d.selector.as_deref(), Some("text=\"Pay\""));
        assert_eq!(d.source.unwrap().to_string(), "app.yaml:11");
        assert_eq!(d.closest[0].text.as_deref(), Some("Pay now"));
        assert_eq!(d.tree_elements, 3);
        assert_eq!(d.warnings, vec!["slow".to_string()]);
    }

    #[test]
    fn passing_tests_keep_no_trace_unless_always() {
        let dir = tempfile::tempdir().unwrap();
        for (mode, kept) in [
            (TraceMode::OnFailure, false),
            (TraceMode::Always, true),
            (TraceMode::Off, false),
        ] {
            let mut rec = TraceRecorder::new(mode);
            rec.begin_test();
            rec.begin_step(0, "launchApp".into(), None);
            rec.end_step(&step_result(0, StepStatus::Passed, None), vec![], vec![]);
            let out = rec.finish(dir.path(), "t", "d", None, 0, TestStatus::Passed);
            assert_eq!(out.is_some(), kept, "{mode:?}");
        }
    }

    /// VEL-205: a pass after a failure marks the failure's trace stale.
    #[test]
    fn a_later_pass_makes_the_old_failure_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.begin_test();
        let failed = rec
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Failed)
            .unwrap();
        assert!(!is_stale(&failed));
        // That failure is from an earlier run.
        std::fs::File::options()
            .write(true)
            .open(&failed)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        rec.begin_test();
        assert!(rec
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Passed)
            .is_none());
        assert!(is_stale(&failed), "the pass supersedes the failure");
        // Failing again makes the trace current again.
        rec.begin_test();
        let again = rec
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Failed)
            .unwrap();
        assert!(!is_stale(&again));
    }

    #[test]
    fn a_pass_in_the_same_run_never_hides_that_run_s_failure() {
        // Review-s9 M3: in a flake hunt copy 1 failed and copy 2 passed;
        // describe_failure skipped the real failure.
        let dir = tempfile::tempdir().unwrap();
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.set_tag(Some("copy 1".into()));
        rec.begin_test();
        let failed = rec
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Failed)
            .unwrap();
        rec.set_tag(Some("copy 2".into()));
        rec.begin_test();
        assert!(rec
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Passed)
            .is_none());
        assert!(!is_stale(&failed), "this run's own failure was hidden");
    }

    /// Review f5-f6 MAJOR-1: one process (the daemon) serving two runs.
    /// No backdating: run 2 starts after run 1's failure was written.
    #[test]
    fn a_pass_in_a_later_run_of_the_same_process_makes_the_failure_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut run1 = TraceRecorder::new(TraceMode::OnFailure);
        run1.begin_test();
        let failed = run1
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Failed)
            .unwrap();
        // The marker holds milliseconds.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut run2 = TraceRecorder::new(TraceMode::OnFailure);
        run2.begin_test();
        assert!(run2
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Passed)
            .is_none());
        assert!(is_stale(&failed), "run 2's pass must supersede run 1");
    }

    /// Two executors of one run (parallel devices) share the run start:
    /// copy 1 failing on one and copy 2 passing on the other keeps the
    /// failure current.
    #[test]
    fn executors_of_one_run_share_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = TraceRecorder::new(TraceMode::OnFailure);
        let started = a.run_started();
        a.set_tag(Some("copy 1".into()));
        a.begin_test();
        let failed = a
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Failed)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = TraceRecorder::new(TraceMode::OnFailure).with_run_started(started);
        b.set_tag(Some("copy 2".into()));
        b.begin_test();
        assert!(b
            .finish(dir.path(), "t", "d", None, 0, TestStatus::Passed)
            .is_none());
        assert!(!is_stale(&failed), "this run's own failure was hidden");
    }

    #[test]
    fn copies_and_retries_write_separate_step_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.set_tag(Some("round 2@emulator-5556".into()));
        rec.begin_test();
        let p = rec
            .finish(dir.path(), "t", "d", None, 1, TestStatus::Failed)
            .unwrap();
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "steps-round_2_emulator-5556-retry1.json"
        );
    }

    #[test]
    fn trace_folders_are_unique_and_bounded() {
        // VEL-206.
        let root = Path::new("/r");
        assert_eq!(
            trace_dir(root, "login-ok"),
            root.join("traces").join("login-ok")
        );
        assert_ne!(trace_dir(root, "a:b"), trace_dir(root, "a/b"));
        assert_eq!(
            trace_dir(root, "check_out"),
            root.join("traces").join("check_out")
        );
        assert_ne!(trace_dir(root, "a b"), trace_dir(root, "a:b"));
        let long_a = format!("{}A", "x".repeat(300));
        let long_b = format!("{}B", "x".repeat(300));
        assert_ne!(trace_dir(root, &long_a), trace_dir(root, &long_b));
        let name = trace_dir(root, &long_a);
        assert!(name.file_name().unwrap().len() < 120, "{name:?}");
        let _ = folder_name(&"é".repeat(200));
    }

    #[test]
    fn mixed_case_and_unsafe_trace_folders_are_hashed() {
        // trace-case: "a b" and "a_b" shared `a_b` (review-s8 m-h), and
        // "Login" / "login" share one folder on macOS and Windows.
        let root = Path::new("/r");
        assert_ne!(trace_dir(root, "a b"), trace_dir(root, "a_b"));
        assert_ne!(trace_dir(root, "check out"), trace_dir(root, "check_out"));
        let lower = |t: &str| trace_dir(root, t).to_string_lossy().to_lowercase();
        assert_ne!(lower("Login"), lower("login"));
        assert_ne!(lower("LOGIN"), lower("Login"));
        assert_eq!(trace_dir(root, "login"), root.join("traces").join("login"));
        let hashed = trace_dir(root, "Login");
        let f = hashed.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            f.starts_with("Login-") && f.len() == "Login-".len() + 8,
            "{f}"
        );
        // Non-ASCII letters are hashed too (NFC vs NFD file systems).
        assert_ne!(trace_dir(root, "café"), root.join("traces").join("café"));
        // A pure function of the name: the same test finds the same folder.
        assert_eq!(trace_dir(root, "Check Out"), trace_dir(root, "Check Out"));
    }
}
