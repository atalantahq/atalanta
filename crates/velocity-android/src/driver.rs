use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use velocity_common::*;

use crate::adb::Adb;
use crate::adb_backend::AdbBackend;
use crate::async_adb::AsyncAdb;
use crate::parser::parse_hierarchy_v2;
use crate::selector::{find_all_elements, find_element, MatchOptions};
use crate::shell;
use crate::tree_server::{SetText, TreeServer};

type CacheSlot = Arc<Mutex<Option<(Element, Instant)>>>;

/// A screen size and when it was read.
type CachedSize = ((i32, i32), Instant);

/// Android platform driver supporting both subprocess and async TCP ADB backends.
///
/// Set `VELOCITY_ADB_MODE=async` to use the native async TCP client (faster).
/// Default is `subprocess` for backwards compatibility.
pub struct AndroidDriver {
    adb: Box<dyn AdbBackend>,
    /// Per-device cached tree. Each device has its own slot and lock: one
    /// shared slot served device A's tree for device B in parallel runs, and
    /// one shared lock serialized every device's dumps.
    hierarchy_cache: std::sync::Mutex<HashMap<String, CacheSlot>>,
    cache_ttl: Duration,
    /// (width, height) per device, re-read after [`SCREEN_SIZE_TTL`] so a
    /// rotation mid-run is picked up.
    screen_size_cache: std::sync::Mutex<HashMap<String, CachedSize>>,
    /// On-device tree server: ~ms dumps instead of ~2 s `uiautomator dump`.
    tree: TreeServer,
    /// `adb shell` processes running CPU busy loops (flaky hunting).
    stress: Mutex<std::collections::HashMap<String, Vec<velocity_common::process::GroupChild>>>,
    /// System animations off for the run (restored on the device).
    animations: crate::animations::AnimationGuard,
    /// Devices whose status bar is in demo mode (D-V4), exited on cleanup.
    demo_mode: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Launcher activity per (device, package), resolved once.
    launchers: std::sync::Mutex<HashMap<(String, String), Option<String>>>,
    /// Bounds of the field the last inputText focused, per device.
    focused: std::sync::Mutex<HashMap<String, Rect>>,
    /// Per (device, package): when the last launch happened and the pid it
    /// started, so a crash report is only one from that launch.
    launches: std::sync::Mutex<HashMap<(String, String), Launch>>,
    /// Host port forwarded to each device's sync probe (N3).
    probe_ports: std::sync::Mutex<HashMap<String, u16>>,
    /// The device this driver last forwarded host port 19401 to: a launch
    /// sends that forward again only when it points elsewhere (the last
    /// forward wins) or failed. Held across the forward so it says where
    /// the port really points.
    fixed_probe_forward: Mutex<Option<String>>,
    /// When each device's screen was last seen ready (awake, unlocked, no
    /// shade). Any action forgets it; a launch or link within
    /// [`SCREEN_READY_TTL`] of it skips its own `dumpsys` round trip.
    screen_ready: std::sync::Mutex<HashMap<String, Instant>>,
    screen_ready_ttl: Duration,
    /// `ro.build.version.sdk` per device: read once, not on every crash
    /// poll (M-7).
    sdk_levels: std::sync::Mutex<HashMap<String, u32>>,
    /// When exit-info was last read per (device, app): at most once per
    /// [`EXIT_INFO_EVERY`] (M-7).
    exit_info_read: std::sync::Mutex<HashMap<(String, String), Instant>>,
    /// Screen video streams and logcat clocks per device (failure evidence).
    recorders: std::sync::Arc<crate::evidence::Recorders>,
}

/// Longest gap between two `dumpsys activity exit-info` reads while the
/// app isn't known to be in front: a system_server dump is heavier than
/// the crash-log read every poll makes.
const EXIT_INFO_EVERY: Duration = Duration::from_secs(2);

/// What a launch leaves to tell its own crash from older ones.
#[derive(Debug, Clone, Default)]
struct Launch {
    /// Device clock at the launch, as `logcat -T` takes it (`<secs>.<ms>`).
    since: Option<String>,
    /// The same moment as the device's local wall clock, as exit-info and
    /// `logcat -v time` print it (`YYYY-MM-DD HH:MM:SS.mmm`).
    wall: Option<String>,
    /// The app's pid after the launch.
    pid: Option<u32>,
}

/// The on-device sync probe's port.
const PROBE_PORT: u16 = 19401;

/// How long a screen seen ready is taken to still be ready, with no action
/// since. Short: nothing here keeps the screen on, and its own timeout may
/// turn it off just after it was read.
const SCREEN_READY_TTL: Duration = Duration::from_secs(2);

/// The device clock at a launch, for [`Launch`]: epoch, nanoseconds and
/// local wall time (`parse_launch_mark`).
const LAUNCH_MARK: &str = "date '+%s %N %Y-%m-%d %H:%M:%S'";

/// Starts the line `wait_for_activity`'s focus check adds with `pidof`.
const PID_MARK: &str = "velocity-pid:";

impl Default for AndroidDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl AndroidDriver {
    /// The list under `at` scrolled by its own action; false when there is
    /// none or it can't move (the caller drags instead).
    async fn scroll_semantic(
        &self,
        device_id: &str,
        at: (i32, i32),
        finger: Direction,
    ) -> Result<bool> {
        self.invalidate_cache(device_id).await;
        // Finger up/left shows what's below/right: the list moves forward.
        let forward = matches!(finger, Direction::Up | Direction::Left);
        let vertical = matches!(finger, Direction::Up | Direction::Down);
        let r = self
            .tree
            .scroll_node(device_id, at.0, at.1, forward, vertical)
            .await;
        unknown_outcome(device_id, "scroll", &r)?;
        Ok(matches!(r, Some(Ok(true))))
    }

