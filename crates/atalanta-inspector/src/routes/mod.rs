// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

pub mod actions;
pub mod baselines;
pub mod devices;
pub mod headless;
pub mod hierarchy;
pub mod recording;
pub mod screenshot;
pub mod selector;

use atalanta_common::AtalantaError;
use axum::http::StatusCode;

/// The HTTP status a driver error is (VEL-256): an unknown device is 404, a
/// missing element 404, a busy device or an element that can't take the
/// action now (disabled, covered) 409, a lost connection 503; only the rest
/// is 500.
pub fn api_error(e: AtalantaError) -> (StatusCode, String) {
    let status = match e.exit_code() {
        _ if matches!(e, AtalantaError::DeviceNotFound { .. }) => StatusCode::NOT_FOUND,
        _ if matches!(e, AtalantaError::DeviceLocked { .. }) => StatusCode::CONFLICT,
        1 if matches!(e, AtalantaError::ElementNotFound { .. }) => StatusCode::NOT_FOUND,
        // A web tap on a disabled control read as a server fault (M-2).
        1 if matches!(e, AtalantaError::NotActionable { .. }) => StatusCode::CONFLICT,
        2 => StatusCode::BAD_REQUEST,
        3 => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_errors_map_to_their_http_status() {
        let cases = [
            (
                AtalantaError::DeviceNotFound {
                    id: "x".into(),
                    available: vec![],
                },
                StatusCode::NOT_FOUND,
            ),
            (
                AtalantaError::ElementNotFound {
                    selector: "s".into(),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                },
                StatusCode::NOT_FOUND,
            ),
            (
                AtalantaError::ServiceUnavailable {
                    service: "wda".into(),
                    detail: "gone".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                AtalantaError::NotActionable {
                    selector: "s".into(),
                    reason: "disabled".into(),
                },
                StatusCode::CONFLICT,
            ),
            (AtalantaError::Config("bad".into()), StatusCode::BAD_REQUEST),
            (
                AtalantaError::Internal(anyhow::anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (e, want) in cases {
            assert_eq!(api_error(e).0, want);
        }
    }
}
