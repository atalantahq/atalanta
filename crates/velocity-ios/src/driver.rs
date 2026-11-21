use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use velocity_common::selector_match::locate;
use velocity_common::*;

use crate::devicectl::Devicectl;
use crate::parser::parse_ios_hierarchy;
use crate::simctl::Simctl;
use crate::wda_bootstrap::{self, WdaBootstrap};
use crate::wda_manager::WdaManager;

/// How long a tree read stays usable. Every action drops it, so this only
/// bounds reads with no action in between (polling invalidates explicitly).
const TREE_TTL: Duration = Duration::from_millis(2000);
/// How long a visibility-bearing tree read is reused for an unchanged screen
/// under a modal layer: the `visible` flags are copied onto each new poll's
/// tree until the next action, for up to this long.
const VISIBILITY_REUSE: Duration = Duration::from_secs(10);
/// How long a launch waits on the runner's terminate before `simctl
/// terminate` takes over: a healthy runner answers in well under a second.
const RUNNER_TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);

/// One simulator's WebDriverAgent: its own process, port and session, so
/// simulators run in parallel instead of sharing port 8100.
struct DeviceConn {
    /// WDA's port; fixed for the connection's life.
    port: u16,
    wda: Mutex<WdaManager>,
    bootstrap: Mutex<WdaBootstrap>,
    prepared: AtomicBool,
    /// Last parsed tree, shared by the step's lookup, dialog check and settle.
    tree: std::sync::Mutex<Option<(Arc<Element>, Instant)>>,
    /// Bounds of the field the last `inputText` focused; a second
    /// `inputText` into it needn't tap again.
    focused: std::sync::Mutex<Option<Rect>>,
    /// App of the current session, to re-create it after a WDA restart.
    bundle: std::sync::Mutex<String>,
    /// No system-alert check before this: one found none just now. Each
    /// check costs ~80 ms (navigation test 20 s → 30 s when checking every
    /// read), so like VelocityRunner a miss holds for `ios.alertRecheckMs` (1 s).
    alert_clear_until: std::sync::Mutex<Option<Instant>>,
    /// When a hardware key last reached the app. The first one after a
    /// launch is dropped (iOS attaches the hardware keyboard on it), so a
    /// paste before then sends a no-op key first.
    keys_primed: std::sync::Mutex<Option<std::time::SystemTime>>,
    /// Whether this runner's `/source` reports focus (D-5): 0 not known
    /// yet, 1 yes (a focused element was seen), 2 no (a text field was
    /// tapped and nothing reported focus, so the check costs a read for
    /// nothing).
    reports_focus: std::sync::atomic::AtomicU8,
    /// Why the runner can't be relaunched (a crash loop, its port taken),
    /// from the last failed recovery: the step fails with it (exit 3)
    /// instead of "unreachable".
    runner_failing: std::sync::Mutex<Option<VelocityError>>,
    /// The tree (by address) whose `visible` flags were read under a modal
    /// layer ([`IosDriver::with_visibility`]), so it is read once.
    /// Held as a `Weak`, not an address: a dropped tree's address can come
    /// back on the next, unflagged read (seen on an iPhone 12: rows behind a
    /// sheet counted as visible), and a `Weak` keeps it reserved.
    visible_for: std::sync::Mutex<Option<std::sync::Weak<Element>>>,
    /// The full tree from the last successful `get_source_with_visibility`
    /// under a modal layer, and when it was read: its `visible` flags are
    /// reused onto each new poll's tree for `VISIBILITY_REUSE`, so polling an
    /// `assertVisible` under a sheet makes one visibility read instead of one
    /// per poll.
    last_visibility: std::sync::Mutex<Option<(Arc<Element>, Instant)>>,
}

impl DeviceConn {
    fn new(
        device_id: &str,
        base_url: &str,
        port: u16,
        physical: bool,
        timeout: Option<Duration>,
    ) -> Self {
        let mut wda = match timeout {
            Some(t) => WdaManager::with_timeout(base_url, t),
            None => WdaManager::new(base_url),
        };
        wda.client_mut().set_device_id(device_id);
        Self {
            port,
            wda: Mutex::new(wda),
            bootstrap: Mutex::new(WdaBootstrap::new().with_port(port).physical(physical)),
            prepared: AtomicBool::new(false),
            tree: std::sync::Mutex::new(None),
            alert_clear_until: std::sync::Mutex::new(None),
            focused: std::sync::Mutex::new(None),
            bundle: std::sync::Mutex::new(String::new()),
            keys_primed: std::sync::Mutex::new(None),
            reports_focus: std::sync::atomic::AtomicU8::new(0),
            runner_failing: std::sync::Mutex::new(None),
            visible_for: std::sync::Mutex::new(None),
            last_visibility: std::sync::Mutex::new(None),
        }
    }

    /// `e`, or the reason the runner can't come back when a recovery
    /// found one.
    fn or_runner_failing(&self, e: VelocityError) -> VelocityError {
        self.runner_failing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or(e)
    }

    /// The screen is about to change: forget the tree and the focus.
    fn invalidate(&self) {
        *self.tree.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.focused.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .last_visibility
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The last tree read, however old (D-5's pre-tap focus).
    fn last_tree(&self) -> Option<Arc<Element>> {
        self.tree
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(t, _)| t.clone())
    }

    fn cached_tree(&self) -> Option<Arc<Element>> {
        self.tree
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(_, at)| at.elapsed() < TREE_TTL)
            .map(|(t, _)| t.clone())
    }
}

type CrashLogMap = HashMap<(String, String), (std::time::SystemTime, Option<std::path::PathBuf>)>;

/// D-50's copied crash-log directories ([`IosDriver::device_crash_logs`]),
/// removed when the driver goes: each holds the device's whole crash-log
/// domain (review MINOR 12).
#[derive(Default)]
struct CrashLogDirs(std::sync::Mutex<CrashLogMap>);

impl std::ops::Deref for CrashLogDirs {
    type Target = std::sync::Mutex<CrashLogMap>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for CrashLogDirs {
    fn drop(&mut self) {
        let logs = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        for (_, dir) in logs.values() {
            if let Some(dir) = dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// I3 (D-50 sticky crash): the crash each (device, app) was seen to have
/// since its last launch, and the watcher polling a physical iPhone's app
/// state; watchers stop with the driver.
#[derive(Default)]
struct CrashWatch {
    seen: Arc<std::sync::Mutex<HashMap<(String, String), String>>>,
    tasks: std::sync::Mutex<HashMap<(String, String), tokio::task::AbortHandle>>,
}

impl CrashWatch {
    fn key(device_id: &str, app_id: &str) -> (String, String) {
        (device_id.to_string(), app_id.to_string())
    }

    /// Forget the crash and stop the watcher (a launch, a stop).
    fn reset(&self, device_id: &str, app_id: &str) {
        let key = Self::key(device_id, app_id);
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        if let Some(t) = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key)
        {
            t.abort();
        }
    }

    fn note(&self, device_id: &str, app_id: &str, detail: String) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(Self::key(device_id, app_id))
            .or_insert(detail);
    }

    fn get(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(device_id, app_id))
            .cloned()
    }

    fn stop_all(&self) {
        for (_, t) in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            t.abort();
        }
    }
}

impl Drop for CrashWatch {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// What the device watcher records: the runner says the app isn't running.
const DEVICE_APP_GONE: &str = "the app is not running on the device (it died since Velocity \
                               launched it; seen by the crash watcher)";

/// iOS platform driver using xcrun simctl for device management and
/// WebDriverAgent for UI automation.
/// Steps a picker wheel may take to reach a value (a minutes wheel has 60).
const PICKER_MAX_STEPS: usize = 80;

/// How long a picker wheel's value may lag a step (its scroll animation)
/// before an unchanged read means the wheel did not move.
const PICKER_SETTLE: Duration = Duration::from_millis(200);

/// A picker wheel (`XCUIElementTypePickerWheel`).
fn is_picker_wheel(el: &Element) -> bool {
    el.element_type
        .strip_prefix("XCUIElementType")
        .unwrap_or(&el.element_type)
        == "PickerWheel"
}

/// The number a value starts with ("6 o’clock" → 6, "05 minutes" → 5).
fn leading_number(s: &str) -> Option<u32> {
    let d: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
    d.parse().ok()
}

/// A picker wheel's value read: the text, "" when the wheel is gone
/// (`no such element`), and the error for anything else (a lost, busy or
/// restarted WDA is the device's failure, never the test's).
fn wheel_value(read: Result<String>) -> Result<String> {
    match read {
        Err(VelocityError::ElementNotFound { .. }) => Ok(String::new()),
        other => other,
    }
}

/// Whether a wheel showing `value` is at `want`: the same text (ignoring
/// case and typographic quotes), or the same leading number when `want` is
/// a number or starts with one ("6" and "6 o'clock" both match "6 o’clock").
fn wheel_value_matches(value: &str, want: &str) -> bool {
    let norm = |s: &str| s.trim().to_lowercase().replace(['’', '‘'], "'");
    let (v, w) = (norm(value), norm(want));
    if v == w {
        return true;
    }
    match (leading_number(&v), leading_number(&w)) {
        (Some(a), Some(b)) => {
            a == b && (w.chars().all(|c| c.is_ascii_digit()) || v.starts_with(&w))
        }
        _ => false,
    }
}

pub struct IosDriver {
    simctl: Simctl,
    /// Real iPhones and iPads.
    devicectl: Devicectl,
    /// A WDA the user runs themselves (e.g. a real device through iproxy);
    /// every device then shares it. `VELOCITY_WDA_URL` sets it.
    fixed_url: Option<String>,
    conns: Mutex<HashMap<String, Arc<DeviceConn>>>,
    /// When each (device, app) was last launched: a crash report older than
    /// that belongs to an earlier run.
    launched: std::sync::Mutex<HashMap<(String, String), std::time::SystemTime>>,
    /// (simulator, app) pairs seen installed, so the check runs once.
    installed: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
    /// D-50: where a physical device's crash reports were copied after the
    /// app died, per (device, app), with the launch they were copied for;
    /// `None`: the copy failed for that launch, so it isn't tried again.
    /// The directories are removed with the driver, or when a later launch
    /// copies afresh.
    device_crash_logs: CrashLogDirs,
    /// I3: crashes seen since each launch, and the physical-device watchers.
    crash_watch: CrashWatch,
    /// Check that `inputText`'s target is editable and took focus before
    /// typing (`VELOCITY_IOS_FOCUS_CHECK=1`; off by default, D-5).
    focus_check: bool,
    /// After `inputText`'s tap, fail when the tree reports focus on
    /// something else (D-5). On unless `VELOCITY_IOS_FOCUS_CHECK=off`.
    strict_focus: bool,
    /// Keep taps out of the status bar (D-4). On unless
    /// `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off`, which keeps the rest of the
    /// tap guard (keyboard, bars, off screen): for an app that hides its
    /// status bar while WDA still reports a height.
    status_bar_guard: bool,
    /// D-38 (`ios.verifyTap`, off by default): before a tap, ask the runner
    /// what is drawn at the tap point (VelocityRunner's private hit test)
    /// and refuse a tap that would land on something else.
    verify_tap: bool,
    /// Per-call WDA timeout; `None` is the client's default (30 s).
    wda_timeout: Option<Duration>,
    /// How long `launch_app` waits on the runner's terminate before
    /// falling back to `simctl terminate` ([`RUNNER_TERMINATE_TIMEOUT`]).
    terminate_timeout: Duration,
    /// The runner refuses keys while nothing has keyboard focus, before
    /// sending any (VelocityRunner: XCTest's `typeText`), so typing can
    /// start right after a tap and retry until the field takes focus.
    /// WDA sends keys through testmanagerd unchecked: they'd be lost.
    typing_checks_focus: bool,
    /// Each simulator's in-app sync probe port, passed to the app as
    /// `VELOCITY_SYNC_PORT`: simulators share the Mac's loopback, so a
    /// single 19400 let one simulator's host talk to another's app.
    sync_ports: std::sync::Mutex<HashMap<String, u16>>,
    /// Simulators whose status bar is overridden (D-V4), cleared on cleanup.
    status_bar_frozen: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Simulator sync probe ports: [`SYNC_PORT_BASE`] plus a hash of the UDID
/// within [`SYNC_PORT_SPAN`], clear of the probes' defaults (19400/19401)
/// and WDA's ports (8100 up).
const SYNC_PORT_BASE: u16 = 19500;
const SYNC_PORT_SPAN: u16 = 2000;

/// `udid`'s preferred sync probe port: the same on every run, so a probe
/// left over from the previous run is on the port Velocity checks.
fn sync_port_candidate(udid: &str) -> u16 {
    let h = udid.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    SYNC_PORT_BASE + (h % u64::from(SYNC_PORT_SPAN)) as u16
}

/// The first port from `udid`'s candidate on that no other simulator here
/// has and nothing else on the Mac listens on (another process's app, or
/// another Velocity's simulator). `None` when the whole range is taken.
///
/// Known race (review-f3-f4-s10 MINOR 8): the test bind is dropped at
/// once, so two Velocity processes whose UDIDs map to the same or
/// adjacent ports can both pick a port before either app listens; the
/// second app's bind then fails. It is not silent crosstalk: the host
/// checks the probe's `hello` names the app under test and polls
/// otherwise, and never latches on to another app's probe on the suite
/// port (MJ-A). Holding the bind until `simctl launch` would only narrow
/// the window, since the app binds after launch returns.
fn pick_sync_port(udid: &str, taken: &[u16]) -> Option<u16> {
    let first = sync_port_candidate(udid) - SYNC_PORT_BASE;
    (0..SYNC_PORT_SPAN)
        .map(|i| SYNC_PORT_BASE + (first + i) % SYNC_PORT_SPAN)
        .find(|p| !taken.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
}

impl Default for IosDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl IosDriver {
    /// `inputText` on a picker wheel turns it to `text` ("6", "6 o'clock",
    /// "45", "pm"). Typing into a time picker on iOS 26 goes to its number
    /// pad (an hour typed as "6 o'clock" set the minutes to 06), so the
    /// wheel is turned instead: WDA's exact set first, then one step at a
    /// time (`/wda/pickerwheel/select`), reading its value after each,
    /// until it matches (iOSWorld, 2026-11-08: every alarm task stalled).
    async fn set_picker_wheel(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        // VelocityRunner serves none of the routes below (`POST /elements`
        // answers `[]`): say so, not "a picker wheel not found" while it is
        // on screen (review 09 #6).
        if self.fixed_url.is_none() && crate::wda_bootstrap::velocity_runner() {
            return Err(VelocityError::Config(
                "setting a picker wheel needs WebDriverAgent's picker-wheel routes, which \
                 VelocityRunner (VELOCITY_IOS_RUNNER=velocity) does not serve: run this test \
                 with the default WebDriverAgent runner, or tap the wheel's rows instead"
                    .into(),
            ));
        }
        let target = text.to_string();
        let bounds = element.bounds;
        self.act(device_id, |c| {
            let target = target.clone();
            async move {
                let guard = c.wda.lock().await;
                let wda = guard.client();
                let wheels = wda
                    .find_elements("class name", "XCUIElementTypePickerWheel")
                    .await?;
                // The wheel the tree read: the one whose frame is nearest.
                let (cx, cy) = bounds.center();
                let mut best: Option<(f64, String)> = None;
                for w in wheels {
                    if let Ok((x, y, wd, ht)) = wda.element_rect(&w.element_id).await {
                        let d = (x + wd / 2.0 - f64::from(cx)).abs()
                            + (y + ht / 2.0 - f64::from(cy)).abs();
                        if best.as_ref().is_none_or(|(b, _)| d < *b) {
                            best = Some((d, w.element_id));
                        }
                    }
                }
                let Some((_, id)) = best else {
                    return Err(VelocityError::ElementNotFound {
                        selector: "a picker wheel".into(),
                        timeout_ms: 0,
                        screenshot: None,
                        hierarchy_snapshot: None,
                    });
                };
                // A read that failed is not an empty wheel: a lost or busy
                // WDA folded into "" ended in "it stopped at \"\"", a test
                // failure on a healthy-looking device (review 03 M4). Only a
                // wheel gone from the screen reads as empty.
                let value = |id: &str| {
                    let id = id.to_string();
                    async move { wheel_value(wda.element_value(&id).await) }
                };
                let mut now = value(&id).await?;
                if wheel_value_matches(&now, &target) {
                    return Ok(());
                }
                // WDA's own adjust when `text` is one of the wheel's values.
                let _ = wda.send_keys(&id, &target).await;
                now = value(&id).await?;
                if wheel_value_matches(&now, &target) {
                    return Ok(());
                }
                let mut order = match (leading_number(&now), leading_number(&target)) {
                    (Some(a), Some(b)) if b < a => "previous",
                    _ => "next",
                };
                let mut flipped = false;
                for _ in 0..PICKER_MAX_STEPS {
                    wda.picker_step(&id, order).await?;
                    let next = value(&id).await?;
                    if wheel_value_matches(&next, &target) {
                        return Ok(());
                    }
                    if next == now {
                        // UIPickerView updates its value when the scroll
                        // settles: one read inside the animation is not the
                        // end of the wheel. Read again before turning back.
                        tokio::time::sleep(PICKER_SETTLE).await;
                        let settled = value(&id).await?;
                        if wheel_value_matches(&settled, &target) {
                            return Ok(());
                        }
                        if settled != now {
                            now = settled;
                            continue;
                        }
                        // The end of a wheel that doesn't wrap: the other way.
                        if flipped {
                            break;
                        }
                        flipped = true;
                        order = if order == "next" { "previous" } else { "next" };
                    }
                    now = next;
                }
                Err(VelocityError::AssertionFailed {
                    expected: format!("picker wheel set to {target:?}"),
                    actual: format!("it stopped at {now:?}"),
                    selector: "picker wheel".into(),
                    screenshot: None,
                })
            }
        })
        .await
    }

    /// One WDA per simulator on ports from 8100 up, unless `VELOCITY_WDA_URL`
    /// names a WDA to use instead.
    pub fn new() -> Self {
        let fixed_url = velocity_common::env::var("VELOCITY_WDA_URL").ok();
        Self {
            simctl: Simctl::new(),
            devicectl: Devicectl::new(),
            typing_checks_focus: fixed_url.is_none() && crate::wda_bootstrap::velocity_runner(),
            fixed_url,
            conns: Mutex::new(HashMap::new()),
            launched: std::sync::Mutex::new(HashMap::new()),
            installed: std::sync::Mutex::new(std::collections::HashSet::new()),
            device_crash_logs: CrashLogDirs::default(),
            crash_watch: CrashWatch::default(),
            focus_check: velocity_common::env::var("VELOCITY_IOS_FOCUS_CHECK")
                .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "on")),
            strict_focus: !velocity_common::env::var("VELOCITY_IOS_FOCUS_CHECK")
                .is_ok_and(|v| matches!(v.trim(), "0" | "false" | "off")),
            status_bar_guard: !matches!(
                velocity_common::env::var("VELOCITY_IOS_TAP_GUARD_STATUS_BAR")
                    .as_deref()
                    .map(str::trim),
                Ok("off" | "0" | "false")
            ),
            verify_tap: matches!(
                velocity_common::env::var("VELOCITY_IOS_VERIFY_TAP")
                    .as_deref()
                    .map(str::trim),
                Ok("1" | "true" | "on" | "yes")
            ),
            wda_timeout: None,
            terminate_timeout: RUNNER_TERMINATE_TIMEOUT,
            sync_ports: std::sync::Mutex::new(HashMap::new()),
            status_bar_frozen: Default::default(),
        }
    }

    /// The simulator's sync probe port, chosen on its first launch and kept
    /// for the driver's life. Physical devices keep the suite's port: their
    /// probe is reached through the usbmux forward.
    fn sync_port(&self, device_id: &str) -> Option<u16> {
        if is_physical(device_id) {
            return None;
        }
        let mut ports = self.sync_ports.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = ports.get(device_id) {
            return Some(*p);
        }
        let taken: Vec<u16> = ports.values().copied().collect();
        let port = pick_sync_port(device_id, &taken)?;
        debug!(device_id, port, "sync probe port for simulator");
        ports.insert(device_id.to_string(), port);
        Some(port)
    }

    /// The launch environment for a simulator app: its sync probe port.
    fn app_env(&self, device_id: &str) -> Vec<(String, String)> {
        self.sync_port(device_id)
            .map(|p| vec![("VELOCITY_SYNC_PORT".to_string(), p.to_string())])
            .unwrap_or_default()
    }

    /// Give up on each WDA call after `timeout` instead of 30 s (tests).
    pub fn with_wda_timeout(mut self, timeout: Duration) -> Self {
        self.wda_timeout = Some(timeout);
        self
    }

    /// Turn the `inputText` focus check on or off (see
    /// `VELOCITY_IOS_FOCUS_CHECK`).
    pub fn with_focus_check(mut self, on: bool) -> Self {
        self.focus_check = on;
        self
    }

    /// Turn the D-5 check that focus landed on `inputText`'s target on or
    /// off (default on; `VELOCITY_IOS_FOCUS_CHECK=off`).
    pub fn with_strict_focus(mut self, on: bool) -> Self {
        self.strict_focus = on;
        self
    }