    /// Gets past the lock screen before a launch. A sleeping or locked
    /// device ran the app behind the keyguard, and the first assert failed
    /// "not found" after 10 s (a real POS tablet). Like Appium's UiAutomator2
    /// and uiautomator2's `unlock()`: wake, then dismiss a swipe-only lock;
    /// a credential lock is reported, never entered.
    ///
    /// Returns when the screen was seen ready. A launch or link just after
    /// another one (no action between) takes the screen as still ready.
    async fn ensure_unlocked(&self, device_id: &str) -> Result<Instant> {
        if let Some(at) = self.screen_seen_ready(device_id) {
            return Ok(at);
        }
        let at = Instant::now();
        let state = self.screen_state(device_id).await;
        if screen_ready(&state) {
            return Ok(at);
        }
        // Asleep: no window is drawn, every dump fails, and the circuit
        // breaker cycled for minutes (one emulator test took 130 s).
        if !awake(&state) {
            let _ = self
                .adb
                .run_device(device_id, &["shell", "input keyevent KEYCODE_WAKEUP"])
                .await;
        }
        let mut dismissed = false;
        let mut collapsed = false;
        for _ in 0..14 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let at = Instant::now();
            let state = self.screen_state(device_id).await;
            if screen_ready(&state) {
                tracing::info!(device_id, "woke the screen / dismissed the lock screen");
                return Ok(at);
            }
            if shade_open(&state) && !collapsed {
                collapsed = true;
                // GLOBAL_ACTION_DISMISS_NOTIFICATION_SHADE (API 31+) from the
                // server; else `cmd statusbar collapse` over adb.
                if !matches!(self.tree.global(device_id, 15).await, Some(Ok(true))) {
                    let _ = self
                        .adb
                        .run_device(device_id, &["shell", "cmd statusbar collapse"])
                        .await;
                }
            }
            if awake(&state) && keyguard_showing(&state) && !dismissed {
                dismissed = true;
                let _ = self
                    .adb
                    .run_device(device_id, &["shell", "wm dismiss-keyguard"])
                    .await;
            }
        }
        // A secure lock now shows its PIN pad: close it again.
        let _ = self
            .adb
            .run_device(device_id, &["shell", "input keyevent KEYCODE_BACK"])
            .await;
        Err(VelocityError::DeviceLocked {
            id: device_id.to_string(),
        })
    }

    /// When `device_id`'s screen was seen ready, if under
    /// [`SCREEN_READY_TTL`] ago with no action since.
    fn screen_seen_ready(&self, device_id: &str) -> Option<Instant> {
        self.screen_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
            .filter(|at| at.elapsed() < self.screen_ready_ttl)
    }

    /// A launch or link neither locks the screen nor pulls the shade down:
    /// what [`ensure_unlocked`](Self::ensure_unlocked) saw at `at` still
    /// holds after it (the action itself forgot it).
    fn keep_screen_ready(&self, device_id: &str, at: Instant) {
        self.screen_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_string(), at);
    }

    /// Power and keyguard state in one round-trip (~40–100 ms).
    async fn screen_state(&self, device_id: &str) -> String {
        self.adb
            .run_device(
                device_id,
                &[
                    "shell",
                    "dumpsys power | grep -m1 mWakefulness=; dumpsys window policy | grep -E 'mIsShowing=|isKeyguardShowing='; dumpsys window displays | grep -E 'mDisplayId=|mCurrentFocus='",
                ],
            )
            .await
            .unwrap_or_default()
    }

    pub fn new() -> Self {
        let mode = velocity_common::env::var("VELOCITY_ADB_MODE")
            .unwrap_or_else(|_| "subprocess".to_string());
        let adb: Box<dyn AdbBackend> = match mode.as_str() {
            "async" | "tcp" => {
                info!("Using async TCP ADB client");
                Box::new(AsyncAdb::new())
            }
            _ => Box::new(Adb::new()),
        };

        Self {
            adb,
            hierarchy_cache: std::sync::Mutex::new(HashMap::new()),
            cache_ttl: Duration::from_millis(2000),
            screen_size_cache: std::sync::Mutex::new(HashMap::new()),
            tree: TreeServer::new(),
            stress: Mutex::new(std::collections::HashMap::new()),
            animations: crate::animations::AnimationGuard::new(),
            demo_mode: Default::default(),
            launchers: std::sync::Mutex::new(HashMap::new()),
            focused: std::sync::Mutex::new(HashMap::new()),
            launches: std::sync::Mutex::new(HashMap::new()),
            probe_ports: std::sync::Mutex::new(HashMap::new()),
            fixed_probe_forward: Mutex::new(None),
            screen_ready: std::sync::Mutex::new(HashMap::new()),
            screen_ready_ttl: SCREEN_READY_TTL,
            sdk_levels: std::sync::Mutex::new(HashMap::new()),
            exit_info_read: std::sync::Mutex::new(HashMap::new()),
            recorders: Default::default(),
        }
    }

    /// The device's live screen stream (H.264, scrcpy-style), started on
    /// first use and shared with the failure-evidence recorder: one per
    /// device for the driver's life. See [`crate::video::VideoStream`].
    pub async fn video_stream(
        &self,
        device_id: &str,
    ) -> std::result::Result<std::sync::Arc<crate::video::VideoStream>, String> {
        self.recorders
            .stream(&adb_path(), device_id, Duration::from_secs(30))
            .await
    }

    /// A JPEG screenshot from the tree server (smaller and quicker to send
    /// than the PNG [`PlatformDriver::screenshot`] returns), for a live view
    /// without the video stream; `None` when the server can't take one.
    pub async fn screenshot_jpeg(&self, device_id: &str) -> Option<Vec<u8>> {
        match self.tree.screenshot(device_id, true).await {
            Some(Ok(jpeg)) if jpeg.starts_with(&[0xFF, 0xD8]) => Some(jpeg),
            _ => None,
        }
    }

    /// The host port that reaches `device_id`'s sync probe, once a launch
    /// has forwarded it: each device gets its own (`adb forward tcp:0`),
    /// where every device used to share host port 19401 and the last
    /// forward won (N3).
    pub fn sync_probe_port(&self, device_id: &str) -> Option<u16> {
        self.probe_ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
    }

    /// Forward a free host port to the device's probe and remember it.
    async fn forward_probe(&self, device_id: &str) {
        if self.sync_probe_port(device_id).is_some() {
            return;
        }
        let remote = format!("tcp:{PROBE_PORT}");
        let Ok(out) = self
            .adb
            .run_device(device_id, &["forward", "tcp:0", &remote])
            .await
        else {
            return;
        };
        if let Ok(port) = out.trim().parse::<u16>() {
            self.probe_ports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(device_id.to_string(), port);
        }
    }

    /// Host port 19401 to the device's probe, for a client that hasn't
    /// asked [`sync_probe_port`](Self::sync_probe_port) yet: once per device
    /// rather than on every launch, again when the last one failed or went
    /// to another device.
    async fn forward_fixed_probe(&self, device_id: &str) {
        let mut to = self.fixed_probe_forward.lock().await;
        if to.as_deref() == Some(device_id) {
            return;
        }
        let port = format!("tcp:{PROBE_PORT}");
        let forwarded = self
            .adb
            .run_device(device_id, &["forward", &port, &port])
            .await
            .is_ok();
        *to = forwarded.then(|| device_id.to_string());
    }

    async fn forget_fixed_probe(&self, device_id: &str) {
        let mut to = self.fixed_probe_forward.lock().await;
        if to.as_deref() == Some(device_id) {
            *to = None;
        }
    }

    /// Stop the app (`pm clear` also stops it) and read the device clock
    /// for [`Launch`] (epoch for `logcat -T`, local wall time for
    /// exit-info, in milliseconds where `date` has `%N`) in one `adb shell`.
    /// A failed stop fails the launch, as it did on its own; a `date` that
    /// fails or prints something else only leaves the mark unknown.
    async fn stop_and_mark(
        &self,
        device_id: &str,
        app_id: &str,
        clear_state: bool,
    ) -> Result<(Option<String>, Option<String>)> {
        let stop = if clear_state {
            shell::cmd(&["pm", "clear", app_id])
        } else {
            shell::cmd(&["am", "force-stop", app_id])
        };
        let out = self
            .adb
            .run_device(
                device_id,
                &["shell", &format!("{stop} && {{ {LAUNCH_MARK} || true; }}")],
            )
            .await?;
        // `pm clear` prints "Success" first; the clock is the last line.
        let last = out.lines().rev().find(|l| !l.trim().is_empty());
        Ok(parse_launch_mark(last.unwrap_or("")))
    }

    async fn app_pid(&self, device_id: &str, app_id: &str) -> Option<u32> {
        let out = self
            .adb
            .run_device(device_id, &["shell", &shell::cmd(&["pidof", app_id])])
            .await
            .ok()?;
        out.split_whitespace().next()?.parse().ok()
    }

    async fn sdk_level(&self, device_id: &str) -> Option<u32> {
        if let Some(v) = self
            .sdk_levels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
        {
            return Some(*v);
        }
        let v: u32 = self
            .adb
            .run_device(device_id, &["shell", "getprop ro.build.version.sdk"])
            .await
            .ok()?
            .trim()
            .parse()
            .ok()?;
        self.sdk_levels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_string(), v);
        Some(v)
    }

    /// The crash log's last crash of `app_id` since the launch. `logcat -T
    /// <epoch>` is missing on old logcat (API 21-23): then the whole crash
    /// buffer is read with `-v time` and filtered on the host by the
    /// launch's wall clock, instead of crash detection silently going off
    /// (m-10).
    async fn crash_from_logcat(
        &self,
        device_id: &str,
        app_id: &str,
        launch: &Launch,
    ) -> Option<String> {
        let mut args = vec!["logcat", "-d", "-b", "crash", "-v", "brief"];
        if let Some(since) = launch.since.as_deref() {
            args.extend(["-T", since]);
        }
        match self.adb.run_device(device_id, &args).await {
            Ok(out) => crash_from_log(&out, app_id),
            Err(_) if launch.since.is_some() => {
                let wall = launch.wall.as_deref()?;
                let out = self
                    .adb
                    .run_device(device_id, &["logcat", "-d", "-b", "crash", "-v", "time"])
                    .await
                    .ok()?;
                crash_from_log(&time_log_since(&out, wall), app_id)
            }
            Err(_) => None,
        }
    }

    /// Whether the tree server says `app_id` owns the front window.
    async fn app_in_front(&self, device_id: &str, app_id: &str) -> bool {
        matches!(
            self.tree.windows(device_id).await,
            Some(Ok(w)) if front_app(&w) == Some(app_id)
        )
    }

    /// Whether exit-info may be read for (device, app) now; marks it read.
    fn exit_info_due(&self, device_id: &str, app_id: &str) -> bool {
        let mut map = self
            .exit_info_read
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (device_id.to_string(), app_id.to_string());
        if map.get(&key).is_some_and(|t| t.elapsed() < EXIT_INFO_EVERY) {
            return false;
        }
        map.insert(key, Instant::now());
        true
    }

    /// A driver on `adb` with the tree server off (tests).
    #[cfg(test)]
    pub(crate) fn for_tests(adb: Box<dyn AdbBackend>) -> Self {
        Self::with_backend(adb)
    }

    /// A driver on `adb` with the tree server off (tests).
    #[cfg(test)]
    fn with_backend(adb: Box<dyn AdbBackend>) -> Self {
        Self {
            adb,
            tree: TreeServer::off(),
            animations: crate::animations::AnimationGuard::off(),
            ..Self::new()
        }
    }

    /// The package's launcher activity (`pkg/.Main`), resolved once.
    async fn launcher(&self, device_id: &str, package: &str) -> Option<String> {
        let key = (device_id.to_string(), package.to_string());
        if let Some(c) = self
            .launchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return c.clone();
        }
        let component = self
            .adb
            .run_device(
                device_id,
                &[
                    "shell",
                    &shell::cmd(&[
                        "cmd",
                        "package",
                        "resolve-activity",
                        "--brief",
                        "-c",
                        "android.intent.category.LAUNCHER",
                        package,
                    ]),
                ],
            )
            .await
            .ok()
            .and_then(|out| parse_component(&out, package));
        // Only cache a hit: the app may not be installed yet.
        if component.is_some() {
            self.launchers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, component.clone());
        }
        component
    }

    /// Start the app the way the launcher does: `am start` on its resolved
    /// activity with MAIN/LAUNCHER and NEW_TASK|RESET_TASK_IF_NEEDED, which
    /// brings an existing task forward. 196 → 55 ms against `monkey`, which
    /// resolves the activity and injects an event on every launch.
    ///
    /// Returns the app's pid when the last focus check saw one of its
    /// activities in front.
    async fn start_app(&self, device_id: &str, package: &str) -> Result<Option<u32>> {
        self.start_app_once(device_id, package).await?;
        let (up, pid) = self.wait_for_activity(device_id, package).await;
        if up {
            return Ok(pid);
        }
        // A start right after a force-stop sometimes never gets past the
        // splash (the system's starting window): no activity is drawn, every
        // dump says "no active window", and a clearState test sat there for
        // 60 s until the circuit breaker's restart relaunched it (1 run in
        // 3). Start it once more.
        warn!(
            device_id,
            package, "app stuck at its splash screen; relaunching"
        );
        self.adb
            .run_device(
                device_id,
                &["shell", &shell::cmd(&["am", "force-stop", package])],
            )
            .await?;
        self.start_app_once(device_id, package).await?;
        let (up, pid) = self.wait_for_activity(device_id, package).await;
        if !up {
            debug!(
                device_id,
                package, "app activity still not in front after a relaunch"
            );
        }
        Ok(pid)
    }

    /// Wait (up to 3 s) for one of `package`'s activities to have focus.
    /// The splash window is "Splash Screen <package>"; an activity is
    /// "<package>/<class>". Any other app in front (a permission dialog,
    /// a chooser) also counts as launched: only the stuck splash doesn't.
    ///
    /// Also returns the app's pid when the last check found one of its
    /// activities in front: `pidof` rides along in the same `adb shell`,
    /// and with an activity of the app focused its process is the launch's.
    async fn wait_for_activity(&self, device_id: &str, package: &str) -> (bool, Option<u32>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        // Every display's focus: a virtual display (the screen video's
        // mirror, a cast) is listed before display 0 and has none
        // (`default_display_focus`). The `echo` keeps a missing `pidof`
        // (or no match) from failing the check.
        let check = format!(
            "dumpsys window displays | grep -E 'mDisplayId=|mCurrentFocus='; echo \"{PID_MARK}$({} 2>/dev/null)\"",
            shell::cmd(&["pidof", package])
        );
        loop {
            let focus = self
                .adb
                .run_device(device_id, &["shell", &check])
                .await
                .unwrap_or_default();
            if launched(&focus, package) {
                return (true, focused_pid(&focus, package));
            }
            if std::time::Instant::now() >= deadline {
                return (false, focused_pid(&focus, package));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    async fn start_app_once(&self, device_id: &str, package: &str) -> Result<()> {
        if let Some(component) = self.launcher(device_id, package).await {
            let out = self
                .adb
                .run_device(
                    device_id,
                    &[
                        "shell",
                        &shell::cmd(&[
                            "am",
                            "start",
                            "-a",
                            "android.intent.action.MAIN",
                            "-c",
                            "android.intent.category.LAUNCHER",
                            "-f",
                            "0x10200000",
                            "-n",
                            &component,
                        ]),
                    ],
                )
                .await;
            match out {
                Ok(o) if !o.contains("Error") => return Ok(()),
                other => debug!(?other, "am start failed; falling back to monkey"),
            }
        }
        self.adb.launch_app(device_id, package, false).await
    }

    /// Restore emulator snapshot `name` and wait until the device is back.
    async fn restore_snapshot(&self, device_id: &str, name: &str) -> Result<()> {
        check_snapshot_name(name)?;
        if !device_id.starts_with("emulator-") {
            return Err(VelocityError::Config(format!(
                "snapshot reset needs an emulator, {device_id} is not one"
            )));
        }
        let out = self
            .adb
            .run_device(device_id, &["emu", "avd", "snapshot", "load", name])
            .await?;
        if !out.contains("OK") || out.contains("KO") {
            return Err(VelocityError::Config(format!(
                "snapshot '{name}' could not be loaded: {} \
                 (create it on a clean device with: adb -s {device_id} emu avd snapshot save {name})",
                out.trim()
            )));
        }
        // The restore restarts the guest; the tree server went with it, and
        // maybe the adb transport and its forwards.
        self.tree.reset(device_id).await;
        self.forget_fixed_probe(device_id).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let booted = self
                .adb
                .run_device(device_id, &["shell", "getprop", "sys.boot_completed"])
                .await
                .is_ok_and(|s| s.trim() == "1");
            if booted {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(VelocityError::Config(format!(
            "{device_id} did not come back after loading snapshot '{name}'"
        )))
    }

    /// Hierarchy XML from the tree server, falling back to `uiautomator dump`.
    async fn dump_xml(&self, device_id: &str) -> Result<String> {
        match self.tree.dump_checked(device_id).await {
            Some(Ok(xml)) => {
                // Protocol 8 stops 200 nodes deep (the device's Java stack).
                let head = xml.split("<node").next().unwrap_or("");
                if head.contains(" truncated=\"true\"") {
                    warn!(device_id, "the screen's tree is over 200 levels deep; deeper nodes are not in the dump");
                }
                Ok(xml)
            }
            // The server is up but could not read the screen (no window yet,
            // after a relaunch). `uiautomator dump` cannot help then: it needs
            // the UiAutomation connection the server holds, so it failed at
            // once and only added failures toward the circuit breaker.
            Some(Err(detail)) => Err(VelocityError::CommandFailed {
                device_id: device_id.to_string(),
                command: "tree server DUMP".into(),
                detail,
            }),
            None => self.adb.dump_hierarchy(device_id).await,
        }
    }

    fn slot(&self, device_id: &str) -> CacheSlot {
        self.hierarchy_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    async fn get_cached_hierarchy(&self, device_id: &str) -> Result<Element> {
        let slot = self.slot(device_id);
        let mut cache = slot.lock().await;
        if let Some((ref tree, ref timestamp)) = *cache {
            if timestamp.elapsed() < self.cache_ttl {
                return Ok(tree.clone());
            }
        }

        let xml = self.dump_xml(device_id).await?;
        let tree = parse_dump(device_id, &xml)?;
        *cache = Some((tree.clone(), Instant::now()));
        Ok(tree)
    }

    /// Drop the cached tree. Awaited (not spawned) so a lookup issued right
    /// after an action can never read the pre-action tree.
    /// D-5: after `inputText`'s tap, the focused node must be the target
    /// (or overlap it) before any key is sent: keys aimed at a label went
    /// into the field focused before (VEL-S46). Read from the tree server's
    /// lite dump, which reports `focused` on every node; focus can lag the
    /// tap, so a mismatch is re-read for about a second. Without the server
    /// (a `uiautomator dump` costs seconds), or when nothing reports focus,
    /// nothing changes.
    async fn verify_focus(
        &self,
        device_id: &str,
        element: &Element,
        before: Option<&Element>,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        loop {
            let Some(xml) = self.tree.dump_lite(device_id).await else {
                return Ok(());
            };
            let Ok(tree) = crate::parser::parse_hierarchy(&xml) else {
                return Ok(());
            };
            let other = match tree.focus_check_after_tap(element, before) {
                FocusCheck::OnTarget | FocusCheck::Unknown => return Ok(()),
                FocusCheck::Elsewhere(other) => other,
            };
            if std::time::Instant::now() >= deadline {
                return Err(VelocityError::AssertionFailed {
                    expected: format!(
                        "{} at {:?} focused for typing",
                        element.element_type, element.bounds
                    ),
                    actual: format!(
                        "the focus stayed on {} {:?} at {:?}; the keys would go there \
                         (select the text field itself, or VELOCITY_ANDROID_FOCUS_CHECK=off)",
                        other.element_type,
                        other
                            .text
                            .as_deref()
                            .or(other.label.as_deref())
                            .unwrap_or(&other.platform_id),
                        other.bounds
                    ),
                    selector: element
                        .label
                        .clone()
                        .or_else(|| element.text.clone())
                        .unwrap_or_else(|| element.platform_id.clone()),
                    screenshot: None,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The tree before `inputText`'s tap, for D-5 to tell a field that
    /// took the focus from one that kept it: the lookup's cached tree, else
    /// a lite dump (a few ms through the tree server).
    async fn focus_before(&self, device_id: &str) -> Option<Element> {
        if let Some((tree, _)) = self.slot(device_id).lock().await.as_ref() {
            return Some(tree.clone());
        }
        let xml = self.tree.dump_lite(device_id).await?;
        crate::parser::parse_hierarchy(&xml).ok()
    }

    async fn invalidate_cache(&self, device_id: &str) {
        *self.slot(device_id).lock().await = None;
        // An action may lock the screen (power key) or pull the shade down.
        self.screen_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
        // Any action may move focus; inputText re-records it afterwards.
        self.focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    async fn invalidate_all(&self) {
        self.screen_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let slots: Vec<CacheSlot> = self
            .hierarchy_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for slot in slots {
            *slot.lock().await = None;
        }
    }

    async fn get_screen_size_cached(&self, device_id: &str) -> Result<(i32, i32)> {
        if let Some((size, at)) = self
            .screen_size_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
        {
            if at.elapsed() < SCREEN_SIZE_TTL {
                return Ok(*size);
            }
        }
        let size = self.adb.screen_size(device_id).await?;
        self.screen_size_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_string(), (size, Instant::now()));
        Ok(size)
    }

    async fn screen_bounds(&self, device_id: &str) -> Result<Rect> {
        let (w, h) = self.get_screen_size_cached(device_id).await?;
        Ok(Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        })
    }

    /// The key's Android keycode; `None` for a key this driver doesn't
    /// know (`Key` is `#[non_exhaustive]`).
    fn key_to_keycode(key: Key) -> Option<u32> {
        Some(match key {
            Key::Back => 4,
            Key::Home => 3,
            Key::Enter => 66,
            Key::VolumeUp => 24,
            Key::VolumeDown => 25,
            // KEYCODE_DEL is Backspace; KEYCODE_FORWARD_DEL is Delete.
            Key::Backspace => 67,
            Key::Tab => 61,
            Key::Escape => 111,
            Key::Delete => 112,
            Key::ArrowUp => 19,
            Key::ArrowDown => 20,
            Key::ArrowLeft => 21,
            Key::ArrowRight => 22,
            _ => return None,
        })
    }

    /// A tree-server command result as a driver result; `None` means the
    /// server isn't available and the caller falls back to adb.
    fn server(
        device_id: &str,
        what: &str,
        r: Option<std::result::Result<(), String>>,
    ) -> Option<Result<()>> {
        r.map(|r| {
            r.map_err(|detail| VelocityError::CommandFailed {
                device_id: device_id.to_string(),
                command: format!("tree server {what}"),
                detail,
            })
        })
    }

    /// Tap (x, y) `n` times: through the tree server (no `input` process),
    /// else adb.
    async fn tap_point(&self, device_id: &str, x: i32, y: i32, n: u32) -> Result<()> {
        if let Some(r) = Self::server(device_id, "tap", self.tree.taps(device_id, x, y, n).await) {
            return r;
        }
        if n >= 2 {
            self.adb.double_tap(device_id, x, y).await
        } else {
            self.adb.tap(device_id, x, y).await
        }
    }

    async fn key_code(&self, device_id: &str, code: u32) -> Result<()> {
        if let Some(r) = Self::server(device_id, "key", self.tree.key(device_id, code).await) {
            return r;
        }
        self.adb.press_key(device_id, code).await
    }

    async fn swipe_points(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        ms: u64,
    ) -> Result<()> {
        if let Some(r) = Self::server(
            device_id,
            "swipe",
            self.tree.swipe(device_id, from, to, ms).await,
        ) {
            return r;
        }
        self.adb
            .swipe(device_id, from.0, from.1, to.0, to.1, ms)
            .await
    }

    /// Where to touch `bounds` (A-7): its center, unless a keyboard, system
    /// window or overlay in front covers it — then the nearest uncovered
    /// point inside it. Fully covered is an error naming what covers it;
    /// tapping anyway would press a key or the overlay.
    async fn touch_point(&self, device_id: &str, bounds: &Rect) -> Result<(i32, i32)> {
        let center = bounds.center();
        let Some(Ok(windows)) = self.tree.windows(device_id).await else {
            return Ok(center);
        };
        match uncovered_point(&windows, bounds) {
            Ok(p) => Ok(p),
            Err(cover) => Err(VelocityError::AssertionFailed {
                expected: format!("target at {bounds:?} reachable"),
                actual: format!(
                    "covered by the {} ({}); hide it first, e.g. pressKey: back for the keyboard",
                    cover.kind_name(),
                    cover.package
                ),
                selector: String::new(),
                screenshot: None,
            }),
        }
    }

    /// Whether `run-as` works for the package (a debuggable build).
    async fn debuggable(&self, device_id: &str, app_id: &str) -> bool {
        self.adb
            .run_device(
                device_id,
                &["shell", &shell::cmd(&["run-as", app_id, "id"])],
            )
            .await
            .is_ok_and(|o| o.contains("uid="))
    }

    /// Let a device-side settle know an action starts now (only needed for
    /// actions that don't go through the tree server's input).
    async fn mark_action(&self, device_id: &str) {
        let _ = self.tree.mark(device_id).await;
    }

    async fn swipe_direction_impl(&self, device_id: &str, direction: Direction) -> Result<()> {
        let (w, h) = self.get_screen_size_cached(device_id).await?;
        let (x1, y1, x2, y2) = swipe_line(w, h, direction);

        self.swipe_points(device_id, (x1, y1), (x2, y2), 300).await
    }
}

/// A point inside `bounds` no front window covers, preferring the center;
/// otherwise the window covering the center. Windows are front first; only
/// those in front of the target's app window over the point count, and the
/// app's own windows don't (the target's dialog is one). Another app's
/// window in front (a freeform or desktop-mode window, protocol 19 names
/// the active one) does: the tap would go to that app (review 09 #12).
/// The points tried are the shared tap guard's
/// ([`velocity_common::tap_guard`]).
fn uncovered_point(
    windows: &[crate::tree_server::Window],
    bounds: &Rect,
) -> std::result::Result<(i32, i32), crate::tree_server::Window> {
    let contains = |r: &Rect, (x, y): (i32, i32)| {
        x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
    };
    let active = windows
        .iter()
        .find(|w| w.active == Some(true))
        .map(|w| w.package.as_str())
        .filter(|p| !p.is_empty() && *p != "-");
    // An app window of another package than the active one's; the
    // framework's own popups ("android") are drawn for the app.
    let foreign = |w: &crate::tree_server::Window| {
        w.kind == 1
            && active.is_some_and(|a| w.package != a && w.package != "-" && w.package != "android")
    };
    let cover = |p: (i32, i32)| {
        windows
            .iter()
            .take_while(|w| !(w.kind == 1 && !foreign(w) && contains(&w.bounds, p)))
            .find(|w| (w.kind != 1 || foreign(w)) && w.takes_touches() && contains(&w.bounds, p))
    };
    let center = bounds.center();
    let Some(blocker) = cover(center) else {
        return Ok(center);
    };
    velocity_common::tap_guard::free_point(bounds, &[], |p| cover(p).is_some())
        .ok_or_else(|| blocker.clone())
}

/// The package of the frontmost app window (windows are front first).
fn front_app(windows: &[crate::tree_server::Window]) -> Option<&str> {
    windows
        .iter()
        .find(|w| w.kind == 1 && !w.bounds.is_empty())
        .map(|w| w.package.as_str())
        .filter(|p| !p.is_empty() && *p != "-")
}

/// The status and navigation bars: SystemUI's thin system windows (a pulled
/// down notification shade is neither thin nor a bar).
fn system_bar_rects(windows: &[crate::tree_server::Window]) -> Vec<Rect> {
    windows
        .iter()
        .filter(|w| w.kind == 3 && w.package == "com.android.systemui")
        .map(|w| w.bounds)
        .filter(|b| {
            b.width > 0 && b.height > 0 && b.width.min(b.height) * 4 <= b.width.max(b.height)
        })
        .collect()
}

#[async_trait::async_trait]
impl PlatformDriver for AndroidDriver {
    /// SystemUI demo mode, entered once per device (D-V4).
    async fn freeze_status_bar(&self, device_id: &str) -> bool {
        if self
            .demo_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(device_id)
        {
            return true;
        }
        use crate::demo_mode::{Verdict, VERIFY, VERIFY_ON};
        let enter = crate::demo_mode::enter_commands();
        let mut enter: Vec<&str> = enter.iter().map(String::as_str).collect();
        enter.push(VERIFY);
        enter.push(VERIFY_ON);
        let mut read = self
            .adb
            .batch_shell(device_id, &enter)
            .await
            .map(|out| crate::demo_mode::verdict(&out));
        if matches!(read, Ok(Verdict::NotYet)) {
            // `enter` beat SystemUI's observer of the setting: once more.
            let again = crate::demo_mode::retry_commands();
            let again: Vec<&str> = again.iter().map(String::as_str).collect();
            read = self
                .adb
                .batch_shell(device_id, &again)
                .await
                .map(|out| crate::demo_mode::verdict(&out));
        }
        match read {
            Ok(Verdict::Refused | Verdict::NotYet) => {
                // Entered as far as the shell can tell, but SystemUI isn't
                // in demo mode: leave the bar masked (review-d5 m-1,
                // review-fixes #8).
                let _ = self
                    .adb
                    .batch_shell(device_id, &[crate::demo_mode::exit_commands()[0].as_str()])
                    .await;
                tracing::warn!(
                    "{device_id}: status bar not frozen (demo mode didn't take); masking it"
                );
                false
            }
            Ok(_) => {
                self.demo_mode
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(device_id.to_string());
                true
            }
            Err(e) => {
                tracing::warn!(
                    "{device_id}: status bar not frozen (demo mode refused: {e}); masking it"
                );
                false
            }
        }
    }

    /// Leave demo mode on every device this driver froze.
    async fn unfreeze_status_bar(&self) {
        let frozen: Vec<String> = self
            .demo_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for device_id in frozen {
            let exit = crate::demo_mode::exit_commands();
            let exit: Vec<&str> = exit.iter().map(String::as_str).collect();
            if let Err(e) = self.adb.batch_shell(&device_id, &exit).await {
                tracing::warn!("{device_id}: status bar demo mode not exited: {e}");
            }
        }
    }

    async fn cleanup(&self) {
        // The sync-probe forwards this run added: each run left one more
        // listening port in the adb server (m-11).
        let ports: Vec<(String, u16)> = self
            .probe_ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (device_id, port) in ports {
            let local = format!("tcp:{port}");
            let _ = self
                .adb
                .run_device(&device_id, &["forward", "--remove", &local])
                .await;
        }
        *self.fixed_probe_forward.lock().await = None;
        self.tree.stop_all().await;
        self.recorders.stop_all().await;
        self.animations.restore_all().await;
        self.unfreeze_status_bar().await;
        let stressed: Vec<String> = self.stress.lock().await.keys().cloned().collect();
        for device_id in stressed {
            self.set_stress(&device_id, false).await;
        }
    }

    /// Booted *and* the package manager answers: `sys.boot_completed` alone
    /// flips to 1 before system services are usable.
    async fn device_ready(&self, device_id: &str) -> bool {
        let booted = self
            .adb
            .run_device(device_id, &["shell", "getprop", "sys.boot_completed"])
            .await
            .is_ok_and(|s| s.trim() == "1");
        booted
            && self
                .adb
                .run_device(device_id, &["shell", "pm", "path", "android"])
                .await
                .is_ok_and(|s| s.contains("package:"))
    }

    async fn reset_app(
        &self,
        device_id: &str,
        app_id: &str,
        reset: &ResetConfig,
    ) -> Result<ResetStrategy> {
        shell::validate_app_id(app_id)?;
        self.invalidate_cache(device_id).await;
        let mut last_err = None;
        if reset.strategy == ResetStrategy::Snapshot {
            let name = reset.snapshot.as_deref().unwrap_or("velocity-clean");
            match self.restore_snapshot(device_id, name).await {
                Ok(()) => return Ok(ResetStrategy::Snapshot),
                Err(e) => last_err = Some(e),
            }
        }
        if matches!(
            reset.strategy,
            ResetStrategy::Snapshot | ResetStrategy::Reinstall
        ) {
            if let Some(apk) = &reset.app_path {
                let _ = self.adb.run_device(device_id, &["uninstall", app_id]).await;
                match self.adb.install_app(device_id, apk).await {
                    Ok(()) => return Ok(ResetStrategy::Reinstall),
                    Err(e) => last_err = Some(e),
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| VelocityError::Config("reinstall needs reset.appPath".to_string())))
    }

    async fn set_stress(&self, device_id: &str, on: bool) -> bool {
        let mut all = self.stress.lock().await;
        // Killing the host-side adb shell doesn't reliably kill the remote
        // shell, so the loops carry a tag and are killed on the device.
        // Always, also when this process started none: loops a run killed by
        // a second signal left on the device stop here (review d4 m2).
        all.remove(device_id);
        let _ = self
            .adb
            .run_device(device_id, &["shell", "pkill -f velocity-stress"])
            .await;
        if !on {
            return true;
        }
        let procs = all.entry(device_id.to_string()).or_default();
        let adb =
            velocity_common::env::var("VELOCITY_ADB_PATH").unwrap_or_else(|_| "adb".to_string());
        // One busy loop per core keeps the device saturated without
        // starving adb itself (the loops run at the lowest priority).
        let cores = self
            .adb
            .run_device(device_id, &["shell", "nproc"])
            .await
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(4);
        for _ in 0..cores {
            // Its own process group, registered for the second-signal kill:
            // `std::process::exit` skips `kill_on_drop` (review d4 m2).
            match velocity_common::process::spawn_group(
                Command::new(&adb)
                    .args(crate::adb::target_args(device_id))
                    .args([
                        "shell",
                        // The trailing word becomes $0, so `pkill -f` can find it.
                        "nice -n 19 sh -c 'while :; do :; done' velocity-stress",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            ) {
                Ok(child) => procs.push(child),
                Err(_) => return false,
            }
        }
        info!(device_id, cores, "CPU stress on");
        true
    }

    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.adb.list_devices().await
    }

    async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        self.adb.list_devices_strict().await
    }

    async fn boot_device(&self, device_id: &str) -> Result<()> {
        let emulator_path = velocity_common::env::var("VELOCITY_EMULATOR_PATH")
            .unwrap_or_else(|_| "emulator".to_string());

        debug!(avd = device_id, "Launching Android emulator");

        // Start the emulator in the background. device_id is the AVD name.
        Command::new(&emulator_path)
            .arg("-avd")
            .arg(device_id)
            // Quick boot from the saved snapshot (seconds, not a minute+
            // cold boot), but never save over it: test runs must not change
            // the state the next boot starts from. VELOCITY_EMULATOR_COLD_BOOT=1
            // restores the old cold-boot behaviour.
            .args(
                if velocity_common::env::var("VELOCITY_EMULATOR_COLD_BOOT").as_deref() == Ok("1") {
                    ["-no-snapshot-load", "-no-snapshot-save"]
                } else {
                    ["-no-snapshot-save", "-no-boot-anim"]
                },
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                VelocityError::Config(format!(
                    "Failed to launch emulator for AVD '{device_id}'. \
                 Ensure the Android emulator is on PATH or set VELOCITY_EMULATOR_PATH. Error: {e}"
                ))
            })?;

        // Poll adb until the device is booted (up to 60 seconds).
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if Instant::now() > deadline {
                return Err(VelocityError::Config(format!(
                    "Timed out waiting for AVD '{device_id}' to boot"
                )));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;

            let devices = self.adb.list_devices().await.unwrap_or_default();
            let booted = devices.iter().any(|d| d.state == DeviceState::Booted);
            if booted {
                // Wait for sys.boot_completed
                if let Ok(output) = self.adb.list_devices().await {
                    if let Some(d) = output.iter().find(|d| d.state == DeviceState::Booted) {
                        let _ = self
                            .adb
                            .run_device(
                                &d.id,
                                &["wait-for-device", "shell", "getprop", "sys.boot_completed"],
                            )
                            .await;
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
    }

    async fn shutdown_device(&self, device_id: &str) -> Result<()> {
        let _ = self.adb.run_device(device_id, &["emu", "kill"]).await;
        Ok(())
    }

    async fn install_app(&self, device_id: &str, app_path: &str) -> Result<()> {
        self.adb.install_app(device_id, app_path).await
    }

    async fn install_app_if_changed(
        &self,
        device_id: &str,
        app_path: &str,
        app_id: &str,
        hash: &str,
    ) -> Result<velocity_common::InstallOutcome> {
        shell::validate_app_id(app_id)?;
        crate::install::install_if_changed(&*self.adb, device_id, app_path, app_id, hash).await
    }

    async fn launch_app(&self, device_id: &str, app_id: &str, clear_state: bool) -> Result<()> {
        shell::validate_app_id(app_id)?;
        let ready = self.ensure_unlocked(device_id).await?;
        self.invalidate_cache(device_id).await;
        self.keep_screen_ready(device_id, ready);
        self.animations.disable(device_id).await;
        self.mark_action(device_id).await;
        // A launch is a cold start, as on iOS (simctl relaunches) and in
        // Maestro: `am start` on a running app only brought its task to the
        // front, so each test inherited the screen the last one left
        // (VelocityLab: after one test stopped on Login, 24 later tests
        // never saw Home). `pm clear` stops the app too. Batch mode skips
        // the launch when it wants the app kept warm.
        //
        // A crash report from before this launch must not be blamed on it:
        // remember when it happened (and later its pid) rather than wiping
        // the device-wide crash buffer with `logcat -b crash -c` (N8).
        let (since, wall) = self.stop_and_mark(device_id, app_id, clear_state).await?;
        self.exit_info_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(device_id.to_string(), app_id.to_string()));
        // The focus check read the pid if it saw the app's activity in
        // front; else (launcher or a dialog still in front) ask after it.
        let pid = match self.start_app(device_id, app_id).await? {
            Some(pid) => Some(pid),
            None => self.app_pid(device_id, app_id).await,
        };
        self.launches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (device_id.to_string(), app_id.to_string()),
                Launch { since, wall, pid },
            );
        // Best-effort: forward the native sync probe port so the host-side
        // client can reach the probe listening on-device. The fixed 19401
        // forward stays until the sync client asks `sync_probe_port`.
        self.forward_probe(device_id).await;
        self.forward_fixed_probe(device_id).await;
        Ok(())
    }

    fn sync_probe_port(&self, device_id: &str) -> Option<u16> {
        AndroidDriver::sync_probe_port(self, device_id)
    }

    async fn start_evidence(
        &self,
        device_id: &str,
        req: &velocity_common::EvidenceRequest,
    ) -> Option<Box<dyn velocity_common::EvidenceSession>> {
        crate::evidence::AndroidEvidence::start(self.recorders.clone(), &adb_path(), device_id, req)
            .await
            .map(|e| Box::new(e) as Box<dyn velocity_common::EvidenceSession>)
    }

    async fn crash_report(&self, device_id: &str, app_id: &str) -> Option<String> {
        shell::validate_app_id(app_id).ok()?;
        let launch = self
            .launches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id.to_string(), app_id.to_string()))
            .cloned()
            .unwrap_or_default();
        // The crash log since the launch: one adb call, on every poll.
        if let Some(crash) = self.crash_from_logcat(device_id, app_id, &launch).await {
            return Some(crash);
        }
        // API 30+: the system's own record of how the app's processes
        // ended (P3): native crashes and ANRs never reach the crash log.
        // Only records from this launch on, of any pid of the package: the
        // launch pid goes stale when the app restarts itself or a link
        // cold-starts it, and an old record can carry a reused pid (M-6).
        // Not read while the app is in front (it's alive), and at most
        // every EXIT_INFO_EVERY otherwise (M-7).
        let wall = launch.wall.as_deref()?;
        if !self.sdk_level(device_id).await.is_some_and(|v| v >= 30)
            || !self.exit_info_due(device_id, app_id)
            || self.app_in_front(device_id, app_id).await
        {
            return None;
        }
        let out = self
            .adb
            .run_device(
                device_id,
                &[
                    "shell",
                    &shell::cmd(&["dumpsys", "activity", "exit-info", app_id]),
                ],
            )
            .await
            .ok()?;
        crash_from_exit_info(&out, wall, launch.pid)
    }

    /// The crash log, then the front app window from the tree server
    /// (`WINDOWS`, a few ms). Unknown when the server can't answer.
    async fn app_liveness(&self, device_id: &str, app_id: &str) -> velocity_common::Liveness {
        use velocity_common::Liveness;
        if let Some(detail) = self.crash_report(device_id, app_id).await {
            return Liveness::Dead(detail);
        }
        let Some(Ok(windows)) = self.tree.windows(device_id).await else {
            return Liveness::Unknown;
        };
        match front_app(&windows) {
            Some(p) if p == app_id => Liveness::Foreground,
            Some(p) => Liveness::Background(Some(p.to_string())),
            None => Liveness::Unknown,
        }
    }

    async fn system_bars(&self, device_id: &str) -> Option<Vec<Rect>> {
        let Some(Ok(windows)) = self.tree.windows(device_id).await else {
            return None;
        };
        Some(system_bar_rects(&windows))
    }

    /// The build's SHA-256 when Velocity installed it; otherwise
    /// versionCode, versionName and lastUpdateTime from `dumpsys package`:
    /// a reinstall of the same version still changes the install time.
    async fn app_fingerprint(&self, device_id: &str, app_id: &str) -> Option<String> {
        shell::validate_app_id(app_id).ok()?;
        // The build's hash, when Velocity installed it and nothing has
        // replaced it since (see `install.rs`).
        if let Some(fp) = crate::install::fingerprint(&*self.adb, device_id, app_id).await {
            return Some(fp);
        }
        let out = self
            .adb
            .run_device(
                device_id,
                &["shell", &shell::cmd(&["dumpsys", "package", app_id])],
            )
            .await
            .ok()?;
        let pick = |key: &str| {
            out.lines().map(str::trim).find_map(|l| {
                l.split_whitespace()
                    .find(|w| w.starts_with(key))
                    .map(String::from)
            })
        };
        let fp: Vec<String> = ["versionCode=", "versionName=", "lastUpdateTime="]
            .iter()
            .filter_map(|k| pick(k))
            .collect();
        (fp.len() >= 2).then(|| fp.join(" "))
    }

    /// Save the app's data dir as a tar (`run-as`, so debuggable builds);
    /// for other builds, an emulator snapshot of the whole device.
    async fn save_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        shell::validate_app_id(app_id)?;
        self.invalidate_cache(device_id).await;
        let _ = self.adb.stop_app(device_id, app_id).await;
        let dest = velocity_common::state_dir(device_id, app_id, name);
        create_private_dir(&dest)?;
        if self.debuggable(device_id, app_id).await {
            // Cache and native libs are rebuilt by the app; skip them.
            let tar = adb_capture(
                device_id,
                &[
                    "exec-out",
                    "run-as",
                    app_id,
                    "tar",
                    "-cf",
                    "-",
                    "--exclude=./cache",
                    "--exclude=./code_cache",
                    "--exclude=./lib",
                    ".",
                ],
            )
            .await?;
            if tar.len() < 512 {
                return Err(VelocityError::CommandFailed {
                    device_id: device_id.into(),
                    command: format!("saveState {name}"),
                    detail: String::from_utf8_lossy(&tar).trim().to_string(),
                });
            }
            let bytes = tar.len();
            // An app's data dir can run to hundreds of MB: write it off the
            // runtime's threads.
            let path = dest.join("data.tar");
            tokio::task::spawn_blocking(move || write_private(&path, &tar))
                .await
                .map_err(|e| VelocityError::Config(format!("saveState {name}: {e}")))??;
            info!(device_id, app_id, name, bytes, "app state saved");
            return Ok(());
        }
        let snap = snapshot_name(app_id, name);
        let out = self
            .adb
            .run_device(device_id, &["emu", "avd", "snapshot", "save", &snap])
            .await
            .unwrap_or_default();
        if !device_id.starts_with("emulator-") || out.contains("KO") || !out.contains("OK") {
            return Err(VelocityError::Config(format!(
                "saveState for {app_id} needs a debuggable build (run-as) or an emulator that can save snapshots ({})",
                out.trim()
            )));
        }
        write_private(&dest.join("snapshot"), snap.as_bytes())?;
        Ok(())
    }

    async fn restore_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        shell::validate_app_id(app_id)?;
        self.invalidate_cache(device_id).await;
        let saved = velocity_common::state_dir(device_id, app_id, name);
        let _ = self.adb.stop_app(device_id, app_id).await;
        if let Ok(tar) = tokio::fs::read(saved.join("data.tar")).await {
            // Empty the data dir (what the app may write; `lib` is the
            // system's), then unpack the saved copy.
            self.adb
                .run_device(device_id, &["shell", &clear_data_script(app_id)])
                .await?;
            adb_feed(
                device_id,
                &["exec-in", "run-as", app_id, "tar", "-xf", "-"],
                &tar,
            )
            .await?;
            info!(device_id, app_id, name, "app state restored");
            return Ok(());
        }
        if let Ok(snap) = tokio::fs::read_to_string(saved.join("snapshot")).await {
            return self.restore_snapshot(device_id, snap.trim()).await;
        }
        Err(VelocityError::Config(format!(
            "no saved state '{name}' for {app_id} on {device_id}; run a saveState step first"
        )))
    }

    /// Open a deep link through the VIEW intent (what tapping a link does).
    async fn navigate(&self, device_id: &str, url: &str) -> Result<()> {
        // A deep link starts the app too: same lock-screen check as a launch.
        let ready = self.ensure_unlocked(device_id).await?;
        self.invalidate_cache(device_id).await;
        self.keep_screen_ready(device_id, ready);
        self.mark_action(device_id).await;
        let quoted = shell_quote(url);
        let out = self
            .adb
            .run_device(
                device_id,
                &[
                    "shell",
                    "am",
                    "start",
                    "-a",
                    "android.intent.action.VIEW",
                    "-d",
                    &quoted,
                ],
            )
            .await?;
        if out.contains("Error") {
            return Err(VelocityError::CommandFailed {
                device_id: device_id.to_string(),
                command: format!("openLink {url}"),
                detail: out.trim().to_string(),
            });
        }
        Ok(())
    }

    async fn stop_app(&self, device_id: &str, app_id: &str) -> Result<()> {
        shell::validate_app_id(app_id)?;
        self.invalidate_cache(device_id).await;
        self.mark_action(device_id).await;
        self.adb.stop_app(device_id, app_id).await
    }

    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
        let tree = self.get_cached_hierarchy(device_id).await?;
        let screen = self.screen_bounds(device_id).await?;
        let opts = MatchOptions::default();

        find_element(&tree, selector, &opts, &screen)
            .cloned()
            .ok_or_else(|| VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
    }

    async fn find_elements(&self, device_id: &str, selector: &Selector) -> Result<Vec<Element>> {
        let tree = self.get_cached_hierarchy(device_id).await?;
        let screen = self.screen_bounds(device_id).await?;
        let opts = MatchOptions::default();

        Ok(find_all_elements(&tree, selector, &opts, &screen)
            .into_iter()
            .cloned()
            .collect())
    }

    async fn get_hierarchy(&self, device_id: &str) -> Result<Element> {
        // Always fetch fresh (the sync engine needs the live screen), and keep
        // the result: the element lookup that follows a settled sync can reuse
        // this tree instead of paying for another `uiautomator dump`.
        let xml = self.dump_xml(device_id).await?;
        let tree = parse_dump(device_id, &xml)?;
        *self.slot(device_id).lock().await = Some((tree.clone(), Instant::now()));
        Ok(tree)
    }

    fn has_hierarchy_cache(&self) -> bool {
        true
    }

    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        self.get_cached_hierarchy(device_id).await
    }

    async fn invalidate_hierarchy_cache(&self) {
        self.invalidate_all().await;
    }

    async fn tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let (x, y) = self.touch_point(device_id, &element.bounds).await?;
        self.invalidate_cache(device_id).await;
        self.tap_point(device_id, x, y, 1).await
    }

    async fn double_tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let (x, y) = self.touch_point(device_id, &element.bounds).await?;
        self.invalidate_cache(device_id).await;
        self.tap_point(device_id, x, y, 2).await
    }

    async fn long_press(&self, device_id: &str, element: &Element, duration_ms: u64) -> Result<()> {
        let (x, y) = self.touch_point(device_id, &element.bounds).await?;
        self.invalidate_cache(device_id).await;
        let pressed = self.tree.long_press(device_id, x, y, duration_ms).await;
        if let Some(r) = Self::server(device_id, "long press", pressed) {
            return r;
        }
        self.adb.long_press(device_id, x, y, duration_ms).await
    }

    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        // Focus the field first: typing used to go to whatever had focus.
        // Not when the previous inputText already focused it — a second tap
        // moves the caret into the middle of the text.
        let focused = self
            .focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied();
        if !element.bounds.is_empty() && focused != Some(element.bounds) {
            let before = if focus_check_enabled() {
                self.focus_before(device_id).await
            } else {
                None
            };
            let (x, y) = self.touch_point(device_id, &element.bounds).await?;
            self.tap_point(device_id, x, y, 1).await?;
            if focus_check_enabled() {
                self.verify_focus(device_id, element, before.as_ref())
                    .await?;
            }
        }
        self.invalidate_cache(device_id).await;
        // Long text in one action (ACTION_SET_TEXT): a key event per
        // character cost ~10 ms each. When the field refused it, type it key
        // by key; when it took it but reads back reformatted (a mask), it is
        // in: typing it again entered it twice (VEL-164).
        let fast = if text_mode().wants_fast(text) {
            let r = self.tree.set_text(device_id, text, true).await;
            // Sent but unanswered: typing it again could enter it twice.
            unknown_outcome(device_id, "set text", &r)?;
            match r {
                Some(Ok(SetText::Applied)) => true,
                Some(Ok(SetText::Differs(read_back))) => {
                    if !same_but_formatting(&read_back, text) {
                        return Err(VelocityError::CommandFailed {
                            device_id: device_id.to_string(),
                            command: "tree server set text".into(),
                            detail: differs_detail(element, text, &read_back),
                        });
                    }
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        if !fast {
            // Through the server any Unicode types; `input text` can't do
            // emoji or most non-Latin scripts.
            match Self::server(device_id, "text", self.tree.text(device_id, text).await) {
                Some(r) => r?,
                None => {
                    adb_text_allowed(device_id, text, element.obscured)?;
                    self.adb.input_text(device_id, text).await?
                }
            }
        }
        self.focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_string(), element.bounds);
        Ok(())
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()> {
        let (x, y) = self.touch_point(device_id, &element.bounds).await?;
        self.invalidate_cache(device_id).await;
        self.tap_point(device_id, x, y, 1).await?;
        // One action empties the field (ACTION_SET_TEXT ""), not one DEL
        // per character. Falls back to the keys when the field refuses.
        if text_mode() != TextMode::Keys {
            let r = self.tree.set_text(device_id, "", false).await;
            unknown_outcome(device_id, "clear text", &r)?;
            // A mask may show its placeholder ("(___) ___-____") when empty.
            let cleared = match &r {
                Some(Ok(SetText::Applied)) => true,
                Some(Ok(SetText::Differs(read_back))) => {
                    !read_back.chars().any(char::is_alphanumeric)
                }
                _ => false,
            };
            if cleared {
                self.invalidate_cache(device_id).await;
                return Ok(());
            }
        }
        // Caret to the end, then one backspace per character. (MOVE_HOME +
        // a single DEL deleted nothing.)
        let n = element
            .text
            .as_deref()
            .map(|t| t.chars().count())
            .unwrap_or(0)
            .max(1);
        let end = self.tree.key(device_id, 123).await;
        unknown_outcome(device_id, "key", &end)?;
        if end.is_some() {
            for _ in 0..n {
                self.key_code(device_id, 67).await?;
            }
            self.invalidate_cache(device_id).await;
            return Ok(());
        }
        let mut keys = vec![
            "shell".to_string(),
            "input".into(),
            "keyevent".into(),
            "123".into(),
        ];
        keys.extend(std::iter::repeat_n("67".to_string(), n));
        let args: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.adb.run_device(device_id, &args).await?;
        self.invalidate_cache(device_id).await;
        Ok(())
    }

    async fn swipe(&self, device_id: &str, direction: Direction) -> Result<()> {
        self.invalidate_cache(device_id).await;
        self.swipe_direction_impl(device_id, direction).await
    }

    async fn swipe_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.invalidate_cache(device_id).await;
        self.swipe_points(device_id, from, to, 300).await
    }

    async fn drag(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        self.invalidate_cache(device_id).await;
        // `input motionevent` (Android 10+): one finger held down across the
        // commands, so the press, the hold, the slow move and the rest at
        // the end are ours. Older systems: `input draganddrop`.
        let script = drag_script(from, to, hold_ms, move_ms);
        let out = self.adb.batch_shell(device_id, &[script.as_str()]).await?;
        if out.contains("Error") || out.contains("Unknown") || out.contains("usage") {
            let cmd = format!(
                "input draganddrop {} {} {} {} {}",
                from.0,
                from.1,
                to.0,
                to.1,
                hold_ms + move_ms
            );
            self.adb.batch_shell(device_id, &[cmd.as_str()]).await?;
        }
        Ok(())
    }

    async fn press_key(&self, device_id: &str, key: Key) -> Result<()> {
        self.invalidate_cache(device_id).await;
        // Enter in a text field: the keyboard's own action (Search/Go/Send),
        // which a raw ENTER key misses in many Compose and WebView fields.
        // ENTER when no field takes it (or VELOCITY_ANDROID_IME_ENTER=0).
        if key == Key::Enter && ime_enter_enabled() {
            let r = self.tree.ime_enter(device_id).await;
            unknown_outcome(device_id, "ime action", &r)?;
            if matches!(r, Some(Ok(true))) {
                return Ok(());
            }
        }
        let Some(code) = Self::key_to_keycode(key.clone()) else {
            return Err(VelocityError::Config(format!(
                "pressKey: {} is not supported on Android",
                key.name()
            )));
        };
        self.key_code(device_id, code).await
    }

    /// Media sessions and call state in one adb round-trip.
    async fn system_state(&self, device_id: &str) -> Result<velocity_common::SystemState> {
        let out = self
            .adb
            .run_device(
                device_id,
                &[
                    "shell",
                    "dumpsys media_session; echo VELOCITY-CALLS; dumpsys telephony.registry | grep mCallState=",
                ],
            )
            .await?;
        parse_system_state(&out).map_err(|detail| VelocityError::CommandFailed {
            device_id: device_id.to_string(),
            command: "dumpsys media_session".into(),
            detail,
        })
    }

    /// `VELOCITY_ANDROID_SCROLL=semantic`: scroll lists by their own scroll
    /// action (no fling, stops at the end) instead of a touch drag.
    async fn scroll(&self, device_id: &str, finger: Direction) -> Result<()> {
        if semantic_scroll() {
            let (w, h) = self.screen_size(device_id).await?;
            if self
                .scroll_semantic(device_id, (w / 2, h / 2), finger)
                .await?
            {
                return Ok(());
            }
        }
        self.swipe(device_id, finger).await
    }

    async fn scroll_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        if semantic_scroll() {
            let mid = ((from.0 + to.0) / 2, (from.1 + to.1) / 2);
            let finger = if (to.1 - from.1).abs() >= (to.0 - from.0).abs() {
                if to.1 < from.1 {
                    Direction::Up
                } else {
                    Direction::Down
                }
            } else if to.0 < from.0 {
                Direction::Left
            } else {
                Direction::Right
            };
            if self.scroll_semantic(device_id, mid, finger).await? {
                return Ok(());
            }
        }
        self.swipe_coords(device_id, from, to).await
    }

    async fn tap_xy(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        self.invalidate_cache(device_id).await;
        self.tap_point(device_id, x, y, 1).await
    }

    async fn input_raw(&self, device_id: &str, text: &str) -> Result<()> {
        self.invalidate_cache(device_id).await;
        match Self::server(device_id, "text", self.tree.text(device_id, text).await) {
            Some(r) => r,
            None => {
                adb_text_allowed(device_id, text, false)?;
                self.adb.input_text(device_id, text).await
            }
        }
    }

    /// From the tree server (UiAutomation, no `screencap` process), else adb.
    async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        match self.tree.screenshot(device_id, false).await {
            Some(Ok(png)) if png.starts_with(b"\x89PNG") => Ok(png),
            _ => self.adb.screenshot(device_id).await,
        }
    }

    /// A lite dump (A-10): what a settle compares, not cached for lookups.
    async fn get_hierarchy_for_compare(&self, device_id: &str) -> Result<Element> {
        match self.tree.dump_lite(device_id).await {
            Some(xml) => parse_dump(device_id, &xml),
            None => self.get_hierarchy(device_id).await,
        }
    }

    /// Settle from accessibility events on the device (A-4), with
    /// `VELOCITY_ANDROID_EVENT_SETTLE=1`. Off by default: Android throttles
    /// content-change events (~100 ms), so the event timeline trails the
    /// screen and this measured 30–50 ms slower per transition than polling
    /// lite dumps on an API 35 emulator. It saves device load, not time.
    async fn settle_on_device(
        &self,
        device_id: &str,
        reaction_ms: u64,
        quiet_ms: u64,
        cap_ms: u64,
    ) -> Option<DeviceSettle> {
        if !matches!(
            velocity_common::env::var("VELOCITY_ANDROID_EVENT_SETTLE").as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        ) {
            return None;
        }
        match self
            .tree
            .settle(device_id, reaction_ms, quiet_ms, cap_ms)
            .await?
        {
            Ok((settled, reacted, elapsed_ms)) => Some(DeviceSettle {
                settled,
                reacted,
                elapsed_ms,
            }),
            Err(e) => {
                debug!(device_id, "device settle failed: {e}");
                None
            }
        }
    }

    async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)> {
        self.get_screen_size_cached(device_id).await
    }

    async fn get_element_text(&self, device_id: &str, element: &Element) -> Result<String> {
        let tree = self.get_cached_hierarchy(device_id).await?;
        velocity_common::selector_match::locate(&tree, element)
            .map(|e| e.text.clone().unwrap_or_default())
            .ok_or_else(|| VelocityError::ElementNotFound {
                selector: describe_identity(element),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
    }

    async fn is_element_visible(&self, device_id: &str, element: &Element) -> Result<bool> {
        let tree = self.get_cached_hierarchy(device_id).await?;
        let screen = self.screen_bounds(device_id).await?;
        Ok(
            // The shared rule (D-1), `visibility.minArea` included.
            velocity_common::selector_match::locate(&tree, element)
                .is_some_and(|found| velocity_common::selector_match::shown_on(found, &screen)),
        )
    }

    async fn collect_resource_metrics(
        &self,
        device_id: &str,
        package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        shell::validate_app_id(package)?;
        self.adb.collect_resource_metrics(device_id, package).await
    }
}

/// `dir` (and its parents), readable only by this user: a saved state is
/// the app's private data (VEL-359).
fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let err = |e: std::io::Error| VelocityError::Config(format!("{}: {e}", dir.display()));
    std::fs::create_dir_all(dir).map_err(err)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(err)
}

/// Write `bytes` to `path` with mode 0600, also when it already exists.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err = |e: std::io::Error| VelocityError::Config(format!("{}: {e}", path.display()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(err)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(err)?;
    f.write_all(bytes).map_err(err)
}

/// Whether a field reading `read_back` after `text` was appended to it holds
/// that text, up to what an input mask changes: spacing, separators and
/// letter case (`4111 1111 1111 1111` for `4111111111111111`).
fn same_but_formatting(read_back: &str, text: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    norm(read_back).ends_with(&norm(text))
}

/// An error when a tree-server command that changes the device was sent
/// and never answered: doing it another way too could do it twice.
fn unknown_outcome<T>(
    device_id: &str,
    what: &str,
    r: &Option<std::result::Result<T, String>>,
) -> Result<()> {
    match r {
        Some(Err(detail)) if crate::tree_server::is_outcome_unknown(detail) => {
            Err(VelocityError::CommandFailed {
                device_id: device_id.to_string(),
                command: format!("tree server {what}"),
                detail: detail.clone(),
            })
        }
        _ => Ok(()),
    }
}

/// The tree in `xml`; a dump that isn't a whole hierarchy is a failed
/// command on `device_id`.
fn parse_dump(device_id: &str, xml: &str) -> Result<Element> {
    parse_hierarchy_v2(xml).map_err(|e| crate::adb::on_device(e, device_id))
}

/// An emulator snapshot name for `app_id`'s state `name`. It goes to the
/// emulator console (`adb emu avd snapshot save …`) as part of a command
/// line, so it keeps only `[A-Za-z0-9_-]`: a space or newline in `name`
/// would add arguments or a whole console command. When that changed
/// `name`, a hash of the original is appended, so `login.done`,
/// `login done` and `login_done` stay three snapshots, and an all-non-Latin
/// name doesn't collapse to underscores.
fn snapshot_name(app_id: &str, name: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let safe = clean(name);
    let base = format!("velocity-{}-{safe}", clean(app_id));
    if safe == name {
        base
    } else {
        format!("{base}-{:08x}", fnv1a(name.as_bytes()))
    }
}

/// 32-bit FNV-1a: a hash that is the same in every build (std's isn't
/// promised to be), for names saved across runs.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
    })
}

