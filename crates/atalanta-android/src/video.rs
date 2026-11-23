//! Screen video from the on-device video server, scrcpy-style.
//!
//! The tree server's jar also carries `com.atalanta.tree.VideoServer`
//! (`probes/android/atalanta-tree-server`). It runs as its own
//! `app_process` (as scrcpy's server does), mirrors the display onto a
//! MediaCodec H.264 encoder's input surface and streams the encoded
//! packets over an abstract socket that `adb forward` exposes. One
//! [`VideoStream`] per device reads that stream:
//!
//! - it keeps the last N seconds in a [`Ring`] (cut at key frames), from
//!   which a failed test's clip is muxed to MP4 ([`crate::mp4`]);
//! - it broadcasts every packet to subscribers ([`VideoStream::subscribe`]),
//!   for a live view (a browser can decode the Annex-B packets with
//!   WebCodecs).
//!
//! Wire format (big-endian), from the device:
//!
//! ```text
//! u32 codec id ("h264")
//! then packets with a 12-byte header:
//!   session  u32 flags (bit 31 set), u32 width, u32 height     (no payload)
//!   media    u64 pts|flags, u32 size, size bytes of Annex-B H.264
//!            bit 62 config (SPS/PPS), bit 61 key frame, low 61 bits pts (µs)
//! ```
//!
//! and one byte at a time to the device: `K` asks for a key frame, `Q`
//! stops the server. The process exits when its client goes.
//!
//! Before the stream (video server 2, security review #10) the host proves
//! it holds the server's per-start token, and the server that it does
//! (`authenticate`): the `adb forward` port is on the host's loopback,
//! where any local process used to be able to take the one client slot and
//! watch the screen. The token is in a device file only shell or root can
//! read; it never crosses the socket.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// `"h264"` as the server's first four bytes.
pub const CODEC_H264: u32 = 0x6832_3634;
const FLAG_SESSION: u64 = 1 << 63;
const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = (1 << 61) - 1;
/// The largest packet accepted: a corrupt length must not allocate the
/// host out of memory.
const MAX_PACKET: usize = 16 << 20;
/// Packets a slow subscriber may fall behind before it misses some.
const BROADCAST_CAPACITY: usize = 512;
const START_TIMEOUT: Duration = Duration::from_secs(8);
/// The video server protocol this host speaks (`VideoServer.VERSION`).
const VIDEO_VERSION: &str = "2";

/// The handshake before the stream: `HELLO <client nonce>`, check the
/// server's `PROOF <server nonce> <hmac>`, `AUTH <hmac>`, then `OK`. Lines
/// are read a byte at a time, so nothing of the stream after `OK` is
/// buffered away.
pub(crate) async fn authenticate<S>(stream: &mut S, token: &str) -> Result<(), String>
where
    S: AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::tree_server::{client_nonce, constant_time_eq, hex, hmac_sha256};
    let mac = |label: &str, cn: &str, sn: &str| {
        hex(&hmac_sha256(
            token.as_bytes(),
            format!("atalanta-video {label} {cn} {sn}").as_bytes(),
        ))
    };
    let fut = async {
        let cn = client_nonce();
        stream
            .write_all(format!("HELLO {cn}\n").as_bytes())
            .await
            .map_err(|e| format!("video handshake: {e}"))?;
        let proof = read_line(stream).await?;
        let mut it = proof.strip_prefix("PROOF ").unwrap_or("").split(' ');
        let (sn, got) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        if sn.is_empty() || !constant_time_eq(got.as_bytes(), mac("server", &cn, sn).as_bytes()) {
            return Err(
                "the video socket's server could not prove it is Atalanta's; not streaming from it"
                    .to_string(),
            );
        }
        stream
            .write_all(format!("AUTH {}\n", mac("client", &cn, sn)).as_bytes())
            .await
            .map_err(|e| format!("video handshake: {e}"))?;
        match read_line(stream).await?.as_str() {
            "OK" => Ok(()),
            other => Err(format!("the video server refused the handshake: {other:?}")),
        }
    };
    tokio::time::timeout(Duration::from_secs(3), fut)
        .await
        .map_err(|_| "video handshake timed out (3s)".to_string())?
}

/// One line (at most 256 bytes) without its newline.
async fn read_line<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let b = stream
            .read_u8()
            .await
            .map_err(|e| format!("video handshake: {e}"))?;
        if b == b'\n' {
            return Ok(String::from_utf8_lossy(&line).into_owned());
        }
        if line.len() >= 256 {
            return Err("video handshake: line too long".into());
        }
        line.push(b);
    }
}

/// Encoder settings sent to the device server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoOptions {
    /// Longest side of the video, in pixels (the screen is scaled down).
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
    /// Seconds between key frames (the ring buffer cuts at them).
    pub key_frame_interval: u32,
}