    /// Keep taps out of the status bar, or not (default on;
    /// `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off`, D-4).
    pub fn with_status_bar_guard(mut self, on: bool) -> Self {
        self.status_bar_guard = on;
        self
    }

    /// Check each tap point with the runner's hit test, or not (default
    /// off; `ios.verifyTap`, D-38).
    pub fn with_verify_tap(mut self, on: bool) -> Self {
        self.verify_tap = on;
        self
    }

    /// A physical device has no process table or crash folder Velocity can
    /// read cheaply: the app Velocity launched (and didn't stop) reporting
    /// "not running" through the runner means it died (D-50 step 1).
    async fn device_app_gone(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id.to_string(), app_id.to_string()))?;
        let conn = self.conns.lock().await.get(device_id).cloned()?;
        let state = conn.wda.lock().await.client().app_state(app_id).await;
        if state != Some(1) {
            return None;
        }
        // D-50: the device's own crash report says why.
        let reports = self.device_crash_reports(device_id, app_id).await;
        Some(match reports.first() {
            Some((_, _, summary)) => summary.clone(),
            None => "the app is not running on the device (terminated since Velocity \
                     launched it; no crash report for it on the device)"
                .to_string(),
        })
    }

    /// D-50: this launch's crash reports for `app_id` on a physical device,
    /// newest first, copied off it with devicectl once per launch (into a
    /// temporary directory kept for the run).
    async fn device_crash_reports(
        &self,
        device_id: &str,
        app_id: &str,
    ) -> Vec<(std::time::SystemTime, std::path::PathBuf, String)> {
        let key = (device_id.to_string(), app_id.to_string());
        let Some(since) = self
            .launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
        else {
            return Vec::new();
        };
        let cached = self
            .device_crash_logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .filter(|(at, _)| *at == since)
            .map(|(_, dir)| dir.clone());
        let dir = match cached {
            Some(Some(dir)) => dir,
            // Failed for this launch: every crash check retried it, two
            // devicectl round trips of up to 120 s each (review MINOR 12).
            Some(None) => return Vec::new(),
            None => {
                let dir = std::env::temp_dir().join(format!(
                    "velocity-crashlogs-{}-{}-{}",
                    std::process::id(),
                    device_id.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
                    since
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ));
                let _ = tokio::fs::remove_dir_all(&dir).await;
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    debug!("crash log dir: {e}");
                    return Vec::new();
                }
                let copied = self.devicectl.copy_crash_logs(device_id, &dir).await;
                if let Err(e) = &copied {
                    debug!("copying the device's crash logs failed: {e}");
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                }
                let kept = copied.is_ok().then(|| dir.clone());
                let old = self
                    .device_crash_logs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, (since, kept.clone()));
                // An earlier launch's copy: the whole domain, never read again.
                if let Some((_, Some(old))) = old {
                    let _ = tokio::fs::remove_dir_all(old).await;
                }
                match kept {
                    Some(dir) => dir,
                    None => return Vec::new(),
                }
            }
        };
        // Every crash report the device kept, parsed: off the async workers.
        let app_id = app_id.to_string();
        tokio::task::spawn_blocking(move || app_reports(&ips_files(&dir, 4), &app_id, since))
            .await
            .unwrap_or_default()
    }

    /// Clear a simulator app's data container (AsyncStorage, NSUserDefaults,
    /// caches): 208 ms, against 604 ms for a reinstall. A container that
    /// can't be resolved or wiped fails the step: it used to be a WARN, and
    /// the test ran on the previous state as if `clearState` had worked
    /// (VEL-S45).
    async fn clear_sim_data(&self, device_id: &str, app_id: &str) -> Result<()> {
        let failed = |detail: String| VelocityError::CommandFailed {
            device_id: device_id.to_string(),
            command: "clearState".into(),
            detail,
        };
        let data_path = self
            .simctl
            .get_app_container(device_id, app_id, "data")
            .await
            .map_err(|e| failed(format!("could not resolve {app_id}'s data container: {e}")))?;
        debug!(path = %data_path, "clearing app data container");
        // UserDefaults go through cfprefsd, which caches them and
        // wrote them back after the plist was deleted ("Launches:
        // 3" after clearState, ~1 run in 6). Delete them through it,
        // by the container plist's path: `defaults delete <bundle>`
        // (fix 25) named the simulator's own domain, not the app's.
        // (It fails when there are no defaults yet: not an error.)
        let plist = format!("{data_path}/Library/Preferences/{app_id}");
        let _ = self.simctl.defaults_delete(device_id, &plist).await;
        for subdir in &["Library", "Documents", "tmp"] {
            let dir = format!("{}/{}", data_path, subdir);
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(failed(format!("could not clear {dir}: {e}"))),
            }
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| failed(format!("could not recreate {dir}: {e}")))?;
        }
        Ok(())
    }

    /// The app was stopped on purpose (stopApp, saveState, restoreState):
    /// its missing process is no longer a crash.
    fn forget_launch(&self, device_id: &str, app_id: &str) {
        self.launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(device_id.to_string(), app_id.to_string()));
        self.crash_watch.reset(device_id, app_id);
    }

    /// I3 (D-50 sticky crash): on a physical iPhone, poll the app's state
    /// through the runner (`/wda/apps/state`, one cheap call) every
    /// `crash.checkEveryMs` (at least 250 ms) from its launch until it is
    /// stopped or relaunched. "Not running" is recorded once and the
    /// watcher ends; the executor then fails every later step with the
    /// crash at once ([`PlatformDriver::pending_crash`]) instead of waiting
    /// out element timeouts or tapping the home screen. Simulators need
    /// no watcher: their crash check reads the process table.
    async fn start_crash_watch(&self, device_id: &str, app_id: &str) {
        let Some(conn) = self.conns.lock().await.get(device_id).cloned() else {
            return;
        };
        let every = velocity_common::settings::ms("VELOCITY_CRASH_CHECK_EVERY_MS", 1000)
            .max(Duration::from_millis(250));
        let seen = self.crash_watch.seen.clone();
        let key = CrashWatch::key(device_id, app_id);
        let (dev, app) = key.clone();
        // Its own short-timeout client. The session id and auth are read
        // through the client's published handle, never the device's
        // request lock: a stuck 30 s `/source` used to hold every tick up
        // (review 01 #9).
        let (url, published, udid) = {
            let wda = conn.wda.lock().await;
            (
                wda.client().base_url().to_string(),
                wda.client().published(),
                wda.client().device_id().to_string(),
            )
        };
        drop(conn);
        let mut probe =
            crate::wda::WdaClient::with_timeout(&url, every.min(Duration::from_secs(2)));
        probe.set_device_id(&udid);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let (sid, auth) = published.get();
                probe.set_auth(auth);
                if probe.session_id() != sid.as_deref() {
                    probe.set_session_id_opt(sid);
                }
                let state = probe.app_state(&app).await;
                if state == Some(1) {
                    debug!(device_id = %dev, app_id = %app, "crash watcher: the app is gone");
                    seen.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entry((dev.clone(), app.clone()))
                        .or_insert_with(|| DEVICE_APP_GONE.to_string());
                    return;
                }
            }
        });
        if let Some(old) = self
            .crash_watch
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, task.abort_handle())
        {
            old.abort();
        }
    }

    /// Use the WDA at `wda_base_url` for every device.
    pub fn with_wda_url(wda_base_url: &str) -> Self {
        Self {
            fixed_url: Some(wda_base_url.to_string()),
            typing_checks_focus: false,
            ..Self::new()
        }
    }

    /// The device's connection, created (with its port) on first use.
    /// The map lock is never held across a device's own locks or the port
    /// probe (`lsof`), so a device bootstrapping WDA never stalls another.
    async fn conn(&self, device_id: &str) -> Arc<DeviceConn> {
        loop {
            let taken: Vec<u16> = {
                let conns = self.conns.lock().await;
                if let Some(c) = conns.get(device_id) {
                    return c.clone();
                }
                conns.values().map(|c| c.port).collect()
            };
            let (url, port) = match &self.fixed_url {
                Some(url) => {
                    let port = url
                        .rsplit(':')
                        .next()
                        .and_then(|p| p.trim_end_matches('/').parse().ok())
                        .unwrap_or(wda_bootstrap::BASE_PORT);
                    (url.clone(), port)
                }
                None => {
                    let port = wda_bootstrap::pick_port(device_id, &taken).await;
                    // Not `localhost`: it may resolve to ::1 first, and the
                    // runners listen on 127.0.0.1 only (IOS-N7).
                    (format!("http://127.0.0.1:{port}"), port)
                }
            };
            let mut conns = self.conns.lock().await;
            if let Some(c) = conns.get(device_id) {
                return c.clone();
            }
            // Another device took the port while we probed: pick again.
            if self.fixed_url.is_none() && conns.values().any(|c| c.port == port) {
                continue;
            }
            debug!(device_id, port, "WDA connection for device");
            let conn = Arc::new(DeviceConn::new(
                device_id,
                &url,
                port,
                is_physical(device_id),
                self.wda_timeout,
            ));
            conns.insert(device_id.to_string(), conn.clone());
            return conn;
        }
    }

    /// The device's connection with WDA running (launched if needed), and
    /// its existing session adopted if it has one. Opens no new session:
    /// `launch_app` creates the app's own, so creating an empty one first
    /// cost an extra ~5 s per run.
    async fn running(&self, device_id: &str) -> Result<Arc<DeviceConn>> {
        let conn = self.conn(device_id).await;
        if !conn.prepared.load(Ordering::Acquire) {
            if self.fixed_url.is_none() {
                let (wda_ref, auth) = {
                    let mut bootstrap = conn.bootstrap.lock().await;
                    bootstrap.ensure_running(device_id).await?;
                    (bootstrap.wda_ref().map(str::to_string), bootstrap.auth())
                };
                let mut wda = conn.wda.lock().await;
                wda.client_mut().set_wda_ref(wda_ref.as_deref());
                // R-1: the runner's per-launch token (and on a device its
                // own `Host`) on every request.
                wda.client_mut().set_auth(auth);
            }
            conn.wda.lock().await.adopt(device_id).await;
            // VelocityRunner has no frontmost-app lookup: tell it what is
            // installed, so a session without an app reads the screen.
            if self.fixed_url.is_none()
                && crate::wda_bootstrap::velocity_runner()
                && !is_physical(device_id)
            {
                match self.simctl.installed_bundles(device_id).await {
                    Ok(bundles) => {
                        let wda = conn.wda.lock().await;
                        if let Err(e) = wda.client().set_app_candidates(&bundles).await {
                            debug!(device_id, "app candidates not sent: {e}");
                        }
                    }
                    Err(e) => debug!(device_id, "listing installed apps failed: {e}"),
                }
            }
            conn.prepared.store(true, Ordering::Release);
        }
        Ok(conn)
    }

    /// Stop `app_id` through the runner's current session; false when
    /// there is none, the runner refused, or it gave no answer within
    /// `timeout` (a wedged runner would otherwise hold the launch for the
    /// client's full 30 s), and simctl has to do it.
    async fn terminate_in_session(conn: &DeviceConn, app_id: &str, timeout: Duration) -> bool {
        // The timeout covers the wait for the connection too: a request
        // stuck on it (up to the client's 30 s) used to come first (review
        // 01 #9).
        let attempt = async {
            let wda = conn.wda.lock().await;
            wda.client().session_id()?;
            Some(wda.client().terminate_app(app_id).await)
        };
        match tokio::time::timeout(timeout, attempt).await {
            Ok(None) => false,
            Ok(Some(Ok(()))) => true,
            Ok(Some(Err(e))) => {
                debug!(app_id, "runner terminate failed ({e}); using simctl");
                false
            }
            Err(_) => {
                debug!(app_id, ?timeout, "runner terminate timed out; using simctl");
                false
            }
        }
    }

    /// [`Self::running`], plus a session for commands that need one (the
    /// inspector can act before any app launch).
    async fn ready(&self, device_id: &str) -> Result<Arc<DeviceConn>> {
        let conn = self.running(device_id).await?;
        let mut wda = conn.wda.lock().await;
        if wda.client().session_id().is_none() {
            debug!(device_id, "creating default WDA session");
            wda.ensure_session(device_id, "").await?;
        }
        drop(wda);
        Ok(conn)
    }

    /// Where a live view can read `device_id`'s screen as MJPEG
    /// (WebDriverAgent's MJPEG server), best first; empty when there is none
    /// (VelocityRunner, real iPhones: the stream would need its own usbmux
    /// forward). Turns the stream up to 30 fps at half scale first.
    ///
    /// A WDA launched without `MJPEG_SERVER_PORT` (an older Velocity's, kept
    /// running) serves on 9100. That port is only offered when this driver
    /// knows one device, since it may be another simulator's.
    pub async fn mjpeg_urls(&self, device_id: &str) -> Result<Vec<String>> {
        if is_physical(device_id)
            || (self.fixed_url.is_none() && crate::wda_bootstrap::velocity_runner())
        {
            return Ok(Vec::new());
        }
        let conn = self.ready(device_id).await?;
        if let Err(e) = conn.wda.lock().await.client().set_mjpeg(30, 50, 50).await {
            debug!(device_id, "MJPEG settings not applied: {e}");
        }
        let mut urls = vec![format!(
            "http://127.0.0.1:{}/",
            crate::wda_bootstrap::mjpeg_port(conn.port)
        )];
        if self.conns.lock().await.len() == 1 {
            urls.push("http://127.0.0.1:9100/".into());
        }
        Ok(urls)
    }

    /// Ensure WDA is running on `device_id`, building and launching it if
    /// needed.
    pub async fn prepare(&self, device_id: &str) -> Result<()> {
        self.running(device_id).await.map(|_| ())
    }

    /// Stop every WDA process we started.
    pub async fn cleanup(&self) {
        self.crash_watch.stop_all();
        let conns: Vec<Arc<DeviceConn>> = self.conns.lock().await.values().cloned().collect();
        for c in conns {
            c.bootstrap.lock().await.stop().await;
        }
        self.clear_status_bar_overrides().await;
    }

    /// Clear every `simctl status_bar override` this driver set (D-V4).
    pub async fn clear_status_bar_overrides(&self) {
        let frozen: Vec<String> = self
            .status_bar_frozen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for device_id in frozen {
            if let Err(e) = self.simctl.status_bar_clear(&device_id).await {
                tracing::warn!("{device_id}: status bar override not cleared: {e}");
            }
        }
    }

    /// `simctl status_bar override` once per simulator (D-V4). Real
    /// devices have no equivalent: their bar stays masked.
    async fn freeze_simulator_status_bar(&self, device_id: &str) -> bool {
        if is_physical(device_id) {
            return false;
        }
        if self
            .status_bar_frozen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(device_id)
        {
            return true;
        }
        match self.simctl.status_bar_override(device_id).await {
            Ok(()) => {
                self.status_bar_frozen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(device_id.to_string());
                true
            }
            Err(e) => {
                tracing::warn!("{device_id}: status bar not frozen ({e}); masking it");
                false
            }
        }
    }

    /// WDA stopped answering: relaunch it if it died, and open a new session
    /// for the same app. Returns whether it's back.
    ///
    /// Only a WDA that was really restarted gets a session with the app
    /// (which relaunches it). One that still answers was only slow or
    /// dropped a connection: its session is kept when it's alive, else
    /// renewed without the app, so the app's state survives (review M1).
    async fn recover(&self, device_id: &str, conn: &DeviceConn) -> bool {
        conn.invalidate();
        let answers = self.wda_alive(conn).await;
        if answers {
            warn!(
                device_id,
                "WebDriverAgent dropped a request but still answers"
            );
            if conn.wda.lock().await.client().session_alive().await {
                return true;
            }
            if self.renew_session(device_id, conn).await {
                return true;
            }
            // `/status` answers off the main thread (VelocityRunner serves
            // it on its network queue), so a wedged runner looks alive:
            // when its session cannot be renewed either, restart it
            // (review MAJOR-1).
            warn!(
                device_id,
                "WebDriverAgent answers /status but cannot open a session; restarting it"
            );
        } else {
            warn!(device_id, "WebDriverAgent not reachable; restarting it");
        }
        if self.fixed_url.is_none() {
            let mut bootstrap = conn.bootstrap.lock().await;
            let started = if answers {
                bootstrap.restart(device_id).await
            } else {
                bootstrap.ensure_running(device_id).await
            };
            if let Err(e) = started {
                warn!(device_id, "WDA restart failed: {e}");
                if matches!(e, VelocityError::RunnerFailing { .. }) {
                    *conn
                        .runner_failing
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                return false;
            }
            let wda_ref = bootstrap.wda_ref().map(str::to_string);
            let auth = bootstrap.auth();
            drop(bootstrap);
            let mut wda = conn.wda.lock().await;
            wda.client_mut().set_wda_ref(wda_ref.as_deref());
            // A relaunched runner has a new token.
            wda.client_mut().set_auth(auth);
        }
        let bundle = conn
            .bundle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut wda = conn.wda.lock().await;
        wda.invalidate_session();
        match wda.client_mut().create_session(&bundle).await {
            Ok(_) => {
                info!(device_id, "WebDriverAgent restarted; session re-created");
                true
            }
            Err(e) => {
                warn!(device_id, "WDA session re-create failed: {e}");
                false
            }
        }
    }

    /// WDA no longer knows the session (it restarted, or another client
    /// took it over: VEL-143). Nothing ran under the old one, so open a new
    /// one pointed at the same app, without relaunching it. Returns whether
    /// that worked. Always a fresh session: adopting the one `/status`
    /// names would take another client's, and the two would keep taking
    /// it from each other.
    async fn renew_session(&self, device_id: &str, conn: &DeviceConn) -> bool {
        warn!(device_id, "WDA session gone; opening a new one");
        conn.invalidate();
        let bundle = conn
            .bundle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut wda = conn.wda.lock().await;
        wda.invalidate_session();
        match wda.renew(device_id, &bundle).await {
            Ok(()) => true,
            Err(e) => {
                warn!(device_id, "WDA session re-create failed: {e}");
                false
            }
        }
    }

    /// Whether WDA still answers: on a real device through a fresh usbmux
    /// connection to the device port (the local forward accepts whether or
    /// not anything listens there), on a simulator its `/status`.
    async fn wda_alive(&self, conn: &DeviceConn) -> bool {
        if let Some(alive) = conn.bootstrap.lock().await.probe().await {
            return alive;
        }
        conn.wda
            .lock()
            .await
            .client()
            .health_check()
            .await
            .unwrap_or(false)
    }

    /// Fetch and parse the tree. A refused or broken connection means WDA
    /// died; it is restarted and the read (safe to repeat) is retried once.
    /// A lost session is re-opened and the read retried.
    /// D-5: after `inputText`'s tap, when the runner reports focus, it must
    /// be on the target (or overlap it) before any key is sent: keys aimed
    /// at a label went into the field focused before (VEL-S46). Focus can
    /// lag the tap, so a mismatch is re-read for about a second. A tree
    /// with no focused element changes nothing.
    async fn verify_focus(
        &self,
        device_id: &str,
        conn: &DeviceConn,
        element: &Element,
        before: Option<&Element>,
        fail: &(dyn Fn(String) -> VelocityError + Sync),
    ) -> Result<()> {
        use std::sync::atomic::Ordering;
        if conn.reports_focus.load(Ordering::Relaxed) == 2 {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_millis(1000);
        loop {
            let tree = self.read_tree(device_id).await?;
            match tree.focus_check_after_tap(element, before) {
                FocusCheck::OnTarget => {
                    conn.reports_focus.store(1, Ordering::Relaxed);
                    return Ok(());
                }
                FocusCheck::Unknown => {
                    if holds_text_input(element) && conn.reports_focus.load(Ordering::Relaxed) == 0
                    {
                        conn.reports_focus.store(2, Ordering::Relaxed);
                    }
                    return Ok(());
                }
                FocusCheck::Elsewhere(other) => {
                    conn.reports_focus.store(1, Ordering::Relaxed);
                    if Instant::now() >= deadline {
                        return Err(fail(format!(
                            "the focus stayed on {} {:?} at {:?}; the keys would go there \
                             (select the text field itself, or VELOCITY_IOS_FOCUS_CHECK=off)",
                            other.element_type,
                            other.label.as_deref().unwrap_or(&other.platform_id),
                            other.bounds
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
        }
    }

    async fn read_tree(&self, device_id: &str) -> Result<Arc<Element>> {
        let conn = self.ready(device_id).await?;
        let xml = match Self::fetch_source(&conn).await {
            Err(VelocityError::WdaUnreachable { .. }) if self.recover(device_id, &conn).await => {
                Self::fetch_source(&conn).await?
            }
            Err(e @ VelocityError::WdaUnreachable { .. }) => return Err(conn.or_runner_failing(e)),
            Err(VelocityError::WdaSessionLost { .. })
                if self.renew_session(device_id, &conn).await =>
            {
                Self::fetch_source(&conn).await?
            }
            other => other?,
        };
        let mut tree = parse_ios_hierarchy(&xml)?;
        conn.wda
            .lock()
            .await
            .client()
            .note_app_frame(tree.bounds.width, tree.bounds.height);
        let due = conn
            .alert_clear_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|t| Instant::now() >= t);
        if due && system_alerts_enabled() && !Self::has_alert(&tree) {
            match conn.wda.lock().await.client().alert().await {
                Some((text, buttons)) => {
                    debug!(%text, ?buttons, "system alert in front");
                    tree.children
                        .push(system_alert(&tree.bounds, &text, &buttons));
                }
                None => {
                    *conn
                        .alert_clear_until
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now() + alert_recheck());
                }
            }
        }
        let tree = Arc::new(tree);
        *conn.tree.lock().unwrap_or_else(|e| e.into_inner()) = Some((tree.clone(), Instant::now()));
        Ok(tree)
    }

    async fn fetch_source(conn: &DeviceConn) -> Result<String> {
        conn.wda.lock().await.client().get_source().await
    }

    /// The step's tree: the one already read since the last action, else fresh.
    async fn tree(&self, device_id: &str) -> Result<Arc<Element>> {
        let conn = self.conn(device_id).await;
        let tree = match conn.cached_tree() {
            Some(t) => t,
            None => self.read_tree(device_id).await?,
        };
        Ok(self.with_visibility(&conn, tree).await)
    }

    /// A tree with a modal layer in it (a sheet, a popover, an in-app
    /// alert, the `PopoverDismissRegion`), its `visible` flags read from
    /// WebDriverAgent. Trees are read without `visible` (it costs ~10x),
    /// so everything with a frame on screen passes for shown, a target
    /// behind a sheet included. The tap guard refuses those by tree order,
    /// but an `assertVisible` or a bare selector still took them. Paid only
    /// for the step's lookup tree (never for the settle's polls) and only
    /// while a modal layer is up: one more `/source` there. VelocityRunner's
    /// `visible` is frame-only, so it isn't asked. The flags are copied onto
    /// the tree already read, and only when both reads have the same shape
    /// (the screen didn't change between them); otherwise nothing changes.
    /// While nothing changes the `visible` flags are reused for up to
    /// [`VISIBILITY_REUSE`] (10 s): polling an `assertVisible` under a sheet
    /// reads them once, not once per poll. The read is dropped after every
    /// action and when the screen's shape changes. Off with
    /// `VELOCITY_IOS_SHEET_VISIBILITY=off`.
    async fn with_visibility(&self, conn: &DeviceConn, tree: Arc<Element>) -> Arc<Element> {
        if !sheet_visibility()
            || (self.fixed_url.is_none() && crate::wda_bootstrap::velocity_runner())
            || velocity_common::env::var("VELOCITY_IOS_FULL_SOURCE").as_deref() == Ok("1")
            || conn
                .visible_for
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&tree)))
            || !crate::hit::has_modal_layer(&tree)
        {
            return tree;
        }
        // Reuse the last visibility-bearing tree if it's fresh enough and has
        // the same shape as the current one: one visibility read per screen,
        // not one per poll.
        let reused = conn
            .last_visibility
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(_, at)| at.elapsed() < VISIBILITY_REUSE)
            .and_then(|(cached, _)| {
                let mut merged = (*tree).clone();
                crate::hit::copy_earlier_visibility(&mut merged, cached).then_some(merged)
            });
        if let Some(merged) = reused {
            debug!("reusing visibility flags read under a sheet (no new /source request)");
            let merged = Arc::new(merged);
            let mut cache = conn.tree.lock().unwrap_or_else(|e| e.into_inner());
            // Only replace the cached tree this was built from.
            if let Some((t, _)) = cache.as_mut() {
                if Arc::ptr_eq(t, &tree) {
                    *t = merged.clone();
                }
            }
            drop(cache);
            *conn.visible_for.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Arc::downgrade(&merged));
            return merged;
        }
        let started = Instant::now();
        let xml = conn
            .wda
            .lock()
            .await
            .client()
            .get_source_with_visibility()
            .await;
        let full = match xml.and_then(|x| parse_ios_hierarchy(&x)) {
            Ok(t) => t,
            Err(e) => {
                debug!("visibility read under a sheet failed: {e}");
                return tree;
            }
        };
        let full = Arc::new(full);
        *conn
            .last_visibility
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((full.clone(), Instant::now()));
        let mut merged = (*tree).clone();
        if !crate::hit::copy_visibility(&mut merged, &full) {
            debug!("the screen changed between reads; visibility not applied");
            return tree;
        }
        let elapsed_ms = started.elapsed().as_millis();
        debug!("visibility read under a sheet took {elapsed_ms} ms");
        let merged = Arc::new(merged);
        let mut cache = conn.tree.lock().unwrap_or_else(|e| e.into_inner());
        // Only replace the cached tree this was built from.
        if let Some((t, _)) = cache.as_mut() {
            if Arc::ptr_eq(t, &tree) {
                *t = merged.clone();
            }
        }
        drop(cache);
        *conn.visible_for.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&merged));
        merged
    }

    /// Run an action. Actions are never replayed after an ambiguous failure
    /// (a tap might have landed); only a refused connection — the request
    /// never reached WDA — is retried, once, after restarting it, and a lost
    /// session (WDA refused the request) once, in a new session. A
    /// connection that broke mid-request fails the step; WDA is restarted
    /// first when it is dead, so the next step finds it running.
    async fn act<T, F, Fut>(&self, device_id: &str, op: F) -> Result<T>
    where
        F: Fn(Arc<DeviceConn>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let conn = self.ready(device_id).await?;
        conn.invalidate();
        match op(conn.clone()).await {
            Err(VelocityError::WdaUnreachable { .. }) if self.recover(device_id, &conn).await => {
                op(conn).await
            }
            Err(e @ VelocityError::WdaUnreachable { .. }) => Err(conn.or_runner_failing(e)),
            Err(VelocityError::WdaSessionLost { .. })
                if self.renew_session(device_id, &conn).await =>
            {
                op(conn).await
            }
            Err(e @ VelocityError::WdaLost { .. }) => {
                if !self.wda_alive(&conn).await {
                    self.recover(device_id, &conn).await;
                }
                Err(e)
            }
            // WDA named the crash: it sticks until the next launch (I3).
            Err(VelocityError::AppCrashed { app_id, detail }) => {
                self.crash_watch.note(device_id, &app_id, detail.clone());
                Err(VelocityError::AppCrashed { app_id, detail })
            }
            other => other,
        }
    }

    /// Whether a hardware key reached the app since its last launch (the
    /// first one after a launch is dropped).
    fn keys_primed(&self, device_id: &str, conn: &DeviceConn) -> bool {
        let bundle = conn
            .bundle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let launched = self
            .launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id.to_string(), bundle))
            .copied();
        let at = *conn.keys_primed.lock().unwrap_or_else(|e| e.into_inner());
        matches!((at, launched), (Some(a), Some(l)) if a > l)
    }

    /// Launch on a real device. There's no data container to wipe from the
    /// host, so `clearState` there needs a reinstall (`reset.appPath`).
    /// A simulator app that isn't installed is a config error naming it.
    /// WDA opens a session for it, and its launch route answers OK, so the
    /// missing app read as "crashed" (Clock on an iOS 26 simulator). Once
    /// per (simulator, app); any other failure of the check lets the launch
    /// go ahead and fail its own way.
    async fn check_installed(&self, device_id: &str, app_id: &str) -> Result<()> {
        let key = (device_id.to_string(), app_id.to_string());
        if self
            .installed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            return Ok(());
        }
        match self
            .simctl
            .get_app_container(device_id, app_id, "app")
            .await
        {
            Ok(_) => {
                self.installed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key);
                Ok(())
            }
            // simctl's own answer, not a missing xcrun (whose spawn error
            // says the same words: Linux, a machine without Xcode).
            Err(e)
                if e.to_string().contains("No such file or directory")
                    && !e.to_string().contains("Failed to execute") =>
            {
                Err(VelocityError::Config(format!(
                    "{app_id} is not installed on simulator {device_id}"
                )))
            }
            Err(e) => {
                debug!(device_id, app_id, "install check failed: {e}");
                Ok(())
            }
        }
    }

    async fn launch_on_device(
        &self,
        device_id: &str,
        app_id: &str,
        clear_state: bool,
        argv: &[String],
    ) -> Result<()> {
        if clear_state {
            warn!(
                device_id,
                "clearState can't wipe app data on a real device; use reset: {{ strategy: reinstall, appPath }}"
            );
        }
        let conn = self.running(device_id).await?;
        conn.invalidate();
        *conn.bundle.lock().unwrap_or_else(|e| e.into_inner()) = app_id.to_string();
        let mut wda = conn.wda.lock().await;
        wda.client_mut().set_app_args(argv.to_vec());
        if wda.ensure_session(device_id, app_id).await? {
            return Ok(());
        }
        // Always a cold start, as on simulators and Android: WDA's
        // /wda/apps/launch only brings a running app to the front, so a test
        // inherited the previous test's screen and its launch arguments
        // never arrived (seen on an iPhone 12 with the echo app). Not
        // running is fine.
        if let Err(e) = wda.client().terminate_app(app_id).await {
            debug!("terminating {app_id} before its launch: {e}");
        }
        match wda.client().launch_app(app_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                debug!("WDA launch failed ({e}); using devicectl");
                drop(wda);
                self.devicectl.launch(device_id, app_id, None, argv).await
            }
        }
    }

    /// Translate a velocity Selector into a WDA locator strategy and value.
    /// User text goes into NSPredicate string literals, escaped (VEL-147:
    /// it was spliced into a class chain as is, so a `"` rewrote the query).
    /// Text values are normalised as the tree matcher normalises them
    /// (D-10): WDA can't normalise the element side, but a selector
    /// written with a stray newline or double space still finds the label.
    fn selector_to_wda(selector: &Selector) -> Result<(&'static str, String)> {
        use velocity_common::selector_match::normalize;
        // The query as written and normalised: WDA compares the raw label,
        // so a label that really holds a newline, NBSP or double space is
        // found by the raw form, a stray one in the selector by the
        // normalised one (review MINOR 11).
        let cmp = |fields: &[&str], op: &str, s: &str| {
            let norm = normalize(s);
            let mut forms = vec![ns_literal(&norm)];
            if norm != s {
                forms.push(ns_literal(s));
            }
            let parts: Vec<String> = fields
                .iter()
                .flat_map(|f| forms.iter().map(move |lit| format!("{f} {op} {lit}")))
                .collect();
            parts.join(" OR ")
        };
        let group = |q: String| {
            if q.contains(" OR ") {
                format!("({q})")
            } else {
                q
            }
        };
        match selector {
            Selector::Id(id) => Ok(("accessibility id", id.clone())),
            Selector::Text(text) => Ok((
                "predicate string",
                // `value` too: the tree matcher matches a field's contents.
                cmp(&["label", "name", "value"], "==", text),
            )),
            Selector::TextContains(sub) => {
                Ok(("predicate string", cmp(&["label"], "CONTAINS[c]", sub)))
            }
            Selector::AccessibilityId(aid) => Ok(("accessibility id", aid.clone())),
            // WDA wants the full name: `Button` never matched (VEL-308).
            Selector::ClassName(cls) => Ok(("class name", xcui_type(cls))),
            Selector::Index { .. } => Err(VelocityError::Config(
                "Index selector must be resolved at driver level".to_string(),
            )),
            Selector::Compound(selectors) => {
                let mut predicates = Vec::new();
                for s in selectors {
                    match s {
                        Selector::Id(id) | Selector::AccessibilityId(id) => {
                            predicates.push(format!("name == {}", ns_literal(id)));
                        }
                        Selector::Text(text) => {
                            predicates.push(group(cmp(&["label"], "==", text)));
                        }
                        Selector::TextContains(sub) => {
                            predicates.push(group(cmp(&["label"], "CONTAINS[c]", sub)));
                        }
                        Selector::ClassName(cls) => {
                            predicates.push(format!("type == {}", ns_literal(&xcui_type(cls))));
                        }
                        _ => {}
                    }
                }
                if predicates.is_empty() {
                    return Err(VelocityError::Config(
                        "Compound selector produced no predicates".to_string(),
                    ));
                }
                Ok(("predicate string", predicates.join(" AND ")))
            }
            Selector::Css(_) | Selector::XPath(_) => Err(VelocityError::Config(
                "css/xpath selectors are not supported on iOS (web platform only)".into(),
            )),
            // Regexes run in Rust on the tree (D-11): NSPredicate's ICU
            // regex would mean something else.
            Selector::TextMatches(_) | Selector::IdMatches(_) => Err(VelocityError::Config(
                "textMatches/idMatches resolve from the tree, not a WDA query".into(),
            )),
            // Resolved from the tree only; WDA has no layout query.
            Selector::Relative { .. } => Err(VelocityError::Config(
                "relative selectors resolve from the tree, not a WDA query".into(),
            )),
            // Roles, names and states are computed on the tree (S1/S2).
            Selector::Role(_)
            | Selector::Name(_)
            | Selector::Checked(_)
            | Selector::Focused(_)
            | Selector::Selected(_)
            | Selector::Enabled(_) => Err(VelocityError::Config(
                "role, name and state selectors resolve from the tree, not a WDA query".into(),
            )),
        }
    }

    /// Elements from the parsed tree carry bounds; their platform_id is an
    /// accessibility identifier or label, not a WDA element UUID.
    /// An alert already in the app's own tree (one the app presented).
    fn has_alert(root: &Element) -> bool {
        root.element_type.ends_with("Alert") || root.children.iter().any(Self::has_alert)
    }

    /// A drag through the middle of the screen, half its size.
    async fn drag_direction(
        &self,
        device_id: &str,
        direction: Direction,
        fling: bool,
    ) -> Result<()> {
        debug!(?direction, fling, "swiping by direction");
        self.act(device_id, |c| async move {
            let wda = c.wda.lock().await;
            let (w, h) = wda.client().get_screen_size().await?;
            let cx = w as f64 / 2.0;
            let cy = h as f64 / 2.0;
            let dist_x = w as f64 / 4.0;
            let dist_y = h as f64 / 4.0;
            let (from_x, from_y, to_x, to_y) = match direction {
                Direction::Up => (cx, cy + dist_y, cx, cy - dist_y),
                Direction::Down => (cx, cy - dist_y, cx, cy + dist_y),
                Direction::Left => (cx + dist_x, cy, cx - dist_x, cy),
                Direction::Right => (cx - dist_x, cy, cx + dist_x, cy),
            };
            wda.client()
                .swipe(from_x, from_y, to_x, to_y, DRAG_PRESS_S, fling)
                .await
        })
        .await
    }

    async fn drag_line(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        fling: bool,
    ) -> Result<()> {
        debug!(?from, ?to, fling, "swiping by coordinates");
        self.act(device_id, |c| async move {
            c.wda
                .lock()
                .await
                .client()
                .swipe(
                    from.0 as f64,
                    from.1 as f64,
                    to.0 as f64,
                    to.1 as f64,
                    DRAG_PRESS_S,
                    fling,
                )
                .await
        })
        .await
    }

    /// A row switch is tapped at its trailing end, where the knob is in a
    /// left-to-right layout. In a mirrored (RTL) row that is the label, and
    /// the switch stays as it was. So when its value hasn't changed after
    /// [`ROW_SWITCH_WAIT`], tap the leading end once (review 09 #11). The
    /// value is read several times first: a flip that shows late must not
    /// be flipped back.
    async fn row_switch_fallback(&self, device_id: &str, el: &Element) -> Result<()> {
        let deadline = Instant::now() + ROW_SWITCH_WAIT;
        let tree = loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let tree = self.read_tree(device_id).await?;
            let now = crate::hit::find_again(&tree, el).map(|n| n.checked);
            match now {
                // Gone (the row moved, the screen changed): nothing to judge.
                None => return Ok(()),
                Some(v) if v != el.checked => return Ok(()),
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                break tree;
            }
        };
        let Some((x, y)) = crate::hit::row_switch_leading_point(&tree, el) else {
            return Ok(());
        };
        debug!(
            element = ?el.label,
            x, y, "row switch unchanged after a trailing-end tap; tapping its leading end (RTL)"
        );
        self.act(device_id, move |c| async move {
            let wda = c.wda.lock().await;
            wda.client().tap_at(x as f64, y as f64).await
        })
        .await
    }

    /// Where to tap `element` (VEL-341): on the screen, off the keyboard,
    /// the bars and the status bar; an error naming what covers it when no
    /// part is free (as on Android). `VELOCITY_IOS_TAP_GUARD=off` taps the
    /// centre of the frame as before; `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off`
    /// drops only the status bar rule (D-4).
    async fn tap_point(&self, device_id: &str, element: &Element) -> Result<(i32, i32)> {
        if !tap_guard() {
            return Ok(element.bounds.center());
        }
        let tree = self.tree(device_id).await?;
        let size = (tree.bounds.width, tree.bounds.height);
        let bar = if self.status_bar_guard {
            let conn = self.ready(device_id).await?;
            let client = conn.wda.lock().await;
            client.client().status_bar_height(size).await
        } else {
            0
        };
        let point = crate::hit::tap_point(&tree, element, bar);
        if self.verify_tap {
            match point {
                Ok(p) => {
                    self.verify_tap_point(device_id, &tree, element, p).await?;
                    return Ok(p);
                }
                // The only free part is under the status bar. An app that
                // hides its bar draws there itself: the hit test says
                // whose the point is (D-38).
                Err(crate::hit::Blocker::Covered("status bar")) => {
                    if let Ok(p) = crate::hit::tap_point(&tree, element, 0) {
                        if let Ok(Some(crate::wda::HitOwner::App)) =
                            self.verify_tap_point(device_id, &tree, element, p).await
                        {
                            return Ok(p);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        point.map_err(|why| VelocityError::AssertionFailed {
            expected: format!("{} at {:?} reachable", element.element_type, element.bounds),
            // Not `pressKey: back`: on iOS that navigates away and leaves
            // the keyboard up.
            actual: match why {
                crate::hit::Blocker::Covered("keyboard") => format!(
                    "{why}; dismiss the keyboard first (e.g. pressKey: enter, or tap \
                     outside the field) or scroll the element above it, or set \
                     VELOCITY_IOS_TAP_GUARD=off"
                ),
                crate::hit::Blocker::Covered("status bar") => format!(
                    "{why}; scroll it into view first, or, if the app hides its status \
                     bar, set VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off"
                ),
                // A tap behind a presented layer lands on its dimming view
                // and dismisses it (review 09 #2).
                crate::hit::Blocker::Covered(
                    "sheet" | "popover" | "alert" | "sheet's dimming view",
                ) => format!(
                    "{why}, which is in front; act on it or dismiss it first, or set \
                     VELOCITY_IOS_TAP_GUARD=off"
                ),
                _ => format!("{why}; scroll it into view first, or set VELOCITY_IOS_TAP_GUARD=off"),
            },
            selector: element
                .label
                .clone()
                .unwrap_or_else(|| element.platform_id.clone()),
            screenshot: None,
        })
    }

    /// D-38: what the runner's hit test finds at `p` must be `element`,
    /// inside it or around it. `Ok(None)`: the runner can't say
    /// (WebDriverAgent, or the private API is missing), so the frame-only
    /// rule stands; `Ok(Some(owner))`: the tap reaches the target.
    async fn verify_tap_point(
        &self,
        device_id: &str,
        tree: &Element,
        element: &Element,
        p: (i32, i32),
    ) -> Result<Option<crate::wda::HitOwner>> {
        use crate::wda::HitOwner;
        let conn = self.ready(device_id).await?;
        let hit = conn
            .wda
            .lock()
            .await
            .client()
            .hit_test(p.0 as f64, p.1 as f64)
            .await;
        let hit = match hit {
            Ok(Some(h)) => h,
            Ok(None) => {
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    warn!(
                        "ios.verifyTap: the runner has no hit test here (it needs \
                         ios.runner: velocity, and XCTest's private hit test); taps \
                         are checked against the element tree only"
                    );
                }
                return Ok(None);
            }
            Err(e) => {
                debug!("hit test failed ({e}); checking the tap against the tree only");
                return Ok(None);
            }
        };
        let blocker = match (&hit.owner, &hit.element) {
            (HitOwner::SpringBoard, h) => Some(format!(
                "the point {p:?} belongs to the system ({}), not the app: the status \
                 bar or a system alert is on top",
                h.as_ref()
                    .map(|h| h.describe())
                    .unwrap_or_else(|| "SpringBoard".into())
            )),
            // WKWebView content is drawn by WebKit's own WebContent
            // process: its pid is not the app's, though the web page is
            // (DV-5, iPhone 17 Pro simulator). The tree checks still apply.
            (HitOwner::Other, _) if crate::hit::in_web_view(tree, element) => None,
            (HitOwner::Other, _) => Some(format!(
                "another process draws at {p:?} (a remote view or sheet is on top)"
            )),
            (HitOwner::App, deepest) => {
                crate::hit::hit_reaches(tree, element, hit.ax.as_ref(), deepest.as_ref())
                    .err()
                    .map(|on| {
                        format!(
                            "a tap at {p:?} would land on {} drawn over it",
                            on.describe()
                        )
                    })
            }
        };
        match blocker {
            None => Ok(Some(hit.owner)),
            Some(actual) => Err(VelocityError::AssertionFailed {
                expected: format!("{} at {:?} reachable", element.element_type, element.bounds),
                actual: format!("{actual} (ios.verifyTap)"),
                selector: element
                    .label
                    .clone()
                    .unwrap_or_else(|| element.platform_id.clone()),
                screenshot: None,
            }),
        }
    }

    fn is_hierarchy_element(element: &Element) -> bool {
        !element.bounds.is_empty() && (element.visible || element.platform_id.is_empty())
    }

    /// Hardware buttons WDA can press. Back and Enter aren't buttons on
    /// iOS (see `press_key`), so they have no mapping.
    fn key_to_button(key: Key) -> Option<&'static str> {
        match key {
            Key::Home => Some("home"),
            Key::VolumeUp => Some("volumeUp"),
            Key::VolumeDown => Some("volumeDown"),
            _ => None,
        }
    }

    /// Keys typed through WDA's keyboard (`/wda/keys`): Return, and the
    /// software keyboard's delete and tab (XCUIKeyboardKeyDelete is "\u{8}"
    /// there, the character the field clear already sends). Escape,
    /// forward Delete and the arrows exist only on a hardware keyboard,
    /// which WDA can't press: a config error, not a silent no-op (D-9).
    fn key_to_typed(key: &Key) -> Option<&'static str> {
        match key {
            Key::Enter => Some("\n"),
            Key::Backspace => Some("\u{8}"),
            Key::Tab => Some("\t"),
            _ => None,
        }
    }

    /// Match in the step's tree; fall back to a WDA query only for elements
    /// the tree can't show (beyond WDA's snapshot depth in deep trees).
    async fn find_via_hierarchy(
        &self,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Vec<Element>> {
        let tree = self.tree(device_id).await?;
        let screen = Rect {
            x: 0,
            y: 0,
            width: tree.bounds.width,
            height: tree.bounds.height,
        };
        let matches = find_matching_elements(&tree, selector, &screen);
        // An index or a relation is about what is on screen: no WDA query
        // can answer it. Asking WDA anyway made a missing relative target a
        // config error (VEL-154) and an index past the on-screen matches a
        // zero-size element that "passed" (VEL-155).
        if !matches.is_empty() || !wda_can_answer(selector) {
            return Ok(matches);
        }
        self.find_via_wda(device_id, selector).await
    }

    /// The element `selector` resolves to: the first match in TREE order
    /// for a bare selector (as Android, headless, RN and Flutter resolve it,
    /// and as the executor's ambiguity note says), reading order only for
    /// `index:`, nearest first for a relation. It used to be the first of
    /// [`Self::find_via_hierarchy`]'s reading-order list, so `tap: Go` hit a
    /// different twin on iOS than on Android (N-1).
    ///
    /// Local stand-in for a shared `selector_match::resolve` (R1): it is
    /// `find_ref`, which is that rule; switch to `resolve` once it exists.
    async fn find_one_via_hierarchy(
        &self,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Option<Element>> {
        let tree = self.tree(device_id).await?;
        let screen = Rect {
            x: 0,
            y: 0,
            width: tree.bounds.width,
            height: tree.bounds.height,
        };
        if let Some(el) =
            velocity_common::selector_match::find_in_tree(&tree, selector, Some(&screen))
        {
            return Ok(Some(prefer_in_front(&tree, selector, &screen, el)));
        }
        if !wda_can_answer(selector) {
            return Ok(None);
        }
        Ok(self
            .find_via_wda(device_id, selector)
            .await?
            .into_iter()
            .next())
    }

    fn find_via_wda<'a>(
        &'a self,
        device_id: &'a str,
        selector: &'a Selector,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Element>>> + Send + 'a>>
    {
        Box::pin(async move {
            if let Selector::Index {
                selector: inner,
                index,
            } = selector
            {
                let all = self.find_via_wda(device_id, inner).await?;
                return Ok(all.into_iter().nth(*index).into_iter().collect());
            }
            let (using, value) = Self::selector_to_wda(selector)?;
            let conn = self.ready(device_id).await?;
            let wda = conn.wda.lock().await;
            let client = wda.client();
            let mut elements = Vec::new();
            for we in client.find_elements(using, &value).await? {
                // An error is not "hidden" (VEL-336): the element would be
                // dropped and assertNotVisible pass on a broken read.
                let visible = client.is_displayed(&we.element_id).await?;
                if !visible {
                    continue;
                }
                let text = client.get_text(&we.element_id).await.ok();
                elements.push(Element {
                    hint: None,
                    platform_id: we.element_id,
                    label: text.clone(),
                    text,
                    element_type: "Unknown".to_string(),
                    bounds: Rect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                    enabled: true,
                    visible,
                    children: Vec::new(),
                    ..Default::default()
                });
            }
            Ok(elements)
        })
    }
}

#[async_trait::async_trait]
impl PlatformDriver for IosDriver {
    async fn prepare(&self, device_id: &str) -> Result<()> {
        self.prepare(device_id).await
    }

    async fn cleanup(&self) {
        self.cleanup().await;
    }

    async fn freeze_status_bar(&self, device_id: &str) -> bool {
        self.freeze_simulator_status_bar(device_id).await
    }

    async fn unfreeze_status_bar(&self) {
        self.clear_status_bar_overrides().await;
    }

    async fn health_check(&self) -> HealthStatus {
        let conns: Vec<Arc<DeviceConn>> = self.conns.lock().await.values().cloned().collect();
        if conns.is_empty() {
            return HealthStatus::Healthy;
        }
        for c in conns {
            if !c
                .wda
                .lock()
                .await
                .client()
                .health_check()
                .await
                .unwrap_or(false)
            {
                return HealthStatus::Unhealthy;
            }
        }
        HealthStatus::Healthy
    }

    async fn device_ready(&self, device_id: &str) -> bool {
        if is_physical(device_id) {
            return self
                .devicectl
                .list_devices()
                .await
                .iter()
                .any(|d| d.id == device_id && d.state == DeviceState::Booted);
        }
        let booted = self
            .simctl
            .list_devices()
            .await
            .map(|ds| {
                ds.iter()
                    .any(|d| d.id == device_id && d.state == DeviceState::Booted)
            })
            .unwrap_or(false);
        if !booted {
            return false;
        }
        match self.conns.lock().await.get(device_id).cloned() {
            Some(c) => c
                .wda
                .lock()
                .await
                .client()
                .health_check()
                .await
                .unwrap_or(false),
            None => true,
        }
    }

    async fn restart_session(&self) -> Result<()> {
        warn!("Restarting WDA sessions...");
        let conns: Vec<Arc<DeviceConn>> = self.conns.lock().await.values().cloned().collect();
        for c in conns {
            c.invalidate();
            c.wda.lock().await.invalidate_session();
            c.prepared.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Simulators and connected real devices.
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = self.simctl.list_devices().await?;
        devices.extend(self.devicectl.list_devices().await);
        Ok(devices)
    }

    async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = self.simctl.list_devices().await?;
        devices.extend(self.devicectl.try_list_devices().await?);
        Ok(devices)
    }

    async fn boot_device(&self, device_id: &str) -> Result<()> {
        if is_physical(device_id) {
            return Err(VelocityError::Config(format!(
                "{device_id} is a real device: connect and unlock it instead of booting"
            )));
        }
        self.simctl.boot(device_id).await
    }

    async fn shutdown_device(&self, device_id: &str) -> Result<()> {
        if is_physical(device_id) {
            return Ok(());
        }
        self.simctl.shutdown(device_id).await
    }

    async fn install_app(&self, device_id: &str, app_path: &str) -> Result<()> {
        if is_physical(device_id) {
            return self.devicectl.install(device_id, app_path).await;
        }
        self.simctl.install(device_id, app_path).await
    }

    async fn install_app_if_changed(
        &self,
        device_id: &str,
        app_path: &str,
        app_id: &str,
        hash: &str,
    ) -> Result<velocity_common::InstallOutcome> {
        crate::install::install_if_changed(
            &self.simctl,
            &self.devicectl,
            device_id,
            app_path,
            app_id,
            hash,
            is_physical(device_id),
        )
        .await
    }

    async fn reset_app(
        &self,
        device_id: &str,
        app_id: &str,
        reset: &ResetConfig,
    ) -> Result<ResetStrategy> {
        // Simulators have no snapshot API; reinstall is the strongest reset.
        let app = reset.app_path.as_deref().ok_or_else(|| {
            VelocityError::Config("reinstall needs reset.appPath (.app bundle)".into())
        })?;
        if is_physical(device_id) {
            let _ = self.devicectl.uninstall(device_id, app_id).await;
            self.devicectl.install(device_id, app).await?;
            return Ok(ResetStrategy::Reinstall);
        }
        let _ = self.simctl.terminate(device_id, app_id).await;
        let _ = self.simctl.uninstall(device_id, app_id).await;
        self.simctl.install(device_id, app).await?;
        Ok(ResetStrategy::Reinstall)
    }

    async fn launch_app(&self, device_id: &str, app_id: &str, clear_state: bool) -> Result<()> {
        self.launch_app_with(device_id, app_id, clear_state, &LaunchArgs::default())
            .await
    }

    /// `launchApp` with arguments: `-name value` launch arguments on every
    /// launch path (none: exactly the plain launch).
    async fn launch_app_with(
        &self,
        device_id: &str,
        app_id: &str,
        clear_state: bool,
        args: &LaunchArgs,
    ) -> Result<()> {
        let argv = launch_argv(args);
        self.launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (device_id.to_string(), app_id.to_string()),
                std::time::SystemTime::now(),
            );
        // A fresh process: an earlier crash is history.
        self.crash_watch.reset(device_id, app_id);
        if is_physical(device_id) {
            self.launch_on_device(device_id, app_id, clear_state, &argv)
                .await?;
            self.start_crash_watch(device_id, app_id).await;
            return Ok(());
        }
        self.check_installed(device_id, app_id).await?;
        let conn = self.running(device_id).await?;
        // Always a cold start, as on Android and in Maestro: WDA's launch
        // only brings a running app to the front, so tests inherited the
        // previous test's screen and state (launch counter stayed at 1).
        // Through the runner's session when there is one: `simctl
        // terminate` is an xcrun process and a CoreSimulator round trip on
        // every launch.
        //
        // The runner gets [`Self::terminate_timeout`], then simctl. The
        // simctl terminate is awaited before anything relaunches the app:
        // once it returns, the old process is gone whatever the runner
        // does. Trade-off: dropping the timed-out request doesn't recall
        // it. A wedged runner that already read it may still run its
        // terminate later, and if that lands after the relaunch it kills
        // the fresh app. Waiting out the runner (up to 30 s per launch) to
        // close that window costs more than the rare kill, which surfaces
        // as the app being gone at the next step.
        if !Self::terminate_in_session(&conn, app_id, self.terminate_timeout).await {
            if let Err(e) = self.simctl.terminate(device_id, app_id).await {
                debug!(device_id, app_id, "simctl terminate failed: {e}");
            }
        }
        if clear_state {
            self.clear_sim_data(device_id, app_id).await?;
        }

        let env = self.app_env(device_id);
        conn.invalidate();
        *conn.bundle.lock().unwrap_or_else(|e| e.into_inner()) = app_id.to_string();
        let mut wda = conn.wda.lock().await;
        wda.client_mut().set_app_env(env.clone());
        wda.client_mut().set_app_args(argv.clone());
        // A new session launches the app itself; launching it again through
        // simctl cost a second cold start.
        if wda.ensure_session(device_id, app_id).await? {
            return Ok(());
        }
        // Reused session: `simctl launch`, then wait for the app to be in
        // front. WDA's own launch waits for the app to go quiet as well:
        // 1.7 s to the home screen against 0.85 s (measured), and Velocity's
        // settle waits for the screen anyway.
        drop(wda);
        if launch_mode() == LaunchMode::Simctl {
            self.simctl
                .launch_with_env(device_id, app_id, &env, &argv)
                .await?;
            let wda = conn.wda.lock().await;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match wda.client().active_bundle().await {
                    Some(b) if b == app_id => return Ok(()),
                    // The runner can't tell: the settle that follows waits.
                    None => return Ok(()),
                    Some(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
            debug!(
                app_id,
                "app not in front 10 s after simctl launch; continuing"
            );
            return Ok(());
        }
        let wda = conn.wda.lock().await;
        match wda.client().launch_app(app_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                debug!("WDA launch failed ({e}); using simctl");
                drop(wda);
                self.simctl
                    .launch_with_env(device_id, app_id, &env, &argv)
                    .await
            }
        }
    }

    /// Whether the simulator app Velocity launched has died since, and why.
    /// The process table is the signal: the app's `UIKitApplication:` job is
    /// gone although Velocity didn't stop it (`stopApp` / a relaunch clear
    /// the record). The reason comes from the `.ips` crash report when macOS
    /// wrote one; it skips repeats of a recent identical crash, so it often
    /// doesn't. Works with WDA and VelocityRunner alike: with VelocityRunner
    /// a crash used to read "Element not found".
    /// The port the simulator's app was launched with in
    /// `VELOCITY_SYNC_PORT`; `None` before Velocity launched an app there
    /// (and on physical devices): the suite's `sync.nativePortIos`.
    fn sync_probe_port(&self, device_id: &str) -> Option<u16> {
        self.sync_ports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .copied()
    }

    /// Simulators: `simctl io recordVideo` and `log stream`. Real devices
    /// have no video or log capture yet.
    async fn start_evidence(
        &self,
        device_id: &str,
        req: &velocity_common::EvidenceRequest,
    ) -> Option<Box<dyn velocity_common::EvidenceSession>> {
        if is_physical(device_id) {
            return None;
        }
        crate::evidence::SimEvidence::start(self.simctl.xcrun_path(), device_id, req)
            .await
            .map(|e| Box::new(e) as Box<dyn velocity_common::EvidenceSession>)
    }

    async fn copy_crash_logs(
        &self,
        device_id: &str,
        app_id: &str,
        dest: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        let reports = if is_physical(device_id) {
            self.device_crash_reports(device_id, app_id).await
        } else {
            let since = self
                .launched
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(device_id.to_string(), app_id.to_string()))
                .copied();
            match (since, std::env::var_os("HOME")) {
                (Some(since), Some(home)) => {
                    let dir = std::path::PathBuf::from(home).join("Library/Logs/DiagnosticReports");
                    let app_id = app_id.to_string();
                    tokio::task::spawn_blocking(move || {
                        app_reports(&ips_files(&dir, 1), &app_id, since)
                    })
                    .await
                    .unwrap_or_default()
                }
                _ => Vec::new(),
            }
        };
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || copy_reports(&reports, &dest))
            .await
            .unwrap_or_default()
    }

    /// I3: what the device watcher or an error naming the crash recorded
    /// since the launch; no device round trip.
    async fn pending_crash(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id.to_string(), app_id.to_string()))?;
        self.crash_watch.get(device_id, app_id)
    }

    async fn crash_report(&self, device_id: &str, app_id: &str) -> Option<String> {
        if is_physical(device_id) {
            return self.device_app_gone(device_id, app_id).await;
        }
        let since = *self
            .launched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id.to_string(), app_id.to_string()))?;
        let jobs = self.simctl.launchctl_list(device_id).await.ok()?;
        if app_job_running(&jobs, app_id) {
            return None;
        }
        let dir = std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/Logs/DiagnosticReports");
        let app = app_id.to_string();
        let found = tokio::task::spawn_blocking(move || crash_in_reports(&dir, &app, since))
            .await
            .ok()
            .flatten();
        Some(found.unwrap_or_else(|| {
            "the app's process exited unexpectedly (no new crash report: macOS skips \
             repeats of a recent identical crash)"
                .to_string()
        }))
    }

    /// The crash check, then the front app from WDA (`/wda/activeAppInfo`),
    /// then the app's own state (`/wda/apps/state`), which VelocityRunner
    /// answers though it has no front-app lookup.
    async fn app_liveness(&self, device_id: &str, app_id: &str) -> velocity_common::Liveness {
        use velocity_common::Liveness;
        if let Some(detail) = self.crash_report(device_id, app_id).await {
            return Liveness::Dead(detail);
        }
        let Ok(conn) = self.ready(device_id).await else {
            return Liveness::Unknown;
        };
        let wda = conn.wda.lock().await;
        let front = wda.client().active_bundle().await;
        match front {
            Some(b) if b == app_id => Liveness::Foreground,
            Some(b) if !b.is_empty() => Liveness::Background(Some(b)),
            _ => match wda.client().app_state(app_id).await {
                Some(4) => Liveness::Foreground,
                Some(2 | 3) => Liveness::Background(None),
                _ => Liveness::Unknown,
            },
        }
    }

    /// The status bar (WDA's reported height, else the model's), in points.
    async fn system_bars(&self, device_id: &str) -> Option<Vec<Rect>> {
        let conn = self.ready(device_id).await.ok()?;
        let wda = conn.wda.lock().await;
        let (w, h) = wda.client().get_screen_size().await.ok()?;
        let bar = wda.client().status_bar_height((w, h)).await;
        Some(
            (bar > 0)
                .then_some(Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: bar,
                })
                .into_iter()
                .collect(),
        )
    }

    /// Open a deep link. The first time a custom scheme is opened iOS asks
    /// "Open in …?"; the dialog handler answers it like any system alert.
    async fn navigate(&self, device_id: &str, url: &str) -> Result<()> {
        if is_physical(device_id) {
            let conn = self.ready(device_id).await?;
            conn.invalidate();
            let opened = conn.wda.lock().await.client().open_url(url).await;
            return opened;
        }
        if let Some(c) = self.conns.lock().await.get(device_id) {
            c.invalidate();
        }
        self.simctl.openurl(device_id, url).await
    }

    /// The installed bundle's Info.plist modification time (a reinstall or
    /// rebuild changes it). Simulators only.
    async fn app_fingerprint(&self, device_id: &str, app_id: &str) -> Option<String> {
        // The build's hash, when Velocity installed it and the installed
        // copy is still that one (see `install.rs`).
        let physical = is_physical(device_id);
        if let Some(fp) =
            crate::install::fingerprint(&self.simctl, &self.devicectl, device_id, app_id, physical)
                .await
        {
            return Some(fp);
        }
        if physical {
            return None;
        }
        let bundle = self
            .simctl
            .get_app_container(device_id, app_id, "app")
            .await
            .ok()?;
        let meta = std::fs::metadata(std::path::Path::new(&bundle).join("Info.plist")).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(format!("{bundle}@{mtime}"))
    }

    /// Clone the app's data container (APFS `cp -c`: near-instant and no
    /// extra disk until files change). Simulators only: a real device's
    /// container isn't reachable from the host. The keychain isn't in the
    /// container; apps that keep login tokens there need a re-login.
    async fn save_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        if is_physical(device_id) {
            return Err(VelocityError::Config(
                "saveState needs a simulator: a real device's app data isn't reachable from the host"
                    .into(),
            ));
        }
        // Stopped on purpose (as by stopApp): not a crash.
        self.forget_launch(device_id, app_id);
        let _ = self.simctl.terminate(device_id, app_id).await;
        let container = self
            .simctl
            .get_app_container(device_id, app_id, "data")
            .await?;
        let dest = velocity_common::state_dir(device_id, app_id, name);
        let _ = tokio::fs::remove_dir_all(&dest).await;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VelocityError::Config(format!("{}: {e}", parent.display())))?;
        }
        clone_dir(std::path::Path::new(&container), &dest).await?;
        info!(device_id, app_id, name, "app state saved");
        Ok(())
    }

    async fn restore_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        if is_physical(device_id) {
            return Err(VelocityError::Config(
                "restoreState needs a simulator: a real device's app data isn't reachable from the host"
                    .into(),
            ));
        }
        let saved = velocity_common::state_dir(device_id, app_id, name);
        if !saved.exists() {
            return Err(VelocityError::Config(format!(
                "no saved state '{name}' for {app_id} on {device_id}; run a saveState step first"
            )));
        }
        self.forget_launch(device_id, app_id);
        let _ = self.simctl.terminate(device_id, app_id).await;
        if let Some(c) = self.conns.lock().await.get(device_id) {
            c.invalidate();
        }
        let container = self
            .simctl
            .get_app_container(device_id, app_id, "data")
            .await?;
        let container = std::path::PathBuf::from(container);
        // Empty the container, keeping it (its path and permissions belong
        // to the simulator), then clone the saved copy back in.
        let emptied = container.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for e in std::fs::read_dir(&emptied).into_iter().flatten().flatten() {
                let p = e.path();
                let _ = if p.is_dir() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
            }
        })
        .await;
        clone_dir(&saved, &container).await?;
        info!(device_id, app_id, name, "app state restored");
        Ok(())
    }

    async fn stop_app(&self, device_id: &str, app_id: &str) -> Result<()> {
        // Stopped on purpose: not a crash.
        self.forget_launch(device_id, app_id);
        if is_physical(device_id) {
            let conn = self.ready(device_id).await?;
            conn.invalidate();
            let stopped = conn.wda.lock().await.client().terminate_app(app_id).await;
            return stopped;
        }
        if let Some(c) = self.conns.lock().await.get(device_id) {
            c.invalidate();
        }
        self.simctl.terminate(device_id, app_id).await
    }

    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
        debug!(selector = %selector, "finding element");
        self.find_one_via_hierarchy(device_id, selector)
            .await?
            .ok_or_else(|| VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
    }

    async fn find_elements(&self, device_id: &str, selector: &Selector) -> Result<Vec<Element>> {
        debug!(selector = %selector, "finding elements");
        self.find_via_hierarchy(device_id, selector).await
    }

    /// A fresh read (the settle loop needs one every cycle); it also becomes
    /// the step's cached tree, so the lookup after a settle costs nothing.
    async fn get_hierarchy(&self, device_id: &str) -> Result<Element> {
        Ok((*self.read_tree(device_id).await?).clone())
    }

    fn has_hierarchy_cache(&self) -> bool {
        true
    }

    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        Ok((*self.tree(device_id).await?).clone())
    }

    async fn invalidate_hierarchy_cache(&self) {
        for c in self.conns.lock().await.values() {
            *c.tree.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    async fn tap(&self, device_id: &str, element: &Element) -> Result<()> {
        debug!(element_id = %element.platform_id, "tapping element");
        let el = element.clone();
        let point =
            if !el.platform_id.starts_with(ALERT_BUTTON_ID) && Self::is_hierarchy_element(&el) {
                Some(self.tap_point(device_id, &el).await?)
            } else {
                None
            };
        self.act(device_id, |c| {
            let el = el.clone();
            async move {
                let wda = c.wda.lock().await;
                if let Some(name) = el.platform_id.strip_prefix(ALERT_BUTTON_ID) {
                    wda.client().press_alert_button(name).await
                } else if let Some((cx, cy)) = point {
                    wda.client().tap_at(cx as f64, cy as f64).await
                } else {
                    wda.client().click(&el.platform_id).await
                }
            }
        })
        .await?;
        if point.is_some() && tap_guard() && crate::hit::is_row_switch(&el) && el.checked.is_some()
        {
            self.row_switch_fallback(device_id, &el).await?;
        }
        Ok(())
    }

    /// `tapXY` in points, the unit of iOS element bounds.
    async fn tap_xy(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        debug!(x, y, "tapping point");
        self.act(device_id, move |c| async move {
            let wda = c.wda.lock().await;
            wda.client().tap_at(x as f64, y as f64).await
        })
        .await
    }

    async fn double_tap(&self, device_id: &str, element: &Element) -> Result<()> {
        debug!(element_id = %element.platform_id, "double tapping element");
        let el = element.clone();
        let point = if Self::is_hierarchy_element(&el) {
            Some(self.tap_point(device_id, &el).await?)
        } else {
            None
        };
        self.act(device_id, |c| {
            let el = el.clone();
            async move {
                let wda = c.wda.lock().await;
                if let Some((cx, cy)) = point {
                    // One action sequence: no HTTP round trip between taps.
                    wda.client().taps_at(cx as f64, cy as f64, 2).await
                } else {
                    wda.client().click(&el.platform_id).await?;
                    wda.client().click(&el.platform_id).await
                }
            }
        })
        .await
    }

    async fn long_press(&self, device_id: &str, element: &Element, duration_ms: u64) -> Result<()> {
        debug!(element_id = %element.platform_id, duration_ms, "long pressing element");
        let (cx, cy) = if Self::is_hierarchy_element(element) {
            self.tap_point(device_id, element).await?
        } else {
            element.bounds.center()
        };
        let duration_s = duration_ms as f64 / 1000.0;
        self.act(device_id, |c| async move {
            c.wda
                .lock()
                .await
                .client()
                .swipe(cx as f64, cy as f64, cx as f64, cy as f64, duration_s, true)
                .await
        })
        .await
    }

    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        debug!(element_id = %element.platform_id, "inputting text");
        if is_picker_wheel(element) {
            return self.set_picker_wheel(device_id, element, text).await;
        }
        let conn = self.ready(device_id).await?;
        if !Self::is_hierarchy_element(element) {
            let id = element.platform_id.clone();
            let text = text.to_string();
            return self
                .act(device_id, |c| {
                    let (id, text) = (id.clone(), text.clone());
                    async move { c.wda.lock().await.client().send_keys(&id, &text).await }
                })
                .await;
        }
        // Don't tap a field the previous inputText already focused: the tap
        // and the keyboard wait cost ~0.5 s and can move the caret.
        let already_focused =
            *conn.focused.lock().unwrap_or_else(|e| e.into_inner()) == Some(element.bounds);
        // D-5: the tree the lookup read, to tell a field that takes the
        // focus with the tap from one that already had it.
        let before = conn.last_tree();
        let (cx, cy) = if already_focused {
            element.bounds.center()
        } else {
            self.tap_point(device_id, element).await?
        };
        let text = text.to_string();
        // Paste only into an empty field: next to text, iOS "smart insert"
        // adds a space that typing wouldn't.
        let empty = element.text.as_deref().is_none_or(str::is_empty);
        let primed = self.keys_primed(device_id, &conn);
        let paste = element.element_type != "SecureTextField" && use_paste(&text, empty, primed);
        // The tap and the typing are separate actions: a session lost or a
        // WDA restarted while typing replays the typing only, never the tap
        // (VEL-296: the field was tapped twice, after the screen may have
        // changed).
        let not_focusable = |actual: String| VelocityError::AssertionFailed {
            expected: format!(
                "{} at {:?} focused for typing",
                element.element_type, element.bounds
            ),
            actual,
            selector: element
                .label
                .clone()
                .unwrap_or_else(|| element.platform_id.clone()),
            screenshot: None,
        };
        if self.focus_check && !already_focused && !holds_text_input(element) {
            // VEL-S46: the keys went to whatever field had focus before.
            return Err(not_focusable(format!(
                "a {} is not a text field; select the field itself",
                element.element_type
            )));
        }
        // Until when typing that found nothing focused is tried again.
        let mut focus_wait = None;
        if !already_focused {
            self.act(device_id, |c| async move {
                c.wda
                    .lock()
                    .await
                    .client()
                    .tap_at(cx as f64, cy as f64)
                    .await
            })
            .await?;
            let tapped = Instant::now();
            if self.focus_check {
                // The keyboard animates in for 250-400 ms, but is in the
                // tree from its start: read at once, then poll for about a
                // second rather than reading once (review m11).
                let deadline = tapped + FOCUS_WAIT + Duration::from_millis(1000);
                loop {
                    let tree = self.read_tree(device_id).await?;
                    if has_type(&tree, "Keyboard") {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(not_focusable(
                            "no keyboard came up after the tap (the field did not take focus)"
                                .into(),
                        ));
                    }
                    tokio::time::sleep(FOCUS_POLL).await;
                }
            } else {
                focus_wait = self.wait_for_focus(tapped).await;
            }
            if self.strict_focus {
                self.verify_focus(device_id, &conn, element, before.as_deref(), &not_focusable)
                    .await?;
            }
        }
        let pasted = until_focused(focus_wait, || {
            self.act(device_id, |c| {
                let text = text.clone();
                async move {
                    let wda = c.wda.lock().await;
                    if paste {
                        match wda.client().paste_text(&text, !primed).await {
                            Ok(()) => return Ok(true),
                            Err(e) => debug!("paste failed, typing instead: {e}"),
                        }
                    }
                    wda.client().type_text(&text).await.map(|_| false)
                }
            })
        })
        .await?;
        if pasted {
            *conn.keys_primed.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::SystemTime::now());
        }
        *conn.focused.lock().unwrap_or_else(|e| e.into_inner()) = Some(element.bounds);
        Ok(())
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()> {
        debug!(element_id = %element.platform_id, "clearing text");
        if !Self::is_hierarchy_element(element) {
            let id = element.platform_id.clone();
            return self
                .act(device_id, |c| {
                    let id = id.clone();
                    async move { c.wda.lock().await.client().clear(&id).await }
                })
                .await;
        }
        // Focus, then one backspace per character in a single request
        // (the old triple tap cost 3 × ~490 ms and didn't select all in
        // every field type).
        let (cx, cy) = self.tap_point(device_id, element).await?;
        let len = element
            .text
            .as_deref()
            .map(|t| t.chars().count())
            .unwrap_or(0);
        // Text wider than the field puts the tap's caret mid-text, where
        // backspaces leave the end behind: select it all instead, and it's
        // one ~0.4 s call against ~9 ms a backspace for long text.
        let select = select_to_clear(len, element.bounds.width);
        let conn = self.ready(device_id).await?;
        let primed = self.keys_primed(device_id, &conn);
        // Tap and delete are separate actions, so a retried delete never
        // taps again (VEL-296).
        self.act(device_id, |c| async move {
            c.wda
                .lock()
                .await
                .client()
                .tap_at(cx as f64, cy as f64)
                .await
        })
        .await?;
        if len == 0 {
            return Ok(());
        }
        let focus_wait = self.wait_for_focus(Instant::now()).await;
        let selected = until_focused(focus_wait, || {
            self.act(device_id, |c| async move {
                let wda = c.wda.lock().await;
                if select {
                    match wda.client().select_all(!primed).await {
                        Ok(()) => return wda.client().type_text("\u{8}").await.map(|_| true),
                        Err(e) => debug!("select all failed, deleting by key: {e}"),
                    }
                }
                wda.client()
                    .type_text(&"\u{8}".repeat(len))
                    .await
                    .map(|_| false)
            })
        })
        .await?;
        if selected {
            *conn.keys_primed.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::SystemTime::now());
        }
        Ok(())
    }

    /// Type into whatever has focus (Maestro's `inputText`).
    async fn input_raw(&self, device_id: &str, text: &str) -> Result<()> {
        let text = text.to_string();
        self.act(device_id, |c| {
            let text = text.clone();
            async move { c.wda.lock().await.client().type_text(&text).await }
        })
        .await
    }

    async fn swipe(&self, device_id: &str, direction: Direction) -> Result<()> {
        self.drag_direction(device_id, direction, true).await
    }

    async fn swipe_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.drag_line(device_id, from, to, true).await
    }

    async fn drag(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        self.act(device_id, |c| async move {
            c.wda
                .lock()
                .await
                .client()
                .drag_hold(
                    (from.0 as f64, from.1 as f64),
                    (to.0 as f64, to.1 as f64),
                    hold_ms,
                    move_ms,
                )
                .await
        })
        .await
    }

    async fn scroll(&self, device_id: &str, finger: Direction) -> Result<()> {
        self.drag_direction(device_id, finger, false).await
    }

    async fn scroll_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        self.drag_line(device_id, from, to, false).await
    }

    async fn press_key(&self, device_id: &str, key: Key) -> Result<()> {
        if !key.supported_on(velocity_common::Platform::Ios) {
            return Err(VelocityError::Config(format!(
                "pressKey: {} can't be sent on iOS (WebDriverAgent types only back, home, enter, \
                 volumeUp, volumeDown, backspace and tab)",
                key.name()
            )));
        }
        // iOS has no back button. "Back" means what a user would do: tap the
        // navigation bar's back button, or swipe in from the left edge.
        // (Pressing Home backgrounded the app.) The button comes from the
        // tree: WDA's "first button of a navigation bar" was whatever button
        // came first, a right-hand "Delete" included (VEL-344).
        let back = if key == Key::Back {
            let tree = self.tree(device_id).await?;
            // No navigation back button: a sheet's own way out (Close,
            // Cancel, ✕) near the top. The edge swipe pops a pushed screen
            // but never closes a sheet, and "back" in a sheet did nothing
            // (iOSWorld, 2026-11-08).
            match back_button(&tree).or_else(|| sheet_close_button(&tree)) {
                Some(b) => Some(self.tap_point(device_id, &b).await?),
                None => None,
            }
        } else {
            None
        };
        self.act(device_id, |c| {
            let key = key.clone();
            async move {
                let wda = c.wda.lock().await;
                let client = wda.client();
                match key {
                    Key::Back => match back {
                        Some((x, y)) => client.tap_at(x as f64, y as f64).await,
                        None => {
                            let (w, h) = client.get_screen_size().await?;
                            let y = h as f64 / 2.0;
                            client.swipe(2.0, y, w as f64 * 0.7, y, 0.25, true).await
                        }
                    },
                    // Return, delete and tab are keyboard keys, not
                    // hardware buttons.
                    Key::Enter | Key::Backspace | Key::Tab => {
                        client
                            .type_text(Self::key_to_typed(&key).unwrap_or_default())
                            .await
                    }
                    other => {
                        let button = Self::key_to_button(other.clone()).ok_or_else(|| {
                            VelocityError::Config(format!("key {other:?} has no iOS button"))
                        })?;
                        client.press_button(button).await
                    }
                }
            }
        })
        .await
    }

    async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        debug!(device_id, "taking screenshot");
        // WDA's screenshot takes ~66 ms; `simctl io` spawns a process.
        if let Ok(conn) = self.ready(device_id).await {
            if let Ok(png) = conn.wda.lock().await.client().screenshot().await {
                return Ok(png);
            }
        }
        if is_physical(device_id) {
            return Err(VelocityError::WdaUnreachable {
                device_id: device_id.to_string(),
                url: device_id.to_string(),
                detail: "screenshot: WebDriverAgent did not answer".into(),
            });
        }
        self.simctl.screenshot(device_id).await
    }

    async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)> {
        let conn = self.ready(device_id).await?;
        let size = conn.wda.lock().await.client().get_screen_size().await;
        size
    }

    async fn get_element_text(&self, device_id: &str, element: &Element) -> Result<String> {
        debug!(element_id = %element.platform_id, "getting element text");
        if Self::is_hierarchy_element(element) {
            // Read the live screen, not the copy found before the last action.
            let tree = self.tree(device_id).await?;
            let found = locate(&tree, element).ok_or_else(|| VelocityError::ElementNotFound {
                selector: element.platform_id.clone(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })?;
            // A field's text is its value, empty or not: its label ("Email")
            // is not what it holds. Android and headless read the text alone;
            // here a cleared field read as its label (VEL-354).
            if is_text_input(&found.element_type) {
                return Ok(found.text.clone().unwrap_or_default());
            }
            return Ok(found
                .text
                .clone()
                .or_else(|| found.label.clone())
                .unwrap_or_default());
        }
        let conn = self.ready(device_id).await?;
        let text = conn
            .wda
            .lock()
            .await
            .client()
            .get_text(&element.platform_id)
            .await;
        text
    }

    async fn is_element_visible(&self, device_id: &str, element: &Element) -> Result<bool> {
        debug!(element_id = %element.platform_id, "checking element visibility");
        if Self::is_hierarchy_element(element) {
            let tree = self.tree(device_id).await?;
            let screen = tree.bounds;
            // The shared rule (D-1), `visibility.minArea` included.
            return Ok(locate(&tree, element)
                .is_some_and(|e| velocity_common::selector_match::shown_on(e, &screen)));
        }
        let conn = self.ready(device_id).await?;
        let shown = conn
            .wda
            .lock()
            .await
            .client()
            .is_displayed(&element.platform_id)
            .await;
        shown
    }
}

