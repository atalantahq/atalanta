//! Failure evidence: a video of the test and the device's logs, captured
//! while a test runs and kept when it fails (or always, when configured).
//!
//! The executor asks the driver for an [`EvidenceSession`] at the start of
//! each test ([`crate::PlatformDriver::start_evidence`]) and finishes it at
//! the end with what to keep. Each platform captures in its own way:
//!
//! - Android: an H.264 stream from the on-device video server into an
//!   in-memory ring buffer (written as MP4 only when kept), and `logcat`.
//! - iOS simulator: `simctl io recordVideo` and `log stream`.
//! - Web: the CDP screencast (JPEG frames) and console, page errors and
//!   failed requests.
//!
//! Times are milliseconds since the test's start ([`EvidenceRequest::started`]),
//! the same clock the trace's step times use, so a viewer can seek the video
//! to a step and show the log lines around it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// When a kind of evidence (video, logs) is captured and kept:
/// `artifacts.video` / `artifacts.logs`, `--video` / `--logs`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMode {
    /// Not captured.
    Off,
    /// Captured, written only for a failed test (the default).
    #[default]
    #[serde(alias = "onFailure", alias = "on_failure", alias = "retain-on-failure")]
    OnFailure,
    /// Captured and written for every test.
    #[serde(alias = "on")]
    Always,
}

impl CaptureMode {
    pub fn on(self) -> bool {
        self != Self::Off
    }

    /// Whether a test that `failed` (or not) keeps this evidence.
    pub fn keeps(self, failed: bool) -> bool {
        match self {
            Self::Off => false,
            Self::OnFailure => failed,
            Self::Always => true,
        }
    }
}

impl std::str::FromStr for CaptureMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "off" | "never" => Ok(Self::Off),
            "on-failure" | "onFailure" | "on_failure" | "retain-on-failure" => Ok(Self::OnFailure),
            "always" | "on" => Ok(Self::Always),
            other => Err(format!(
                "unknown mode '{other}'; use off, on-failure or always"
            )),
        }
    }
}

impl std::fmt::Display for CaptureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
        })
    }
}

/// What a test wants captured.
#[derive(Debug, Clone)]
pub struct EvidenceRequest {
    pub test: String,
    /// The app under test (bundle id, package, or URL on the web).
    pub app_id: String,
    pub video: CaptureMode,
    /// Android ring buffer length: the last N seconds are kept.
    pub video_seconds: u32,
    pub logs: CaptureMode,
    /// The test's start: every time in the evidence is relative to it.
    pub started: Instant,
}

impl EvidenceRequest {
    /// Milliseconds since the test started.
    pub fn now_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }
}

/// What a kept evidence file is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AttachmentKind {
    /// `video.mp4`.
    Video,
    /// A web screencast: `screencast.json` and its JPEG frames.
    Screencast,
    /// The app's own log lines (logcat for its pids, `log stream`, the
    /// browser console).
    AppLog,
    /// The whole device log.
    DeviceLog,
    /// A crash report.
    Crash,
    /// The test's `trace.html`.
    Trace,
}

/// A file kept for a test, linked from the reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub path: PathBuf,
}

impl Attachment {
    pub fn new(kind: AttachmentKind, path: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            path: path.into(),
        }
    }

    /// The MIME type reports declare for it.
    pub fn mime(&self) -> &'static str {
        match self.kind {
            AttachmentKind::Video => "video/mp4",
            AttachmentKind::Screencast => "application/json",
            AttachmentKind::Trace => "text/html",
            _ => "text/plain",
        }
    }

    /// A short title: "video", "app log", …
    pub fn title(&self) -> &'static str {
        match self.kind {
            AttachmentKind::Video => "video",
            AttachmentKind::Screencast => "screencast",
            AttachmentKind::AppLog => "app log",
            AttachmentKind::DeviceLog => "device log",
            AttachmentKind::Crash => "crash report",
            AttachmentKind::Trace => "trace",
        }
    }
}

/// One log line with its time since the test's start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogLine {
    pub t: i64,
    /// `V D I W E F`, or ' ' when unknown.
    #[serde(default)]
    pub level: char,
    pub text: String,
}

/// What a finished (or checkpointed) session left.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub attachments: Vec<Attachment>,
    /// Test time of the video's first frame: the video's 0 s is this many
    /// milliseconds after the test started (negative when the ring buffer
    /// reaches back before it).
    pub video_offset_ms: Option<i64>,
    /// The app's log lines (redacted), for the trace viewer.
    pub log_lines: Vec<LogLine>,
    /// Crashes and ANRs seen in the logs, for the failure message.
    pub notes: Vec<String>,
}

