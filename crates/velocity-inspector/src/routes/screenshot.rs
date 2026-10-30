use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct ScreenshotQuery {
    /// The content hash the live view announced (`?v=`): served from the copy
    /// it hashed, so the device isn't captured twice.
    pub v: Option<String>,
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
    Query(query): Query<ScreenshotQuery>,
) -> Result<Response, (StatusCode, String)> {
    if let Some(v) = &query.v {
        let cached = state.screenshots.read().await.get(&device_id).cloned();
        if let Some((hash, png)) = cached.filter(|(hash, _)| format!("{hash:x}") == *v) {
            return Ok((
                [
                    (header::CONTENT_TYPE, "image/png".to_string()),
                    (header::CACHE_CONTROL, "private, max-age=60".to_string()),
                    (header::ETAG, format!("\"{hash:x}\"")),
                ],
                png.as_ref().clone(),
            )
                .into_response());
        }
    }

    let png_bytes = state
        .driver
        .screenshot(&device_id)
        .await
        .map_err(super::api_error)?;

    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        png_bytes,
    )
        .into_response())
}
