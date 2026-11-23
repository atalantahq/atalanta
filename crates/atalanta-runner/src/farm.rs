use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use atalanta_common::{DeviceInfo, DeviceState, PlatformDriver, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

/// A pooled device that can be leased for test execution.
#[derive(Debug, Clone)]
pub struct PooledDevice {
    pub info: DeviceInfo,
    pub in_use: bool,
    pub health: Health,
}

/// Where a device stands. One value per device: health used to live in four
/// separate maps that a vanished device was never removed from, so
/// "devices − quarantined" could go negative and stall the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Leasable; `infra_streak` consecutive infrastructure failures so far.
    Healthy { infra_streak: u32 },
    /// Quarantined, and re-probed with backoff until it's ready again.
    Probation {
        failed_probes: u32,
        next_probe: Instant,
    },
    /// Out for the rest of the run (failed every probe, or can't run the app).
    Retired,
}

impl Health {
    fn leasable(&self) -> bool {
        matches!(self, Self::Healthy { .. })
    }
}

/// A lease on a device from the farm. Automatically returns the device when dropped.
pub struct Lease {
    device_id: String,
    farm: Arc<Mutex<FarmState>>,
    _permit: OwnedSemaphorePermit,
    /// Cross-process lease (see `lease.rs`); released with this lease.
    _lock: Option<crate::lease::DeviceLock>,
}

impl Lease {
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

impl Drop for Lease {
    /// Return the device synchronously, before the semaphore permit (a field)
    /// is released — otherwise a waiter could take the permit, find every
    /// device still marked in use, and fail.
    fn drop(&mut self) {
        let mut state = self.farm.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(device) = state.devices.get_mut(&self.device_id) {
            device.in_use = false;
            debug!(device = %self.device_id, "device returned to pool");
        }
    }
}

struct FarmState {
    devices: HashMap<String, PooledDevice>,
    /// Permits the semaphore currently hands out; kept equal to the number of
    /// leasable devices so a permit always comes with a free device.
    permits: usize,
}

impl FarmState {
    fn new(permits: usize) -> Self {
        Self {
            devices: HashMap::new(),
            permits,
        }
    }

    fn healthy(&self) -> usize {
        self.devices
            .values()
            .filter(|d| d.health.leasable())
            .count()
    }

    fn on_probation(&self) -> usize {
        self.devices
            .values()
            .filter(|d| matches!(d.health, Health::Probation { .. }))
            .count()
    }
}

/// Consecutive infrastructure failures before a device is quarantined.
const QUARANTINE_AFTER: u32 = 2;
/// Failed readiness probes before a quarantined device is given up on.
const MAX_PROBES: u32 = 6;

/// Wait before probe `n` (10, 20, 40, 60, 60, … s).
fn probe_backoff(failed_probes: u32) -> std::time::Duration {
    std::time::Duration::from_secs((10u64 << failed_probes.min(3)).min(60))
}

/// Manages a pool of devices for parallel test execution.
///
/// Devices are discovered from the platform driver on `refresh()`, and can be
/// leased exclusively via `acquire()`. When the `Lease` is dropped, the device
/// is automatically returned to the pool.
///
/// v2: Supports `--workers N` where N can exceed device count (workers queue for devices),
/// and `--device-filter` to target specific devices by name/ID pattern.
pub struct DeviceFarm {
    driver: Arc<dyn PlatformDriver>,
    state: Arc<Mutex<FarmState>>,
    semaphore: Arc<Semaphore>,
    max_devices: usize,
    device_filter: Option<String>,
}

impl DeviceFarm {
    pub fn new(driver: Arc<dyn PlatformDriver>, max_devices: usize) -> Self {
        Self {
            driver,
            state: Arc::new(Mutex::new(FarmState::new(max_devices))),
            semaphore: Arc::new(Semaphore::new(max_devices)),
            max_devices,
            device_filter: None,
        }
    }

    /// Create a farm with a device filter pattern (glob-style: "Pixel*", "*iPhone*").
    pub fn with_filter(
        driver: Arc<dyn PlatformDriver>,
        max_devices: usize,
        filter: Option<String>,
    ) -> Self {
        Self {
            driver,
            state: Arc::new(Mutex::new(FarmState::new(max_devices))),
            semaphore: Arc::new(Semaphore::new(max_devices)),
            max_devices,
            device_filter: filter,
        }
    }

