// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, Json};

use crate::state::AppState;

/// What the not-yet-wired headless endpoints answer: 501, rather than a 200
/// that claims success (VEL-257).
fn not_implemented(what: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": format!("{what} is not implemented yet; use /api/devices/{{id}}/hierarchy and /screenshot"),
        })),
    )
        .into_response()
}

/// GET /api/headless/render-tree
pub async fn get_render_tree(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    not_implemented("The headless render tree")
}

/// POST /api/headless/re-render
pub async fn re_render() -> impl IntoResponse {
    not_implemented("Headless re-render")
}

/// GET /api/headless/status
pub async fn headless_status() -> impl IntoResponse {
    not_implemented("Headless status")
}
