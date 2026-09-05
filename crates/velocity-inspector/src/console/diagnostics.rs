//! The connection wizard's checks: `GET /api/diagnostics`.
//!
//! The CLI's `inspect` plugs in `velocity doctor`'s checks
//! (`velocity_doctor::run_checks`, in process) and the report is passed
//! through as `velocity doctor --json` prints it: `{verdict, checks: [{id,
//! area, status, summary, detail, fix: {kind, value}}]}`. Without a
//! [`Diagnostics`] (the default), the route answers 501.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::AppState;

pub enum DiagResult {
    /// No diagnostics in this build: why.
    Unavailable(String),
    /// The report, passed through to the page.
    Report(serde_json::Value),
    /// They exist but failed to run.
    Failed(String),
}

#[async_trait::async_trait]
pub trait Diagnostics: Send + Sync {
    async fn run(&self) -> DiagResult;
}

pub struct NoDiagnostics;

#[async_trait::async_trait]
impl Diagnostics for NoDiagnostics {
    async fn run(&self) -> DiagResult {
        DiagResult::Unavailable("diagnostics are not available in this build".into())
    }
}

/// `GET /api/diagnostics`.
pub async fn get(State(state): State<Arc<AppState>>) -> Response {
    match state.console.diagnostics.run().await {
        DiagResult::Report(v) => {
            Json(serde_json::json!({"available": true, "report": v})).into_response()
        }
        DiagResult::Unavailable(why) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"available": false, "message": why})),
        )
            .into_response(),
        DiagResult::Failed(why) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"available": true, "message": why})),
        )
            .into_response(),
    }
}
