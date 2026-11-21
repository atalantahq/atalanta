use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;
use velocity_common::{DeviceInfo, DeviceState, Platform, Result, VelocityError};

/// Wrapper around `xcrun simctl` subprocess calls for iOS simulator management.
pub struct Simctl {
    xcrun_path: String,
}

impl Default for Simctl {
    fn default() -> Self {
        Self::new()
    }
}

/// A simctl that ran and failed. Only what the user can fix (an unknown
/// device, a bad argument or path) is a config error; anything else is the
/// device's trouble (VEL-146: every failure exited 2 and was never retried).
fn exit_error(args: &[&str], stderr: &str) -> VelocityError {
    let detail = stderr.trim().to_string();
    let lower = detail.to_ascii_lowercase();
    let users = [
        "invalid device",
        "unknown device",
        "invalid argument",
        "no such file",
        "unrecognized subcommand",
    ];
    if users.iter().any(|p| lower.contains(p)) {
        return VelocityError::Config(format!("simctl command failed: {detail}"));
    }
    VelocityError::CommandFailed {
        device_id: args.get(1).copied().unwrap_or_default().to_string(),
        command: format!("simctl {}", args.first().copied().unwrap_or_default()),
        detail,
    }
}

impl Simctl {
    pub fn new() -> Self {
        Self::with_xcrun(
            &velocity_common::env::var("VELOCITY_XCRUN_PATH")
                .unwrap_or_else(|_| "xcrun".to_string()),
        )
    }

    /// Run simctl through this `xcrun` (tests use a stub).
    pub fn with_xcrun(xcrun: &str) -> Self {
        Self {
            xcrun_path: xcrun.to_string(),
        }
    }

    /// The `xcrun` this wrapper runs.
    pub fn xcrun_path(&self) -> &str {
        &self.xcrun_path
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let timeout = match args.first() {
            Some(&"install") | Some(&"boot") | Some(&"erase") | Some(&"clone")
            | Some(&"bootstatus") | Some(&"create") => velocity_common::process::LONG_TIMEOUT,
            _ => velocity_common::process::default_timeout(),
        };
        self.run_for(args, timeout).await
    }

    async fn run_for(&self, args: &[&str], timeout: std::time::Duration) -> Result<Vec<u8>> {
        self.run_env(args, timeout, &[]).await
    }