    /// Discover available devices from the driver and populate the pool.
    pub async fn refresh(&self) -> Result<usize> {
        let devices = self.driver.list_devices().await?;
        let mut state = self.lock();

        let mut booted: Vec<DeviceInfo> = devices
            .into_iter()
            .filter(|d| d.state == DeviceState::Booted)
            .collect();

        // Apply device filter if set
        if let Some(ref pattern) = self.device_filter {
            booted.retain(|d| device_filter_matches(pattern, d));
        }

        let booted: Vec<DeviceInfo> = booted.into_iter().take(self.max_devices).collect();

        let count = booted.len();

        // Devices still present keep their lease and health; vanished ones
        // drop out with theirs.
        let mut new_devices = HashMap::new();
        for info in booted {
            let (in_use, health) = state
                .devices
                .get(&info.id)
                .map(|d| (d.in_use, d.health))
                .unwrap_or((false, Health::Healthy { infra_streak: 0 }));
            new_devices.insert(
                info.id.clone(),
                PooledDevice {
                    info,
                    in_use,
                    health,
                },
            );
        }

        state.devices = new_devices;
        let leasable = state.healthy();
        self.set_permits(&mut state, leasable.min(self.max_devices));
        debug!(count, "device pool refreshed");
        Ok(count)
    }

