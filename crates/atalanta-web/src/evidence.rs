// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Web failure evidence: the page's screencast and its console.
//!
//! - **Video.** `Page.startScreencast` sends JPEG frames as the page
//!   repaints. They are kept in memory (the last `videoSeconds`) and, for a
//!   kept test, written as `screencast/<n>.jpg` plus `screencast.json`
//!   (each frame's test time). No MP4: encoding would need ffmpeg, and the
//!   trace viewer plays the frames itself.
//! - **Logs.** Console calls, uncaught exceptions, `Log` entries
//!   (interventions, CSP, network errors the browser reports) and requests
//!   that failed or answered 4xx/5xx, written as `console.log`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atalanta_common::evidence::{
    self, Attachment, AttachmentKind, Evidence, EvidenceOut, EvidenceRequest, EvidenceSession,
    LogBuffer, LogLine,
};
use chromiumoxide::cdp::browser_protocol::log::{EnableParams as LogEnable, EventEntryAdded};
use chromiumoxide::cdp::browser_protocol::network::{EventLoadingFailed, EventResponseReceived};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
    StopScreencastParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, EventExceptionThrown};
use futures::StreamExt;
use serde::Serialize;
use tracing::{debug, warn};

use crate::cdp::Page;

/// One kept screencast frame.
struct Shot {
    t: i64,
    jpeg: Vec<u8>,
    at: Instant,
}

#[derive(Default)]
struct Frames {
    shots: VecDeque<Shot>,
    bytes: usize,
    width: u32,
    height: u32,
}

/// Frames over this many bytes in memory drop the oldest.
const MAX_FRAME_BYTES: usize = 256 << 20;

impl Frames {
    fn push(&mut self, shot: Shot, keep: Duration) {
        self.bytes += shot.jpeg.len();
        let now = shot.at;
        self.shots.push_back(shot);
        while self.shots.len() > 1
            && (self.bytes > MAX_FRAME_BYTES || now.duration_since(self.shots[0].at) > keep)
        {
            if let Some(old) = self.shots.pop_front() {
                self.bytes -= old.jpeg.len();
            }
        }
    }
}

/// `screencast.json`: what the trace viewer plays.
#[derive(Serialize)]
struct Index {
    width: u32,
    height: u32,
    frames: Vec<IndexFrame>,
}

#[derive(Serialize)]
struct IndexFrame {
    t: i64,
    file: String,
}

pub struct WebEvidence {
    page: Arc<Page>,
    frames: Arc<Mutex<Frames>>,
    lines: LogBuffer,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    screencast: bool,
}

