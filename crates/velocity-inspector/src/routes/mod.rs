pub mod actions;
pub mod baselines;
pub mod devices;
pub mod headless;
pub mod hierarchy;
pub mod recording;
pub mod screenshot;
pub mod selector;

use axum::http::StatusCode;
use velocity_common::VelocityError;

/// The HTTP status a driver error is (VEL-256): an unknown device is 404, a
/// missing element 404, a busy device or an element that can't take the
/// action now (disabled, covered) 409, a lost connection 503; only the rest
/// is 500.
pub fn api_error(e: VelocityError) -> (StatusCode, String) {
    let status = match e.exit_code() {
        _ if matches!(e, VelocityError::DeviceNotFound { .. }) => StatusCode::NOT_FOUND,
        _ if matches!(e, VelocityError::DeviceLocked { .. }) => StatusCode::CONFLICT,
        1 if matches!(e, VelocityError::ElementNotFound { .. }) => StatusCode::NOT_FOUND,
        // A web tap on a disabled control read as a server fault (M-2).
        1 if matches!(e, VelocityError::NotActionable { .. }) => StatusCode::CONFLICT,
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
                VelocityError::DeviceNotFound {
                    id: "x".into(),
                    available: vec![],
                },
                StatusCode::NOT_FOUND,
            ),
            (
                VelocityError::ElementNotFound {
                    selector: "s".into(),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                },
                StatusCode::NOT_FOUND,
            ),
            (
                VelocityError::ServiceUnavailable {
                    service: "wda".into(),
                    detail: "gone".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                VelocityError::NotActionable {
                    selector: "s".into(),
                    reason: "disabled".into(),
                },
                StatusCode::CONFLICT,
            ),
            (VelocityError::Config("bad".into()), StatusCode::BAD_REQUEST),
            (
                VelocityError::Internal(anyhow::anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (e, want) in cases {
            assert_eq!(api_error(e).0, want);
        }
    }
}
