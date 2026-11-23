// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Where the mirror's pictures come from, and how they are framed.
//!
//! A device has at most one live feed, shared by every viewer:
//! - **H.264** (Android): the driver's [`VideoStream`], whose broadcast every
//!   viewer subscribes to;
//! - **pushed frames** (the web's CDP screencast, iOS WebDriverAgent's
//!   MJPEG): a [`FrameFeed`] read by one task that fans it out ([`FrameHub`]);
//! - **screenshots** otherwise: one polling task per device ([`FrameHub`]
//!   again) that sends a frame only when the screen changed, fast after a
//!   change or an input and backing off to 1 s while nothing moves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atalanta_android::video::VideoStream;
use tokio::sync::{broadcast, mpsc};

use crate::state::AppState;

/// A still picture of the screen.
#[derive(Clone, Debug)]
pub struct ImageFrame {
    pub data: Arc<[u8]>,
    pub png: bool,
    /// Its size in pixels, when known (0 otherwise).
    pub width: u32,
    pub height: u32,
    /// When the host got it, Unix µs.
    pub received_us: u64,
}

impl ImageFrame {
    pub fn jpeg(data: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            data: data.into(),
            png: false,
            width,
            height,
            received_us: unix_us(),
        }
    }
    /// A PNG or JPEG, told apart by its signature; its size from the header.
    pub fn sniff(data: Vec<u8>) -> Self {
        let png = data.starts_with(b"\x89PNG");
        let (width, height) = if png { png_size(&data) } else { (0, 0) };
        Self {
            data: data.into(),
            png,
            width,
            height,
            received_us: unix_us(),
        }
    }
}

/// The size in a PNG's IHDR.
pub fn png_size(png: &[u8]) -> (u32, u32) {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return (0, 0);
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    (w, h)
}

pub fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Pushed frames, and whatever keeps them coming (dropped to stop).
pub struct FrameFeed {
    pub frames: mpsc::Receiver<ImageFrame>,
    /// What the frames come from, for the page ("web-screencast", "ios-mjpeg").
    pub source: &'static str,
    pub guard: Box<dyn std::any::Any + Send + Sync>,
}

/// The fast paths a device's screen can be watched through. The default
/// methods say there is none: the mirror then polls screenshots.
#[async_trait::async_trait]
pub trait LiveSource: Send + Sync {
    /// The device's H.264 stream (Android), started or reused.
    async fn h264(&self, _device_id: &str) -> Option<Result<Arc<VideoStream>, String>> {
        None
    }
    /// Pushed frames (web screencast, iOS MJPEG).
    async fn frames(&self, _device_id: &str) -> Option<Result<FrameFeed, String>> {
        None
    }
    /// One still, cheaper than the driver's PNG screenshot (a JPEG).
    async fn still(&self, _device_id: &str) -> Option<ImageFrame> {
        None
    }
}

/// No fast path at all.
pub struct NoLive;
impl LiveSource for NoLive {}

// ── Binary framing ──────────────────────────────────────────────────────

/// Codec configuration (SPS/PPS, Annex-B).
pub const KIND_CONFIG: u8 = 1;
/// One H.264 access unit (Annex-B).
pub const KIND_H264: u8 = 2;
pub const KIND_JPEG: u8 = 3;
pub const KIND_PNG: u8 = 4;
/// Flag bit: a key frame.
pub const FLAG_KEY: u8 = 1;
pub const HEADER_LEN: usize = 18;

