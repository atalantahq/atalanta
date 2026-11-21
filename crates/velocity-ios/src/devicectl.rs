//! Real iPhones and iPads through `xcrun devicectl` (Xcode 15+): listing,
//! install, launch, and opening links. Simulators stay on `simctl`.
//!
//! A device counts as ready when it is paired, connected (USB or network;
//! `devicectl` reports a transport) and has Developer Mode on, which
//! XCTest — and so WebDriverAgent — needs.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;
use velocity_common::process::{output_with_timeout, LONG_TIMEOUT};
use velocity_common::{DeviceInfo, DeviceState, DeviceType, Platform, Result, VelocityError};

/// The word after `--` in a devicectl launch with arguments: devicectl
/// hands the `--` to the app, and this is what Foundation pairs it with.
const LAUNCH_ARGS_FILLER: &str = "velocity";

pub struct Devicectl {
    xcrun: String,
    /// `list devices` timeouts: the first call in the process, then later
    /// ones (see [`Self::list_devices`]).
    list_timeouts: (Duration, Duration),
    /// A `list devices` call has run already.
    listed: std::sync::atomic::AtomicBool,
}

impl Default for Devicectl {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes `devicectl`'s JSON output file however the run ends (VEL-294:
/// a timeout returned before the removal and left it behind).
struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `devicectl list devices` failed once already this process: later
/// failures stay at debug level.
static LIST_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Devicectl {
    /// `xcrun` from `VELOCITY_XCRUN_PATH`, like `simctl`.
    pub fn new() -> Self {
        Self::with_xcrun(
            &velocity_common::env::var("VELOCITY_XCRUN_PATH").unwrap_or_else(|_| "xcrun".into()),
        )
    }

    /// Run devicectl through this `xcrun` (tests use a stub).
    pub fn with_xcrun(xcrun: &str) -> Self {
        Self {
            xcrun: xcrun.to_string(),
            list_timeouts: (Duration::from_secs(30), Duration::from_secs(10)),
            listed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Run `xcrun devicectl <args> --json-output <tmp>` on device `udid`
    /// ("" for none) and return the JSON. A command that succeeded without
    /// JSON is fine unless the caller `needs_json` (VEL-295: a successful
    /// install read as failed).
    async fn run_json(
        &self,
        udid: &str,
        args: &[&str],
        timeout: Duration,
        needs_json: bool,
    ) -> Result<serde_json::Value> {
        let out = TempFile(std::env::temp_dir().join(format!(
            "velocity-devicectl-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        let command = format!("devicectl {}", args.join(" "));
        let failed = |detail: String| VelocityError::CommandFailed {
            device_id: udid.to_string(),
            command: command.clone(),
            detail,
        };
        // `--json-output` goes before a `--`: everything after it is the
        // app's own command line, which devicectl must not parse.
        let split = args.iter().position(|a| *a == "--").unwrap_or(args.len());
        let mut cmd = Command::new(&self.xcrun);
        cmd.arg("devicectl")
            .args(&args[..split])
            .arg("--json-output")
            .arg(&out.0)
            .args(&args[split..])
            .stdin(Stdio::null());
        debug!(?args, "devicectl");
        // Not a config error: a wedged CoreDevice is a device problem
        // (VEL-146).
        let output = output_with_timeout(&mut cmd, timeout)
            .await
            .map_err(|e| failed(e.to_string()))?;
        let json = std::fs::read_to_string(&out.0).ok();
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !udid.is_empty() && detail.to_ascii_lowercase().contains("locked") {
                return Err(VelocityError::DeviceLocked {
                    id: udid.to_string(),
                });
            }
            return Err(failed(detail));
        }
        match json.and_then(|s| serde_json::from_str(&s).ok()) {
            Some(v) => Ok(v),
            None if !needs_json => Ok(serde_json::Value::Null),
            None => Err(failed("devicectl wrote no JSON".into())),
        }
    }

    /// Paired real devices. Missing Xcode or no devices is an empty list.
    /// The first call may take 30 s: CoreDeviceService's cold start after a
    /// boot often takes 10-20 s, and real devices vanished from that first
    /// run (review m12). Later ones get 10 s: a wedged CoreDevice used to
    /// add 30 s to every call without a word (VEL-294). The first failure
    /// is a warning.
    pub async fn list_devices(&self) -> Vec<DeviceInfo> {
        match self.try_list_devices().await {
            Ok(d) => d,
            Err(e) => {
                if LIST_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    debug!("devicectl list failed: {e}");
                } else {
                    tracing::warn!("listing real iOS devices failed (devicectl): {e}");
                }
                Vec::new()
            }
        }
    }

    /// [`list_devices`](Self::list_devices), with the failure. The first
    /// call's 30 s allowance is used up only by a call that finished: one
    /// cut short by its caller (the up-front `--device` check's own
    /// timeout) leaves it to the next call (review d4 m3).
    pub async fn try_list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let first = !self.listed.load(std::sync::atomic::Ordering::Relaxed);
        let timeout = if first {
            self.list_timeouts.0
        } else {
            self.list_timeouts.1
        };
        let listed = self.run_json("", &["list", "devices"], timeout, true).await;
        self.listed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        listed.map(|v| parse_devices(&v))
    }

    pub async fn install(&self, udid: &str, app_path: &str) -> Result<()> {
        let app_path = positional_path(app_path);
        self.run_json(
            udid,
            &["device", "install", "app", "--device", udid, &app_path],
            LONG_TIMEOUT,
            false,
        )
        .await
        .map(|_| ())
    }

    /// What `devicectl` reports for the installed `bundle_id` (bundle URL
    /// and versions), or `None` when it isn't installed: the install-by-hash
    /// stamp for a real device.
    pub async fn app_stamp(&self, udid: &str, bundle_id: &str) -> Option<String> {
        check_bundle_id(bundle_id).ok()?;
        let v = self
            .run_json(
                udid,
                &[
                    "device",
                    "info",
                    "apps",
                    "--device",
                    udid,
                    "--bundle-id",
                    bundle_id,
                ],
                Duration::from_secs(60),
                true,
            )
            .await
            .ok()?;
        parse_app_stamp(&v, bundle_id)
    }

    pub async fn uninstall(&self, udid: &str, bundle_id: &str) -> Result<()> {
        check_bundle_id(bundle_id)?;
        self.run_json(
            udid,
            &["device", "uninstall", "app", "--device", udid, bundle_id],
            Duration::from_secs(120),
            false,
        )
        .await
        .map(|_| ())
    }

    /// Launch (relaunching if it runs), optionally with a URL to open and
    /// launch arguments (`-name value` words, after the bundle id).
    pub async fn launch(
        &self,
        udid: &str,
        bundle_id: &str,
        url: Option<&str>,
        launch_args: &[String],
    ) -> Result<()> {
        check_bundle_id(bundle_id)?;
        let mut args = vec![
            "device",
            "process",
            "launch",
            "--device",
            udid,
            "--terminate-existing",
        ];
        if let Some(u) = url {
            if u.starts_with('-') {
                return Err(VelocityError::Config(format!("not a URL to open: {u:?}")));
            }
            args.extend(["--payload-url", u]);
        }
        args.push(bundle_id);
        // Without `--`, devicectl reads `-server` as its own short options
        // (`-s -e -r ...`) and fails, or a `-j` argument as its
        // `--json-output`. But devicectl passes the `--` on to the app, where
        // Foundation pairs it with the next word as a `-key value`: the
        // first real argument was lost (iPhone 12, devicectl 477.30). The
        // filler word takes that pairing instead, so every argument reaches
        // the app and `UserDefaults`; the app also sees `-- velocity`.
        if !launch_args.is_empty() {
            args.extend(["--", LAUNCH_ARGS_FILLER]);
            args.extend(launch_args.iter().map(String::as_str));
        }
        self.run_json(udid, &args, Duration::from_secs(60), false)
            .await
            .map(|_| ())
    }

    /// D-50: copy the device's crash reports (the `systemCrashLogs`
    /// domain) into the local directory `dest`. The domain root is named
    /// `.`; `/` is tried when that source is refused (not verified on
    /// hardware), never after any other failure.
    pub async fn copy_crash_logs(&self, udid: &str, dest: &std::path::Path) -> Result<()> {
        let dest = positional_path(&dest.to_string_lossy());
        let mut last = None;
        for source in [".", "/"] {
            let args = [
                "device",
                "copy",
                "from",
                "--device",
                udid,
                "--domain-type",
                "systemCrashLogs",
                "--source",
                source,
                "--destination",
                &dest,
            ];
            match self
                .run_json(udid, &args, Duration::from_secs(120), false)
                .await
            {
                Ok(_) => return Ok(()),
                // Only a refused source is worth the other spelling: a
                // timeout or a locked phone would just cost another 120 s
                // (review MINOR 12).
                Err(e) if source == "." && refused_source(&e) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("one attempt ran"))
    }
}

/// Whether a devicectl copy failed over its `--source` (a path the domain
/// doesn't have), not the device or the transfer.
fn refused_source(e: &VelocityError) -> bool {
    // devicectl's own stderr, not the command line (which holds `--source`).
    let VelocityError::CommandFailed { detail, .. } = e else {
        return false;
    };
    let msg = detail.to_ascii_lowercase();
    !msg.contains("timed out")
        && [
            "source",
            "no such file",
            "not found",
            "does not exist",
            "invalid path",
        ]
        .iter()
        .any(|k| msg.contains(k))
}

/// A bundle id as Apple allows it (letters, digits, `.`, `-`; `_` is
/// accepted too), never one devicectl would read as an option (VEL-293:
/// an app id like `--environment-variables` became a flag; there is no
/// shell, so argv is the only way in).
fn check_bundle_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(VelocityError::Config(format!(
            "not a valid iOS bundle id: {id:?}"
        )))
    }
}

/// A path that can't be read as an option: `-x.app` becomes `./-x.app`.
fn positional_path(path: &str) -> String {
    if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_string()
    }
}

/// Real devices from `devicectl list devices` JSON.
pub fn parse_devices(v: &serde_json::Value) -> Vec<DeviceInfo> {
    let Some(list) = v.pointer("/result/devices").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|d| {
            let hw = d.get("hardwareProperties")?;
            if hw.get("reality").and_then(|r| r.as_str()) != Some("physical") {
                return None;
            }
            if hw.get("platform").and_then(|p| p.as_str()) != Some("iOS") {
                return None;
            }
            let udid = hw.get("udid")?.as_str()?.to_string();
            let props = d.get("deviceProperties");
            let conn = d.get("connectionProperties");
            let str_at = |o: Option<&serde_json::Value>, k: &str| {
                o.and_then(|o| o.get(k))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            };
            // A Wi-Fi device that is asleep or out of reach keeps its
            // transport but has no tunnel (VEL-S31).
            let connected = str_at(conn, "transportType").is_some()
                && str_at(conn, "pairingState").as_deref() == Some("paired")
                && str_at(conn, "tunnelState").as_deref() != Some("unavailable");
            let developer_mode = str_at(props, "developerModeStatus").as_deref() == Some("enabled");
            // Without the Developer Disk Image's services mounted, the
            // runner launch fails later with a vaguer error (N9). Older
            // devicectl builds don't report the key: absent counts as ready.
            // The key means something only while the tunnel is up: an idle
            // device (`tunnelState: disconnected`, opened on first use)
            // reports `false` (seen on an iPhone 12, iOS 26.5.2, 2026-11-21).
            // Only a tunnel known to be idle voids it: an older devicectl
            // with no `tunnelState` still has its DDI check.
            let idle = str_at(conn, "tunnelState").as_deref() == Some("disconnected");
            let ddi = idle
                || props
                    .and_then(|p| p.get("ddiServicesAvailable"))
                    .and_then(|v| v.as_bool())
                    != Some(false);
            Some(DeviceInfo {
                id: udid,
                name: str_at(props, "name")
                    .or_else(|| str_at(Some(hw), "marketingName"))
                    .unwrap_or_else(|| "iOS device".into()),
                platform: Platform::Ios,
                state: if connected && developer_mode && ddi {
                    DeviceState::Booted
                } else {
                    DeviceState::Shutdown
                },
                os_version: str_at(props, "osVersionNumber"),
                device_type: DeviceType::Physical,
            })
        })
        .collect()
}

