//! Client for the on-device UI-tree server (`probes/android/velocity-tree-server`).
//!
//! `uiautomator dump` starts a JVM, connects UiAutomation and waits for idle on
//! every call — about 2 s each on an API 34 emulator. The tree server holds one
//! UiAutomation connection and answers a dump in a few milliseconds, with
//! byte-identical XML, so the parser is shared.
//!
//! The server is a 4 KB dex jar embedded in this binary. On first use per
//! device it is pushed to `/data/local/tmp`, started with `app_process` as the
//! shell user, and reached through `adb forward`. Any failure tears the
//! session down and the caller falls back to `uiautomator dump`; after
//! [`MAX_FAILURES`] in a row the server is left alone for a while (2 s,
//! doubling to 60 s) and then tried again. Disable it with
//! `VELOCITY_ANDROID_TREE_SERVER=0`.
//!
//! Only one UiAutomation connection can exist per device, so while the server
//! runs `uiautomator dump` fails — tearing down always sends `QUIT` first.
//!
//! Since protocol 3 the same connection also injects input (taps, swipes,
//! keys, text), takes screenshots, lists windows (for occlusion checks) and
//! counts accessibility events, so a settle can wait on the device instead
//! of polling dumps. Each call returns `None` when the server is unavailable;
//! callers then use `adb shell`.
//!
//! Since protocol 13 the forwarded port (`127.0.0.1:<random>` on the host,
//! reachable by any web page's blind `fetch()`) is closed by the server on
//! the first line that isn't a command, and every connection must present
//! the server's session token (`AUTH`) before anything but `PING`/`QUIT`.
//! The server writes a fresh token to [`TOKEN_FILE`] on the device (shell
//! or root only) at each start; the host reads it through adb (R-2, R-3).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// The dex jar built by `probes/android/velocity-tree-server/build.sh`.
const SERVER_JAR: &[u8] = include_bytes!("../assets/velocity-tree.jar");
/// Must match `Server.VERSION` in the embedded jar.
const SERVER_VERSION: &str = "13";
/// The oldest protocol the host still speaks. Protocol 7 has no request
/// ids: a command that changes the device and loses its reply is never
/// resent. Protocol 8 tags those commands `#<id>`; the server answers a
/// repeated id from its reply cache, so after a reconnect to the same
/// server (same boot nonce) the host can resend safely. Protocol 9 adds
/// `hint`/`state-desc`/`error` to dumps and refuses a `SETTEXT a` on a
/// non-empty password field; nothing else changed, so 7 and 8 still work.
/// Protocol 10 adds the Compose `test-tag` and 11 the `role-desc` (S1)
/// to full dumps; older servers just don't send them. Protocol 12 adds
/// `ENCODERS` (the device's H.264 encoders) and ships the video server
/// (`com.velocity.tree.VideoServer`, see [`crate::video`]) in the same jar;
/// the tree commands are unchanged. Protocol 13 (R-2) closes the
/// connection on any line that isn't a command and requires `AUTH <token>`
/// first ([`AUTH_VERSION`]); the commands themselves are unchanged, so an
/// older server is still spoken to without it.
const OLDEST_VERSION: u32 = 7;
/// The first protocol whose server requires `AUTH <token>`.
const AUTH_VERSION: u32 = 13;
/// Where a protocol 13 server leaves its session token on the device:
/// readable by its own uid (shell or root) only.
const TOKEN_FILE: &str = "/data/local/tmp/velocity-tree.token";
/// Text sent per `TEXT` command: long strings typed as one burst of key
/// events overran the keyboard (Appium and Maestro chunk too).
const TEXT_CHUNK: usize = 256;
const SOCKET_NAME: &str = "velocity_tree";
/// Failures in a row before the server is left alone for a while.
const MAX_FAILURES: u32 = 2;
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// In every error for a command that was sent but not answered.
const OUTCOME_UNKNOWN: &str = "whether it ran is unknown";

/// Whether `msg` says a command that changes the device may have run: the
/// caller must not do it another way (adb, keys) as well.
pub fn is_outcome_unknown(msg: &str) -> bool {
    msg.contains(OUTCOME_UNKNOWN)
}

/// What [`TreeServer::start`] says when another tool holds UiAutomation.
const ANOTHER_CLIENT: &str = "another UI automation tool";

/// `VELOCITY_ANDROID_KEEP_A11Y=1`: don't suppress other accessibility
/// services. A server already running keeps the mode it started with.
fn keep_a11y() -> bool {
    matches!(
        velocity_common::env::var("VELOCITY_ANDROID_KEEP_A11Y").as_deref(),
        Ok("1") | Ok("true")
    )
}
const START_TIMEOUT: Duration = Duration::from_secs(8);
const DUMP_TIMEOUT: Duration = Duration::from_secs(10);

struct Session {
    stream: BufReader<TcpStream>,
    port: u16,
    /// The protocol this server speaks (from `PONG`).
    proto: u32,
    /// Protocol 8: the server's boot nonce. A reconnect that gets another
    /// one reached a restarted server, whose reply cache is empty.
    nonce: Option<String>,
    /// Protocol 13: the server's session token, presented again on every
    /// new connection (reconnects, resends).
    token: Option<String>,
    /// The `adb shell app_process …` process; `None` when we attached to a
    /// server left running by an earlier Velocity process.
    _child: Option<velocity_common::process::GroupChild>,
}

#[derive(Default)]
struct DeviceState {
    session: Option<Session>,
    /// Failures in a row (start, connection, timeout).
    failures: u32,
    /// After [`MAX_FAILURES`] in a row, the server is not tried again
    /// before this (exponential, [`BACKOFF_MIN`] to [`BACKOFF_MAX`]); the
    /// first success resets it. Two hiccups used to turn the server off for
    /// the rest of the process (VEL-161).
    retry_at: Option<Instant>,
    /// Another UiAutomation client holds the device: never retried.
    disabled: bool,
    /// A request was sent and its caller dropped before the reply came (a
    /// step timeout, a `select!`). The reply is still on its way down this
    /// connection, and the next command would read it as its own.
    in_flight: bool,
}

impl DeviceState {
    fn succeeded(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }

    fn failed(&mut self, device_id: &str, why: &str) {
        self.failures += 1;
        if why.contains(ANOTHER_CLIENT) {
            if !self.disabled {
                warn!(device_id, "tree server off for this device: {why}");
            }
            self.disabled = true;
            return;
        }
        if self.failures >= MAX_FAILURES {
            let delay = backoff(self.failures);
            self.retry_at = Some(Instant::now() + delay);
            warn!(
                device_id,
                "tree server unavailable ({why}); using adb, retrying in {} s",
                delay.as_secs()
            );
        } else {
            warn!(device_id, "tree server unavailable, using adb: {why}");
        }
    }
}

/// How long to leave the server alone after `failures` in a row.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(MAX_FAILURES).min(16);
    (BACKOFF_MIN * 2u32.pow(doublings)).min(BACKOFF_MAX)
}

/// Whether `cmd` changes the device (input, text, actions). Those are sent
/// at most once; reads (PING, DUMP, SHOT, WINDOWS, GEN, SETTLE, MARK) can
/// be resent after a reconnect.
fn mutates(cmd: &str) -> bool {
    matches!(
        verb(cmd),
        "TAP"
            | "TAPS"
            | "PRESS"
            | "SWIPE"
            | "KEY"
            | "TEXT"
            | "SETTEXT"
            | "IME"
            | "GLOBAL"
            | "SCROLLNODE"
    )
}

/// The command word of `cmd` (never its arguments, which may be typed text),
/// after any `#<id>` tag.
fn verb(cmd: &str) -> &str {
    cmd.split_whitespace()
        .find(|w| !w.starts_with('#'))
        .unwrap_or("")
}

/// A request id no other command (of this process or another one on the
/// same device) uses: a process-random prefix and a counter.
fn next_request_id() -> String {
    static PREFIX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let prefix = PREFIX.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        fnv1a(&[&nanos.to_le_bytes()[..], &std::process::id().to_le_bytes()].concat())
    });
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{prefix:x}-{n}")
}

/// What `SETTEXT` did to the focused field.
#[derive(Debug, Clone, PartialEq)]
pub enum SetText {
    /// The field holds the text.
    Applied,
    /// The field refused it and is unchanged.
    Refused,
    /// Protocol 8: applied, but the field reads back this instead (a mask
    /// reformatted it, or it was cut to the field's length). The text is
    /// in the field: typing it again would enter it twice (VEL-164).
    Differs(String),
}

/// `OK 1`, `OK 0` or `OK 2 <hex read-back>`.
fn parse_set_text(line: &str) -> Result<SetText, String> {
    let mut it = line.split_whitespace().skip(1);
    match (it.next(), it.next()) {
        (Some("1"), _) => Ok(SetText::Applied),
        (Some("0"), _) => Ok(SetText::Refused),
        (Some("2"), hex) => {
            let hex = hex.unwrap_or("");
            let bytes: Option<Vec<u8>> = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
                .collect();
            match bytes {
                Some(b) if hex.len() % 2 == 0 => {
                    Ok(SetText::Differs(String::from_utf8_lossy(&b).into_owned()))
                }
                _ => Err(format!("bad set text reply {line:?}")),
            }
        }
        _ => Err(format!("bad set text reply {line:?}")),
    }
}

