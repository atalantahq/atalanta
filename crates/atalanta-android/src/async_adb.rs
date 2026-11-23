use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;
use velocity_common::{DeviceInfo, DeviceState, DeviceType, Platform, Result, VelocityError};

/// Native async ADB client that speaks the ADB wire protocol directly over TCP.
/// Connects to the ADB server on localhost:5037 instead of spawning subprocess.
pub struct AsyncAdb {
    host: String,
    port: u16,
    /// Host-side commands (install, push, emu, forward…) aren't shell
    /// commands; the `adb` client implements them correctly, so delegate.
    subprocess: crate::adb::Adb,
    /// Deadline for one request, connect to last byte (VEL-162: a hung
    /// shell used to hang the run).
    timeout: Duration,
    /// Devices whose adbd has no shell protocol v2 (Android 6 and older):
    /// plain `shell:`, without an exit status.
    no_shell_v2: Mutex<HashSet<String>>,
}

impl Default for AsyncAdb {
    fn default() -> Self {
        Self::new()
    }
}

/// The wire format limits a request to a 4-hex-digit length.
const MAX_REQUEST: usize = 0xffff;

/// A failed read or write on the adb server connection: the server or the
/// device went away, which is transient (VEL-163), not a config error.
fn lost(who: &str, what: &str, e: impl std::fmt::Display) -> VelocityError {
    VelocityError::AdbConnectionLost {
        device_id: who.to_string(),
        reason: format!("{what}: {e}"),
    }
}

impl AsyncAdb {
    pub fn new() -> Self {
        let host = velocity_common::env::var("VELOCITY_ADB_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = velocity_common::env::var("VELOCITY_ADB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5037);
        Self::with_server(host, port)
    }