/// Where and what to write when a session ends.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceOut<'a> {
    /// The test's folder (`traces/<test>/`).
    pub dir: &'a Path,
    /// Added to every file name: `-retry1`, a copy's tag.
    pub suffix: &'a str,
    pub keep_video: bool,
    pub keep_logs: bool,
}

impl EvidenceOut<'_> {
    /// `<dir>/<stem><suffix>.<ext>`.
    pub fn file(&self, stem: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{stem}{}.{ext}", self.suffix))
    }
}

/// A test's capture in progress.
#[async_trait::async_trait]
pub trait EvidenceSession: Send + Sync {
    /// Crashes and ANRs seen so far (no device call).
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }

    /// Write what has been captured so far without stopping
    /// (`--pause-on-failure`). Sessions that can't (a recording that only
    /// finalizes when stopped) write nothing.
    async fn checkpoint(&mut self, _out: EvidenceOut<'_>) -> Evidence {
        Evidence::default()
    }

    /// Stop capturing and write what `out` keeps. Never fails: evidence
    /// that can't be written is logged and left out.
    async fn finish(self: Box<Self>, out: EvidenceOut<'_>) -> Evidence;
}

/// The most log lines one test keeps in memory; older ones are dropped.
pub const MAX_LOG_LINES: usize = 200_000;

/// Log lines collected by a background reader, shared with the session.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
}

#[derive(Debug, Default)]
struct LogBufferInner {
    lines: std::collections::VecDeque<LogLine>,
    dropped: usize,
}

impl LogBuffer {
    pub fn push(&self, line: LogLine) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.lines.len() >= MAX_LOG_LINES {
            g.lines.pop_front();
            g.dropped += 1;
        }
        g.lines.push_back(line);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.lines.iter().cloned().collect()
    }

    pub fn dropped(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lines
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Lines as a text file, each secret masked (`crate::redact`): logs quote
/// what the app printed, typed text and tokens included.
pub fn write_log(path: &Path, lines: &[LogLine]) -> std::io::Result<()> {
    let mut text = String::with_capacity(lines.len() * 80);
    for l in lines {
        text.push_str(&crate::redact::redact(&l.text));
        text.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsutil::write_atomic(path, text.as_bytes())
}

/// The lines with secrets masked, for embedding in a trace.
pub fn redacted(lines: Vec<LogLine>) -> Vec<LogLine> {
    lines
        .into_iter()
        .map(|mut l| {
            if let std::borrow::Cow::Owned(s) = crate::redact::redact(&l.text) {
                l.text = s;
            }
            l
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_keep() {
        assert_eq!(
            "on-failure".parse::<CaptureMode>(),
            Ok(CaptureMode::OnFailure)
        );
        assert_eq!("always".parse::<CaptureMode>(), Ok(CaptureMode::Always));
        assert_eq!("off".parse::<CaptureMode>(), Ok(CaptureMode::Off));
        assert!("sometimes".parse::<CaptureMode>().is_err());
        assert!(CaptureMode::OnFailure.keeps(true));
        assert!(!CaptureMode::OnFailure.keeps(false));
        assert!(CaptureMode::Always.keeps(false));
        assert!(!CaptureMode::Off.keeps(true));
        let m: CaptureMode = serde_json::from_str("\"onFailure\"").unwrap();
        assert_eq!(m, CaptureMode::OnFailure);
        assert_eq!(serde_json::to_string(&m).unwrap(), "\"on-failure\"");
    }

    #[test]
    fn log_buffer_drops_the_oldest() {
        let b = LogBuffer::default();
        for i in 0..(MAX_LOG_LINES + 3) {
            b.push(LogLine {
                t: i as i64,
                level: 'I',
                text: String::new(),
            });
        }
        assert_eq!(b.len(), MAX_LOG_LINES);
        assert_eq!(b.dropped(), 3);
        assert_eq!(b.snapshot()[0].t, 3);
    }

    #[test]
    fn written_logs_mask_secrets() {
        crate::redact::register_secret("hunter2-very-secret");
        let dir = std::env::temp_dir().join(format!("vel-evidence-{}", std::process::id()));
        let path = dir.join("app.log");
        write_log(
            &path,
            &[LogLine {
                t: 0,
                level: 'I',
                text: "login with hunter2-very-secret and Bearer abcdefghijklmnop".into(),
            }],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2-very-secret"), "{text}");
        assert!(!text.contains("abcdefghijklmnop"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