/// `PONG <version>` (protocol 7) or `PONG <version> <nonce>` (8).
fn parse_pong(line: &str) -> Option<(String, Option<String>)> {
    let mut it = line.trim().strip_prefix("PONG ")?.split_whitespace();
    let version = it.next()?.to_string();
    Some((version, it.next().map(str::to_string)))
}

/// The full-dump command: `DUMP notag` skips the Compose testTag reads
/// (D-53) when `VELOCITY_ANDROID_COMPOSE_TESTTAG=0`. A protocol 9 server
/// reads `notag` as a plain full dump, which has no `test-tag` anyway.
fn dump_command() -> &'static str {
    if crate::parser::compose_test_tags() {
        "DUMP"
    } else {
        "DUMP notag"
    }
}

/// Whether the host can talk to a server that says `version`.
fn supported(version: &str) -> Option<u32> {
    let v: u32 = version.parse().ok()?;
    let newest: u32 = SERVER_VERSION.parse().ok()?;
    (OLDEST_VERSION..=newest).contains(&v).then_some(v)
}

/// The longest reply header (`OK <len>`, `ERR <message>`) read.
const MAX_HEADER: u64 = 256;

/// The largest payload accepted for `cmd`'s reply: a corrupt or hostile
/// length must not allocate the host out of memory (VEL-160).
fn payload_cap(cmd: &str) -> usize {
    match verb(cmd) {
        "DUMP" => 64 << 20,
        "SHOT" => 32 << 20,
        _ => 1 << 20,
    }
}

/// What [`TreeServer::probe`] learned.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// The jar's path on the device.
    pub jar: String,
    /// The protocol the server answered PING with; `None` when no server
    /// was running and none was started.
    pub version: Option<String>,
    /// A server was already running (another Velocity's, or a kept one).
    pub attached: bool,
}

pub struct TreeServer {
    adb_path: String,
    enabled: bool,
    /// One lock per device: a slow command (a dump, a long settle) on one
    /// device never holds up another's. The outer lock is held only to find
    /// or add a device's slot.
    devices: std::sync::Mutex<HashMap<String, Arc<Mutex<DeviceState>>>>,
}

impl Default for TreeServer {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeServer {
    pub fn new() -> Self {
        let enabled = !matches!(
            velocity_common::env::var("VELOCITY_ANDROID_TREE_SERVER").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        );
        Self {
            adb_path: velocity_common::env::var("VELOCITY_ADB_PATH")
                .unwrap_or_else(|_| "adb".to_string()),
            enabled,
            devices: Default::default(),
        }
    }

    /// Run this `adb` instead of `VELOCITY_ADB_PATH` / `adb`.
    pub fn with_adb_path(mut self, adb_path: impl Into<String>) -> Self {
        self.adb_path = adb_path.into();
        self
    }

    /// A tree server that is never used (tests).
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self {
            adb_path: "adb".into(),
            enabled: false,
            devices: Default::default(),
        }
    }

