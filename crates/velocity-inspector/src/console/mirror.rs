//! `GET /api/mirror/{device}` (WebSocket): the live screen.
//!
//! Text messages are JSON:
//! - `{"type":"hello","mode":"h264"|"frames","source":…}` first, and again
//!   when the mirror falls back from H.264 to frames;
//! - `{"type":"session","width":…,"height":…}` when the picture's size
//!   changes (a rotation): the page resizes its canvas;
//! - `{"type":"config","codec":"avc1.42e01f"}` before each SPS/PPS;
//! - `{"type":"fallback","reason":…}` when H.264 is not (or no longer)
//!   available; frames follow on the same socket;
//! - `{"type":"lagged"}`: frames were dropped, decoding resumes at the next
//!   key frame.
//!
//! Binary messages are frames with an 18-byte header
//! ([`super::live::frame_message`]): kind (1 SPS/PPS, 2 H.264 access
//! unit, 3 JPEG, 4 PNG), flags (bit 0: key frame), the encoder's pts in µs
//! and the time the host received it (Unix µs; 0 for the frames replayed
//! from the ring buffer to a new viewer), then the payload (Annex-B
//! for H.264, which WebCodecs decodes when no `description` is given).
//!
//! The page sends `{"type":"key"}` (its decoder needs a key frame),
//! `{"type":"pause"}` (tab hidden: nothing is sent) and `{"type":"resume"}`
//! (a key frame is asked for). `?mode=frames` skips H.264 (no WebCodecs).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use velocity_android::video::{VideoEvent, VideoStream};

use super::live::{
    avc_codec, frame_message, unix_us, FrameHub, ImageFrame, KIND_CONFIG, KIND_H264, KIND_JPEG,
    KIND_PNG,
};
use crate::state::AppState;

#[derive(Deserialize, Default)]
pub struct MirrorQuery {
    #[serde(default)]
    pub mode: Option<String>,
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Query(q): Query<MirrorQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let want_h264 = q.mode.as_deref() != Some("frames");
    // The page only ever sends small control messages.
    ws.max_message_size(64 << 10)
        .on_upgrade(move |socket| handle(socket, state, device_id, want_h264))
}

pub enum End {
    ClientGone,
    StreamEnded(String),
}

async fn text(socket: &mut WebSocket, v: serde_json::Value) -> bool {
    socket
        .send(Message::Text(v.to_string().into()))
        .await
        .is_ok()
}

async fn handle(mut socket: WebSocket, state: Arc<AppState>, device_id: String, want_h264: bool) {
    if want_h264 {
        let fallback = match state.console.live.h264(&device_id).await {
            Some(Ok(stream)) => match run_h264(&mut socket, &stream).await {
                End::ClientGone => return,
                End::StreamEnded(why) => Some(why),
            },
            Some(Err(why)) => Some(why),
            None => None,
        };
        if let Some(why) = fallback {
            if !text(&mut socket, json!({"type": "fallback", "reason": why})).await {
                return;
            }
        }
    }
    let hub = state.hubs.get(&state, &device_id).await;
    run_frames(&mut socket, &hub).await;
}

/// A client control message: `key`, `pause`, `resume`.
fn control(msg: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(msg)
        .ok()?
        .get("type")?
        .as_str()
        .map(str::to_string)
}

