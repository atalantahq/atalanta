// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Android failure evidence: video from the device's video server (or
//! `adb shell screenrecord` when its encoder can't be used) and logcat.
//!
//! One [`VideoStream`] runs per device for the whole run, started by the
//! first test that wants video; every test's clip comes out of its ring
//! buffer, so a passing test costs nothing but the stream itself. Starting
//! is never waited for: the first test may miss its first second.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use atalanta_common::evidence::{
    self, Attachment, AttachmentKind, Evidence, EvidenceOut, EvidenceRequest, EvidenceSession,
    LogLine,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::logcat::Logcat;
use crate::video::{ScreenRecord, VideoOptions, VideoStream};

/// How a device's video is captured.
#[derive(Default)]
enum Source {
    #[default]
    None,
    Starting,
    Stream(Arc<VideoStream>),
    /// The video server failed: screenrecord per test.
    Fallback(String),
}

/// Video streams and clock offsets per device.
#[derive(Default)]
pub struct Recorders {
    video: Mutex<HashMap<String, Source>>,
    clocks: Mutex<HashMap<String, i64>>,
}

/// Times the video server may die before a device falls back for good.
const MAX_RESTARTS: u32 = 3;

impl Recorders {
    /// The device's running stream, started (and waited for) if needed.
    pub async fn stream(
        self: &Arc<Self>,
        adb_path: &str,
        device_id: &str,
        keep: Duration,
    ) -> Result<Arc<VideoStream>, String> {
        {
            let map = self.video.lock().await;
            match map.get(device_id) {
                Some(Source::Stream(s)) if s.is_alive() => {
                    s.keep_at_least(keep);
                    return Ok(s.clone());
                }
                Some(Source::Fallback(why)) => return Err(why.clone()),
                _ => {}
            }
        }
        self.start(adb_path, device_id, keep).await
    }

    async fn start(
        self: &Arc<Self>,
        adb_path: &str,
        device_id: &str,
        keep: Duration,
    ) -> Result<Arc<VideoStream>, String> {
        static RESTARTS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, u32>>> =
            std::sync::LazyLock::new(Default::default);
        self.video
            .lock()
            .await
            .insert(device_id.to_string(), Source::Starting);
        let result = VideoStream::start(adb_path, device_id, VideoOptions::from_env(), keep).await;
        let mut map = self.video.lock().await;
        match result {
            Ok(s) => {
                let s = Arc::new(s);
                map.insert(device_id.to_string(), Source::Stream(s.clone()));
                Ok(s)
            }
            Err(e) => {
                let n = {
                    let mut r = RESTARTS.lock().unwrap_or_else(|e| e.into_inner());
                    let n = r.entry(device_id.to_string()).or_default();
                    *n += 1;
                    *n
                };
                let why = format!("on-device video encoder failed ({e})");
                // The server itself failing (no encoder, refused size) won't
                // fix itself; an adb hiccup might: retried by later tests.
                let encoder =
                    e.contains("video server") || e.contains("video stream") || e.contains("codec");
                if n >= MAX_RESTARTS || encoder {
                    warn!(device_id, "{why}; falling back to adb screenrecord");
                    map.insert(device_id.to_string(), Source::Fallback(why.clone()));
                } else {
                    map.insert(device_id.to_string(), Source::None);
                }
                Err(why)
            }
        }
    }

    /// For a test starting now: the running stream (asked for a key frame
    /// so the clip starts at the test), or `None` while one starts in the
    /// background, or the fallback.
    async fn for_test(self: &Arc<Self>, adb_path: &str, device_id: &str, keep: Duration) -> Pick {
        let mut map = self.video.lock().await;
        // `ATALANTA_ANDROID_VIDEO=screenrecord`: skip the video server (a
        // device whose encoder streams but records garbage).
        if forced_screenrecord() {
            if !matches!(map.get(device_id), Some(Source::Fallback(_))) {
                info!(
                    device_id,
                    "android video: adb screenrecord (ATALANTA_ANDROID_VIDEO)"
                );
                map.insert(
                    device_id.to_string(),
                    Source::Fallback("ATALANTA_ANDROID_VIDEO=screenrecord".into()),
                );
            }
            return Pick::Fallback;
        }
        match map.get(device_id) {
            Some(Source::Stream(s)) if s.is_alive() => {
                s.keep_at_least(keep);
                s.request_key_frame();
                return Pick::Stream(s.clone());
            }
            Some(Source::Stream(s)) => {
                warn!(
                    device_id,
                    "video stream ended ({}); restarting",
                    s.ended().unwrap_or_default()
                );
            }
            Some(Source::Fallback(_)) => return Pick::Fallback,
            Some(Source::Starting) => return Pick::Pending,
            _ => {}
        }
        map.insert(device_id.to_string(), Source::Starting);
        drop(map);
        let (me, adb, dev) = (self.clone(), adb_path.to_string(), device_id.to_string());
        tokio::spawn(async move {
            let _ = me.start(&adb, &dev, keep).await;
        });
        Pick::Pending
    }

