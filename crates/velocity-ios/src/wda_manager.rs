use tracing::debug;
use velocity_common::{Result, VelocityError};

use crate::wda::WdaClient;

/// Manages the WDA client lifecycle, creating sessions on demand and reusing them
/// across tests for the same device.
pub struct WdaManager {
    wda_client: WdaClient,
    device_id: Option<String>,
    bundle_id: Option<String>,
}

impl WdaManager {
    /// Create a new manager targeting a WDA instance at the given base URL.
    pub fn new(wda_base_url: &str) -> Self {
        Self {
            wda_client: WdaClient::new(wda_base_url),
            device_id: None,
            bundle_id: None,
        }
    }

    /// [`Self::new`] with a per-call timeout other than the default 30 s.
    pub fn with_timeout(wda_base_url: &str, timeout: std::time::Duration) -> Self {
        Self {
            wda_client: WdaClient::with_timeout(wda_base_url, timeout),
            device_id: None,
            bundle_id: None,
        }
    }

    /// Take over the session a running WDA already holds (left by an
    /// earlier run): creating one costs 3–5 s. Returns whether one was adopted.
    pub async fn adopt(&mut self, device_id: &str) -> bool {
        if self.wda_client.session_id().is_some() {
            return true;
        }
        match self.wda_client.active_session_id().await {
            Some(sid) => {
                debug!(device_id, session_id = %sid, "adopting running WDA session");
                self.wda_client.set_session_id_opt(Some(sid));
                self.device_id = Some(device_id.to_string());
                self.bundle_id = None;
                true
            }
            None => false,
        }
    }

    /// Ensure a live WDA session that queries `bundle_id` on `device_id`.
    /// Returns true when a session was created with the app — which also
    /// launched it, so the caller needn't launch it again. Otherwise the
    /// existing session is pointed at the app and the caller launches it.
    pub async fn ensure_session(&mut self, device_id: &str, bundle_id: &str) -> Result<bool> {
        if self.wda_client.session_id().is_none() {
            self.adopt(device_id).await;
        }
        if self.wda_client.session_id().is_some() && self.device_id.as_deref() == Some(device_id) {
            // A WDA restart drops every session, so ask for this one.
            if self.wda_client.session_alive().await {
                if self.bundle_id.as_deref() != Some(bundle_id) {
                    self.wda_client.retarget(bundle_id).await?;
                    self.bundle_id = Some(bundle_id.to_string());
                }
                debug!(device_id, "reusing WDA session");
                return Ok(false);
            }
            debug!(device_id, "WDA session gone; creating a new one");
        }
        self.wda_client.set_session_id_opt(None);
        self.wda_client.create_session(bundle_id).await?;
        self.device_id = Some(device_id.to_string());
        self.bundle_id = Some(bundle_id.to_string());
        Ok(!bundle_id.is_empty())
    }

    /// A fresh session (never the one `/status` names) that queries
    /// `bundle_id` without launching it.
    pub async fn renew(&mut self, device_id: &str, bundle_id: &str) -> Result<()> {
        self.wda_client.set_session_id_opt(None);
        self.wda_client.create_session("").await?;
        if !bundle_id.is_empty() {
            self.wda_client.retarget(bundle_id).await?;
        }
        self.device_id = Some(device_id.to_string());
        self.bundle_id = Some(bundle_id.to_string());
        Ok(())
    }

    /// Check if the WDA instance is responsive.
    pub async fn health_check(&self) -> Result<bool> {
        self.wda_client.health_check().await
    }

    /// Get a reference to the underlying WDA client.
    pub fn client(&self) -> &WdaClient {
        &self.wda_client
    }

    /// Get a mutable reference to the underlying WDA client.
    pub fn client_mut(&mut self) -> &mut WdaClient {
        &mut self.wda_client
    }

    /// Force-invalidate the current session so the next operation creates a fresh one.
    pub fn invalidate_session(&mut self) {
        // Clear the session ID without sending a delete request — useful when
        // the session is already lost and we want to recreate on next use.
        self.wda_client.set_session_id_opt(None);
        self.device_id = None;
        self.bundle_id = None;
    }

    /// Delete the current session if one exists. Called during cleanup.
    pub async fn teardown(&mut self) -> Result<()> {
        if self.wda_client.session_id().is_some() {
            self.wda_client.delete_session().await?;
            self.device_id = None;
        }
        Ok(())
    }
}

impl WdaManager {
    /// Attempt to recover a lost session by creating a fresh one.
    pub async fn recover_session(
        &mut self,
        device_id: &str,
        bundle_id: &str,
        test_name: &str,
        attempt: u32,
        max_attempts: u32,
    ) -> Result<()> {
        debug!(
            device_id,
            test_name, attempt, max_attempts, "attempting WDA session recovery"
        );

        // Clean up old session state
        let _ = self.wda_client.delete_session().await;

        match self.wda_client.create_session(bundle_id).await {
            Ok(_) => {
                self.device_id = Some(device_id.to_string());
                self.bundle_id = Some(bundle_id.to_string());
                debug!(device_id, "WDA session recovered");
                Ok(())
            }
            Err(e) => {
                debug!(device_id, "WDA session recovery failed: {e}");
                Err(VelocityError::WdaSessionLost {
                    test_name: test_name.to_string(),
                    attempt,
                    max: max_attempts,
                })
            }
        }
    }
}