/// Forward the H.264 stream until the client goes or the stream ends.
pub async fn run_h264(socket: &mut WebSocket, stream: &VideoStream) -> End {
    let sub = stream.subscribe();
    let mut events = sub.events;
    if !text(
        socket,
        json!({"type": "hello", "mode": "h264", "source": "android-video"}),
    )
    .await
    {
        return End::ClientGone;
    }
    let mut need_key = true;
    let mut paused = false;
    let mut alive = tokio::time::interval(Duration::from_secs(1));
    let mut pending: std::collections::VecDeque<VideoEvent> = sub.primer.into();
    // The picture on screen now: the stream's last group of pictures. A
    // still screen sends no new frame (a software encoder may not repeat
    // one), so a new viewer would wait for the screen to change.
    let mut seen_pts: Option<u64> = None;
    if pending.len() == 2 {
        if let Some(clip) = stream.clip(Some(std::time::Instant::now())) {
            seen_pts = clip.frames.last().map(|f| f.pts_us);
            pending.extend(clip.frames.into_iter().map(VideoEvent::Frame));
        }
    }
    loop {
        let from_ring = !pending.is_empty();
        let ev = if let Some(ev) = pending.pop_front() {
            Ok(ev)
        } else {
            tokio::select! {
                ev = events.recv() => ev,
                _ = alive.tick() => {
                    if !stream.is_alive() {
                        return End::StreamEnded(stream.ended().unwrap_or_else(|| "the video stream stopped".into()));
                    }
                    continue;
                }
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => match control(&t).as_deref() {
                            Some("key") => { need_key = true; stream.request_key_frame(); }
                            Some("pause") => paused = true,
                            Some("resume") => { paused = false; need_key = true; stream.request_key_frame(); }
                            _ => {}
                        },
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return End::ClientGone,
                        _ => {}
                    }
                    continue;
                }
            }
        };
        let ok = match ev {
            Ok(VideoEvent::Session { width, height }) => {
                need_key = true;
                // A new encoder session starts its clock again.
                seen_pts = None;
                text(
                    socket,
                    json!({"type": "session", "width": width, "height": height}),
                )
                .await
            }
            Ok(VideoEvent::Config(c)) => {
                text(socket, json!({"type": "config", "codec": avc_codec(&c)})).await
                    && socket
                        .send(Message::Binary(
                            frame_message(KIND_CONFIG, false, 0, unix_us(), &c).into(),
                        ))
                        .await
                        .is_ok()
            }
            Ok(VideoEvent::Frame(f)) => {
                if paused || (need_key && !f.key) {
                    continue;
                }
                // Already sent from the ring buffer.
                if !from_ring && seen_pts.is_some_and(|p| f.pts_us <= p) {
                    continue;
                }
                need_key = false;
                // Replayed frames are old: 0 keeps them out of the page's
                // latency figures.
                let received = if from_ring {
                    0
                } else {
                    unix_us().saturating_sub(f.received.elapsed().as_micros() as u64)
                };
                socket
                    .send(Message::Binary(
                        frame_message(KIND_H264, f.key, f.pts_us, received, &f.data).into(),
                    ))
                    .await
                    .is_ok()
            }
            Err(RecvError::Lagged(_)) => {
                // Too slow for the stream: skip to the next key frame.
                need_key = true;
                stream.request_key_frame();
                text(socket, json!({"type": "lagged"})).await
            }
            Err(RecvError::Closed) => {
                return End::StreamEnded(
                    stream
                        .ended()
                        .unwrap_or_else(|| "the video stream ended".into()),
                )
            }
        };
        if !ok {
            return End::ClientGone;
        }
    }
}

async fn send_image(socket: &mut WebSocket, f: &ImageFrame, size: &mut (u32, u32)) -> bool {
    if f.width > 0 && (f.width, f.height) != *size {
        *size = (f.width, f.height);
        if !text(
            socket,
            json!({"type": "session", "width": f.width, "height": f.height}),
        )
        .await
        {
            return false;
        }
    }
    let kind = if f.png { KIND_PNG } else { KIND_JPEG };
    socket
        .send(Message::Binary(
            frame_message(kind, true, 0, f.received_us, &f.data).into(),
        ))
        .await
        .is_ok()
}

/// Pushed or polled frames until the client goes.
pub async fn run_frames(socket: &mut WebSocket, hub: &FrameHub) {
    let mut rx = hub.tx.subscribe();
    if !text(
        socket,
        json!({"type": "hello", "mode": "frames", "source": hub.source()}),
    )
    .await
    {
        return;
    }
    let mut size = (0, 0);
    let mut paused = false;
    if let Some(f) = hub.latest() {
        if !send_image(socket, &f, &mut size).await {
            return;
        }
    }
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(f) => {
                    if !paused && !send_image(socket, &f, &mut size).await {
                        return;
                    }
                }
                // Stills stand alone: the next one is all we need.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => match control(&t).as_deref() {
                    Some("pause") => paused = true,
                    Some("resume") | Some("key") => {
                        paused = false;
                        if let Some(f) = hub.latest() {
                            if !send_image(socket, &f, &mut size).await {
                                return;
                            }
                        }
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            }
        }
    }
}