    /// The stream once it is up, without starting one.
    async fn current(&self, device_id: &str) -> Option<Arc<VideoStream>> {
        match self.video.lock().await.get(device_id) {
            Some(Source::Stream(s)) => Some(s.clone()),
            _ => None,
        }
    }

    async fn clock(&self, adb_path: &str, device_id: &str) -> Option<i64> {
        if let Some(&o) = self.clocks.lock().await.get(device_id) {
            return Some(o);
        }
        let o = crate::logcat::clock_offset_ms(adb_path, device_id).await?;
        self.clocks.lock().await.insert(device_id.to_string(), o);
        Some(o)
    }

    /// Stop every stream (driver cleanup).
    pub async fn stop_all(&self) {
        let streams: Vec<Arc<VideoStream>> = self
            .video
            .lock()
            .await
            .drain()
            .filter_map(|(_, s)| match s {
                Source::Stream(s) => Some(s),
                _ => None,
            })
            .collect();
        for s in streams {
            s.stop().await;
        }
    }
}

/// `ATALANTA_ANDROID_VIDEO=screenrecord`.
fn forced_screenrecord() -> bool {
    atalanta_common::env::var("ATALANTA_ANDROID_VIDEO").is_ok_and(|v| v.trim() == "screenrecord")
}

enum Pick {
    Stream(Arc<VideoStream>),
    Pending,
    Fallback,
}

enum Video {
    Off,
    Stream(Arc<VideoStream>),
    /// Started in the background by this test: looked up at the end.
    Pending,
    Screen(ScreenRecord),
}

/// One test's capture.
pub struct AndroidEvidence {
    req: EvidenceRequest,
    device_id: String,
    recorders: Arc<Recorders>,
    video: Video,
    logcat: Arc<Mutex<Option<Logcat>>>,
    logcat_task: Option<tokio::task::JoinHandle<()>>,
}

impl AndroidEvidence {
    pub async fn start(
        recorders: Arc<Recorders>,
        adb_path: &str,
        device_id: &str,
        req: &EvidenceRequest,
    ) -> Option<Self> {
        if !req.video.on() && !req.logs.on() {
            return None;
        }
        let wall_start = SystemTime::now() - req.started.elapsed();
        let keep = Duration::from_secs(u64::from(req.video_seconds.max(1)));
        let video = if req.video.on() {
            match recorders.for_test(adb_path, device_id, keep).await {
                Pick::Stream(s) => Video::Stream(s),
                Pick::Pending => Video::Pending,
                Pick::Fallback => Video::Screen(ScreenRecord::start(
                    adb_path,
                    device_id,
                    VideoOptions::from_env(),
                )),
            }
        } else {
            Video::Off
        };
        let logcat = Arc::new(Mutex::new(None));
        let logcat_task = (req.logs.on() && crate::shell::validate_app_id(&req.app_id).is_ok())
            .then(|| {
                let (slot, rec, adb, dev, app, started) = (
                    logcat.clone(),
                    recorders.clone(),
                    adb_path.to_string(),
                    device_id.to_string(),
                    req.app_id.clone(),
                    req.started,
                );
                tokio::spawn(async move {
                    // The device clock is read once per device; the app's
                    // pids from an earlier test, if it is running.
                    let pidof = format!("pidof {app}");
                    let args = ["shell", pidof.as_str()];
                    let (offset, pids) = tokio::join!(
                        rec.clock(&adb, &dev),
                        crate::tree_server::run_adb(&adb, &dev, &args)
                    );
                    let since = crate::logcat::since_arg(wall_start, offset.unwrap_or(0));
                    let pids = pids
                        .map(|o| crate::logcat::parse_pidof(&o))
                        .unwrap_or_default();
                    match Logcat::start(&adb, &dev, &app, since, pids, move || {
                        started.elapsed().as_millis() as i64
                    }) {
                        Ok(l) => *slot.lock().await = Some(l),
                        Err(e) => warn!(device_id = %dev, "logcat not captured: {e}"),
                    }
                })
            });
        Some(Self {
            req: req.clone(),
            device_id: device_id.to_string(),
            recorders,
            video,
            logcat,
            logcat_task,
        })
    }

    /// The stream this test's clip comes from.
    async fn stream(&self) -> Option<Arc<VideoStream>> {
        match &self.video {
            Video::Stream(s) => Some(s.clone()),
            Video::Pending => self.recorders.current(&self.device_id).await,
            _ => None,
        }
    }