impl Default for VideoOptions {
    /// 720p (1280 on the long side), 2 Mbit/s, 30 fps, a key frame a second.
    fn default() -> Self {
        Self {
            max_size: 1280,
            bit_rate: 2_000_000,
            max_fps: 30,
            key_frame_interval: 1,
        }
    }
}

impl VideoOptions {
    /// The defaults, overridden by `ATALANTA_ANDROID_VIDEO_SIZE`,
    /// `ATALANTA_ANDROID_VIDEO_BITRATE` and `ATALANTA_ANDROID_VIDEO_FPS`.
    pub fn from_env() -> Self {
        let mut o = Self::default();
        let num = |k: &str| {
            atalanta_common::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .filter(|v| *v > 0)
        };
        if let Some(v) = num("ATALANTA_ANDROID_VIDEO_SIZE") {
            o.max_size = v.max(64);
        }
        if let Some(v) = num("ATALANTA_ANDROID_VIDEO_BITRATE") {
            o.bit_rate = v;
        }
        if let Some(v) = num("ATALANTA_ANDROID_VIDEO_FPS") {
            o.max_fps = v;
        }
        o
    }
}

/// One thing the device sent.
#[derive(Debug, Clone)]
pub enum VideoEvent {
    /// The video's size, before its first frame and after every change (a
    /// rotation). The next packet is a [`VideoEvent::Config`].
    Session { width: u32, height: u32 },
    /// SPS and PPS, Annex-B.
    Config(Arc<[u8]>),
    /// One encoded frame.
    Frame(VideoFrame),
}

/// One encoded frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// The encoder's presentation time, in microseconds (device clock).
    pub pts_us: u64,
    pub key: bool,
    /// Annex-B NAL units.
    pub data: Arc<[u8]>,
    /// When the host received it.
    pub received: Instant,
}

/// The codec id the stream starts with.
pub async fn read_codec<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<u32> {
    r.read_u32().await
}

/// The next event of the stream; `None` at a clean end.
pub async fn read_event<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<VideoEvent>> {
    let mut head = [0u8; 12];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let word = u64::from_be_bytes(head[..8].try_into().unwrap_or_default());
    if word & FLAG_SESSION != 0 {
        let width = u32::from_be_bytes(head[4..8].try_into().unwrap_or_default());
        let height = u32::from_be_bytes(head[8..12].try_into().unwrap_or_default());
        return Ok(Some(VideoEvent::Session { width, height }));
    }
    let size = u32::from_be_bytes(head[8..12].try_into().unwrap_or_default()) as usize;
    if size > MAX_PACKET {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("video packet of {size} bytes"),
        ));
    }
    let mut data = vec![0u8; size];
    r.read_exact(&mut data).await?;
    let data: Arc<[u8]> = data.into();
    if word & FLAG_CONFIG != 0 {
        return Ok(Some(VideoEvent::Config(data)));
    }
    Ok(Some(VideoEvent::Frame(VideoFrame {
        pts_us: word & PTS_MASK,
        key: word & FLAG_KEY_FRAME != 0,
        data,
        received: Instant::now(),
    })))
}

/// A muxable piece of the stream: parameter sets, size, and frames from a
/// key frame on.
#[derive(Debug, Clone)]
pub struct Clip {
    pub config: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<VideoFrame>,
}

impl Clip {
    /// The clip as an MP4 file.
    pub fn to_mp4(&self) -> Result<Vec<u8>, String> {
        let frames: Vec<crate::mp4::Frame<'_>> = self
            .frames
            .iter()
            .map(|f| crate::mp4::Frame {
                pts_us: f.pts_us,
                key: f.key,
                data: &f.data,
            })
            .collect();
        crate::mp4::mux_h264(&self.config, self.width, self.height, &frames)
    }

    /// When the clip's first frame arrived.
    pub fn started(&self) -> Option<Instant> {
        self.frames.first().map(|f| f.received)
    }
}

/// The last `keep` of the stream, always starting at a key frame.
#[derive(Debug)]
pub struct Ring {
    keep: Duration,
    config: Option<Arc<[u8]>>,
    size: Option<(u32, u32)>,
    frames: VecDeque<VideoFrame>,
    bytes: usize,
}