    /// [`Self::run_for`] with extra environment for simctl itself
    /// (`SIMCTL_CHILD_*` reaches the launched app).
    async fn run_env(
        &self,
        args: &[&str],
        timeout: std::time::Duration,
        env: &[(String, String)],
    ) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.xcrun_path);
        cmd.arg("simctl");
        cmd.args(args);
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        debug!(args = ?args, "simctl command");

        let output = velocity_common::process::output_with_timeout(&mut cmd, timeout)
            .await
            .map_err(|e| {
                // A hung simctl is the device's trouble, not the config's.
                if e.kind() == std::io::ErrorKind::TimedOut {
                    VelocityError::CommandFailed {
                        device_id: args.get(1).copied().unwrap_or_default().to_string(),
                        command: format!("simctl {}", args.first().copied().unwrap_or_default()),
                        detail: format!("no answer within {}s", timeout.as_secs_f32()),
                    }
                } else {
                    VelocityError::Config(format!("Failed to execute xcrun simctl: {e}"))
                }
            })?;

        if !output.status.success() {
            return Err(exit_error(args, &String::from_utf8_lossy(&output.stderr)));
        }

        Ok(output.stdout)
    }

    async fn run_text(&self, args: &[&str]) -> Result<String> {
        let bytes = self.run(args).await?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    /// List all available simulator devices by parsing `xcrun simctl list devices --json`.
    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let output = self.run_text(&["list", "devices", "--json"]).await?;
        let parsed: serde_json::Value = serde_json::from_str(&output)
            .map_err(|e| VelocityError::Config(format!("Failed to parse simctl JSON: {e}")))?;

        let mut devices = Vec::new();
        let devices_obj = parsed
            .get("devices")
            .and_then(|d| d.as_object())
            .ok_or_else(|| {
                VelocityError::Config("Missing 'devices' key in simctl output".to_string())
            })?;

        for (runtime, device_list) in devices_obj {
            let os_version = parse_runtime_version(runtime);
            let entries = match device_list.as_array() {
                Some(arr) => arr,
                None => continue,
            };

            for entry in entries {
                let udid = entry
                    .get("udid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let state_str = entry
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                let is_available = entry
                    .get("isAvailable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !is_available {
                    continue;
                }

                let state = match state_str {
                    "Booted" => DeviceState::Booted,
                    "Shutdown" => DeviceState::Shutdown,
                    _ => DeviceState::Unknown,
                };

                devices.push(DeviceInfo {
                    id: udid,
                    name,
                    platform: Platform::Ios,
                    state,
                    os_version: os_version.clone(),
                    device_type: velocity_common::DeviceType::Simulator,
                });
            }
        }

        Ok(devices)
    }

    /// Boot a simulator device.
    pub async fn boot(&self, device_id: &str) -> Result<()> {
        debug!(device = device_id, "booting simulator");
        self.run_text(&["boot", device_id])
            .await
            .map_err(|e| VelocityError::DeviceBootFailed {
                id: device_id.to_string(),
                reason: format!("{e}"),
            })?;
        Ok(())
    }

    /// Shut down a simulator device.
    pub async fn shutdown(&self, device_id: &str) -> Result<()> {
        debug!(device = device_id, "shutting down simulator");
        self.run_text(&["shutdown", device_id]).await?;
        Ok(())
    }

    /// Install an app bundle onto a simulator.
    pub async fn install(&self, device_id: &str, app_path: &str) -> Result<()> {
        debug!(device = device_id, app = app_path, "installing app");
        self.run_text(&["install", device_id, app_path]).await?;
        Ok(())
    }

    /// Launch an app by bundle identifier.
    pub async fn launch(&self, device_id: &str, bundle_id: &str) -> Result<()> {
        self.launch_with_env(device_id, bundle_id, &[], &[]).await
    }

    /// Launch an app with `env` in its environment: simctl passes each
    /// `SIMCTL_CHILD_<name>` it is given to the app as `<name>`.
    pub async fn launch_with_env(
        &self,
        device_id: &str,
        bundle_id: &str,
        env: &[(String, String)],
        args: &[String],
    ) -> Result<()> {
        debug!(
            device = device_id,
            bundle = bundle_id,
            ?env,
            "launching app"
        );
        let child: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (format!("SIMCTL_CHILD_{k}"), v.clone()))
            .collect();
        // Launch arguments go after the bundle id (`-name value`); argv
        // items, never a shell line.
        let mut argv = vec!["launch", device_id, bundle_id];
        argv.extend(args.iter().map(String::as_str));
        self.run_env(&argv, velocity_common::process::default_timeout(), &child)
            .await?;
        Ok(())
    }

    /// Open a URL (deep link or universal link) on the simulator.
    pub async fn openurl(&self, device_id: &str, url: &str) -> Result<()> {
        debug!(device = device_id, url, "opening url");
        self.run_text(&["openurl", device_id, url]).await?;
        Ok(())
    }

    /// Terminate a running app by bundle identifier.
    /// Stop the app. Already stopped is success (like Android's
    /// `am force-stop`): simctl reports it as "found nothing to terminate".
    pub async fn terminate(&self, device_id: &str, bundle_id: &str) -> Result<()> {
        self.terminate_within(
            device_id,
            bundle_id,
            velocity_common::process::default_timeout(),
        )
        .await
    }

    /// [`Self::terminate`], killed after `timeout`.
    pub async fn terminate_within(
        &self,
        device_id: &str,
        bundle_id: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        debug!(device = device_id, bundle = bundle_id, "terminating app");
        match self
            .run_for(&["terminate", device_id, bundle_id], timeout)
            .await
            .map(|b| String::from_utf8_lossy(&b).to_string())
        {
            Err(e) if not_running(&e.to_string()) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Bundle ids of the apps installed on the simulator, test runners
    /// (`*.xctrunner`) left out.
    /// `defaults delete <domain>` inside the simulator, through cfprefsd so
    /// its cache can't restore what was deleted. `domain` may be a plist
    /// path without `.plist` (an app's container preferences).
    pub async fn defaults_delete(&self, device_id: &str, domain: &str) -> Result<()> {
        self.run(&["spawn", device_id, "defaults", "delete", domain])
            .await
            .map(|_| ())
    }

    /// `launchctl list` inside the simulator (its running jobs and apps).
    pub async fn launchctl_list(&self, device_id: &str) -> Result<String> {
        let out = self.run(&["spawn", device_id, "launchctl", "list"]).await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub async fn installed_bundles(&self, device_id: &str) -> Result<Vec<String>> {
        let plist = self.run_text(&["listapps", device_id]).await?;
        Ok(parse_listapps(&plist))
    }

    /// Uninstall an app from the simulator, removing all its data.
    pub async fn uninstall(&self, device_id: &str, bundle_id: &str) -> Result<()> {
        debug!(device = device_id, bundle = bundle_id, "uninstalling app");
        self.run_text(&["uninstall", device_id, bundle_id]).await?;
        Ok(())
    }

    /// Get the path to an app's data container on the simulator.
    pub async fn get_app_container(
        &self,
        device_id: &str,
        bundle_id: &str,
        container: &str,
    ) -> Result<String> {
        let output = self
            .run_text(&["get_app_container", device_id, bundle_id, container])
            .await?;
        Ok(output.trim().to_string())
    }

    /// Take a screenshot from the simulator, returning raw PNG bytes.
    /// Uses `xcrun simctl io screenshot -` to pipe output to stdout.
    /// The status bar arguments `screenshots.freezeStatusBar` sets (D-V4):
    /// Apple's marketing time, full battery and signal, Wi-Fi.
    pub fn status_bar_override_args(device_id: &str) -> Vec<&str> {
        vec![
            "status_bar",
            device_id,
            "override",
            "--time",
            "9:41",
            "--dataNetwork",
            "wifi",
            "--wifiMode",
            "active",
            "--wifiBars",
            "3",
            "--cellularMode",
            "active",
            "--cellularBars",
            "4",
            "--operatorName",
            "",
            "--batteryState",
            "charged",
            "--batteryLevel",
            "100",
        ]
    }

    /// Freeze the simulator's status bar.
    pub async fn status_bar_override(&self, device_id: &str) -> Result<()> {
        let args = Self::status_bar_override_args(device_id);
        self.run(&args).await.map(|_| ())
    }

    /// Show the simulator's real status bar again.
    pub async fn status_bar_clear(&self, device_id: &str) -> Result<()> {
        self.run(&["status_bar", device_id, "clear"])
            .await
            .map(|_| ())
    }

    pub async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        debug!(device = device_id, "taking screenshot via simctl");
        self.run(&["io", device_id, "screenshot", "--type=png", "-"])
            .await
    }

    /// `simctl list <what> --json` (`devices`, `devicetypes`, `runtimes`).
    pub async fn list_json(&self, what: &str) -> Result<serde_json::Value> {
        let output = self.run_text(&["list", what, "--json"]).await?;
        serde_json::from_str(&output)
            .map_err(|e| VelocityError::Config(format!("Failed to parse simctl JSON: {e}")))
    }

    /// Clone a shut-down simulator as `name`; returns the new UDID. A clone
    /// keeps the source's first-boot work and installed apps.
    pub async fn clone_device(&self, source: &str, name: &str) -> Result<String> {
        debug!(source, name, "cloning simulator");
        Ok(self
            .run_text(&["clone", source, name])
            .await?
            .trim()
            .to_string())
    }

    /// Create a simulator of `device_type` (identifier or name), on the
    /// newest runtime unless `runtime` is given; returns its UDID.
    pub async fn create(
        &self,
        name: &str,
        device_type: &str,
        runtime: Option<&str>,
    ) -> Result<String> {
        debug!(name, device_type, ?runtime, "creating simulator");
        let mut args = vec!["create", name, device_type];
        args.extend(runtime);
        Ok(self.run_text(&args).await?.trim().to_string())
    }

    /// Boot if needed, and wait until the simulator has finished booting
    /// (`simctl bootstatus -b`).
    pub async fn bootstatus(&self, device_id: &str) -> Result<()> {
        debug!(device = device_id, "waiting for boot");
        self.run_text(&["bootstatus", device_id, "-b"])
            .await
            .map_err(|e| VelocityError::DeviceBootFailed {
                id: device_id.to_string(),
                reason: format!("{e}"),
            })?;
        Ok(())
    }

    /// Delete a simulator and its data.
    pub async fn delete(&self, device_id: &str) -> Result<()> {
        debug!(device = device_id, "deleting simulator");
        self.run_text(&["delete", device_id]).await?;
        Ok(())
    }
}

/// Extract an iOS version string from a simctl runtime identifier.
/// e.g. "com.apple.CoreSimulator.SimRuntime.iOS-17-4" -> Some("17.4")
fn parse_runtime_version(runtime: &str) -> Option<String> {
    let suffix = runtime.rsplit('.').next()?;
    if !suffix.starts_with("iOS-") {
        return None;
    }
    let version = suffix.strip_prefix("iOS-")?;
    Some(version.replace('-', "."))
}

/// The top-level keys of `simctl listapps`, an old-style plist whose
/// entries open at one indent: `    "com.apple.Bridge" =     {`.
fn parse_listapps(plist: &str) -> Vec<String> {
    plist
        .lines()
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| {
            let (key, rest) = l.split_once('=')?;
            let key = key.trim().trim_matches('"');
            (rest.trim() == "{" && !key.ends_with(".xctrunner")).then(|| key.to_string())
        })
        .collect()
}

