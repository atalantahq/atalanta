use std::process::Stdio;

use atalanta_common::{AtalantaError, DeviceInfo, DeviceState, DeviceType, Platform, Result};
use tokio::process::Command;
use tracing::debug;

/// Wrapper around ADB subprocess calls with optional command batching.
pub struct Adb {
    adb_path: String,
    /// Devices whose `uiautomator dump /dev/tty` printed no XML: they get
    /// the file-based dump from then on (one old device no longer turns
    /// `/dev/tty` off for every other one).
    no_dev_tty: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Android version per device, read once (`list_devices` runs every
    /// few seconds in a parallel run).
    os_versions: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

/// Remote adb servers from `ATALANTA_ADB_SERVERS` (`host:port,host:port`).
/// Their devices join the pool as `host:port/serial` (Marathon's
/// `adb_servers`): a farm machine runs `adb -a nodaemon server` and every
/// Atalanta on the network can lease its devices.
pub fn remote_servers() -> Vec<(String, u16)> {
    atalanta_common::env::var("ATALANTA_ADB_SERVERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| {
            let (host, port) = s.trim().rsplit_once(':')?;
            Some((host.to_string(), port.parse().ok()?))
        })
        .collect()
}

/// Whether `host:port` is this machine's own adb server, whose devices are
/// already listed without a prefix.
pub fn is_local_server(host: &str, port: u16) -> bool {
    let local_port = atalanta_common::env::var("ATALANTA_ADB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5037);
    let local_host = atalanta_common::env::var("ATALANTA_ADB_HOST").ok();
    port == local_port
        && (matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
            || local_host.as_deref() == Some(host))
}

/// `(host, port, serial)` for a remote device id `host:port/serial`.
pub fn split_remote(device_id: &str) -> Option<(&str, u16, &str)> {
    let (server, serial) = device_id.split_once('/')?;
    let (host, port) = server.rsplit_once(':')?;
    Some((host, port.parse().ok()?, serial))
}

/// The `adb` arguments that install `path` (VEL-317): one APK with
/// `install`, a directory of split APKs (base plus config splits) with
/// `install-multiple`. An app bundle (`.aab`) or an APK set (`.apks`) is not
/// something adb installs: that is a config error with the bundletool steps.
pub(crate) fn install_args(path: &str) -> Result<Vec<String>> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".aab") || lower.ends_with(".apks") {
        return Err(AtalantaError::Config(format!(
            "{path} is an app bundle, which adb can't install. Build the APKs for the device \
             with bundletool (`bundletool build-apks --bundle app.aab --output app.apks \
             --connected-device`), unzip the .apks, and point appPath at the folder of .apk \
             files (or at a universal APK: `--mode=universal`)."
        )));
    }
    let p = std::path::Path::new(path);
    if !p.is_dir() {
        return Ok(vec!["install".into(), "-r".into(), path.into()]);
    }
    let mut apks: Vec<String> = std::fs::read_dir(p)
        .map_err(|e| AtalantaError::Config(format!("{path}: {e}")))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|f| f.is_file() && f.extension().is_some_and(|x| x.eq_ignore_ascii_case("apk")))
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    apks.sort();
    match apks.len() {
        0 => Err(AtalantaError::Config(format!(
            "{path} is a directory with no .apk files in it"
        ))),
        1 => Ok(vec!["install".into(), "-r".into(), apks.remove(0)]),
        _ => Ok(["install-multiple".to_string(), "-r".to_string()]
            .into_iter()
            .chain(apks)
            .collect()),
    }
}

/// The adb arguments that select `device_id`: `-s serial`, plus `-H/-P`
/// for a device on a remote server.
pub fn target_args(device_id: &str) -> Vec<String> {
    match split_remote(device_id) {
        Some((host, port, serial)) => vec![
            "-H".into(),
            host.into(),
            "-P".into(),
            port.to_string(),
            "-s".into(),
            serial.into(),
        ],
        None => vec!["-s".into(), device_id.into()],
    }
}

/// Where the adb server serving `device_id` listens (for `adb forward`
/// ports: a remote server opens them on its own host).
pub fn server_host(device_id: &str) -> String {
    split_remote(device_id)
        .map(|(h, _, _)| h.to_string())
        .unwrap_or_else(|| "127.0.0.1".into())
}

impl Default for Adb {
    fn default() -> Self {
        Self::new()
    }
}

impl Adb {
    pub fn new() -> Self {
        Self::with_path(
            atalanta_common::env::var("ATALANTA_ADB_PATH").unwrap_or_else(|_| "adb".to_string()),
        )
    }