/// Copy the contents of `from` into `to` (created if needed) as APFS
/// clones, falling back to a plain copy on other file systems.
async fn clone_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .map_err(|e| VelocityError::Config(format!("{}: {e}", to.display())))?;
    let src = format!("{}/.", from.display());
    for clone in [true, false] {
        let mut cmd = tokio::process::Command::new("cp");
        if clone {
            cmd.arg("-c");
        }
        cmd.arg("-R").arg(&src).arg(to);
        let out = velocity_common::process::output_with_timeout(
            &mut cmd,
            velocity_common::process::LONG_TIMEOUT,
        )
        .await
        .map_err(|e| VelocityError::Config(format!("cp: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        if !clone {
            return Err(VelocityError::Config(format!(
                "copying app data failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Whether `e` is a text input or holds one.
fn holds_text_input(e: &Element) -> bool {
    is_text_input(&e.element_type) || e.children.iter().any(holds_text_input)
}

fn has_type(e: &Element, element_type: &str) -> bool {
    e.element_type == element_type || e.children.iter().any(|c| has_type(c, element_type))
}

/// Types whose value is typed text.
fn is_text_input(element_type: &str) -> bool {
    matches!(
        element_type,
        "TextField" | "SecureTextField" | "SearchField" | "TextView"
    )
}

/// Whether `udid` is a real device's: the shapes Apple gives them, 40 hex
/// digits (before the iPhone XS) or 8-16 hex digits split by a dash. It was
/// "anything but a dashed UUID", so `booted`, a simulator's name or a
/// dash-less simulator UUID went down the devicectl path (VEL-284).
fn is_physical(udid: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
    match udid.split_once('-') {
        None => hex(udid, 40),
        Some((a, b)) => hex(a, 8) && hex(b, 16),
    }
}

/// A sheet's close button: a button in the top quarter of the screen whose
/// label, text or identifier is exactly one of the standard ways out
/// ([`is_close_name`]), never Done or Save (those commit), the topmost one,
/// the last in the tree when two are level. Prefix and suffix forms are not
/// matched: "Close account" or "Close all tabs" near the top of a sheet is an
/// app action, not a way out (review 09 #3). A button behind a presented
/// layer is skipped: the tap would land on the layer's dimming view.
fn sheet_close_button(tree: &Element) -> Option<Element> {
    fn walk<'a>(e: &'a Element, top: i32, out: &mut Vec<&'a Element>) {
        if e.element_type == "Button" && e.visible && !e.bounds.is_empty() && e.bounds.y < top {
            let names = [
                e.label.as_deref(),
                e.text.as_deref(),
                Some(e.platform_id.as_str()),
            ];
            if names.into_iter().flatten().any(is_close_name) {
                out.push(e);
            }
        }
        for c in &e.children {
            walk(c, top, out);
        }
    }
    let mut found = Vec::new();
    walk(tree, tree.bounds.y + tree.bounds.height / 4, &mut found);
    found
        .into_iter()
        .rev()
        .filter(|e| crate::hit::behind_modal(tree, e).is_none())
        .min_by_key(|e| e.bounds.y)
        .cloned()
}

/// A standard name for a sheet's way out: Close, Cancel, Dismiss, ✕, the
/// `xmark` SF Symbols (`xmark`, `xmark.circle.fill`, …) and the usual
/// close-button identifiers, compared whole and ignoring case.
fn is_close_name(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    matches!(
        s.as_str(),
        "close" | "cancel" | "dismiss" | "xmark" | "✕" | "close_button" | "closebutton"
    ) || (s.starts_with("xmark.") && !s.contains(' '))
}

/// The back button of the front-most navigation bar on screen: a button
/// whose label or identifier is "Back", or the identifier UIKit and SwiftUI
/// give the system back button ("BackButton"; its label is the previous
/// screen's title). Nothing else: a positional "left-most button of the
/// bar" was a root screen's Edit, Menu or avatar button (review 09 #3).
/// `None` when the bar has none (a root screen): `pressKey: back` then
/// swipes in from the edge, which does nothing on a root screen. A button
/// behind a presented layer (a sheet over the screen) doesn't count.
fn back_button(tree: &Element) -> Option<Element> {
    fn bars<'a>(e: &'a Element, screen: &Rect, out: &mut Vec<&'a Element>) {
        if e.element_type == "NavigationBar" && !e.bounds.is_empty() && e.bounds.intersects(screen)
        {
            out.push(e);
        }
        for c in &e.children {
            bars(c, screen, out);
        }
    }
    fn buttons<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
        if e.element_type == "Button" && !e.bounds.is_empty() {
            out.push(e);
        }
        for c in &e.children {
            buttons(c, out);
        }
    }
    let mut found = Vec::new();
    bars(tree, &tree.bounds, &mut found);
    // Later in the tree is in front (a pushed screen's bar).
    let bar = *found.last()?;
    let mut candidates = Vec::new();
    buttons(bar, &mut candidates);
    let is_back = |b: &&Element| {
        b.label
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("back"))
            || b.platform_id.eq_ignore_ascii_case("back")
            || b.platform_id == "BackButton"
    };
    candidates
        .into_iter()
        .find(|b| is_back(b) && crate::hit::behind_modal(tree, b).is_none())
        .cloned()
}

/// An NSPredicate string literal: `"…"` with `\\` and `"` escaped.
fn ns_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `Button` → `XCUIElementTypeButton`; full names are kept.
fn xcui_type(class: &str) -> String {
    if class.starts_with("XCUIElementType") {
        class.to_string()
    } else {
        format!("XCUIElementType{class}")
    }
}

/// `first`, the bare pick for `selector`, unless it is behind a presented
/// layer (a sheet, popover or alert) and another match is not: then the
/// first such match in tree order. The presenting screen stays in the tree
/// under a sheet, so `tapOn: Save` picked the "Save" behind an action sheet
/// and the tap dismissed it (review 09 #2). An `index:` or a relation keeps
/// its own pick, and with nothing in front the pick stands (the tap guard
/// then refuses it, naming the layer).
fn prefer_in_front(tree: &Element, selector: &Selector, screen: &Rect, first: Element) -> Element {
    if matches!(selector, Selector::Index { .. } | Selector::Relative { .. })
        || crate::hit::behind_modal(tree, &first).is_none()
    {
        return first;
    }
    velocity_common::selector_match::find_all_tree_order(tree, selector, Some(screen))
        .into_iter()
        .find(|e| crate::hit::behind_modal(tree, e).is_none())
        .cloned()
        .unwrap_or(first)
}

/// Whether a WDA query means the same as `selector` (no index, no relation).
fn wda_can_answer(selector: &Selector) -> bool {
    match selector {
        Selector::Index { .. }
        | Selector::Relative { .. }
        | Selector::TextMatches(_)
        | Selector::IdMatches(_)
        | Selector::Role(_)
        | Selector::Name(_)
        | Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => false,
        Selector::Compound(parts) => parts.iter().all(wda_can_answer),
        _ => true,
    }
}

/// Search the parsed Element tree for all elements matching a Selector.
/// Matching is shared with every other platform (velocity_common::selector_match).
fn find_matching_elements(root: &Element, selector: &Selector, screen: &Rect) -> Vec<Element> {
    match selector {
        Selector::Index { .. } => {
            velocity_common::selector_match::find_in_tree(root, selector, Some(screen))
                .into_iter()
                .collect()
        }
        _ => {
            let mut out = Vec::new();
            velocity_common::selector_match::find_all_in_tree(
                root,
                selector,
                Some(screen),
                &mut out,
            );
            out
        }
    }
}

/// Press before a swipe's drag, in seconds. WDA's `duration` is only a
/// wait before the drag (the drag speed is XCTest's own), so the old 0.3 s
/// was dead time on every swipe and scroll; VelocityRunner already uses
/// 0.05 s. Measured by hand: 1.64 s → 1.38 s a drag, same scroll.
const DRAG_PRESS_S: f64 = 0.05;

/// How long a "no system alert" answer is trusted.
fn alert_recheck() -> Duration {
    velocity_common::settings::ms("VELOCITY_IOS_ALERT_RECHECK_MS", 1000)
}

/// `VELOCITY_IOS_TAP_GUARD=off` taps the centre of the frame, as before the
/// tap-point resolver ([`crate::hit`]).
/// `VELOCITY_IOS_SHEET_VISIBILITY=off`: don't read `visible` under a sheet.
fn sheet_visibility() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_IOS_SHEET_VISIBILITY").as_deref(),
        Ok("off") | Ok("0") | Ok("false")
    )
}

/// How long a row switch's value may take to show a flip before the other
/// end is tapped ([`IosDriver::row_switch_fallback`]).
const ROW_SWITCH_WAIT: Duration = Duration::from_millis(600);

fn tap_guard() -> bool {
    velocity_common::env::var("VELOCITY_IOS_TAP_GUARD")
        .as_deref()
        .map(str::trim)
        != Ok("off")
}

/// `VELOCITY_IOS_SYSTEM_ALERTS=0` skips the system-alert check on each read.
fn system_alerts_enabled() -> bool {
    velocity_common::env::var("VELOCITY_IOS_SYSTEM_ALERTS").as_deref() != Ok("0")
}

/// How a simulator relaunches an app in a reused session
/// (`VELOCITY_IOS_LAUNCH=simctl|wda`, default simctl).
#[derive(Debug, Clone, Copy, PartialEq)]
enum LaunchMode {
    Simctl,
    Wda,
}

fn launch_mode() -> LaunchMode {
    match velocity_common::env::var("VELOCITY_IOS_LAUNCH").as_deref() {
        Ok("wda") => LaunchMode::Wda,
        _ => LaunchMode::Simctl,
    }
}

/// `launchctl list` (in the simulator) has a job for `bundle_id`'s app:
/// `<pid> <status> UIKitApplication:<bundle>[…]`.
fn app_job_running(jobs: &str, bundle_id: &str) -> bool {
    let needle = format!("UIKitApplication:{bundle_id}[");
    jobs.lines()
        .any(|l| l.contains(&needle) && l.split_whitespace().next().is_some_and(|pid| pid != "-"))
}

/// The `.ips` files under `dir`, `depth` levels deep (a copied crash log
/// domain has `Retired/` and per-process folders).
fn ips_files(dir: &std::path::Path, depth: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() && depth > 1 {
            out.extend(ips_files(&p, depth - 1));
        } else if kind.is_file() && p.extension().and_then(|x| x.to_str()) == Some("ips") {
            out.push(p);
        }
    }
    out
}