/// A snapshot name read from config or a saved state, checked before it is
/// put on the emulator console's command line.
fn check_snapshot_name(name: &str) -> Result<()> {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(VelocityError::Config(format!(
            "invalid emulator snapshot name {name:?}: use letters, digits, '-', '_' and '.'"
        )))
    }
}

/// Empty `app_id`'s data dir as the app (`run-as`), except `lib` (the
/// system's). The appId is quoted, and the loop is one quoted `sh -c` word.
fn clear_data_script(app_id: &str) -> String {
    shell::cmd(&[
        "run-as",
        app_id,
        "sh",
        "-c",
        r#"for f in * .[!.]*; do [ "$f" = lib ] || rm -rf "$f"; done"#,
    ])
}

fn adb_path() -> String {
    velocity_common::env::var("VELOCITY_ADB_PATH").unwrap_or_else(|_| "adb".to_string())
}

/// Raw stdout of `adb -s <device> <args>` (binary safe).
async fn adb_capture(device_id: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(adb_path());
    cmd.args(crate::adb::target_args(device_id))
        .args(args)
        .stdin(Stdio::null());
    let out = velocity_common::process::output_with_timeout(
        &mut cmd,
        velocity_common::process::LONG_TIMEOUT,
    )
    .await
    .map_err(|e| VelocityError::CommandFailed {
        device_id: device_id.into(),
        command: args.join(" "),
        detail: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(VelocityError::CommandFailed {
            device_id: device_id.into(),
            command: args.join(" "),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out.stdout)
}

/// `adb -s <device> <args>` with `input` on stdin (`exec-in`).
async fn adb_feed(device_id: &str, args: &[&str], input: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new(adb_path())
        .args(crate::adb::target_args(device_id))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| VelocityError::Config(format!("adb: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .await
            .map_err(|e| VelocityError::Config(format!("adb stdin: {e}")))?;
    }
    let out = tokio::time::timeout(
        velocity_common::process::LONG_TIMEOUT,
        child.wait_with_output(),
    )
    .await
    .map_err(|_| {
        VelocityError::Config(format!(
            "adb exec-in timed out after {:?}",
            velocity_common::process::LONG_TIMEOUT
        ))
    })?
    .map_err(|e| VelocityError::Config(format!("adb: {e}")))?;
    if !out.status.success() {
        return Err(VelocityError::CommandFailed {
            device_id: device_id.into(),
            command: args.join(" "),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Quote for the device shell: `&` and `?` in a URL would otherwise end
/// or glob the command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// How long a screen size is trusted before it's read again.
const SCREEN_SIZE_TTL: Duration = Duration::from_secs(5);

/// A swipe through the centre covering half the screen along its axis
/// (vertical swipes by height, horizontal ones by width), always on screen.
fn swipe_line(w: i32, h: i32, direction: Direction) -> (i32, i32, i32, i32) {
    let (cx, cy) = (w / 2, h / 2);
    // Vertical swipes run 30%–70% of the height: from 25%, a landscape
    // phone's swipe starts on the toolbar above the list (Settings' list
    // begins at y=284 of 1080) and nothing scrolls.
    let (dx, dy) = (w / 4, h / 5);
    match direction {
        Direction::Up => (cx, cy + dy, cx, cy - dy),
        Direction::Down => (cx, cy - dy, cx, cy + dy),
        Direction::Left => (cx + dx, cy, cx - dx, cy),
        Direction::Right => (cx - dx, cy, cx + dx, cy),
    }
}

/// The exception of the last `FATAL EXCEPTION` block for `app_id` in
/// Android's crash log, as "java.lang.Foo: message".
fn crash_from_log(log: &str, app_id: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let mut found = None;
    for (i, l) in lines.iter().enumerate() {
        if l.contains(&format!("Process: {app_id},"))
            || l.trim_end().ends_with(&format!("Process: {app_id}"))
        {
            // The exception is the first line after the process line that names one.
            found = lines[i + 1..]
                .iter()
                .take(4)
                .map(|x| x.split_once("): ").map(|(_, m)| m).unwrap_or(x).trim())
                .find(|m| m.contains("Exception") || m.contains("Error"))
                .map(String::from);
        }
    }
    found
}

/// `date '+%s %N %Y-%m-%d %H:%M:%S'` → (`<secs>.<ms>`, `YYYY-MM-DD
/// HH:MM:SS.mmm`). Milliseconds are `000` when `date` has no `%N`; the wall
/// time is `None` when the format isn't understood (very old toolbox).
fn parse_launch_mark(out: &str) -> (Option<String>, Option<String>) {
    let words: Vec<&str> = out.split_whitespace().collect();
    let Some(secs) = words.first().and_then(|w| w.parse::<u64>().ok()) else {
        return (None, None);
    };
    let ms = words
        .get(1)
        .filter(|n| n.len() >= 3 && n.chars().all(|c| c.is_ascii_digit()))
        .map_or("000", |n| &n[..3]);
    let wall = match (words.get(2), words.get(3)) {
        (Some(d), Some(t)) if d.len() == 10 && t.len() == 8 => Some(format!("{d} {t}.{ms}")),
        _ => None,
    };
    (Some(format!("{secs}.{ms}")), wall)
}

/// The lines of a `logcat -v time` dump from `wall` (`YYYY-MM-DD
/// HH:MM:SS.mmm`) on, with the time stripped, so they read as `-v brief`.
fn time_log_since(log: &str, wall: &str) -> String {
    // `-v time` prints `MM-DD HH:MM:SS.mmm` (no year).
    let mark = wall.get(5..).unwrap_or(wall);
    let mut out = String::new();
    for line in log.lines() {
        let Some((stamp, rest)) = line.get(..18).zip(line.get(18..)) else {
            continue;
        };
        if stamp >= mark {
            out.push_str(rest.trim_start());
            out.push('\n');
        }
    }
    out
}

/// The app's crash in `dumpsys activity exit-info <pkg>`, as "process 4321
/// ended: APP CRASH(EXCEPTION) (description)": a record of a crash, native
/// crash, ANR or initialization failure (reasons 4-7) of any of the
/// package's processes, timestamped at or after `since` (the launch's wall
/// clock). Records from before the launch never match, whatever their pid
/// (M-6). `pid`, the launch's, only picks between several: then the newest.
fn crash_from_exit_info(out: &str, since: &str, pid: Option<u32>) -> Option<String> {
    let mut records: Vec<String> = Vec::new();
    for l in out.lines().map(str::trim) {
        if l.starts_with("ApplicationExitInfo") {
            records.push(String::new());
        } else if let Some(r) = records.last_mut() {
            r.push(' ');
            r.push_str(l);
        }
    }
    let field = |r: &str, key: &str| -> Option<String> {
        let at = r.find(&format!(" {key}="))? + key.len() + 2;
        Some(r[at..].split_whitespace().next()?.to_string())
    };
    let mut best: Option<(bool, String, String)> = None;
    for r in &records {
        // `timestamp=2026-10-15 01:02:03.456`: a date and a time.
        let Some(at) = r.find(" timestamp=") else {
            continue;
        };
        let stamp: String = r[at + 11..]
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        if stamp.as_str() < since {
            continue;
        }
        let Some(reason) = field(r, "reason").and_then(|v| v.parse::<u32>().ok()) else {
            continue;
        };
        // ApplicationExitInfo.REASON_CRASH, _CRASH_NATIVE, _ANR,
        // _INITIALIZATION_FAILURE.
        if !(4..=7).contains(&reason) {
            continue;
        }
        let rec_pid = field(r, "pid").unwrap_or_default();
        let name = r
            .split_once("reason=")
            .and_then(|(_, r)| r.split_once('(')?.1.split_once(')'))
            .map(|(n, _)| n.to_string())
            .unwrap_or_else(|| format!("reason {reason}"));
        let description = r
            .split_once("description=")
            .map(|(_, d)| d.split(" state=").next().unwrap_or(d).trim())
            .filter(|d| !d.is_empty() && *d != "null");
        let text = match description {
            Some(d) => format!("process {rec_pid} ended: {name} ({d})"),
            None => format!("process {rec_pid} ended: {name}"),
        };
        let is_launch_pid = pid.is_some_and(|p| rec_pid == p.to_string());
        let better = match &best {
            None => true,
            Some((bp, bs, _)) => (is_launch_pid, &stamp) > (*bp, bs),
        };
        if better {
            best = Some((is_launch_pid, stamp, text));
        }
    }
    best.map(|(_, _, t)| t)
}

/// The `pkg/activity` line of `cmd package resolve-activity --brief`.
fn parse_component(out: &str, package: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .rev()
        .find(|l| l.starts_with(&format!("{package}/")))
        .map(String::from)
}

fn describe_identity(el: &Element) -> String {
    if el.platform_id.is_empty() {
        format!(
            "{} {:?} at {:?}",
            el.element_type,
            el.label.as_deref().unwrap_or(""),
            el.bounds
        )
    } else {
        format!("id={:?}", el.platform_id)
    }
}

/// `dumpsys media_session` (then `VELOCITY-CALLS` and telephony's
/// `mCallState=` lines) as a [`velocity_common::SystemState`]. A session
/// block starts at `<tag> <package>/<tag> (userId=N)`; it plays when
/// `active=true` and `state=PlaybackState {state=PLAYING(3)` (`state=3` on
/// older Android); `metadata: … description=<title>, …` gives the title.
fn parse_system_state(dump: &str) -> std::result::Result<velocity_common::SystemState, String> {
    let (media, calls) = dump.split_once("VELOCITY-CALLS").unwrap_or((dump, ""));
    // Output this parser doesn't know (another Android version or OEM, or
    // "Can't find service: media_session") read as "nothing playing", so
    // `mediaPlaying: false` passed whatever was playing (VEL-S44).
    let understood = media.contains("Sessions Stack")
        || media.contains("MEDIA SESSION SERVICE")
        || media
            .lines()
            .any(|l| l.trim().contains("(userId=") && l.trim().ends_with(')'));
    if !understood {
        let first = media.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(format!(
            "media session state not understood (dumpsys media_session said {:?})",
            first.unwrap_or("nothing")
        ));
    }
    let mut state = velocity_common::SystemState::default();
    let mut session: Option<(bool, bool, Option<String>)> = None; // (active, playing, title)
    let flush = |s: &mut Option<(bool, bool, Option<String>)>,
                 st: &mut velocity_common::SystemState| {
        if let Some((true, true, title)) = s.take() {
            st.media_playing = true;
            if let Some(t) = title {
                st.media_titles.push(t);
            }
        }
    };
    for line in media.lines() {
        let t = line.trim();
        if t.contains("(userId=") && t.ends_with(')') {
            flush(&mut session, &mut state);
            session = Some((false, false, None));
            continue;
        }
        let Some(cur) = session.as_mut() else {
            continue;
        };
        if t == "active=true" {
            cur.0 = true;
        } else if let Some(rest) = t.strip_prefix("state=PlaybackState {state=") {
            cur.1 = rest.starts_with("PLAYING") || rest.starts_with("3,");
        } else if let Some(i) = t.find("description=") {
            let title = t[i + "description=".len()..]
                .split(", ")
                .next()
                .unwrap_or("")
                .trim();
            if !title.is_empty() && title != "null" {
                cur.2 = Some(title.to_string());
            }
        }
    }
    flush(&mut session, &mut state);
    state.in_call = calls
        .lines()
        .filter_map(|l| l.trim().strip_prefix("mCallState="))
        .any(|v| v.trim().parse::<u32>().is_ok_and(|n| n > 0));
    Ok(state)
}

/// How `inputText` / `clearText` put text in (`VELOCITY_ANDROID_TEXT`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum TextMode {
    /// A key event per character (what a person's keyboard sends).
    Keys,
    /// One ACTION_SET_TEXT, read back, else keys.
    Fast,
    /// Fast for text of [`FAST_TEXT_MIN`] characters or more (default).
    Auto,
}

/// Shorter text is typed key by key in `auto` mode.
const FAST_TEXT_MIN: usize = 16;

impl TextMode {
    fn wants_fast(self, text: &str) -> bool {
        // A newline or tab is the Enter or Tab key, as in the key-event and
        // `adb shell input` paths; SET_TEXT would put the raw character in
        // and never fire the keyboard action.
        if text.contains(['\n', '\t']) {
            return false;
        }
        match self {
            Self::Keys => false,
            Self::Fast => true,
            Self::Auto => text.chars().count() >= FAST_TEXT_MIN,
        }
    }
}

/// Why a SETTEXT read back different, for the step's error. A secret cut
/// by `maxLength` or reformatted reads back as a prefix or variant of
/// itself, which redaction (exact values) misses, so for a password field
/// or a registered secret only the lengths are told (audit 2026-11 AND-4).
fn differs_detail(element: &Element, text: &str, read_back: &str) -> String {
    let secret = element.obscured || velocity_common::redact::redact(text) != text;
    if secret {
        format!(
            "the field took the text ({} characters) but now reads {} characters; not typed \
             again",
            text.chars().count(),
            read_back.chars().count()
        )
    } else {
        format!("the field took the text but now reads {read_back:?}; not typed again")
    }
}

/// D-51 (VEL-S35): the `adb shell input text` fallback (no tree server)
/// takes ASCII only, and never a secret: the text becomes a device shell
/// command line, which any process on the device can read while it runs
/// and adb may log. A password field (`obscured`), a registered secret or
/// a secret-shaped value is refused, naming the tree server.
fn adb_text_allowed(device_id: &str, text: &str, obscured: bool) -> Result<()> {
    let refuse = |why: &str| VelocityError::CommandFailed {
        device_id: device_id.to_string(),
        command: "input text".into(),
        detail: format!(
            "{why} is not typed with `adb shell input text` (it would be a device \
             command line); typing it needs Velocity's tree server, which is not \
             available on this device"
        ),
    };
    if let Some(c) = text.chars().find(|c| !c.is_ascii()) {
        return Err(refuse(&format!("{c:?} (not ASCII)")));
    }
    if obscured {
        return Err(refuse("text for a password field"));
    }
    if velocity_common::redact::redact(text) != text {
        return Err(refuse("a secret"));
    }
    Ok(())
}

/// D-5: whether `inputText` checks that its tap moved the focus to the
/// target. On unless `VELOCITY_ANDROID_FOCUS_CHECK=off` (or `0`/`false`).
fn focus_check_enabled() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_ANDROID_FOCUS_CHECK")
            .as_deref()
            .map(str::trim),
        Ok("0" | "off" | "false")
    )
}

fn text_mode() -> TextMode {
    match velocity_common::env::var("VELOCITY_ANDROID_TEXT").as_deref() {
        Ok("keys") => TextMode::Keys,
        Ok("fast") => TextMode::Fast,
        _ => TextMode::Auto,
    }
}

fn ime_enter_enabled() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_ANDROID_IME_ENTER").as_deref(),
        Ok("0") | Ok("false")
    )
}

fn semantic_scroll() -> bool {
    velocity_common::env::var("VELOCITY_ANDROID_SCROLL").as_deref() == Ok("semantic")
}

/// The focused window (a `dumpsys window` `mCurrentFocus=` line) shows a
/// launch got past the splash: an activity of `package`, or anything that
/// isn't `package`'s splash window (or nothing at all, unknown).
fn launched(focus: &str, package: &str) -> bool {
    let Some(line) = default_display_focus(focus) else {
        return true;
    };
    if line.contains("mCurrentFocus=null") {
        return false;
    }
    if line.contains(&format!("{package}/")) {
        return true;
    }
    !line.contains("Splash Screen")
}

/// The pid `wait_for_activity`'s check read, when the focused window is
/// an activity of `package` (its process is up, so the pid is the launch's).
fn focused_pid(check: &str, package: &str) -> Option<u32> {
    default_display_focus(check)?
        .contains(&format!("{package}/"))
        .then(|| {
            check
                .lines()
                .find_map(|l| l.trim().strip_prefix(PID_MARK))?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .flatten()
}

/// The `mCurrentFocus=` line of display 0 in `dumpsys window displays`
/// output (filtered to `mDisplayId=` and `mCurrentFocus=` lines). Other
/// displays come first when they exist: the screen video's mirroring
/// virtual display has no focus (`null`), and reading it made every launch
/// look stuck at its splash. Output without display headers (older
/// filters, tests) gives its first focus line.
fn default_display_focus(dump: &str) -> Option<&str> {
    let mut display: Option<bool> = None;
    for line in dump.lines() {
        if let Some(rest) = line.trim().strip_prefix("Display: mDisplayId=") {
            let id = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("");
            display = Some(id == "0");
        } else if line.contains("mCurrentFocus=") && display != Some(false) {
            return Some(line);
        }
    }
    None
}

/// Awake, per `dumpsys power` (unknown counts as awake: never block on it).
fn awake(state: &str) -> bool {
    !state.contains("mWakefulness=") || state.contains("mWakefulness=Awake")
}

/// Awake, no keyguard, no pulled-down notification shade: an app launched
/// now is on screen.
fn screen_ready(state: &str) -> bool {
    awake(state) && !keyguard_showing(state) && !shade_open(state)
}

/// The notification shade has focus with no keyguard up. An open shade sat
/// over Settings on a POS tablet and every lookup failed "not found"; on
/// Android 12+ the keyguard is drawn in the same window, hence the check.
fn shade_open(state: &str) -> bool {
    !keyguard_showing(state)
        && default_display_focus(state).is_some_and(|l| l.contains("NotificationShade"))
}

/// Whether `dumpsys window policy` shows the keyguard up.
fn keyguard_showing(dump: &str) -> bool {
    dump.lines().any(|l| {
        let l = l.trim();
        l == "mIsShowing=true" || l == "isKeyguardShowing=true"
    })
}

#[cfg(test)]
mod launch_tests {
    use super::parse_component;

    #[test]
    fn the_focus_check_gives_the_pid_only_with_the_app_in_front() {
        use super::focused_pid;
        let p = "com.velocity.lab";
        let up = "  mCurrentFocus=Window{1 u0 com.velocity.lab/com.velocity.lab.MainActivity}\nvelocity-pid:4321 4400\n";
        assert_eq!(focused_pid(up, p), Some(4321));
        let splash =
            "  mCurrentFocus=Window{2 u0 Splash Screen com.velocity.lab}\nvelocity-pid:4321\n";
        assert_eq!(focused_pid(splash, p), None);
        let launcher =
            "  mCurrentFocus=Window{3 u0 com.android.launcher3/.Launcher}\nvelocity-pid:\n";
        assert_eq!(focused_pid(launcher, p), None);
        assert_eq!(focused_pid("velocity-pid:4321\n", p), None);
        let no_pidof = "  mCurrentFocus=Window{1 u0 com.velocity.lab/com.velocity.lab.MainActivity}\nvelocity-pid:\n";
        assert_eq!(focused_pid(no_pidof, p), None);
    }

    fn win(layer: i32, kind: i32, y: i32, h: i32) -> crate::tree_server::Window {
        crate::tree_server::Window {
            layer,
            kind,
            bounds: velocity_common::Rect {
                x: 0,
                y,
                width: 1080,
                height: h,
            },
            package: "pkg".into(),
            touchable: None,
            title: String::new(),
            active: None,
        }
    }

    /// Review 09 #12: in freeform / desktop windowing another app's window
    /// can float over the app under test. With protocol 19's active flag it
    /// covers like any window; the app's own popups don't.
    #[test]
    fn another_apps_window_in_front_covers_the_target() {
        let mut other = win(5, 1, 1000, 600);
        other.package = "com.other".into();
        other.active = Some(false);
        let mut app = win(1, 1, 0, 2400);
        app.package = "com.app".into();
        app.active = Some(true);
        let under = velocity_common::Rect {
            x: 100,
            y: 1100,
            width: 400,
            height: 200,
        };
        let cover = super::uncovered_point(&[other.clone(), app.clone()], &under).unwrap_err();
        assert_eq!(cover.package, "com.other");
        assert_eq!(cover.kind_name(), "app window");
        // Partly under it: tapped on the free part.
        let half = velocity_common::Rect {
            x: 100,
            y: 900,
            width: 400,
            height: 200,
        };
        let (_, y) = super::uncovered_point(&[other.clone(), app.clone()], &half).unwrap();
        assert!(y < 1000, "{y}");
        // The app's own popup window in front is part of the app.
        let mut popup = other.clone();
        popup.package = "com.app".into();
        assert!(super::uncovered_point(&[popup, app.clone()], &under).is_ok());
        // An older server names no active window: as before, app windows
        // never cover.
        other.active = None;
        app.active = None;
        assert!(super::uncovered_point(&[other, app], &under).is_ok());
    }

    #[test]
    fn taps_avoid_the_keyboard_and_name_it_when_fully_covered() {
        // Keyboard over the bottom 900 px, then the app.
        let windows = vec![win(10, 2, 1500, 900), win(1, 1, 0, 2400)];
        let button = velocity_common::Rect {
            x: 100,
            y: 1400,
            width: 400,
            height: 200,
        };
        let (_, y) = super::uncovered_point(&windows, &button).unwrap();
        assert!(y < 1500, "tapped the uncovered top part, got y={y}");
        let hidden = velocity_common::Rect {
            x: 100,
            y: 1700,
            width: 400,
            height: 200,
        };
        let cover = super::uncovered_point(&windows, &hidden).unwrap_err();
        assert_eq!(cover.kind_name(), "keyboard");
        let free = velocity_common::Rect {
            x: 100,
            y: 200,
            width: 400,
            height: 200,
        };
        assert_eq!(super::uncovered_point(&windows, &free).unwrap(), (300, 300));
        // Review 09 #5: SystemUI's rounded-corner strip over the bottom
        // takes no touches, so a button under it is tapped at its centre.
        let mut decor = win(40, 3, 2280, 120);
        decor.title = "ScreenDecorOverlayBottom".into();
        let mut untouchable = win(41, 3, 2280, 120);
        untouchable.touchable = Some(false);
        let bottom = velocity_common::Rect {
            x: 100,
            y: 2290,
            width: 400,
            height: 100,
        };
        let app = win(1, 1, 0, 2400);
        assert_eq!(
            super::uncovered_point(&[decor, untouchable, app], &bottom).unwrap(),
            (300, 2340)
        );
    }

    #[test]
    fn the_front_app_and_the_system_bars_come_from_the_windows() {
        let with = |mut w: crate::tree_server::Window, pkg: &str| {
            w.package = pkg.into();
            w
        };
        let windows = vec![
            with(win(30, 3, 0, 63), "com.android.systemui"),
            with(win(29, 3, 2274, 126), "com.android.systemui"),
            with(
                win(10, 2, 1500, 900),
                "com.google.android.inputmethod.latin",
            ),
            with(win(2, 1, 0, 2400), "com.android.launcher3"),
            with(win(1, 1, 0, 2400), "com.app"),
        ];
        assert_eq!(super::front_app(&windows), Some("com.android.launcher3"));
        assert_eq!(super::front_app(&windows[4..]), Some("com.app"));
        let bars = super::system_bar_rects(&windows);
        assert_eq!(bars.len(), 2, "{bars:?}");
        assert_eq!((bars[0].y, bars[0].height), (0, 63));
        // A pulled-down shade is not a bar.
        let shade = vec![with(win(40, 3, 0, 2400), "com.android.systemui")];
        assert!(super::system_bar_rects(&shade).is_empty());
    }

    #[test]
    fn crash_log_yields_the_exception() {
        let log = "--------- beginning of crash\n\
E/AndroidRuntime( 4321): FATAL EXCEPTION: main\n\
E/AndroidRuntime( 4321): Process: com.velocity.lab, PID: 4321\n\
E/AndroidRuntime( 4321): java.lang.IllegalStateException: VelocityLab crash button\n\
E/AndroidRuntime( 4321): \tat com.velocity.lab.MainActivity.crash(MainActivity.kt:1)\n";
        assert_eq!(
            super::crash_from_log(log, "com.velocity.lab").as_deref(),
            Some("java.lang.IllegalStateException: VelocityLab crash button")
        );
        assert_eq!(super::crash_from_log(log, "com.other"), None);
        assert_eq!(super::crash_from_log("", "com.velocity.lab"), None);
    }

    #[test]
    fn swipes_stay_on_screen_in_both_axes() {
        use velocity_common::Direction;
        for d in [
            Direction::Up,
            Direction::Down,
            Direction::Left,
            Direction::Right,
        ] {
            let (x1, y1, x2, y2) = super::swipe_line(1080, 2400, d);
            for (x, y) in [(x1, y1), (x2, y2)] {
                assert!(
                    (0..1080).contains(&x) && (0..2400).contains(&y),
                    "{d:?}: ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn vertical_swipes_start_below_a_landscape_toolbar() {
        use velocity_common::Direction;
        // Landscape 2340x1080: Settings' list starts at y=284.
        let (_, y1, _, y2) = super::swipe_line(2340, 1080, Direction::Down);
        assert!(y1 > 284 && y2 < 1080, "{y1}->{y2}");
        let (_, y1, _, y2) = super::swipe_line(2340, 1080, Direction::Up);
        assert!(y2 > 284 && y1 < 1080, "{y1}->{y2}");
    }

    #[test]
    fn urls_are_quoted_for_the_device_shell() {
        assert_eq!(super::shell_quote("app://x?a=1&b=2"), "'app://x?a=1&b=2'");
        assert_eq!(super::shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn reads_the_component_from_resolve_activity() {
        let out = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.android.settings/.Settings\n";
        assert_eq!(
            parse_component(out, "com.android.settings").as_deref(),
            Some("com.android.settings/.Settings")
        );
        assert_eq!(parse_component("No activity found\n", "com.x"), None);
    }
}

#[cfg(test)]
mod app_id_tests {
    use super::*;
    use crate::test_support::FakeAdb;

    const EXIT_INFO: &str = "ACTIVITY MANAGER PROCESS EXIT INFO (dumpsys activity exit-info)
  package: com.velocity.lab
    Historical Process Exit for uid=10123
        ApplicationExitInfo #0:
          timestamp=2026-10-15 01:02:03.456 pid=4321 realUid=10123 packageUid=10123 definingUid=10123 user=0
          process=com.velocity.lab reason=4 (APP CRASH(EXCEPTION)) subreason=0 (UNKNOWN) status=0
          importance=100 pss=0.00 rss=0.00 description=crash state=empty trace=null
        ApplicationExitInfo #1:
          timestamp=2026-10-15 01:00:00.000 pid=1111 realUid=10123 packageUid=10123 definingUid=10123 user=0
          process=com.velocity.lab reason=10 (USER REQUESTED) subreason=0 (UNKNOWN) status=0
          importance=100 pss=0.00 rss=0.00 description=stop state=empty trace=null
";

    /// A launch at 01:01:00 on the device's wall clock.
    const LAUNCHED: &str = "2026-10-15 01:01:00.000";

    /// P3 / M-6: only crash-like reasons count, of any pid of the package,
    /// and never a record from before the launch, even with the launch's
    /// pid (a reused pid on a re-booted AVD).
    #[test]
    fn exit_info_is_matched_by_time_not_only_by_pid() {
        let got = crash_from_exit_info(EXIT_INFO, LAUNCHED, Some(4321)).unwrap();
        assert!(got.starts_with("process 4321 ended: APP CRASH"), "{got}");
        assert!(got.ends_with("(crash)"), "{got}");
        // The app restarted itself: the launch pid is stale, the crash of
        // the new process still counts.
        let got = crash_from_exit_info(EXIT_INFO, LAUNCHED, Some(7777)).unwrap();
        assert!(got.starts_with("process 4321 ended"), "{got}");
        assert!(crash_from_exit_info(EXIT_INFO, LAUNCHED, None).is_some());
        // Launched after the crash: that record is history.
        assert_eq!(
            crash_from_exit_info(EXIT_INFO, "2026-10-15 01:05:00.000", Some(4321)),
            None
        );
        // Both records are after the launch: the crash counts, the user stop not.
        assert_eq!(
            crash_from_exit_info(EXIT_INFO, "2026-10-15 00:59:00.000", Some(1111))
                .map(|s| s.contains("4321")),
            Some(true),
            "the crash after the launch, not the user stop"
        );
        let only_stop = EXIT_INFO
            .split("        ApplicationExitInfo #1:")
            .nth(1)
            .map(|r| format!("        ApplicationExitInfo #1:{r}"))
            .unwrap();
        assert_eq!(
            crash_from_exit_info(&only_stop, "2026-10-15 00:00:00.000", Some(1111)),
            None,
            "user stop"
        );
    }

    #[test]
    fn launch_marks_and_time_logs_parse() {
        assert_eq!(
            parse_launch_mark("1760490060 123456789 2026-10-15 01:01:00\n"),
            (
                Some("1760490060.123".into()),
                Some("2026-10-15 01:01:00.123".into())
            )
        );
        // No %N (macOS, old toolbox): whole seconds.
        assert_eq!(
            parse_launch_mark("1760490060 N 2026-10-15 01:01:00"),
            (
                Some("1760490060.000".into()),
                Some("2026-10-15 01:01:00.000".into())
            )
        );
        assert_eq!(
            parse_launch_mark("1760490060"),
            (Some("1760490060.000".into()), None)
        );
        let log = "10-15 01:00:59.000 E/AndroidRuntime( 1): Process: com.old, PID: 1\n10-15 01:01:02.000 E/AndroidRuntime( 2): Process: com.velocity.lab, PID: 2\n";
        let kept = time_log_since(log, LAUNCHED);
        assert!(
            !kept.contains("com.old") && kept.starts_with("E/AndroidRuntime( 2)"),
            "{kept}"
        );
    }

    // Review d4 m2: a run killed by a second signal can't stop its stress
    // loops on the device; the next run's `--stress` used to start more
    // beside them, since only loops it started itself were killed first.
    #[tokio::test]
    async fn stress_on_first_kills_loops_an_earlier_run_left() {
        let fake = FakeAdb::new("stress-leftover");
        fake.stub("pkill", "");
        // No cores: no loop is spawned (that would use the real adb).
        fake.stub("nproc", "echo 0");
        let d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        assert!(d.set_stress("emulator-5554", true).await);
        assert!(
            fake.log().contains("DEVICE_SH:pkill -f velocity-stress"),
            "{}",
            fake.log()
        );
    }

    /// A fake device on API 34 whose `date` reads 01:01:00 at the launch.
    fn api34(name: &str) -> (FakeAdb, AndroidDriver) {
        let fake = FakeAdb::new(name);
        fake.stub("pidof", "echo 4321");
        fake.stub("date", "echo 1760490060 000000000 2026-10-15 01:01:00");
        fake.stub(
            "getprop",
            "case \"$1\" in ro.build.version.sdk) echo 34;; *) echo 1;; esac",
        );
        fake.stub(
            "dumpsys",
            "[ \"$2\" = exit-info ] && cat \"$DEV/exit-info\"; exit 0",
        );
        let d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        (fake, d)
    }

    const FATAL: &str = "E/AndroidRuntime( 5555): FATAL EXCEPTION: main\nE/AndroidRuntime( 5555): Process: com.velocity.lab, PID: 5555\nE/AndroidRuntime( 5555): java.lang.IllegalStateException: boom\n";

    /// M-6: the launch pid has only a user-stop record (the app restarted
    /// itself), so exit-info has no crash; the crash log must still decide.
    #[tokio::test]
    async fn no_exit_info_match_falls_back_to_the_crash_log() {
        let (fake, d) = api34("exit-info-fallback");
        let stop = EXIT_INFO
            .split("        ApplicationExitInfo #1:")
            .nth(1)
            .unwrap()
            .replace("01:00:00.000", "01:01:30.000")
            .replace("pid=1111", "pid=4321");
        std::fs::write(
            fake.dir.join("device/exit-info"),
            format!(
                "{}        ApplicationExitInfo #1:{stop}",
                EXIT_INFO
                    .split("        ApplicationExitInfo #0:")
                    .next()
                    .unwrap()
            ),
        )
        .unwrap();
        std::fs::write(fake.dir.join("device/crash.log"), FATAL).unwrap();
        fake.stub("adb-logcat", "cat \"$DEV/crash.log\"");
        d.launch_app("emulator-5554", "com.velocity.lab", false)
            .await
            .unwrap();
        let report = d.crash_report("emulator-5554", "com.velocity.lab").await;
        assert_eq!(
            report.as_deref(),
            Some("java.lang.IllegalStateException: boom"),
            "{}",
            fake.log()
        );
    }

    /// M-7: the crash poll read getprop and exit-info on every call.
    #[tokio::test]
    async fn crash_polls_cache_the_api_level_and_space_out_exit_info() {
        let (fake, d) = api34("exit-info-cost");
        std::fs::write(
            fake.dir.join("device/exit-info"),
            "ApplicationExitInfo none\n",
        )
        .unwrap();
        fake.stub("adb-logcat", "exit 0");
        d.launch_app("emulator-5554", "com.velocity.lab", false)
            .await
            .unwrap();
        for _ in 0..5 {
            assert_eq!(
                d.crash_report("emulator-5554", "com.velocity.lab").await,
                None
            );
        }
        let getprops = fake
            .calls("getprop")
            .iter()
            .filter(|l| l.contains("ro.build.version.sdk"))
            .count();
        assert!(getprops <= 1, "{getprops} getprop calls:\n{}", fake.log());
        let dumps = fake
            .calls("dumpsys")
            .iter()
            .filter(|l| l.contains("exit-info"))
            .count();
        assert_eq!(dumps, 1, "{}", fake.log());
    }

    /// m-10: logcat without epoch `-T` (API 21-23) turned crash detection
    /// off; the whole buffer is read and filtered by the launch time instead.
    #[tokio::test]
    async fn logcat_without_t_still_reports_crashes_since_the_launch() {
        let (fake, d) = api34("logcat-no-t");
        std::fs::write(fake.dir.join("device/exit-info"), "").unwrap();
        std::fs::write(
            fake.dir.join("device/crash.log"),
            "10-15 01:00:30.000 E/AndroidRuntime( 9): Process: com.velocity.lab, PID: 9\n10-15 01:00:30.000 E/AndroidRuntime( 9): java.lang.OldError: earlier run\n",
        )
        .unwrap();
        fake.stub(
            "adb-logcat",
            "case \"$*\" in *-T*) echo 'unknown option -T' >&2; exit 1;; esac; cat \"$DEV/crash.log\"",
        );
        d.launch_app("emulator-5554", "com.velocity.lab", false)
            .await
            .unwrap();
        assert_eq!(
            d.crash_report("emulator-5554", "com.velocity.lab").await,
            None,
            "a crash from before the launch"
        );
        let new = FATAL
            .lines()
            .map(|l| format!("10-15 01:01:05.000 {l}\n"))
            .collect::<String>();
        std::fs::write(fake.dir.join("device/crash.log"), new).unwrap();
        assert_eq!(
            d.crash_report("emulator-5554", "com.velocity.lab")
                .await
                .as_deref(),
            Some("java.lang.IllegalStateException: boom")
        );
    }

    /// N8, P3, N3 on a fake adb: a launch no longer wipes the device's crash
    /// buffer; it records the time and pid, so a crash report reads the
    /// log since the launch and (API 30+) exit-info for that pid; and the
    /// sync probe gets a host port of its own.
    #[tokio::test]
    async fn a_launch_marks_its_crashes_without_wiping_the_buffer() {
        let fake = FakeAdb::new("launch-crash");
        let dev = "emulator-5554";
        let app = "com.velocity.lab";
        fake.stub("pidof", "echo 4321");
        fake.stub("date", "echo 1760490060 000000000 2026-10-15 01:01:00");
        fake.stub(
            "getprop",
            "case \"$1\" in ro.build.version.sdk) echo 34;; *) echo 1;; esac",
        );
        std::fs::write(fake.dir.join("device/exit-info"), EXIT_INFO).unwrap();
        fake.stub(
            "dumpsys",
            "[ \"$2\" = exit-info ] && cat \"$DEV/exit-info\"; exit 0",
        );
        // Host-side `adb forward tcp:0 …` prints the port it picked.
        fake.stub(
            "adb-forward",
            "case \"$*\" in *tcp:0*) echo 41234;; esac; exit 0",
        );
        fake.stub("adb-logcat", "exit 0");
        let d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        d.launch_app(dev, app, false).await.unwrap();
        let log = fake.log();
        assert!(
            !log.contains("[logcat][-b][crash][-c]"),
            "crash buffer wiped:\n{log}"
        );
        assert_eq!(d.sync_probe_port(dev), Some(41234));

        let report = d.crash_report(dev, app).await;
        assert!(
            report.as_deref().is_some_and(|r| r.contains("APP CRASH")),
            "{report:?}\n{}",
            fake.log()
        );
        let log = fake.log();
        assert!(
            log.lines()
                .any(|l| l.contains("[logcat]") && l.contains("[-T]")),
            "{log}"
        );
        assert!(log.contains("[exit-info][com.velocity.lab]"), "{log}");

        // m-11: cleanup removes the forward the launch added.
        d.cleanup().await;
        let log = fake.log();
        assert!(
            log.contains("[forward][--remove][tcp:41234]"),
            "forward left behind:\n{log}"
        );
        assert_eq!(d.sync_probe_port(dev), None);
    }

    /// A fake device whose screen is on and unlocked and whose app comes
    /// up at once, for counting a launch's adb round trips.
    fn launchable(name: &str) -> (FakeAdb, AndroidDriver) {
        let fake = FakeAdb::new(name);
        fake.stub(
            "dumpsys",
            "case \"$1 $2\" in\n\
             'power ') echo '  mWakefulness=Awake';;\n\
             'window policy') echo '    mIsShowing=false';;\n\
             'window displays') echo '  mCurrentFocus=Window{1 u0 com.velocity.lab/com.velocity.lab.MainActivity}';;\n\
             esac",
        );
        fake.stub("cmd", "echo com.velocity.lab/.MainActivity");
        fake.stub("pidof", "echo 4321");
        fake.stub("date", "echo 1760490060 000000000 2026-10-15 01:01:00");
        fake.stub(
            "adb-forward",
            "case \"$*\" in *tcp:0*) echo 41234;; esac; exit 0",
        );
        let mut d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        // However slow the fake is under load, a launch counts as "just
        // after" the last one.
        d.screen_ready_ttl = Duration::from_secs(600);
        (fake, d)
    }

    /// adb invocations in the fake's log.
    fn trips(fake: &FakeAdb) -> usize {
        fake.log()
            .lines()
            .filter(|l| l.starts_with("ARGV:"))
            .count()
    }

    /// The devices `adb forward tcp:19401 tcp:19401` went to, in order.
    fn fixed_forwards(fake: &FakeAdb) -> Vec<String> {
        fake.log()
            .lines()
            .filter_map(|l| l.strip_prefix("ARGV:[-s]["))
            .filter_map(|l| l.strip_suffix("][forward][tcp:19401][tcp:19401]"))
            .map(String::from)
            .collect()
    }

    fn launch_of(d: &AndroidDriver, dev: &str) -> Launch {
        d.launches
            .lock()
            .unwrap()
            .get(&(dev.to_string(), "com.velocity.lab".to_string()))
            .cloned()
            .unwrap()
    }

    /// A launch took 9 adb round trips, 7 every time after the first: the
    /// screen check, force-stop, date, `am start`, the focus check, pidof
    /// and the fixed probe forward. Now force-stop and date share a shell,
    /// the focus check reads the pid, the forward is sent once, and a
    /// launch right after another reuses its screen check.
    #[tokio::test]
    async fn a_launch_takes_few_adb_round_trips() {
        let (fake, mut d) = launchable("launch-trips");
        let dev = "emulator-5554";
        let app = "com.velocity.lab";
        d.launch_app(dev, app, false).await.unwrap();
        let first = trips(&fake);
        d.launch_app(dev, app, false).await.unwrap();
        let second = trips(&fake) - first;
        // Any action may lock the screen: the next launch checks again.
        d.press_key(dev, Key::Home).await.unwrap();
        let before = trips(&fake);
        d.launch_app(dev, app, false).await.unwrap();
        let third = trips(&fake) - before;
        // So does a launch once the screen check is older than the TTL.
        d.screen_ready_ttl = Duration::ZERO;
        let before = trips(&fake);
        d.launch_app(dev, app, false).await.unwrap();
        let fourth = trips(&fake) - before;
        let log = fake.log();
        assert_eq!((first, second, third, fourth), (7, 3, 4, 4), "{log}");

        let launch = launch_of(&d, dev);
        assert_eq!(launch.pid, Some(4321));
        assert_eq!(launch.since.as_deref(), Some("1760490060.000"));
        assert_eq!(launch.wall.as_deref(), Some("2026-10-15 01:01:00.000"));
        assert_eq!(d.sync_probe_port(dev), Some(41234));
        assert!(
            log.contains("DEVICE_SH:'am' 'force-stop' 'com.velocity.lab' && { date"),
            "{log}"
        );
        assert_eq!(fixed_forwards(&fake), ["emulator-5554"], "{log}");
        // pidof only ever ran inside the focus check.
        assert!(!log.contains("DEVICE_SH:'pidof'"), "{log}");
        assert_eq!(
            fake.calls("dumpsys")
                .iter()
                .filter(|c| c.contains("[power]"))
                .count(),
            3
        );
    }

    /// `pm clear` prints "Success" before the clock; a stop that fails
    /// still fails the launch as the same kind of error, and a `date` that
    /// fails only leaves the mark unknown.
    #[tokio::test]
    async fn a_launchs_stop_and_clock_share_one_shell() {
        let (fake, d) = launchable("launch-stop-mark");
        let dev = "emulator-5554";
        let app = "com.velocity.lab";
        fake.stub("pm", "echo Success");
        d.launch_app(dev, app, true).await.unwrap();
        assert_eq!(launch_of(&d, dev).since.as_deref(), Some("1760490060.000"));
        assert_eq!(fake.calls("pm"), ["PM:[clear][com.velocity.lab]"]);
        assert!(fake.calls("am").iter().all(|c| !c.contains("force-stop")));

        fake.stub("date", "exit 1");
        d.launch_app(dev, app, false).await.unwrap();
        assert_eq!(launch_of(&d, dev).since, None);

        fake.stub("am", "echo 'error: closed' >&2; exit 1");
        let err = d.launch_app(dev, app, false).await.unwrap_err();
        assert!(
            matches!(err, VelocityError::AdbConnectionLost { .. }),
            "{err:?}"
        );
        assert_eq!(fake.calls("date").len(), 2, "date ran after a failed stop");
    }

    /// Host port 19401 is forwarded again when another device took it, or
    /// when the last forward failed.
    #[tokio::test]
    async fn the_fixed_probe_forward_follows_the_last_device() {
        let (fake, d) = launchable("launch-fixed-forward");
        let app = "com.velocity.lab";
        for dev in [
            "emulator-5554",
            "emulator-5554",
            "emulator-5556",
            "emulator-5554",
        ] {
            d.launch_app(dev, app, false).await.unwrap();
        }
        assert_eq!(
            fixed_forwards(&fake),
            ["emulator-5554", "emulator-5556", "emulator-5554"],
            "{}",
            fake.log()
        );
        fake.stub("adb-forward", "exit 1");
        d.launch_app("emulator-5556", app, false).await.unwrap();
        d.launch_app("emulator-5556", app, false).await.unwrap();
        assert_eq!(fixed_forwards(&fake).len(), 5, "{}", fake.log());
    }

    /// VEL-156: every driver entry that puts an appId on a device command
    /// line rejects one that isn't a package name, before running anything.
    #[tokio::test]
    async fn app_ids_are_validated_at_entry() {
        let fake = FakeAdb::new("driver-appid");
        let marker = fake.dir.join("PWNED");
        let d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        let dev = "emulator-5554";
        let evil = format!("com.x';touch {}'", marker.display());
        assert!(d.launch_app(dev, &evil, true).await.is_err());
        assert!(d.launch_app(dev, &evil, false).await.is_err());
        assert!(d.stop_app(dev, &evil).await.is_err());
        assert!(d
            .reset_app(dev, &evil, &ResetConfig::default())
            .await
            .is_err());
        assert!(d.crash_report(dev, &evil).await.is_none());
        assert!(d.app_fingerprint(dev, &evil).await.is_none());
        assert!(d.save_app_state(dev, &evil, "s").await.is_err());
        assert!(d.restore_app_state(dev, &evil, "s").await.is_err());
        assert!(d.collect_resource_metrics(dev, &evil).await.is_err());
        assert!(d
            .install_app_if_changed(dev, "/x.apk", &evil, "abc")
            .await
            .is_err());
        assert!(!marker.exists());
        assert!(!fake.log().contains(&evil), "{}", fake.log());
    }

    #[tokio::test]
    async fn every_shell_arg_is_quoted() {
        let fake = FakeAdb::new("driver-quoted");
        let d = AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())));
        assert!(!d.debuggable("emulator-5554", "com.velocity.lab").await);
        d.stop_app("emulator-5554", "com.velocity.lab")
            .await
            .unwrap();
        assert_eq!(fake.calls("run-as"), ["RUN_AS:[com.velocity.lab][id]"]);
        assert!(fake
            .log()
            .contains("DEVICE_SH:'run-as' 'com.velocity.lab' 'id'"));
        assert_eq!(fake.calls("am"), ["AM:[force-stop][com.velocity.lab]"]);
    }

    #[test]
    fn clear_data_script_keeps_lib_and_quotes_the_app_id() {
        let fake = FakeAdb::new("driver-clear");
        // The fake run-as runs its command in the device dir.
        fake.stub("run-as", "shift; exec \"$@\"");
        let dev = fake.dir.join("device");
        for f in ["a", ".hidden", "lib"] {
            std::fs::write(dev.join(f), "x").unwrap();
        }
        let out = std::process::Command::new(&fake.adb)
            .args([
                "-s",
                "emulator-5554",
                "shell",
                &clear_data_script("com.velocity.lab"),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(dev.join("lib").exists());
        assert!(!dev.join("a").exists() && !dev.join(".hidden").exists());
        assert_eq!(fake.calls("run-as")[0], "RUN_AS:[com.velocity.lab][sh][-c][for f in * .[!.]*; do [ \"$f\" = lib ] || rm -rf \"$f\"; done]");
    }

    /// VEL-359: saved app data was world-readable (0644 in 0755 dirs).
    #[test]
    fn saved_state_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vel-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("dev/app/name");
        create_private_dir(&dest).unwrap();
        let file = dest.join("data.tar");
        std::fs::write(&file, b"old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&file, b"tar").unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"tar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_names_cannot_inject_console_commands() {
        assert_eq!(
            snapshot_name("com.velocity.lab", "logged-in"),
            "velocity-com_velocity_lab-logged-in"
        );
        let n = snapshot_name("com.x", "a b\nkill");
        assert!(n.starts_with("velocity-com_x-a_b_kill-"), "{n}");
        assert!(check_snapshot_name(&n).is_ok(), "{n}");
        assert_eq!(
            snapshot_name("com.x", "login_done"),
            "velocity-com_x-login_done"
        );
    }

    /// Review MINOR-14: names that sanitise alike stay apart.
    #[test]
    fn sanitised_snapshot_names_do_not_collide() {
        let names = [
            "login.done",
            "login done",
            "login_done",
            "ログイン",
            "日本語",
        ];
        let snaps: std::collections::HashSet<String> =
            names.iter().map(|n| snapshot_name("com.x", n)).collect();
        assert_eq!(snaps.len(), names.len(), "{snaps:?}");
        for s in &snaps {
            assert!(check_snapshot_name(s).is_ok(), "{s}");
        }
        // Stable across runs and builds.
        assert_eq!(fnv1a(b"login.done"), fnv1a(b"login.done"));
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert!(check_snapshot_name("velocity-clean").is_ok());
        assert!(check_snapshot_name("x\nkill").is_err());
        assert!(check_snapshot_name("a b").is_err());
    }
}

#[cfg(test)]
mod lock_tests {
    use super::{
        keyguard_showing, launched, parse_system_state, screen_ready, shade_open, TextMode,
    };

    /// VEL-S44: output the parser doesn't know was "nothing playing".
    #[test]
    fn unknown_media_session_output_is_an_error() {
        for out in [
            "Can't find service: media_session\nVELOCITY-CALLS\n",
            "",
            "Some OEM format\n  playing: yes\nVELOCITY-CALLS\n    mCallState=0\n",
        ] {
            assert!(parse_system_state(out).is_err(), "{out:?}");
        }
        let none = parse_system_state("  Sessions Stack - have 0 sessions:\nVELOCITY-CALLS\n");
        assert!(!none.unwrap().media_playing);
    }

    #[test]
    fn media_and_call_state_are_read_from_dumpsys() {
        let dump = "  Sessions Stack - have 2 sessions:\n    Other com.other/Other (userId=0)\n      active=false\n      state=PlaybackState {state=PLAYING(3), position=0}\n      metadata: size=1, description=Not me, null, null\n    VelocityLab com.velocity.lab/VelocityLab (userId=0)\n      active=true\n      state=PlaybackState {state=PLAYING(3), position=0, buffered position=0}\n      metadata: size=1, description=Velocity Lab Song, null, null\nVELOCITY-CALLS\n    mCallState=0\n";
        let s = parse_system_state(dump).unwrap();
        assert!(s.media_playing);
        assert_eq!(
            s.media_titles,
            vec!["Velocity Lab Song".to_string()],
            "inactive sessions don't count"
        );
        assert!(!s.in_call);
        let ringing = parse_system_state(
            "MEDIA SESSION SERVICE (dumpsys media_session)\nVELOCITY-CALLS\n  mCallState=1\n",
        )
        .unwrap();
        assert!(ringing.in_call && !ringing.media_playing);
    }

    #[test]
    fn auto_text_mode_types_short_text_and_sets_long_text() {
        assert!(!TextMode::Auto.wants_fast("qa@velocity"));
        assert!(TextMode::Auto.wants_fast("a sentence long enough to set"));
        assert!(TextMode::Fast.wants_fast("x"));
        assert!(!TextMode::Keys.wants_fast("a sentence long enough to set"));
    }

    #[test]
    fn a_launch_stuck_at_the_splash_is_told_from_a_real_one() {
        let p = "com.velocity.lab";
        assert!(launched(
            "  mCurrentFocus=Window{1 u0 com.velocity.lab/com.velocity.lab.MainActivity}",
            p
        ));
        assert!(!launched(
            "  mCurrentFocus=Window{2 u0 Splash Screen com.velocity.lab}",
            p
        ));
        assert!(!launched("  mCurrentFocus=null", p));
        // A mirroring virtual display (the screen video) is listed first,
        // with no focus: display 0 decides.
        let two = "  Display: mDisplayId=3\n  mCurrentFocus=null\n  Display: mDisplayId=0 (organized)\n  mCurrentFocus=Window{4 u0 com.velocity.lab/com.velocity.lab.MainActivity}\n";
        assert!(launched(two, p));
        assert_eq!(
            crate::driver::default_display_focus(
                "  Display: mDisplayId=10\n  mCurrentFocus=Window{x}\n"
            ),
            None
        );
        assert!(crate::driver::default_display_focus(two)
            .unwrap()
            .contains("MainActivity"));
        assert!(launched("  mCurrentFocus=Window{3 u0 com.android.permissioncontroller/.GrantPermissionsActivity}", p));
        assert!(launched("", p), "unknown counts as launched");
    }

    #[test]
    fn keyguard_and_sleep_are_read_from_dumpsys() {
        let locked = "  mWakefulness=Awake\n      KeyguardStateMonitor\n        mIsShowing=true\n        mInputRestricted=true\n";
        assert!(keyguard_showing(locked));
        assert!(!screen_ready(locked));
        assert!(!screen_ready(
            "  mWakefulness=Asleep\n        mIsShowing=false\n"
        ));
        assert!(screen_ready(
            "  mWakefulness=Awake\n        mIsShowing=false\n"
        ));
        assert!(
            screen_ready(""),
            "an unreadable state must not block a launch"
        );
    }

    #[test]
    fn an_open_shade_is_seen_but_a_lock_screen_is_not_a_shade() {
        let shade = "  mWakefulness=Awake\n        mIsShowing=false\n  mCurrentFocus=Window{e67afe u0 NotificationShade}\n";
        assert!(shade_open(shade) && !screen_ready(shade));
        let lock = "  mWakefulness=Awake\n        mIsShowing=true\n  mCurrentFocus=Window{e67afe u0 NotificationShade}\n";
        assert!(!shade_open(lock));
        let app = "  mWakefulness=Awake\n        mIsShowing=false\n  mCurrentFocus=Window{113f u0 com.velocity.lab/com.velocity.lab.MainActivity}\n";
        assert!(screen_ready(app));
    }
}

#[cfg(test)]
mod set_text_tests {
    use super::*;
    use crate::test_support::FakeAdb;
    use crate::tree_server::tests::{scripted, Act};
    use std::sync::Arc;

    fn field() -> Element {
        Element {
            hint: None,
            platform_id: "f".into(),
            label: None,
            text: None,
            element_type: "EditText".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    /// A driver whose tree server answers `SETTEXT` with `reply`; returns
    /// the result of typing `text` and the commands the server got.
    async fn type_with(name: &str, reply: &'static str, text: &str) -> (Result<()>, Vec<String>) {
        let (port, seen) = scripted(Arc::new(move |cmd, _| {
            if cmd.contains("SETTEXT") {
                Act::Say(reply.as_bytes().to_vec())
            } else {
                Act::Say(b"OK 0\n".to_vec())
            }
        }))
        .await;
        let fake = FakeAdb::new(name);
        let d = AndroidDriver {
            tree: crate::tree_server::attached_for_test(fake.path(), port).await,
            ..AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())))
        };
        let r = d.input_text("emulator-5554", &field(), text).await;
        assert!(fake.calls("input").is_empty(), "{}", fake.log());
        let log = seen.lock().unwrap().clone();
        (r, log)
    }

    /// VEL-164: a card field took the text and reformatted it; the read-back
    /// differed and the same text was then typed again key by key.
    #[tokio::test]
    async fn a_reformatted_field_is_not_typed_into_twice() {
        let card = "4111111111111111";
        let reply: &'static str =
            Box::leak(format!("OK 2 {}\n", hex("4111 1111 1111 1111")).into_boxed_str());
        let (r, seen) = type_with("st-mask", reply, card).await;
        r.unwrap();
        assert!(seen
            .iter()
            .any(|c| c.ends_with(&format!("SETTEXT a {}", hex(card)))));
        assert!(
            !seen
                .iter()
                .any(|c| c.contains("TEXT ") && !c.contains("SETTEXT")),
            "{seen:?}"
        );
    }

    /// The field took it but holds something else: an error, not a retype.
    #[tokio::test]
    async fn a_field_that_reads_back_otherwise_is_an_error() {
        let reply: &'static str = Box::leak(format!("OK 2 {}\n", hex("41111")).into_boxed_str());
        let (r, seen) = type_with("st-differs", reply, "4111111111111111").await;
        let err = r.unwrap_err().to_string();
        assert!(err.contains("not typed again"), "{err}");
        assert_eq!(seen.len(), 1, "{seen:?}");
    }

    /// N4: a protocol 9 server's `hint` reaches the element, so an empty
    /// Flutter/Compose field can be found by its placeholder.
    #[tokio::test]
    async fn the_tree_servers_hint_reaches_the_element() {
        let xml = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.EditText" package="com.app" content-desc="" hint="Email" state-desc="" error="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,200]" /></hierarchy>"#;
        let (port, seen) = scripted(Arc::new(move |cmd, _| {
            if cmd.ends_with("DUMP") {
                Act::Say(format!("OK {}\n{xml}", xml.len()).into_bytes())
            } else {
                Act::Say(b"OK 0\n".to_vec())
            }
        }))
        .await;
        let fake = FakeAdb::new("hint-dump");
        let d = AndroidDriver {
            tree: crate::tree_server::attached_for_test(fake.path(), port).await,
            ..AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())))
        };
        let root = d.get_hierarchy("emulator-5554").await.unwrap();
        fn find(e: &Element) -> Option<&Element> {
            if e.element_type == "EditText" {
                return Some(e);
            }
            e.children.iter().find_map(find)
        }
        let field = find(&root).expect("the field");
        assert_eq!(field.hint.as_deref(), Some("Email"));
        assert!(seen.lock().unwrap().iter().any(|c| c.ends_with("DUMP")));
    }

    /// D-5 / VEL-S46: the tap on a label left the focus on another field;
    /// no key is sent. With the focus on the target, typing goes ahead.
    #[tokio::test]
    async fn input_text_fails_when_the_focus_is_elsewhere() {
        let dump = |focus_top: bool| -> String {
            let xml = format!(
                r#"<hierarchy rotation="0"><node index="0" text="Name" class="android.widget.EditText" focused="{}" checked="false" enabled="true" selected="false" bounds="[0,0][1080,200]" /><node index="1" text="Email" class="android.widget.EditText" focused="{}" checked="false" enabled="true" selected="false" bounds="[0,400][1080,600]" /></hierarchy>"#,
                focus_top, !focus_top
            );
            format!("OK {}\n{xml}", xml.len())
        };
        let focus_top = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let ft = focus_top.clone();
        let (port, seen) = scripted(Arc::new(move |cmd, _| {
            if cmd.ends_with("DUMP lite") {
                Act::Say(dump(ft.load(std::sync::atomic::Ordering::SeqCst)).into_bytes())
            } else {
                Act::Say(b"OK 0\n".to_vec())
            }
        }))
        .await;
        let fake = FakeAdb::new("focus-elsewhere");
        let d = AndroidDriver {
            tree: crate::tree_server::attached_for_test(fake.path(), port).await,
            ..AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())))
        };
        let target = Element {
            element_type: "EditText".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y: 400,
                width: 1080,
                height: 200,
            },
            ..field()
        };
        let err = d
            .input_text("emulator-5554", &target, "x")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("focus stayed on EditText"), "{err}");
        let typed = |seen: &Arc<std::sync::Mutex<Vec<String>>>| {
            seen.lock()
                .unwrap()
                .iter()
                .any(|c| c.contains(" TEXT ") || c.contains("SETTEXT"))
        };
        assert!(!typed(&seen), "no key sent");
        focus_top.store(false, std::sync::atomic::Ordering::SeqCst);
        d.input_text("emulator-5554", &target, "x").await.unwrap();
        assert!(typed(&seen));
    }

    /// D-51 (VEL-S35): without the tree server, a password field's text, a
    /// registered secret and non-ASCII text never reach `adb shell input
    /// text`; plain ASCII still does.
    #[tokio::test]
    async fn the_adb_text_fallback_refuses_secrets_and_non_ascii() {
        let fake = FakeAdb::new("adb-secret");
        let d = AndroidDriver {
            tree: crate::tree_server::TreeServer::off(),
            ..AndroidDriver::with_backend(Box::new(Adb::with_path(fake.path())))
        };
        let pw = Element {
            obscured: true,
            ..field()
        };
        let e = d
            .input_text("emulator-5554", &pw, "plainpass")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("password field") && e.contains("tree server"),
            "{e}"
        );
        velocity_common::redact::register_secret("d51-registered-secret");
        for text in ["d51-registered-secret", "héllo"] {
            let e = d
                .input_text("emulator-5554", &field(), text)
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("tree server"), "{text}: {e}");
            assert!(!e.contains("d51-registered-secret"), "{e}");
            d.input_raw("emulator-5554", text).await.unwrap_err();
        }
        assert!(fake.calls("input").is_empty(), "{}", fake.log());
        d.input_text("emulator-5554", &field(), "hello")
            .await
            .unwrap();
        assert!(!fake.calls("input").is_empty(), "{}", fake.log());
    }

    /// A refusal leaves the field as it was: typing key by key is right.
    #[tokio::test]
    async fn a_refused_set_text_is_typed_as_keys() {
        let text = "a sentence long enough to set";
        let (r, seen) = type_with("st-refused", "OK 0\n", text).await;
        r.unwrap();
        assert!(
            seen.iter()
                .any(|c| c.ends_with(&format!(" TEXT {}", hex(text)))),
            "{seen:?}"
        );
    }

    /// MINOR-15: a newline is the Enter key on every path. Long text with
    /// one went in through SET_TEXT as a raw character instead.
    #[tokio::test]
    async fn a_newline_is_always_the_enter_key() {
        let text = "first line of text\nsecond line";
        let (r, seen) = type_with("st-newline", "OK 1\n", text).await;
        r.unwrap();
        assert!(!seen.iter().any(|c| c.contains("SETTEXT")), "{seen:?}");
        assert!(seen
            .iter()
            .any(|c| c.ends_with(&format!("TEXT {}", hex(text)))));
        assert_eq!(
            shell::input_text_script("a\nb")
                .unwrap()
                .matches("keyevent 66")
                .count(),
            1
        );
    }

    #[test]
    fn formatting_is_ignored_but_content_is_not() {
        assert!(same_but_formatting("(415) 555-0100", "4155550100"));
        assert!(same_but_formatting("Hello WORLD", "hello world"));
        assert!(same_but_formatting("prefix 12/34", "1234"));
        assert!(!same_but_formatting("4111", "41111111"));
    }
}