    fn slot(&self, device_id: &str) -> Arc<Mutex<DeviceState>> {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    /// The current hierarchy XML from the server, or `None` if the server is
    /// unavailable (the caller then uses `uiautomator dump`).
    pub async fn dump(&self, device_id: &str) -> Option<String> {
        self.text_payload(device_id, dump_command()).await
    }

    /// A dump with only the attributes a settle compares (A-10): smaller,
    /// so cheaper to build, send and parse. Not for element lookups.
    pub async fn dump_lite(&self, device_id: &str) -> Option<String> {
        self.text_payload(device_id, "DUMP lite").await
    }

    /// Like [`Self::dump`], but tells "the server answered with an error"
    /// (`Some(Err)`) from "no server" (`None`).
    pub async fn dump_checked(
        &self,
        device_id: &str,
    ) -> Option<std::result::Result<String, String>> {
        match self
            .call(device_id, dump_command(), true, DUMP_TIMEOUT)
            .await?
        {
            Ok((_, bytes)) => Some(String::from_utf8(bytes).map_err(|e| e.to_string())),
            Err(e) => Some(Err(e.to_string())),
        }
    }

    async fn text_payload(&self, device_id: &str, cmd: &str) -> Option<String> {
        match self.call(device_id, cmd, true, DUMP_TIMEOUT).await? {
            Ok((_, bytes)) => String::from_utf8(bytes).ok(),
            Err(e) => {
                debug!(device_id, "tree server {cmd}: {e}");
                None
            }
        }
    }

    /// Tap at (x, y), `n` times (a double tap is 2).
    pub async fn taps(
        &self,
        device_id: &str,
        x: i32,
        y: i32,
        n: u32,
    ) -> Option<Result<(), String>> {
        self.unit(device_id, &format!("TAPS {x} {y} {n}"), DUMP_TIMEOUT)
            .await
    }

    pub async fn long_press(
        &self,
        device_id: &str,
        x: i32,
        y: i32,
        ms: u64,
    ) -> Option<Result<(), String>> {
        self.unit(
            device_id,
            &format!("PRESS {x} {y} {ms}"),
            DUMP_TIMEOUT + Duration::from_millis(ms),
        )
        .await
    }

    pub async fn swipe(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        ms: u64,
    ) -> Option<Result<(), String>> {
        let cmd = format!("SWIPE {} {} {} {} {ms}", from.0, from.1, to.0, to.1);
        self.unit(device_id, &cmd, DUMP_TIMEOUT + Duration::from_millis(ms))
            .await
    }

    pub async fn key(&self, device_id: &str, code: u32) -> Option<Result<(), String>> {
        self.unit(device_id, &format!("KEY {code}"), DUMP_TIMEOUT)
            .await
    }

    /// A command answered `OK <n>`: `Some(Ok(n == 1))`.
    async fn flag(&self, device_id: &str, cmd: &str) -> Option<Result<bool, String>> {
        Some(
            self.call(device_id, cmd, false, DUMP_TIMEOUT)
                .await?
                .map(|(line, _)| line.split_whitespace().nth(1) == Some("1")),
        )
    }

    /// Set the focused field's text in one action (`append`: after what is
    /// there). A protocol 7 server says `Refused` both when the field
    /// refused it and when it reads back differently.
    pub async fn set_text(
        &self,
        device_id: &str,
        text: &str,
        append: bool,
    ) -> Option<Result<SetText, String>> {
        let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
        let mode = if append { "a" } else { "r" };
        Some(
            self.call(
                device_id,
                &format!("SETTEXT {mode} {hex}"),
                false,
                DUMP_TIMEOUT,
            )
            .await?
            .and_then(|(line, _)| parse_set_text(&line)),
        )
    }

    /// The focused field's keyboard action (Search/Go/Send/Done), API 30+.
    pub async fn ime_enter(&self, device_id: &str) -> Option<Result<bool, String>> {
        self.flag(device_id, "IME").await
    }

    /// `UiAutomation.performGlobalAction` (`GLOBAL_ACTION_*`).
    pub async fn global(&self, device_id: &str, action: u32) -> Option<Result<bool, String>> {
        self.flag(device_id, &format!("GLOBAL {action}")).await
    }

    /// Scroll the list under (x, y) by its own action, along the vertical
    /// or horizontal axis (VEL-S48: a protocol 8 server skips a list that
    /// scrolls the other way; protocol 7 ignores the axis). `Ok(false)`: no
    /// scrollable there, or it can't move further that way.
    pub async fn scroll_node(
        &self,
        device_id: &str,
        x: i32,
        y: i32,
        forward: bool,
        vertical: bool,
    ) -> Option<Result<bool, String>> {
        let dir = if forward { 1 } else { -1 };
        let axis = if vertical { "v" } else { "h" };
        self.flag(device_id, &format!("SCROLLNODE {x} {y} {dir} {axis}"))
            .await
    }

    /// Type `text` into the focused field (any Unicode), [`TEXT_CHUNK`]
    /// characters per command, each with a deadline that grows with its
    /// length. `None` only when nothing was typed: once a chunk went in,
    /// the server going away is an error, not a cue to type it all again
    /// over adb.
    pub async fn text(&self, device_id: &str, text: &str) -> Option<Result<(), String>> {
        let chars: Vec<char> = text.chars().collect();
        let mut typed = 0;
        for chunk in chars.chunks(TEXT_CHUNK) {
            let chunk: String = chunk.iter().collect();
            let hex: String = chunk.bytes().map(|b| format!("{b:02x}")).collect();
            let timeout = DUMP_TIMEOUT + Duration::from_millis(20 * chunk.chars().count() as u64);
            match self.unit(device_id, &format!("TEXT {hex}"), timeout).await {
                Some(Ok(())) => typed += chunk.chars().count(),
                Some(Err(e)) if typed > 0 => {
                    return Some(Err(format!(
                        "{e} (after {typed} of {} characters were typed)",
                        chars.len()
                    )))
                }
                None if typed > 0 => {
                    return Some(Err(format!(
                        "tree server went away after {typed} of {} characters were typed; \
                         the rest is not typed another way",
                        chars.len()
                    )))
                }
                other => return other,
            }
        }
        Some(Ok(()))
    }

    /// A PNG (or JPEG) screenshot taken by the server: no `screencap` process.
    pub async fn screenshot(&self, device_id: &str, jpeg: bool) -> Option<Result<Vec<u8>, String>> {
        let cmd = if jpeg { "SHOT jpeg 80" } else { "SHOT png" };
        Some(
            self.call(device_id, cmd, true, DUMP_TIMEOUT)
                .await?
                .map(|(_, b)| b),
        )
    }

    /// Windows on screen, front first (A-7).
    /// Protocol 12: the device's H.264 encoders, `(name, hardware)`.
    pub async fn encoders(
        &self,
        device_id: &str,
    ) -> Option<Result<Vec<(String, Option<bool>)>, String>> {
        Some(
            self.call(device_id, "ENCODERS", true, DUMP_TIMEOUT)
                .await?
                .map(|(_, bytes)| parse_encoders(&String::from_utf8_lossy(&bytes))),
        )
    }

    pub async fn windows(&self, device_id: &str) -> Option<Result<Vec<Window>, String>> {
        Some(
            self.call(device_id, "WINDOWS", true, DUMP_TIMEOUT)
                .await?
                .map(|(_, b)| parse_windows(&String::from_utf8_lossy(&b))),
        )
    }

    /// Remember now as the start of an action (inputs through the server
    /// mark themselves; `adb shell input` ones need this). Only if the
    /// server already runs: starting it just to mark would delay the action
    /// by its startup (~100 ms), and a fresh server's mark is its start.
    pub async fn mark(&self, device_id: &str) -> Option<Result<(), String>> {
        let slot = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned();
        let running = match slot {
            Some(slot) => slot.lock().await.session.is_some(),
            None => false,
        };
        if !running {
            return None;
        }
        self.unit(device_id, "MARK", DUMP_TIMEOUT).await
    }

    /// Wait on the device for the screen to settle after the last action,
    /// from accessibility events (A-4): `(settled, reacted, elapsed ms)`.
    pub async fn settle(
        &self,
        device_id: &str,
        reaction_ms: u64,
        quiet_ms: u64,
        cap_ms: u64,
    ) -> Option<Result<(bool, bool, u64), String>> {
        let cmd = format!("SETTLE {reaction_ms} {quiet_ms} {cap_ms}");
        let timeout = Duration::from_millis(cap_ms) + Duration::from_secs(5);
        Some(
            self.call(device_id, &cmd, false, timeout)
                .await?
                .and_then(|(line, _)| {
                    let mut it = line.split_whitespace().skip(1);
                    let mut next = || it.next().and_then(|v| v.parse::<u64>().ok());
                    match (next(), next(), next()) {
                        (Some(s), Some(r), Some(ms)) => Ok((s == 1, r == 1, ms)),
                        _ => Err(format!("bad settle reply {line:?}")),
                    }
                }),
        )
    }

    async fn unit(
        &self,
        device_id: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Option<Result<(), String>> {
        Some(self.call(device_id, cmd, false, timeout).await?.map(|_| ()))
    }

    /// Send one command. `None`: the server isn't usable (not started,
    /// backing off after failures, or the connection broke before the
    /// command went out — it is torn down and counted as a failure); the
    /// caller may use adb instead. `Some(Err)`: the server answered `ERR`
    /// (the session stays up), or a command that changes the device
    /// ([`mutates`]) was sent and its outcome is unknown. That one is never
    /// resent and never repeated over adb: a tap or typed text would
    /// happen twice (VEL-158, VEL-313).
    async fn call(
        &self,
        device_id: &str,
        cmd: &str,
        payload: bool,
        timeout: Duration,
    ) -> Option<Result<(String, Vec<u8>), String>> {
        if !self.enabled {
            return None;
        }
        let slot = self.slot(device_id);
        let mut state = slot.lock().await;
        if state.disabled || state.retry_at.is_some_and(|t| Instant::now() < t) {
            return None;
        }
        if state.session.is_none() {
            match self.start(device_id).await {
                Ok(s) => state.session = Some(s),
                Err(e) => {
                    state.failed(device_id, &e);
                    return None;
                }
            }
        }
        if std::mem::take(&mut state.in_flight) {
            if let Some(s) = state.session.take() {
                state.session = self.fresh_connection(device_id, s).await;
            }
            state.session.as_ref()?;
        }
        let mutate = mutates(cmd);
        // Protocol 8: tag a command that changes the device, so that sending
        // it again (same id) after a lost reply can never run it twice.
        let id = mutate.then(next_request_id);
        for attempt in 0..2 {
            let st = &mut *state;
            let session = st.session.as_mut()?;
            let line = match &id {
                Some(id) if session.proto >= 8 => format!("#{id} {cmd}"),
                _ => cmd.to_string(),
            };
            st.in_flight = true;
            let sent = Instant::now();
            let outcome =
                tokio::time::timeout(timeout, request(&mut session.stream, &line, payload)).await;
            debug!(
                device_id,
                cmd,
                ms = sent.elapsed().as_millis() as u64,
                "tree server reply"
            );
            if outcome.is_err() && mutate {
                // The reply may still come down this connection: leave
                // `in_flight` set so the next command reconnects.
                state.failed(device_id, "timed out");
                return Some(Err(format!(
                    "tree server did not answer {} within {} ms; {OUTCOME_UNKNOWN}",
                    verb(cmd),
                    timeout.as_millis()
                )));
            }
            st.in_flight = false;
            match outcome {
                Ok(Ok(Reply::Ok(line, bytes))) => {
                    // Failures count only in a row: a long-lived process
                    // (the daemon) must not drift into the slow fallback.
                    state.succeeded();
                    return Some(Ok((line, bytes)));
                }
                Ok(Ok(Reply::Err(msg))) => {
                    state.succeeded();
                    // The server's own words can never pass for "unknown".
                    return Some(Err(
                        msg.replace(OUTCOME_UNKNOWN, "whether it ran is not known")
                    ));
                }
                Ok(Err(RequestError::Unanswered(why) | RequestError::Broken(why))) if mutate => {
                    if let Some(s) = state.session.take() {
                        if s.proto >= 8 {
                            // Same server, same id: answered from its cache
                            // if it ran, run now if it never arrived.
                            match self.resend(device_id, s, &line, payload, timeout).await {
                                Ok((s, reply)) => {
                                    state.session = Some(s);
                                    state.succeeded();
                                    debug!(device_id, "resent {} after a lost reply", verb(cmd));
                                    return Some(match reply {
                                        Reply::Ok(l, b) => Ok((l, b)),
                                        Reply::Err(m) => Err(m.replace(
                                            OUTCOME_UNKNOWN,
                                            "whether it ran is not known",
                                        )),
                                    });
                                }
                                Err((port, resend_why)) => {
                                    debug!(device_id, "no safe resend: {resend_why}");
                                    self.unforward(device_id, port).await;
                                }
                            }
                        } else {
                            self.unforward(device_id, s.port).await;
                        }
                    }
                    state.failed(device_id, &why);
                    return Some(Err(format!(
                        "tree server connection lost after sending {} ({why}); \
                         {OUTCOME_UNKNOWN}, so it is not repeated",
                        verb(cmd)
                    )));
                }
                Ok(Err(RequestError::Stale(why) | RequestError::Unanswered(why)))
                    if attempt == 0 =>
                {
                    // The request never reached the server (another process
                    // stopped it, or the device restarted it), or it only
                    // reads: start it again and resend.
                    debug!(
                        device_id,
                        "tree server connection stale ({why}); restarting"
                    );
                    if let Some(s) = state.session.take() {
                        self.unforward(device_id, s.port).await;
                    }
                    match self.start(device_id).await {
                        Ok(s) => state.session = Some(s),
                        Err(e) => {
                            state.failed(device_id, &e);
                            return None;
                        }
                    }
                }
                outcome => {
                    let why = match outcome {
                        Ok(Err(
                            RequestError::Stale(e)
                            | RequestError::Unanswered(e)
                            | RequestError::Broken(e),
                        )) => e,
                        _ => "timed out".to_string(),
                    };
                    state.failed(device_id, &format!("{cmd:?} failed ({why}); falling back"));
                    if let Some(s) = state.session.take() {
                        self.shutdown(device_id, s).await;
                    }
                    return None;
                }
            }
        }
        None
    }

    /// After a lost reply: reconnect to the same server and send `line`
    /// (with its request id) again. Only when the server is the one the
    /// first send reached (same boot nonce): a restarted server has no
    /// record of the id and could run the command a second time. `Err`
    /// gives back the port to unforward.
    async fn resend(
        &self,
        device_id: &str,
        old: Session,
        line: &str,
        payload: bool,
        timeout: Duration,
    ) -> Result<(Session, Reply), (u16, String)> {
        let port = old.port;
        let mut s = open_session(
            &crate::adb::server_host(device_id),
            port,
            old._child,
            old.token.clone(),
        )
        .await
        .map_err(|e| (port, e))?;
        if s.nonce.is_none() || s.nonce != old.nonce {
            return Err((port, "the server restarted".into()));
        }
        match tokio::time::timeout(timeout, request(&mut s.stream, line, payload)).await {
            Ok(Ok(reply)) => Ok((s, reply)),
            Ok(Err(
                RequestError::Stale(e) | RequestError::Unanswered(e) | RequestError::Broken(e),
            )) => Err((port, e)),
            Err(_) => Err((port, "timed out".into())),
        }
    }

    /// `velocity doctor --deep`: push the jar (unless this build is
    /// already on the device) and handshake. A server already listening is
    /// PINGed and left running (it may be another run's). Otherwise, only
    /// when `may_start` (the caller holds the device's lease), one is
    /// started, PINGed and stopped again; without it `version` is `None`.
    pub async fn probe(&self, device_id: &str, may_start: bool) -> Result<Probe, String> {
        let jar = self.push_jar(device_id).await?;
        let host = crate::adb::server_host(device_id);
        if let Ok(port) = self.forward(device_id).await {
            let pinged = match connect(&host, port).await {
                Ok(mut stream) => ping(&mut stream).await.ok(),
                Err(_) => None,
            };
            self.unforward(device_id, port).await;
            if let Some((version, _)) = pinged {
                return Ok(Probe {
                    jar,
                    version: Some(version),
                    attached: true,
                });
            }
        }
        if !may_start {
            return Ok(Probe {
                jar,
                version: None,
                attached: false,
            });
        }
        let session = self.start(device_id).await?;
        let version = session.proto.to_string();
        self.shutdown(device_id, session).await;
        Ok(Probe {
            jar,
            version: Some(version),
            attached: false,
        })
    }

    /// Remove every Velocity tree-server jar from the device and push this
    /// build's again (`velocity doctor --fix` after a failed probe).
    pub async fn repush(&self, device_id: &str) -> Result<String, String> {
        self.adb(
            device_id,
            &["shell", "rm -f /data/local/tmp/velocity-tree-*.jar"],
        )
        .await?;
        self.push_jar(device_id).await
    }

    /// The protocol version this build's embedded server speaks.
    pub fn server_version() -> &'static str {
        SERVER_VERSION
    }

    /// Forget `device_id`'s session and failure count — after a snapshot
    /// restore the on-device server is gone, and that's not its fault.
    pub async fn reset(&self, device_id: &str) {
        let slot = self.slot(device_id);
        let mut state = slot.lock().await;
        state.succeeded();
        if let Some(s) = state.session.take() {
            self.unforward(device_id, s.port).await;
        }
    }

    /// Stop every running server (call before handing the device to other
    /// UiAutomation clients, and at cleanup).
    pub async fn stop_all(&self) {
        let slots: Vec<(String, Arc<Mutex<DeviceState>>)> = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect();
        for (device_id, slot) in slots {
            let session = slot.lock().await.session.take();
            if let Some(s) = session {
                self.shutdown(&device_id, s).await;
            }
        }
    }

    async fn start(&self, device_id: &str) -> Result<Session, String> {
        // A server from an earlier run may still be listening: reuse it if it
        // speaks our version, otherwise ask it to quit.
        if let Ok(port) = self.forward(device_id).await {
            if let Ok(mut stream) = connect(&crate::adb::server_host(device_id), port).await {
                match ping(&mut stream).await {
                    Ok((v, nonce)) if v == SERVER_VERSION => {
                        // Its token is in the device file it wrote at start;
                        // one we can't present is asked to quit (QUIT needs
                        // no token) and replaced.
                        let token = self.read_token(device_id).await;
                        match &token {
                            Ok(t) => match auth(&mut stream, t).await {
                                Ok(()) => {
                                    debug!(device_id, port, "attached to running tree server");
                                    return Ok(Session {
                                        stream,
                                        port,
                                        proto: supported(&v).unwrap_or(OLDEST_VERSION),
                                        nonce,
                                        token: token.ok(),
                                        _child: None,
                                    });
                                }
                                Err(e) => debug!(device_id, "running tree server: {e}"),
                            },
                            Err(e) => debug!(device_id, "running tree server: {e}"),
                        }
                        let _ = stream.get_mut().write_all(b"QUIT\n").await;
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    Ok(_) => {
                        let _ = stream.get_mut().write_all(b"QUIT\n").await;
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    Err(_) => {}
                }
            }
            self.unforward(device_id, port).await;
        }

        let remote = self.push_jar(device_id).await?;
        // Its own process group, registered for the second-signal kill:
        // `std::process::exit` skips `kill_on_drop` (review d4 m2).
        let mut child = velocity_common::process::spawn_group(
            Command::new(&self.adb_path)
                .args(crate::adb::target_args(device_id))
                .args([
                    "shell",
                    &format!(
                        "CLASSPATH={remote}:/system/framework/uiautomator.jar \
                     exec app_process /system/bin com.velocity.tree.Server{}",
                        // Keep TalkBack and other accessibility services running
                        // while Velocity drives the device (off: they're paused).
                        if keep_a11y() { " --keep-a11y" } else { "" }
                    ),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::null()),
        )
        .map_err(|e| format!("spawn adb: {e}"))?;

        // Keep the server's stderr: "UiAutomationService … already
        // registered" means another tool (Maestro, Appium, uiautomator) holds
        // the device's only UiAutomation connection — say so instead of
        // silently falling back.
        let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stderr) = child.stderr.take() {
            let device = device_id.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(device_id = %device, "tree server stderr: {line}");
                    let _ = err_tx.send(line);
                }
            });
        }
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(START_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("listening") {
                    return Ok(());
                }
                debug!(device_id, "tree server: {line}");
            }
            Err("server exited before listening".to_string())
        })
        .await
        .map_err(|_| format!("server did not start in time ({START_TIMEOUT:?})"))
        .and_then(|r| r);
        if let Err(why) = ready {
            let mut stderr = Vec::new();
            while let Ok(line) = err_rx.try_recv() {
                stderr.push(line);
            }
            let joined = stderr.join(" | ");
            if joined.contains("already registered") {
                return Err(format!(
                    "{ANOTHER_CLIENT} (Maestro, Appium, uiautomator) is connected to \
                     this device; stop it to use the fast tree server ({why})"
                ));
            }
            return Err(if joined.is_empty() {
                why
            } else {
                format!("{why}: {joined}")
            });
        }
        // Keep draining stdout so the pipe never fills and blocks the server.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let port = self.forward(device_id).await?;
        let token = match self.read_token(device_id).await {
            Ok(t) => Some(t),
            Err(e) => {
                self.unforward(device_id, port).await;
                return Err(e);
            }
        };
        let session = match open_session(
            &crate::adb::server_host(device_id),
            port,
            Some(child),
            token,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                self.unforward(device_id, port).await;
                return Err(e);
            }
        };
        info!(
            device_id,
            port,
            proto = session.proto,
            "tree server started"
        );
        Ok(session)
    }

    /// The same server on a new connection, after an abandoned request left
    /// the old one out of step. The server serves each connection on its own
    /// thread, so this works even while the old request is still running.
    async fn fresh_connection(&self, device_id: &str, old: Session) -> Option<Session> {
        debug!(device_id, "tree server request was abandoned; reconnecting");
        let started = std::time::Instant::now();
        let host = crate::adb::server_host(device_id);
        match connect(&host, old.port).await {
            Ok(mut stream) => match ping(&mut stream).await {
                Ok((v, nonce))
                    if supported(&v) == Some(old.proto)
                        && present_token(&mut stream, old.proto, old.token.as_deref())
                            .await
                            .is_ok() =>
                {
                    debug!(
                        device_id,
                        ms = started.elapsed().as_millis() as u64,
                        "reconnected"
                    );
                    return Some(Session {
                        stream,
                        nonce,
                        ..old
                    });
                }
                other => debug!(device_id, ?other, "reconnect ping failed"),
            },
            Err(e) => debug!(device_id, %e, "reconnect failed"),
        }
        self.shutdown(device_id, old).await;
        debug!(
            device_id,
            ms = started.elapsed().as_millis() as u64,
            "tree server shut down after a failed reconnect"
        );
        None
    }

    async fn shutdown(&self, device_id: &str, mut s: Session) {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = s.stream.get_mut().write_all(b"QUIT\n").await;
            let mut buf = Vec::new();
            let _ = (&mut s.stream)
                .take(MAX_HEADER)
                .read_until(b'\n', &mut buf)
                .await;
        })
        .await;
        self.unforward(device_id, s.port).await;
    }

    /// Protocol 13: the running server's session token, from the file it
    /// wrote on the device (shell or root only: a web page can't read it).
    async fn read_token(&self, device_id: &str) -> Result<String, String> {
        let out = self
            .adb(device_id, &["shell", &format!("cat {TOKEN_FILE}")])
            .await?;
        let token = out.trim();
        if token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(token.to_string())
        } else {
            Err(format!("no tree server token in {TOKEN_FILE}"))
        }
    }

    /// Push the embedded jar unless this exact build is already on the device.
    async fn push_jar(&self, device_id: &str) -> Result<String, String> {
        push_server_jar(&self.adb_path, device_id).await
    }

    async fn forward(&self, device_id: &str) -> Result<u16, String> {
        let out = self
            .adb(
                device_id,
                &["forward", "tcp:0", &format!("localabstract:{SOCKET_NAME}")],
            )
            .await?;
        out.trim()
            .parse()
            .map_err(|e| format!("unexpected forward output {out:?}: {e}"))
    }

    async fn unforward(&self, device_id: &str, port: u16) {
        let _ = self
            .adb(device_id, &["forward", "--remove", &format!("tcp:{port}")])
            .await;
    }

    async fn adb(&self, device_id: &str, args: &[&str]) -> Result<String, String> {
        run_adb(&self.adb_path, device_id, args).await
    }
}