/// A console argument as text: its value, else its description.
fn arg_text(v: &chromiumoxide::cdp::js_protocol::runtime::RemoteObject) -> String {
    match &v.value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => v
            .description
            .clone()
            .unwrap_or_else(|| format!("{:?}", v.r#type).to_lowercase()),
    }
}

/// The level letter of a console call type (`error` → E, `warning` → W).
fn console_level(kind: &str) -> char {
    match kind {
        "error" | "assert" => 'E',
        "warning" | "warn" => 'W',
        "debug" | "trace" => 'D',
        _ => 'I',
    }
}

impl WebEvidence {
    pub async fn start(page: Arc<Page>, req: &EvidenceRequest) -> Option<Self> {
        if !req.video.on() && !req.logs.on() {
            return None;
        }
        let frames = Arc::new(Mutex::new(Frames::default()));
        let lines = LogBuffer::default();
        let mut tasks = Vec::new();
        let started = req.started;
        let now = move || started.elapsed().as_millis() as i64;
        let mut screencast = false;
        if req.video.on() {
            match page.page.event_listener::<EventScreencastFrame>().await {
                Ok(mut events) => {
                    let p = page.clone();
                    let f = frames.clone();
                    let keep = Duration::from_secs(u64::from(req.video_seconds.max(1)));
                    tasks.push(tokio::spawn(async move {
                        use base64::Engine;
                        while let Some(e) = events.next().await {
                            // Ack first: Chrome sends the next frame only then.
                            let _ = p.exec(ScreencastFrameAckParams::new(e.session_id)).await;
                            let Ok(jpeg) = base64::engine::general_purpose::STANDARD
                                .decode(AsRef::<str>::as_ref(&e.data))
                            else {
                                continue;
                            };
                            let mut g = f.lock().unwrap_or_else(|e| e.into_inner());
                            g.width = e.metadata.device_width as u32;
                            g.height = e.metadata.device_height as u32;
                            g.push(
                                Shot {
                                    t: now(),
                                    jpeg,
                                    at: Instant::now(),
                                },
                                keep,
                            );
                        }
                    }));
                    let params = StartScreencastParams::builder()
                        .format(StartScreencastFormat::Jpeg)
                        .quality(60)
                        .max_width(1280)
                        .max_height(1280)
                        .every_nth_frame(1)
                        .build();
                    match page.exec(params).await {
                        Ok(_) => screencast = true,
                        Err(e) => warn!("web screencast not started: {e}"),
                    }
                }
                Err(e) => warn!("web screencast not watched: {e}"),
            }
        }
        if req.logs.on() {
            if let Err(e) = page.exec(LogEnable::default()).await {
                debug!("Log domain not enabled: {e}");
            }
            if let Ok(mut ev) = page.page.event_listener::<EventConsoleApiCalled>().await {
                let b = lines.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        let kind = serde_json::to_value(&e.r#type)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_else(|| "log".into());
                        let text: Vec<String> = e.args.iter().map(arg_text).collect();
                        b.push(LogLine {
                            t: now(),
                            level: console_level(&kind),
                            text: format!("console.{kind}: {}", text.join(" ")),
                        });
                    }
                }));
            }
            if let Ok(mut ev) = page.page.event_listener::<EventExceptionThrown>().await {
                let b = lines.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        let d = &e.exception_details;
                        let what = d
                            .exception
                            .as_ref()
                            .and_then(|x| x.description.clone())
                            .unwrap_or_else(|| d.text.clone());
                        let at = d.url.as_deref().unwrap_or("");
                        b.push(LogLine {
                            t: now(),
                            level: 'E',
                            text: format!(
                                "page error: {what} ({at}:{}:{})",
                                d.line_number + 1,
                                d.column_number + 1
                            ),
                        });
                    }
                }));
            }
            if let Ok(mut ev) = page.page.event_listener::<EventEntryAdded>().await {
                let b = lines.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        let level = serde_json::to_value(&e.entry.level)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default();
                        let url = e
                            .entry
                            .url
                            .as_deref()
                            .map(|u| format!(" ({u})"))
                            .unwrap_or_default();
                        b.push(LogLine {
                            t: now(),
                            level: console_level(&level),
                            text: format!("browser {level}: {}{url}", e.entry.text),
                        });
                    }
                }));
            }
            if let Ok(mut ev) = page.page.event_listener::<EventLoadingFailed>().await {
                let b = lines.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        if e.canceled == Some(true) {
                            continue;
                        }
                        b.push(LogLine {
                            t: now(),
                            level: 'E',
                            text: format!(
                                "request failed: {} (request {})",
                                e.error_text,
                                e.request_id.inner()
                            ),
                        });
                    }
                }));
            }
            if let Ok(mut ev) = page.page.event_listener::<EventResponseReceived>().await {
                let b = lines.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(e) = ev.next().await {
                        let status = e.response.status;
                        if status >= 400 {
                            b.push(LogLine {
                                t: now(),
                                level: if status >= 500 { 'E' } else { 'W' },
                                text: format!("HTTP {status} {}", e.response.url),
                            });
                        }
                    }
                }));
            }
        }
        Some(Self {
            page,
            frames,
            lines,
            tasks,
            screencast,
        })
    }

    fn write_frames(&self, out: EvidenceOut<'_>, ev: &mut Evidence) {
        let g = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        if g.shots.is_empty() {
            return;
        }
        let dir_name = format!("screencast{}", out.suffix);
        let dir = out.dir.join(&dir_name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("screencast not written: {e}");
            return;
        }
        let mut index = Index {
            width: g.width,
            height: g.height,
            frames: Vec::with_capacity(g.shots.len()),
        };
        for (i, s) in g.shots.iter().enumerate() {
            let name = format!("{i:05}.jpg");
            if std::fs::write(dir.join(&name), &s.jpeg).is_err() {
                continue;
            }
            index.frames.push(IndexFrame {
                t: s.t,
                file: format!("{dir_name}/{name}"),
            });
        }
        let path = out.file("screencast", "json");
        match serde_json::to_vec(&index)
            .map_err(std::io::Error::other)
            .and_then(|b| atalanta_common::fsutil::write_atomic(&path, &b))
        {
            Ok(()) => {
                ev.video_offset_ms = index.frames.first().map(|f| f.t);
                ev.attachments
                    .push(Attachment::new(AttachmentKind::Screencast, path));
            }
            Err(e) => warn!("screencast index not written: {e}"),
        }
    }

    fn write_logs(&self, out: EvidenceOut<'_>, ev: &mut Evidence) {
        let all = self.lines.snapshot();
        let path = out.file("console", "log");
        match evidence::write_log(&path, &all) {
            Ok(()) => ev
                .attachments
                .push(Attachment::new(AttachmentKind::AppLog, path)),
            Err(e) => warn!("console log not written: {e}"),
        }
        ev.log_lines = evidence::redacted(all);
        ev.notes = ev
            .log_lines
            .iter()
            .filter(|l| l.text.starts_with("page error: "))
            .take(3)
            .map(|l| l.text.clone())
            .collect();
    }
}

#[async_trait::async_trait]
impl EvidenceSession for WebEvidence {
    async fn checkpoint(&mut self, out: EvidenceOut<'_>) -> Evidence {
        let mut ev = Evidence::default();
        if out.keep_video {
            self.write_frames(out, &mut ev);
        }
        if out.keep_logs {
            self.write_logs(out, &mut ev);
        }
        ev
    }

    async fn finish(self: Box<Self>, out: EvidenceOut<'_>) -> Evidence {
        if self.screencast {
            let stop = self.page.exec(StopScreencastParams::default());
            let _ = tokio::time::timeout(Duration::from_secs(2), stop).await;
        }
        for t in &self.tasks {
            t.abort();
        }
        let mut ev = Evidence::default();
        if out.keep_video {
            self.write_frames(out, &mut ev);
        }
        if out.keep_logs {
            self.write_logs(out, &mut ev);
        }
        ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_the_last_seconds() {
        let t0 = Instant::now();
        let mut f = Frames::default();
        for i in 0..10u64 {
            f.push(
                Shot {
                    t: i as i64 * 1000,
                    jpeg: vec![0; 10],
                    at: t0 + Duration::from_secs(i),
                },
                Duration::from_secs(3),
            );
        }
        assert_eq!(f.shots.len(), 4, "frames 6..=9 are within 3 s of the last");
        assert_eq!(f.shots[0].t, 6000);
        assert_eq!(f.bytes, 40);
    }

    #[test]
    fn console_levels() {
        assert_eq!(console_level("error"), 'E');
        assert_eq!(console_level("warning"), 'W');
        assert_eq!(console_level("log"), 'I');
    }
}