/// When a report says the crash happened: its header's `timestamp`
/// (`2026-10-15 10:20:31.00 +0000`), else the file's mtime (a copy off a
/// device may not keep it).
fn report_time(head: &serde_json::Value, path: &std::path::Path) -> Option<std::time::SystemTime> {
    head["timestamp"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_str(t.trim(), "%Y-%m-%d %H:%M:%S%.f %z").ok())
        .map(std::time::SystemTime::from)
        .or_else(|| std::fs::metadata(path).and_then(|m| m.modified()).ok())
}

/// `bundle_id`'s reports among `files` from `since` on, newest first, with
/// their one-line summary.
fn app_reports(
    files: &[std::path::PathBuf],
    bundle_id: &str,
    since: std::time::SystemTime,
) -> Vec<(std::time::SystemTime, std::path::PathBuf, String)> {
    let mut out: Vec<_> = files
        .iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            let head: serde_json::Value = serde_json::from_str(text.lines().next()?).ok()?;
            let t = report_time(&head, p)?;
            if t < since {
                return None;
            }
            Some((t, p.clone(), summarize_ips(&text, bundle_id)?))
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.0));
    out
}

/// Copy `reports` into `dest` (created when there is one); the files
/// written.
fn copy_reports(
    reports: &[(std::time::SystemTime, std::path::PathBuf, String)],
    dest: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    if reports.is_empty() || std::fs::create_dir_all(dest).is_err() {
        return Vec::new();
    }
    reports
        .iter()
        .filter_map(|(_, p, _)| {
            let to = dest.join(p.file_name()?);
            std::fs::copy(p, &to).ok().map(|_| to)
        })
        .collect()
}