/// The shell line for a held drag: DOWN, a hold, steps of MOVE, a rest at
/// the end (a pointer still for 40 ms or more stops the list's velocity),
/// UP.
fn drag_script(from: (i32, i32), to: (i32, i32), hold_ms: u64, move_ms: u64) -> String {
    let steps = (move_ms / 120).clamp(3, 20) as i32;
    let secs = |ms: u64| format!("{}.{:03}", ms / 1000, ms % 1000);
    let mut parts = vec![
        format!("input motionevent DOWN {} {}", from.0, from.1),
        format!("sleep {}", secs(hold_ms)),
    ];
    for i in 1..=steps {
        let x = from.0 + (to.0 - from.0) * i / steps;
        let y = from.1 + (to.1 - from.1) * i / steps;
        parts.push(format!("input motionevent MOVE {x} {y}"));
    }
    parts.push("sleep 0.250".into());
    parts.push(format!("input motionevent UP {} {}", to.0, to.1));
    parts.join("; ")
}

#[cfg(test)]
mod drag_tests {
    // Row 3.1: one finger down across the commands, held, moved in steps,
    // rested, lifted.
    #[test]
    fn a_drag_holds_moves_rests_and_lifts() {
        let s = super::drag_script((100, 200), (100, 500), 700, 900);
        let parts: Vec<&str> = s.split("; ").collect();
        assert_eq!(parts[0], "input motionevent DOWN 100 200");
        assert_eq!(parts[1], "sleep 0.700");
        assert_eq!(parts[parts.len() - 2], "sleep 0.250");
        assert_eq!(parts[parts.len() - 1], "input motionevent UP 100 500");
        let moves: Vec<&&str> = parts.iter().filter(|p| p.contains("MOVE")).collect();
        assert_eq!(moves.len(), 7, "{s}");
        assert_eq!(*moves[moves.len() - 1], "input motionevent MOVE 100 500");
    }
}