    /// A client for the adb server at `host:port`.
    pub fn with_server(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            subprocess: crate::adb::Adb::new(),
            timeout: velocity_common::process::default_timeout(),
            no_shell_v2: Mutex::new(HashSet::new()),
        }
    }

    /// `fut` within the request deadline.
    async fn deadline<T>(
        &self,
        who: &str,
        what: &str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::time::timeout(self.timeout, fut)
            .await
            .unwrap_or_else(|_| {
                Err(lost(
                    who,
                    what,
                    format!("no answer within {} s", self.timeout.as_secs()),
                ))
            })
    }

    /// A connection to the adb server that serves `device_id` (a remote
    /// one for `host:port/serial` ids), and the device's serial there.
    async fn connect_for<'a>(&self, device_id: &'a str) -> Result<(TcpStream, &'a str)> {
        match crate::adb::split_remote(device_id) {
            Some((host, port, serial)) => {
                let addr = format!("{host}:{port}");
                let s = TcpStream::connect(&addr).await.map_err(|e| {
                    VelocityError::AdbConnectionLost {
                        device_id: device_id.to_string(),
                        reason: format!("adb server {addr}: {e}"),
                    }
                })?;
                Ok((s, serial))
            }
            None => Ok((self.connect(device_id).await?, device_id)),
        }
    }

    /// Open a fresh connection to the ADB server. The local default server
    /// is started when it isn't running, as the `adb` client does.
    async fn connect(&self, who: &str) -> Result<TcpStream> {
        let addr = format!("{}:{}", self.host, self.port);
        match TcpStream::connect(&addr).await {
            Ok(s) => Ok(s),
            Err(e) if crate::adb::is_local_server(&self.host, self.port) && self.port == 5037 => {
                debug!("adb server not running ({e}); starting it");
                let _ = self.subprocess.start_server().await;
                TcpStream::connect(&addr)
                    .await
                    .map_err(|e| lost(who, &format!("adb server at {addr} (is adb installed?)"), e))
            }
            Err(e) => Err(lost(who, &format!("adb server at {addr}"), e)),
        }
    }

    /// Send an ADB protocol message: 4-char hex length prefix + payload.
    async fn send_message(stream: &mut TcpStream, who: &str, payload: &str) -> Result<()> {
        // A longer one would wrap the length prefix and desync the server
        // (VEL-311).
        if payload.len() > MAX_REQUEST {
            return Err(VelocityError::CommandFailed {
                device_id: who.to_string(),
                command: payload.chars().take(60).collect(),
                detail: format!(
                    "the command is {} bytes; adb accepts at most {MAX_REQUEST}",
                    payload.len()
                ),
            });
        }
        let msg = format!("{:04x}{}", payload.len(), payload);
        stream
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| lost(who, "ADB write failed", e))
    }

    /// Read the 4-byte status response ("OKAY" or "FAIL").
    async fn read_status(stream: &mut TcpStream, who: &str) -> Result<bool> {
        let mut buf = [0u8; 4];
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| lost(who, "ADB read status failed", e))?;
        Ok(&buf == b"OKAY")
    }

    /// Read a length-prefixed response (4-char hex length + data).
    async fn read_length_prefixed(stream: &mut TcpStream, who: &str) -> Result<String> {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| lost(who, "ADB read length failed", e))?;
        let len_str = String::from_utf8_lossy(&len_buf);
        let len = usize::from_str_radix(&len_str, 16)
            .map_err(|e| lost(who, &format!("ADB invalid length '{len_str}'"), e))?;

        if len == 0 {
            return Ok(String::new());
        }

        let mut data = vec![0u8; len];
        stream
            .read_exact(&mut data)
            .await
            .map_err(|e| lost(who, "ADB read data failed", e))?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// Read all remaining data from the stream until EOF.
    async fn read_until_eof(stream: &mut TcpStream, who: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(64 * 1024);
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|e| lost(who, "ADB read to EOF failed", e))?;
        Ok(buf)
    }

    /// Execute an ADB host command (no device transport).
    async fn host_command(&self, command: &str) -> Result<String> {
        let who = "adb server";
        self.deadline(who, command, async {
            let mut stream = self.connect(who).await?;
            Self::send_message(&mut stream, who, command).await?;
            if !Self::read_status(&mut stream, who).await? {
                let error = Self::read_length_prefixed(&mut stream, who)
                    .await
                    .unwrap_or_default();
                return Err(lost(who, "ADB host command failed", error));
            }
            Self::read_length_prefixed(&mut stream, who).await
        })
        .await
    }

    /// A connection switched to `device_id`'s transport with `service`
    /// accepted: `Ok(None)` when the device refused the service (its
    /// message is logged).
    async fn open_service(&self, device_id: &str, service: &str) -> Result<Option<TcpStream>> {
        let (mut stream, serial) = self.connect_for(device_id).await?;
        Self::send_message(&mut stream, device_id, &format!("host:transport:{serial}")).await?;
        if !Self::read_status(&mut stream, device_id).await? {
            let error = Self::read_length_prefixed(&mut stream, device_id)
                .await
                .unwrap_or_default();
            return Err(crate::adb::classify_transport(device_id, &error));
        }
        Self::send_message(&mut stream, device_id, service).await?;
        if !Self::read_status(&mut stream, device_id).await? {
            let error = Self::read_length_prefixed(&mut stream, device_id)
                .await
                .unwrap_or_default();
            debug!(device_id, service, "device refused the service: {error}");
            return Ok(None);
        }
        Ok(Some(stream))
    }

    /// Execute a shell command on a device and return stdout as a string.
    /// A non-zero exit status is an error, as with the `adb` client
    /// (VEL-312); output that isn't UTF-8 is read lossily.
    async fn shell(&self, device_id: &str, command: &str) -> Result<String> {
        // Typed text, a launch argument or a link's token: not in the log.
        debug!(
            device = device_id,
            command = %velocity_common::redact::redact(&crate::adb::redact_typed_text(&[command])),
            "async adb shell"
        );
        self.deadline(device_id, "shell", async {
            let v2 = !self
                .no_shell_v2
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(device_id);
            if v2 {
                if let Some(mut stream) = self
                    .open_service(device_id, &format!("shell,v2,raw:{command}"))
                    .await?
                {
                    let (out, err, status) = read_shell_v2(&mut stream, device_id).await?;
                    if status != 0 {
                        let detail = if err.trim().is_empty() {
                            format!("exit status {status}")
                        } else {
                            err
                        };
                        return Err(crate::adb::classify_failure(
                            device_id,
                            &["shell", command],
                            &detail,
                        ));
                    }
                    return Ok(out);
                }
                self.no_shell_v2
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(device_id.to_string());
            }
            let mut stream = self
                .open_service(device_id, &format!("shell:{command}"))
                .await?
                .ok_or_else(|| lost(device_id, "shell", "the device refused the shell service"))?;
            let data = Self::read_until_eof(&mut stream, device_id).await?;
            Ok(String::from_utf8_lossy(&data).into_owned())
        })
        .await
    }

    /// Run a command through the `exec:` service and return its raw bytes.
    /// Unlike `shell:`, `exec:` never allocates a PTY, so binary output (a
    /// PNG) arrives byte-for-byte on every Android version.
    async fn shell_raw(&self, device_id: &str, command: &str) -> Result<Vec<u8>> {
        self.deadline(device_id, "exec", async {
            let mut stream = self
                .open_service(device_id, &format!("exec:{command}"))
                .await?
                .ok_or_else(|| lost(device_id, "exec", "the device refused the exec service"))?;
            Self::read_until_eof(&mut stream, device_id).await
        })
        .await
    }

    // ── Public API matching Adb interface ──

    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        let output = self.host_command("host:devices-long").await?;
        let mut devices = Vec::new();

        for line in output.lines() {
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
                id,
                name,
                platform: Platform::Android,
                state,
                os_version,
                device_type,
            });
        }

        devices.extend(self.subprocess.list_remote_devices().await);
        self.subprocess.fill_os_versions(&mut devices).await;
        Ok(devices)
    }

    pub async fn install_app(&self, device_id: &str, apk_path: &str) -> Result<()> {
        // `pm install` needs the APK on the device first; it was never pushed,
        // so this always failed. `adb install` pushes and installs.
        self.subprocess.install_app(device_id, apk_path).await
    }

    pub async fn launch_app(
        &self,
        device_id: &str,
        package: &str,
        clear_state: bool,
    ) -> Result<()> {
        crate::shell::validate_app_id(package)?;
        if clear_state {
            self.shell(device_id, &crate::shell::cmd(&["pm", "clear", package]))
                .await?;
        }
        self.shell(device_id, &crate::adb::monkey_launch(package))
            .await?;
        Ok(())
    }

    pub async fn stop_app(&self, device_id: &str, package: &str) -> Result<()> {
        crate::shell::validate_app_id(package)?;
        self.shell(
            device_id,
            &crate::shell::cmd(&["am", "force-stop", package]),
        )
        .await?;
        Ok(())
    }

    pub async fn dump_hierarchy(&self, device_id: &str) -> Result<String> {
        let output = self.shell(device_id, "uiautomator dump /dev/tty").await?;
        if let Some(xml) = crate::adb::tty_dump_xml(device_id, &output)? {
            return Ok(xml);
        }
        // Without a controlling terminal (the norm over the adb protocol on
        // recent Android) /dev/tty swallows the XML — dump to a file instead.
        let output = self.shell(device_id, crate::adb::DUMP_SCRIPT).await?;
        crate::adb::file_dump_xml(device_id, &output)
    }

    pub async fn tap(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        self.shell(device_id, &format!("input tap {x} {y}")).await?;
        Ok(())
    }

    /// Both taps in one device shell (VEL-S47, see [`crate::shell::double_tap_script`]).
    pub async fn double_tap(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        let out = self
            .shell(device_id, &crate::shell::double_tap_script(x, y))
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
        self.shell(
            device_id,
            &format!("input swipe {x} {y} {x} {y} {duration_ms}"),
        )
        .await?;
        Ok(())
    }

    pub async fn input_text(&self, device_id: &str, text: &str) -> Result<()> {
        // Nothing to type. An empty `shell,v2,raw:` waits on stdin until the
        // deadline, then reports a lost connection.
        if text.is_empty() {
            return Ok(());
        }
        let script = crate::shell::input_text_script(text)
            .map_err(|e| crate::adb::on_device(e, device_id))?;
        self.shell(device_id, &script).await?;
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
        self.shell(
            device_id,
            &format!("input swipe {x1} {y1} {x2} {y2} {duration_ms}"),
        )
        .await?;
        Ok(())
    }

    pub async fn press_key(&self, device_id: &str, keycode: u32) -> Result<()> {
        self.shell(device_id, &format!("input keyevent {keycode}"))
            .await?;
        Ok(())
    }

    pub async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        // Raw bytes, untouched: rewriting "\r\n" would corrupt the PNG
        // signature (89 50 4E 47 0D 0A 1A 0A) and any CRLF in the data.
        // Output that isn't a PNG is a capture failure, as on the
        // subprocess backend (review 03 M1), not a "screenshot" to decode.
        let bytes = self.shell_raw(device_id, "screencap -p").await?;
        crate::adb::screencap_result(device_id, true, bytes, b"")
    }

    pub async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)> {
        let output = self.shell(device_id, crate::adb::SCREEN_SIZE_CMD).await?;
        Ok(crate::adb::parse_screen_size(&output))
    }

    /// Run a device command (used by driver for boot_device wait).
    pub async fn run_device(&self, device_id: &str, args: &[&str]) -> Result<String> {
        if args.first() == Some(&"shell") {
            let cmd = args[1..].join(" ");
            return self.shell(device_id, &cmd).await;
        }
        // Host-side commands (`emu`, `install`, `uninstall`, `forward`,
        // `wait-for-device`…) aren't device shell commands — running them in
        // the shell silently did nothing or failed.
        self.subprocess.run_device(device_id, args).await
    }

    /// Execute multiple shell commands joined with &&.
    pub async fn batch_shell(&self, device_id: &str, commands: &[&str]) -> Result<String> {
        if commands.is_empty() {
            return Ok(String::new());
        }
        let script = commands.join(" && ");
        self.shell(device_id, &script).await
    }

    /// Collect resource metrics (heap, PSS, CPU) for a package in a single round-trip.
    /// Uses batch_shell to combine `dumpsys meminfo --short` and `top` into one TCP call.
    /// Returns (java_heap_kb, native_heap_kb, total_pss_kb, cpu_percent).
    pub async fn collect_resource_metrics(
        &self,
        device_id: &str,
        package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        validate_package_name(package)?;
        let output = self
            .batch_shell(
                device_id,
                &[
                    &format!("dumpsys meminfo {} --short", package),
                    &build_cpu_metrics_command(package),
                ],
            )
            .await?;

        let (meminfo_part, cpu_part) = output.split_once(CPU_SEPARATOR).unwrap_or((&output, ""));

        let (java_heap, native_heap, total_pss) = parse_meminfo_short(meminfo_part);
        let cpu = parse_cpu_line(cpu_part);

        Ok((java_heap, native_heap, total_pss, cpu))
    }
}

