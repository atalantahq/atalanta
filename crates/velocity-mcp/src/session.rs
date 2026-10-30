use std::path::PathBuf;
use std::time::{Duration, Instant};

use velocity_common::{DeviceInfo, PlatformDriver, Result};

/// Tracks state across an MCP session for caching and context.
pub struct McpSession {
    /// Currently selected device ID.
    pub current_device: Option<String>,

    /// Cached device list with expiration.
    device_list_cache: Option<(Vec<DeviceInfo>, Instant)>,

    /// Path to last captured screenshot.
    pub last_screenshot: Option<PathBuf>,

    /// Path to the velocity config file, if available.
    pub config_path: Option<String>,

    /// Cache TTL for device list queries.
    cache_ttl: Duration,

    /// Port the inspector is running on, if started.
    pub inspector_port: Option<u16>,

    /// URL (with its session token) to open the inspector at, if started.
    pub inspector_url: Option<String>,

    /// Handle to the inspector background task.
    pub inspector_handle: Option<tokio::task::JoinHandle<()>>,
}

impl McpSession {
    pub fn new(device_id: Option<String>, config_path: Option<String>) -> Self {
        Self {
            current_device: device_id,
            device_list_cache: None,
            last_screenshot: None,
            config_path,
            cache_ttl: Duration::from_secs(30),
            inspector_port: None,
            inspector_url: None,
            inspector_handle: None,
        }
    }

    /// Get device list, using cache if still valid.
    pub async fn get_devices(&mut self, driver: &dyn PlatformDriver) -> Result<Vec<DeviceInfo>> {
        if let Some((ref devices, timestamp)) = self.device_list_cache {
            if timestamp.elapsed() < self.cache_ttl {
                return Ok(devices.clone());
            }
        }

        let devices = driver.list_devices().await?;
        self.device_list_cache = Some((devices.clone(), Instant::now()));
        Ok(devices)
    }

    /// Invalidate the device list cache (e.g. after boot/shutdown).
    pub fn invalidate_device_cache(&mut self) {
        self.device_list_cache = None;
    }

    /// Get the current device ID, falling back to auto-detection.
    pub async fn resolve_device_id(&mut self, driver: &dyn PlatformDriver) -> Result<String> {
        if let Some(ref id) = self.current_device {
            return Ok(id.clone());
        }

        let devices = self.get_devices(driver).await?;
        let id = devices
            .first()
            // No device is a device problem (exit 3), not a config one (VEL-051).
            .ok_or_else(|| velocity_common::VelocityError::ServiceUnavailable {
                service: "device".to_string(),
                detail: "No devices found. Boot a device first.".to_string(),
            })?
            .id
            .clone();

        self.current_device = Some(id.clone());
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{Direction, Element, Key, Selector};

    /// A driver that sees no devices; nothing else is called.
    struct NoDevices;

    #[async_trait::async_trait]
    impl PlatformDriver for NoDevices {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(Vec::new())
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            unreachable!()
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
            unreachable!()
        }
        async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
            unreachable!()
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            unreachable!()
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
            unreachable!()
        }
        async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
            unreachable!()
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            unreachable!()
        }
        async fn press_key(&self, _: &str, _: Key) -> Result<()> {
            unreachable!()
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            unreachable!()
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            unreachable!()
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            unreachable!()
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            unreachable!()
        }
    }

    /// VEL-051: no device to run on is a device error (exit 3), not config (exit 2).
    #[tokio::test]
    async fn no_devices_is_a_device_error() {
        let mut session = McpSession::new(None, None);
        let err = session.resolve_device_id(&NoDevices).await.unwrap_err();
        assert_eq!(err.exit_code(), 3, "{err:?}");
        assert!(err.to_string().contains("No devices found"), "{err}");
    }
}
