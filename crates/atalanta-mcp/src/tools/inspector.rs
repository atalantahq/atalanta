use std::sync::Arc;
use std::time::Duration;

use atalanta_common::{AtalantaError, PlatformDriver, Result};
use atalanta_inspector::InspectorServer;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::session::McpSession;

const DEFAULT_PORT: u16 = 9876;
const MAX_PORT_ATTEMPTS: u16 = 10;

pub async fn open_inspector(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    session: &mut McpSession,
    args: &Value,
) -> Result<Value> {
    // Idempotent: return existing URL if already running
    if let Some(port) = session.inspector_port {
        return Ok(serde_json::json!({
            "status": "already_running",
            "url": session.inspector_url.clone().unwrap_or_else(|| format!("http://localhost:{port}")),
            "port": port
        }));
    }

    let requested_port = args
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| {
            if p > u16::MAX as u64 {
                Err(AtalantaError::Config(format!(
                    "Requested inspector port {p} exceeds {}",
                    u16::MAX
                )))
            } else {
                Ok(p as u16)
            }
        })
        .transpose()?
        .unwrap_or(DEFAULT_PORT);

    // Find an available port
    let listener = find_available_port(requested_port)
        .await
        .map_err(|_| AtalantaError::Config(no_port_message(requested_port)))?;
    let addr = listener
        .local_addr()
        .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Failed to inspect listener: {e}")))?;
    let port = addr.port();

    let driver_clone = driver.clone();
    let device_id_owned = device_id.to_string();
    let (ready_tx, ready_rx) = oneshot::channel();
    // The URL carries the per-session token the inspector API requires.
    let server = InspectorServer::new(driver_clone, Some(device_id_owned), None);
    let url = server.url(addr);

    let handle = tokio::spawn(async move {
        let _ = ready_tx.send(Ok(port));
        if let Err(e) = server.start_with_listener(listener).await {
            tracing::error!("Inspector server exited with error: {e}");
        }
    });

    let ready_port = match tokio::time::timeout(Duration::from_secs(2), ready_rx).await {
        Ok(Ok(Ok(port))) => port,
        Ok(Ok(Err(err))) => {
            handle.abort();
            return Err(AtalantaError::Config(err));
        }
        Ok(Err(_)) => {
            handle.abort();
            return Err(AtalantaError::Internal(anyhow::anyhow!(
                "Inspector startup channel closed before readiness"
            )));
        }
        Err(_) => {
            handle.abort();
            return Err(AtalantaError::Internal(anyhow::anyhow!(
                "Timed out waiting for inspector startup"
            )));
        }
    };

    session.inspector_port = Some(ready_port);
    session.inspector_url = Some(url.clone());
    session.inspector_handle = Some(handle);

    Ok(serde_json::json!({
        "status": "started",
        "url": url,
        "port": ready_port
    }))
}

pub async fn close_inspector(session: &mut McpSession, _args: &Value) -> Result<Value> {
    if let Some(handle) = session.inspector_handle.take() {
        handle.abort();
        session.inspector_port = None;
        session.inspector_url = None;
        Ok(serde_json::json!({
            "status": "stopped"
        }))
    } else {
        Ok(serde_json::json!({
            "status": "not_running"
        }))
    }
}

/// `start + MAX_PORT_ATTEMPTS` overflowed (and panicked) near 65535 (VEL-246).
fn no_port_message(start: u16) -> String {
    let end = start.saturating_add(MAX_PORT_ATTEMPTS.saturating_sub(1));
    format!("No available port found in range {start}-{end}")
}

async fn find_available_port(start: u16) -> std::result::Result<TcpListener, ()> {
    let end = start.saturating_add(MAX_PORT_ATTEMPTS.saturating_sub(1));
    for port in start..=end {
        if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{port}")).await {
            return Ok(listener);
        }
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_port_message_does_not_overflow_near_u16_max() {
        assert_eq!(
            no_port_message(65535),
            "No available port found in range 65535-65535"
        );
        assert_eq!(
            no_port_message(9876),
            "No available port found in range 9876-9885"
        );
    }
}