/// `url|version|bundleVersion` of `bundle_id` in `devicectl device info
/// apps` JSON.
fn parse_app_stamp(v: &serde_json::Value, bundle_id: &str) -> Option<String> {
    let app = v
        .pointer("/result/apps")?
        .as_array()?
        .iter()
        .find(|a| a.get("bundleIdentifier").and_then(|b| b.as_str()) == Some(bundle_id))?;
    let field = |k: &str| app.get(k).and_then(|x| x.as_str()).unwrap_or("");
    Some(format!(
        "{}|{}|{}",
        field("url"),
        field("version"),
        field("bundleVersion")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_installed_app_is_stamped_with_its_url_and_versions() {
        let v = json!({ "result": { "apps": [
            { "bundleIdentifier": "other.app", "url": "file:///x/Other.app/" },
            { "bundleIdentifier": "dev.velocity.demo", "url": "file:///private/var/containers/Bundle/Application/1234/Demo.app/",
              "version": "1.2", "bundleVersion": "42" }
        ]}});
        assert_eq!(
            parse_app_stamp(&v, "dev.velocity.demo").as_deref(),
            Some("file:///private/var/containers/Bundle/Application/1234/Demo.app/|1.2|42")
        );
        assert_eq!(parse_app_stamp(&v, "missing.app"), None);
        assert_eq!(
            parse_app_stamp(&json!({ "result": { "apps": [] } }), "x"),
            None
        );
    }

    #[test]
    fn only_connected_devices_with_developer_mode_are_ready() {
        let dev = |udid: &str, transport: Option<&str>, dev_mode: &str| {
            let mut conn = json!({ "pairingState": "paired", "tunnelState": "connected" });
            if let Some(t) = transport {
                conn["transportType"] = json!(t);
            }
            json!({
                "connectionProperties": conn,
                "deviceProperties": { "name": "Test iPhone", "osVersionNumber": "26.0", "developerModeStatus": dev_mode },
                "hardwareProperties": { "platform": "iOS", "reality": "physical", "udid": udid }
            })
        };
        let v = json!({ "result": { "devices": [
            dev("A", Some("wired"), "enabled"),
            dev("B", None, "enabled"),
            dev("C", Some("localNetwork"), "disabled"),
            {
                "connectionProperties": { "pairingState": "paired", "tunnelState": "connected", "transportType": "wired" },
                "deviceProperties": { "name": "No DDI", "developerModeStatus": "enabled", "ddiServicesAvailable": false },
                "hardwareProperties": { "platform": "iOS", "reality": "physical", "udid": "D" }
            },
            {
                "connectionProperties": { "pairingState": "paired", "tunnelState": "disconnected", "transportType": "wired" },
                "deviceProperties": { "name": "Idle", "developerModeStatus": "enabled", "ddiServicesAvailable": false },
                "hardwareProperties": { "platform": "iOS", "reality": "physical", "udid": "E" }
            },
            { "hardwareProperties": { "platform": "iOS", "reality": "virtual", "udid": "SIM" } }
        ]}});
        let devices = parse_devices(&v);
        assert_eq!(devices.len(), 5, "simulators come from simctl");
        let state = |id: &str| devices.iter().find(|d| d.id == id).unwrap().state;
        assert_eq!(state("A"), DeviceState::Booted);
        assert_eq!(state("B"), DeviceState::Shutdown, "not connected");
        assert_eq!(state("C"), DeviceState::Shutdown, "no Developer Mode");
        assert_eq!(state("D"), DeviceState::Shutdown, "no DDI services (N9)");
        assert_eq!(
            state("E"),
            DeviceState::Booted,
            "an idle tunnel's ddiServicesAvailable means nothing yet"
        );
        assert!(devices
            .iter()
            .all(|d| d.device_type == DeviceType::Physical));
    }

    /// A stub `xcrun` in a fresh directory: it logs its arguments to
    /// `args.log`, sets `$OUT` to the `--json-output` path, then runs `body`.
    fn stub(name: &str, body: &str) -> (std::path::PathBuf, Devicectl) {
        let dir = std::env::temp_dir().join(format!(
            "velocity-devicectl-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("xcrun");
        let text = format!(
            "echo \"$@\" >> '{log}'\nOUT=\"\"; prev=\"\"\nfor a in \"$@\"; do [ \"$prev\" = --json-output ] && OUT=\"$a\"; prev=\"$a\"; done\necho \"$OUT\" > '{out}'\n{body}",
            log = dir.join("args.log").display(),
            out = dir.join("out.path").display(),
        );
        crate::write_stub(&script, &text);
        let d = Devicectl::with_xcrun(script.to_str().unwrap());
        (dir, d)
    }

    #[tokio::test]
    async fn an_app_id_that_looks_like_an_option_never_reaches_devicectl() {
        // VEL-293: `--environment-variables` became a devicectl flag.
        let (dir, d) = stub("argv", "exit 0");
        let e = d
            .launch("UDID-1", "--environment-variables", None, &[])
            .await
            .unwrap_err();
        assert!(matches!(e, VelocityError::Config(_)), "{e:?}");
        assert!(d.uninstall("UDID-1", "-x").await.is_err());
        assert!(d.launch("UDID-1", "a.b", Some("-x"), &[]).await.is_err());
        assert!(!dir.join("args.log").exists(), "devicectl not run");
        d.launch("UDID-1", "dev.velocity.demo", Some("demo://x"), &[])
            .await
            .unwrap();
        d.install("UDID-1", "-odd.app").await.unwrap();
        d.launch(
            "UDID-1",
            "dev.velocity.demo",
            None,
            &["-server".into(), "https://x y".into()],
        )
        .await
        .unwrap();
        let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
        assert!(log.contains(" ./-odd.app "), "{log}");
        let launch = log.lines().last().unwrap();
        assert!(
            launch.contains("dev.velocity.demo --json-output ")
                && launch.ends_with(" -- velocity -server https://x y"),
            "launch arguments after `--`, past devicectl's own options: {launch}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D-50: the crash-log copy's command line.
    #[tokio::test]
    async fn crash_logs_are_copied_from_the_system_crash_logs_domain() {
        let (dir, d) = stub("crashcopy", "exit 0");
        let dest = dir.join("crash");
        d.copy_crash_logs("UDID-1", &dest).await.unwrap();
        let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(
            log.starts_with(&format!(
                "devicectl device copy from --device UDID-1 --domain-type systemCrashLogs \
                 --source . --destination {} --json-output ",
                dest.display()
            )),
            "{log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        // `.` refused: `/` is tried; both refused: the error.
        let (dir, d) = stub("crashcopy2", "echo 'bad source' >&2; exit 1");
        let e = d
            .copy_crash_logs("UDID-2", &dir.join("c"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("bad source"), "{e}");
        let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
        assert!(log.lines().nth(1).unwrap().contains("--source / "), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
        // Review MINOR 12: any other failure isn't retried with `/`.
        let (dir, d) = stub("crashcopy3", "echo 'the device disconnected' >&2; exit 1");
        assert!(d.copy_crash_logs("UDID-3", &dir.join("c")).await.is_err());
        let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_first_device_list_waits_for_a_cold_core_device() {
        // Review m12: CoreDeviceService's cold start (10-20 s) outlasted the
        // 10 s limit, so real devices vanished from the first run. Scaled
        // down: the first call may take 4 s, later ones 1 s.
        let devices = r#"{"result":{"devices":[{"connectionProperties":{"pairingState":"paired","tunnelState":"connected","transportType":"wired"},"deviceProperties":{"name":"Test iPhone","osVersionNumber":"26.0","developerModeStatus":"enabled"},"hardwareProperties":{"platform":"iOS","reality":"physical","udid":"A"}}]}}"#;
        let (dir, mut d) = stub("cold", &format!("sleep 2; echo '{devices}' > \"$OUT\""));
        d.list_timeouts = (Duration::from_secs(4), Duration::from_secs(1));
        let first = d.list_devices().await;
        assert_eq!(first.len(), 1, "the slow first list is waited for");
        let later = d.list_devices().await;
        assert!(later.is_empty(), "later lists keep the short limit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_cancelled_first_list_keeps_the_cold_allowance() {
        // Review d4 m3: the up-front `--device` check's own timeout cut the
        // first (30 s) call short, and every later call, the farm's
        // included, got only 10 s. Scaled down: 4 s first, 1 s later.
        let devices = r#"{"result":{"devices":[{"connectionProperties":{"pairingState":"paired","tunnelState":"connected","transportType":"wired"},"deviceProperties":{"name":"Test iPhone","osVersionNumber":"26.0","developerModeStatus":"enabled"},"hardwareProperties":{"platform":"iOS","reality":"physical","udid":"A"}}]}}"#;
        let (dir, mut d) = stub("cancel", &format!("sleep 2; echo '{devices}' > \"$OUT\""));
        d.list_timeouts = (Duration::from_secs(4), Duration::from_secs(1));
        let cut = tokio::time::timeout(Duration::from_millis(300), d.try_list_devices()).await;
        assert!(cut.is_err());
        let first = d.try_list_devices().await.unwrap();
        assert_eq!(
            first.len(),
            1,
            "the next call still waits for a cold CoreDevice"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_list_is_an_error_for_the_strict_caller() {
        let (dir, d) = stub("listfail", "echo 'CoreDevice broke' >&2; exit 1");
        assert!(d.try_list_devices().await.is_err());
        assert!(d.list_devices().await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_timed_out_devicectl_leaves_no_temp_file() {
        // VEL-294: the timeout returned before the file was removed.
        let (dir, d) = stub("timeout", "echo '{}' > \"$OUT\"; sleep 5");
        let e = d
            .run_json(
                "UDID-1",
                &["list", "devices"],
                Duration::from_millis(1500),
                true,
            )
            .await
            .unwrap_err();
        assert!(!matches!(e, VelocityError::Config(_)), "VEL-146: {e:?}");
        let out = std::fs::read_to_string(dir.join("out.path")).unwrap();
        assert!(!out.trim().is_empty());
        assert!(
            !std::path::Path::new(out.trim()).exists(),
            "{out} left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn success_without_json_is_success_and_failures_name_the_device() {
        // VEL-295: a successful install read as a failure; errors said
        // device ''.
        let (dir, d) = stub("nojson", "exit 0");
        d.install("UDID-1", "/tmp/App.app").await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (dir, d) = stub("fails", "echo 'no such app' >&2; exit 1");
        let e = d.install("UDID-2", "/tmp/App.app").await.unwrap_err();
        assert!(e.to_string().contains("UDID-2"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
        let (dir, d) = stub("locked", "echo 'The device is locked.' >&2; exit 1");
        let e = d.launch("UDID-3", "a.b", None, &[]).await.unwrap_err();
        assert!(matches!(e, VelocityError::DeviceLocked { .. }), "{e:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wifi_device_without_a_tunnel_is_not_ready() {
        // VEL-S31: an asleep Wi-Fi phone read as ready.
        let v = json!({ "result": { "devices": [{
            "connectionProperties": { "pairingState": "paired", "transportType": "localNetwork", "tunnelState": "unavailable" },
            "deviceProperties": { "name": "P", "developerModeStatus": "enabled" },
            "hardwareProperties": { "platform": "iOS", "reality": "physical", "udid": "W" }
        }]}});
        assert_eq!(parse_devices(&v)[0].state, DeviceState::Shutdown);
    }
}