    async fn write_clip(&self, out: EvidenceOut<'_>, ev: &mut Evidence) {
        let Some(stream) = self.stream().await else {
            debug!(device_id = %self.device_id, "no video yet for this test");
            return;
        };
        let Some(clip) = stream.clip(Some(self.req.started)) else {
            debug!(device_id = %self.device_id, "the video ring holds no frames");
            return;
        };
        let started = self.req.started;
        let offset = clip.started().map(|t| {
            if t >= started {
                (t - started).as_millis() as i64
            } else {
                -((started - t).as_millis() as i64)
            }
        });
        let path = out.file("video", "mp4");
        // Muxing and writing a clip of several MB, off the runtime's threads.
        let dest = path.clone();
        let written = tokio::task::spawn_blocking(move || {
            let bytes = clip.to_mp4()?;
            write(&dest, &bytes).map_err(|e| format!("{}: {e}", dest.display()))
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        if let Err(e) = written {
            warn!("video not written: {e}");
            return;
        }
        ev.video_offset_ms = offset;
        ev.attachments
            .push(Attachment::new(AttachmentKind::Video, path));
    }

    async fn write_logs(&self, out: EvidenceOut<'_>, ev: &mut Evidence) {
        let guard = self.logcat.lock().await;
        let Some(l) = guard.as_ref() else {
            return;
        };
        let (all, app) = l.split();
        let device = out.file("device", "log");
        let app_path = out.file("app", "log");
        // A whole logcat buffer, redacted line by line and written: off the
        // runtime's threads.
        let (dev_to, app_to) = (device.clone(), app_path.clone());
        let written = tokio::task::spawn_blocking(move || {
            let dev_written = evidence::write_log(&dev_to, &all);
            let app_written = evidence::write_log(&app_to, &app);
            (dev_written, app_written, all, app)
        })
        .await;
        let (dev_written, app_written, all, app) = match written {
            Ok(w) => w,
            Err(e) => {
                warn!("logs not written: {e}");
                return;
            }
        };
        match dev_written {
            Ok(()) => ev
                .attachments
                .push(Attachment::new(AttachmentKind::DeviceLog, device)),
            Err(e) => warn!("device log not written: {e}"),
        }
        match app_written {
            Ok(()) => ev
                .attachments
                .push(Attachment::new(AttachmentKind::AppLog, app_path)),
            Err(e) => warn!("app log not written: {e}"),
        }
        ev.log_lines = viewer_lines(if app.is_empty() { all } else { app });
    }
}

/// The most log lines a trace viewer embeds.
const VIEWER_LINES: usize = 20_000;

fn viewer_lines(mut lines: Vec<LogLine>) -> Vec<LogLine> {
    if lines.len() > VIEWER_LINES {
        lines.drain(..lines.len() - VIEWER_LINES);
    }
    evidence::redacted(lines)
}

fn write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    atalanta_common::fsutil::write_atomic(path, bytes)
}

#[async_trait::async_trait]
impl EvidenceSession for AndroidEvidence {
    fn notes(&self) -> Vec<String> {
        self.logcat
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(Logcat::notes))
            .unwrap_or_default()
    }

    async fn checkpoint(&mut self, out: EvidenceOut<'_>) -> Evidence {
        let mut ev = Evidence::default();
        if out.keep_video {
            self.write_clip(out, &mut ev).await;
        }
        if out.keep_logs {
            self.write_logs(out, &mut ev).await;
        }
        ev
    }

    async fn finish(mut self: Box<Self>, out: EvidenceOut<'_>) -> Evidence {
        let mut ev = Evidence::default();
        if let Some(t) = self.logcat_task.take() {
            // Never wait long for a logcat that is still starting.
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
        // Lines still in flight matter only when they're kept.
        let drain = if out.keep_logs {
            Duration::from_millis(300)
        } else {
            Duration::ZERO
        };
        if let Some(l) = self.logcat.lock().await.as_mut() {
            l.stop(drain).await;
            ev.notes = l.notes();
        }
        match std::mem::replace(&mut self.video, Video::Off) {
            Video::Screen(rec) => {
                let dest: Option<PathBuf> = out.keep_video.then(|| out.file("video", "mp4"));
                let started = self.req.started;
                let (files, first) = rec.finish(dest.as_deref()).await;
                if !files.is_empty() {
                    info!("video (adb screenrecord) saved to {}", files[0].display());
                }
                ev.video_offset_ms =
                    first.map(|t| t.saturating_duration_since(started).as_millis() as i64);
                ev.attachments.extend(
                    files
                        .into_iter()
                        .map(|f| Attachment::new(AttachmentKind::Video, f)),
                );
            }
            other => {
                self.video = other;
                if out.keep_video {
                    self.write_clip(out, &mut ev).await;
                }
            }
        }
        if out.keep_logs {
            self.write_logs(out, &mut ev).await;
        }
        ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_lines_keep_the_last_and_mask_secrets() {
        atalanta_common::redact::register_secret("android-secret-value-1");
        let lines: Vec<LogLine> = (0..VIEWER_LINES + 5)
            .map(|i| LogLine {
                t: i as i64,
                level: 'I',
                text: format!("line {i} android-secret-value-1"),
            })
            .collect();
        let v = viewer_lines(lines);
        assert_eq!(v.len(), VIEWER_LINES);
        assert_eq!(v[0].t, 5);
        assert!(!v[0].text.contains("android-secret-value-1"));
    }
}
