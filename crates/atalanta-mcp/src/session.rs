use std::path::PathBuf;
use std::time::{Duration, Instant};

use atalanta_common::{DeviceInfo, PlatformDriver, Result};

/// Tracks state across an MCP session for caching and context.
pub struct McpSession {
    /// Currently selected device ID.
    pub current_device: Option<String>,

    /// Cached device list with expiration.
    device_list_cache: Option<(Vec<DeviceInfo>, Instant)>,

    /// Path to last captured screenshot.
    pub last_screenshot: Option<PathBuf>,

    /// Path to the atalanta config file, if available.
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

    /// Get device list, using cache if still valid. An empty list is not
    /// kept: it is the moment before a device boots, and caching it made
    /// "No devices found" last 30 s after the emulator was up.
    pub async fn get_devices(&mut self, driver: &dyn PlatformDriver) -> Result<Vec<DeviceInfo>> {
        if let Some((ref devices, timestamp)) = self.device_list_cache {
            if timestamp.elapsed() < self.cache_ttl {
                return Ok(devices.clone());
            }
        }
        self.fresh_devices(driver).await
    }

    /// Read the devices again (what the `list_devices` tool asks for), and
    /// remember them for the lookups that follow.
    pub async fn fresh_devices(&mut self, driver: &dyn PlatformDriver) -> Result<Vec<DeviceInfo>> {
        let devices = driver.list_devices().await?;
        self.device_list_cache = (!devices.is_empty()).then(|| (devices.clone(), Instant::now()));
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
            .ok_or_else(|| atalanta_common::AtalantaError::ServiceUnavailable {
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
    use atalanta_common::{Direction, Element, Key, Selector};

    /// A driver that sees no devices, or the ones in `booted` once it is
    /// set; nothing else is called.
    #[derive(Default)]
    struct NoDevices {
        booted: std::sync::Mutex<Vec<DeviceInfo>>,
        lists: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl PlatformDriver for NoDevices {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.booted.lock().unwrap().clone())
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
        let err = session
            .resolve_device_id(&NoDevices::default())
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 3, "{err:?}");
        assert!(err.to_string().contains("No devices found"), "{err}");
    }

    fn emulator() -> DeviceInfo {
        DeviceInfo {
            id: "emulator-5554".into(),
            name: "Pixel".into(),
            platform: atalanta_common::Platform::Android,
            state: atalanta_common::DeviceState::Booted,
            os_version: None,
            device_type: Default::default(),
        }
    }

    // An agent that boots an emulator and lists again must see it: an empty
    // list was cached for 30 s, and "No devices found" with it.
    #[tokio::test]
    async fn a_device_that_boots_after_an_empty_list_is_found() {
        let driver = NoDevices::default();
        let mut session = McpSession::new(None, None);
        assert!(session.get_devices(&driver).await.unwrap().is_empty());
        *driver.booted.lock().unwrap() = vec![emulator()];
        assert_eq!(session.get_devices(&driver).await.unwrap().len(), 1);
        assert_eq!(
            session.resolve_device_id(&driver).await.unwrap(),
            "emulator-5554"
        );
    }

    // `list_devices` is a request to look: it reads the devices again.
    #[tokio::test]
    async fn a_fresh_list_is_read_again() {
        let driver = NoDevices::default();
        *driver.booted.lock().unwrap() = vec![emulator()];
        let mut session = McpSession::new(None, None);
        session.get_devices(&driver).await.unwrap();
        session.get_devices(&driver).await.unwrap();
        assert_eq!(driver.lists.load(std::sync::atomic::Ordering::SeqCst), 1);
        session.fresh_devices(&driver).await.unwrap();
        assert_eq!(driver.lists.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