/// `adb <target> <args>`'s stdout, or its stderr as the error.
pub(crate) async fn run_adb(
    adb_path: &str,
    device_id: &str,
    args: &[&str],
) -> Result<String, String> {
    let mut cmd = Command::new(adb_path);
    cmd.args(crate::adb::target_args(device_id))
        .args(args)
        .stdin(Stdio::null());
    let out = velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(30))
        .await
        .map_err(|e| format!("adb: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "adb {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Push the embedded jar (the tree server and, since protocol 12, the
/// video server) unless this exact build is already on the device. The
/// remote name carries a content hash, so upgrades never reuse an old jar.
pub(crate) async fn push_server_jar(adb_path: &str, device_id: &str) -> Result<String, String> {
    let remote = format!(
        "/data/local/tmp/velocity-tree-{:016x}.jar",
        fnv1a(SERVER_JAR)
    );
    let present = run_adb(
        adb_path,
        device_id,
        &["shell", &format!("test -f {remote} && echo yes")],
    )
    .await
    .map(|o| o.trim() == "yes")
    .unwrap_or(false);
    if present {
        return Ok(remote);
    }
    let local = write_private_temp("velocity-tree", ".jar", SERVER_JAR)
        .map_err(|e| format!("write jar: {e}"))?;
    let result = run_adb(
        adb_path,
        device_id,
        &["push", &local.to_string_lossy(), &remote],
    )
    .await;
    let _ = std::fs::remove_file(&local);
    result.map(|_| remote)
}

async fn connect(host: &str, port: u16) -> Result<BufReader<TcpStream>, String> {
    let stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("connect to {host}:{port} timed out"))?
        .map_err(|e| format!("connect: {e}"))?;
    let _ = stream.set_nodelay(true);
    Ok(BufReader::new(stream))
}

/// Connect to a server on `port` and agree on a protocol: any version from
/// [`OLDEST_VERSION`] to [`SERVER_VERSION`] works, so a device still running
/// an older jar is used as it is.
async fn open_session(
    host: &str,
    port: u16,
    child: Option<velocity_common::process::GroupChild>,
    token: Option<String>,
) -> Result<Session, String> {
    let mut stream = connect(host, port).await?;
    let (version, nonce) = ping(&mut stream).await?;
    let proto = supported(&version).ok_or_else(|| {
        format!("server version {version}, expected {OLDEST_VERSION} to {SERVER_VERSION}")
    })?;
    present_token(&mut stream, proto, token.as_deref()).await?;
    Ok(Session {
        stream,
        port,
        proto,
        nonce,
        token,
        _child: child,
    })
}

/// `AUTH` on a protocol 13+ server when there is a token (tests talk to
/// stand-in servers without one); older servers have no `AUTH`.
async fn present_token(
    stream: &mut BufReader<TcpStream>,
    proto: u32,
    token: Option<&str>,
) -> Result<(), String> {
    match token {
        Some(t) if proto >= AUTH_VERSION => auth(stream, t).await,
        _ => Ok(()),
    }
}

/// `AUTH <token>` → `OK 0`; the server closes the connection otherwise.
async fn auth(stream: &mut BufReader<TcpStream>, token: &str) -> Result<(), String> {
    let fut = async {
        stream
            .get_mut()
            .write_all(format!("AUTH {token}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut line = Vec::new();
        (&mut *stream)
            .take(MAX_HEADER)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| e.to_string())?;
        let line = String::from_utf8_lossy(&line);
        if line.trim() == "OK 0" {
            Ok(())
        } else {
            Err(format!("tree server refused the token: {:?}", line.trim()))
        }
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .map_err(|_| "auth timed out (2s)".to_string())?
}

async fn ping(stream: &mut BufReader<TcpStream>) -> Result<(String, Option<String>), String> {
    let fut = async {
        stream
            .get_mut()
            .write_all(b"PING\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        stream
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        parse_pong(&line).ok_or_else(|| format!("bad ping reply {line:?}"))
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .map_err(|_| "ping timed out (2s)".to_string())?
}

enum Reply {
    Ok(String, Vec<u8>),
    Err(String),
}

enum RequestError {
    /// Nothing was sent: the write failed, or the server had already
    /// closed the connection. Safe to resend after a restart.
    Stale(String),
    /// Sent, but the connection closed or failed before any reply: the
    /// command may or may not have run.
    Unanswered(String),
    /// Failed mid-reply, or the reply was malformed.
    Broken(String),
}

/// Whether the server already closed `stream`, or sent bytes nobody asked
/// for (out of step): either way a command must not go down it. A write to
/// a closed TCP socket can still succeed, so without this check a stopped
/// server only showed after the command was sent.
fn closed_or_out_of_step(stream: &BufReader<TcpStream>) -> bool {
    if !stream.buffer().is_empty() {
        return true;
    }
    let mut byte = [0u8; 1];
    match stream.get_ref().try_read(&mut byte) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    }
}

/// One request; with `payload` the reply is `OK <len>` followed by bytes.
async fn request(
    stream: &mut BufReader<TcpStream>,
    cmd: &str,
    payload: bool,
) -> Result<Reply, RequestError> {
    if closed_or_out_of_step(stream) {
        return Err(RequestError::Stale("server closed the connection".into()));
    }
    stream
        .get_mut()
        .write_all(format!("{cmd}\n").as_bytes())
        .await
        .map_err(|e| RequestError::Stale(e.to_string()))?;
    // A header is one short line: never buffer an endless one (VEL-159).
    let mut header = Vec::new();
    let n = (&mut *stream)
        .take(MAX_HEADER)
        .read_until(b'\n', &mut header)
        .await
        .map_err(|e| RequestError::Unanswered(e.to_string()))?;
    if n == 0 {
        return Err(RequestError::Unanswered(
            "server closed the connection".into(),
        ));
    }
    if header.last() != Some(&b'\n') {
        return Err(RequestError::Broken(format!(
            "reply header longer than {MAX_HEADER} bytes or cut off"
        )));
    }
    let header = String::from_utf8_lossy(&header).trim().to_string();
    if let Some(msg) = header.strip_prefix("ERR ") {
        return Ok(Reply::Err(msg.to_string()));
    }
    if !header.starts_with("OK") {
        return Err(RequestError::Broken(format!("server said {header:?}")));
    }
    if !payload {
        return Ok(Reply::Ok(header, Vec::new()));
    }
    let len: usize = header
        .strip_prefix("OK ")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| RequestError::Broken(format!("server said {header:?}")))?;
    let cap = payload_cap(cmd);
    if len > cap {
        return Err(RequestError::Broken(format!(
            "{} reply of {len} bytes is over the {cap} byte limit",
            verb(cmd)
        )));
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|e| RequestError::Broken(format!("{len} byte reply: {e}")))?;
    buf.resize(len, 0);
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| RequestError::Broken(e.to_string()))?;
    Ok(Reply::Ok(header, buf))
}

/// A window on screen (from `WINDOWS`), front first.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub layer: i32,
    /// `AccessibilityWindowInfo` type: 1 application, 2 input method,
    /// 3 system, 4 accessibility overlay, 5 split-screen divider.
    pub kind: i32,
    pub bounds: velocity_common::Rect,
    pub package: String,
}

impl Window {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "app window",
            2 => "keyboard",
            3 => "system window",
            4 => "accessibility overlay",
            5 => "split-screen divider",
            _ => "window",
        }
    }
}

/// `ENCODERS` lines: `<name> <1|0|?>`.
fn parse_encoders(text: &str) -> Vec<(String, Option<bool>)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?.to_string();
            let hw = match it.next() {
                Some("1") => Some(true),
                Some("0") => Some(false),
                _ => None,
            };
            Some((name, hw))
        })
        .collect()
}

