//! Who drives a device from the console.
//!
//! Watching a device (the mirror, the tree) needs nothing: it doesn't
//! change the device. Driving it (input on the mirror, the toolbar's tap,
//! type, swipe and key) takes the device's cross-process lease, the same
//! `flock` `velocity run` and the MCP server take, and holds it until it is
//! released or the inspector exits. While another process holds it, input
//! is refused with 409 and the holder (`busy: pid=… cmd=run`), and the page
//! says so. A console run of a suite releases the console's lease first:
//! its `velocity run` child takes it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use velocity_common::types::{Element, Key, Rect};
use velocity_common::Platform;

use crate::state::AppState;

/// Why a lease wasn't taken.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseFail {
    /// Another process holds it: who.
    Held(String),
    /// The lease file can't be used.
    Io(String),
}

/// Cross-process device leases (`velocity_runner::lease` in the CLI).
pub trait Leases: Send + Sync {
    /// Take `device_id`'s lease without waiting; the guard releases it.
    fn try_lock(&self, device_id: &str) -> Result<Box<dyn std::any::Any + Send + Sync>, LeaseFail>;
}

/// No cross-process leasing (tests, `VELOCITY_LEASES=off`).
pub struct NoLeases;
impl Leases for NoLeases {
    fn try_lock(&self, _: &str) -> Result<Box<dyn std::any::Any + Send + Sync>, LeaseFail> {
        Ok(Box::new(()))
    }
}

struct Held {
    _guard: Box<dyn std::any::Any + Send + Sync>,
    since: Instant,
}

/// The leases this console holds, and devices a console task is using.
#[derive(Default)]
pub struct Control {
    held: Mutex<HashMap<String, Held>>,
    /// Device → the console task running on it.
    tasks: std::sync::Mutex<HashMap<String, u64>>,
}

pub type ApiError = (StatusCode, String);

impl Control {
    /// A console task runs on `device_id` now (`None`: it finished).
    pub fn set_task(&self, device_id: &str, task: Option<u64>) {
        let mut t = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        match task {
            Some(id) => t.insert(device_id.to_string(), id),
            None => t.remove(device_id),
        };
    }

    pub fn task_on(&self, device_id: &str) -> Option<u64> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
    }

    pub async fn holds(&self, device_id: &str) -> bool {
        self.held.lock().await.contains_key(device_id)
    }

    /// Let go of every lease this console holds (it is shutting down).
    pub async fn release_all(&self) {
        self.held.lock().await.clear();
    }

    pub async fn release(&self, device_id: &str) -> bool {
        self.held.lock().await.remove(device_id).is_some()
    }

    /// Take the lease (or keep the one held) so this console may drive
    /// `device_id`. 409 with the holder when it is busy.
    pub async fn ensure(&self, leases: &dyn Leases, device_id: &str) -> Result<(), ApiError> {
        if let Some(id) = self.task_on(device_id) {
            return Err((
                StatusCode::CONFLICT,
                format!("busy: console task #{id} is running on {device_id}"),
            ));
        }
        let mut held = self.held.lock().await;
        if held.contains_key(device_id) {
            return Ok(());
        }
        // Web "devices" are this process's own browser: nothing to share.
        if Platform::of_device_id(device_id) == Platform::Web {
            held.insert(
                device_id.to_string(),
                Held {
                    _guard: Box::new(()),
                    since: Instant::now(),
                },
            );
            return Ok(());
        }
        match leases.try_lock(device_id) {
            Ok(guard) => {
                held.insert(
                    device_id.to_string(),
                    Held {
                        _guard: guard,
                        since: Instant::now(),
                    },
                );
                Ok(())
            }
            Err(LeaseFail::Held(who)) => Err((StatusCode::CONFLICT, format!("busy: {who}"))),
            Err(LeaseFail::Io(e)) => Err((StatusCode::SERVICE_UNAVAILABLE, e)),
        }
    }

    /// Whether another process holds the lease now: takes and drops it at
    /// once when this console doesn't hold it.
    pub async fn probe(&self, leases: &dyn Leases, device_id: &str) -> Option<String> {
        if self.holds(device_id).await || Platform::of_device_id(device_id) == Platform::Web {
            return None;
        }
        match leases.try_lock(device_id) {
            Ok(_) => None,
            Err(LeaseFail::Held(who)) => Some(who),
            Err(LeaseFail::Io(e)) => Some(e),
        }
    }
}