/// One binary mirror message: `kind u8, flags u8, pts_us u64, host
/// receive time (Unix µs) u64`, all big-endian, then the payload.
pub fn frame_message(kind: u8, key: bool, pts_us: u64, received_us: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.push(kind);
    out.push(if key { FLAG_KEY } else { 0 });
    out.extend_from_slice(&pts_us.to_be_bytes());
    out.extend_from_slice(&received_us.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// The WebCodecs codec string (`avc1.PPCCLL`) of an Annex-B SPS/PPS blob:
/// profile, constraint flags and level from the first SPS.
pub fn avc_codec(config: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 3 < config.len() {
        let start = if config[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if config[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let nal = i + start;
        if nal + 3 < config.len() && config[nal] & 0x1F == 7 {
            return Some(format!(
                "avc1.{:02x}{:02x}{:02x}",
                config[nal + 1],
                config[nal + 2],
                config[nal + 3]
            ));
        }
        i = nal;
    }
    None
}

// ── Shared frame hubs ───────────────────────────────────────────────────

/// One device's pushed or polled frames, fanned out to every viewer. It
/// runs while a viewer holds it.
pub struct FrameHub {
    pub tx: broadcast::Sender<ImageFrame>,
    pub latest: Arc<Mutex<Option<ImageFrame>>>,
    pub source: Mutex<&'static str>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for FrameHub {
    fn drop(&mut self) {
        if let Some(t) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }
}

impl FrameHub {
    pub fn latest(&self) -> Option<ImageFrame> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn source(&self) -> &'static str {
        *self.source.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The live hubs by device.
#[derive(Default)]
pub struct Hubs {
    map: tokio::sync::Mutex<HashMap<String, Weak<FrameHub>>>,
}

/// The polling period right after a change.
pub const POLL_FAST: Duration = Duration::from_millis(80);
/// The longest it backs off to while the screen stays the same.
pub const POLL_SLOWEST: Duration = Duration::from_millis(1000);

impl Hubs {
    /// `device_id`'s hub, started if no viewer has one.
    pub async fn get(&self, state: &Arc<AppState>, device_id: &str) -> Arc<FrameHub> {
        let mut map = self.map.lock().await;
        if let Some(hub) = map.get(device_id).and_then(Weak::upgrade) {
            return hub;
        }
        let (tx, _) = broadcast::channel(4);
        let hub = Arc::new(FrameHub {
            tx,
            latest: Arc::new(Mutex::new(None)),
            source: Mutex::new("screenshots"),
            task: Mutex::new(None),
        });
        let feed = match state.console.live.frames(device_id).await {
            Some(Ok(feed)) => Some(feed),
            Some(Err(e)) => {
                tracing::info!(device_id, "no pushed frames ({e}); polling screenshots");
                None
            }
            None => None,
        };
        let task = match feed {
            Some(feed) => {
                *hub.source.lock().unwrap_or_else(|e| e.into_inner()) = feed.source;
                tokio::spawn(forward(
                    feed,
                    hub.tx.clone(),
                    hub.latest.clone(),
                    Arc::downgrade(state),
                    device_id.to_string(),
                ))
            }
            None => tokio::spawn(poll(
                Arc::downgrade(state),
                device_id.to_string(),
                hub.tx.clone(),
                hub.latest.clone(),
            )),
        };
        *hub.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(device_id.to_string(), Arc::downgrade(&hub));
        hub
    }
}

fn publish(
    tx: &broadcast::Sender<ImageFrame>,
    latest: &Mutex<Option<ImageFrame>>,
    frame: ImageFrame,
) {
    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame.clone());
    let _ = tx.send(frame);
}

/// Pushed frames to the hub; when the feed ends, polling takes over.
async fn forward(
    mut feed: FrameFeed,
    tx: broadcast::Sender<ImageFrame>,
    latest: Arc<Mutex<Option<ImageFrame>>>,
    state: Weak<AppState>,
    device_id: String,
) {
    while let Some(f) = feed.frames.recv().await {
        publish(&tx, &latest, f);
    }
    tracing::info!(%device_id, "pushed frames ended; polling screenshots");
    drop(feed);
    poll(state, device_id, tx, latest).await;
}

/// Screenshots, sent when they change. 80 ms after a change, doubling to
/// 1 s while the screen stays still; an input (`AppState::poke`) or a
/// change starts it fast again.
async fn poll(
    state: Weak<AppState>,
    device_id: String,
    tx: broadcast::Sender<ImageFrame>,
    latest: Arc<Mutex<Option<ImageFrame>>>,
) {
    let mut last: Option<u64> = None;
    let mut period = POLL_FAST;
    loop {
        let Some(st) = state.upgrade() else { return };
        let frame = match st.console.live.still(&device_id).await {
            Some(f) => Some(f),
            None => st
                .driver
                .screenshot(&device_id)
                .await
                .ok()
                .filter(|b| !b.is_empty())
                .map(ImageFrame::sniff),
        };
        match frame {
            Some(f) => {
                let hash = crate::snapshot::bytes_hash(&f.data);
                if last != Some(hash) {
                    last = Some(hash);
                    period = POLL_FAST;
                    publish(&tx, &latest, f);
                } else {
                    period = (period * 2).min(POLL_SLOWEST);
                }
            }
            None => period = POLL_SLOWEST,
        }
        // Held for at most a period: the server's state outlives it anyway.
        let woke = st.activity.notified();
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = woke => { period = POLL_FAST; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_strings_come_from_the_sps() {
        // SPS of a Baseline 3.1 stream (42 E0 1F), then a PPS.
        let cfg = [
            0, 0, 0, 1, 0x67, 0x42, 0xE0, 0x1F, 0x8D, 0, 0, 0, 1, 0x68, 0xCE,
        ];
        assert_eq!(avc_codec(&cfg).as_deref(), Some("avc1.42e01f"));
        let three = [0, 0, 1, 0x68, 0xCE, 0, 0, 1, 0x27, 0x64, 0x00, 0x29];
        assert_eq!(avc_codec(&three).as_deref(), Some("avc1.640029"));
        assert_eq!(avc_codec(&[0, 0, 0, 1, 0x68]), None);
    }

    #[test]
    fn frames_carry_a_fixed_header() {
        let m = frame_message(KIND_H264, true, 0x0102, 7, b"abc");
        assert_eq!(m.len(), HEADER_LEN + 3);
        assert_eq!(m[0], KIND_H264);
        assert_eq!(m[1], FLAG_KEY);
        assert_eq!(u64::from_be_bytes(m[2..10].try_into().unwrap()), 0x0102);
        assert_eq!(u64::from_be_bytes(m[10..18].try_into().unwrap()), 7);
        assert_eq!(&m[18..], b"abc");
    }

    #[test]
    fn png_sizes_come_from_the_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        assert_eq!(png_size(&png), (1080, 2400));
        let f = ImageFrame::sniff(png);
        assert!(f.png && f.width == 1080);
        assert_eq!(png_size(b"nope"), (0, 0));
    }
}
