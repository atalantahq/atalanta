use crate::error::Result;
use crate::types::{DeviceInfo, Direction, Element, Key, LaunchArgs, Selector};
use crate::VelocityError;

/// Health status of a platform driver connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// Whether the app under test is running and in front
/// ([`PlatformDriver::app_liveness`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// Running, and the app in front.
    Foreground,
    /// Running or not, another app is in front (its id, when known).
    Background(Option<String>),
    /// It died since its last launch: what the crash log says.
    Dead(String),
    /// The driver can't tell.
    Unknown,
}

/// The core trait that platform drivers must implement.
/// Each platform (iOS, Android) provides its own implementation.
#[async_trait::async_trait]
pub trait PlatformDriver: Send + Sync {
    // Platform bootstrap — override to auto-start required services (e.g. WDA on iOS)
    async fn prepare(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }
    async fn cleanup(&self) {}

    // Health check for connection validation
    async fn health_check(&self) -> HealthStatus {
        HealthStatus::Healthy
    }

    // Explicit session restart for recovery
    async fn restart_session(&self) -> Result<()> {
        Ok(())
    }

    // Device management
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>>;
    /// [`list_devices`](Self::list_devices), but an error when part of the
    /// listing failed instead of leaving those devices out (iOS: a failed
    /// `devicectl list` hides every real device). For checks that reject
    /// a `--device` they can't see: an error means "don't know".
    async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        self.list_devices().await
    }
    async fn boot_device(&self, device_id: &str) -> Result<()>;
    async fn shutdown_device(&self, device_id: &str) -> Result<()>;

    // App lifecycle
    async fn install_app(&self, device_id: &str, app_path: &str) -> Result<()>;
    /// Install the build at `app_path` as `app_id`, unless the build whose
    /// hash is `hash` (see [`crate::install::hash_app`]) is already
    /// installed there and the installed copy hasn't changed since. Drivers
    /// that can remember what they installed override this; the default
    /// always installs.
    async fn install_app_if_changed(
        &self,
        device_id: &str,
        app_path: &str,
        _app_id: &str,
        _hash: &str,
    ) -> Result<crate::InstallOutcome> {
        self.install_app(device_id, app_path).await?;
        Ok(crate::InstallOutcome::Installed)
    }
    async fn launch_app(&self, device_id: &str, app_id: &str, clear_state: bool) -> Result<()>;
    /// Launch `app_id` on `device_id` with launch arguments (AUT-024).
    /// Empty args fall through to [`Self::launch_app`]; non-empty args that a
    /// platform doesn't yet support return a configuration error. Android and
    /// iOS support lands in part 2.
    async fn launch_app_with(
        &self,
        device_id: &str,
        app_id: &str,
        clear_state: bool,
        args: &LaunchArgs,
    ) -> Result<()> {
        if args.is_empty() {
            self.launch_app(device_id, app_id, clear_state).await
        } else {
            Err(VelocityError::Config(
                "launch arguments are not supported on this platform yet".into(),
            ))
        }
    }
    async fn stop_app(&self, device_id: &str, app_id: &str) -> Result<()>;

    // Element queries
    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element>;
    async fn find_elements(&self, device_id: &str, selector: &Selector) -> Result<Vec<Element>>;
    async fn get_hierarchy(&self, device_id: &str) -> Result<Element>;
    /// Returns the hierarchy using the driver's internal cache if still valid.
    /// Platforms with hierarchy caching override this; the default falls back to a fresh dump.
    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        self.get_hierarchy(device_id).await
    }
    /// A tree for change detection only (settle polling): drivers may leave
    /// out attributes that never matter for "did the screen change", and
    /// must not cache it for lookups.
    async fn get_hierarchy_for_compare(&self, device_id: &str) -> Result<Element> {
        self.get_hierarchy(device_id).await
    }

    /// Wait on the device itself for the screen to settle after the last
    /// action (Android: from accessibility events, no tree polling).
    /// `None` when the driver can't; the sync engine then polls trees.
    async fn settle_on_device(
        &self,
        _device_id: &str,
        _reaction_ms: u64,
        _quiet_ms: u64,
        _cap_ms: u64,
    ) -> Option<crate::types::DeviceSettle> {
        None
    }

    /// Something that changes whenever the installed build of `app_id`
    /// changes (version, install time). `None` when unknown; the result
    /// cache then never skips the app's tests.
    /// The exception that crashed `app_id` since its last launch, if it did.
    async fn crash_report(&self, _device_id: &str, _app_id: &str) -> Option<String> {
        None
    }

    /// I3 (D-50, sticky crash): a crash of `app_id` since its last launch
    /// that the driver has already seen on its own (a physical iPhone's
    /// crash watcher, an error naming the crash), with what it knows of
    /// it. Must not ask the device: the executor calls it before every
    /// step and on every attempt of an element wait, and fails at once
    /// while it says `Some`. Cleared by the next launch or `stop_app`.
    async fn pending_crash(&self, _device_id: &str, _app_id: &str) -> Option<String> {
        None
    }

    /// D-50: copy the crash reports for `app_id` since its last launch into
    /// the directory `dest` (created when there is something to copy), and
    /// return the files written. Drivers that can't read the device's
    /// crash logs return none.
    async fn copy_crash_logs(
        &self,
        _device_id: &str,
        _app_id: &str,
        _dest: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    /// Start capturing a test's video and device logs ([`crate::evidence`]).
    /// Called at the start of each test; `None` when the driver captures
    /// nothing (or `req` asks for nothing). Must not block the test: slow
    /// setup runs in the background.
    async fn start_evidence(
        &self,
        _device_id: &str,
        _req: &crate::evidence::EvidenceRequest,
    ) -> Option<Box<dyn crate::evidence::EvidenceSession>> {
        None
    }

    /// The host port that reaches `device_id`'s in-app sync probe, when the
    /// driver gives each device its own (an `adb forward tcp:0` per device).
    /// `None`: the suite's `sync.nativePort*` setting.
    fn sync_probe_port(&self, _device_id: &str) -> Option<u16> {
        None
    }

    /// Whether `app_id` is alive and in front. Drivers that know the front
    /// app override this; the default only knows a crash ([`Self::crash_report`]).
    async fn app_liveness(&self, device_id: &str, app_id: &str) -> Liveness {
        match self.crash_report(device_id, app_id).await {
            Some(detail) => Liveness::Dead(detail),
            None => Liveness::Unknown,
        }
    }

    /// Where the system draws over the app (status bar, navigation bar), in
    /// the tree's coordinates, so a screenshot comparison can leave out the
    /// clock and battery. `Some(vec![])`: nothing to leave out (a browser
    /// viewport, a headless render). `None`: the driver can't tell.
    async fn system_bars(&self, _device_id: &str) -> Option<Vec<crate::types::Rect>> {
        None
    }

    /// Freeze what the status bar shows (clock, battery, signal,
    /// notifications) so screenshot comparisons can include it
    /// (`screenshots.freezeStatusBar`, D-V4): Android demo mode, iOS
    /// `simctl status_bar override`. Undone by [`Self::unfreeze_status_bar`]
    /// and by `cleanup`. Best effort: `false` when the driver can't (the
    /// bar then stays masked).
    async fn freeze_status_bar(&self, _device_id: &str) -> bool {
        false
    }

    /// Undo every [`Self::freeze_status_bar`] (the end of a run, also
    /// under `velocity daemon`, which keeps the driver: review-d5 m-4). The
    /// next freeze enters again.
    async fn unfreeze_status_bar(&self) {}

    async fn app_fingerprint(&self, _device_id: &str, _app_id: &str) -> Option<String> {
        None
    }

    /// Save `app_id`'s data on `device_id` as `name` (login, caches,
    /// preferences), for [`Self::restore_app_state`]. Leaves the app stopped.
    async fn save_app_state(&self, _device_id: &str, _app_id: &str, _name: &str) -> Result<()> {
        Err(crate::VelocityError::Unsupported(
            "saveState isn't supported on this platform".into(),
        ))
    }

    /// Put back the data saved as `name`. Leaves the app stopped.
    async fn restore_app_state(&self, _device_id: &str, _app_id: &str, _name: &str) -> Result<()> {
        Err(crate::VelocityError::Unsupported(
            "restoreState isn't supported on this platform".into(),
        ))
    }

    /// How many on-screen elements the last successful
    /// [`Self::find_element`] of `selector` on `device_id` matched, when the
    /// driver counted them in that same lookup (D-6 / R5): the executor's
    /// ambiguity check then needs no lookup of its own. `None`: not counted.
    fn last_match_count(&self, _device_id: &str, _selector: &Selector) -> Option<usize> {
        None
    }

    /// Whether [`Self::get_hierarchy_cached`] is served from a cache, so an
    /// extra read right after a lookup is free.
    fn has_hierarchy_cache(&self) -> bool {
        false
    }
    /// Whether `device_id` is ready to run tests: booted, and its services
    /// answering. Used to admit devices and to let a quarantined one back in.
    async fn device_ready(&self, _device_id: &str) -> bool {
        matches!(self.health_check().await, HealthStatus::Healthy)
    }

    /// Drop any cached hierarchy so the next lookup sees the live screen. Called
    /// while polling for an element; drivers without a cache needn't override.
    async fn invalidate_hierarchy_cache(&self) {}

    /// Start (`on = true`) or stop loading the device's CPU, for flaky
    /// hunting: starved devices expose timing-dependent tests that plain
    /// reruns miss. Returns false when the platform can't do it.
    async fn set_stress(&self, _device_id: &str, _on: bool) -> bool {
        false
    }

    /// Reset `app_id` with a strategy stronger than `clear` (see
    /// [`crate::ResetStrategy`]). Returns which strategy actually ran; an
    /// error means none could, and the caller falls back to a plain clear.
    async fn reset_app(
        &self,
        _device_id: &str,
        _app_id: &str,
        _reset: &crate::test_types::ResetConfig,
    ) -> Result<crate::test_types::ResetStrategy> {
        Err(VelocityError::Unsupported(
            "this platform only supports the clear reset strategy".into(),
        ))
    }

    // Actions
    async fn tap(&self, device_id: &str, element: &Element) -> Result<()>;
    async fn double_tap(&self, device_id: &str, element: &Element) -> Result<()>;
    async fn long_press(&self, device_id: &str, element: &Element, duration_ms: u64) -> Result<()>;
    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()>;
    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()>;
    async fn swipe(&self, device_id: &str, direction: Direction) -> Result<()>;
    async fn swipe_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()>;

    /// `drag` (row 3.1): press at `from`, hold `hold_ms` (a long press, so a
    /// list picks the row up), move to `to` over `move_ms`, rest there so the
    /// list drops it in place instead of flinging, and lift.
    async fn drag(
        &self,
        _device_id: &str,
        _from: (i32, i32),
        _to: (i32, i32),
        _hold_ms: u64,
        _move_ms: u64,
    ) -> Result<()> {
        Err(VelocityError::Unsupported(
            "drag isn't supported on this platform (Android, iOS and the web are)".into(),
        ))
    }

    /// Whether the app has requests in flight that the settle should wait
    /// for (web: `web.networkIdle`). Drivers that can't tell say no.
    async fn network_busy(&self, _device_id: &str) -> bool {
        false
    }

    /// Answer requests matching `rule` until [`Self::clear_mocks`] (web).
    async fn add_mock(&self, _device_id: &str, _rule: crate::types::MockRule) -> Result<()> {
        Err(VelocityError::Unsupported(
            "mockRequest is supported on the web only".into(),
        ))
    }

    /// Drop the mocks `add_mock` added (the suite's own stay).
    async fn clear_mocks(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }

    /// Media and call state for `assertSystemState`.
    async fn system_state(&self, _device_id: &str) -> Result<crate::types::SystemState> {
        Err(VelocityError::Unsupported(
            "assertSystemState isn't supported by this driver (it reads an Android device's services)".into(),
        ))
    }

    /// A drag made to scroll (`scrollUntilVisible`), not a swipe gesture: it
    /// should move the content a known distance and stop. iOS strokes it
    /// without a fling; a pager still needs `swipe`'s fling to turn a page.
    async fn scroll(&self, device_id: &str, finger: Direction) -> Result<()> {
        self.swipe(device_id, finger).await
    }
    /// [`Self::scroll`] along a line.
    async fn scroll_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.swipe_coords(device_id, from, to).await
    }
    async fn press_key(&self, device_id: &str, key: Key) -> Result<()>;

    // Raw actions (for WebView / non-accessible UI)
    async fn tap_xy(&self, _device_id: &str, _x: i32, _y: i32) -> Result<()> {
        Err(VelocityError::Unsupported(
            "tapXY not supported on this platform".into(),
        ))
    }
    async fn input_raw(&self, _device_id: &str, _text: &str) -> Result<()> {
        Err(VelocityError::Unsupported(
            "inputRaw not supported on this platform".into(),
        ))
    }

    // Screen
    async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>>;
    /// The capture `assertScreenshot` compares: a driver that can hold the
    /// screen still for it (web: animations finished, caret hidden) does.
    async fn screenshot_for_compare(&self, device_id: &str) -> Result<Vec<u8>> {
        self.screenshot(device_id).await
    }
    async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)>;
    /// `element` brought on screen if it isn't, with its bounds re-read
    /// (`scrollUntilVisible within:` a container below the fold). Drivers
    /// whose elements can't be scrolled to this way return it unchanged.
    async fn reveal(&self, _device_id: &str, element: &Element) -> Result<Element> {
        Ok(element.clone())
    }

    // Element state
    async fn get_element_text(&self, device_id: &str, element: &Element) -> Result<String>;
    /// `assertText`: whether the element's text is `expected`, and the
    /// text read (for the failure message). Exact by default; a driver
    /// that normalises the text it reads (web: whitespace, zero-width
    /// characters, NFC, CSS `text-transform`) compares `expected`
    /// normalised the same way, so a value `text:` finds on an element
    /// also passes `assertText` on it.
    async fn element_text_matches(
        &self,
        device_id: &str,
        element: &Element,
        expected: &str,
    ) -> Result<(bool, String)> {
        let actual = self.get_element_text(device_id, element).await?;
        Ok((actual == expected, actual))
    }
    async fn is_element_visible(&self, device_id: &str, element: &Element) -> Result<bool>;

    // Web navigation (web platform only — default impls return a clear error on mobile)
    async fn navigate(&self, _device_id: &str, _url: &str) -> Result<()> {
        Err(VelocityError::Unsupported(
            "openLink/goto isn't supported on this platform".into(),
        ))
    }
    /// The page now shown (web), for the natural-language `goto` origin
    /// check (D-17). `None` where there is no page.
    async fn current_url(&self, _device_id: &str) -> Option<String> {
        None
    }
    async fn go_back(&self, _device_id: &str) -> Result<()> {
        Err(VelocityError::Unsupported(
            "goBack isn't supported on this platform (it's the browser's back; on a device use `pressKey: { key: back }`)".into(),
        ))
    }
    async fn refresh_page(&self, _device_id: &str) -> Result<()> {
        Err(VelocityError::Unsupported(
            "refresh isn't supported on this platform (it reloads a web page)".into(),
        ))
    }

    // Resource profiling (opt-in, Android-only for now)
    // Returns (java_heap_kb, native_heap_kb, total_pss_kb, cpu_percent)
    async fn collect_resource_metrics(
        &self,
        _device_id: &str,
        _package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        Err(VelocityError::Unsupported(
            "Resource profiling not available on this platform".into(),
        ))
    }
}
