//! One driver over several platforms, for a console that lists Android,
//! iOS and web devices together: every call goes to the driver of the
//! device id's platform ([`Platform::of_device_id`]), and the device list
//! is all of theirs.

use std::path::Path;
use std::sync::Arc;

use velocity_common::evidence::{EvidenceRequest, EvidenceSession};
use velocity_common::test_types::{ResetConfig, ResetStrategy};
use velocity_common::types::{
    DeviceInfo, DeviceSettle, DeviceState, DeviceType, Direction, Element, Key, MockRule, Rect,
    Selector, SystemState,
};
use velocity_common::{
    HealthStatus, InstallOutcome, Liveness, Platform, PlatformDriver, Result, VelocityError,
};

pub struct RoutingDriver {
    drivers: Vec<(Platform, Arc<dyn PlatformDriver>)>,
}

impl RoutingDriver {
    pub fn new(drivers: Vec<(Platform, Arc<dyn PlatformDriver>)>) -> Self {
        Self { drivers }
    }

    async fn list(&self, strict: bool) -> Result<Vec<DeviceInfo>> {
        let mut all = Vec::new();
        for (p, d) in &self.drivers {
            if strict {
                all.extend(d.list_devices_strict().await?);
            } else {
                match d.list_devices().await {
                    Ok(list) => all.extend(list),
                    Err(e) => tracing::debug!("{p} devices not listed: {e}"),
                }
            }
            if *p == Platform::Web && !all.iter().any(|x: &DeviceInfo| x.id == "web") {
                all.push(DeviceInfo {
                    id: "web".into(),
                    name: "Chrome Browser (starts when selected)".into(),
                    platform: Platform::Web,
                    state: DeviceState::Booted,
                    os_version: None,
                    device_type: DeviceType::Unknown,
                });
            }
        }
        Ok(all)
    }

    fn of(&self, device_id: &str) -> Result<&Arc<dyn PlatformDriver>> {
        let p = Platform::of_device_id(device_id);
        // "web-2" and the like: a web id that isn't plain "web".
        let p = if p == Platform::Android && device_id.starts_with("web-") {
            Platform::Web
        } else {
            p
        };
        self.drivers
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, d)| d)
            .ok_or_else(|| VelocityError::DeviceNotFound {
                id: device_id.to_string(),
                available: vec![],
            })
    }
}

