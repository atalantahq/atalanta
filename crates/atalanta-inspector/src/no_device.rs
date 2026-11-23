// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! A driver with no device, for an inspector that only serves the
//! screenshot review page (`atalanta baselines review --serve`): listing
//! devices returns none and every device action is a config error.

use atalanta_common::types::{DeviceInfo, Direction, Element, Key, Selector};
use atalanta_common::{AtalantaError, PlatformDriver, Result};

pub struct NoDevice;

fn none<T>() -> Result<T> {
    Err(AtalantaError::Config(
        "this inspector serves screenshot review only; start `atalanta inspect` to drive a device"
            .into(),
    ))
}

#[async_trait::async_trait]
impl PlatformDriver for NoDevice {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }
    async fn boot_device(&self, _: &str) -> Result<()> {
        none()
    }
    async fn shutdown_device(&self, _: &str) -> Result<()> {
        none()
    }
    async fn install_app(&self, _: &str, _: &str) -> Result<()> {
        none()
    }
    async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
        none()
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        none()
    }
    async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
        none()
    }
    async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
        none()
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        none()
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        none()
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        none()
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        none()
    }
    async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
        none()
    }
    async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
        none()
    }
    async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
        none()
    }
    async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        none()
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        none()
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        none()
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        none()
    }
    async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
        none()
    }
    async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
        none()
    }
}