    /// Acquire a lease on an available device. Blocks until a device is free.
    /// A device another process holds (see `lease.rs`) is skipped; if every
    /// free device is held elsewhere this waits, up to 5 minutes.
    pub async fn acquire(&self) -> Result<Lease> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        loop {
            // A closed pool has no device to give: a device error (exit 3),
            // not a configuration one (VEL-051).
            let permit = self.semaphore.clone().acquire_owned().await.map_err(|_| {
                crate::runner::device_unavailable(
                    "Device pool",
                    "closed; no device left to lease".to_string(),
                )
            })?;

            let picked = {
                let mut state = self.lock();
                let mut picked = None;
                for (id, d) in state.devices.iter_mut() {
                    if d.in_use || !d.health.leasable() {
                        continue;
                    }
                    // A web device is a browser this process starts for
                    // itself: another Atalanta process's `web-0` is a
                    // different Chrome, so no cross-process lease (two
                    // concurrent `--parallel` web runs waited 5 minutes on
                    // each other's names, review-d6 m6).
                    let lock = if d.info.platform == atalanta_common::Platform::Web {
                        Ok(crate::lease::DeviceLock::unleased())
                    } else {
                        crate::lease::try_lock(id)
                    };
                    match lock {
                        Ok(lock) => {
                            d.in_use = true;
                            picked = Some((id.clone(), lock));
                            break;
                        }
                        Err(crate::lease::LeaseError::Held(holder)) => {
                            debug!(device = %id, %holder, "device leased by another process")
                        }
                        // Waiting won't make the lease dir usable.
                        Err(crate::lease::LeaseError::Io(e)) => {
                            return Err(crate::runner::device_unavailable(
                                &format!("Device {id}"),
                                e,
                            ));
                        }
                    }
                }
                picked
            };

            if let Some((id, lock)) = picked {
                debug!(device = %id, "device leased");
                return Ok(Lease {
                    device_id: id,
                    farm: self.state.clone(),
                    _permit: permit,
                    _lock: Some(lock),
                });
            }
            drop(permit);
            if std::time::Instant::now() >= deadline {
                return Err(crate::runner::device_unavailable(
                    "No device",
                    "all free devices are leased by other Atalanta processes".to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Number of devices currently available (not leased).
    pub async fn available_count(&self) -> usize {
        let state = self.lock();
        state
            .devices
            .values()
            .filter(|d| !d.in_use && d.health.leasable())
            .count()
    }

    /// Total number of booted devices in the pool.
    pub async fn total_count(&self) -> usize {
        self.lock().devices.len()
    }

    /// Devices not quarantined.
    pub fn healthy_count(&self) -> usize {
        self.lock().healthy()
    }

    /// Ids of the devices not quarantined or retired.
    pub fn healthy_ids(&self) -> Vec<String> {
        self.lock()
            .devices
            .iter()
            .filter(|(_, d)| d.health.leasable())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `device_id`'s health, if it's in the pool.
    pub fn health(&self, device_id: &str) -> Option<Health> {
        self.lock().devices.get(device_id).map(|d| d.health)
    }

    /// Record how a test went on `device_id`. After `QUARANTINE_AFTER`
    /// consecutive infrastructure failures the device leaves the pool, so one
    /// sick emulator can't fail the rest of the suite (flaky failures cluster
    /// on shared causes like a device — Parry et al. 2025). Returns true if
    /// this call quarantined it.
    pub fn record_outcome(&self, device_id: &str, infrastructure_failure: bool) -> bool {
        let mut state = self.lock();
        let Some(device) = state.devices.get_mut(device_id) else {
            return false;
        };
        let Health::Healthy { infra_streak } = &mut device.health else {
            return false;
        };
        if !infrastructure_failure {
            *infra_streak = 0;
            return false;
        }
        *infra_streak += 1;
        if *infra_streak < QUARANTINE_AFTER {
            return false;
        }
        device.health = Health::Probation {
            failed_probes: 0,
            next_probe: Instant::now() + probe_backoff(0),
        };
        warn!(device = %device_id, "device quarantined after repeated infrastructure failures");
        let healthy = state.healthy();
        self.set_permits(&mut state, healthy.min(self.max_devices));
        true
    }

    /// Take `device_id` out of the pool for the rest of the run, with no
    /// readiness probes to bring it back: it can't run this build at all
    /// (the app won't install there). Once nothing is left, waiters are
    /// released with an error instead of hanging.
    pub fn retire(&self, device_id: &str) {
        let mut state = self.lock();
        if let Some(d) = state.devices.get_mut(device_id) {
            d.health = Health::Retired;
        }
        let healthy = state.healthy();
        self.set_permits(&mut state, healthy.min(self.max_devices));
        if healthy == 0 && state.on_probation() == 0 {
            self.semaphore.close();
        }
    }

    /// Re-check quarantined devices whose probe is due; return a ready one to
    /// the pool (probation, not a life sentence). A device that fails
    /// `MAX_PROBES` probes is retired. When nothing healthy remains and no
    /// device is on probation, waiters are released with an error instead of
    /// hanging. Returns the ids that came back.
    pub async fn probe_quarantined(&self) -> Vec<String> {
        let due: Vec<String> = {
            let state = self.lock();
            let now = Instant::now();
            state
                .devices
                .iter()
                .filter(|(_, d)| matches!(d.health, Health::Probation { next_probe, .. } if next_probe <= now))
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut back = Vec::new();
        for id in due {
            let ready = self.driver.device_ready(&id).await;
            let mut state = self.lock();
            let Some(device) = state.devices.get_mut(&id) else {
                continue;
            };
            if ready {
                device.health = Health::Healthy { infra_streak: 0 };
                let healthy = state.healthy();
                self.set_permits(&mut state, healthy.min(self.max_devices));
                warn!(device = %id, "device passed its readiness probe; back in the pool");
                back.push(id);
            } else if let Health::Probation { failed_probes, .. } = device.health {
                let failed = failed_probes + 1;
                device.health = if failed >= MAX_PROBES {
                    warn!(device = %id, "device failed {MAX_PROBES} readiness probes; leaving it out");
                    Health::Retired
                } else {
                    Health::Probation {
                        failed_probes: failed,
                        next_probe: Instant::now() + probe_backoff(failed),
                    }
                };
            }
        }
        let state = self.lock();
        if state.healthy() == 0 && state.on_probation() == 0 && !state.devices.is_empty() {
            self.semaphore.close();
        }
        back
    }

    /// Make the next probe of every device on probation due now (tests, and
    /// `atalanta devices` diagnostics).
    pub fn probe_now(&self) {
        let mut state = self.lock();
        for d in state.devices.values_mut() {
            if let Health::Probation { next_probe, .. } = &mut d.health {
                *next_probe = Instant::now();
            }
        }
    }

    fn set_permits(&self, state: &mut FarmState, target: usize) {
        if target < state.permits {
            // Permits held by running leases are forgotten as they return.
            let forgotten = self.semaphore.forget_permits(state.permits - target);
            state.permits -= forgotten;
            if state.permits > target {
                let excess = state.permits - target;
                let semaphore = self.semaphore.clone();
                // Forget the rest once they're released.
                tokio::spawn(async move {
                    if let Ok(p) = semaphore.acquire_many_owned(excess as u32).await {
                        p.forget();
                    }
                });
                state.permits = target;
            }
        } else if target > state.permits {
            self.semaphore.add_permits(target - state.permits);
            state.permits = target;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FarmState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The parts of a `--device` filter: one pattern, or a list of ids.
fn filter_parts(filter: &str) -> Vec<String> {
    filter
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether device `d` passes the `--device` filter, ignoring case: a list
/// (`simA,simB`) names exact ids; a single value is a `*` glob or else a
/// substring, of the name or the id (`Pixel` is every Pixel). The farm and
/// the CLI's up-front `--device` check share this rule.
pub fn device_filter_matches(filter: &str, d: &DeviceInfo) -> bool {
    let parts = filter_parts(filter);
    let id = d.id.to_lowercase();
    if parts.len() > 1 {
        return parts.contains(&id);
    }
    let Some(pat) = parts.first() else {
        return true;
    };
    let name = d.name.to_lowercase();
    if pat.contains('*') {
        glob_match(pat, &name) || glob_match(pat, &id)
    } else {
        name.contains(pat.as_str()) || id.contains(pat.as_str())
    }
}

/// The first part of a `--device` filter that no device in `devices`
/// matches under [`device_filter_matches`], if any. Parts `skip` accepts
/// aren't checked (a remote adb id the listing can't see).
pub fn unmatched_device_filter(
    filter: &str,
    devices: &[DeviceInfo],
    skip: impl Fn(&str) -> bool,
) -> Option<String> {
    let parts = filter_parts(filter);
    if parts.len() > 1 {
        return parts
            .into_iter()
            .find(|p| !skip(p) && !devices.iter().any(|d| d.id.to_lowercase() == *p));
    }
    let pat = parts.into_iter().next()?;
    if skip(&pat) || devices.iter().any(|d| device_filter_matches(&pat, d)) {
        return None;
    }
    Some(filter.trim().to_string())
}

/// `*` wildcard match over the whole text (both sides already lower case).
/// The shared matcher: a hand-rolled one here took the first occurrence of
/// the pattern's last part, so `emulator-*5` never matched `emulator-5555`.
fn glob_match(pattern: &str, text: &str) -> bool {
    atalanta_common::glob_match(pattern, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            platform: atalanta_common::Platform::Ios,
            state: DeviceState::Booted,
            os_version: None,
            device_type: Default::default(),
        }
    }

    // Review d4 M2: the CLI's up-front check and the farm share one rule.
    #[test]
    fn the_device_filter_rule_is_the_farms() {
        let devs = vec![
            dev("290403AA-1111", "atalanta iPhone 17 Pro 1"),
            dev("A1B2C3D4-2222", "atalanta iPhone 17 Pro 2"),
            dev("emulator-5554", "Pixel_7"),
        ];
        let none = |_: &str| false;
        // A single value: a substring of a name or id, any case.
        assert!(unmatched_device_filter("pixel", &devs, none).is_none());
        assert!(unmatched_device_filter("iPhone 17 Pro", &devs, none).is_none());
        assert!(unmatched_device_filter("*5554", &devs, none).is_none());
        assert!(device_filter_matches("PIXEL", &devs[2]));
        // A list: exact ids, any case.
        assert!(unmatched_device_filter("290403aa-1111,a1b2c3d4-2222", &devs, none).is_none());
        assert_eq!(
            unmatched_device_filter("290403aa-1111,Pixel_7", &devs, none).as_deref(),
            Some("pixel_7")
        );
        assert_eq!(
            unmatched_device_filter("Galaxy", &devs, none).as_deref(),
            Some("Galaxy")
        );
        // Skipped parts aren't checked.
        let remote = |p: &str| p.contains('/');
        assert!(unmatched_device_filter("10.0.0.5:5037/emulator-5556", &devs, remote).is_none());
    }
    // V1: the glob took the first occurrence of its last part, so a pattern
    // whose last part also occurs earlier in the id never matched.
    #[test]
    fn a_device_glob_matches_when_its_last_part_also_occurs_earlier() {
        let d = dev("emulator-5555", "Pixel_7");
        assert!(device_filter_matches("emulator-*5", &d));
        assert!(device_filter_matches("emulator-5*5", &d));
        assert!(!device_filter_matches("emulator-*4", &d));
        assert!(device_filter_matches("*_7", &dev("x", "Pixel_7_7")));
    }

    use atalanta_common::{AtalantaError, Platform};

    struct MockDriver {
        devices: Vec<DeviceInfo>,
        /// What `device_ready` answers (readiness probes).
        ready: std::sync::atomic::AtomicBool,
    }

    impl MockDriver {
        fn new(devices: Vec<DeviceInfo>) -> Self {
            Self {
                devices,
                ready: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for MockDriver {
        async fn device_ready(&self, _: &str) -> bool {
            self.ready.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(self.devices.clone())
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(
            &self,
            _: &str,
            _: &atalanta_common::Selector,
        ) -> Result<atalanta_common::Element> {
            Err(AtalantaError::ElementNotFound {
                selector: "mock".to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
        }
        async fn find_elements(
            &self,
            _: &str,
            _: &atalanta_common::Selector,
        ) -> Result<Vec<atalanta_common::Element>> {
            Ok(vec![])
        }
        async fn get_hierarchy(&self, _: &str) -> Result<atalanta_common::Element> {
            Err(AtalantaError::ElementNotFound {
                selector: "mock".to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
        }
        async fn tap(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn double_tap(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn long_press(&self, _: &str, _: &atalanta_common::Element, _: u64) -> Result<()> {
            Ok(())
        }
        async fn input_text(&self, _: &str, _: &atalanta_common::Element, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clear_text(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn swipe(&self, _: &str, _: atalanta_common::Direction) -> Result<()> {
            Ok(())
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str, _: atalanta_common::Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1080, 2400))
        }
        async fn get_element_text(&self, _: &str, _: &atalanta_common::Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &atalanta_common::Element) -> Result<bool> {
            Ok(false)
        }
    }

    fn make_devices(count: usize) -> Vec<DeviceInfo> {
        (0..count)
            .map(|i| DeviceInfo {
                id: format!("device-{i}"),
                name: format!("Device {i}"),
                platform: Platform::Android,
                state: DeviceState::Booted,
                os_version: None,
                device_type: atalanta_common::DeviceType::Emulator,
            })
            .collect()
    }

    /// review-d6 m6: web devices are this process's own browsers; a
    /// cross-process lease on the same name (another run's `web-0`) must
    /// not block them.
    #[tokio::test]
    async fn web_devices_are_not_leased_across_processes() {
        let id = format!("web-fz-lease-{}", std::process::id());
        let mut devices = make_devices(1);
        devices[0].id = id.clone();
        devices[0].platform = Platform::Web;
        // Another process holding the same name.
        let held = crate::lease::try_lock(&id).unwrap();
        let farm = DeviceFarm::new(Arc::new(MockDriver::new(devices)), 1);
        farm.refresh().await.unwrap();
        let lease = tokio::time::timeout(std::time::Duration::from_secs(3), farm.acquire())
            .await
            .expect("a web device waited on another process's lease")
            .unwrap();
        assert_eq!(lease.device_id(), id);
        drop(held);
    }

    #[tokio::test]
    async fn test_refresh_populates_pool() {
        let driver = Arc::new(MockDriver::new(make_devices(3)));
        let farm = DeviceFarm::new(driver, 10);
        let count = farm.refresh().await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(farm.total_count().await, 3);
        assert_eq!(farm.available_count().await, 3);
    }

    #[tokio::test]
    async fn test_acquire_and_drop_returns_device() {
        let driver = Arc::new(MockDriver::new(make_devices(2)));
        let farm = DeviceFarm::new(driver, 2);
        farm.refresh().await.unwrap();

        let lease = farm.acquire().await.unwrap();
        assert_eq!(farm.available_count().await, 1);

        drop(lease);
        // Give the spawned drop task a moment to run
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(farm.available_count().await, 2);
    }

    #[tokio::test]
    async fn test_max_devices_cap() {
        let driver = Arc::new(MockDriver::new(make_devices(10)));
        let farm = DeviceFarm::new(driver, 3);
        farm.refresh().await.unwrap();
        assert_eq!(farm.total_count().await, 3);
    }

    #[tokio::test]
    async fn a_device_list_limits_the_pool_to_exactly_those() {
        let driver = Arc::new(MockDriver::new(vec![
            booted("sim-a"),
            booted("sim-b"),
            booted("tablet"),
        ]));
        let farm = DeviceFarm::with_filter(driver.clone(), 3, Some("sim-a, sim-b".into()));
        assert_eq!(farm.refresh().await.unwrap(), 2);
        let one = DeviceFarm::with_filter(driver, 3, Some("tablet".into()));
        assert_eq!(one.refresh().await.unwrap(), 1);
    }

    fn booted(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            platform: Platform::Android,
            state: DeviceState::Booted,
            os_version: None,
            device_type: Default::default(),
        }
    }

    #[tokio::test]
    async fn released_device_is_immediately_leasable() {
        let farm = DeviceFarm::new(Arc::new(MockDriver::new(vec![booted("a")])), 1);
        farm.refresh().await.unwrap();
        for _ in 0..50 {
            let lease = farm.acquire().await.unwrap();
            drop(lease);
        }
    }

    #[tokio::test]
    async fn repeated_infra_failures_quarantine_a_device() {
        let farm = DeviceFarm::new(Arc::new(MockDriver::new(vec![booted("a"), booted("b")])), 2);
        farm.refresh().await.unwrap();
        assert!(!farm.record_outcome("a", true));
        assert!(
            !farm.record_outcome("a", false),
            "a success resets the streak"
        );
        assert!(!farm.record_outcome("a", true));
        assert!(
            farm.record_outcome("a", true),
            "second in a row quarantines"
        );
        assert_eq!(farm.healthy_count(), 1);
        for _ in 0..5 {
            let lease = farm.acquire().await.unwrap();
            assert_eq!(lease.device_id(), "b");
        }
    }

    #[tokio::test]
    async fn quarantined_device_returns_after_passing_its_probe() {
        let farm = DeviceFarm::new(Arc::new(MockDriver::new(vec![booted("a"), booted("b")])), 2);
        farm.refresh().await.unwrap();
        farm.record_outcome("a", true);
        assert!(farm.record_outcome("a", true));
        assert_eq!(farm.healthy_count(), 1);
        farm.probe_now();
        assert_eq!(farm.probe_quarantined().await, vec!["a".to_string()]);
        assert_eq!(farm.healthy_count(), 2);
    }

    #[tokio::test]
    async fn a_device_that_never_recovers_fails_acquire_instead_of_hanging() {
        let driver = Arc::new(MockDriver::new(vec![booted("a")]));
        driver
            .ready
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let farm = DeviceFarm::new(driver, 1);
        farm.refresh().await.unwrap();
        farm.record_outcome("a", true);
        farm.record_outcome("a", true);
        for _ in 0..MAX_PROBES {
            farm.probe_now();
            assert!(farm.probe_quarantined().await.is_empty());
        }
        let r = tokio::time::timeout(std::time::Duration::from_secs(1), farm.acquire()).await;
        assert!(matches!(r, Ok(Err(_))), "acquire must error, not hang");
    }

    struct ShrinkingDriver {
        devices: std::sync::Mutex<Vec<DeviceInfo>>,
    }

    #[async_trait::async_trait]
    impl PlatformDriver for ShrinkingDriver {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(self.devices.lock().unwrap().clone())
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(
            &self,
            _: &str,
            _: &atalanta_common::Selector,
        ) -> Result<atalanta_common::Element> {
            Err(AtalantaError::Config("unused".into()))
        }
        async fn find_elements(
            &self,
            _: &str,
            _: &atalanta_common::Selector,
        ) -> Result<Vec<atalanta_common::Element>> {
            Ok(vec![])
        }
        async fn get_hierarchy(&self, _: &str) -> Result<atalanta_common::Element> {
            Err(AtalantaError::Config("unused".into()))
        }
        async fn tap(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn double_tap(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn long_press(&self, _: &str, _: &atalanta_common::Element, _: u64) -> Result<()> {
            Ok(())
        }
        async fn input_text(&self, _: &str, _: &atalanta_common::Element, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clear_text(&self, _: &str, _: &atalanta_common::Element) -> Result<()> {
            Ok(())
        }
        async fn swipe(&self, _: &str, _: atalanta_common::Direction) -> Result<()> {
            Ok(())
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str, _: atalanta_common::Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1080, 2400))
        }
        async fn get_element_text(&self, _: &str, _: &atalanta_common::Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &atalanta_common::Element) -> Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_quarantined_device_that_disappears_takes_its_health_with_it() {
        let driver = Arc::new(ShrinkingDriver {
            devices: std::sync::Mutex::new(vec![booted("farm-gone-a"), booted("farm-gone-b")]),
        });
        let farm = DeviceFarm::new(driver.clone(), 2);
        farm.refresh().await.unwrap();
        farm.record_outcome("farm-gone-a", true);
        assert!(farm.record_outcome("farm-gone-a", true), "quarantined");
        // The quarantined device is unplugged.
        driver
            .devices
            .lock()
            .unwrap()
            .retain(|d| d.id != "farm-gone-a");
        farm.refresh().await.unwrap();
        // Used to be 1 - 1 - … underflow; now just the one healthy device.
        assert_eq!(farm.healthy_count(), 1);
        assert_eq!(farm.health("farm-gone-a"), None);
        let lease = farm.acquire().await.unwrap();
        assert_eq!(lease.device_id(), "farm-gone-b");
    }
}
