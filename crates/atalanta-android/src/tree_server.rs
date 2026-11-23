// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Client for the on-device UI-tree server (`probes/android/atalanta-tree-server`).
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
//! `MAX_FAILURES` in a row the server is left alone for a while (2 s,
//! doubling to 60 s) and then tried again. Disable it with
//! `ATALANTA_ANDROID_TREE_SERVER=0`.
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
//! The server writes a fresh token to `TOKEN_FILE` on the device (shell
//! or root only) at each start; the host reads it through adb (R-2, R-3).
//!
//! Since protocol 18 the handshake is mutual (`CHALLENGE_VERSION`): the
//! socket name is public, and an app that bound it first used to answer
//! `PONG` and receive the token and every typed secret. Now the host sends
//! `HELLO <nonce>` and checks the server's HMAC over it with the token
//! before it sends anything else, and authenticates with an HMAC of its
//! own: the token never crosses the socket.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// The dex jar built by `probes/android/atalanta-tree-server/build.sh`.
const SERVER_JAR: &[u8] = include_bytes!("../assets/atalanta-tree.jar");
/// Must match `Server.VERSION` in the embedded jar.
const SERVER_VERSION: &str = "19";
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
/// (`com.atalanta.tree.VideoServer`, see [`crate::video`]) in the same jar;
/// the tree commands are unchanged. Protocol 13 (R-2) closes the
/// connection on any line that isn't a command and requires `AUTH <token>`
/// first; the commands themselves are unchanged. Protocol 18 makes the
/// handshake mutual ([`CHALLENGE_VERSION`]), and with a token in hand the
/// host speaks to nothing older (tests' stand-ins without a token still
/// exercise 7 to 17). Protocol 19 adds whether each `WINDOWS` line is the
/// active window ([`Window::active`]), dumps only same-app popups and
/// reads the active window's display, and takes the host's wait in the
/// request tag (`#<id>@<ms>`, see [`TreeServer::call`]).
const OLDEST_VERSION: u32 = 7;
/// The first protocol whose server proves it holds the token (`HELLO` →
/// `PROOF`) and takes an HMAC, not the token, in `AUTH`. With a token in
/// hand the host talks to no older server: one that can't prove itself
/// may be an app squatting on the socket name (security review #2).
const CHALLENGE_VERSION: u32 = 18;
/// Where a protocol 13 server leaves its session token on the device:
/// readable by its own uid (shell or root) only.
const TOKEN_FILE: &str = "/data/local/tmp/atalanta-tree.token";
/// Text sent per `TEXT` command: long strings typed as one burst of key
/// events overran the keyboard (Appium and Maestro chunk too).
const TEXT_CHUNK: usize = 256;
const SOCKET_NAME: &str = "atalanta_tree";
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