/// The shell protocol v2 stream up to the exit packet: (stdout, stderr,
/// exit status). Each packet is an id byte (1 stdout, 2 stderr, 3 exit), a
/// little-endian u32 length and the data.
async fn read_shell_v2(stream: &mut TcpStream, who: &str) -> Result<(String, String, u8)> {
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        let mut head = [0u8; 5];
        stream
            .read_exact(&mut head)
            .await
            .map_err(|e| lost(who, "shell ended without an exit status", e))?;
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > 1 << 20 {
            return Err(lost(who, "shell protocol", format!("{len} byte packet")));
        }
        let mut data = vec![0u8; len];
        stream
            .read_exact(&mut data)
            .await
            .map_err(|e| lost(who, "shell output cut off", e))?;
        match head[0] {
            1 => out.extend_from_slice(&data),
            2 => err.extend_from_slice(&data),
            3 => {
                let status = data.first().copied().unwrap_or(0);
                return Ok((
                    String::from_utf8_lossy(&out).into_owned(),
                    String::from_utf8_lossy(&err).into_owned(),
                    status,
                ));
            }
            _ => {}
        }
    }
}

pub(crate) const CPU_SEPARATOR: &str = "---VELOCITY_CPU_SEPARATOR---";

pub(crate) fn validate_package_name(package: &str) -> Result<()> {
    let is_valid = !package.is_empty()
        && package
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_');

    if is_valid {
        Ok(())
    } else {
        Err(VelocityError::Config(format!(
            "Invalid Android package name '{package}'"
        )))
    }
}

