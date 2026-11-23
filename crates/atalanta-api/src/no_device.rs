//! The driver of a run whose steps are all `http` (with `wait` and
//! `setConfig`): there is no device to find, lease, prepare or install on.
//! It lists one stand-in per `--parallel` slot, so the sequential and the
//! parallel runner work unchanged; anything that needs a screen is refused.

use velocity_common::{
    DeviceInfo, DeviceState, DeviceType, Direction, Element, Key, Platform, PlatformDriver, Result,
    Selector, VelocityError,
};

pub struct NoDeviceDriver {
    slots: usize,
    platform: Platform,
}

impl NoDeviceDriver {
    /// `slots` stand-ins (at least one), reported as `platform`.
    pub fn new(slots: usize, platform: Platform) -> Self {
        Self {
            slots: slots.max(1),
            platform,
        }
    }
}

fn refuse(what: &str) -> VelocityError {
    VelocityError::Config(format!(
        "{what} needs a device, and this run has none (its tests have only http, wait and setConfig steps)"
    ))
}

#[async_trait::async_trait]
impl PlatformDriver for NoDeviceDriver {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok((0..self.slots)
            .map(|i| DeviceInfo {
                id: velocity_common::no_device_id(i),
                name: "no device (http steps only)".into(),
                platform: self.platform,
                state: DeviceState::Booted,
                os_version: None,
                device_type: DeviceType::Unknown,
            })
            .collect())
    }
    async fn boot_device(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }
    async fn shutdown_device(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }
    async fn install_app(&self, _device_id: &str, _app_path: &str) -> Result<()> {
        Err(refuse("installing an app"))
    }
    async fn launch_app(&self, _device_id: &str, _app_id: &str, _clear: bool) -> Result<()> {
        Err(refuse("launchApp"))
    }
    async fn stop_app(&self, _device_id: &str, _app_id: &str) -> Result<()> {
        Err(refuse("stopApp"))
    }
    async fn find_element(&self, _device_id: &str, _selector: &Selector) -> Result<Element> {
        Err(refuse("finding an element"))
    }
    async fn find_elements(&self, _device_id: &str, _selector: &Selector) -> Result<Vec<Element>> {
        Err(refuse("finding elements"))
    }
    async fn get_hierarchy(&self, _device_id: &str) -> Result<Element> {
        Err(refuse("reading the screen"))
    }
    async fn tap(&self, _device_id: &str, _element: &Element) -> Result<()> {
        Err(refuse("tap"))
    }
    async fn double_tap(&self, _device_id: &str, _element: &Element) -> Result<()> {
        Err(refuse("doubleTap"))
    }
    async fn long_press(&self, _device_id: &str, _element: &Element, _ms: u64) -> Result<()> {
        Err(refuse("longPress"))
    }
    async fn input_text(&self, _device_id: &str, _element: &Element, _text: &str) -> Result<()> {
        Err(refuse("inputText"))
    }
    async fn clear_text(&self, _device_id: &str, _element: &Element) -> Result<()> {
        Err(refuse("clearText"))
    }
    async fn swipe(&self, _device_id: &str, _direction: Direction) -> Result<()> {
        Err(refuse("swipe"))
    }
    async fn swipe_coords(
        &self,
        _device_id: &str,
        _from: (i32, i32),
        _to: (i32, i32),
    ) -> Result<()> {
        Err(refuse("swipe"))
    }
    async fn press_key(&self, _device_id: &str, _key: Key) -> Result<()> {
        Err(refuse("pressKey"))
    }
    async fn screenshot(&self, _device_id: &str) -> Result<Vec<u8>> {
        Err(refuse("a screenshot"))
    }
    async fn screen_size(&self, _device_id: &str) -> Result<(i32, i32)> {
        Err(refuse("the screen size"))
    }
    async fn get_element_text(&self, _device_id: &str, _element: &Element) -> Result<String> {
        Err(refuse("reading text"))
    }
    async fn is_element_visible(&self, _device_id: &str, _element: &Element) -> Result<bool> {
        Err(refuse("checking visibility"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stand_ins_are_listed_and_everything_else_is_refused() {
        let d = NoDeviceDriver::new(0, Platform::Android);
        let devices = d.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1, "at least one slot");
        assert!(velocity_common::is_no_device(&devices[0].id));
        let d = NoDeviceDriver::new(3, Platform::Ios);
        let ids: Vec<String> = d
            .list_devices()
            .await
            .unwrap()
            .into_iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(ids, ["http-0", "http-1", "http-2"]);
        d.boot_device("http-0").await.unwrap();
        for e in [
            d.launch_app("http-0", "a", false).await.unwrap_err(),
            d.screenshot("http-0").await.unwrap_err(),
            d.press_key("http-0", Key::Back).await.unwrap_err(),
        ] {
            assert!(matches!(e, VelocityError::Config(_)), "{e:?}");
            assert_eq!(e.exit_code(), 2);
            assert!(e.to_string().contains("this run has none"), "{e}");
        }
    }
}
