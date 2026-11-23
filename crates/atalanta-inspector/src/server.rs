// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use crate::console::{control, diagnostics, mirror, project, tasks};
use crate::embedded;
use crate::routes::{
    actions, baselines, devices, headless, hierarchy, recording, screenshot, selector,
};
use crate::security;
use crate::state::AppState;
use crate::ws;

/// The screenshot review page. It needs the token like `/api` does.
pub const REVIEW_PATH: &str = "/review";

pub fn build_router(state: Arc<AppState>) -> Router {
    if state.review_only {
        return review_router(state);
    }
    let api = Router::new()
        .route("/devices", get(devices::list))
        .route("/devices/select", post(devices::select))
        .route("/devices/current", get(devices::current))
        .route("/devices/{id}/screenshot", get(screenshot::get))
        .route("/devices/{id}/hierarchy", get(hierarchy::get))
        .route("/devices/{id}/tap", post(actions::tap))
        .route("/devices/{id}/type", post(actions::type_text))
        .route("/devices/{id}/swipe", post(actions::swipe))
        .route("/devices/{id}/press-key", post(actions::press_key))
        .route("/selector/generate", post(selector::generate))
        .route("/flow/save", post(recording::save_flow))
        .route("/headless/render-tree", get(headless::get_render_tree))
        .route("/headless/re-render", post(headless::re_render))
        .route("/headless/status", get(headless::headless_status))
        .route("/ws/{device_id}", get(ws::handler))
        // The console (live mirror, control, tasks, diagnostics).
        .route("/mirror/{device_id}", get(mirror::handler))
        .route(
            "/devices/{id}/control",
            get(control::status)
                .post(control::take)
                .delete(control::release),
        )
        .route("/devices/{id}/input", post(control::input))
        .route("/project/suites", get(project::suites))
        .route("/diagnostics", get(diagnostics::get))
        .route("/tasks", get(tasks::list))
        .route("/tasks/run", post(tasks::start_run))
        .route("/tasks/explore", post(tasks::start_explore))
        .route("/tasks/{id}", get(tasks::get))
        .route("/tasks/{id}/inject", post(tasks::inject))
        .route("/tasks/{id}/cancel", post(tasks::cancel))
        .route("/tasks/{id}/save", post(tasks::save))
        .route("/tasks/{id}/events", get(tasks::events))
        .route("/tasks/{id}/files", get(tasks::files))
        .route("/tasks/{id}/files/{*path}", get(tasks::file))
        .route("/baselines", get(baselines::list))
        .route("/baselines/image", get(baselines::image))
        .route("/baselines/accept", post(baselines::accept))
        .route("/baselines/reject", post(baselines::reject))
        // An unknown API route is a 404, not the UI's HTML (VEL-256).
        .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "Unknown API route") });

    Router::new()
        .nest("/api", api)
        // Server-rendered, so it works without the React UI (U1).
        .route(REVIEW_PATH, get(baselines::page))
        .fallback(get(embedded::static_handler))
        // No CORS layer: only the inspector's own page may call the API.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::guard,
        ))
        .with_state(state)
}

/// The review server's routes (`atalanta baselines review --serve`): the
/// page and the baseline API, nothing else. Audit 2026-11 INSP-1: a shared
/// review URL granted the whole console, including `POST /api/tasks/run`
/// (a `atalanta run` child, with the project's `exec` allow-list).
fn review_router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/baselines", get(baselines::list))
        .route("/baselines/image", get(baselines::image))
        .route("/baselines/accept", post(baselines::accept))
        .route("/baselines/reject", post(baselines::reject))
        .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "Unknown API route") });
    Router::new()
        .nest("/api", api)
        .route(REVIEW_PATH, get(baselines::page))
        .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "Not found") })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::guard,
        ))
        .with_state(state)
}