    /// A client that runs the `adb` at `adb_path`.
    pub fn with_path(adb_path: impl Into<String>) -> Self {
        Self {
            adb_path: adb_path.into(),
            no_dev_tty: Default::default(),
            os_versions: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Execute multiple shell commands in a single ADB invocation by
    /// joining them with `&&`. Returns combined stdout. Use for independent
    /// commands where ordering doesn't matter (e.g. pre-flight checks).
    pub async fn batch_shell(&self, device_id: &str, commands: &[&str]) -> Result<String> {
        if commands.is_empty() {
            return Ok(String::new());
        }
        if commands.len() == 1 {
            return self.run(device_id, &["shell", commands[0]]).await;
        }
        let script = commands.join(" && ");
        self.run(device_id, &["shell", &script]).await
    }

    pub async fn collect_resource_metrics(
        &self,
        device_id: &str,
        package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        crate::async_adb::validate_package_name(package)?;
        let output = self
            .batch_shell(
                device_id,
                &[
                    &format!("dumpsys meminfo {} --short", package),
                    &crate::async_adb::build_cpu_metrics_command(package),
                ],
            )
            .await?;

        let (meminfo_part, cpu_part) = output
            .split_once(crate::async_adb::CPU_SEPARATOR)
            .unwrap_or((&output, ""));

        let (java_heap, native_heap, total_pss) =
            crate::async_adb::parse_meminfo_short(meminfo_part);
        let cpu = crate::async_adb::parse_cpu_line(cpu_part);

        Ok((java_heap, native_heap, total_pss, cpu))
    }

    async fn run(&self, device_id: &str, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(target_args(device_id));
        cmd.args(args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Typed text may be a password, and a launch argument or a link a
        // token: neither goes to the log, as it doesn't to an error.
        debug!(
            device = device_id,
            command = %atalanta_common::redact::redact(&redact_typed_text(args)),
            "adb command"
        );

        let timeout = match args.first() {
            Some(&"install") | Some(&"install-multiple") | Some(&"push") | Some(&"pull") => {
                atalanta_common::process::LONG_TIMEOUT
            }
            _ => atalanta_common::process::default_timeout(),
        };
        let output = atalanta_common::process::output_with_timeout(&mut cmd, timeout)
            .await
            .map_err(|e| exec_failed(device_id, "Failed to execute adb", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(match classify_failure(device_id, args, &stderr) {
                AtalantaError::DeviceNotFound { id, .. } => AtalantaError::DeviceNotFound {
                    id,
                    available: self.serials().await,
                },
                e => e,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// The serials the local adb server lists (for a "not found" message).
    async fn serials(&self) -> Vec<String> {
        self.run_global(&["devices"])
            .await
            .map(|out| parse_devices(&out, "").into_iter().map(|d| d.id).collect())
            .unwrap_or_default()
    }

    /// `adb start-server` (the async client connects to the server itself).
    pub(crate) async fn start_server(&self) -> Result<()> {
        self.run_global(&["start-server"]).await.map(|_| ())
    }

    /// Public version of run for use by the driver.
    pub async fn run_device(&self, device_id: &str, args: &[&str]) -> Result<String> {
        self.run(device_id, args).await
    }

    async fn run_global(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = atalanta_common::process::output_with_timeout(
            &mut cmd,
            atalanta_common::process::default_timeout(),
        )
        .await
        .map_err(|e| AtalantaError::Config(format!("Failed to execute adb: {e}")))?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let output = self.run_global(&["devices", "-l"]).await?;
        let mut devices = parse_devices(&output, "");
        devices.extend(self.list_remote_devices().await);
        self.fill_os_versions(&mut devices).await;
        Ok(devices)
    }

    /// The devices of the local server and every `ATALANTA_ADB_SERVERS`
    /// one, with no Android versions; an error when any listing failed
    /// (adb exits 1, or prints no `List of devices attached` header), where
    /// [`list_devices`](Self::list_devices) reads it as "no devices". For
    /// the up-front `--device` check: an error there means "don't know".
    pub async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        let listing = |out: String, server: &str| {
            if out.contains("List of devices attached") {
                Ok(out)
            } else {
                Err(AtalantaError::Config(format!(
                    "adb devices ({server}) printed no device list"
                )))
            }
        };
        let mut devices = parse_devices(
            &listing(self.run_global(&["devices", "-l"]).await?, "local")?,
            "",
        );
        for (host, port) in remote_servers() {
            if is_local_server(&host, port) {
                continue;
            }
            let port_s = port.to_string();
            let out = self
                .run_global(&["-H", &host, "-P", &port_s, "devices", "-l"])
                .await?;
            devices.extend(parse_devices(
                &listing(out, &format!("{host}:{port}"))?,
                &format!("{host}:{port}/"),
            ));
        }
        Ok(devices)
    }

    /// Set each online device's Android version (`ro.build.version.release`),
    /// asking each device once.
    pub async fn fill_os_versions(&self, devices: &mut [DeviceInfo]) {
        for d in devices
            .iter_mut()
            .filter(|d| d.state == DeviceState::Booted)
        {
            let cached = self
                .os_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&d.id)
                .cloned();
            let version = match cached {
                Some(v) => Some(v),
                None => {
                    let v = self
                        .run(&d.id, &["shell", "getprop", "ro.build.version.release"])
                        .await
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if let Some(v) = &v {
                        self.os_versions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(d.id.clone(), v.clone());
                    }
                    v
                }
            };
            d.os_version = version;
        }
    }

    /// Devices on the `ATALANTA_ADB_SERVERS`, ids prefixed `host:port/`.
    /// An unreachable server is skipped (logged), not an error.
    pub async fn list_remote_devices(&self) -> Vec<DeviceInfo> {
        let mut all = Vec::new();
        for (host, port) in remote_servers() {
            // Listing the local server again would put each of its devices
            // in the pool twice, under two ids that lease separately.
            if is_local_server(&host, port) {
                tracing::debug!("adb server {host}:{port} is the local one; skipped");
                continue;
            }
            let port_s = port.to_string();
            match self
                .run_global(&["-H", &host, "-P", &port_s, "devices", "-l"])
                .await
            {
                Ok(out) => all.extend(parse_devices(&out, &format!("{host}:{port}/"))),
                Err(e) => tracing::warn!("adb server {host}:{port} unreachable: {e}"),
            }
        }
        all
    }
}

/// `adb devices -l` output → devices, ids prefixed with `prefix`.
fn parse_devices(output: &str, prefix: &str) -> Vec<DeviceInfo> {
    {
        let mut devices = Vec::new();

        for line in output.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }

            let id = parts[0].to_string();
            let state = match parts[1] {
                "device" => DeviceState::Booted,
                "offline" => DeviceState::Shutdown,
                _ => DeviceState::Unknown,
            };

            let mut name = id.clone();
            // Filled from `getprop` (see `Adb::fill_os_versions`); adb's
            // `transport_id:` used to be stored here, which is not a version.
            let os_version = None;
            let mut device_type = DeviceType::Emulator;
            for part in &parts[2..] {
                if let Some(model) = part.strip_prefix("model:") {
                    name = model.to_string();
                }
                if part.starts_with("usb:") {
                    device_type = DeviceType::Physical;
                }
            }

            devices.push(DeviceInfo {
                id: format!("{prefix}{id}"),
                name,
                platform: Platform::Android,
                state,
                os_version,
                device_type,
            });
        }

        devices
    }
}

impl Adb {
    pub async fn install_app(&self, device_id: &str, apk_path: &str) -> Result<()> {
        let args = install_args(apk_path)?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(device_id, &args).await?;
        Ok(())
    }

    pub async fn launch_app(
        &self,
        device_id: &str,
        package: &str,
        clear_state: bool,
    ) -> Result<()> {
        crate::shell::validate_app_id(package)?;
        if clear_state {
            self.run(
                device_id,
                &["shell", &crate::shell::cmd(&["pm", "clear", package])],
            )
            .await?;
        }
        // Launch the main activity via monkey (auto-detects launcher activity)
        self.run(device_id, &["shell", &monkey_launch(package)])
            .await?;
        Ok(())
    }

    pub async fn stop_app(&self, device_id: &str, package: &str) -> Result<()> {
        crate::shell::validate_app_id(package)?;
        self.run(
            device_id,
            &["shell", &crate::shell::cmd(&["am", "force-stop", package])],
        )
        .await?;
        Ok(())
    }

    pub async fn dump_hierarchy(&self, device_id: &str) -> Result<String> {
        // Try /dev/tty first (faster, single command) — skip if it printed
        // no XML on this device before.
        let tty = !self
            .no_dev_tty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(device_id);
        if tty {
            let output = self
                .run(device_id, &["shell", "uiautomator", "dump", "/dev/tty"])
                .await?;
            match tty_dump_xml(device_id, &output) {
                Ok(Some(xml)) => return Ok(xml),
                Ok(None) => {
                    debug!(device_id, "/dev/tty dump printed no XML; using a file");
                    self.no_dev_tty
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(device_id.to_string());
                }
                Err(e) => return Err(e),
            }
        }
        let output = self.run(device_id, &["shell", DUMP_SCRIPT]).await?;
        file_dump_xml(device_id, &output)
    }

    pub async fn tap(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        self.run(
            device_id,
            &["shell", "input", "tap", &x.to_string(), &y.to_string()],
        )
        .await?;
        Ok(())
    }

    /// Both taps in one device shell (VEL-S47, see [`crate::shell::double_tap_script`]).
    pub async fn double_tap(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        let out = self
            .run(
                device_id,
                &["shell", &crate::shell::double_tap_script(x, y)],
            )
            .await?;
        crate::shell::double_tap_result(device_id, &out)
    }

    pub async fn long_press(
        &self,
        device_id: &str,
        x: i32,
        y: i32,
        duration_ms: u64,
    ) -> Result<()> {
        self.run(
            device_id,
            &[
                "shell",
                "input",
                "swipe",
                &x.to_string(),
                &y.to_string(),
                &x.to_string(),
                &y.to_string(),
                &duration_ms.to_string(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn input_text(&self, device_id: &str, text: &str) -> Result<()> {
        // Nothing to type. `adb shell ""` would open an interactive shell.
        if text.is_empty() {
            return Ok(());
        }
        // Quoted, so typed text is never device shell code (VEL-026).
        let script = crate::shell::input_text_script(text).map_err(|e| on_device(e, device_id))?;
        self.run(device_id, &["shell", &script]).await?;
        Ok(())
    }

    pub async fn swipe(
        &self,
        device_id: &str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: u64,
    ) -> Result<()> {
        self.run(
            device_id,
            &[
                "shell",
                "input",
                "swipe",
                &x1.to_string(),
                &y1.to_string(),
                &x2.to_string(),
                &y2.to_string(),
                &duration_ms.to_string(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn press_key(&self, device_id: &str, keycode: u32) -> Result<()> {
        self.run(
            device_id,
            &["shell", "input", "keyevent", &keycode.to_string()],
        )
        .await?;
        Ok(())
    }

    pub async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(target_args(device_id));
        // exec-out is binary-safe (no PTY, no newline translation).
        cmd.args(["exec-out", "screencap", "-p"]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = atalanta_common::process::output_with_timeout(
            &mut cmd,
            atalanta_common::process::default_timeout(),
        )
        .await
        .map_err(|e| exec_failed(device_id, "Failed to take screenshot", e))?;

        screencap_result(
            device_id,
            output.status.success(),
            output.stdout,
            &output.stderr,
        )
    }

    pub async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)> {
        let output = self.run(device_id, &["shell", SCREEN_SIZE_CMD]).await?;
        Ok(parse_screen_size(&output))
    }
}

/// `wm size` plus the display rotation, in one shell call. Android 10+
/// prints `mDisplayRotation=ROTATION_90` in `dumpsys window displays`, older
/// ones `mCurrentRotation=1` / `mRotation=1`. `true`: no match is no error.
pub(crate) const SCREEN_SIZE_CMD: &str = "wm size; dumpsys window displays | grep -m1 -E 'mDisplayRotation=|mCurrentRotation=|mRotation=[0-9R]'; true";

/// Width and height of the screen as it's shown now. `wm size` reports the
/// natural (portrait) size; an override (`wm size 720x1280`) wins over the
/// physical size, and a 90°/270° rotation swaps them. (This used to return
/// the larger side twice "to cope with rotation", which put the centre of a
/// swipe off the side of the screen and mis-scaled screenshot masks.)
pub(crate) fn parse_screen_size(output: &str) -> (i32, i32) {
    let size = |prefix: &str| {
        output.lines().find_map(|l| {
            let (w, h) = l.trim().strip_prefix(prefix)?.trim().split_once('x')?;
            Some((w.trim().parse::<i32>().ok()?, h.trim().parse::<i32>().ok()?))
        })
    };
    let (w, h) = size("Override size:")
        .or_else(|| size("Physical size:"))
        .unwrap_or((1080, 2400));
    let rotated = rotation(output).is_some_and(|r| r % 2 == 1);
    if rotated {
        (h, w)
    } else {
        (w, h)
    }
}

/// Quarter turns from the first rotation field in `output`:
/// `mDisplayRotation=ROTATION_90`, `mCurrentRotation=1`, `mRotation=1`, or
/// `dumpsys input`'s `SurfaceOrientation: 1`.
fn rotation(output: &str) -> Option<u8> {
    let value = |v: &str| -> Option<u8> {
        let v = v.trim_start();
        let token: String = v
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        match token.strip_prefix("ROTATION_") {
            Some(deg) => Some((deg.parse::<u16>().ok()? / 90) as u8),
            None => token.parse().ok(),
        }
    };
    [
        "mDisplayRotation=",
        "mCurrentRotation=",
        "mRotation=",
        "SurfaceOrientation:",
    ]
    .iter()
    .find_map(|key| {
        output
            .lines()
            .find_map(|l| l.split_once(key).and_then(|(_, rest)| value(rest)))
    })
}

/// `uiautomator dump` to a file of its own (never one an earlier dump
/// left), then uiautomator's own output, a separator and the file. AOSP's
/// DumpCommand reports `ERROR: could not get idle state.` and `ERROR: null
/// root node` and still exits 0, and writes nothing then: reading a fixed
/// path after `&&` parsed an earlier step's screen as the live one
/// (VEL-334). `ATALANTA_DUMP_DIR` is only set by tests.
/// `umask 077` keeps the screen's text from other apps while the file
/// exists (the shell's umask is 0 on many builds; audit 2026-11 AND-5).
pub(crate) const DUMP_SCRIPT: &str =
    "umask 077; f=${ATALANTA_DUMP_DIR:-/data/local/tmp}/atalanta-dump-$$.xml; \
     rm -f $f; uiautomator dump $f 2>&1; echo ATALANTA-DUMP; cat $f 2>/dev/null; rm -f $f";

/// uiautomator's success line ("hierchary" is AOSP's spelling).
fn dump_succeeded(status: &str) -> bool {
    status.contains("hierchary dumped to:") || status.contains("hierarchy dumped to:")
}

/// The first `ERROR:` line uiautomator printed.
fn dump_error(status: &str) -> Option<&str> {
    // Only uiautomator's own words: a node's text in the XML that follows
    // can start a line with "Error:" too.
    let status = status.split("<?xml").next().unwrap_or("");
    status
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("ERROR:") || l.starts_with("Error:"))
}

/// The complete `<hierarchy>…</hierarchy>` document in `s` (from `<?xml`
/// when there is one), or `None` when it is missing or cut off.
fn complete_xml(s: &str) -> Option<&str> {
    let start = s.find("<?xml").or_else(|| s.find("<hierarchy"))?;
    let rest = &s[start..];
    let end = rest
        .rfind("</hierarchy>")
        .map(|i| i + "</hierarchy>".len())
        .or_else(|| {
            // `<hierarchy …/>`: an empty screen, still a whole document.
            let open = rest.find("<hierarchy")?;
            let close = open + rest[open..].find('>')?;
            (rest[..close].ends_with('/')).then_some(close + 1)
        })?;
    Some(&rest[..end])
}

fn dump_failed(device_id: &str, detail: String) -> AtalantaError {
    AtalantaError::CommandFailed {
        device_id: device_id.to_string(),
        command: "uiautomator dump".into(),
        detail,
    }
}

/// The output of `uiautomator dump /dev/tty`: the XML, `Ok(None)` when
/// this device prints none there (use a file), or uiautomator's error.
pub(crate) fn tty_dump_xml(device_id: &str, output: &str) -> Result<Option<String>> {
    if let Some(err) = dump_error(output) {
        return Err(dump_failed(device_id, err.to_string()));
    }
    Ok(complete_xml(output).map(String::from))
}

/// The output of [`DUMP_SCRIPT`]: the XML, or why there is none. Only a
/// dump uiautomator reported as written, holding a whole document, counts.
pub(crate) fn file_dump_xml(device_id: &str, output: &str) -> Result<String> {
    let Some((status, file)) = output.split_once("ATALANTA-DUMP") else {
        return Err(dump_failed(
            device_id,
            format!("unexpected output: {}", snippet(output)),
        ));
    };
    if let Some(err) = dump_error(status) {
        return Err(dump_failed(device_id, err.to_string()));
    }
    if !dump_succeeded(status) {
        return Err(dump_failed(
            device_id,
            format!("no dump was written: {}", snippet(status)),
        ));
    }
    complete_xml(file).map(String::from).ok_or_else(|| {
        dump_failed(
            device_id,
            format!("the dump is not a whole hierarchy: {}", snippet(file)),
        )
    })
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    let cut: String = s.chars().take(200).collect();
    if cut.len() < s.len() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// `monkey` starting `package`'s launcher activity, quoted.
pub(crate) fn monkey_launch(package: &str) -> String {
    crate::shell::cmd(&[
        "monkey",
        "-p",
        package,
        "-c",
        "android.intent.category.LAUNCHER",
        "1",
    ])
}

/// `adb` could not be run for `device_id`. An adb that is missing or not
/// executable (`ATALANTA_ADB_PATH` wrong, adb removed mid-run) is the
/// user's setup, a config error (exit 2) that no retry fixes; it used to be
/// reported as a lost connection (exit 3, transient), which the runner
/// retried and counted against the device. A timeout or any other failure is
/// still a lost connection.
pub(crate) fn exec_failed(device_id: &str, what: &str, e: std::io::Error) -> AtalantaError {
    use std::io::ErrorKind;
    if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) {
        return AtalantaError::Config(format!(
            "{what}: {e} (is adb installed? see ATALANTA_ADB_PATH)"
        ));
    }
    AtalantaError::AdbConnectionLost {
        device_id: device_id.to_string(),
        reason: format!("{what}: {e}"),
    }
}

/// `e` with `device_id` filled in where it was built without one.
pub(crate) fn on_device(e: AtalantaError, device_id: &str) -> AtalantaError {
    match e {
        AtalantaError::CommandFailed {
            device_id: d,
            command,
            detail,
        } if d.is_empty() => AtalantaError::CommandFailed {
            device_id: device_id.to_string(),
            command,
            detail,
        },
        e => e,
    }
}

/// Tell a broken connection from a command that ran and failed. Only the
/// former is transient: retrying `pm clear` on a missing package, or a
/// shell command with a non-zero exit, can't succeed and shouldn't count
/// toward the circuit breaker as a dead device. A serial adb doesn't know
/// is `DeviceNotFound`, and an unauthorized device (the RSA prompt was
/// never accepted) a config error: no retry fixes it (VEL-055).
/// `adb exec-out screencap -p`'s outcome. Only an adb-level failure
/// (offline, closed, no device) is a lost connection; `screencap` failing
/// on a healthy device ("Unable to take screenshot", a secure window, the
/// display off) or writing something that isn't a PNG is a capture
/// failure. All three used to be `AdbConnectionLost`: retried with session
/// restarts and counted toward the circuit breaker, so a device whose
/// screen can't be captured ended the suite as "unresponsive" (review 03
/// M1).
pub(crate) fn screencap_result(
    device_id: &str,
    success: bool,
    stdout: Vec<u8>,
    stderr: &[u8],
) -> Result<Vec<u8>> {
    if !success {
        let stderr = String::from_utf8_lossy(stderr);
        return Err(
            match classify_failure(device_id, &["exec-out", "screencap", "-p"], &stderr) {
                AtalantaError::CommandFailed { detail, .. } => AtalantaError::ScreenCaptureFailed {
                    device_id: device_id.to_string(),
                    detail: if detail.is_empty() {
                        "screencap exited with an error".to_string()
                    } else {
                        format!("screencap: {detail}")
                    },
                },
                other => other,
            },
        );
    }
    // Never rewrite the bytes: the PNG signature itself contains "\r\n".
    if !stdout.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(AtalantaError::ScreenCaptureFailed {
            device_id: device_id.to_string(),
            detail: if stdout.is_empty() {
                "screencap returned nothing (a secure window, or the display is off)".to_string()
            } else {
                format!("screencap did not return a PNG ({} bytes)", stdout.len())
            },
        });
    }
    Ok(stdout)
}

pub(crate) fn classify_failure(device_id: &str, args: &[&str], stderr: &str) -> AtalantaError {
    const CONNECTION: [&str; 6] = [
        "device offline",
        "closed",
        "protocol fault",
        "connection reset",
        "cannot connect",
        "no devices",
    ];
    let lower = stderr.to_ascii_lowercase();
    if let Some(e) = device_problem(device_id, &lower, stderr) {
        e
    } else if CONNECTION.iter().any(|m| lower.contains(m)) {
        AtalantaError::AdbConnectionLost {
            device_id: device_id.to_string(),
            reason: stderr.trim().to_string(),
        }
    } else {
        AtalantaError::CommandFailed {
            device_id: device_id.to_string(),
            command: redact_typed_text(args),
            detail: stderr.trim().to_string(),
        }
    }
}

/// `args` joined for an error message, with typed text (possibly a
/// password) replaced: an `input text` script keeps only its first word, so
/// the text never reaches reports, JUnit or MCP output.
pub(crate) fn redact_typed_text(args: &[&str]) -> String {
    args.iter()
        .map(|a| {
            if a.contains("input text") {
                "input text <redacted>"
            } else {
                a
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// adb's own "no such device" and "unauthorized" answers.
fn device_problem(device_id: &str, lower: &str, raw: &str) -> Option<AtalantaError> {
    // adb phrases it "device 'emulator-5554' not found", or "device not
    // found" over the wire.
    let missing = (lower.contains("device '") && lower.contains("' not found"))
        || lower.contains("device not found")
        || lower.contains("no devices/emulators found");
    if missing {
        return Some(AtalantaError::DeviceNotFound {
            id: device_id.to_string(),
            available: Vec::new(),
        });
    }
    // adb's own words ("error: device unauthorized.", "device 'x'
    // unauthorized"), not any shell command's stderr that mentions it.
    if lower.contains("device unauthorized")
        || (lower.contains("device '") && lower.contains("' unauthorized"))
    {
        return Some(AtalantaError::Config(format!(
            "device '{device_id}' is unauthorized: accept the \"Allow USB debugging\" (RSA key) \
             prompt on the device, or run `adb kill-server` and reconnect ({})",
            raw.trim()
        )));
    }
    None
}

/// Why the adb server refused to switch to `device_id`'s transport.
pub(crate) fn classify_transport(device_id: &str, reason: &str) -> AtalantaError {
    device_problem(device_id, &reason.to_ascii_lowercase(), reason).unwrap_or_else(|| {
        AtalantaError::AdbConnectionLost {
            device_id: device_id.to_string(),
            reason: format!("Transport switch failed: {reason}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_failures_are_not_connection_losses() {
        let e = classify_failure(
            "d",
            &["shell", "pm", "clear", "x"],
            "Error: package not found\n",
        );
        assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e}");
        assert!(!e.kind().is_transient());
        for msg in ["error: device offline", "error: closed"] {
            let e = classify_failure("d", &["shell", "true"], msg);
            assert!(
                matches!(e, AtalantaError::AdbConnectionLost { .. }),
                "{msg}"
            );
            assert!(e.kind().is_transient());
        }
    }

    /// A launch argument or a link's token is a secret too: the debug log
    /// of an adb command masks registered secrets and typed text.
    #[test]
    fn adb_commands_are_logged_without_secrets() {
        use crate::test_support::{capture_logs, FakeAdb};
        atalanta_common::redact::register_secret("adb-log-secret-4f81c9");
        let fake = FakeAdb::new("adb-log-secret");
        let (_, logs) = capture_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let adb = Adb::with_path(fake.path());
                adb.run_device(
                    "emulator-5554",
                    &[
                        "shell",
                        "am start -n com.x/.A --es token adb-log-secret-4f81c9",
                    ],
                )
                .await
                .unwrap();
                adb.input_text("emulator-5554", "typed-pass-77")
                    .await
                    .unwrap();
            })
        });
        assert!(logs.contains("adb command"), "{logs}");
        assert!(logs.contains("com.x/.A"), "{logs}");
        assert!(!logs.contains("adb-log-secret-4f81c9"), "{logs}");
        assert!(!logs.contains("typed-pass-77"), "{logs}");
    }

    /// A missing adb is the user's setup (exit 2), not a lost connection
    /// (exit 3, retried): `ATALANTA_ADB_PATH` pointing nowhere.
    #[tokio::test]
    async fn a_missing_adb_binary_is_a_config_error() {
        let adb = Adb::with_path("/nonexistent/atalanta-test-adb");
        let e = adb
            .run_device("emulator-5554", &["shell", "true"])
            .await
            .unwrap_err();
        assert!(matches!(e, AtalantaError::Config(_)), "{e}");
        assert_eq!(e.exit_code(), 2);
        assert!(!e.is_transient(), "{e}");
        let e = adb.screenshot("emulator-5554").await.unwrap_err();
        assert!(matches!(e, AtalantaError::Config(_)), "{e}");
        // A timeout stays a lost connection.
        let t = exec_failed(
            "d",
            "Failed to execute adb",
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out after 60.0s"),
        );
        assert!(matches!(t, AtalantaError::AdbConnectionLost { .. }), "{t}");
    }

    /// VEL-055: a serial adb doesn't know was a lost connection, and an
    /// unauthorized device a transient one.
    #[test]
    fn missing_and_unauthorized_devices_are_told_apart() {
        for msg in [
            "adb: device 'emulator-5590' not found",
            "error: no devices/emulators found",
        ] {
            let e = classify_failure("emulator-5590", &["shell", "true"], msg);
            assert!(
                matches!(&e, AtalantaError::DeviceNotFound { id, .. } if id == "emulator-5590"),
                "{msg}: {e}"
            );
        }
        let e = classify_failure("R58N", &["shell", "true"], "error: device unauthorized.\n");
        assert!(e.kind().is_permanent(), "{e}");
        assert!(e.to_string().contains("RSA"), "{e}");
        // NIT-9: a shell command's own "unauthorized" is not the device's.
        let e = classify_failure(
            "R58N",
            &["shell", "curl", "x"],
            "curl: (22) 401 Unauthorized\n",
        );
        assert!(!e.to_string().contains("RSA"), "{e}");
        // A shell command's own "not found" is the command's failure.
        let e = classify_failure(
            "d",
            &["shell", "pm", "clear", "x"],
            "Error: package not found",
        );
        assert!(matches!(e, AtalantaError::CommandFailed { .. }));
        assert!(matches!(
            classify_transport("d", "device 'd' not found"),
            AtalantaError::DeviceNotFound { .. }
        ));
        assert!(matches!(
            classify_transport("d", "device offline"),
            AtalantaError::AdbConnectionLost { .. }
        ));
    }
}

#[cfg(test)]
mod fake_adb_tests {
    use super::*;
    use crate::test_support::FakeAdb;

    /// Review 03 M1: only adb's own failures are a lost connection; a
    /// capture that fails on a healthy device is a capture failure.
    #[test]
    fn screencap_failures_are_classified() {
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        assert!(screencap_result("d", true, png.clone(), b"").is_ok());
        let kind = |r: Result<Vec<u8>>| match r.unwrap_err() {
            AtalantaError::ScreenCaptureFailed { .. } => "capture",
            AtalantaError::AdbConnectionLost { .. } => "lost",
            AtalantaError::DeviceNotFound { .. } => "missing",
            other => panic!("{other:?}"),
        };
        assert_eq!(
            kind(screencap_result("d", true, Vec::new(), b"")),
            "capture"
        );
        assert_eq!(
            kind(screencap_result("d", true, b"junk".to_vec(), b"")),
            "capture"
        );
        assert_eq!(
            kind(screencap_result(
                "d",
                false,
                Vec::new(),
                b"Unable to take screenshot"
            )),
            "capture"
        );
        assert_eq!(
            kind(screencap_result(
                "d",
                false,
                Vec::new(),
                b"error: device offline"
            )),
            "lost"
        );
        assert_eq!(
            kind(screencap_result(
                "d",
                false,
                Vec::new(),
                b"error: device 'd' not found"
            )),
            "missing"
        );
    }

    /// VEL-317: a folder of split APKs went to `adb install` (which fails),
    /// and a bundle got adb's generic error.
    #[tokio::test]
    async fn split_apks_use_install_multiple_and_bundles_are_refused() {
        let fake = FakeAdb::new("install-splits");
        let adb = Adb::with_path(fake.path());
        let splits = fake.dir.join("splits");
        std::fs::create_dir_all(&splits).unwrap();
        for f in ["base.apk", "split_config.arm64_v8a.apk", "notes.txt"] {
            std::fs::write(splits.join(f), b"x").unwrap();
        }
        adb.install_app("emulator-5554", &splits.to_string_lossy())
            .await
            .unwrap();
        let argv = fake
            .log()
            .lines()
            .find(|l| l.starts_with("ARGV:"))
            .unwrap()
            .to_string();
        assert!(
            argv.contains("[install-multiple][-r]")
                && argv.contains("base.apk]")
                && argv.contains("split_config.arm64_v8a.apk]")
                && !argv.contains("notes.txt"),
            "{argv}"
        );
        assert_eq!(
            install_args("/b/app.apk").unwrap(),
            ["install", "-r", "/b/app.apk"]
        );
        for bundle in ["/b/app.aab", "/b/app.APKS"] {
            let err = adb.install_app("emulator-5554", bundle).await.unwrap_err();
            assert!(
                matches!(err, AtalantaError::Config(ref m) if m.contains("bundletool")),
                "{err}"
            );
        }
        let empty = fake.dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(install_args(&empty.to_string_lossy()).is_err());
    }

    /// VEL-S47: the fallback double tap was two `adb shell input tap`
    /// processes, hundreds of ms apart: two single taps.
    #[tokio::test]
    async fn double_tap_is_one_shell_or_an_error() {
        let fake = FakeAdb::new("double-tap");
        let adb = Adb::with_path(fake.path());
        fake.stub("getprop", "echo 33");
        adb.double_tap("emulator-5554", 10, 20).await.unwrap();
        assert_eq!(
            fake.calls("input"),
            ["INPUT:[tap][10][20]", "INPUT:[tap][10][20]"]
        );
        let shells = fake
            .log()
            .lines()
            .filter(|l| l.starts_with("DEVICE_SH:"))
            .count();
        assert_eq!(shells, 1, "{}", fake.log());

        // Before Android 12 `input` is too slow for a double tap.
        let old = FakeAdb::new("double-tap-old");
        old.stub("getprop", "echo 30");
        let err = Adb::with_path(old.path())
            .double_tap("emulator-5554", 10, 20)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tree server"), "{err}");
        assert!(old.calls("input").is_empty());
    }

    /// VEL-026: the `input text` fallback ran `$`, backticks and newlines as
    /// device shell code.
    #[tokio::test]
    async fn input_text_fallback_is_not_shell_code() {
        let fake = FakeAdb::new("text");
        let marker = fake.dir.join("PWNED_TEXT");
        let adb = Adb::with_path(fake.path());
        let backtick = format!("a`touch${{IFS}}{}`b", marker.display());
        for c in ["price $HOME", &backtick, "line1\nline2"] {
            adb.input_text("emulator-5554", c).await.unwrap();
        }
        let typed = fake.calls("input");
        assert!(!marker.exists(), "backtick text ran `touch` on the device");
        assert!(
            typed.contains(&"INPUT:[text][price%s$HOME]".to_string()),
            "$HOME expanded: {typed:?}"
        );
        assert!(
            typed.contains(&format!(
                "INPUT:[text][a`touch${{IFS}}{}`b]",
                marker.display()
            )),
            "{typed:?}"
        );
        assert!(
            typed.contains(&"INPUT:[keyevent][66]".to_string())
                && typed.contains(&"INPUT:[text][line2]".to_string()),
            "newline must be Enter, line2 typed: {typed:?}"
        );
    }

    /// Empty text is nothing to type: no `adb shell ""`, which opens an
    /// interactive device shell (and hangs AsyncAdb until its deadline).
    #[tokio::test]
    async fn empty_text_runs_nothing() {
        let fake = FakeAdb::new("empty");
        let adb = Adb::with_path(fake.path());
        adb.input_text("emulator-5554", "").await.unwrap();
        assert!(!fake.log().contains("ARGV:"), "{}", fake.log());
    }

    /// A failing `input text` (a password field, say) doesn't put the
    /// text in the error.
    #[tokio::test]
    async fn typed_text_is_not_in_the_error() {
        let fake = FakeAdb::new("redact");
        fake.stub("input", "echo nope >&2; exit 1");
        let adb = Adb::with_path(fake.path());
        let e = adb
            .input_text("emulator-5554", "hunter2 secret")
            .await
            .unwrap_err();
        let AtalantaError::CommandFailed { command, .. } = &e else {
            panic!("{e}");
        };
        assert_eq!(command, "shell input text <redacted>");
        assert!(!e.to_string().contains("hunter2"), "{e}");
    }

    #[tokio::test]
    async fn non_ascii_text_without_the_server_is_an_error() {
        let fake = FakeAdb::new("unicode");
        let adb = Adb::with_path(fake.path());
        let e = adb.input_text("emulator-5554", "café").await.unwrap_err();
        assert!(
            matches!(&e, AtalantaError::CommandFailed { device_id, .. } if device_id == "emulator-5554"),
            "{e}"
        );
        assert!(fake.calls("input").is_empty(), "typed a partial text");
    }

    /// VEL-156: an appId reached `pm clear`, `monkey -p` and `am force-stop`
    /// unquoted.
    #[tokio::test]
    async fn app_ids_are_validated_and_quoted() {
        let fake = FakeAdb::new("appid");
        let marker = fake.dir.join("PWNED_APPID");
        let adb = Adb::with_path(fake.path());
        let evil = format!("com.x;touch {}", marker.display());
        assert!(adb.launch_app("emulator-5554", &evil, true).await.is_err());
        assert!(adb.stop_app("emulator-5554", &evil).await.is_err());
        assert!(!marker.exists(), "appId ran as shell code");
        assert!(!fake.log().contains("DEVICE_SH"), "{}", fake.log());

        adb.launch_app("emulator-5554", "com.atalanta.lab", true)
            .await
            .unwrap();
        adb.stop_app("emulator-5554", "com.atalanta.lab")
            .await
            .unwrap();
        assert_eq!(fake.calls("pm"), ["PM:[clear][com.atalanta.lab]"]);
        assert_eq!(fake.calls("am"), ["AM:[force-stop][com.atalanta.lab]"]);
        assert!(fake
            .log()
            .contains("DEVICE_SH:'pm' 'clear' 'com.atalanta.lab'"));
    }
}

#[cfg(test)]
mod dump_tests {
    use super::*;
    use crate::test_support::FakeAdb;

    const XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="Live screen" class="android.widget.TextView" bounds="[0,0][100,100]" /></hierarchy>"#;

    /// `uiautomator` that prints `on_tty` for `dump /dev/tty`, else runs
    /// `to_file` (with `$2` the file).
    fn device(name: &str, on_tty: &str, to_file: &str) -> FakeAdb {
        let fake = FakeAdb::new(name);
        fake.stub(
            "uiautomator",
            &format!("if [ \"$2\" = /dev/tty ]; then {on_tty}; exit 0; fi\n{to_file}\nexit 0"),
        );
        // The device's sdcard lives in the fake device dir.
        fake.stub(
            "cat",
            "for f in \"$@\"; do case \"$f\" in /sdcard/*) f=\"$DEV$f\";; esac; /bin/cat \"$f\" || exit 1; done",
        );
        fake
    }

    /// VEL-334: uiautomator prints `ERROR: could not get idle state.` and
    /// exits 0 on an animating screen; the dump used to `cat` the file an
    /// earlier dump left and return that old screen as the live one.
    #[tokio::test]
    async fn failed_uiautomator_dump_never_reads_stale_file() {
        let idle = "echo 'ERROR: could not get idle state.'";
        let fake = device("stale", idle, idle);
        let sdcard = fake.dir.join("device/sdcard");
        std::fs::create_dir_all(&sdcard).unwrap();
        std::fs::write(
            sdcard.join("atalanta_hierarchy.xml"),
            XML.replace("Live screen", "Order confirmed"),
        )
        .unwrap();
        let adb = Adb::with_path(fake.path());
        let e = adb.dump_hierarchy("emulator-5554").await.unwrap_err();
        assert!(
            matches!(&e, AtalantaError::CommandFailed { detail, .. } if detail.contains("idle state")),
            "{e}"
        );
        // Not even once /dev/tty is off for the device.
        adb.no_dev_tty
            .lock()
            .unwrap()
            .insert("emulator-5554".into());
        let e = adb.dump_hierarchy("emulator-5554").await.unwrap_err();
        assert!(!e.to_string().contains("Order confirmed"), "{e}");
        assert!(e.to_string().contains("idle state"), "{e}");
    }

    #[tokio::test]
    async fn truncated_dump_is_an_error() {
        let cut = &XML[..XML.len() - 20];
        let fake = device(
            "cut",
            ":",
            &format!(
                "printf '%s' {} > \"$2\"; echo \"UI hierchary dumped to: $2\"",
                crate::shell::quote(cut)
            ),
        );
        let adb = Adb::with_path(fake.path());
        let e = adb.dump_hierarchy("emulator-5554").await.unwrap_err();
        assert!(e.to_string().contains("not a whole hierarchy"), "{e}");
    }

    #[tokio::test]
    async fn a_good_file_dump_is_read_and_tty_is_off_per_device() {
        let fake = device(
            "good",
            ":",
            &format!(
                "printf '%s' {} > \"$2\"; echo \"UI hierchary dumped to: $2\"",
                crate::shell::quote(XML)
            ),
        );
        let adb = Adb::with_path(fake.path());
        assert_eq!(adb.dump_hierarchy("emulator-5554").await.unwrap(), XML);
        assert_eq!(adb.dump_hierarchy("emulator-5554").await.unwrap(), XML);
        let tty_dumps = |dev: &str| {
            fake.log()
                .lines()
                .filter(|l| l.contains("/dev/tty") && l.contains(dev))
                .count()
        };
        assert_eq!(tty_dumps("emulator-5554"), 1, "tty tried once per device");
        // The dump file is gone afterwards.
        let left: Vec<_> = std::fs::read_dir(fake.dir.join("device"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // Another device still gets the faster /dev/tty dump.
        adb.dump_hierarchy("emulator-5556").await.unwrap();
        assert_eq!(tty_dumps("emulator-5556"), 1);
    }

    #[test]
    fn tty_output_is_checked_too() {
        let ok = format!("{XML}UI hierchary dumped to: /dev/tty\n");
        assert_eq!(tty_dump_xml("d", &ok).unwrap().as_deref(), Some(XML));
        assert_eq!(tty_dump_xml("d", "").unwrap(), None);
        assert_eq!(tty_dump_xml("d", &XML[..50]).unwrap(), None);
        assert!(tty_dump_xml(
            "d",
            "ERROR: null root node returned by UiTestAutomationBridge.\n"
        )
        .is_err());
        let empty =
            r#"<?xml version='1.0' ?><hierarchy rotation="0"/>UI hierchary dumped to: /dev/tty"#;
        assert!(tty_dump_xml("d", empty).unwrap().unwrap().ends_with("/>"));
        // NIT-8: a node whose text has a line starting "Error:" is content.
        let shown = "<?xml version='1.0' ?><hierarchy rotation=\"0\"><node text=\"Pay\nError: card declined\" bounds=\"[0,0][1,1]\" /></hierarchy>";
        let out = format!("{shown}UI hierchary dumped to: /dev/tty\n");
        assert_eq!(tty_dump_xml("d", &out).unwrap().as_deref(), Some(shown));
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;

    #[test]
    fn remote_ids_route_to_their_server() {
        assert_eq!(target_args("emulator-5554"), ["-s", "emulator-5554"]);
        assert_eq!(
            target_args("farm1.local:5037/R58N123"),
            ["-H", "farm1.local", "-P", "5037", "-s", "R58N123"]
        );
        assert_eq!(server_host("farm1.local:5037/R58N123"), "farm1.local");
        assert_eq!(server_host("emulator-5554"), "127.0.0.1");
        // A local serial with a colon (adb over Wi-Fi) is not remote.
        assert_eq!(split_remote("192.168.1.5:5555"), None);
    }

    #[test]
    fn listed_remote_devices_carry_their_server() {
        let out = "List of devices attached\nemulator-5554 device product:x model:Pixel_8 transport_id:1\n";
        let d = parse_devices(out, "farm1:5037/");
        assert_eq!(d[0].id, "farm1:5037/emulator-5554");
        assert_eq!(d[0].name, "Pixel_8");
    }

    #[test]
    fn screen_size_is_width_by_height_in_the_current_rotation() {
        assert_eq!(
            parse_screen_size("Physical size: 1080x2400\n"),
            (1080, 2400)
        );
        assert_eq!(
            parse_screen_size("Physical size: 1080x2400\nOverride size: 720x1600\n"),
            (720, 1600)
        );
        assert_eq!(
            parse_screen_size("Physical size: 1080x2400\n      SurfaceOrientation: 1\n"),
            (2400, 1080)
        );
        assert_eq!(parse_screen_size("garbage"), (1080, 2400));
        // Android 14: dumpsys window displays (no SurfaceOrientation line).
        let a14 = |rot: &str| {
            format!("Physical size: 1080x2340\n  overrideConfig={{1.0 winConfig={{ mBounds=Rect(0, 0 - 1080, 2340) mDisplayRotation={rot} mWindowingMode=fullscreen mRotation={rot}}}\n")
        };
        assert_eq!(parse_screen_size(&a14("ROTATION_0")), (1080, 2340));
        assert_eq!(parse_screen_size(&a14("ROTATION_90")), (2340, 1080));
        assert_eq!(parse_screen_size(&a14("ROTATION_180")), (1080, 2340));
        assert_eq!(
            parse_screen_size("Physical size: 1080x2340\n  mCurrentRotation=3\n"),
            (2340, 1080)
        );
    }
}