/// `GET /api/devices/{id}/control`: who drives it, and the size taps use.
pub async fn status(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Json<serde_json::Value> {
    let c = &state.control;
    let mine = c.holds(&device_id).await;
    let since = if mine {
        c.held
            .lock()
            .await
            .get(&device_id)
            .map(|h| h.since.elapsed().as_secs())
    } else {
        None
    };
    let task = c.task_on(&device_id);
    let busy = match task {
        Some(id) => Some(format!("console task #{id}")),
        None => c.probe(state.console.leases.as_ref(), &device_id).await,
    };
    // Only a device this console prepared (or the one it started with):
    // asking another for its size would start its services (WDA), and the
    // device list asks for every device's status.
    let prepared = state.prepared.lock().await.contains(&device_id)
        || state.device_id().await.as_deref() == Some(device_id.as_str());
    let screen = if prepared {
        state.driver.screen_size(&device_id).await.ok()
    } else {
        None
    };
    Json(json!({
        "deviceId": device_id,
        "mine": mine,
        "heldSecs": since,
        "busy": busy,
        "task": task,
        "screen": screen.map(|(w, h)| json!({"width": w, "height": h})),
    }))
}

/// `POST /api/devices/{id}/control`: take the lease.
pub async fn take(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .control
        .ensure(state.console.leases.as_ref(), &device_id)
        .await?;
    Ok(Json(json!({"ok": true, "mine": true})))
}

/// `DELETE /api/devices/{id}/control`: let other processes have it.
pub async fn release(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Json<serde_json::Value> {
    let was = state.control.release(&device_id).await;
    Json(json!({"ok": true, "released": was}))
}

/// Input on the mirror, in the coordinates taps use (device pixels on
/// Android, points on iOS, CSS pixels on the web).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Input {
    Tap {
        x: i32,
        y: i32,
    },
    LongPress {
        x: i32,
        y: i32,
        #[serde(default)]
        ms: Option<u64>,
    },
    Swipe {
        from: (i32, i32),
        to: (i32, i32),
    },
    /// A wheel: the finger moves against the scroll (`dy` > 0 scrolls the
    /// content down, so the finger goes up).
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Key {
        key: Key,
    },
    Text {
        text: String,
    },
    /// Web only: load this http(s) URL.
    Navigate {
        url: String,
    },
}

/// The longest text one input may type.
const MAX_TEXT: usize = 4096;

fn point(x: i32, y: i32) -> Element {
    Element {
        bounds: Rect {
            x,
            y,
            width: 1,
            height: 1,
        },
        enabled: true,
        visible: true,
        ..Default::default()
    }
}

/// `POST /api/devices/{id}/input`.
pub async fn input(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Json(input): Json<Input>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .control
        .ensure(state.console.leases.as_ref(), &device_id)
        .await?;
    state.poke();
    let d = &state.driver;
    let id = device_id.as_str();
    let r = match input {
        Input::Tap { x, y } => match d.tap_xy(id, x, y).await {
            Err(velocity_common::VelocityError::Config(_)) => d.tap(id, &point(x, y)).await,
            other => other,
        },
        Input::LongPress { x, y, ms } => {
            d.long_press(id, &point(x, y), ms.unwrap_or(800).min(10_000))
                .await
        }
        Input::Swipe { from, to } => d.swipe_coords(id, from, to).await,
        Input::Scroll { x, y, dx, dy } => {
            // The finger moves against the content.
            d.scroll_coords(id, (x, y), (x.saturating_sub(dx), y.saturating_sub(dy)))
                .await
        }
        Input::Key { key } => d.press_key(id, key).await,
        Input::Text { text } => {
            if text.len() > MAX_TEXT {
                return Err((StatusCode::BAD_REQUEST, "text too long".into()));
            }
            d.input_raw(id, &text).await
        }
        Input::Navigate { url } => {
            let lower = url.to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://")) || url.len() > 4096
            {
                return Err((StatusCode::BAD_REQUEST, "an http(s) URL".into()));
            }
            d.navigate(id, &url).await
        }
    };
    r.map_err(crate::routes::api_error)?;
    state.poke();
    Ok(Json(json!({"ok": true})))
}
