// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use atalanta_common::types::Element;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::state::AppState;

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(device_id): Path<String>,
) -> Result<Json<Element>, (StatusCode, String)> {
    let hierarchy = state
        .driver
        .get_hierarchy(&device_id)
        .await
        .map_err(super::api_error)?;

    Ok(Json(hierarchy))
}
