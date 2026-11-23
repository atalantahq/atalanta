// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::time::{sleep_until, Instant};

use crate::snapshot;
use crate::state::AppState;

#[derive(serde::Deserialize, Default)]
pub struct LiveQuery {
    /// `0`: the page shows the live mirror, so screenshots aren't polled
    /// here (only the hierarchy and errors).
    #[serde(default)]
    pub screenshots: Option<String>,
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Query(q): Query<LiveQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let shots = q.screenshots.as_deref() != Some("0");
    ws.on_upgrade(move |socket| handle_socket(socket, state, device_id, shots))
}

/// The live view's polling period while things change.
const FAST: Duration = Duration::from_secs(1);
/// The longest period it backs off to while nothing changes.
const SLOWEST: Duration = Duration::from_secs(4);
/// Unchanged polls before backing off.
const IDLE_POLLS: u32 = 3;

/// What this socket last sent, so it only sends what changed.
#[doc(hidden)]
#[derive(Default)]
pub struct Sent {
    /// Don't take screenshots (the mirror shows the screen).
    pub no_screenshots: bool,
    tree: Option<u64>,
    screenshot: Option<u64>,
    error: Option<String>,
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    device_id: String,
    shots: bool,
) {
    let mut sent = Sent {
        no_screenshots: !shots,
        ..Sent::default()
    };
    let mut period = FAST;
    let mut idle = 0u32;
    let mut next = Instant::now();

    loop {
        tokio::select! {
            _ = sleep_until(next) => {
                let (messages, changed) = poll(&state, &device_id, &mut sent).await;
                let mut gone = false;
                for msg in messages {
                    if !send(&mut socket, msg).await {
                        gone = true;
                        break;
                    }
                }
                if gone {
                    break;
                }
                // Back off while nothing changes (research P-5): 1 s doubling to 4 s.
                if changed {
                    idle = 0;
                    period = FAST;
                } else {
                    idle += 1;
                    if idle >= IDLE_POLLS {
                        period = (period * 2).min(SLOWEST);
                    }
                }
                next = Instant::now() + period;
            }

            _ = state.activity.notified() => {
                // An action ran: poll at full speed again, starting now.
                idle = 0;
                period = FAST;
                next = Instant::now();
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Handle client messages (e.g., refresh requests)
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                            if parsed.get("type").and_then(|t| t.as_str()) == Some("refresh") {
                                // Resend everything now, not a period from now.
                                sent = Sent {
                                    no_screenshots: !shots,
                                    ..Sent::default()
                                };
                                idle = 0;
                                period = FAST;
                                next = Instant::now();
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

async fn send(socket: &mut WebSocket, msg: serde_json::Value) -> bool {
    socket
        .send(Message::Text(msg.to_string().into()))
        .await
        .is_ok()
}

/// One poll: the messages to send (the screenshot URL and the hierarchy if
/// they changed, an error if it's a new one, and the performance sample) and
/// whether the screen changed. `sent` is updated as though they were sent.
#[doc(hidden)]
pub async fn poll(
    state: &AppState,
    device_id: &str,
    sent: &mut Sent,
) -> (Vec<serde_json::Value>, bool) {
    let mut out = Vec::new();
    let mut changed = false;
    let mut error = None;

    let shot = if sent.no_screenshots {
        Err(None)
    } else {
        state.driver.screenshot(device_id).await.map_err(Some)
    };
    match shot {
        Err(None) => {}
        Ok(png) => {
            let hash = snapshot::bytes_hash(&png);
            if sent.screenshot != Some(hash) {
                state
                    .screenshots
                    .write()
                    .await
                    .insert(device_id.to_string(), (hash, Arc::new(png)));
                let msg = serde_json::json!({
                    "type": "screenshot",
                    "url": format!(
                        "/api/devices/{}/screenshot?v={hash:x}",
                        encode_path_segment(device_id)
                    ),
                });
                out.push(msg);
                sent.screenshot = Some(hash);
                changed = true;
            }
        }
        Err(Some(e)) => error = Some(e.to_string()),
    }

    match state.driver.get_hierarchy(device_id).await {
        Ok(tree) => {
            let hash = snapshot::tree_hash(&tree);
            if sent.tree != Some(hash) {
                let snap = state.publish_snapshot(device_id, tree, hash).await;
                out.push(snap.message());
                sent.tree = Some(hash);
                changed = true;
            }
        }
        Err(e) => error = error.or(Some(e.to_string())),
    }

    // The page shows one error and clears it on the next good poll (VEL-260):
    // send each distinct error once, and `ok` once it has gone.
    if error != sent.error {
        let msg = match &error {
            Some(message) => serde_json::json!({ "type": "error", "message": message }),
            None => serde_json::json!({ "type": "ok" }),
        };
        out.push(msg);
        sent.error = error;
    }

    // Push performance metrics if app_id is set (opt-in, no overhead when None)
    if let Some(app_id) = state.app_id().await {
        if let Ok((java, native, pss, cpu)) = state
            .driver
            .collect_resource_metrics(device_id, &app_id)
            .await
        {
            let perf_msg = serde_json::json!({
                "type": "performance",
                "javaHeapKb": java,
                "nativeHeapKb": native,
                "totalPssKb": pss,
                "cpuPercent": cpu,
            });
            out.push(perf_msg);
        }
    }

    (out, changed)
}

/// Percent-encode a device id for use as one URL path segment (VEL-S25).
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