/// `ATALANTA_ANDROID_KEEP_A11Y=1`: don't suppress other accessibility
/// services. A server already running keeps the mode it started with.
fn keep_a11y() -> bool {
    matches!(
        atalanta_common::env::var("ATALANTA_ANDROID_KEEP_A11Y").as_deref(),
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
    /// server left running by an earlier Atalanta process.
    _child: Option<atalanta_common::process::GroupChild>,
    /// Removes `port`'s `adb forward` if the session is dropped without
    /// [`TreeServer::release`] (a cancelled caller); `None` in tests.
    forward: Option<ForwardGuard>,
}

/// An `adb forward` that is removed when this is dropped armed: a caller
/// cancelled between `adb forward` and its `--remove` (a step deadline
/// during a start) used to leave the forward behind (review 01 #6).
struct ForwardGuard {
    adb_path: String,
    device_id: String,
    port: u16,
    armed: bool,
}

impl ForwardGuard {
    fn new(adb_path: &str, device_id: &str, port: u16) -> Self {
        Self {
            adb_path: adb_path.to_string(),
            device_id: device_id.to_string(),
            port,
            armed: true,
        }
    }

    /// The forward is removed (or handed on) by other means now.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (adb, device, port) = (
            std::mem::take(&mut self.adb_path),
            std::mem::take(&mut self.device_id),
            self.port,
        );
        debug!(device_id = %device, port, "removing an abandoned tree server forward");
        rt.spawn(async move {
            let _ = run_adb(
                &adb,
                &device,
                &["forward", "--remove", &format!("tcp:{port}")],
            )
            .await;
        });
    }
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
/// (D-53) when `ATALANTA_ANDROID_COMPOSE_TESTTAG=0`. A protocol 9 server
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
    /// A server was already running (another Atalanta's, or a kept one).
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
            atalanta_common::env::var("ATALANTA_ANDROID_TREE_SERVER").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        );
        Self {
            adb_path: atalanta_common::env::var("ATALANTA_ADB_PATH")
                .unwrap_or_else(|_| "adb".to_string()),
            enabled,
            devices: Default::default(),
        }
    }

    /// Run this `adb` instead of `ATALANTA_ADB_PATH` / `adb`.
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
                debug!(device_id, "tree server {}: {e}", verb(cmd));
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

    /// Type `text` into the focused field (any Unicode), `TEXT_CHUNK`
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
        let reply = self.call(device_id, "WINDOWS", true, DUMP_TIMEOUT).await?;
        let proto = self
            .slot(device_id)
            .lock()
            .await
            .session
            .as_ref()
            .map_or(OLDEST_VERSION, |s| s.proto);
        Some(reply.map(|(_, b)| parse_windows(&String::from_utf8_lossy(&b), proto)))
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
        if state.in_flight {
            // The session stays in `state` while it reconnects, and
            // `in_flight` is cleared only once the new connection is in:
            // a caller dropped meanwhile (a step deadline) leaves either
            // the old, still out-of-step connection marked as such, or
            // the new one; never a killed server or a stream read as
            // in step (review 01 #6).
            let reconnected = match state.session.as_mut() {
                Some(s) => self.fresh_connection(device_id, s).await,
                None => false,
            };
            if !reconnected {
                if let Some(s) = state.session.take() {
                    self.shutdown(device_id, s).await;
                }
                state.in_flight = false;
                return None;
            }
            state.in_flight = false;
        }
        let mutate = mutates(cmd);
        // Protocol 8: tag a command that changes the device, so that sending
        // it again (same id) after a lost reply can never run it twice.
        let id = mutate.then(next_request_id);
        for attempt in 0..2 {
            let st = &mut *state;
            let session = st.session.as_mut()?;
            let line = match &id {
                // Protocol 19: how long this host waits, so a resend whose
                // first run is still going doesn't park a server thread
                // for 60 s after the host gave up (review 01 #12). Older
                // servers take `<id>@<ms>` as the id, which is as unique.
                Some(id) if session.proto >= 19 => {
                    format!("#{id}@{} {cmd}", timeout.as_millis())
                }
                Some(id) if session.proto >= 8 => format!("#{id} {cmd}"),
                _ => cmd.to_string(),
            };
            st.in_flight = true;
            let sent = Instant::now();
            let outcome =
                tokio::time::timeout(timeout, request(&mut session.stream, &line, payload)).await;
            // The verb only: `TEXT` and `SETTEXT` carry the typed text (a
            // password), hex-encoded, which redaction doesn't know.
            debug!(
                device_id,
                cmd = verb(cmd),
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
                    let proto = state.session.as_ref().map_or(0, |s| s.proto);
                    if proto >= 8 {
                        // Same server, same id: answered from its cache if
                        // it ran, run now if it never arrived. The session
                        // stays in `state` (marked in flight) while it is
                        // resent: a dropped caller leaves it for the next
                        // command to reconnect, not killed (review 01 #6).
                        state.in_flight = true;
                        let resent = match state.session.as_mut() {
                            Some(s) => self.resend(device_id, s, &line, payload, timeout).await,
                            None => Err("no session".into()),
                        };
                        match resent {
                            Ok(reply) => {
                                state.in_flight = false;
                                state.succeeded();
                                debug!(device_id, "resent {} after a lost reply", verb(cmd));
                                return Some(match reply {
                                    Reply::Ok(l, b) => Ok((l, b)),
                                    Reply::Err(m) => Err(
                                        m.replace(OUTCOME_UNKNOWN, "whether it ran is not known")
                                    ),
                                });
                            }
                            Err(resend_why) => {
                                debug!(device_id, "no safe resend: {resend_why}");
                            }
                        }
                    }
                    state.in_flight = false;
                    if let Some(s) = state.session.take() {
                        self.release(device_id, s).await;
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
                        self.release(device_id, s).await;
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
                    state.failed(
                        device_id,
                        &format!("{:?} failed ({why}); falling back", verb(cmd)),
                    );
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
    /// record of the id and could run the command a second time. The new
    /// connection replaces `s`'s stream only once the reply is in, so a
    /// dropped caller leaves `s` as it was.
    async fn resend(
        &self,
        device_id: &str,
        s: &mut Session,
        line: &str,
        payload: bool,
        timeout: Duration,
    ) -> Result<Reply, String> {
        let (mut stream, proto, nonce) = handshake(
            &crate::adb::server_host(device_id),
            s.port,
            s.token.as_deref(),
        )
        .await?;
        if nonce.is_none() || nonce != s.nonce || proto != s.proto {
            return Err("the server restarted".into());
        }
        match tokio::time::timeout(timeout, request(&mut stream, line, payload)).await {
            Ok(Ok(reply)) => {
                s.stream = stream;
                Ok(reply)
            }
            Ok(Err(
                RequestError::Stale(e) | RequestError::Unanswered(e) | RequestError::Broken(e),
            )) => Err(e),
            Err(_) => Err("timed out".into()),
        }
    }

    /// `atalanta doctor --deep`: push the jar (unless this build is
    /// already on the device) and handshake. A server already listening is
    /// PINGed and left running (it may be another run's). Otherwise, only
    /// when `may_start` (the caller holds the device's lease), one is
    /// started, PINGed and stopped again; without it `version` is `None`.
    pub async fn probe(&self, device_id: &str, may_start: bool) -> Result<Probe, String> {
        let jar = self.push_jar(device_id).await?;
        let host = crate::adb::server_host(device_id);
        if let Ok(port) = self.forward(device_id).await {
            let mut guard = ForwardGuard::new(&self.adb_path, device_id, port);
            let pinged = match connect(&host, port).await {
                Ok(mut stream) => ping(&mut stream).await.ok(),
                Err(_) => None,
            };
            self.unforward(device_id, port).await;
            guard.disarm();
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

    /// Remove every Atalanta tree-server jar from the device and push this
    /// build's again (`atalanta doctor --fix` after a failed probe).
    pub async fn repush(&self, device_id: &str) -> Result<String, String> {
        self.adb(
            device_id,
            &["shell", "rm -f /data/local/tmp/atalanta-tree-*.jar"],
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
            self.release(device_id, s).await;
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
        // speaks our version (and proves it is ours), otherwise ask it to quit.
        let mut replaced = false;
        if let Ok(port) = self.forward(device_id).await {
            let mut guard = ForwardGuard::new(&self.adb_path, device_id, port);
            if let Ok(mut stream) = connect(&crate::adb::server_host(device_id), port).await {
                match ping(&mut stream).await {
                    Ok((v, nonce)) if v == SERVER_VERSION => {
                        // Its token is in the device file it wrote at start;
                        // a server that can't prove it holds that token is
                        // asked to quit (QUIT needs no token) and replaced,
                        // and is never sent the token.
                        let proto = supported(&v).unwrap_or(OLDEST_VERSION);
                        let token = self.read_token(device_id).await;
                        match &token {
                            Ok(t) => {
                                match present_token(&mut stream, proto, nonce.as_deref(), Some(t))
                                    .await
                                {
                                    Ok(()) => {
                                        debug!(device_id, port, "attached to running tree server");
                                        return Ok(Session {
                                            stream,
                                            port,
                                            proto,
                                            nonce,
                                            token: token.ok(),
                                            _child: None,
                                            forward: Some(guard),
                                        });
                                    }
                                    Err(e) => warn!(device_id, "running tree server: {e}"),
                                }
                            }
                            Err(e) => debug!(device_id, "running tree server: {e}"),
                        }
                        let _ = stream.get_mut().write_all(b"QUIT\n").await;
                        replaced = true;
                    }
                    Ok(_) => {
                        let _ = stream.get_mut().write_all(b"QUIT\n").await;
                        replaced = true;
                    }
                    Err(_) => {}
                }
            }
            if replaced {
                self.wait_gone(device_id, port).await;
            }
            self.unforward(device_id, port).await;
            guard.disarm();
        }

        match self.spawn_server(device_id).await {
            // The server we just asked to quit may still hold UiAutomation:
            // its registration goes when system_server sees it die. Give it
            // a second and try once more before blaming another tool
            // (review 01 #2).
            Err(e) if replaced && e.contains(ANOTHER_CLIENT) => {
                debug!(device_id, "old tree server still registered; retrying: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.spawn_server(device_id).await
            }
            other => other,
        }
    }

    /// After `QUIT`: until the old server stops answering `PING` (at most
    /// ~3 s), instead of a fixed 300 ms that a loaded emulator outlasts.
    async fn wait_gone(&self, device_id: &str, port: u16) {
        let host = crate::adb::server_host(device_id);
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let alive = match connect(&host, port).await {
                Ok(mut stream) => ping(&mut stream).await.is_ok(),
                Err(_) => false,
            };
            if !alive {
                return;
            }
        }
        debug!(device_id, "old tree server still answering after QUIT");
    }

    /// Push the jar, start the server and open a session to it.
    async fn spawn_server(&self, device_id: &str) -> Result<Session, String> {
        let remote = self.push_jar(device_id).await?;
        // Its own process group, registered for the second-signal kill:
        // `std::process::exit` skips `kill_on_drop` (review d4 m2).
        let mut child = atalanta_common::process::spawn_group(
            Command::new(&self.adb_path)
                .args(crate::adb::target_args(device_id))
                .args([
                    "shell",
                    &format!(
                        "CLASSPATH={remote}:/system/framework/uiautomator.jar \
                     exec app_process /system/bin com.atalanta.tree.Server{}",
                        // Keep TalkBack and other accessibility services running
                        // while Atalanta drives the device (off: they're paused).
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
            if joined.contains("already bound") {
                // Protocol 18 says so in words: something else holds
                // @atalanta_tree. If it isn't a Atalanta server, it may be
                // an app waiting for typed text: worth a loud warning.
                warn!(
                    device_id,
                    "the tree server's socket @{SOCKET_NAME} is held by another process \
                     on the device (maybe an app impersonating it); using adb instead"
                );
                return Err(format!(
                    "@{SOCKET_NAME} is already bound on the device by another process: {joined}"
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
        let mut guard = ForwardGuard::new(&self.adb_path, device_id, port);
        let token = match self.read_token(device_id).await {
            Ok(t) => Some(t),
            Err(e) => {
                self.unforward(device_id, port).await;
                guard.disarm();
                return Err(e);
            }
        };
        let mut session = match open_session(
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
                guard.disarm();
                return Err(e);
            }
        };
        session.forward = Some(guard);
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
    /// `s` keeps its old stream unless the new one is ready: false when
    /// none could be opened (the caller shuts the session down).
    async fn fresh_connection(&self, device_id: &str, s: &mut Session) -> bool {
        debug!(device_id, "tree server request was abandoned; reconnecting");
        let started = std::time::Instant::now();
        let host = crate::adb::server_host(device_id);
        match handshake(&host, s.port, s.token.as_deref()).await {
            Ok((stream, proto, nonce)) if proto == s.proto => {
                debug!(
                    device_id,
                    ms = started.elapsed().as_millis() as u64,
                    "reconnected"
                );
                s.stream = stream;
                s.nonce = nonce;
                true
            }
            other => {
                debug!(
                    device_id,
                    ok = other.is_ok(),
                    ms = started.elapsed().as_millis() as u64,
                    "reconnect failed"
                );
                false
            }
        }
    }

    /// Remove `s`'s forward and let it go (its server, if we started it,
    /// is killed with it). Unforwarded first and only then disarmed: a
    /// drop in between still removes the forward.
    async fn release(&self, device_id: &str, mut s: Session) {
        self.unforward(device_id, s.port).await;
        if let Some(f) = s.forward.as_mut() {
            f.disarm();
        }
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
        self.release(device_id, s).await;
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
    let out = atalanta_common::process::output_with_timeout(&mut cmd, Duration::from_secs(30))
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
        "/data/local/tmp/atalanta-tree-{:016x}.jar",
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
    let local = write_private_temp("atalanta-tree", ".jar", SERVER_JAR)
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
    child: Option<atalanta_common::process::GroupChild>,
    token: Option<String>,
) -> Result<Session, String> {
    let (stream, proto, nonce) = handshake(host, port, token.as_deref()).await?;
    Ok(Session {
        stream,
        port,
        proto,
        nonce,
        token,
        _child: child,
        forward: None,
    })
}

/// A new connection to the server on `port`, past `PING` and (with a
/// token) the mutual handshake: the stream, the protocol and the boot
/// nonce.
async fn handshake(
    host: &str,
    port: u16,
    token: Option<&str>,
) -> Result<(BufReader<TcpStream>, u32, Option<String>), String> {
    let mut stream = connect(host, port).await?;
    let (version, nonce) = ping(&mut stream).await?;
    let proto = supported(&version).ok_or_else(|| {
        format!("server version {version}, expected {OLDEST_VERSION} to {SERVER_VERSION}")
    })?;
    present_token(&mut stream, proto, nonce.as_deref(), token).await?;
    Ok((stream, proto, nonce))
}

/// The mutual handshake when there is a token (tests talk to stand-in
/// servers without one). With a token, a server older than
/// [`CHALLENGE_VERSION`] is refused: it can't prove it holds the token, so
/// it may be an app that bound the public socket name, and the old `AUTH`
/// would hand it the token.
async fn present_token(
    stream: &mut BufReader<TcpStream>,
    proto: u32,
    boot_nonce: Option<&str>,
    token: Option<&str>,
) -> Result<(), String> {
    let Some(token) = token else { return Ok(()) };
    if proto < CHALLENGE_VERSION {
        return Err(format!(
            "tree server at protocol {proto} can't prove it holds the session token \
             (needs {CHALLENGE_VERSION}); not sending it"
        ));
    }
    let Some(boot_nonce) = boot_nonce else {
        return Err("tree server sent no boot nonce; not authenticating".into());
    };
    let client_nonce = client_nonce();
    let fut = async {
        let proof = exchange(stream, &format!("HELLO {client_nonce}")).await?;
        let want = server_proof(token, boot_nonce, &client_nonce);
        let got = proof.strip_prefix("PROOF ").unwrap_or("");
        if !constant_time_eq(got.as_bytes(), want.as_bytes()) {
            return Err(format!(
                "the process on @{SOCKET_NAME} could not prove it is Atalanta's tree server \
                 (another app may have bound the socket name); nothing was sent to it"
            ));
        }
        let reply = exchange(
            stream,
            &format!("AUTH {}", client_proof(token, boot_nonce, &client_nonce)),
        )
        .await?;
        if reply == "OK 0" {
            Ok(())
        } else {
            Err(format!("tree server refused the token: {reply:?}"))
        }
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .map_err(|_| "auth timed out (2s)".to_string())?
}

/// Send one line and read one reply line (at most [`MAX_HEADER`] bytes).
async fn exchange(stream: &mut BufReader<TcpStream>, line: &str) -> Result<String, String> {
    stream
        .get_mut()
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    (&mut *stream)
        .take(MAX_HEADER)
        .read_until(b'\n', &mut reply)
        .await
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&reply).trim().to_string())
}

/// 128 random bits as hex, new for every handshake.
pub(crate) fn client_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// What the server answers `HELLO <client nonce>` with (`Server.serverProof`).
fn server_proof(token: &str, boot_nonce: &str, client_nonce: &str) -> String {
    hex(&hmac_sha256(
        token.as_bytes(),
        format!("atalanta-tree server {boot_nonce} {client_nonce}").as_bytes(),
    ))
}

/// What `AUTH` carries (`Server.clientProofMatches`): another message, so a
/// server's proof is never a client's.
fn client_proof(token: &str, boot_nonce: &str, client_nonce: &str) -> String {
    hex(&hmac_sha256(
        token.as_bytes(),
        format!("atalanta-tree client {boot_nonce} {client_nonce}").as_bytes(),
    ))
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn ping(stream: &mut BufReader<TcpStream>) -> Result<(String, Option<String>), String> {
    let fut = async {
        stream
            .get_mut()
            .write_all(b"PING\n")
            .await
            .map_err(|e| e.to_string())?;
        // Capped like every other reply: whatever holds the socket (an app
        // squatting `@atalanta_tree`) could stream bytes with no newline
        // and grow this buffer for the whole timeout (audit 2026-11 AND-2).
        let mut raw = Vec::new();
        (&mut *stream)
            .take(MAX_HEADER)
            .read_until(b'\n', &mut raw)
            .await
            .map_err(|e| e.to_string())?;
        let line = String::from_utf8_lossy(&raw).into_owned();
        parse_pong(&line).ok_or_else(|| format!("bad ping reply {:?}", clip_reply(&line)))
    };
    tokio::time::timeout(Duration::from_secs(2), fut)
        .await
        .map_err(|_| "ping timed out (2s)".to_string())?
}

/// A reply quoted in an error, cut short.
fn clip_reply(s: &str) -> String {
    s.chars().take(80).collect()
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
    pub bounds: atalanta_common::Rect,
    pub package: String,
    /// Whether the window takes touches (protocol 18, API 33+); `None`
    /// when the server can't tell.
    pub touchable: Option<bool>,
    /// The window's title (protocol 18), empty when none.
    pub title: String,
    /// Whether it is the active window, the one the dump is read from
    /// (protocol 19); `None` from an older server.
    pub active: Option<bool>,
}

impl Window {
    /// Whether a tap at a point inside this window can be taken by it, so
    /// it hides what is under it from a finger. A window with no touchable
    /// region, or SystemUI's rounded-corner / cutout strips (titled
    /// `ScreenDecorOverlay…`, `FLAG_NOT_TOUCHABLE`), passes touches through
    /// (review 09 #5).
    pub fn takes_touches(&self) -> bool {
        self.touchable != Some(false) && !self.title.starts_with("ScreenDecorOverlay")
    }

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

/// `WINDOWS` lines from a server speaking `proto`: `<layer> <type>
/// <l,t,r,b> <package>`, then (18) `<touchable 1|0|?>`, then (19) `<active
/// 1|0>`, then (18) the title to the end of the line.
fn parse_windows(text: &str, proto: u32) -> Vec<Window> {
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
            // Protocol 18: whether it takes touches, then its title.
            let touchable = match it.next() {
                Some("1") => Some(true),
                Some("0") => Some(false),
                _ => None,
            };
            let active = if proto >= 19 {
                match it.next() {
                    Some("1") => Some(true),
                    Some("0") => Some(false),
                    _ => None,
                }
            } else {
                None
            };
            let title = it.collect::<Vec<_>>().join(" ");
            let title = if title == "-" { String::new() } else { title };
            (b.len() == 4).then(|| Window {
                layer,
                kind,
                bounds: atalanta_common::Rect {
                    x: b[0],
                    y: b[1],
                    width: b[2].saturating_sub(b[0]),
                    height: b[3].saturating_sub(b[1]),
                },
                package,
                touchable,
                title,
                active,
            })
        })
        .collect()
}

/// Entry points for the cargo-fuzz targets in `fuzz/` (built only with
/// `--cfg fuzzing`, which `cargo fuzz` sets): the reply-line parsers stay
/// private to this module otherwise.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    /// Every reply-line parser on `text`, line by line and whole.
    pub fn parse_reply_lines(text: &str) {
        for line in text.lines() {
            let _ = super::parse_set_text(line);
            let _ = super::parse_pong(line);
            let _ = super::supported(line);
        }
        let _ = super::parse_encoders(text);
        for proto in [17, 18, 19] {
            let _ = super::parse_windows(text, proto);
        }
    }
}

/// Write `bytes` to a new file in the temp dir that only this user can
/// read. The name is unpredictable and the file must not exist yet, so a
/// file or symlink planted at a guessable path (the old
/// `atalanta-tree-<pid>.jar`) is never followed or overwritten (VEL-358).
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

    /// Audit 2026-11 AND-2: a squatter streaming bytes with no newline gets
    /// a quick "bad reply", not an unbounded buffer.
    #[tokio::test]
    async fn a_ping_reply_without_a_newline_is_cut_at_the_header_cap() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let junk = vec![b'A'; 1 << 20];
            for _ in 0..64 {
                if s.write_all(&junk).await.is_err() {
                    break;
                }
            }
        });
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut reader = BufReader::new(stream);
        let started = std::time::Instant::now();
        let e = ping(&mut reader).await.unwrap_err();
        assert!(e.contains("bad ping reply"), "{e}");
        assert!(e.len() < 200, "{}", e.len());
        assert!(started.elapsed() < Duration::from_secs(2));
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

    /// Review 01 #6: a caller dropped while the session reconnects (after an
    /// abandoned request) used to drop the moved-out session: the server was
    /// killed and the next call restarted it. Now it stays, still marked out
    /// of step, and the next call reconnects.
    #[tokio::test]
    async fn a_reconnect_cut_short_keeps_the_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((sock, _)) = listener.accept().await {
                n += 1;
                // Every connection after the first answers PING late.
                let slow_ping = n > 1;
                tokio::spawn(async move {
                    let mut sock = BufReader::new(sock);
                    let mut line = String::new();
                    while sock.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let cmd = line.trim().to_string();
                        line.clear();
                        let reply = match cmd.as_str() {
                            "PING" => {
                                if slow_ping {
                                    tokio::time::sleep(Duration::from_millis(300)).await;
                                }
                                format!("PONG {SERVER_VERSION}\n")
                            }
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
        let ts = TreeServer {
            adb_path: "adb".into(),
            enabled: true,
            devices: Default::default(),
        };
        ts.slot("dev").lock().await.session =
            Some(open_session("127.0.0.1", port, None, None).await.unwrap());
        let t = Duration::from_secs(2);
        let cut =
            tokio::time::timeout(Duration::from_millis(50), ts.call("dev", "SLOW", false, t)).await;
        assert!(cut.is_err());
        // Cut again, this time inside the reconnect's PING.
        let cut =
            tokio::time::timeout(Duration::from_millis(100), ts.call("dev", "FAST", false, t))
                .await;
        assert!(cut.is_err());
        {
            let slot = ts.slot("dev");
            let state = slot.lock().await;
            assert!(state.session.is_some(), "the session was dropped");
            assert!(state.in_flight, "the old stream passes for in step");
        }
        let reply = ts.call("dev", "FAST", false, t).await.unwrap().unwrap();
        assert_eq!(reply.0, "OK FAST");
    }

    /// A forward whose owner was dropped before `--remove` is removed.
    #[tokio::test]
    async fn an_abandoned_forward_is_removed() {
        let fake = crate::test_support::FakeAdb::new("abandoned-forward");
        fake.stub("adb-forward", "exit 0");
        drop(ForwardGuard::new(&fake.path(), "dev", 4321));
        let mut disarmed = ForwardGuard::new(&fake.path(), "dev", 4322);
        disarmed.disarm();
        drop(disarmed);
        let log = fake.dir.join("adb.log");
        let mut seen = String::new();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen = std::fs::read_to_string(&log).unwrap_or_default();
            if seen.contains("tcp:4321") {
                break;
            }
        }
        assert!(seen.contains("[forward][--remove][tcp:4321]"), "{seen}");
        assert!(!seen.contains("tcp:4322"), "{seen}");
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
        // A boot nonce: protocol 18's handshake (after a restart through
        // the fake adb, which holds a token) needs one.
        scripted_as(
            Arc::new(|_| format!("PONG {SERVER_VERSION} b007\n")),
            script,
        )
        .await
    }

    /// [`scripted`], answering `PING` with `pong(connection number)`.
    async fn scripted_as(
        pong: Arc<dyn Fn(usize) -> String + Send + Sync>,
        script: Script,
    ) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        scripted_proving(pong, script, false).await
    }

    /// [`scripted_as`]; with `lie`, a server that doesn't know the token
    /// (an app squatting on the socket name) and answers `HELLO` with a
    /// made-up proof.
    async fn scripted_proving(
        pong: Arc<dyn Fn(usize) -> String + Send + Sync>,
        script: Script,
        lie: bool,
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
                    let mut hello: Option<String> = None;
                    while sock.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let cmd = line.trim().to_string();
                        line.clear();
                        if cmd == "PING" {
                            let _ = sock.get_mut().write_all(pong.as_bytes()).await;
                            continue;
                        }
                        // Protocol 18's handshake, as Server.java does it.
                        let tok = crate::test_support::FAKE_TREE_TOKEN;
                        let boot = parse_pong(&pong).and_then(|(_, n)| n).unwrap_or_default();
                        if let Some(cn) = cmd.strip_prefix("HELLO ") {
                            log.lock().unwrap().push(cmd.clone());
                            hello = Some(cn.to_string());
                            let proof = if lie {
                                "0".repeat(64)
                            } else {
                                server_proof(tok, &boot, cn)
                            };
                            let _ = sock
                                .get_mut()
                                .write_all(format!("PROOF {proof}\n").as_bytes())
                                .await;
                            continue;
                        }
                        if let Some(proof) = cmd.strip_prefix("AUTH ") {
                            log.lock().unwrap().push(cmd.clone());
                            let ok = hello
                                .as_deref()
                                .is_some_and(|cn| proof == client_proof(tok, &boot, cn));
                            if !ok {
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

    /// Typed text (a password) travels as `TEXT <hex>` / `SETTEXT <mode>
    /// <hex>`; the debug log of every reply named the whole command, hex
    /// included, which secret redaction (exact values) can't recognise.
    #[test]
    fn typed_text_never_reaches_the_log() {
        let secret = "hunter2-Zq9-not-a-real-password";
        let ((seen, typed), logs) = crate::test_support::capture_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (port, seen) = scripted(Arc::new(|_, _| Act::Say(b"OK 1\n".to_vec()))).await;
                let (ts, _fake) = attached("ts-log-secret", port).await;
                let a = ts.text("dev", secret).await;
                let b = ts.set_text("dev", secret, true).await;
                (seen, (a.is_some(), b.is_some()))
            })
        });
        assert_eq!(typed, (true, true));
        let wire = hex(secret.as_bytes());
        let sent = seen.lock().unwrap().join("\n");
        assert!(
            sent.contains(&wire),
            "the server never got the text: {sent}"
        );
        assert!(
            logs.contains("tree server reply"),
            "nothing was logged: {logs}"
        );
        assert!(logs.contains("TEXT"), "{logs}");
        assert!(!logs.contains(&wire) && !logs.contains(secret), "{logs}");
    }

    /// VEL-158: a reply lost after the tap was sent was taken for a stale
    /// connection: the server was restarted and the tap sent again.
    #[tokio::test]
    async fn read_error_after_write_is_not_replayed_for_taps() {
        // A new boot nonce per connection: every reconnect reaches a
        // "restarted" server, which has no record of the tap's id (with the
        // same nonce the id would be resent, and the server dedupes it).
        let (port, seen) = scripted_as(
            Arc::new(|conn| format!("PONG {SERVER_VERSION} n{conn}\n")),
            Arc::new(|cmd, _| match verb(cmd) {
                "TAPS" => Act::Close,
                _ => Act::Say(b"OK\n".to_vec()),
            }),
        )
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
        assert!(!log[0].contains('@'), "protocol 8 has no wait in its tag");
    }

    /// Review 01 #12: from protocol 19 the tag carries how long the host
    /// waits, so a resend never parks a server thread longer.
    #[tokio::test]
    async fn protocol_19_tags_carry_the_hosts_wait() {
        let (port, seen) = scripted_as(
            Arc::new(|_| "PONG 19 b007\n".to_string()),
            Arc::new(|_, _| Act::Say(b"OK 0\n".to_vec())),
        )
        .await;
        let (ts, _fake) = attached("ts-wait-tag", port).await;
        assert_eq!(ts.key("dev", 4).await, Some(Ok(())));
        let log = seen.lock().unwrap().clone();
        let key = log.iter().find(|c| verb(c) == "KEY").unwrap();
        let tag = key.split_whitespace().next().unwrap();
        assert!(tag.starts_with('#') && tag.contains('@'), "{key}");
        let ms: u64 = tag.rsplit('@').next().unwrap().parse().unwrap();
        assert!(ms > 0 && ms <= 60_000, "{key}");
    }

    /// R-2 and security review #2: a session runs the protocol 18
    /// handshake first, and again on the new connection a resend opens. The
    /// token itself never goes down the socket; a server that refuses the
    /// proof is no session at all.
    #[tokio::test]
    async fn protocol_18_authenticates_both_ways_on_every_connection() {
        let (port, seen) = scripted_as(
            Arc::new(|_| "PONG 18 b007\n".to_string()),
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
        assert_eq!(
            verbs,
            ["HELLO", "AUTH", "TAPS", "HELLO", "AUTH", "TAPS"],
            "{log:?}"
        );
        assert!(log.iter().all(|c| !c.contains(&tok)), "token sent: {log:?}");
        // A fresh client nonce per connection.
        assert_ne!(log[0], log[3]);

        let refused = open_session("127.0.0.1", port, None, Some("f".repeat(32))).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("could not prove")),
            "{:?}",
            refused.map(|_| ())
        );
    }

    /// Security review #2: something on the socket that can't prove it
    /// holds the token (an app that bound `@atalanta_tree` first, or an old
    /// server that would take the token in the clear) is sent nothing
    /// after `HELLO`: no `AUTH`, no token, no command.
    #[tokio::test]
    async fn an_impostor_server_gets_neither_token_nor_commands() {
        let tok = crate::test_support::FAKE_TREE_TOKEN.to_string();
        let (port, seen) = scripted_proving(
            Arc::new(|_| "PONG 18 b007\n".to_string()),
            Arc::new(|_, _| Act::Say(b"OK 0\n".to_vec())),
            true,
        )
        .await;
        let err = open_session("127.0.0.1", port, None, Some(tok.clone()))
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("could not prove"), "{err}");
        let log = seen.lock().unwrap().clone();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("HELLO "), "{log:?}");

        // A server claiming an older protocol gets nothing at all: from
        // 13 to 17 it would have been sent the token.
        for v in ["17", "13", "12", "7"] {
            let (old, seen) = scripted_as(
                Arc::new(move |_| format!("PONG {v} b007\n")),
                Arc::new(|_, _| Act::Say(b"OK 0\n".to_vec())),
            )
            .await;
            let err = open_session("127.0.0.1", old, None, Some(tok.clone()))
                .await
                .map(|_| ())
                .unwrap_err();
            assert!(err.contains("can't prove"), "{v}: {err}");
            assert!(seen.lock().unwrap().is_empty(), "{v}: sent to");
        }
    }

    /// The HMAC both sides compute: RFC 4231 test case 2 (Server.java's
    /// HostChecks pins the same vector), and the two proofs differ.
    #[test]
    fn hmac_matches_the_rfc_and_the_proofs_are_distinct() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (RFC 4231 case 6).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        let t = crate::test_support::FAKE_TREE_TOKEN;
        assert_ne!(server_proof(t, "b", "c"), client_proof(t, "b", "c"));
        assert_ne!(server_proof(t, "b", "c"), server_proof(t, "b2", "c"));
        assert_eq!(client_nonce().len(), 32);
        assert_ne!(client_nonce(), client_nonce());
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
        assert_eq!(supported("14"), Some(14));
        assert_eq!(supported("15"), Some(15));
        assert_eq!(supported("16"), Some(16));
        assert_eq!(supported("17"), Some(17));
        assert_eq!(supported("18"), Some(18));
        assert_eq!(supported("19"), Some(19));
        assert_eq!(supported("20"), None);
        assert_eq!(supported("x"), None);
        assert_eq!(verb("#ab-1@10000 TAP 1 2"), "TAP");
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
            std::env::temp_dir().join(format!("atalanta-tree-{}.jar", std::process::id()));
        let _ = std::fs::remove_file(&planted);
        std::fs::write(&planted, b"planted").unwrap();
        let a = write_private_temp("atalanta-tree", ".jar", b"jar").unwrap();
        let b = write_private_temp("atalanta-tree", ".jar", b"jar").unwrap();
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

    /// The jar was rebuilt with `Server.java`'s protocol 14 (protocol 9's
    /// hint and state in dumps and no password append, the Compose
    /// `test-tag` of D-53, 11's `role-desc`, S1, 13's AUTH gate, R-2, and
    /// 14's finger touches, which the Files app requires, 15's popup
    /// windows in front of the active one, and 16's read from a cleared
    /// accessibility cache, and 17's empty text for a field showing its
    /// placeholder, and 18's mutual HELLO/AUTH handshake, gated cache
    /// clears and `indeterminate` spinners); a forgotten
    /// `build.sh` leaves older strings.
    #[test]
    fn embedded_jar_speaks_protocol_19() {
        assert_eq!(supported("7"), Some(7));
        assert_eq!(supported("8"), Some(8));
        assert_eq!(supported("9"), Some(9));
        assert_eq!(supported("10"), Some(10));
        assert_eq!(supported("11"), Some(11));
        assert_eq!(supported("12"), Some(12));
        assert_eq!(supported("13"), Some(13));
        assert_eq!(supported("14"), Some(14));
        assert_eq!(supported("15"), Some(15));
        assert_eq!(supported("16"), Some(16));
        assert_eq!(supported("17"), Some(17));
        assert_eq!(supported("18"), Some(18));
        assert_eq!(supported("19"), Some(19));
        assert_eq!(supported("20"), None);
        // The dex is stored deflated: inflate it with the system `unzip`.
        let jar = write_private_temp("atalanta-tree-test", ".jar", SERVER_JAR).unwrap();
        let out = std::process::Command::new("unzip")
            .args(["-p", &jar.to_string_lossy(), "classes.dex"])
            .output();
        let _ = std::fs::remove_file(&jar);
        // Never a vacuous pass (review 07 H4): without `unzip` the jar went
        // unchecked and the test still reported PASS.
        let out = out.expect("`unzip` is needed to read the embedded jar");
        assert!(
            out.status.success(),
            "unzip couldn't read classes.dex: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The version the host expects is the one compiled into the jar:
        // javac folds `"PONG " + VERSION + " "` into one constant. This, not
        // `SERVER_VERSION` compared with itself, catches a stale jar.
        for marker in [
            format!("PONG {SERVER_VERSION} "),
            format!("atalanta-tree {SERVER_VERSION} listening on @"),
        ] {
            assert!(
                out.stdout.windows(marker.len()).any(|w| w == marker.as_bytes()),
                "the embedded jar doesn't speak protocol {SERVER_VERSION} ({marker:?} missing): rerun build.sh"
            );
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
            b"Lcom/atalanta/tree/VideoServer;",
            b"max-fps-to-encoder",
            b"PointerProperties",
            b"popupsAbove",
            b"createVirtualDisplay",
            // 18: the mutual handshake, the gated cache clear (and its
            // pre-34 fallback), spinners, keyboard-sourced keys.
            b"atalanta-tree server ",
            b"PROOF ",
            b"AccessibilityInteractionClient",
            b"indeterminate",
            b"getRegionInScreen",
            b"listening on @",
            // 19: the active window's display, same-app popups, the
            // host's wait in the request tag, the video server's handshake.
            b"getWindowsOnAllDisplays",
            b"the host stopped waiting",
            b"atalanta-video server ",
            b"atalanta-video client ",
        ] {
            assert!(
                out.stdout.windows(marker.len()).any(|w| w == marker),
                "{}",
                String::from_utf8_lossy(marker)
            );
        }
    }

    /// Review 07 M5: CLAUDE.md, which agents read first, names the
    /// protocol the code speaks (it said 13 while the code said 17).
    #[test]
    fn claude_md_names_this_protocol() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../CLAUDE.md");
        let Ok(doc) = std::fs::read_to_string(path) else {
            panic!("CLAUDE.md not found at {path}");
        };
        assert!(
            doc.contains(&format!("(protocol {SERVER_VERSION}:")),
            "CLAUDE.md's tree-server line must say protocol {SERVER_VERSION}"
        );
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

    /// Review 07 M8: the line protocol's parsers take any bytes.
    #[test]
    fn mangled_replies_never_panic() {
        let windows = "40 3 0,0,1080,120 com.android.systemui 0 0 ScreenDecorOverlay\n\
                       3 1 0,0,1080,2400 com.app 1 1 -\n12 2 0,1500,1080,2400 ime\n";
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            let mut b = windows.as_bytes().to_vec();
            for _ in 0..1 + next() % 4 {
                let at = (next() as usize) % b.len().max(1);
                match next() % 3 {
                    0 if !b.is_empty() => {
                        let i = at.min(b.len() - 1);
                        b[i] = next() as u8;
                    }
                    1 => b.truncate(at),
                    _ => {
                        let at = at.min(b.len());
                        b.splice(at..at, *b"-2147483649,");
                    }
                };
            }
            let text = String::from_utf8_lossy(&b);
            for proto in [17, 18, 19] {
                let _ = parse_windows(&text, proto);
            }
            let _ = parse_pong(&text);
            let _ = parse_set_text(&text);
            let _ = parse_encoders(&text);
        }
    }

    /// VEL-310: extreme bounds overflowed the subtraction.
    #[test]
    fn window_bounds_saturate() {
        let w = parse_windows("1 1 -2147483648,0,2147483647,10 x\n", 19);
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
        let w = parse_windows("12 2 0,1500,1080,2400 com.google.android.inputmethod.latin\n3 1 0,0,1080,2400 com.app\n", 17);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].kind_name(), "keyboard");
        assert_eq!(w[0].bounds.y, 1500);
        assert_eq!(w[0].bounds.height, 900);
        assert_eq!(w[1].package, "com.app");
        assert_eq!(w[1].touchable, None);
        assert!(w[1].title.is_empty());
        // Protocol 18: touchable and the title to the end of the line.
        let w = parse_windows(
            "40 3 0,0,1080,120 com.android.systemui 0 ScreenDecorOverlay\n\
             39 3 0,0,1080,63 com.android.systemui 1 Status Bar\n\
             3 1 0,0,1080,2400 com.app ? -\n",
            18,
        );
        assert_eq!(w[0].touchable, Some(false));
        assert!(!w[0].takes_touches());
        assert_eq!(w[1].title, "Status Bar");
        assert!(w[1].takes_touches());
        assert_eq!(w[2].touchable, None);
        assert!(w[2].title.is_empty() && w[2].takes_touches());
        let decor = Window {
            touchable: None,
            title: "ScreenDecorOverlayBottom".into(),
            ..w[0].clone()
        };
        assert!(!decor.takes_touches());
        assert_eq!(w[1].active, None);
        // Protocol 19: whether it is the active window, before the title.
        let w = parse_windows(
            "5 1 100,200,700,900 com.other 1 0 Floating\n\
             3 1 0,0,1080,2400 com.app 1 1 -\n",
            19,
        );
        assert_eq!(w[0].active, Some(false));
        assert_eq!(w[0].title, "Floating");
        assert_eq!(w[1].active, Some(true));
        assert!(w[1].title.is_empty());
    }

    /// Runs against a real device when one is named in ATALANTA_TEST_ANDROID.
    #[tokio::test]
    async fn dumps_from_a_real_device() {
        let Some(device) = crate::test_support::device_under_test() else {
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