#[async_trait::async_trait]
impl PlatformDriver for RoutingDriver {
    async fn prepare(&self, device_id: &str) -> Result<()> {
        self.of(device_id)?.prepare(device_id).await
    }
    async fn cleanup(&self) {
        for (_, d) in &self.drivers {
            d.cleanup().await;
        }
    }
    /// As healthy as the least healthy driver.
    async fn health_check(&self) -> HealthStatus {
        let mut worst = HealthStatus::Healthy;
        for (_, d) in &self.drivers {
            match d.health_check().await {
                HealthStatus::Unhealthy => return HealthStatus::Unhealthy,
                HealthStatus::Degraded => worst = HealthStatus::Degraded,
                HealthStatus::Healthy => {}
            }
        }
        worst
    }
    async fn restart_session(&self) -> Result<()> {
        for (_, d) in &self.drivers {
            d.restart_session().await?;
        }
        Ok(())
    }
    /// Every platform's devices. A platform whose tools are missing lists
    /// none rather than failing the rest; the web always offers its browser.
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.list(false).await
    }
    /// Like [`Self::list_devices`], but a platform that can't list them all
    /// is an error: the caller wants "don't know" rather than a short list.
    async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        self.list(true).await
    }
    async fn boot_device(&self, id: &str) -> Result<()> {
        self.of(id)?.boot_device(id).await
    }
    async fn shutdown_device(&self, id: &str) -> Result<()> {
        self.of(id)?.shutdown_device(id).await
    }
    async fn install_app(&self, id: &str, path: &str) -> Result<()> {
        self.of(id)?.install_app(id, path).await
    }
    async fn install_app_if_changed(
        &self,
        id: &str,
        path: &str,
        app: &str,
        hash: &str,
    ) -> Result<InstallOutcome> {
        self.of(id)?
            .install_app_if_changed(id, path, app, hash)
            .await
    }
    async fn launch_app(&self, id: &str, app: &str, clear: bool) -> Result<()> {
        self.of(id)?.launch_app(id, app, clear).await
    }
    async fn launch_app_with(
        &self,
        id: &str,
        app: &str,
        clear: bool,
        args: &velocity_common::LaunchArgs,
    ) -> Result<()> {
        self.of(id)?.launch_app_with(id, app, clear, args).await
    }
    async fn stop_app(&self, id: &str, app: &str) -> Result<()> {
        self.of(id)?.stop_app(id, app).await
    }
    async fn find_element(&self, id: &str, s: &Selector) -> Result<Element> {
        self.of(id)?.find_element(id, s).await
    }
    async fn find_elements(&self, id: &str, s: &Selector) -> Result<Vec<Element>> {
        self.of(id)?.find_elements(id, s).await
    }
    async fn get_hierarchy(&self, id: &str) -> Result<Element> {
        self.of(id)?.get_hierarchy(id).await
    }
    async fn get_hierarchy_cached(&self, id: &str) -> Result<Element> {
        self.of(id)?.get_hierarchy_cached(id).await
    }
    async fn get_hierarchy_for_compare(&self, id: &str) -> Result<Element> {
        self.of(id)?.get_hierarchy_for_compare(id).await
    }
    async fn settle_on_device(
        &self,
        id: &str,
        reaction_ms: u64,
        quiet_ms: u64,
        cap_ms: u64,
    ) -> Option<DeviceSettle> {
        self.of(id)
            .ok()?
            .settle_on_device(id, reaction_ms, quiet_ms, cap_ms)
            .await
    }
    async fn crash_report(&self, id: &str, app: &str) -> Option<String> {
        self.of(id).ok()?.crash_report(id, app).await
    }
    async fn pending_crash(&self, id: &str, app: &str) -> Option<String> {
        self.of(id).ok()?.pending_crash(id, app).await
    }
    async fn copy_crash_logs(&self, id: &str, app: &str, dest: &Path) -> Vec<std::path::PathBuf> {
        match self.of(id) {
            Ok(d) => d.copy_crash_logs(id, app, dest).await,
            Err(_) => vec![],
        }
    }
    async fn start_evidence(
        &self,
        id: &str,
        req: &EvidenceRequest,
    ) -> Option<Box<dyn EvidenceSession>> {
        self.of(id).ok()?.start_evidence(id, req).await
    }
    fn sync_probe_port(&self, id: &str) -> Option<u16> {
        self.of(id).ok()?.sync_probe_port(id)
    }
    async fn app_liveness(&self, id: &str, app: &str) -> Liveness {
        match self.of(id) {
            Ok(d) => d.app_liveness(id, app).await,
            Err(_) => Liveness::Unknown,
        }
    }
    async fn system_bars(&self, id: &str) -> Option<Vec<Rect>> {
        self.of(id).ok()?.system_bars(id).await
    }
    async fn freeze_status_bar(&self, id: &str) -> bool {
        match self.of(id) {
            Ok(d) => d.freeze_status_bar(id).await,
            Err(_) => false,
        }
    }
    async fn unfreeze_status_bar(&self) {
        for (_, d) in &self.drivers {
            d.unfreeze_status_bar().await;
        }
    }
    async fn app_fingerprint(&self, id: &str, app: &str) -> Option<String> {
        self.of(id).ok()?.app_fingerprint(id, app).await
    }
    async fn save_app_state(&self, id: &str, app: &str, name: &str) -> Result<()> {
        self.of(id)?.save_app_state(id, app, name).await
    }
    async fn restore_app_state(&self, id: &str, app: &str, name: &str) -> Result<()> {
        self.of(id)?.restore_app_state(id, app, name).await
    }
    fn last_match_count(&self, id: &str, s: &Selector) -> Option<usize> {
        self.of(id).ok()?.last_match_count(id, s)
    }
    /// Only when every driver caches: the caller can't say which device it
    /// means, and an extra read on one that doesn't is a full dump.
    fn has_hierarchy_cache(&self) -> bool {
        !self.drivers.is_empty() && self.drivers.iter().all(|(_, d)| d.has_hierarchy_cache())
    }
    async fn device_ready(&self, id: &str) -> bool {
        match self.of(id) {
            Ok(d) => d.device_ready(id).await,
            Err(_) => false,
        }
    }
    async fn invalidate_hierarchy_cache(&self) {
        for (_, d) in &self.drivers {
            d.invalidate_hierarchy_cache().await;
        }
    }
    async fn set_stress(&self, id: &str, on: bool) -> bool {
        match self.of(id) {
            Ok(d) => d.set_stress(id, on).await,
            Err(_) => false,
        }
    }
    async fn reset_app(&self, id: &str, app: &str, reset: &ResetConfig) -> Result<ResetStrategy> {
        self.of(id)?.reset_app(id, app, reset).await
    }
    async fn tap(&self, id: &str, e: &Element) -> Result<()> {
        self.of(id)?.tap(id, e).await
    }
    async fn double_tap(&self, id: &str, e: &Element) -> Result<()> {
        self.of(id)?.double_tap(id, e).await
    }
    async fn long_press(&self, id: &str, e: &Element, ms: u64) -> Result<()> {
        self.of(id)?.long_press(id, e, ms).await
    }
    async fn touch_forced(
        &self,
        id: &str,
        e: &Element,
        touch: velocity_common::Touch,
    ) -> Result<()> {
        self.of(id)?.touch_forced(id, e, touch).await
    }
    async fn input_text(&self, id: &str, e: &Element, text: &str) -> Result<()> {
        self.of(id)?.input_text(id, e, text).await
    }
    async fn clear_text(&self, id: &str, e: &Element) -> Result<()> {
        self.of(id)?.clear_text(id, e).await
    }
    async fn swipe(&self, id: &str, d: Direction) -> Result<()> {
        self.of(id)?.swipe(id, d).await
    }
    async fn swipe_coords(&self, id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.of(id)?.swipe_coords(id, from, to).await
    }
    async fn drag(
        &self,
        id: &str,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        self.of(id)?.drag(id, from, to, hold_ms, move_ms).await
    }
    async fn network_busy(&self, id: &str) -> bool {
        match self.of(id) {
            Ok(d) => d.network_busy(id).await,
            Err(_) => false,
        }
    }
    async fn add_mock(&self, id: &str, rule: MockRule) -> Result<()> {
        self.of(id)?.add_mock(id, rule).await
    }
    async fn clear_mocks(&self, id: &str) -> Result<()> {
        self.of(id)?.clear_mocks(id).await
    }
    async fn system_state(&self, id: &str) -> Result<SystemState> {
        self.of(id)?.system_state(id).await
    }
    async fn scroll(&self, id: &str, d: Direction) -> Result<()> {
        self.of(id)?.scroll(id, d).await
    }
    async fn scroll_coords(&self, id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.of(id)?.scroll_coords(id, from, to).await
    }
    async fn press_key(&self, id: &str, key: Key) -> Result<()> {
        self.of(id)?.press_key(id, key).await
    }
    async fn keyboard_frame(&self, id: &str) -> Result<Option<Rect>> {
        self.of(id)?.keyboard_frame(id).await
    }
    async fn hide_keyboard(&self, id: &str) -> Result<bool> {
        self.of(id)?.hide_keyboard(id).await
    }
    async fn tap_xy(&self, id: &str, x: i32, y: i32) -> Result<()> {
        self.of(id)?.tap_xy(id, x, y).await
    }
    async fn input_raw(&self, id: &str, text: &str) -> Result<()> {
        self.of(id)?.input_raw(id, text).await
    }
    async fn screenshot(&self, id: &str) -> Result<Vec<u8>> {
        self.of(id)?.screenshot(id).await
    }
    async fn screenshot_for_compare(&self, id: &str) -> Result<Vec<u8>> {
        self.of(id)?.screenshot_for_compare(id).await
    }
    async fn screen_size(&self, id: &str) -> Result<(i32, i32)> {
        self.of(id)?.screen_size(id).await
    }
    async fn reveal(&self, id: &str, e: &Element) -> Result<Element> {
        self.of(id)?.reveal(id, e).await
    }
    async fn get_element_text(&self, id: &str, e: &Element) -> Result<String> {
        self.of(id)?.get_element_text(id, e).await
    }
    async fn element_text_matches(
        &self,
        id: &str,
        e: &Element,
        expected: &str,
    ) -> Result<(bool, String)> {
        self.of(id)?.element_text_matches(id, e, expected).await
    }
    async fn is_element_visible(&self, id: &str, e: &Element) -> Result<bool> {
        self.of(id)?.is_element_visible(id, e).await
    }
    async fn navigate(&self, id: &str, url: &str) -> Result<()> {
        self.of(id)?.navigate(id, url).await
    }
    async fn current_url(&self, id: &str) -> Option<String> {
        self.of(id).ok()?.current_url(id).await
    }
    async fn go_back(&self, id: &str) -> Result<()> {
        self.of(id)?.go_back(id).await
    }
    async fn refresh_page(&self, id: &str) -> Result<()> {
        self.of(id)?.refresh_page(id).await
    }
    async fn collect_resource_metrics(
        &self,
        id: &str,
        package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        self.of(id)?.collect_resource_metrics(id, package).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::no_device::NoDevice;

    #[tokio::test]
    async fn devices_route_by_platform_and_the_web_is_always_offered() {
        let r = RoutingDriver::new(vec![
            (
                Platform::Android,
                Arc::new(NoDevice) as Arc<dyn PlatformDriver>,
            ),
            (Platform::Web, Arc::new(NoDevice)),
        ]);
        let list = r.list_devices().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "web");
        assert!(r.of("emulator-5554").is_ok());
        assert!(r.of("web").is_ok());
        assert!(r.of("web-2").is_ok());
        // No iOS driver: an iOS id is an unknown device, not a panic.
        assert!(matches!(
            r.of("BD7E26D9-B9DB-4A6D-86D1-33D0B83A7A20"),
            Err(VelocityError::DeviceNotFound { .. })
        ));
    }

    /// A method added to the trait and not forwarded here would silently
    /// take the trait default for every platform.
    #[test]
    fn every_trait_method_is_forwarded() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |p: &Path| {
            std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        };
        let traits = read(&root.join("../velocity-common/src/traits.rs"));
        let routing = read(&root.join("src/console/routing.rs"));
        let routing = &routing[..routing.find("#[cfg(test)]").unwrap()];

        let body = traits
            .split_once("pub trait PlatformDriver")
            .expect("PlatformDriver trait in traits.rs")
            .1;
        let body = &body[..body.find("\n}").expect("end of the trait")];
        let methods: Vec<&str> = body
            .lines()
            .filter_map(|l| {
                let l = l.trim_start();
                let l = l.strip_prefix("async ").unwrap_or(l);
                let name = l.strip_prefix("fn ")?;
                Some(&name[..name.find(['(', '<'])?])
            })
            .collect();
        assert!(methods.len() > 50, "parsed only {methods:?}");
        let missing: Vec<_> = methods
            .iter()
            .filter(|m| !routing.contains(&format!("fn {m}(")))
            .collect();
        assert!(
            missing.is_empty(),
            "RoutingDriver doesn't forward {missing:?}"
        );
    }
}