impl Ring {
    pub fn new(keep: Duration) -> Self {
        Self {
            keep,
            config: None,
            size: None,
            frames: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn set_keep(&mut self, keep: Duration) {
        self.keep = keep;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn push(&mut self, event: &VideoEvent) {
        match event {
            VideoEvent::Session { width, height } => {
                // A new size starts a new stream: frames of two sizes
                // can't share one MP4 track.
                self.size = Some((*width, *height));
                self.frames.clear();
                self.bytes = 0;
                self.config = None;
            }
            VideoEvent::Config(c) => self.config = Some(c.clone()),
            VideoEvent::Frame(f) => {
                // Nothing decodes before the first key frame.
                if self.frames.is_empty() && !f.key {
                    return;
                }
                self.bytes += f.data.len();
                self.frames.push_back(f.clone());
                self.trim(f.received);
            }
        }
    }

    /// Drop whole groups of pictures older than `keep`: everything before
    /// the newest key frame that is at least `keep` old, so the oldest
    /// frame kept is a key frame and at least `keep` is covered.
    fn trim(&mut self, now: Instant) {
        let Some(cutoff) = now.checked_sub(self.keep) else {
            return;
        };
        let cut = self
            .frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.key && f.received <= cutoff)
            .map(|(i, _)| i)
            .next_back();
        if let Some(cut) = cut.filter(|&c| c > 0) {
            for f in self.frames.drain(..cut) {
                self.bytes -= f.data.len();
            }
        }
    }

    /// The frames from the last key frame at or before `since` (the first
    /// key frame kept, when `since` is older) to now. `None` without a
    /// size, parameter sets or frames.
    pub fn clip(&self, since: Option<Instant>) -> Option<Clip> {
        let (width, height) = self.size?;
        let config = self.config.clone()?;
        let start = match since {
            Some(t) => self
                .frames
                .iter()
                .enumerate()
                .filter(|(_, f)| f.key && f.received <= t)
                .map(|(i, _)| i)
                .next_back()
                .unwrap_or(0),
            None => 0,
        };
        let frames: Vec<VideoFrame> = self.frames.iter().skip(start).cloned().collect();
        if frames.is_empty() {
            return None;
        }
        Some(Clip {
            config,
            width,
            height,
            frames,
        })
    }
}

/// What a subscriber needs to start decoding: the current size and
/// parameter sets (a key frame follows soon: subscribing asks for one).
#[derive(Debug)]
pub struct VideoSubscription {
    /// `Session` and `Config` events to feed the decoder first (empty
    /// while the stream hasn't sent them yet: they arrive on `events`).
    pub primer: Vec<VideoEvent>,
    /// Every event from now on. A subscriber that falls more than 512
    /// packets behind gets `RecvError::Lagged`; it should then wait for
    /// the next key frame (or call [`VideoStream::request_key_frame`]).
    pub events: broadcast::Receiver<VideoEvent>,
}

struct Shared {
    ring: Mutex<Ring>,
    alive: std::sync::atomic::AtomicBool,
    /// Why the stream ended, if it did.
    ended: Mutex<Option<String>>,
    frames: std::sync::atomic::AtomicU64,
}

/// A running screen stream of one device.
///
/// This is the live API: `VideoStream::start` (or
/// `AndroidDriver::video_stream`) gives a stream a live console can
/// [`subscribe`](Self::subscribe) to; the evidence recorder uses the same
/// stream's ring buffer.
pub struct VideoStream {
    device_id: String,
    tx: broadcast::Sender<VideoEvent>,
    control: mpsc::UnboundedSender<u8>,
    shared: Arc<Shared>,
    started: Instant,
    _child: Option<atalanta_common::process::GroupChild>,
    port: Option<u16>,
    adb_path: String,
}

impl std::fmt::Debug for VideoStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoStream")
            .field("device_id", &self.device_id)
            .field("alive", &self.is_alive())
            .finish()
    }
}