pub(crate) fn build_cpu_metrics_command(package: &str) -> String {
    format!(
        "echo '{CPU_SEPARATOR}' && {{ if top -b -n 1 -s 1 >/dev/null 2>&1; then top -b -n 1 -s 1; elif top -n 1 >/dev/null 2>&1; then top -n 1; elif ps -A -o %CPU,ARGS >/dev/null 2>&1; then ps -A -o %CPU,ARGS; else ps; fi; }} | grep -F \"{package}\" || true"
    )
}

/// The KB figure after `label` on `line`: the first number only. Android 10+
/// prints a second column after it (`Java Heap:  6540   19384` is Pss then
/// Rss; `TOTAL PSS: 37264  TOTAL RSS: 93944  TOTAL SWAP PSS: 1260` is three
/// figures on one line), which made the whole remainder fail to parse as one
/// number and every value read 0.
fn kb_after(line: &str, label: &str) -> Option<u64> {
    line.strip_prefix(label)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Parse `dumpsys meminfo <pkg> --short` output.
/// Returns (java_heap_kb, native_heap_kb, total_pss_kb).
pub(crate) fn parse_meminfo_short(output: &str) -> (u64, u64, u64) {
    let mut java_heap: u64 = 0;
    let mut native_heap: u64 = 0;
    let mut total_pss: u64 = 0;

    for line in output.lines() {
        let trimmed = line.trim();
        // Lines look like: "  Java Heap:     1234"
        //                   "  Native Heap:    567"
        //                   "  TOTAL PSS:     8901"
        //                   or in some formats: "  TOTAL:     8901"
        if let Some(kb) = kb_after(trimmed, "Java Heap:") {
            java_heap = kb;
        } else if let Some(kb) = kb_after(trimmed, "Native Heap:") {
            native_heap = kb;
        } else if let Some(kb) = kb_after(trimmed, "TOTAL PSS:") {
            total_pss = kb;
        } else if total_pss == 0 {
            if let Some(kb) = kb_after(trimmed, "TOTAL:") {
                total_pss = kb;
            }
        }
    }

    (java_heap, native_heap, total_pss)
}

/// Parse CPU% from a `top` (or `ps`) line for the target package. The
/// formats:
///
/// - toolbox `top -n 1` (Android 7 and older): a `12%` column;
/// - toybox `top` (8+): the column after the one-letter process state,
///   `1234 u0_a42 10 -10 1.2G 100M 50M S 12.5 3.2 0:01.00 com.example.app`;
/// - `ps -A -o %CPU,ARGS`: `12.5 com.example.app`.
///
/// Any other line reads as 0: scanning the line for a float that "looks like
/// CPU" returned the PID, the priority or the nice value.
pub(crate) fn parse_cpu_line(output: &str) -> f32 {
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let Some(cpu) = fields
            .iter()
            .find_map(|f| f.strip_suffix('%')?.parse::<f32>().ok())
        {
            return cpu;
        }
        let state = fields
            .iter()
            .position(|f| f.len() == 1 && f.bytes().all(|b| b.is_ascii_uppercase()));
        if let Some(cpu) = state
            .and_then(|i| fields.get(i + 1))
            .and_then(|f| f.parse::<f32>().ok())
        {
            return cpu;
        }
        if fields.len() >= 2 && fields[0].contains('.') {
            if let Ok(cpu) = fields[0].parse::<f32>() {
                return cpu;
            }
        }
    }
    0.0
}