#[cfg(test)]
mod key_tests {
    use super::AndroidDriver;
    use velocity_common::Key;

    /// D-9: the editing and arrow keys are Android keyevents, Backspace
    /// is KEYCODE_DEL (67) and Delete KEYCODE_FORWARD_DEL (112).
    #[test]
    fn editing_and_arrow_keys_map_to_keyevents() {
        let codes: Vec<u32> = [
            Key::Backspace,
            Key::Tab,
            Key::Escape,
            Key::Delete,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
        ]
        .into_iter()
        .filter_map(AndroidDriver::key_to_keycode)
        .collect();
        assert_eq!(codes, [67, 61, 111, 112, 19, 20, 21, 22]);
        assert_eq!(AndroidDriver::key_to_keycode(Key::Back), Some(4));
    }
}

#[cfg(test)]
mod differs_tests {
    use velocity_common::Element;

    /// Audit 2026-11 AND-4: a secret cut by maxLength reads back as its own
    /// prefix, which exact-value redaction misses: only lengths are told.
    #[test]
    fn a_secret_that_reads_back_different_is_not_quoted() {
        let field = Element {
            obscured: true,
            ..Default::default()
        };
        let d = super::differs_detail(&field, "hunter2-long-api-key", "hunter2-long");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(
            d.contains("20 characters") && d.contains("12 characters"),
            "{d}"
        );
        let plain = Element::default();
        let d = super::differs_detail(&plain, "(555) 123", "555-123");
        assert!(d.contains("555-123"), "{d}");
    }
}
