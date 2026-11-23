use std::sync::Arc;

use atalanta_common::types::DeviceInfo;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::state::AppState;

pub async fn list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DeviceInfo>>, (StatusCode, String)> {
    let devices = state
        .driver
        .list_devices()
        .await
        .map_err(super::api_error)?;
    Ok(Json(devices))
}

#[derive(Deserialize)]
pub struct SelectRequest {
    pub device_id: String,
}

pub async fn select(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SelectRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Verify device exists
    let devices = state
        .driver
        .list_devices()
        .await
        .map_err(super::api_error)?;

    let device = devices
        .iter()
        .find(|d| d.id == req.device_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Device '{}' not found", req.device_id),
            )
        })?;

    let info = serde_json::to_value(device)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Bootstrap the device's services once (WDA on iOS, the browser on
    // the web), as `atalanta inspect` does for the device it starts with.
    let mut prepared = state.prepared.lock().await;
    if !prepared.contains(&req.device_id) {
        state
            .driver
            .prepare(&req.device_id)
            .await
            .map_err(super::api_error)?;
        prepared.insert(req.device_id.clone());
    }
    drop(prepared);

    *state.current_device.write().await = Some(req.device_id);

    Ok(Json(info))
}

/// `GET /api/devices/current`: the device the server has selected (the
/// one `atalanta inspect` started with, or the page's last pick).
pub async fn current(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "deviceId": state.device_id().await }))
}