/// The newest `.ips` crash report in `dir` for `bundle_id` written after
/// `since`, summarised: "EXC_CRASH (SIGABRT): Abort trap: 6, in <frame>".
fn crash_in_reports(
    dir: &std::path::Path,
    bundle_id: &str,
    since: std::time::SystemTime,
) -> Option<String> {
    // Reports written since the launch, newest first; only this app's count.
    let mut recent: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("ips"))
        .filter_map(|e| {
            let t = e.metadata().and_then(|m| m.modified()).ok()?;
            (t >= since).then(|| (t, e.path()))
        })
        .collect();
    recent.sort_by_key(|r| std::cmp::Reverse(r.0));
    recent
        .into_iter()
        .find_map(|(_, p)| summarize_ips(&std::fs::read_to_string(&p).ok()?, bundle_id))
}

/// One `.ips` report (a JSON header line, then the JSON body) for
/// `bundle_id`, as a one-line reason; `None` for another app's report.
fn summarize_ips(text: &str, bundle_id: &str) -> Option<String> {
    let (head, body) = text.split_once('\n')?;
    let head: serde_json::Value = serde_json::from_str(head).ok()?;
    if head["bundleID"].as_str() != Some(bundle_id) {
        return None;
    }
    let body: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let ex = &body["exception"];
    let kind = ex["type"].as_str().unwrap_or("crash");
    let signal = ex["signal"]
        .as_str()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let why = body["termination"]["indicator"].as_str().unwrap_or("");
    // The first frame in the app's own image (not libsystem/libswift).
    let thread = body["faultingThread"].as_u64().unwrap_or(0) as usize;
    let app_image = body["usedImages"].as_array().and_then(|imgs| {
        imgs.iter()
            .position(|i| i["name"].as_str() == head["name"].as_str())
    });
    let frame = body["threads"][thread]["frames"]
        .as_array()
        .and_then(|frames| {
            frames
                .iter()
                .find(|f| {
                    app_image.is_some() && f["imageIndex"].as_u64().map(|i| i as usize) == app_image
                })
                .and_then(|f| f["symbol"].as_str())
        });
    Some(format!(
        "{kind}{signal}{}{}",
        if why.is_empty() {
            String::new()
        } else {
            format!(": {why}")
        },
        frame.map(|f| format!(", in {f}")).unwrap_or_default()
    ))
}