fn parse_windows(text: &str) -> Vec<Window> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let layer = it.next()?.parse().ok()?;
            let kind = it.next()?.parse().ok()?;
            let b: Vec<i32> = it
                .next()?
                .split(',')
                .filter_map(|v| v.parse().ok())
                .collect();
            let package = it.next().unwrap_or("-").to_string();
            (b.len() == 4).then(|| Window {
                layer,
                kind,
                bounds: velocity_common::Rect {
                    x: b[0],
                    y: b[1],
                    width: b[2].saturating_sub(b[0]),
                    height: b[3].saturating_sub(b[1]),
                },
                package,
            })
        })
        .collect()
}

/// Write `bytes` to a new file in the temp dir that only this user can
/// read. The name is unpredictable and the file must not exist yet, so a
/// file or symlink planted at a guessable path (the old
/// `velocity-tree-<pid>.jar`) is never followed or overwritten (VEL-358).
fn write_private_temp(
    prefix: &str,
    suffix: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir();
    let mut last = None;
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(
            "{prefix}-{}-{:016x}{suffix}",
            std::process::id(),
            fnv1a(&[&nanos.to_le_bytes()[..], &seq.to_le_bytes()].concat()) ^ seq
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes) {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free temp name")))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A tree server whose session is already open on `port` (driver tests).
#[cfg(test)]
pub(crate) async fn attached_for_test(adb_path: String, port: u16) -> TreeServer {
    let ts = TreeServer {
        adb_path,
        enabled: true,
        devices: Default::default(),
    };
    ts.slot("emulator-5554").lock().await.session =
        Some(open_session("127.0.0.1", port, None, None).await.unwrap());
    ts
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in server: `PING` → `PONG <version>`, `SLOW` answers after
    /// 300 ms, anything else at once; one thread per connection, like the
    /// real one.
    async fn fake_server() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut sock = BufReader::new(sock);
                    let mut line = String::new();
                    while sock.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let cmd = line.trim().to_string();
                        line.clear();
                        let reply = match cmd.as_str() {
                            "PING" => format!("PONG {SERVER_VERSION}\n"),
                            "SLOW" => {
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                "OK slow\n".to_string()
                            }
                            other => format!("OK {other}\n"),
                        };
                        if sock.get_mut().write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn an_abandoned_request_does_not_answer_the_next_one() {
        let port = fake_server().await;
        let ts = TreeServer {
            adb_path: "adb".into(),
            enabled: true,
            devices: Default::default(),
        };
        ts.slot("dev").lock().await.session =
            Some(open_session("127.0.0.1", port, None, None).await.unwrap());
        let t = Duration::from_secs(2);
        // The caller gives up mid-request (a step timeout).
        let cut =
            tokio::time::timeout(Duration::from_millis(50), ts.call("dev", "SLOW", false, t)).await;
        assert!(cut.is_err());
        let reply = ts.call("dev", "FAST", false, t).await.unwrap().unwrap();
        assert_eq!(reply.0, "OK FAST", "read the abandoned request's reply");
        assert!(
            ts.slot("dev").lock().await.session.is_some(),
            "kept the server"
        );
    }

    /// What the scripted server does with a command.
    #[derive(Clone)]
    pub(crate) enum Act {
        Say(Vec<u8>),
        /// Read the command, then close the connection without a reply.
        Close,
        /// Never answer.
        Hang,
        /// Send these bytes forever, with no newline.
        Flood,
    }

    pub(crate) type Script = Arc<dyn Fn(&str, usize) -> Act + Send + Sync>;

    /// A server that answers `PING` and does what `script` says for every
    /// other command (given how many times that verb came before). Every
    /// command it receives is recorded.
    pub(crate) async fn scripted(script: Script) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        scripted_as(Arc::new(|_| format!("PONG {SERVER_VERSION}\n")), script).await
    }

    /// [`scripted`], answering `PING` with `pong(connection number)`.
    async fn scripted_as(
        pong: Arc<dyn Fn(usize) -> String + Send + Sync>,
        script: Script,
    ) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (script, log) = (script.clone(), log.clone());
                let conn = conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let pong = pong(conn);
                tokio::spawn(async move {
                    let mut sock = BufReader::new(sock);
                    let mut line = String::new();
                    while sock.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let cmd = line.trim().to_string();
                        line.clear();
                        if cmd == "PING" {
                            let _ = sock.get_mut().write_all(pong.as_bytes()).await;
                            continue;
                        }
                        if let Some(token) = cmd.strip_prefix("AUTH ") {
                            log.lock().unwrap().push(cmd.clone());
                            if token != crate::test_support::FAKE_TREE_TOKEN {
                                let _ = sock.get_mut().write_all(b"ERR bad token\n").await;
                                break;
                            }
                            let _ = sock.get_mut().write_all(b"OK 0\n").await;
                            continue;
                        }
                        let n = {
                            let mut log = log.lock().unwrap();
                            let n = log.iter().filter(|c| verb(c) == verb(&cmd)).count();
                            log.push(cmd.clone());
                            n
                        };
                        match script(&cmd, n) {
                            Act::Say(b) => {
                                if sock.get_mut().write_all(&b).await.is_err() {
                                    break;
                                }
                            }
                            Act::Close => break,
                            Act::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                            Act::Flood => {
                                let chunk = vec![b'x'; 64 * 1024];
                                while sock.get_mut().write_all(&chunk).await.is_ok() {}
                                break;
                            }
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    /// A tree server whose `adb forward` hands out `port` (a fake adb), so a
    /// restart attaches to the scripted server again.
    async fn attached(name: &str, port: u16) -> (TreeServer, crate::test_support::FakeAdb) {
        let fake = crate::test_support::FakeAdb::new(name);
        std::fs::write(fake.dir.join("device/port"), port.to_string()).unwrap();
        fake.stub(
            "adb-forward",
            "case \"$*\" in *--remove*) exit 0;; esac; cat \"$DEV/port\"",
        );
        let ts = TreeServer {
            adb_path: fake.path(),
            enabled: true,
            devices: Default::default(),
        };
        ts.slot("dev").lock().await.session =
            Some(open_session("127.0.0.1", port, None, None).await.unwrap());
        (ts, fake)
    }

    fn count(seen: &std::sync::Mutex<Vec<String>>, verb_: &str) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|c| verb(c) == verb_)
            .count()
    }

    /// VEL-158: a reply lost after the tap was sent was taken for a stale
    /// connection: the server was restarted and the tap sent again.
    #[tokio::test]
    async fn read_error_after_write_is_not_replayed_for_taps() {
        let (port, seen) = scripted(Arc::new(|cmd, _| match verb(cmd) {
            "TAPS" => Act::Close,
            _ => Act::Say(b"OK\n".to_vec()),
        }))
        .await;
        let (ts, _fake) = attached("ts-replay", port).await;
        let r = ts.taps("dev", 10, 20, 1).await;
        let err = r.expect("not a silent adb fallback").unwrap_err();
        assert!(is_outcome_unknown(&err), "{err}");
        assert_eq!(count(&seen, "TAPS"), 1, "the tap was sent again");
        // Reads still recover by themselves.
        assert!(ts
            .call("dev", "MARK", false, Duration::from_secs(2))
            .await
            .is_some());
    }

    /// VEL-313: a tap that timed out fell back to `adb shell input tap`.
    #[tokio::test]
    async fn timeout_of_mutate_does_not_fall_back_to_adb() {
        let (port, seen) = scripted(Arc::new(|cmd, _| match verb(cmd) {
            "TAPS" => Act::Hang,
            _ => Act::Say(b"OK\n".to_vec()),
        }))
        .await;
        let (ts, fake) = attached("ts-timeout", port).await;
        let r = ts
            .call("dev", "TAPS 1 2 1", false, Duration::from_millis(200))
            .await;
        let err = r.expect("timed-out tap must not fall back").unwrap_err();
        assert!(is_outcome_unknown(&err), "{err}");
        assert_eq!(count(&seen, "TAPS"), 1);
        assert!(!fake.log().contains("input"), "{}", fake.log());
        // The next command gets a fresh connection, not the late reply.
        let mark = ts.call("dev", "MARK", false, Duration::from_secs(2)).await;
        assert_eq!(mark.unwrap().unwrap().0, "OK");
    }

    /// A server stopped between steps closed the connection before the
    /// command: that is still restarted and sent, once.
    #[tokio::test]
    async fn a_closed_connection_before_sending_is_restarted() {
        let (port, seen) = scripted(Arc::new(|_, _| Act::Say(b"OK\n".to_vec()))).await;
        let (ts, _fake) = attached("ts-stale", port).await;
        // The server side of the session goes away.
        {
            let slot = ts.slot("dev");
            let mut st = slot.lock().await;
            let s = st.session.as_mut().unwrap();
            s.stream.get_mut().write_all(b"MARK\n").await.unwrap();
            let mut l = String::new();
            s.stream.read_line(&mut l).await.unwrap();
            let fresh = connect("127.0.0.1", port).await.unwrap();
            let old = std::mem::replace(&mut s.stream, fresh);
            let (r, w) = old.into_inner().into_split();
            drop((r, w));
        }
        // Make the replaced stream look closed: a connection the peer shut.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        drop(listener.accept().await.unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;
        ts.slot("dev").lock().await.session.as_mut().unwrap().stream = BufReader::new(client);
        let r = ts.taps("dev", 1, 2, 1).await;
        assert!(matches!(r, Some(Ok(()))), "restart and send");
        assert_eq!(count(&seen, "TAPS"), 1);
    }

    /// VEL-159: a header with no newline was buffered without limit.
    #[tokio::test]
    async fn an_endless_header_is_cut_off() {
        let (port, _) = scripted(Arc::new(|_, _| Act::Flood)).await;
        let (ts, _fake) = attached("ts-header", port).await;
        let started = Instant::now();
        // Cut off at once, not when the call's own deadline passes.
        let r = ts.call("dev", "DUMP", true, Duration::from_secs(60)).await;
        assert!(r.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "read until the timeout: {:?}",
            started.elapsed()
        );
    }

    /// VEL-160: `OK <len>` was allocated as given (1 TiB: abort).
    #[tokio::test]
    async fn an_oversized_payload_is_refused() {
        let (port, _) = scripted(Arc::new(|_, _| Act::Say(b"OK 1099511627776\n".to_vec()))).await;
        let (ts, _fake) = attached("ts-payload", port).await;
        let started = Instant::now();
        let r = ts.call("dev", "DUMP", true, Duration::from_secs(60)).await;
        assert!(r.is_none());
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    /// VEL-161: two hiccups turned the server off for the whole process.
    #[tokio::test]
    async fn recovers_after_two_hiccups() {
        let xml = "<hierarchy/>";
        let (port, seen) = scripted(Arc::new(move |cmd, n| match verb(cmd) {
            // A reply cut off mid-payload, twice.
            "DUMP" if n < 2 => Act::Say(b"OK 100\nabc".to_vec()),
            "DUMP" => Act::Say(format!("OK {}\n{xml}", xml.len()).into_bytes()),
            _ => Act::Say(b"OK\n".to_vec()),
        }))
        .await;
        let (ts, _fake) = attached("ts-backoff", port).await;
        // Each cut reply is noticed at once (the server closes after it).
        for _ in 0..2 {
            assert!(ts
                .call("dev", "DUMP", true, Duration::from_millis(300))
                .await
                .is_none());
        }
        // Backing off: not even tried.
        assert!(ts.dump("dev").await.is_none());
        assert_eq!(count(&seen, "DUMP"), 2);
        // Once the backoff is over the server is used again.
        ts.slot("dev").lock().await.retry_at = Some(Instant::now());
        assert_eq!(ts.dump("dev").await.as_deref(), Some(xml));
        let st = ts.slot("dev");
        let st = st.lock().await;
        assert_eq!((st.failures, st.retry_at), (0, None));
    }

    /// Protocol 8: a tap whose reply was lost is sent again on a new
    /// connection to the same server (same boot nonce) with the same id,
    /// which the server answers from its reply cache instead of tapping
    /// again. Reads carry no id.
    #[tokio::test]
    async fn same_id_after_reconnect_is_deduplicated() {
        let (port, seen) = scripted_as(
            Arc::new(|_| "PONG 8 b007\n".to_string()),
            Arc::new(|cmd, n| match verb(cmd) {
                "TAPS" if n == 0 => Act::Close,
                _ => Act::Say(b"OK 0\n".to_vec()),
            }),
        )
        .await;
        let (ts, fake) = attached("ts-dedup", port).await;
        assert_eq!(ts.taps("dev", 10, 20, 1).await, Some(Ok(())));
        let taps: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|c| verb(c) == "TAPS")
            .cloned()
            .collect();
        assert_eq!(taps.len(), 2, "{taps:?}");
        assert!(taps[0].starts_with('#') && taps[0].ends_with(" TAPS 10 20 1"));
        assert_eq!(taps[0], taps[1], "resent under another id");
        assert!(!fake.log().contains("input"));
        // The next tap gets a new id; reads are untagged.
        assert_eq!(ts.taps("dev", 1, 2, 1).await, Some(Ok(())));
        ts.call("dev", "MARK", false, Duration::from_secs(2)).await;
        let log = seen.lock().unwrap().clone();
        assert_ne!(log[2], log[0]);
        assert_eq!(log.last().unwrap(), "MARK");
    }

    /// R-2: a protocol 13 session presents its token first, and again on
    /// the new connection a resend opens; a server that refuses it is no
    /// session at all. An older server gets no `AUTH` (it has none).
    #[tokio::test]
    async fn protocol_13_presents_the_token_on_every_connection() {
        let (port, seen) = scripted_as(
            Arc::new(|_| "PONG 13 b007\n".to_string()),
            Arc::new(|cmd, n| match verb(cmd) {
                "TAPS" if n == 0 => Act::Close,
                _ => Act::Say(b"OK 0\n".to_vec()),
            }),
        )
        .await;
        let tok = crate::test_support::FAKE_TREE_TOKEN.to_string();
        let fake = crate::test_support::FakeAdb::new("ts-auth");
        let ts = TreeServer {
            adb_path: fake.path(),
            enabled: true,
            devices: Default::default(),
        };
        ts.slot("dev").lock().await.session = Some(
            open_session("127.0.0.1", port, None, Some(tok.clone()))
                .await
                .unwrap(),
        );
        assert_eq!(ts.taps("dev", 10, 20, 1).await, Some(Ok(())));
        let log = seen.lock().unwrap().clone();
        let verbs: Vec<&str> = log.iter().map(|c| verb(c)).collect();
        assert_eq!(verbs, ["AUTH", "TAPS", "AUTH", "TAPS"], "{log:?}");
        assert!(log
            .iter()
            .filter(|c| verb(c) == "AUTH")
            .all(|c| *c == format!("AUTH {tok}")));

        let refused = open_session("127.0.0.1", port, None, Some("wrong".into())).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("refused the token")),
            "{:?}",
            refused.map(|_| ())
        );

        let (old, seen) = scripted_as(
            Arc::new(|_| "PONG 12 b007\n".to_string()),
            Arc::new(|_, _| Act::Say(b"OK 0\n".to_vec())),
        )
        .await;
        let s = open_session("127.0.0.1", old, None, Some(tok))
            .await
            .unwrap();
        assert_eq!(s.proto, 12);
        assert!(seen.lock().unwrap().is_empty(), "no AUTH to protocol 12");
    }

    /// A reconnect that reaches a restarted server (new nonce): its reply
    /// cache is empty, so the tap is not sent again.
    #[tokio::test]
    async fn a_restarted_server_is_not_resent_to() {
        let (port, seen) = scripted_as(
            Arc::new(|conn| format!("PONG 8 nonce{conn}\n")),
            Arc::new(|cmd, _| match verb(cmd) {
                "TAPS" => Act::Close,
                _ => Act::Say(b"OK 0\n".to_vec()),
            }),
        )
        .await;
        let (ts, _fake) = attached("ts-restarted", port).await;
        let err = ts.taps("dev", 10, 20, 1).await.unwrap().unwrap_err();
        assert!(is_outcome_unknown(&err), "{err}");
        assert_eq!(count(&seen, "TAPS"), 1);
    }

    /// A device still running a protocol 7 jar works: no ids (it would not
    /// understand them) and no resend.
    #[tokio::test]
    async fn protocol_7_servers_still_work() {
        let (port, seen) = scripted_as(
            Arc::new(|_| "PONG 7\n".to_string()),
            Arc::new(|cmd, _| match verb(cmd) {
                "TAPS" => Act::Close,
                "SETTEXT" => Act::Say(b"OK 0\n".to_vec()),
                _ => Act::Say(b"OK 0\n".to_vec()),
            }),
        )
        .await;
        let (ts, _fake) = attached("ts-proto7", port).await;
        assert_eq!(ts.key("dev", 4).await, Some(Ok(())));
        assert_eq!(
            ts.set_text("dev", "x", true).await,
            Some(Ok(SetText::Refused))
        );
        let err = ts.taps("dev", 10, 20, 1).await.unwrap().unwrap_err();
        assert!(is_outcome_unknown(&err), "{err}");
        let log = seen.lock().unwrap().clone();
        assert_eq!(log[0], "KEY 4");
        assert!(log.iter().all(|c| !c.starts_with('#')), "{log:?}");
        assert_eq!(count(&seen, "TAPS"), 1);
    }

    /// VEL-S48: the axis goes with the scroll, so the server can skip a
    /// carousel under a vertical scroll.
    #[tokio::test]
    async fn scroll_node_names_its_axis() {
        let (port, seen) = scripted(Arc::new(|_, _| Act::Say(b"OK 1\n".to_vec()))).await;
        let (ts, _fake) = attached("ts-axis", port).await;
        assert_eq!(
            ts.scroll_node("dev", 5, 6, true, true).await,
            Some(Ok(true))
        );
        assert_eq!(
            ts.scroll_node("dev", 5, 6, false, false).await,
            Some(Ok(true))
        );
        let log = seen.lock().unwrap().clone();
        assert!(log[0].ends_with(" SCROLLNODE 5 6 1 v"), "{log:?}");
        assert!(log[1].ends_with(" SCROLLNODE 5 6 -1 h"), "{log:?}");
    }

    #[test]
    fn versions_are_negotiated() {
        assert_eq!(parse_pong("PONG 7\n"), Some(("7".into(), None)));
        assert_eq!(
            parse_pong("PONG 8 1f2e\n"),
            Some(("8".into(), Some("1f2e".into())))
        );
        assert_eq!(parse_pong("ERR peer uid 10123 not allowed"), None);
        assert_eq!(supported("7"), Some(7));
        assert_eq!(supported("8"), Some(8));
        assert_eq!(supported("6"), None);
        assert_eq!(supported("9"), Some(9));
        assert_eq!(supported("10"), Some(10));
        assert_eq!(supported("11"), Some(11));
        assert_eq!(supported("12"), Some(12));
        assert_eq!(supported("13"), Some(13));
        assert_eq!(supported("14"), None);
        assert_eq!(supported("x"), None);
        assert_eq!(verb("#ab-1 TAPS 1 2 1"), "TAPS");
        assert!(mutates("#ab-1 TEXT 68"));
    }

    /// VEL-164: SETTEXT says whether the field took the text, refused it,
    /// or took it and reads back something else.
    #[test]
    fn settext_replies_three_ways() {
        assert_eq!(parse_set_text("OK 1"), Ok(SetText::Applied));
        assert_eq!(parse_set_text("OK 0"), Ok(SetText::Refused));
        assert_eq!(
            parse_set_text("OK 2 34312031"),
            Ok(SetText::Differs("41 1".into()))
        );
        assert_eq!(parse_set_text("OK 2"), Ok(SetText::Differs(String::new())));
        assert!(parse_set_text("OK 2 3").is_err());
        assert!(parse_set_text("OK 2 zz").is_err());
        assert!(parse_set_text("OK").is_err());
    }

    /// P11: long text goes in 256-character commands, and once some of it
    /// is typed, losing the server is an error, never "type it all over
    /// adb".
    #[tokio::test]
    async fn long_text_is_typed_in_chunks() {
        let (port, seen) = scripted(Arc::new(|_, _| Act::Say(b"OK 0\n".to_vec()))).await;
        let (ts, _fake) = attached("ts-chunks", port).await;
        let text: String = "é".repeat(600);
        assert_eq!(ts.text("dev", &text).await, Some(Ok(())));
        let sizes: Vec<usize> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.rsplit(' ').next().unwrap().len() / 4)
            .collect();
        assert_eq!(sizes, [256, 256, 88]);
        assert_eq!(ts.text("dev", "").await, Some(Ok(())));

        // The second chunk finds the server gone and it can't be restarted.
        let (port, _) = scripted(Arc::new(|_, n| {
            if n == 0 {
                Act::Say(b"OK 0\n".to_vec())
            } else {
                Act::Close
            }
        }))
        .await;
        let (ts, _fake) = attached("ts-chunks-gone", port).await;
        let r = ts.text("dev", &"a".repeat(300)).await;
        let err = r.expect("not a silent adb fallback").unwrap_err();
        assert!(err.contains("256 of 300"), "{err}");
    }

    /// NIT-7: an `ERR` the server sends can't be taken for "the outcome
    /// is unknown".
    #[tokio::test]
    async fn a_server_error_never_reads_as_unknown_outcome() {
        let (port, _) = scripted(Arc::new(|_, _| {
            Act::Say(b"ERR boom; whether it ran is unknown\n".to_vec())
        }))
        .await;
        let (ts, _fake) = attached("ts-err-text", port).await;
        let err = ts.key("dev", 4).await.unwrap().unwrap_err();
        assert!(!is_outcome_unknown(&err), "{err}");
    }

    #[test]
    fn backoff_doubles_to_a_minute() {
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(6), Duration::from_secs(32));
        assert_eq!(backoff(7), Duration::from_secs(60));
        assert_eq!(backoff(1000), Duration::from_secs(60));
        let mut st = DeviceState::default();
        st.failed("d", "another UI automation tool (Maestro…) is connected");
        assert!(st.disabled);
    }

    #[test]
    fn only_device_changing_commands_are_sent_once() {
        for m in [
            "TAP 1 2",
            "TAPS 1 2 2",
            "TEXT 6869",
            "SETTEXT a 68",
            "KEY 4",
            "SWIPE 1 2 3 4 5",
        ] {
            assert!(mutates(m), "{m}");
        }
        for r in [
            "PING",
            "DUMP",
            "DUMP lite",
            "SHOT png",
            "WINDOWS",
            "MARK",
            "SETTLE 1 2 3",
        ] {
            assert!(!mutates(r), "{r}");
        }
    }

    /// VEL-358: the jar went to a guessable path with plain `fs::write`,
    /// following a symlink planted there.
    #[test]
    fn the_temp_jar_is_private_and_never_follows_a_planted_file() {
        use std::os::unix::fs::PermissionsExt;
        let planted =
            std::env::temp_dir().join(format!("velocity-tree-{}.jar", std::process::id()));
        let _ = std::fs::remove_file(&planted);
        std::fs::write(&planted, b"planted").unwrap();
        let a = write_private_temp("velocity-tree", ".jar", b"jar").unwrap();
        let b = write_private_temp("velocity-tree", ".jar", b"jar").unwrap();
        assert_ne!(a, b);
        assert_ne!(a, planted);
        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        for p in [a, b, planted] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn embedded_jar_is_a_dex_jar() {
        // A zip (PK\x03\x04) holding classes.dex.
        assert_eq!(&SERVER_JAR[..4], b"PK\x03\x04");
        assert!(SERVER_JAR.windows(11).any(|w| w == b"classes.dex"));
    }

    /// The jar was rebuilt with `Server.java`'s protocol 13 (protocol 9's
    /// hint and state in dumps and no password append, the Compose
    /// `test-tag` of D-53, 11's `role-desc`, S1, and 13's AUTH gate, R-2);
    /// a forgotten `build.sh` leaves older strings.
    #[test]
    fn embedded_jar_speaks_protocol_13() {
        assert_eq!(SERVER_VERSION, "13");
        assert_eq!(supported("7"), Some(7));
        assert_eq!(supported("8"), Some(8));
        assert_eq!(supported("9"), Some(9));
        assert_eq!(supported("10"), Some(10));
        assert_eq!(supported("11"), Some(11));
        assert_eq!(supported("12"), Some(12));
        assert_eq!(supported("13"), Some(13));
        assert_eq!(supported("14"), None);
        // The dex is stored deflated: inflate it with the system `unzip`.
        let jar = write_private_temp("velocity-tree-test", ".jar", SERVER_JAR).unwrap();
        let out = std::process::Command::new("unzip")
            .args(["-p", &jar.to_string_lossy(), "classes.dex"])
            .output();
        let _ = std::fs::remove_file(&jar);
        let Ok(out) = out else { return };
        if !out.status.success() {
            return;
        }
        for marker in [
            &b"is still running"[..],
            b"not allowed",
            b"truncated",
            b"state-desc",
            b"set text on a password field",
            b"test-tag",
            b"androidx.compose.ui.semantics.testTag",
            b"notag",
            b"role-desc",
            b"AccessibilityNodeInfo.roleDescription",
            b"ENCODERS",
            b"Lcom/velocity/tree/VideoServer;",
            b"max-fps-to-encoder",
            b"createVirtualDisplay",
        ] {
            assert!(
                out.stdout.windows(marker.len()).any(|w| w == marker),
                "{}",
                String::from_utf8_lossy(marker)
            );
        }
    }

    #[tokio::test]
    async fn disabled_by_env_returns_none() {
        let server = TreeServer {
            adb_path: "adb".into(),
            enabled: false,
            devices: Default::default(),
        };
        assert!(server.dump("emulator-5554").await.is_none());
    }

    /// VEL-310: extreme bounds overflowed the subtraction.
    #[test]
    fn window_bounds_saturate() {
        let w = parse_windows("1 1 -2147483648,0,2147483647,10 x\n");
        assert_eq!(w[0].bounds.width, i32::MAX);
    }

    #[test]
    fn parses_encoders() {
        let e = parse_encoders("c2.android.avc.encoder 0\nOMX.hw.avc 1\nold ?\n\n");
        assert_eq!(
            e,
            vec![
                ("c2.android.avc.encoder".to_string(), Some(false)),
                ("OMX.hw.avc".to_string(), Some(true)),
                ("old".to_string(), None)
            ]
        );
    }

    #[test]
    fn parses_windows_front_first() {
        let w = parse_windows("12 2 0,1500,1080,2400 com.google.android.inputmethod.latin\n3 1 0,0,1080,2400 com.app\n");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].kind_name(), "keyboard");
        assert_eq!(w[0].bounds.y, 1500);
        assert_eq!(w[0].bounds.height, 900);
        assert_eq!(w[1].package, "com.app");
    }

    /// Runs against a real device when one is named in VELOCITY_TEST_ANDROID.
    #[tokio::test]
    async fn dumps_from_a_real_device() {
        let Ok(device) = std::env::var("VELOCITY_TEST_ANDROID") else {
            return;
        };
        let server = TreeServer::new();
        let xml = server.dump(&device).await.expect("tree server dump");
        assert!(xml.starts_with("<?xml") && xml.contains("<hierarchy"));
        let start = std::time::Instant::now();
        for _ in 0..5 {
            server.dump(&device).await.expect("repeat dump");
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        let lite = server.dump_lite(&device).await.expect("lite dump");
        assert!(
            lite.len() < xml.len(),
            "lite {} vs full {}",
            lite.len(),
            xml.len()
        );
        assert!(!lite.contains("clickable="));
        let png = server.screenshot(&device, false).await.unwrap().unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        let windows = server.windows(&device).await.unwrap().unwrap();
        assert!(windows.iter().any(|w| w.kind == 1), "{windows:?}");
        server.mark(&device).await.unwrap().unwrap();
        let (settled, _, ms) = server
            .settle(&device, 100, 50, 2000)
            .await
            .unwrap()
            .unwrap();
        assert!(settled && ms < 2000);
        // Recents always changes the screen; the key goes through the server.
        server.key(&device, 187).await.unwrap().unwrap();
        let (_, reacted, _) = server
            .settle(&device, 300, 150, 3000)
            .await
            .unwrap()
            .unwrap();
        server.key(&device, 3).await.unwrap().unwrap();
        assert!(reacted, "opening recents produced no accessibility events");
        server.stop_all().await;
    }
}