#[cfg(test)]
mod fake_server_tests {
    use super::*;
    use std::sync::Arc;

    /// What the fake adb server does with a device service request.
    #[derive(Clone)]
    enum Serve {
        /// Refuse `host:transport` with this message.
        NoTransport(&'static str),
        /// Accept, then close without a byte (the server died).
        Die,
        /// Accept, then never answer.
        Hang,
        /// Shell v2: stdout, stderr, exit status.
        V2(Vec<u8>, &'static str, u8),
        /// Refuse `shell,v2`; answer plain `shell:` with this.
        OldShell(&'static str),
    }

    async fn read_msg(s: &mut TcpStream) -> Option<String> {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await.ok()?;
        let n = usize::from_str_radix(std::str::from_utf8(&len).ok()?, 16).ok()?;
        let mut b = vec![0u8; n];
        s.read_exact(&mut b).await.ok()?;
        Some(String::from_utf8_lossy(&b).into_owned())
    }

    async fn fail(s: &mut TcpStream, msg: &str) {
        let _ = s
            .write_all(format!("FAIL{:04x}{msg}", msg.len()).as_bytes())
            .await;
    }

    /// A fake adb server on 127.0.0.1; every request it reads is recorded.
    async fn server(serve: Serve) -> (AsyncAdb, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let (serve, log) = (serve.clone(), log.clone());
                tokio::spawn(async move {
                    let Some(transport) = read_msg(&mut s).await else {
                        return;
                    };
                    log.lock().unwrap().push(transport);
                    if let Serve::NoTransport(msg) = serve {
                        return fail(&mut s, msg).await;
                    }
                    let _ = s.write_all(b"OKAY").await;
                    let Some(service) = read_msg(&mut s).await else {
                        return;
                    };
                    log.lock().unwrap().push(service.clone());
                    match serve {
                        Serve::Die => {}
                        Serve::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                        Serve::V2(out, err, status) => {
                            let _ = s.write_all(b"OKAY").await;
                            let mut b = Vec::new();
                            for (id, data) in
                                [(1u8, out), (2, err.as_bytes().to_vec()), (3, vec![status])]
                            {
                                b.push(id);
                                b.extend_from_slice(&(data.len() as u32).to_le_bytes());
                                b.extend_from_slice(&data);
                            }
                            let _ = s.write_all(&b).await;
                        }
                        Serve::OldShell(out) if service.starts_with("shell:") => {
                            let _ = s.write_all(b"OKAY").await;
                            let _ = s.write_all(out.as_bytes()).await;
                        }
                        Serve::OldShell(_) => fail(&mut s, "closed").await,
                        Serve::NoTransport(_) => unreachable!(),
                    }
                });
            }
        });
        let mut adb = AsyncAdb::with_server("127.0.0.1", port);
        adb.timeout = Duration::from_millis(500);
        (adb, seen)
    }