/// `platform_id` prefix of a system-alert button; the rest is its label.
const ALERT_BUTTON_ID: &str = "velocity-system-alert-button:";

/// A SpringBoard alert as a tree node, so selectors and the dialog handler
/// see it like an in-app one. Its buttons have no real frames (WDA gives only
/// labels): they get distinct slots across the middle of the screen, and a
/// tap on one presses it by label.
fn system_alert(screen: &Rect, text: &str, buttons: &[String]) -> Element {
    let row = 44;
    let top = screen.y + screen.height / 2 - row * buttons.len() as i32 / 2;
    let children = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| Element {
            hint: None,
            platform_id: format!("{ALERT_BUTTON_ID}{b}"),
            label: Some(b.clone()),
            text: Some(b.clone()),
            element_type: "XCUIElementTypeButton".into(),
            bounds: Rect {
                x: screen.x + 40,
                y: top + row * i as i32,
                width: (screen.width - 80).max(1),
                height: row,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        })
        .chain(std::iter::once(Element {
            hint: None,
            platform_id: String::new(),
            label: Some(text.to_string()),
            text: Some(text.to_string()),
            element_type: "XCUIElementTypeStaticText".into(),
            bounds: Rect {
                x: screen.x + 40,
                y: top - row,
                width: (screen.width - 80).max(1),
                height: row,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }))
        .collect();
    Element {
        hint: None,
        platform_id: String::new(),
        label: Some(text.to_string()),
        text: None,
        element_type: "XCUIElementTypeAlert".into(),
        bounds: *screen,
        enabled: true,
        visible: true,
        children,
        ..Default::default()
    }
}

/// `launchApp` arguments as iOS launch arguments: `-name value` pairs, the
/// form Xcode scheme arguments take and `UserDefaults` reads (booleans as
/// YES/NO). Argv items for simctl, devicectl and WDA, never a shell line.
fn launch_argv(args: &LaunchArgs) -> Vec<String> {
    let mut out = Vec::new();
    for (name, value) in &args.inner {
        let text = match value {
            LaunchArg::Bool(true) => "YES".to_string(),
            LaunchArg::Bool(false) => "NO".to_string(),
            LaunchArg::Int(n) => n.to_string(),
            LaunchArg::Float(f) => f.to_string(),
            LaunchArg::Str(t) => t.clone(),
        };
        out.extend([format!("-{name}"), text]);
    }
    out
}
#[cfg(test)]
mod device_kind_tests {
    use super::is_physical;

    #[test]
    fn tells_real_devices_from_simulators_by_udid() {
        assert!(!is_physical("290403AA-C40F-404B-9CB0-DAFB6A943142"));
        assert!(is_physical("00008130-001451402EC1001C"));
        assert!(is_physical("abcdef0123456789abcdef0123456789abcdef01"));
        // VEL-284: none of these is a device.
        for sim in [
            "booted",
            "iPhone 17 Pro",
            "290403AAC40F404B9CB0DAFB6A943142",
            "",
            "velocity-iPhone-17-Pro-1",
        ] {
            assert!(!is_physical(sim), "{sim}");
        }
    }
}

/// How long after a focusing tap the field gets to take focus before the
/// first key, and how often that is checked.
const FOCUS_WAIT: Duration = Duration::from_millis(100);
const FOCUS_POLL: Duration = Duration::from_millis(40);

impl IosDriver {
    /// After a focusing tap at `tapped`: when the runner checks focus
    /// itself, the deadline for [`until_focused`]'s retries; otherwise
    /// waits out [`FOCUS_WAIT`], since there is no cheaper signal than a
    /// whole `/source` read.
    async fn wait_for_focus(&self, tapped: Instant) -> Option<Instant> {
        if self.typing_checks_focus {
            return Some(tapped + FOCUS_WAIT);
        }
        tokio::time::sleep_until((tapped + FOCUS_WAIT).into()).await;
        None
    }
}

/// Run `op` (typing), again every [`FOCUS_POLL`] while the runner answers
/// that nothing has keyboard focus and `deadline` hasn't passed; the last
/// attempt's answer is the result, as after a fixed wait.
async fn until_focused<T, F, Fut>(deadline: Option<Instant>, op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    loop {
        match (op().await, deadline) {
            (Err(e), Some(d)) if is_unfocused(&e) && Instant::now() < d => {
                tokio::time::sleep_until(d.min(Instant::now() + FOCUS_POLL).into()).await;
            }
            (other, _) => return other,
        }
    }
}

/// The runner's "nothing has keyboard focus" (see `WdaClient`'s error
/// mapping): no key was sent.
fn is_unfocused(e: &VelocityError) -> bool {
    matches!(e, VelocityError::AssertionFailed { actual, .. } if actual.contains("has keyboard focus"))
}

/// Whether `inputText` pastes `text` (`ios.text`): `paste` always, `keys`
/// never, `auto` (default) into an empty field when it's quicker: a paste
/// costs ~0.4 s (~0.75 s for the first after a launch, which primes the
/// keyboard) against ~9 ms a typed character. Text with a newline or
/// backspace is always typed: those are key presses (submit, delete).
/// So is a secret (a registered `--secret`/secret-named value, or any text
/// for a secure field), whatever `ios.text` says: the simulator's
/// pasteboard is the Mac's, and clipboard managers keep what passes
/// through it (security review #9).
fn use_paste(text: &str, field_empty: bool, primed: bool) -> bool {
    if text.contains(['\n', '\r', '\u{8}']) {
        return false;
    }
    if velocity_common::redact::redact(text) != text {
        return false;
    }
    match velocity_common::env::var("VELOCITY_IOS_TEXT")
        .unwrap_or_default()
        .as_str()
    {
        "paste" => true,
        "keys" => false,
        _ => {
            let from = if primed { 50 } else { 100 };
            field_empty && text.chars().count() >= from
        }
    }
}

/// Whether `clearText` selects all rather than backspacing: long text, or
/// text likely wider than the field (~8 pt a character).
fn select_to_clear(len: usize, field_width: i32) -> bool {
    len >= 40 || (len as i32) * 8 > field_width.max(1)
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn auto_pastes_long_text_into_empty_fields_only() {
        let long = "x".repeat(60);
        assert!(use_paste(&long, true, true));
        assert!(!use_paste(&long, true, false), "unprimed needs 100+");
        assert!(use_paste(&"x".repeat(120), true, false));
        assert!(!use_paste(&long, false, true));
        assert!(!use_paste("short", true, true));
        assert!(!use_paste(&format!("{long}\n"), true, true));
        // A registered secret is typed, never put on the pasteboard.
        let secret = format!("pw-{}", "s".repeat(60));
        velocity_common::redact::register_secret(&secret);
        assert!(!use_paste(&secret, true, true));
        assert!(select_to_clear(45, 370) && select_to_clear(30, 200));
        assert!(!select_to_clear(5, 370) && !select_to_clear(0, 370));
    }
}

#[cfg(test)]
mod wda_query_tests {
    use super::*;

    #[test]
    fn predicate_literals_escape_quotes_and_backslashes() {
        // VEL-147: `"` in a selector ended the literal and rewrote the query.
        assert_eq!(ns_literal(r#"a"b\c"#), r#""a\"b\\c""#);
        let (using, q) =
            IosDriver::selector_to_wda(&Selector::TextContains(r#"x" OR label != "y"#.into()))
                .unwrap();
        assert_eq!(using, "predicate string");
        assert_eq!(q, r#"label CONTAINS[c] "x\" OR label != \"y""#);
        // Backticks need no escaping outside a class chain.
        let (_, q) = IosDriver::selector_to_wda(&Selector::Compound(vec![
            Selector::Text("`a`".into()),
            Selector::ClassName("Button".into()),
        ]))
        .unwrap();
        assert_eq!(q, r#"label == "`a`" AND type == "XCUIElementTypeButton""#);
    }

    #[test]
    fn a_text_query_also_matches_a_field_value() {
        // Review nit 2: the tree matcher matches values, WDA didn't.
        let (_, q) = IosDriver::selector_to_wda(&Selector::Text("Ada".into())).unwrap();
        assert_eq!(q, r#"label == "Ada" OR name == "Ada" OR value == "Ada""#);
    }

    #[test]
    fn text_queries_are_normalised_like_the_tree_matcher() {
        // D-10: the selector side is normalised for WDA too.
        let (_, q) =
            IosDriver::selector_to_wda(&Selector::Text(" Sign\n in\u{200B} ".into())).unwrap();
        assert!(
            q.starts_with("label == \"Sign in\" OR label == \" Sign\n in"),
            "{q}"
        );
    }

    #[test]
    fn wda_queries_also_match_a_label_as_written() {
        // Review MINOR 11: only the query was normalised, so a label that
        // really holds a newline or NBSP no longer matched through WDA.
        let (_, q) = IosDriver::selector_to_wda(&Selector::Text("Line 1\nLine 2".into())).unwrap();
        assert!(q.contains(r#"label == "Line 1 Line 2""#), "{q}");
        assert!(q.contains("label == \"Line 1\nLine 2\""), "{q}");
        let (_, q) =
            IosDriver::selector_to_wda(&Selector::TextContains("a\u{a0}b".into())).unwrap();
        assert!(q.contains("label CONTAINS[c] \"a b\""), "{q}");
        assert!(q.contains("label CONTAINS[c] \"a\u{a0}b\""), "{q}");
        // Already normal: one form, as before.
        let (_, q) = IosDriver::selector_to_wda(&Selector::Text("Ada".into())).unwrap();
        assert_eq!(q, r#"label == "Ada" OR name == "Ada" OR value == "Ada""#);
        let (_, q) = IosDriver::selector_to_wda(&Selector::Compound(vec![
            Selector::Text("a  b".into()),
            Selector::ClassName("Button".into()),
        ]))
        .unwrap();
        assert!(
            q.starts_with(r#"(label == "a b" OR label == "a  b") AND type"#),
            "{q}"
        );
    }

    #[test]
    fn short_class_names_get_the_xcui_prefix() {
        // VEL-308: `className: Button` never matched through WDA.
        let (using, q) = IosDriver::selector_to_wda(&Selector::ClassName("Button".into())).unwrap();
        assert_eq!((using, q.as_str()), ("class name", "XCUIElementTypeButton"));
        let (_, q) =
            IosDriver::selector_to_wda(&Selector::ClassName("XCUIElementTypeCell".into())).unwrap();
        assert_eq!(q, "XCUIElementTypeCell");
    }

    fn node(ty: &str, label: &str, x: i32, y: i32, w: i32, h: i32) -> Element {
        Element {
            hint: None,
            platform_id: label.into(),
            label: Some(label.into()),
            text: None,
            element_type: ty.into(),
            bounds: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn back_is_the_front_bars_back_button_never_a_positional_guess() {
        // VEL-344: a root screen's bar with only "Delete" on the right.
        let mut root = node("Application", "app", 0, 0, 402, 874);
        let mut bar = node("NavigationBar", "Inbox", 0, 62, 402, 44);
        bar.children = vec![node("Button", "Delete", 330, 66, 64, 36)];
        root.children = vec![bar];
        assert!(back_button(&root).is_none(), "edge swipe instead");

        // Review 09 #3: a root screen's leading "Edit" / "Menu" / avatar
        // button is not back, however far left it sits.
        let mut root_bar = node("NavigationBar", "Mail", 0, 62, 402, 44);
        root_bar.children = vec![
            node("Button", "Edit", 8, 66, 50, 36),
            node("Button", "Compose", 340, 66, 50, 36),
        ];
        root.children.push(root_bar);
        assert!(back_button(&root).is_none(), "edge swipe, never Edit");

        // The system back button: labelled with the previous title,
        // identified as "BackButton".
        let mut pushed = node("NavigationBar", "Detail", 0, 62, 402, 44);
        let mut system_back = node("Button", "Inbox", 8, 66, 80, 36);
        system_back.platform_id = "BackButton".into();
        pushed.children = vec![node("Button", "Edit", 330, 66, 64, 36), system_back];
        root.children.push(pushed);
        assert_eq!(back_button(&root).unwrap().platform_id, "BackButton");

        let mut labelled = node("NavigationBar", "X", 0, 62, 402, 44);
        labelled.children = vec![node("Button", "Back", 150, 66, 60, 36)];
        root.children.push(labelled);
        assert_eq!(back_button(&root).unwrap().platform_id, "Back");
    }

    #[test]
    fn a_bare_selector_picks_the_match_in_front_of_a_sheet() {
        // Review 09 #2: "Save" behind an action sheet comes first in tree
        // order; the sheet's own "Save" is the one a user can tap.
        let screen = Rect {
            x: 0,
            y: 0,
            width: 402,
            height: 874,
        };
        let mut root = node("Application", "app", 0, 0, 402, 874);
        let behind = node("Button", "Save", 300, 100, 80, 44);
        let mut sheet = node("Sheet", "", 8, 600, 386, 266);
        let mut in_front = node("Button", "Save", 8, 650, 386, 56);
        in_front.platform_id = "sheet_save".into();
        sheet.children = vec![in_front];
        root.children = vec![behind.clone(), sheet];
        let sel = Selector::Text("Save".into());
        let first = velocity_common::selector_match::find_in_tree(&root, &sel, Some(&screen))
            .expect("a match");
        assert_eq!(first.platform_id, "Save", "tree order puts it first");
        let picked = prefer_in_front(&root, &sel, &screen, first.clone());
        assert_eq!(picked.platform_id, "sheet_save");
        // An index keeps its own pick.
        let indexed = Selector::Index {
            selector: Box::new(sel),
            index: 0,
        };
        assert_eq!(
            prefer_in_front(&root, &indexed, &screen, first).platform_id,
            "Save"
        );
        // Nothing in front: the pick stands (the tap guard names the sheet).
        root.children.truncate(1);
        root.children.push(node("Sheet", "", 8, 600, 386, 266));
        let only = behind.clone();
        assert_eq!(
            prefer_in_front(&root, &Selector::Text("Save".into()), &screen, only).platform_id,
            "Save"
        );
    }

    #[test]
    fn back_skips_a_back_button_behind_a_presented_sheet() {
        let mut root = node("Application", "app", 0, 0, 402, 874);
        let mut bar = node("NavigationBar", "Detail", 0, 62, 402, 44);
        bar.children = vec![node("Button", "Back", 8, 66, 60, 36)];
        root.children = vec![bar];
        assert!(back_button(&root).is_some());
        // An action sheet over it: the tap would dismiss the sheet.
        let mut sheet = node("Sheet", "", 0, 600, 402, 274);
        sheet.children = vec![node("Button", "Cancel", 8, 800, 386, 56)];
        root.children.push(sheet);
        assert!(back_button(&root).is_none());
    }

    #[test]
    fn index_and_relative_never_go_to_wda() {
        let rel = Selector::Relative {
            target: Box::new(Selector::Text("Edit".into())),
            relation: Relation::Below,
            anchor: Box::new(Selector::Text("Name".into())),
        };
        let idx = Selector::Index {
            selector: Box::new(Selector::Text("Row".into())),
            index: 5,
        };
        assert!(!wda_can_answer(&rel) && !wda_can_answer(&idx));
        assert!(!wda_can_answer(&Selector::Compound(vec![
            Selector::Text("a".into()),
            idx
        ])));
        assert!(wda_can_answer(&Selector::Text("a".into())));
    }

    /// S1/S2: a compound with a role or a state part never goes to WDA,
    /// whose predicate would drop that part and match any "Pay".
    #[test]
    fn role_name_and_state_never_go_to_wda() {
        for part in [
            Selector::Role("button".into()),
            Selector::Name("Pay".into()),
            Selector::Checked(true),
            Selector::Enabled(false),
            Selector::Selected(true),
            Selector::Focused(true),
        ] {
            let sel = Selector::Compound(vec![Selector::Text("Pay".into()), part]);
            assert!(!wda_can_answer(&sel), "{sel}");
        }
        assert!(IosDriver::selector_to_wda(&Selector::Role("button".into())).is_err());
    }
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod fake_wda;

#[cfg(test)]
mod recovery_tests {
    use super::fake_wda::*;
    use super::*;

    /// A WDA that answers `/status` (session S1) and drops the first
    /// `/source` request; `alive` says whether S1 still exists.
    async fn dropping_wda(alive: bool) -> FakeWda {
        let reads = std::sync::Mutex::new(0);
        FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S1"}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S2","capabilities":{}}"#);
            }
            if r.method == "GET" && r.path == "/session/S1" && !alive {
                return w3c(404, "invalid session id", "Session does not exist");
            }
            if r.path.contains("alert") {
                return w3c(404, "no such alert", "none");
            }
            if r.path.ends_with("/source") {
                let mut n = reads.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    return Resp::Close;
                }
                return source(&app(&node("Button", "Pay", 10, 100, 100, 40)));
            }
            ok("null")
        })
        .await
    }

    async fn driver_on(fake: &FakeWda) -> IosDriver {
        let d = IosDriver::with_wda_url(&fake.url);
        let conn = d.conn(SIM).await;
        *conn.bundle.lock().unwrap() = "com.example.app".into();
        d
    }

    /// VelocityRunner: no `/wda/activeAppInfo`; `/wda/apps/state` answers
    /// `state` for every app.
    async fn runner_with_state(state: u8) -> FakeWda {
        FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S1"}"#.into());
            }
            if r.path.ends_with("/wda/apps/state") {
                return ok(&state.to_string());
            }
            if r.path.ends_with("activeAppInfo") {
                return w3c(404, "unknown command", "GET /wda/activeAppInfo");
            }
            ok("null")
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_app_the_runner_reports_not_running_is_dead() {
        // D-50 step 1 / P6: physical devices had no liveness signal at all.
        const DEV: &str = "00008130-001451402EC1001C";
        let f = runner_with_state(1).await;
        let d = IosDriver::with_wda_url(&f.url);
        d.conn(DEV).await;
        assert_eq!(
            d.crash_report(DEV, "com.example.app").await,
            None,
            "never launched"
        );
        d.launched.lock().unwrap().insert(
            (DEV.to_string(), "com.example.app".to_string()),
            std::time::SystemTime::now(),
        );
        assert!(matches!(
            d.app_liveness(DEV, "com.example.app").await,
            velocity_common::Liveness::Dead(m) if m.contains("not running")
        ));
        d.forget_launch(DEV, "com.example.app");
        assert_eq!(
            d.crash_report(DEV, "com.example.app").await,
            None,
            "stopped on purpose"
        );

        let f = runner_with_state(4).await;
        let d = IosDriver::with_wda_url(&f.url);
        assert_eq!(
            d.app_liveness(SIM, "com.example.app").await,
            velocity_common::Liveness::Foreground,
            "the runner's app state stands in for activeAppInfo"
        );
    }

    /// I3 (D-50 sticky crash): on a physical iPhone a watcher polls the
    /// app's state from its launch; once the runner says "not running" the
    /// crash is pending (no device call to read it) until a relaunch, and
    /// a stop ends the watcher.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_crash_is_seen_by_the_watcher_and_sticks_until_relaunch() {
        use std::sync::atomic::AtomicU8;
        const DEV: &str = "00008130-001451402EC1003F";
        const APP: &str = "com.example.app";
        let state = Arc::new(AtomicU8::new(4));
        let st = state.clone();
        let f = FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            if r.path.ends_with("/wda/apps/state") {
                return ok(&st.load(Ordering::SeqCst).to_string());
            }
            ok("null")
        })
        .await;
        let d = IosDriver::with_wda_url(&f.url);
        d.launch_app(DEV, APP, false).await.unwrap();
        let polls = || f.count("POST", "/wda/apps/state");
        let wait_for = |want: bool| {
            let d = &d;
            async move {
                for _ in 0..40 {
                    if d.pending_crash(DEV, APP).await.is_some() == want {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                false
            }
        };
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(polls() >= 1, "the watcher polls the app's state");
        assert_eq!(d.pending_crash(DEV, APP).await, None, "alive");

        state.store(1, Ordering::SeqCst);
        assert!(wait_for(true).await, "the crash is seen within a poll");
        let why = d.pending_crash(DEV, APP).await.unwrap();
        assert!(why.contains("not running"), "{why}");
        // Sticky: the app's state doesn't matter any more.
        state.store(4, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(d.pending_crash(DEV, APP).await.is_some(), "sticks");

        // A relaunch clears it and watches afresh.
        d.launch_app(DEV, APP, false).await.unwrap();
        assert_eq!(d.pending_crash(DEV, APP).await, None, "relaunched");

        // A stop on purpose ends the watcher: no more polls.
        d.stop_app(DEV, APP).await.unwrap();
        let after_stop = polls();
        state.store(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(polls(), after_stop, "no watcher after stopApp");
        assert_eq!(d.pending_crash(DEV, APP).await, None, "stopped on purpose");

        // Simulators have no watcher (the process table says it).
        let f2 = runner_with_state(1).await;
        let sim = IosDriver::with_wda_url(&f2.url);
        sim.crash_watch.note(SIM, APP, "x".into());
        assert_eq!(
            sim.pending_crash(SIM, APP).await,
            None,
            "nothing pending for an app Velocity didn't launch"
        );
    }

    /// D-50: after the app dies on a physical iPhone, its crash reports are
    /// copied off with devicectl (once per launch): the newest one since
    /// the launch names the crash and lands in the test's artifacts; older
    /// reports and other apps' are left out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_crash_report_is_copied_off_with_devicectl() {
        const DEV: &str = "00008130-001451402EC1001D";
        const APP: &str = "com.example.app";
        let dir = std::env::temp_dir().join(format!("velocity-d50-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ips = |bundle: &str, ts: &str| {
            format!(
                "{{\"bundleID\":\"{bundle}\",\"name\":\"App\",\"timestamp\":\"{ts}\"}}\n\
                 {{\"exception\":{{\"type\":\"EXC_CRASH\",\"signal\":\"SIGABRT\"}}}}"
            )
        };
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.f +0000");
        std::fs::write(dir.join("new.ips"), ips(APP, &(now.to_string()))).unwrap();
        std::fs::write(
            dir.join("old.ips"),
            ips(APP, "2020-01-01 00:00:00.00 +0000"),
        )
        .unwrap();
        std::fs::write(dir.join("other.ips"), ips("com.other", &now.to_string())).unwrap();
        let log = dir.join("args.log");
        let xcrun = dir.join("xcrun");
        // Copies the three reports into `<destination>/Retired`.
        crate::write_stub(
            &xcrun,
            &format!(
                "echo \"$@\" >> '{log}'\nprev=''; for a in \"$@\"; do [ \"$prev\" = --destination ] && D=\"$a\"; prev=\"$a\"; done\n\
                 mkdir -p \"$D/Retired\" && cp '{d}'/*.ips \"$D/Retired/\"\nexit 0",
                log = log.display(),
                d = dir.display()
            ),
        );
        let f = runner_with_state(1).await;
        let mut d = IosDriver::with_wda_url(&f.url);
        d.devicectl = Devicectl::with_xcrun(xcrun.to_str().unwrap());
        d.conn(DEV).await;
        d.launched.lock().unwrap().insert(
            (DEV.to_string(), APP.to_string()),
            std::time::SystemTime::now() - Duration::from_secs(60),
        );
        let why = d.crash_report(DEV, APP).await.unwrap();
        assert!(why.contains("EXC_CRASH (SIGABRT)"), "{why}");
        let dest = dir.join("artifacts");
        let saved = d.copy_crash_logs(DEV, APP, &dest).await;
        assert_eq!(saved, [dest.join("new.ips")]);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "copied once per launch: {calls}");
        assert!(
            calls.contains(&format!(
                "devicectl device copy from --device {DEV} --domain-type systemCrashLogs --source ."
            )),
            "{calls}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review MINOR 12: a failed copy is remembered for the launch (every
    /// crash check re-ran it), and a copy is removed with the driver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_crash_log_copy_is_not_retried_and_copies_are_cleaned_up() {
        const DEV: &str = "00008130-001451402EC1002E";
        const APP: &str = "com.example.app";
        let dir = std::env::temp_dir().join(format!("velocity-m12-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("args.log");
        let xcrun = dir.join("xcrun");
        crate::write_stub(
            &xcrun,
            &format!(
                "echo \"$@\" >> '{}'\necho 'the device disconnected' >&2; exit 1",
                log.display()
            ),
        );
        let f = runner_with_state(1).await;
        let mut d = IosDriver::with_wda_url(&f.url);
        d.devicectl = Devicectl::with_xcrun(xcrun.to_str().unwrap());
        d.conn(DEV).await;
        d.launched.lock().unwrap().insert(
            (DEV.to_string(), APP.to_string()),
            std::time::SystemTime::now() - Duration::from_secs(60),
        );
        for _ in 0..3 {
            let why = d.crash_report(DEV, APP).await.unwrap();
            assert!(why.contains("no crash report"), "{why}");
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "tried once per launch: {calls}");
        // A copy that worked is removed when the driver goes.
        let kept = dir.join("kept-copy");
        std::fs::create_dir_all(&kept).unwrap();
        d.device_crash_logs.lock().unwrap().insert(
            (DEV.to_string(), "other.app".to_string()),
            (std::time::SystemTime::now(), Some(kept.clone())),
        );
        drop(d);
        assert!(!kept.exists(), "removed with the driver");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clock on an iOS 26 simulator: WDA opened a session for an app the
    /// simulator doesn't have, and the missing app then read as "crashed".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launching_an_app_the_simulator_does_not_have_says_so() {
        let f = FakeWda::start(|r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            ok("null")
        })
        .await;
        let dir = std::env::temp_dir().join(format!("velocity-missing-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xcrun = dir.join("xcrun");
        crate::write_stub(
            &xcrun,
            "case \"$*\" in\n*get_app_container*com.apple.mobiletimer*) \
             echo 'The operation couldn’t be completed. No such file or directory' >&2; exit 2;;\n\
             *get_app_container*) echo /tmp/x.app;;\nesac\nexit 0",
        );
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = Simctl::with_xcrun(xcrun.to_str().unwrap());
        let e = d
            .launch_app(SIM, "com.apple.mobiletimer", false)
            .await
            .unwrap_err();
        assert!(matches!(e, VelocityError::Config(_)), "{e:?}");
        assert!(e.to_string().contains("not installed"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_state_that_could_not_clear_fails_the_launch() {
        // VEL-S45: an unresolvable container was a WARN and the test ran on
        // the previous run's data; a failed wipe was ignored.
        let dir = std::env::temp_dir().join(format!("velocity-s45-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let container = dir.join("container");
        std::fs::create_dir_all(&container).unwrap();
        // "Library" is a file: removing it as a directory fails.
        std::fs::write(container.join("Library"), "x").unwrap();
        let xcrun = dir.join("xcrun");
        crate::write_stub(
            &xcrun,
            &format!(
                "case \"$*\" in\n*get_app_container*nope*) echo 'No such app' >&2; exit 1;;\n\
                 *get_app_container*) echo '{}';;\nesac\nexit 0",
                container.display()
            ),
        );
        let mut d = IosDriver::with_wda_url("http://127.0.0.1:9");
        d.simctl = Simctl::with_xcrun(xcrun.to_str().unwrap());
        let e = d
            .launch_app(SIM, "com.example.nope", true)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("data container"), "{e}");
        let e = d
            .launch_app(SIM, "com.example.app", true)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("could not clear"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_read_on_a_live_wda_keeps_the_session_and_the_app() {
        // Review M1: recovery opened a session with the bundle, which
        // relaunched the app mid-test, though WDA was alive all along.
        let f = dropping_wda(true).await;
        let d = driver_on(&f).await;
        let tree = d.get_hierarchy(SIM).await.unwrap();
        assert_eq!(tree.children.len(), 1);
        assert_eq!(f.count("POST", "/session"), 0, "session kept");
        assert_eq!(f.count("GET", "/source"), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_wda_that_lost_the_session_gets_one_without_the_app() {
        let f = dropping_wda(false).await;
        let d = driver_on(&f).await;
        d.get_hierarchy(SIM).await.unwrap();
        let log = f.log.lock().unwrap().clone();
        let made: Vec<_> = log
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/session")
            .collect();
        assert_eq!(made.len(), 1, "{log:?}");
        assert!(
            !made[0].body.contains("bundleId"),
            "app relaunched: {}",
            made[0].body
        );
        assert!(
            log.iter().any(|r| r.path.ends_with("/appium/settings")
                && r.body.contains("com.example.app")),
            "queries still point at the app"
        );
    }
}

#[cfg(test)]
mod sync_port_tests {
    use super::fake_wda::*;
    use super::*;

    /// A WDA whose `/status` names session S1, alive when `reuse`; every
    /// other call succeeds.
    async fn wda(reuse: bool) -> FakeWda {
        FakeWda::start(move |r| {
            if r.path == "/status" {
                let sid = if reuse { r#","sessionId":"S1""# } else { "" };
                return Resp::Json(200, format!(r#"{{"value":{{"ready":true}}{sid}}}"#));
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S2","capabilities":{}}"#);
            }
            ok("null")
        })
        .await
    }

    #[tokio::test]
    async fn freeze_status_bar_overrides_a_simulator_once_and_cleanup_clears_it() {
        // D-V4: command lines only, through a logging xcrun stub.
        let dir = std::env::temp_dir().join(format!("velocity-statusbar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (simctl, log) = logging_xcrun(&dir);
        let mut d = IosDriver::with_wda_url("http://127.0.0.1:9");
        d.simctl = simctl;
        assert!(d.freeze_status_bar(SIM).await);
        assert!(d.freeze_status_bar(SIM).await);
        // A real iPhone has no simctl: its bar stays masked.
        assert!(!d.freeze_status_bar("00008110-001A2B3C4D5E6F70").await);
        d.cleanup().await;
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| l.trim_end_matches(" port=").to_string())
            .collect();
        assert_eq!(
            calls,
            [
                format!(
                    "simctl status_bar {SIM} override --time 9:41 --dataNetwork wifi \
                     --wifiMode active --wifiBars 3 --cellularMode active --cellularBars 4 \
                     --operatorName  --batteryState charged --batteryLevel 100"
                ),
                format!("simctl status_bar {SIM} clear"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// review-d5 m-4: the end of a run (also a daemon's, which keeps the
    /// driver) clears the override; the next run's freeze sets it again.
    #[tokio::test]
    async fn unfreeze_status_bar_clears_the_override_and_a_later_freeze_redoes_it() {
        let dir =
            std::env::temp_dir().join(format!("velocity-statusbar-thaw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (simctl, log) = logging_xcrun(&dir);
        let mut d = IosDriver::with_wda_url("http://127.0.0.1:9");
        d.simctl = simctl;
        assert!(d.freeze_status_bar(SIM).await);
        d.unfreeze_status_bar().await;
        assert!(d.freeze_status_bar(SIM).await);
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter_map(|l| l.split_whitespace().nth(3).map(str::to_string))
            .collect();
        assert_eq!(calls, ["override", "clear", "override"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An `xcrun` that logs each simctl call with the app's
    /// `SIMCTL_CHILD_VELOCITY_SYNC_PORT`.
    fn logging_xcrun(dir: &std::path::Path) -> (Simctl, std::path::PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.join("calls.log");
        let xcrun = dir.join("xcrun");
        crate::write_stub(
            &xcrun,
            &format!(
                "echo \"$* port=$SIMCTL_CHILD_VELOCITY_SYNC_PORT\" >> '{}'\nexit 0",
                log.display()
            ),
        );
        (Simctl::with_xcrun(xcrun.to_str().unwrap()), log)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_session_launches_the_app_with_the_simulators_sync_port() {
        let f = wda(false).await;
        let dir = std::env::temp_dir().join(format!("velocity-syncport-a-{}", std::process::id()));
        let (simctl, _) = logging_xcrun(&dir);
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        assert_eq!(d.sync_probe_port(SIM), None, "nothing launched yet");
        d.launch_app(SIM, "com.example.app", false).await.unwrap();
        let port = d.sync_probe_port(SIM).expect("a per-simulator port");
        let sessions: Vec<Req> = f
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/session")
            .cloned()
            .collect();
        assert_eq!(sessions.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&sessions[0].body).unwrap();
        let always = &body["capabilities"]["alwaysMatch"];
        assert_eq!(always["bundleId"], "com.example.app");
        // DV-2: no WDA idle wait inside the launch (iPhone 17 Pro, iOS 26.0).
        assert_eq!(always["shouldWaitForQuiescence"], false, "{body}");
        assert_eq!(always["waitForIdleTimeout"], 0, "{body}");
        assert_eq!(
            always["environment"]["VELOCITY_SYNC_PORT"],
            port.to_string(),
            "{body}"
        );
        // A relaunch keeps the port the host connects to.
        d.launch_app(SIM, "com.example.app", false).await.unwrap();
        assert_eq!(d.sync_probe_port(SIM), Some(port));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// AUT-024: `launchApp` arguments reach `simctl launch` after the
    /// bundle id as `-name value` argv items.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launch_arguments_go_after_the_bundle_id_on_simctl() {
        let f = wda(true).await;
        let dir = std::env::temp_dir().join(format!("velocity-launchargs-{}", std::process::id()));
        let (simctl, log) = logging_xcrun(&dir);
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        let mut args = LaunchArgs::default();
        args.inner.insert("isE2E".into(), LaunchArg::Bool(true));
        args.inner.insert(
            "server".into(),
            LaunchArg::Str("https://staging.example.com".into()),
        );
        d.launch_app_with(SIM, "com.example.app", false, &args)
            .await
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.lines().any(|l| l.starts_with(&format!(
                "simctl launch {SIM} com.example.app -isE2E YES -server https://staging.example.com"
            ))),
            "{calls}"
        );
        // The next plain launch carries none of them.
        d.launch_app(SIM, "com.example.app", false).await.unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        let last = calls
            .lines()
            .rfind(|l| l.starts_with("simctl launch"))
            .unwrap();
        assert!(!last.contains("-isE2E"), "{last}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_argv_renders_each_type() {
        let mut args = LaunchArgs::default();
        args.inner.insert("a".into(), LaunchArg::Bool(false));
        args.inner.insert("b".into(), LaunchArg::Int(-3));
        args.inner.insert("c".into(), LaunchArg::Float(2.5));
        args.inner
            .insert("d".into(), LaunchArg::Str("-starts with dash".into()));
        assert_eq!(
            launch_argv(&args),
            [
                "-a",
                "NO",
                "-b",
                "-3",
                "-c",
                "2.5",
                "-d",
                "-starts with dash"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reused_session_launches_through_simctl_with_the_port_in_simctl_child() {
        let f = wda(true).await;
        let dir = std::env::temp_dir().join(format!("velocity-syncport-b-{}", std::process::id()));
        let (simctl, log) = logging_xcrun(&dir);
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        d.launch_app(SIM, "com.example.app", false).await.unwrap();
        let port = d.sync_probe_port(SIM).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls
                .lines()
                .any(|l| l == format!("simctl launch {SIM} com.example.app port={port}")),
            "{calls}"
        );
        assert_eq!(f.count("POST", "/session"), 0, "session reused");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn simulators_get_distinct_stable_ports_and_devices_none() {
        let d = IosDriver::with_wda_url("http://127.0.0.1:9");
        let a = d.sync_port("00000000-0000-0000-0000-00000000000A").unwrap();
        let b = d.sync_port("00000000-0000-0000-0000-00000000000B").unwrap();
        assert_ne!(a, b);
        assert!((SYNC_PORT_BASE..SYNC_PORT_BASE + SYNC_PORT_SPAN).contains(&a));
        assert_eq!(d.sync_port("00000000-0000-0000-0000-00000000000A"), Some(a));
        assert_eq!(
            d.app_env("00000000-0000-0000-0000-00000000000A"),
            vec![("VELOCITY_SYNC_PORT".to_string(), a.to_string())]
        );
        // Real devices reach the probe through usbmux: no port, no env.
        assert_eq!(d.sync_port("00008130-001451402EC1001C"), None);
        assert!(d.app_env("00008130-001451402EC1001C").is_empty());
        assert_eq!(d.sync_probe_port("00008130-001451402EC1001C"), None);
    }

    #[test]
    fn a_taken_or_bound_candidate_moves_to_the_next_free_port() {
        let udid = "00000000-0000-0000-0000-0000000000CD";
        let first = sync_port_candidate(udid);
        assert_eq!(sync_port_candidate(udid), first, "deterministic");
        // Another simulator here has it.
        let next = pick_sync_port(udid, &[first]).unwrap();
        assert_ne!(next, first);
        // Something else on the Mac listens on it.
        if let Ok(_held) = std::net::TcpListener::bind(("127.0.0.1", first)) {
            assert_ne!(pick_sync_port(udid, &[]), Some(first));
        }
    }
}

#[cfg(test)]
mod relaunch_and_typing_tests {

    #[test]
    fn back_in_a_sheet_finds_its_close_button_never_done() {
        let b = |label: &str, id: &str, y: i32| Element {
            element_type: "Button".into(),
            label: Some(label.into()),
            platform_id: id.into(),
            bounds: Rect {
                x: 20,
                y,
                width: 60,
                height: 40,
            },
            visible: true,
            enabled: true,
            ..Default::default()
        };
        let mut root = Element {
            element_type: "Application".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 402,
                height: 874,
            },
            visible: true,
            ..Default::default()
        };
        root.children = vec![b("Done", "done", 90), b("Close", "thread_close_button", 90)];
        assert_eq!(
            sheet_close_button(&root).map(|e| e.platform_id),
            Some("thread_close_button".into()),
            "its label is exactly Close"
        );
        root.children = vec![b("Done", "done", 90), b("Close", "x", 700)];
        assert!(
            sheet_close_button(&root).is_none(),
            "a close button low on the screen isn't the sheet's"
        );
        root.children = vec![b("xmark", "xmark", 80)];
        assert!(sheet_close_button(&root).is_some());
        root.children = vec![b("xmark.circle.fill", "", 80)];
        assert!(sheet_close_button(&root).is_some());
        // Review 09 #3: app actions that start or end like a close are not
        // a way out of the sheet.
        for (label, id) in [
            ("Close account", "close_account"),
            ("Close all tabs", "tabs_close"),
            ("Close tab", "tab_close"),
            ("Close Ring", "ring_close_button"),
            ("Delete", "delete_xmarker"),
            ("X", "share_x"),
        ] {
            root.children = vec![b(label, id, 90)];
            assert!(sheet_close_button(&root).is_none(), "{label}");
        }
        for name in [
            "Close",
            "cancel",
            "Dismiss",
            "✕",
            "closeButton",
            "close_button",
        ] {
            root.children = vec![b("", name, 90)];
            assert!(sheet_close_button(&root).is_some(), "{name}");
        }
    }

    #[test]
    fn a_failed_wheel_read_is_the_devices_error_not_an_empty_value() {
        // Review 03 M4: a dead WDA read was folded into "" and the loop
        // ended in an assertion failure (exit 1).
        let lost = wheel_value(Err(VelocityError::WdaLost {
            device_id: String::new(),
            url: "http://127.0.0.1:8100".into(),
            detail: "connection reset".into(),
        }))
        .unwrap_err();
        assert_eq!(lost.exit_code(), 3, "{lost}");
        let busy = wheel_value(Err(VelocityError::CommandFailed {
            device_id: "http://127.0.0.1:8100".into(),
            command: "WDA element value".into(),
            detail: "WDA busy".into(),
        }));
        assert!(busy.is_err());
        let gone = wheel_value(Err(VelocityError::ElementNotFound {
            selector: "element value".into(),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        }));
        assert_eq!(gone.unwrap(), "");
        assert_eq!(wheel_value(Ok("PM".into())).unwrap(), "PM");
    }

    #[test]
    fn picker_values_match_by_text_or_leading_number() {
        assert!(wheel_value_matches("6 o’clock", "6 o'clock"));
        assert!(wheel_value_matches("6 o’clock", "6"));
        assert!(wheel_value_matches("45 minutes", "45"));
        assert!(wheel_value_matches("05 minutes", "5"));
        assert!(wheel_value_matches("PM", "pm"));
        assert!(!wheel_value_matches("16 o’clock", "6"));
        assert!(!wheel_value_matches("6 o’clock", "7"));
        assert!(!wheel_value_matches("am", "pm"));
        assert_eq!(leading_number("12 o’clock"), Some(12));
        assert_eq!(leading_number("pm"), None);
    }

    use super::fake_wda::*;
    use super::*;

    const APP: &str = "com.example.app";

    /// A WDA whose `/status` names session S1 when `session`; terminate
    /// answers `terminate`, `/wda/keys` answers `keys(n)` for its n-th call
    /// (from 1), and `/source` is one text field over a keyboard when
    /// `keyboard`.
    async fn wda(
        session: bool,
        terminate: fn() -> Resp,
        keys: fn(usize) -> Resp,
        keyboard: bool,
    ) -> FakeWda {
        let typed = std::sync::Mutex::new(0);
        FakeWda::start(move |r| {
            if r.path == "/status" {
                let sid = if session { r#","sessionId":"S1""# } else { "" };
                return Resp::Json(200, format!(r#"{{"value":{{"ready":true}}{sid}}}"#));
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S2","capabilities":{}}"#);
            }
            if r.path.contains("alert") {
                return w3c(404, "no such alert", "none");
            }
            if r.path.ends_with("/wda/apps/terminate") {
                return terminate();
            }
            if r.path.ends_with("/wda/keys") {
                let mut n = typed.lock().unwrap();
                *n += 1;
                return keys(*n);
            }
            if r.path.ends_with("/source") {
                let kb = if keyboard {
                    node("Keyboard", "", 0, 500, 402, 300)
                } else {
                    String::new()
                };
                return source(&app(&format!(
                    "{}{kb}",
                    node("TextField", "Email", 20, 100, 362, 44)
                )));
            }
            ok("null")
        })
        .await
    }

    fn ok_null() -> Resp {
        ok("null")
    }

    fn unfocused() -> Resp {
        w3c(
            500,
            "unknown error",
            "Failed to synthesize event: Neither element nor any descendant has keyboard focus.",
        )
    }

    /// An `xcrun` logging each call to `<dir>/calls.log`.
    fn xcrun(tag: &str) -> (Simctl, std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("velocity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let stub = dir.join("xcrun");
        crate::write_stub(
            &stub,
            &format!("echo \"$*\" >> '{}'\nexit 0", log.display()),
        );
        (Simctl::with_xcrun(stub.to_str().unwrap()), log, dir)
    }

    fn simctl_terminates(log: &std::path::Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == format!("simctl terminate {SIM} {APP}"))
            .count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_launch_with_a_session_terminates_the_app_through_the_runner() {
        let f = wda(true, || ok("true"), |_| ok_null(), false).await;
        let (simctl, log, dir) = xcrun("term-runner");
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        d.launch_app(SIM, APP, false).await.unwrap();
        d.launch_app(SIM, APP, false).await.unwrap();
        let terminates: Vec<Req> = f
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/session/S1/wda/apps/terminate")
            .cloned()
            .collect();
        assert_eq!(terminates.len(), 2, "both launches cold-start the app");
        let body: serde_json::Value = serde_json::from_str(&terminates[0].body).unwrap();
        assert_eq!(body["bundleId"], APP);
        assert_eq!(simctl_terminates(&log), 0, "no simctl round trip");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_runner_terminate_falls_back_to_simctl() {
        let f = wda(
            true,
            || w3c(500, "unknown error", "boom"),
            |_| ok_null(),
            false,
        )
        .await;
        let (simctl, log, dir) = xcrun("term-refused");
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        d.launch_app(SIM, APP, false).await.unwrap();
        assert_eq!(f.count("POST", "/wda/apps/terminate"), 1);
        assert_eq!(simctl_terminates(&log), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_runner_terminate_falls_back_to_simctl_within_its_timeout() {
        let f = wda(
            true,
            || Resp::Hang(Duration::from_secs(60)),
            |_| ok_null(),
            false,
        )
        .await;
        let (simctl, log, dir) = xcrun("term-hung");
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        assert_eq!(d.terminate_timeout, RUNNER_TERMINATE_TIMEOUT);
        let conn = d.running(SIM).await.unwrap();
        let t = std::time::Instant::now();
        assert!(!IosDriver::terminate_in_session(&conn, APP, RUNNER_TERMINATE_TIMEOUT).await);
        let waited = t.elapsed();
        assert!(
            waited >= RUNNER_TERMINATE_TIMEOUT && waited < Duration::from_secs(7),
            "gave up on the runner after {waited:?}, not the client's 30 s"
        );
        // The launch path: a short timeout here, and simctl has terminated
        // the app before the launch returns.
        d.terminate_timeout = Duration::from_millis(300);
        let t = std::time::Instant::now();
        d.launch_app(SIM, APP, false).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(f.count("POST", "/wda/apps/terminate"), 2);
        assert_eq!(simctl_terminates(&log), 1);
        let calls = std::fs::read_to_string(&log).unwrap();
        let term = calls.find("simctl terminate").unwrap();
        let launch = calls.find("simctl launch").expect("relaunched");
        assert!(
            term < launch,
            "relaunched before simctl terminated:\n{calls}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_a_session_simctl_terminates_until_the_launch_makes_one() {
        let f = wda(false, || ok("true"), |_| ok_null(), false).await;
        let (simctl, log, dir) = xcrun("term-nosession");
        let mut d = IosDriver::with_wda_url(&f.url);
        d.simctl = simctl;
        d.launch_app(SIM, APP, false).await.unwrap();
        assert_eq!(f.count("POST", "/wda/apps/terminate"), 0);
        assert_eq!(simctl_terminates(&log), 1);
        // The launch created session S2: the next one terminates through it.
        d.launch_app(SIM, APP, false).await.unwrap();
        assert_eq!(f.count("POST", "/session/S2/wda/apps/terminate"), 1);
        assert_eq!(simctl_terminates(&log), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    async fn field(d: &IosDriver) -> Element {
        d.find_element(SIM, &Selector::Text("Email".into()))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_starts_at_once_and_retries_until_the_field_takes_focus() {
        let f = wda(
            true,
            || ok("true"),
            |n| if n < 3 { unfocused() } else { ok_null() },
            false,
        )
        .await;
        let mut d = IosDriver::with_wda_url(&f.url);
        d.typing_checks_focus = true;
        let field = field(&d).await;
        d.input_text(SIM, &field, "hi").await.unwrap();
        assert_eq!(f.count("POST", "/actions"), 1, "tapped once");
        assert_eq!(f.count("POST", "/wda/keys"), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_field_that_never_takes_focus_fails_after_the_same_wait() {
        let f = wda(true, || ok("true"), |_| unfocused(), false).await;
        let mut d = IosDriver::with_wda_url(&f.url);
        d.typing_checks_focus = true;
        let field = field(&d).await;
        let started = Instant::now();
        let e = d.input_text(SIM, &field, "hi").await.unwrap_err();
        assert!(matches!(e, VelocityError::AssertionFailed { .. }), "{e:?}");
        assert!(started.elapsed() >= FOCUS_WAIT, "{:?}", started.elapsed());
        assert!(f.count("POST", "/wda/keys") >= 2, "polled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wda_keeps_the_fixed_wait_and_types_once() {
        // WDA sends keys without checking focus: an early key is lost, not
        // refused, so there is nothing to retry on.
        let f = wda(true, || ok("true"), |_| unfocused(), false).await;
        let d = IosDriver::with_wda_url(&f.url);
        let field = field(&d).await;
        let started = Instant::now();
        assert!(d.input_text(SIM, &field, "hi").await.is_err());
        assert!(started.elapsed() >= FOCUS_WAIT);
        assert_eq!(f.count("POST", "/wda/keys"), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_focus_check_reads_once_when_the_keyboard_is_up() {
        let f = wda(true, || ok("true"), |_| ok_null(), true).await;
        let mut d = IosDriver::with_wda_url(&f.url);
        d.focus_check = true;
        d.strict_focus = false;
        let field = field(&d).await;
        let reads = f.count("GET", "/source");
        d.input_text(SIM, &field, "hi").await.unwrap();
        assert_eq!(f.count("GET", "/source") - reads, 1, "one poll");
        assert_eq!(f.count("POST", "/wda/keys"), 1);
    }

    /// iPhone 12, 2026-11-21: after the read was reused, polls of an
    /// `assertNotVisible` saw rows behind the sheet as visible, and a tap
    /// behind it went through. A dropped tree's address came back on the
    /// next, unflagged read, which then passed for already merged. Every
    /// poll's tree must carry the flags.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_poll_under_a_sheet_keeps_the_hidden_flags() {
        let plain = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Behind" x="8" y="100" width="386" height="44" enabled="true"/><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="" x="0" y="60" width="402" height="814" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Save" x="8" y="650" width="386" height="56" enabled="true"/></XCUIElementTypeSheet></XCUIElementTypeApplication>"#;
        let flagged = plain
            .replace(r#"label="Behind" "#, r#"label="Behind" visible="false" "#)
            .replace(r#"label="Save" "#, r#"label="Save" visible="true" "#);
        let f = FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            if r.path.ends_with("/source") {
                if r.query == "excluded_attributes=accessible" {
                    return source(&flagged);
                }
                return source(plain);
            }
            ok("null")
        })
        .await;
        let d = IosDriver::with_wda_url(&f.url);
        fn behind(e: &Element) -> Option<&Element> {
            if e.label.as_deref() == Some("Behind") {
                return Some(e);
            }
            e.children.iter().find_map(behind)
        }
        for poll in 0..40 {
            // What a polling assertVisible does between reads.
            d.invalidate_hierarchy_cache().await;
            let tree = d.get_hierarchy_cached(SIM).await.unwrap();
            let b = behind(&tree).expect("the row behind the sheet");
            assert!(!b.visible, "poll {poll}: {b:?}");
        }
    }

    /// Two lookups of the same sheet screen with no action between them make
    /// ONE visibility request, not two.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_lookups_of_the_same_sheet_reuse_visibility() {
        let sheet_xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true"><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="" x="8" y="600" width="386" height="266" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Save" x="8" y="650" width="386" height="56" enabled="true"/></XCUIElementTypeSheet></XCUIElementTypeApplication>"#;
        let visibility_count = Arc::new(std::sync::Mutex::new(0));
        let plain_count = Arc::new(std::sync::Mutex::new(0));
        let vc = visibility_count.clone();
        let pc = plain_count.clone();
        let vc2 = visibility_count.clone();
        let pc2 = plain_count.clone();
        let f = FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            if r.path.ends_with("/source") {
                if r.query == "excluded_attributes=accessible" {
                    *vc.lock().unwrap() += 1;
                    return source(sheet_xml);
                }
                *pc.lock().unwrap() += 1;
                return source(sheet_xml);
            }
            ok("null")
        })
        .await;
        let d = IosDriver::with_wda_url(&f.url);
        // First lookup: plain read + visibility read.
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(*pc2.lock().unwrap(), 1, "one plain read");
        assert_eq!(*vc2.lock().unwrap(), 1, "one visibility read");
        // Second lookup on the same screen: reused visibility, no new request.
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(*pc2.lock().unwrap(), 1, "still one plain read");
        assert_eq!(
            *vc2.lock().unwrap(),
            1,
            "still one visibility read (reused)"
        );
        drop(visibility_count);
        drop(plain_count);
    }

    /// After an action (a tap) the next lookup makes a fresh visibility request
    /// because `invalidate` cleared `last_visibility`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_action_invalidates_visibility_cache() {
        let sheet_xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true"><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="" x="8" y="600" width="386" height="266" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Save" x="8" y="650" width="386" height="56" enabled="true"/></XCUIElementTypeSheet></XCUIElementTypeApplication>"#;
        let visibility_count = Arc::new(std::sync::Mutex::new(0));
        let vc_closure = visibility_count.clone();
        let vc_test = visibility_count.clone();
        let f = FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            if r.method == "POST" && r.path.ends_with("/actions") {
                return ok("null");
            }
            if r.path.ends_with("/source") {
                if r.query == "excluded_attributes=accessible" {
                    *vc_closure.lock().unwrap() += 1;
                }
                return source(sheet_xml);
            }
            ok("null")
        })
        .await;
        let d = IosDriver::with_wda_url(&f.url);
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(
            *vc_test.lock().unwrap(),
            1,
            "one visibility read before the tap"
        );
        d.tap(
            SIM,
            &d.find_element(SIM, &Selector::Text("Save".into()))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(
            *vc_test.lock().unwrap(),
            2,
            "fresh visibility read after the tap"
        );
        drop(visibility_count);
    }

    /// A screen whose shape changed between reads makes a fresh visibility
    /// request: `copy_visibility` returns false when the cached visibility
    /// tree no longer matches the plain tree.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shape_change_makes_a_fresh_visibility_request() {
        let sheet_xml_a = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true"><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="" x="8" y="600" width="386" height="266" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Save" x="8" y="650" width="386" height="56" enabled="true"/></XCUIElementTypeSheet></XCUIElementTypeApplication>"#;
        let sheet_xml_b = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="402" height="874" enabled="true"><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="" x="8" y="600" width="386" height="266" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Save" x="20" y="200" width="60" height="60" enabled="true"/></XCUIElementTypeSheet></XCUIElementTypeApplication>"#;
        let visibility_count = Arc::new(std::sync::Mutex::new(0));
        let vc_closure = visibility_count.clone();
        let vc_test = visibility_count.clone();
        let state = Arc::new(std::sync::Mutex::new(0u8));
        let st = state.clone();
        let f = FakeWda::start(move |r| {
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
            }
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
            }
            if r.path.ends_with("/source") {
                let n = *st.lock().unwrap();
                if r.query == "excluded_attributes=accessible" {
                    *vc_closure.lock().unwrap() += 1;
                }
                return if n == 0 {
                    source(sheet_xml_a)
                } else {
                    source(sheet_xml_b)
                };
            }
            ok("null")
        })
        .await;
        let d = IosDriver::with_wda_url(&f.url);
        // First lookup: reads sheet A, visibility read caches it.
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(
            *vc_test.lock().unwrap(),
            1,
            "one visibility read for screen A"
        );
        // Let the plain tree cache (2 s TTL) expire but not the visibility cache (10 s reuse).
        tokio::time::sleep(TREE_TTL + Duration::from_millis(50)).await;
        *state.lock().unwrap() = 1;
        // Second lookup: plain read fetches screen B (tree cache expired),
        // visibility cache holds A (different shape) → copy_visibility fails → fresh visibility read.
        d.find_element(SIM, &Selector::Text("Save".into()))
            .await
            .unwrap();
        assert_eq!(
            *vc_test.lock().unwrap(),
            2,
            "fresh visibility read after shape change"
        );
        drop(visibility_count);
    }
}