/// simctl's error for terminating an app that isn't running.
fn not_running(message: &str) -> bool {
    message.contains("found nothing to terminate")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listapps_yields_installed_bundles_without_test_runners() {
        let plist = "{\n    \"com.apple.Bridge\" =     {\n        ApplicationType = System;\n        GroupContainers =         {\n        };\n    };\n    \"dev.velocity.runner.xctrunner\" =     {\n    };\n    \"com.example.app\" =     {\n    };\n}\n";
        assert_eq!(
            parse_listapps(plist),
            ["com.apple.Bridge", "com.example.app"]
        );
    }

    #[test]
    fn stopping_a_stopped_app_is_not_an_error() {
        assert!(not_running(
            "simctl command failed: An error was encountered processing the command (domain=NSPOSIXErrorDomain, code=3):\nSimulator device failed to terminate com.apple.Preferences.\nfound nothing to terminate"
        ));
        assert!(!not_running("simctl command failed: Invalid device: x"));
    }

    #[test]
    fn test_parse_runtime_version() {
        assert_eq!(
            parse_runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-17-4"),
            Some("17.4".to_string())
        );
        assert_eq!(
            parse_runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-16-2"),
            Some("16.2".to_string())
        );
        assert_eq!(
            parse_runtime_version("com.apple.CoreSimulator.SimRuntime.tvOS-17-0"),
            None
        );
        assert_eq!(parse_runtime_version("garbage"), None);
    }

    #[test]
    fn test_parse_simctl_json() {
        let json = r#"{
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-17-4": [
                    {
                        "udid": "AAAA-BBBB-CCCC",
                        "name": "iPhone 15 Pro",
                        "state": "Booted",
                        "isAvailable": true
                    },
                    {
                        "udid": "DDDD-EEEE-FFFF",
                        "name": "iPhone SE",
                        "state": "Shutdown",
                        "isAvailable": true
                    },
                    {
                        "udid": "XXXX-YYYY-ZZZZ",
                        "name": "Unavailable Device",
                        "state": "Shutdown",
                        "isAvailable": false
                    }
                ]
            }
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let devices_obj = parsed["devices"].as_object().unwrap();

        let mut devices = Vec::new();
        for (runtime, device_list) in devices_obj {
            let os_version = parse_runtime_version(runtime);
            for entry in device_list.as_array().unwrap() {
                let is_available = entry["isAvailable"].as_bool().unwrap_or(false);
                if !is_available {
                    continue;
                }
                devices.push(DeviceInfo {
                    id: entry["udid"].as_str().unwrap().to_string(),
                    name: entry["name"].as_str().unwrap().to_string(),
                    platform: Platform::Ios,
                    state: match entry["state"].as_str().unwrap() {
                        "Booted" => DeviceState::Booted,
                        "Shutdown" => DeviceState::Shutdown,
                        _ => DeviceState::Unknown,
                    },
                    os_version: os_version.clone(),
                    device_type: velocity_common::DeviceType::Simulator,
                });
            }
        }

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "iPhone 15 Pro");
        assert_eq!(devices[0].state, DeviceState::Booted);
        assert_eq!(devices[0].os_version.as_deref(), Some("17.4"));
        assert_eq!(devices[1].name, "iPhone SE");
        assert_eq!(devices[1].state, DeviceState::Shutdown);
    }

    #[test]
    fn a_failing_simctl_is_a_device_error_unless_the_input_is_wrong() {
        // VEL-146: every non-zero exit was Config (exit 2, never retried).
        let e = super::exit_error(
            &["boot", "UDID"],
            "Unable to boot device in current state: Booted\n",
        );
        assert!(matches!(e, VelocityError::CommandFailed { .. }), "{e:?}");
        assert!(e.to_string().contains("UDID"), "{e}");
        let e = super::exit_error(&["boot", "nope"], "Invalid device: nope");
        assert!(matches!(e, VelocityError::Config(_)), "{e:?}");
    }
}