    /// VEL-163: the adb server dying mid-command was a config error (exit 2).
    #[tokio::test]
    async fn adb_server_dying_mid_command_is_connection_lost() {
        let (adb, _) = server(Serve::Die).await;
        let e = adb
            .run_device("emulator-5554", &["shell", "true"])
            .await
            .unwrap_err();
        assert!(matches!(e, VelocityError::AdbConnectionLost { .. }), "{e}");
        assert!(e.is_transient());
        // No server at all: the same, never "Configuration error".
        let gone = AsyncAdb::with_server("127.0.0.1", 9);
        let e = gone.list_devices().await.unwrap_err();
        assert!(matches!(e, VelocityError::AdbConnectionLost { .. }), "{e}");
    }

    /// VEL-162: a shell that never answers hung the run.
    #[tokio::test]
    async fn a_hung_shell_times_out() {
        let (adb, _) = server(Serve::Hang).await;
        let started = std::time::Instant::now();
        let e = adb.screen_size("emulator-5554").await.unwrap_err();
        assert!(e.to_string().contains("no answer"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// VEL-311: a request of 64 KiB or more wrapped the length prefix.
    #[tokio::test]
    async fn an_oversized_request_is_refused_before_sending() {
        let (adb, seen) = server(Serve::V2(Vec::new(), "", 0)).await;
        let long = "x".repeat(70_000);
        let e = adb
            .batch_shell("emulator-5554", &[&long])
            .await
            .unwrap_err();
        assert!(matches!(e, VelocityError::CommandFailed { .. }), "{e}");
        assert!(
            !seen.lock().unwrap().iter().any(|m| m.contains("xxxx")),
            "the oversized request was sent"
        );
    }

    /// VEL-312: `shell:` dropped the exit status, so a failed command
    /// passed.
    #[tokio::test]
    async fn the_exit_status_is_checked() {
        let (adb, seen) = server(Serve::V2(b"partial".to_vec(), "boom", 1)).await;
        let e = adb
            .run_device("emulator-5554", &["shell", "false"])
            .await
            .unwrap_err();
        assert!(
            matches!(&e, VelocityError::CommandFailed { detail, .. } if detail == "boom"),
            "{e}"
        );
        assert_eq!(seen.lock().unwrap()[1], "shell,v2,raw:false");
        let (adb, _) = server(Serve::V2(b"ok\xff\n".to_vec(), "", 0)).await;
        // One invalid byte no longer fails the whole call.
        let out = adb
            .run_device("emulator-5554", &["shell", "x"])
            .await
            .unwrap();
        assert_eq!(out, "ok\u{fffd}\n");
    }

    #[tokio::test]
    async fn old_devices_fall_back_to_plain_shell() {
        let (adb, seen) = server(Serve::OldShell("hello")).await;
        assert_eq!(
            adb.run_device("d", &["shell", "echo"]).await.unwrap(),
            "hello"
        );
        assert_eq!(
            adb.run_device("d", &["shell", "echo"]).await.unwrap(),
            "hello"
        );
        let v2 = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.starts_with("shell,v2"))
            .count();
        assert_eq!(v2, 1, "v2 is tried once per device");
    }

    #[tokio::test]
    async fn missing_and_unauthorized_devices_over_the_wire() {
        let (adb, _) = server(Serve::NoTransport("device 'emulator-5590' not found")).await;
        let e = adb.tap("emulator-5590", 1, 2).await.unwrap_err();
        assert!(matches!(e, VelocityError::DeviceNotFound { .. }), "{e}");
        let (adb, _) = server(Serve::NoTransport("device unauthorized.")).await;
        let e = adb.tap("R58N", 1, 2).await.unwrap_err();
        assert!(e.kind().is_permanent(), "{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meminfo_short() {
        let output = r#"
           App Summary
                       Pss(KB)
                        ------
           Java Heap:     8432
         Native Heap:     3216
                Code:     1024
               Stack:      128
            Graphics:      512
       Private Other:      256
              System:     2048
             Unknown:      100

           TOTAL PSS:    15716
        "#;
        let (java, native, pss) = parse_meminfo_short(output);
        assert_eq!(java, 8432);
        assert_eq!(native, 3216);
        assert_eq!(pss, 15716);
    }

    #[test]
    fn test_parse_meminfo_short_total_fallback() {
        let output = "  Java Heap:     1000\n  Native Heap:     500\n  TOTAL:     2500\n";
        let (java, native, pss) = parse_meminfo_short(output);
        assert_eq!(java, 1000);
        assert_eq!(native, 500);
        assert_eq!(pss, 2500);
    }

    #[test]
    fn test_parse_cpu_line() {
        let output = "  1234  u0_a42  10  -10  1.2G  100M  S  12.5  com.example.app\n";
        assert_eq!(parse_cpu_line(output), 12.5);
    }

    /// Review (Nov 2026): the fallback scan returned the first number in
    /// 0..=100, which on every real `top` line is the priority or the PID.
    #[test]
    fn cpu_is_read_from_its_own_column_in_each_top_format() {
        // toybox `top` (Android 8+): ... SHR S %CPU %MEM TIME+ ARGS
        let toybox = "12345 u0_a123 10 -10 5.0G 100M 50M S 12.5 3.2 0:12.34 com.example.app";
        assert_eq!(parse_cpu_line(toybox), 12.5);
        // toolbox `top -n 1` (Android 7 and older): the `12%` column; the
        // priority (0) and a low PID come first.
        let toolbox = "42 0 12% S 45 1G 100M fg u0_a42 com.example.app";
        assert_eq!(parse_cpu_line(toolbox), 12.0);
        // `ps -A -o %CPU,ARGS`
        assert_eq!(parse_cpu_line("3.5 com.example.app\n"), 3.5);
        assert_eq!(parse_cpu_line("0.0 com.example.app --flag\n"), 0.0);
        // Plain `ps`: no CPU column at all, so 0, not the PID or the VSS.
        assert_eq!(
            parse_cpu_line("u0_a42 4321 567 1234 5678 0 0 S com.example.app"),
            0.0
        );
    }

    /// Android 10+ prints the Rss column after Pss, and the totals on one
    /// line: each figure is the first number after its label.
    #[test]
    fn meminfo_with_an_rss_column_still_reads() {
        let output = r#"
 App Summary
                       Pss(KB)                        Rss(KB)
                        ------                         ------
           Java Heap:     6540                          19384
         Native Heap:     5232                           7148
                Code:    10000                          20000

           TOTAL PSS:    37264            TOTAL RSS:    93944       TOTAL SWAP PSS:     1260
        "#;
        assert_eq!(parse_meminfo_short(output), (6540, 5232, 37264));
    }

    #[test]
    fn test_parse_cpu_line_empty() {
        assert_eq!(parse_cpu_line(""), 0.0);
        assert_eq!(parse_cpu_line("no matching output"), 0.0);
    }
}