static NEXT_SOCKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl VideoStream {
    /// Start the device's video server and read its stream. The jar is
    /// pushed if needed (the tree server's), the server started with
    /// `app_process` and reached through `adb forward`.
    pub async fn start(
        adb_path: &str,
        device_id: &str,
        opts: VideoOptions,
        keep: Duration,
    ) -> Result<Self, String> {
        let begun = Instant::now();
        let remote = crate::tree_server::push_server_jar(adb_path, device_id).await?;
        // Its own socket name per start: a server left by a killed run
        // (it exits within 15 s without a client) never collides.
        let socket = format!(
            "atalanta_video_{}_{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut child = atalanta_common::process::spawn_group(
            Command::new(adb_path)
                .args(crate::adb::target_args(device_id))
                .args([
                    "shell",
                    &format!(
                        "CLASSPATH={remote} exec app_process / com.atalanta.tree.VideoServer \
                         --max-size {} --bit-rate {} --max-fps {} --iframe {} --socket {socket}",
                        opts.max_size, opts.bit_rate, opts.max_fps, opts.key_frame_interval
                    ),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::null()),
        )
        .map_err(|e| format!("spawn adb: {e}"))?;
        let stderr_lines = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let keep_lines = stderr_lines.clone();
            let device = device_id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(device_id = %device, "video server: {line}");
                    let mut g = keep_lines.lock().unwrap_or_else(|e| e.into_inner());
                    if g.len() < 50 {
                        g.push(line);
                    }
                }
            });
        }
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(START_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("listening") {
                    // The jar is pushed by content hash, so this is always
                    // the embedded server; say so plainly if it isn't.
                    if !line.starts_with(&format!("atalanta-video {VIDEO_VERSION} ")) {
                        return Err(format!(
                            "video server speaks {line:?}, expected version {VIDEO_VERSION}"
                        ));
                    }
                    return Ok(());
                }
            }
            Err("video server exited before listening".to_string())
        })
        .await
        .map_err(|_| format!("video server did not start in time ({START_TIMEOUT:?})"))
        .and_then(|r| r);
        if let Err(why) = ready {
            let err = stderr_lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .join(" | ");
            return Err(if err.is_empty() {
                why
            } else {
                format!("{why}: {err}")
            });
        }
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let out = crate::tree_server::run_adb(
            adb_path,
            device_id,
            &["forward", "tcp:0", &format!("localabstract:{socket}")],
        )
        .await?;
        let port: u16 = out
            .trim()
            .parse()
            .map_err(|e| format!("unexpected forward output {out:?}: {e}"))?;
        // From here on the forward is ours to remove on every way out: a
        // connect that timed out or a stream header that never came used to
        // leave one more listening port in the adb server per failed start.
        let connected = async {
            // The server's token, from the file only shell or root can read.
            let token_file = format!("/data/local/tmp/{socket}.token");
            let token = crate::tree_server::run_adb(
                adb_path,
                device_id,
                &["shell", &format!("cat {token_file}")],
            )
            .await
            .map(|t| t.trim().to_string())
            .ok()
            .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| format!("no video server token in {token_file}"))?;
            let host = crate::adb::server_host(device_id);
            let mut stream = tokio::time::timeout(
                Duration::from_secs(3),
                TcpStream::connect((host.as_str(), port)),
            )
            .await
            .map_err(|_| format!("video connect to {host}:{port} timed out"))?
            .map_err(|e| format!("video connect: {e}"))?;
            let _ = stream.set_nodelay(true);
            authenticate(&mut stream, &token).await?;
            let (read, write) = stream.into_split();
            let mut read = BufReader::with_capacity(256 << 10, read);
            let codec = tokio::time::timeout(Duration::from_secs(5), read_codec(&mut read))
                .await
                .map_err(|_| {
                    let err = stderr_lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .join(" | ");
                    format!("no stream header from the video server in 5s {err}")
                })?
                .map_err(|e| {
                    let err = stderr_lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .join(" | ");
                    format!("video stream: {e} {err}")
                })?;
            if codec != CODEC_H264 {
                return Err(format!("unexpected video codec {codec:#x}"));
            }
            Ok::<_, String>((read, write))
        }
        .await;
        let (read, write) = match connected {
            Ok(rw) => rw,
            Err(e) => {
                let _ = crate::tree_server::run_adb(
                    adb_path,
                    device_id,
                    &["forward", "--remove", &format!("tcp:{port}")],
                )
                .await;
                return Err(e);
            }
        };
        let me = Self::from_parts(
            device_id,
            read,
            write,
            keep,
            Some(child),
            Some(port),
            adb_path,
        );
        info!(
            device_id,
            port,
            ms = begun.elapsed().as_millis() as u64,
            "android video stream started"
        );
        Ok(me)
    }

    /// A stream over an already-connected reader and writer (tests, and
    /// [`Self::start`]).
    pub fn from_parts<R, W>(
        device_id: &str,
        mut read: R,
        mut write: W,
        keep: Duration,
        child: Option<atalanta_common::process::GroupChild>,
        port: Option<u16>,
        adb_path: &str,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (control, mut control_rx) = mpsc::unbounded_channel::<u8>();
        let shared = Arc::new(Shared {
            ring: Mutex::new(Ring::new(keep)),
            alive: std::sync::atomic::AtomicBool::new(true),
            ended: Mutex::new(None),
            frames: std::sync::atomic::AtomicU64::new(0),
        });
        tokio::spawn(async move {
            while let Some(b) = control_rx.recv().await {
                if write.write_all(&[b]).await.is_err() || b == b'Q' {
                    break;
                }
                let _ = write.flush().await;
            }
        });
        let reader_shared = shared.clone();
        let reader_tx = tx.clone();
        let device = device_id.to_string();
        tokio::spawn(async move {
            let why = loop {
                match read_event(&mut read).await {
                    Ok(Some(ev)) => {
                        if matches!(ev, VideoEvent::Frame(_)) {
                            reader_shared
                                .frames
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        reader_shared
                            .ring
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(&ev);
                        // No subscriber is fine.
                        let _ = reader_tx.send(ev);
                    }
                    Ok(None) => break "the video server closed the stream".to_string(),
                    Err(e) => break format!("video stream: {e}"),
                }
            };
            debug!(device_id = %device, "{why}");
            *reader_shared
                .ended
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(why);
            reader_shared
                .alive
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
        Self {
            device_id: device_id.to_string(),
            tx,
            control,
            shared,
            started: Instant::now(),
            _child: child,
            port,
            adb_path: adb_path.to_string(),
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Whether the stream is still running.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Why the stream ended, once it has.
    pub fn ended(&self) -> Option<String> {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Frames received so far.
    pub fn frames_received(&self) -> u64 {
        self.shared
            .frames
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// When the stream started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// The video's current size.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.shared
            .ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .size
    }

    /// Ask the encoder for a key frame now (a new subscriber, a test start).
    pub fn request_key_frame(&self) {
        let _ = self.control.send(b'K');
    }

    /// Follow the stream live: the current size and parameter sets, then
    /// every event. Asks for a key frame so decoding can start at once.
    pub fn subscribe(&self) -> VideoSubscription {
        let events = self.tx.subscribe();
        let ring = self.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        let mut primer = Vec::new();
        if let Some((width, height)) = ring.size {
            primer.push(VideoEvent::Session { width, height });
        }
        if let Some(c) = &ring.config {
            primer.push(VideoEvent::Config(c.clone()));
        }
        drop(ring);
        self.request_key_frame();
        VideoSubscription { primer, events }
    }

    /// Keep at least `keep` of video in the ring buffer.
    pub fn keep_at_least(&self, keep: Duration) {
        let mut ring = self.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        if keep > ring.keep {
            ring.set_keep(keep);
        }
    }

    /// The ring buffer's frames from the key frame at or before `since`.
    pub fn clip(&self, since: Option<Instant>) -> Option<Clip> {
        self.shared
            .ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clip(since)
    }

    /// Stop the device server and remove the port forward.
    pub async fn stop(&self) {
        let _ = self.control.send(b'Q');
        if let Some(port) = self.port {
            let _ = crate::tree_server::run_adb(
                &self.adb_path,
                &self.device_id,
                &["forward", "--remove", &format!("tcp:{port}")],
            )
            .await;
        }
    }
}

/// `adb shell screenrecord`, the fallback when the device's encoder can't
/// be driven through the video server: one recording per test, in 180 s
/// segments (screenrecord's limit), pulled only when kept.
pub struct ScreenRecord {
    adb_path: String,
    device_id: String,
    tag: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// `None` once [`Self::finish`] took it.
    task: Option<tokio::task::JoinHandle<Vec<(String, Instant)>>>,
}

/// Dropped without [`ScreenRecord::finish`] (an aborted actor, a cut
/// finish budget), the detached loop used to keep starting 180 s
/// `screenrecord` segments until the process exited and left their mp4s
/// on the device (review 01 #5). Stop the loop (aborting it drops the
/// running `adb` child, which is `kill_on_drop`), and when a runtime is
/// still there, stop the device side and delete this recording's files.
impl Drop for ScreenRecord {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        task.abort();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (adb, dev, tag) = (
                self.adb_path.clone(),
                self.device_id.clone(),
                self.tag.clone(),
            );
            rt.spawn(async move {
                let _ = crate::tree_server::run_adb(
                    &adb,
                    &dev,
                    &[
                        "shell",
                        &format!("pkill -INT -f {tag}; sleep 1; rm -f /data/local/tmp/{tag}-*.mp4"),
                    ],
                )
                .await;
            });
        }
    }
}

impl ScreenRecord {
    pub fn start(adb_path: &str, device_id: &str, opts: VideoOptions) -> Self {
        let tag = format!(
            "atalanta-rec-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (adb, dev, t, s) = (
            adb_path.to_string(),
            device_id.to_string(),
            tag.clone(),
            stop.clone(),
        );
        let task = tokio::spawn(async move {
            let mut segments = Vec::new();
            let mut n = 0u32;
            while !s.load(std::sync::atomic::Ordering::SeqCst) {
                let file = format!("/data/local/tmp/{t}-{n}.mp4");
                let child = Command::new(&adb)
                    .args(crate::adb::target_args(&dev))
                    .args([
                        "shell",
                        &format!(
                            "exec screenrecord --bit-rate {} --time-limit 180 {file}",
                            opts.bit_rate
                        ),
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn();
                let Ok(mut child) = child else {
                    break;
                };
                segments.push((file, Instant::now()));
                let ran = Instant::now();
                let _ = child.wait().await;
                if ran.elapsed() < Duration::from_secs(2)
                    && !s.load(std::sync::atomic::Ordering::SeqCst)
                {
                    warn!(device_id = %dev, "adb screenrecord exited at once; no video for this test");
                    break;
                }
                n += 1;
            }
            segments
        });
        Self {
            adb_path: adb_path.to_string(),
            device_id: device_id.to_string(),
            tag,
            stop,
            task: Some(task),
        }
    }

    /// Stop recording; with `keep`, pull the segments to `dest` (the first
    /// as `dest`, later ones as `<stem>-2.mp4`, …). Returns the files and
    /// when the first segment started.
    pub async fn finish(
        mut self,
        keep: Option<&std::path::Path>,
    ) -> (Vec<std::path::PathBuf>, Option<Instant>) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // SIGINT makes screenrecord finish the file.
        let _ = crate::tree_server::run_adb(
            &self.adb_path,
            &self.device_id,
            &["shell", &format!("pkill -INT -f {}", self.tag)],
        )
        .await;
        // Taken only now: a finish cut short before this (its caller's
        // budget) still leaves Drop to stop the loop and clean the device.
        let segments = match self.task.take() {
            Some(task) => match tokio::time::timeout(Duration::from_secs(10), task).await {
                Ok(Ok(s)) => s,
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let mut out = Vec::new();
        let first = segments.first().map(|(_, t)| *t);
        if let Some(dest) = keep {
            for (i, (remote, _)) in segments.iter().enumerate() {
                let local = if i == 0 {
                    dest.to_path_buf()
                } else {
                    let stem = dest
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    dest.with_file_name(format!("{stem}-{}.mp4", i + 1))
                };
                if let Some(dir) = local.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match crate::tree_server::run_adb(
                    &self.adb_path,
                    &self.device_id,
                    &["pull", remote, &local.to_string_lossy()],
                )
                .await
                {
                    Ok(_) => out.push(local),
                    Err(e) => warn!("screenrecord segment not pulled: {e}"),
                }
            }
        }
        let rm: Vec<String> = segments.into_iter().map(|(r, _)| r).collect();
        if !rm.is_empty() {
            let _ = crate::tree_server::run_adb(
                &self.adb_path,
                &self.device_id,
                &["shell", &format!("rm -f {}", rm.join(" "))],
            )
            .await;
        }
        (out, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start that fails after `adb forward` (nothing listens on the
    /// forwarded port) removes that forward again, instead of leaving one
    /// more listening port in the adb server per failed start.
    #[tokio::test]
    async fn a_failed_start_removes_its_forward() {
        let fake = crate::test_support::FakeAdb::new("video-forward-leak");
        // A port nothing listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        fake.stub(
            "app_process",
            "echo 'atalanta-video 2 listening on @test'; sleep 30",
        );
        fake.stub(
            "adb-forward",
            &format!("case \"$*\" in *tcp:0*) echo {port};; esac; exit 0"),
        );
        fake.stub(
            "cat",
            "case \"$1\" in /data/local/tmp/atalanta_video_*.token) echo 0123456789abcdef0123456789abcdef;; *) exec /bin/cat \"$@\";; esac",
        );
        let started = VideoStream::start(
            &fake.path(),
            "emulator-5554",
            VideoOptions::default(),
            Duration::from_secs(5),
        )
        .await;
        let e = started
            .map(|_| ())
            .expect_err("nothing listens on the forwarded port");
        assert!(e.contains("video connect"), "{e}");
        let log = fake.log();
        assert!(
            log.contains(&format!("[forward][--remove][tcp:{port}]")),
            "the forward was left behind:\n{log}"
        );
    }

    fn frame(at: Instant, key: bool, pts: u64) -> VideoEvent {
        VideoEvent::Frame(VideoFrame {
            pts_us: pts,
            key,
            data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, 1].into(),
            received: at,
        })
    }

    fn header(word: u64, size: u32) -> Vec<u8> {
        let mut v = word.to_be_bytes().to_vec();
        v.extend_from_slice(&size.to_be_bytes());
        v
    }

    #[tokio::test]
    async fn parses_the_wire_format() {
        let mut wire = CODEC_H264.to_be_bytes().to_vec();
        // session 584x1280
        wire.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        wire.extend_from_slice(&584u32.to_be_bytes());
        wire.extend_from_slice(&1280u32.to_be_bytes());
        wire.extend(header(FLAG_CONFIG, 3));
        wire.extend_from_slice(&[7, 8, 9]);
        wire.extend(header(FLAG_KEY_FRAME | 123_456, 2));
        wire.extend_from_slice(&[0x65, 1]);
        wire.extend(header(200_000, 1));
        wire.push(0x41);
        let mut r = &wire[..];
        assert_eq!(read_codec(&mut r).await.unwrap(), CODEC_H264);
        assert!(matches!(
            read_event(&mut r).await.unwrap(),
            Some(VideoEvent::Session {
                width: 584,
                height: 1280
            })
        ));
        assert!(
            matches!(read_event(&mut r).await.unwrap(), Some(VideoEvent::Config(c)) if *c == [7, 8, 9])
        );
        match read_event(&mut r).await.unwrap() {
            Some(VideoEvent::Frame(f)) => assert!(f.key && f.pts_us == 123_456),
            other => panic!("{other:?}"),
        }
        match read_event(&mut r).await.unwrap() {
            Some(VideoEvent::Frame(f)) => assert!(!f.key && f.pts_us == 200_000),
            other => panic!("{other:?}"),
        }
        assert!(read_event(&mut r).await.unwrap().is_none(), "clean end");
    }

    /// Security review #10: the stream only after both sides proved they
    /// hold the token; an impostor's proof is refused before AUTH is sent.
    #[tokio::test]
    async fn the_video_handshake_is_mutual() {
        use crate::tree_server::{hex, hmac_sha256};
        let token = "0123456789abcdef0123456789abcdef";
        let mac = |label: &str, cn: &str, sn: &str| {
            hex(&hmac_sha256(
                token.as_bytes(),
                format!("atalanta-video {label} {cn} {sn}").as_bytes(),
            ))
        };
        // A server that knows the token (as VideoServer.authenticated).
        let (mut host, mut dev) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move {
            let hello = read_line(&mut dev).await.unwrap();
            let cn = hello.strip_prefix("HELLO ").unwrap().to_string();
            let sn = "ffeeddccbbaa99887766554433221100";
            dev.write_all(format!("PROOF {sn} {}\n", mac("server", &cn, sn)).as_bytes())
                .await
                .unwrap();
            let auth = read_line(&mut dev).await.unwrap();
            assert_eq!(auth, format!("AUTH {}", mac("client", &cn, sn)));
            dev.write_all(b"OK\n").await.unwrap();
            dev.write_u32(CODEC_H264).await.unwrap();
        });
        authenticate(&mut host, token).await.unwrap();
        // Nothing of the stream was read with the handshake.
        assert_eq!(read_codec(&mut host).await.unwrap(), CODEC_H264);
        server.await.unwrap();

        // An impostor without the token gets no AUTH.
        let (mut host, mut dev) = tokio::io::duplex(1024);
        let impostor = tokio::spawn(async move {
            let _ = read_line(&mut dev).await.unwrap();
            dev.write_all(format!("PROOF ab {}\n", "0".repeat(64)).as_bytes())
                .await
                .unwrap();
            let mut rest = Vec::new();
            let _ = dev.read_to_end(&mut rest).await;
            rest
        });
        let e = authenticate(&mut host, token).await.unwrap_err();
        assert!(e.contains("could not prove"), "{e}");
        drop(host);
        assert!(impostor.await.unwrap().is_empty(), "no AUTH sent");
    }

    #[tokio::test]
    async fn refuses_a_huge_packet() {
        let wire = header(0, u32::MAX);
        let mut r = &wire[..];
        assert!(read_event(&mut r).await.is_err());
    }

    #[test]
    fn ring_keeps_whole_groups_from_a_key_frame() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut ring = Ring::new(Duration::from_secs(2));
        ring.push(&VideoEvent::Session {
            width: 8,
            height: 8,
        });
        ring.push(&VideoEvent::Config(vec![1].into()));
        // A P-frame before any key frame can't be decoded: dropped.
        ring.push(&frame(at(0), false, 0));
        assert!(ring.is_empty());
        // Key frames each second, P-frames between.
        for s in 0..6u64 {
            ring.push(&frame(at(s * 1000), true, s * 1_000_000));
            ring.push(&frame(at(s * 1000 + 500), false, s * 1_000_000 + 500_000));
        }
        // Now = 5.5 s, keep 2 s: cutoff 3.5 s; the newest key frame at or
        // before it is at 3 s, so 3 s..5.5 s stays (>= 2 s, from a key).
        let clip = ring.clip(None).unwrap();
        assert!(clip.frames[0].key);
        assert_eq!(clip.frames[0].pts_us, 3_000_000);
        assert_eq!(clip.frames.len(), 6);
        // A test that started at 4.2 s: its clip starts at the 4 s key frame.
        let clip = ring.clip(Some(at(4200))).unwrap();
        assert_eq!(clip.frames[0].pts_us, 4_000_000);
        assert!(clip.frames[0].key);
        // Started before anything kept: from the oldest key frame.
        assert_eq!(
            ring.clip(Some(at(100))).unwrap().frames[0].pts_us,
            3_000_000
        );
        assert_eq!(ring.bytes(), 6 * 6);
    }

    #[test]
    fn a_new_size_starts_over() {
        let t0 = Instant::now();
        let mut ring = Ring::new(Duration::from_secs(30));
        ring.push(&VideoEvent::Session {
            width: 8,
            height: 16,
        });
        ring.push(&VideoEvent::Config(vec![1].into()));
        ring.push(&frame(t0, true, 0));
        ring.push(&VideoEvent::Session {
            width: 16,
            height: 8,
        });
        assert!(
            ring.clip(None).is_none(),
            "no config or frames for the new size yet"
        );
        ring.push(&VideoEvent::Config(vec![2].into()));
        ring.push(&frame(t0, true, 10));
        let clip = ring.clip(None).unwrap();
        assert_eq!((clip.width, clip.height, clip.frames.len()), (16, 8, 1));
    }

    #[tokio::test]
    async fn subscribers_get_a_primer_and_live_events() {
        let (dev_side, host_side) = tokio::io::duplex(1 << 16);
        let (host_read, host_write) = tokio::io::split(host_side);
        let (mut dev_read, mut dev_write) = tokio::io::split(dev_side);
        let stream = VideoStream::from_parts(
            "emulator-5554",
            host_read,
            host_write,
            Duration::from_secs(30),
            None,
            None,
            "adb",
        );
        // Session + config first, then subscribe: the primer carries them.
        let mut wire = 0x8000_0000u32.to_be_bytes().to_vec();
        wire.extend_from_slice(&8u32.to_be_bytes());
        wire.extend_from_slice(&8u32.to_be_bytes());
        wire.extend(header(FLAG_CONFIG, 1));
        wire.push(7);
        dev_write.write_all(&wire).await.unwrap();
        for _ in 0..100 {
            if stream.size().is_some() && stream.subscribe().primer.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut sub = stream.subscribe();
        assert_eq!(sub.primer.len(), 2);
        // Subscribing asked the device for a key frame.
        let mut asked = [0u8; 1];
        dev_read.read_exact(&mut asked).await.unwrap();
        assert_eq!(&asked, b"K");
        let mut f = header(FLAG_KEY_FRAME | 5, 2);
        f.extend_from_slice(&[0x65, 1]);
        dev_write.write_all(&f).await.unwrap();
        match tokio::time::timeout(Duration::from_secs(2), sub.events.recv()).await {
            Ok(Ok(VideoEvent::Frame(fr))) => assert!(fr.key && fr.pts_us == 5),
            other => panic!("{other:?}"),
        }
        assert!(stream.clip(None).is_some());
        // The device side goes away (both halves of the pipe).
        drop(dev_write);
        drop(dev_read);
        for _ in 0..100 {
            if !stream.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!stream.is_alive());
        assert!(stream.ended().is_some());
    }

    /// The muxed clip of a fake stream parses back.
    #[test]
    fn a_clip_muxes() {
        let t0 = Instant::now();
        let mut ring = Ring::new(Duration::from_secs(30));
        ring.push(&VideoEvent::Session {
            width: 584,
            height: 1280,
        });
        ring.push(&VideoEvent::Config(
            vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x29, 0, 0, 0, 1, 0x68, 0xce].into(),
        ));
        ring.push(&frame(t0, true, 0));
        ring.push(&frame(t0 + Duration::from_millis(33), false, 33_000));
        let mp4 = ring.clip(None).unwrap().to_mp4().unwrap();
        let p = crate::mp4::parse(&mp4).unwrap();
        assert_eq!(p.sample_sizes.len(), 2);
        assert_eq!((p.width, p.height), (584, 1280));
    }

    /// Runs against a real device when one is named in ATALANTA_TEST_ANDROID:
    /// the stream starts, a clip muxes, and (if ffprobe is installed) the
    /// file probes as H.264 of the stream's size.
    #[tokio::test]
    async fn records_from_a_real_device() {
        let Some(device) = crate::test_support::device_under_test() else {
            return;
        };
        let adb = atalanta_common::env::var("ATALANTA_ADB_PATH").unwrap_or_else(|_| "adb".into());
        let begun = Instant::now();
        let stream = VideoStream::start(
            &adb,
            &device,
            VideoOptions::default(),
            Duration::from_secs(30),
        )
        .await
        .expect("video stream");
        let started = begun.elapsed();
        let mut sub = stream.subscribe();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let mut live = 0;
        while let Ok(ev) = sub.events.try_recv() {
            if matches!(ev, VideoEvent::Frame(_)) {
                live += 1;
            }
        }
        assert!(live > 0, "no live frames");
        let clip = stream.clip(None).expect("a clip");
        let (w, h) = (clip.width, clip.height);
        assert!(w.max(h) <= 1280 && w % 8 == 0 && h % 8 == 0, "{w}x{h}");
        let mp4 = clip.to_mp4().unwrap();
        crate::mp4::parse(&mp4).unwrap();
        let path = std::env::temp_dir().join(format!("atalanta-video-{}.mp4", std::process::id()));
        std::fs::write(&path, &mp4).unwrap();
        eprintln!(
            "started in {started:?}; {} frames, {} bytes -> {}",
            clip.frames.len(),
            mp4.len(),
            path.display()
        );
        if let Ok(out) = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,width,height",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                text.trim(),
                format!("h264,{w},{h}"),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        stream.stop().await;
        let _ = std::fs::remove_file(&path);
    }
}
