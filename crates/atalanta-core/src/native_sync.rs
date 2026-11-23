// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::time::Duration;

use atalanta_common::{AtalantaError, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// Request sent to the native sync probe.
#[derive(Debug, Default, Serialize)]
struct ProbeRequest {
    cmd: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    /// How long idle must hold before the probe answers (probe protocol 2;
    /// older probes ignore it).
    #[serde(skip_serializing_if = "Option::is_none")]
    quiet_ms: Option<u64>,
    /// `config`: URL regexes whose requests never hold the app busy.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_urls: Option<Vec<String>>,
    /// `config`: timers due further out than this don't count, for this
    /// connection only.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_timer_ms: Option<u64>,
    /// `config`: the app's framework (`react_native`): the probe turns on
    /// that framework's trackers (probe protocol 4).
    #[serde(skip_serializing_if = "Option::is_none")]
    framework: Option<&'static str>,
}

/// What a probe said to `config`.
#[derive(Debug, Default, Deserialize)]
pub struct ProbeConfigReply {
    /// The `ignore_urls` patterns the probe could not compile.
    #[serde(default)]
    pub invalid: Vec<String>,
    /// The framework the probe turned tracking on for; `None` when it has
    /// none for the one sent (or none was sent).
    #[serde(default)]
    pub framework: Option<String>,
    /// Trackers that can't tell on this device (e.g. an RN hook that
    /// didn't take).
    #[serde(default)]
    pub unknown: Vec<String>,
}

/// A `web.networkIgnore` pattern as a probe `ignore_urls` regex. The web
/// reads a pattern with `*` as a glob over the whole URL and any other as a
/// substring, `?` included (VEL-076); the probes search with a regex
/// (`Matcher.find`, `NSRegularExpression.firstMatch`), so a glob is anchored
/// and a substring is not. Only characters that are special in both Java
/// and ICU regexes are escaped.
///
/// The probes compile their patterns case-insensitively; the web's
/// `glob_match` is case-sensitive, so every regex starts with `(?-i)` and
/// both sides match the same URLs (review-d7 m4). The probes' engines
/// backtrack, and `^a.*b.*c$` costs O(n^k) on a long URL that doesn't
/// match (review-d7 m5): runs of `*` collapse to one, and each middle
/// segment is an atomic `(?>.*?seg)`, its earliest match. Taking the
/// earliest match of each middle segment is what the web's greedy
/// `glob_match` does, so the result is the same, in O(n·m). Java and ICU
/// both support atomic groups.
pub fn ignore_glob_to_regex(pattern: &str) -> String {
    let escape = |part: &str| {
        let mut out = String::with_capacity(part.len());
        for c in part.chars() {
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    if !pattern.contains('*') {
        return format!("(?-i){}", escape(pattern));
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().expect("split yields a part");
    let (last, middle) = rest.split_last().expect("a `*` yields two parts");
    let mut out = format!("(?-i)^{}", escape(first));
    // Empty middle parts are runs of `*`: one wildcard covers them.
    for seg in middle.iter().filter(|s| !s.is_empty()) {
        out.push_str(&format!("(?>.*?{})", escape(seg)));
    }
    out.push_str(&format!(".*{}$", escape(last)));
    out
}

/// The suite's `web.networkIgnore` (`ATALANTA_WEB_NETWORK_IGNORE`, comma
/// separated) as probe regexes.
fn network_ignore_regexes() -> Vec<String> {
    atalanta_common::env::var("ATALANTA_WEB_NETWORK_IGNORE")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ignore_glob_to_regex)
                .collect()
        })
        .unwrap_or_default()
}

/// The longest hold the host asks a probe for: idle that lasts this long is
/// idle (EarlGrey's three consecutive idle passes, in time).
const PROBE_QUIET_MAX_MS: u64 = 50;

/// The shortest hold a probe applies whatever the host asks: three samples
/// 16 ms apart (`IdleWait`'s `pollMs * (minSamples - 1)`, both probes). A
/// wait shorter than the hold can't answer idle.
const PROBE_MIN_HOLD_MS: u64 = 32;

/// The first probe protocol that takes `max_timer_ms` for one connection.
const PROBE_MAX_TIMER_VERSION: u64 = 3;

/// The first probe protocol that takes `framework` in `config` (React
/// Native tracking) and tracks Android main-looper timers.
const PROBE_FRAMEWORK_VERSION: u64 = 4;

/// A probe's answer to `hello` (probe protocol 2, or 3 with
/// per-connection `max_timer_ms`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProbeHello {
    /// iOS bundle id (Android probes send it too).
    #[serde(default)]
    pub bundle: String,
    /// Android package (older protocol-2 spellings).
    #[serde(default)]
    pub package: Option<String>,
    /// Android process name, when it differs from the package
    /// (`android:process`). Older probes sent it as `bundle`.
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub pid: Option<u64>,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub platform: Option<String>,
    /// Trackers that can't tell on this device (e.g. `animation`).
    #[serde(default)]
    pub unknown: Vec<String>,
}

impl ProbeHello {
    /// The app the probe runs in.
    pub fn app(&self) -> &str {
        if self.bundle.is_empty() {
            self.package.as_deref().unwrap_or("")
        } else {
            &self.bundle
        }
    }

    /// Whether the probe runs in `app`: its package, or its process name
    /// (probes before m7 reported the process as the package, and a process
    /// named `com.app:ui` belongs to `com.app`).
    pub fn is_app(&self, app: &str) -> bool {
        let named = self.app();
        named.is_empty()
            || [
                Some(named),
                self.package.as_deref(),
                self.process.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|n| n == app || n.split(':').next() == Some(app))
    }
}

/// What a probe said to `hello`.
#[derive(Debug, Clone)]
pub enum Handshake {
    /// A probe that names its app.
    Identified(ProbeHello),
    /// A probe from before `hello` (it answered "unknown command"): it
    /// works, but which app it belongs to can't be checked.
    Legacy,
}

/// Response from the native sync probe.
/// `idle` defaults to false so probe error replies (`{"error": "..."}`)
/// deserialize instead of surfacing as a parse failure.
#[derive(Debug, Deserialize)]
pub struct ProbeResponse {
    #[serde(default)]
    pub idle: bool,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub waited_ms: Option<u64>,
    #[serde(default)]
    pub ts: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub unknown: Vec<String>,
}

/// Client for communicating with a native sync probe (iOS or Android)
/// running inside the app under test.
///
/// Protocol: newline-delimited JSON over TCP.
/// - iOS probe listens on localhost:19400
/// - Android probe listens on localhost:19401 (via adb port-forward)
pub struct NativeSyncClient {
    host: String,
    port: u16,
    connect_timeout: Duration,
    stream: Option<BufReader<TcpStream>>,
    /// A request was written and its reply not yet read. Still set when a
    /// call starts means the last one was dropped mid-exchange (a step
    /// watchdog, the crash-watch `select!`): its reply may still be in the
    /// socket, and the next request would read it as its own (review 01
    /// #1). The stream is then dropped and the probe reconnected.
    in_flight: bool,
}

impl NativeSyncClient {
    pub fn new(port: u16, connect_timeout_ms: u64) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port,
            connect_timeout: Duration::from_millis(connect_timeout_ms),
            stream: None,
            in_flight: false,
        }
    }

    /// Try to connect to the probe. Returns true if successful.
    pub async fn connect(&mut self) -> bool {
        let addr = format!("{}:{}", self.host, self.port);
        debug!(addr = %addr, "connecting to native sync probe");

        match tokio::time::timeout(self.connect_timeout, TcpStream::connect(&addr)).await {
            Ok(Ok(stream)) => {
                debug!("native sync probe connected");
                self.stream = Some(BufReader::new(stream));
                self.in_flight = false;
                true
            }
            Ok(Err(e)) => {
                debug!(error = %e, "native sync probe connection failed");
                self.stream = None;
                false
            }
            Err(_) => {
                debug!("native sync probe connection timed out");
                self.stream = None;
                false
            }
        }
    }

    /// The host port this client talks to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Ask the probe who it is. A probe from before the handshake answers
    /// "unknown command": that is [`Handshake::Legacy`], not an error. Any
    /// other failure (no reply, the connection dropped, as an `adb forward`
    /// with no probe behind it does) is an error.
    pub async fn hello(&mut self) -> Result<Handshake> {
        let request = ProbeRequest {
            cmd: "hello",
            ..Default::default()
        };
        let line = self
            .round_trip(
                request,
                self.connect_timeout.max(Duration::from_millis(500)),
            )
            .await?;
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            hello: bool,
            #[serde(default)]
            error: Option<String>,
            #[serde(flatten)]
            info: ProbeHello,
        }
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|e| self.lost(format!("unreadable hello reply: {e}")))?;
        match reply.error {
            Some(e) if e.contains("unknown command") => Ok(Handshake::Legacy),
            Some(e) => Err(AtalantaError::Config(format!("Probe error: {e}"))),
            None if reply.hello => Ok(Handshake::Identified(reply.info)),
            None => Err(AtalantaError::Config(format!(
                "Unexpected probe hello reply: {}",
                line.trim()
            ))),
        }
    }

    /// Send `config` (after `hello`): `ignore_urls` are added to the
    /// probe's process-wide list (a pattern it already has is not added
    /// again), `max_timer_ms` holds for this connection only. Returns the
    /// patterns the probe could not compile.
    pub async fn configure(
        &mut self,
        ignore_urls: &[String],
        max_timer_ms: Option<u64>,
    ) -> Result<Vec<String>> {
        Ok(self
            .configure_with(ignore_urls, max_timer_ms, None)
            .await?
            .invalid)
    }

    /// [`Self::configure`], also naming the app's `framework`
    /// (`react_native`, probe protocol 4): the reply says whether the probe
    /// turned that framework's tracking on, and what it can't tell.
    pub async fn configure_with(
        &mut self,
        ignore_urls: &[String],
        max_timer_ms: Option<u64>,
        framework: Option<&'static str>,
    ) -> Result<ProbeConfigReply> {
        let request = ProbeRequest {
            cmd: "config",
            ignore_urls: (!ignore_urls.is_empty()).then(|| ignore_urls.to_vec()),
            max_timer_ms,
            framework,
            ..Default::default()
        };
        let line = self
            .round_trip(
                request,
                self.connect_timeout.max(Duration::from_millis(500)),
            )
            .await?;
        #[derive(Deserialize)]
        struct Reply {
            #[serde(default)]
            error: Option<String>,
            #[serde(flatten)]
            reply: ProbeConfigReply,
        }
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|e| self.lost(format!("unreadable config reply: {e}")))?;
        match reply.error {
            Some(e) => Err(AtalantaError::Config(format!("Probe error: {e}"))),
            None => Ok(reply.reply),
        }
    }

    /// Check if currently connected.
    /// A connection whose last exchange was abandoned half-way counts as
    /// not connected: its next reply can't be trusted to be ours.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some() && !self.in_flight
    }

    /// Disconnect from the probe.
    pub fn disconnect(&mut self) {
        self.stream = None;
        self.in_flight = false;
    }

    /// The probe's socket broke or went quiet: transient, not a config
    /// error (review 03 M2).
    fn lost(&self, detail: impl Into<String>) -> AtalantaError {
        AtalantaError::SyncProbeLost {
            port: self.port,
            detail: detail.into(),
        }
    }

    /// Send a status query to the probe.
    pub async fn status(&mut self) -> Result<ProbeResponse> {
        self.send_command(ProbeRequest {
            cmd: "status",
            ..Default::default()
        })
        .await
    }

    /// Wait for the app to become idle, with a timeout.
    pub async fn wait_for_idle(&mut self, timeout_ms: u64) -> Result<ProbeResponse> {
        self.wait_for_idle_holding(timeout_ms, None).await
    }

    /// Wait for the app to become idle and stay idle for `quiet_ms`.
    pub async fn wait_for_idle_holding(
        &mut self,
        timeout_ms: u64,
        quiet_ms: Option<u64>,
    ) -> Result<ProbeResponse> {
        self.send_command(ProbeRequest {
            cmd: "wait_idle",
            timeout_ms: Some(timeout_ms),
            quiet_ms,
            ..Default::default()
        })
        .await
    }

    /// Send a command and read the response. The whole round-trip is bounded
    /// by the requested wait timeout plus a margin — otherwise a probe that
    /// accepts the socket but never replies (app suspended, probe wedged)
    /// hangs the entire run.
    async fn send_command(&mut self, request: ProbeRequest) -> Result<ProbeResponse> {
        let io_timeout =
            Duration::from_millis(request.timeout_ms.unwrap_or(0).saturating_add(5_000));
        let line = self.round_trip(request, io_timeout).await?;

        let response: ProbeResponse =
            serde_json::from_str(&line).map_err(|e| self.lost(format!("unreadable reply: {e}")))?;

        if let Some(error) = response.error {
            return Err(AtalantaError::Config(format!("Probe error: {error}")));
        }

        Ok(response)
    }

    /// Send one request line and read one reply line, within `io_timeout`.
    async fn round_trip(&mut self, request: ProbeRequest, io_timeout: Duration) -> Result<String> {
        let response = tokio::time::timeout(io_timeout, self.round_trip_inner(request)).await;
        match response {
            Ok(result) => result,
            Err(_) => {
                self.disconnect();
                Err(self.lost(format!("did not reply within {}ms", io_timeout.as_millis())))
            }
        }
    }

    async fn round_trip_inner(&mut self, request: ProbeRequest) -> Result<String> {
        if self.in_flight {
            // The previous exchange was cancelled before its reply was
            // read: that reply would be taken for this one's.
            debug!("native sync probe: last request was abandoned; reconnecting");
            self.disconnect();
            if !self.connect().await {
                return Err(self.lost("not connected"));
            }
        }
        // Serialize request as JSON + newline
        let mut json = serde_json::to_string(&request)
            .map_err(|e| AtalantaError::io("Failed to serialize probe request", e))?;
        json.push('\n');

        let port = self.port;
        let lost = |detail: String| AtalantaError::SyncProbeLost { port, detail };
        let Some(stream) = self.stream.as_mut() else {
            return Err(lost("not connected".into()));
        };
        self.in_flight = true;

        // Write request
        let written = stream.get_mut().write_all(json.as_bytes()).await;
        if let Err(e) = written {
            self.disconnect();
            return Err(lost(format!("write failed: {e}")));
        }

        // Read response (one line)
        let mut line = String::new();
        let read = stream.read_line(&mut line).await;
        if let Err(e) = read {
            self.disconnect();
            return Err(lost(format!("read failed: {e}")));
        }

        if line.is_empty() {
            self.disconnect();
            return Err(lost("disconnected".into()));
        }
        self.in_flight = false;

        Ok(line)
    }
}

/// Unified sync manager that supports native probe, polling, or auto-detection.
pub struct SyncManager {
    mode: atalanta_common::SyncMode,
    native_client: Option<NativeSyncClient>,
    polling_engine: crate::sync::AdaptiveSyncEngine,
    native_available: Option<bool>,
    connect_attempts: u32,
    /// Native sync answered at least once this run. Until then a failure
    /// is the usual "app has no probe" (an adb port forward accepts the
    /// connection and drops it), not worth a warning.
    native_worked: bool,
    /// A web page: the page's own render settle (`settle_on_device`)
    /// instead of tree polling, then the network-idle wait. `sync.mode`
    /// still applies: `none` is the network wait only, `polling` the tree.
    web: bool,
    /// An action ran since the last wait (web: give its requests a moment
    /// to start).
    acted: bool,
    /// The app under test: a probe whose `hello` names another app is not
    /// trusted (another simulator's or another app's probe on the port).
    app_id: Option<String>,
    /// The connection's `hello` was checked.
    handshake_done: bool,
    /// The probe on the port belongs to another app: tree polling until
    /// the next launch, in Native mode too (it used to skip sync entirely
    /// while the warning said "polling").
    probe_rejected: bool,
    /// The suite set its own probe port (not the default): it wins over
    /// the driver's per-device forward, as before per-device ports.
    suite_port_explicit: bool,
    /// One-time warnings already given (keyed by kind).
    warned: std::collections::HashSet<&'static str>,
    /// The suite's `sync.nativePort*`, tried when nothing listens on the
    /// driver's per-device port: a probe built before
    /// `ATALANTA_SYNC_PORT`, or an app that passes its own port, still
    /// listens there.
    suite_port: u16,
    /// The per-device port had no probe past the grace period and the
    /// app's own probe answered on the suite's port. The device port is
    /// re-tried every [`DEVICE_PORT_RECHECK_STEPS`] steps, when the suite
    /// probe fails, and after the next launch (MJ-A).
    on_suite_port: bool,
    /// Only an iOS simulator's port can be "not there yet": Android's
    /// per-device `adb forward` always accepts, so a refusal there never
    /// sends it to the suite port (another device's forward).
    suite_fallback: bool,
    /// When the device port first refused since the last launch/reset.
    device_refused_since: Option<std::time::Instant>,
    /// A launch's whole grace passed with nothing on the device port (an
    /// app built without the probe). Later launches don't wait out the
    /// grace again: it cost 2.5 s per `launchApp` on an iOS simulator
    /// (DV-3). The port is still tried at every sync, so a probe that does
    /// come up is used, and an answer clears this.
    device_port_silent: bool,
    /// Steps served from the suite port since the device port was last tried.
    suite_steps: u32,
    /// `ignore_urls` for the probe's `config`: `None` reads the suite's
    /// `web.networkIgnore` when a connection is set up.
    probe_ignore: Option<Vec<String>>,
    /// The suite's `framework`: `react_native` asks the probe for React
    /// Native tracking in `config`.
    framework: atalanta_common::Framework,
}

/// How long the per-device port may refuse after a launch before the
/// suite port is tried: right after `simctl launch` the app's listener is
/// often not up yet (MJ-A).
const DEVICE_PORT_GRACE: Duration = Duration::from_millis(2_500);
/// While on the suite port, the device port is re-tried every this many steps.
const DEVICE_PORT_RECHECK_STEPS: u32 = 10;

/// How many failed probe connections before Auto mode stops re-trying native
/// sync. The first sync usually runs right after launchApp, before the in-app
/// probe has bound its port — a single-failure cache would permanently
/// disable native sync for the whole suite.
const MAX_NATIVE_CONNECT_ATTEMPTS: u32 = 3;

/// The web render settle's reaction window when the suite keeps the
/// default: a click's handler runs before the click returns and React
/// commits in the same task, so the page has usually reacted by the time
/// the settle starts; 30 ms is the network wait's own window, so a static
/// page costs nothing over the old network-only wait (M-3). The native
/// default (300 ms) made every web action wait that long.
pub const WEB_REACTION_MS: u64 = 30;
/// The web settle cap when the suite keeps the default (1 s): a page that
/// never goes quiet (a JS animation the page settle can't tell from a
/// render) costs this per action. `sync.mode: none` skips the settle.
pub const WEB_SETTLE_CAP_MS: u64 = 1_000;

/// (reaction, cap) for the web settle: the values set (`web.settle`,
/// `sync.*`, a step's `settle:`), else the web defaults. Only an unset
/// timing gets the web's: a value that happened to equal the native default
/// (`settle: { reactionMs: 300 }`) was swapped for 30 ms (NB-8, SUM-4).
fn web_timings(config: &atalanta_common::SyncConfig) -> (u64, u64) {
    (
        config.reaction_ms.unwrap_or(WEB_REACTION_MS),
        config.settle_cap_ms.unwrap_or(WEB_SETTLE_CAP_MS),
    )
}

impl SyncManager {
    pub fn new(config: &atalanta_common::SyncConfig, platform: atalanta_common::Platform) -> Self {
        // Web platform: there is no native probe; the page reports its own
        // settle. The mode used to be forced to `none`, so a user's
        // `sync.mode`/`quietMs`/`reactionMs` were ignored without a word
        // and nothing waited for a render (VEL-067).
        if platform == atalanta_common::Platform::Web {
            return Self {
                mode: config.mode,
                native_client: None,
                polling_engine: crate::sync::AdaptiveSyncEngine::new(config.clone()),
                native_available: None,
                connect_attempts: 0,
                native_worked: false,
                web: true,
                acted: false,
                app_id: None,
                handshake_done: false,
                probe_rejected: false,
                suite_port_explicit: false,
                warned: Default::default(),
                suite_port: 0,
                on_suite_port: false,
                suite_fallback: false,
                device_refused_since: None,
                device_port_silent: false,
                suite_steps: 0,
                probe_ignore: None,
                framework: atalanta_common::Framework::default(),
            };
        }

        let defaults = atalanta_common::SyncConfig::default();
        let ports = match platform {
            atalanta_common::Platform::Ios => {
                Some((config.native_port_ios, defaults.native_port_ios))
            }
            atalanta_common::Platform::Android => {
                Some((config.native_port_android, defaults.native_port_android))
            }
            // Returned above.
            atalanta_common::Platform::Web => None,
        };
        let (native_port, default_port) = ports.unwrap_or((0, 0));

        let native_client = match config.mode {
            _ if ports.is_none() => None,
            atalanta_common::SyncMode::Polling | atalanta_common::SyncMode::None => None,
            _ => Some(NativeSyncClient::new(
                native_port,
                config.probe_connect_timeout_ms,
            )),
        };

        Self {
            mode: config.mode,
            native_client,
            polling_engine: crate::sync::AdaptiveSyncEngine::new(config.clone()),
            native_available: None,
            connect_attempts: 0,
            native_worked: false,
            web: false,
            acted: false,
            app_id: None,
            handshake_done: false,
            probe_rejected: false,
            suite_port_explicit: native_port != default_port,
            warned: Default::default(),
            suite_port: native_port,
            on_suite_port: false,
            suite_fallback: platform == atalanta_common::Platform::Ios,
            device_refused_since: None,
            device_port_silent: false,
            suite_steps: 0,
            probe_ignore: None,
            framework: atalanta_common::Framework::default(),
        }
    }

    /// The app's framework (the suite's `framework`): `react_native` turns
    /// on the probe's React Native tracking (JS timers, mounting, ...).
    pub fn with_framework(mut self, framework: atalanta_common::Framework) -> Self {
        self.framework = framework;
        self
    }

    /// The framework the probe is told about.
    pub fn framework(&self) -> atalanta_common::Framework {
        self.framework
    }

    /// The probe's `ignore_urls` (regexes), instead of the suite's
    /// `web.networkIgnore`.
    pub fn with_probe_ignore(mut self, regexes: Vec<String>) -> Self {
        self.probe_ignore = Some(regexes);
        self
    }

    /// The app under test, checked against the probe's `hello`.
    pub fn for_app(mut self, app_id: &str) -> Self {
        if !app_id.is_empty() {
            self.app_id = Some(app_id.to_string());
        }
        self
    }

    fn warn_once(&mut self, kind: &'static str, message: impl FnOnce() -> String) {
        if self.warned.insert(kind) {
            warn!("{}", message());
        }
    }

    /// Wait for the UI to stabilize using the configured sync strategy.
    /// The polling settle's timings (per-step `settle:`, `setConfig`).
    pub fn sync_config_mut(&mut self) -> &mut atalanta_common::SyncConfig {
        self.polling_engine.config_mut()
    }

    /// A wait's error may be the native probe's own (its connection, its
    /// setup) rather than the screen's: only in `sync.mode: native`, where
    /// probe errors aren't swallowed. Auto mode falls back to polling, so
    /// its errors come from reading the screen.
    pub(crate) fn probe_errors_surface(&self) -> bool {
        !self.web && matches!(self.mode, atalanta_common::SyncMode::Native)
    }

    /// The settle cap in force: the web's own default when none is set
    /// there, else the configured one (bounded by the sync timeout).
    pub fn settle_cap_ms(&mut self) -> u64 {
        let web = self.web;
        let cfg = self.polling_engine.config_mut();
        let cap = if web {
            web_timings(cfg).1
        } else {
            cfg.settle_cap()
        };
        cap.min(cfg.timeout_ms)
    }

    /// Web: wait for the requests the action started (`web.networkIdle`).
    /// A request a click starts shows up within a few ms, so a quiet
    /// network for 30 ms after the action means there's nothing to wait
    /// for. Bounded by the settle cap.
    ///
    /// The 30 ms run from `since` (the end of the action): the page's
    /// render settle before this already spent them, and a static page
    /// paid the window twice.
    async fn network_idle(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
        since: std::time::Instant,
        cap: Duration,
    ) {
        let start = std::time::Instant::now();
        let react = if std::mem::take(&mut self.acted) {
            Duration::from_millis(30)
        } else {
            Duration::ZERO
        };
        let mut seen = false;
        loop {
            let busy = driver.network_busy(device_id).await;
            seen |= busy;
            let waited = start.elapsed();
            if (!busy && (seen || since.elapsed() >= react)) || waited >= cap {
                if busy {
                    tracing::debug!("network still busy after {cap:?}; moving on");
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn wait_for_idle(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> Result<()> {
        self.polling_engine.clear_stats();
        if self.web {
            return self.web_wait_for_idle(driver, device_id).await;
        }
        match self.mode {
            atalanta_common::SyncMode::None => Ok(()),
            atalanta_common::SyncMode::Native => {
                if self.native_or_rejected(driver, device_id).await? {
                    return Ok(());
                }
                self.polling_engine.wait_for_idle(driver, device_id).await
            }
            atalanta_common::SyncMode::Polling => {
                self.polling_engine.wait_for_idle(driver, device_id).await
            }
            atalanta_common::SyncMode::Auto => {
                // Try native first, fallback to polling
                if self.try_native(driver, device_id).await {
                    match self.native_wait_for_idle(driver, device_id).await {
                        Ok(()) => {
                            self.connect_attempts = 0;
                            self.native_worked = true;
                            return Ok(());
                        }
                        Err(e) => {
                            if self.native_worked {
                                warn!(error = %e, "native sync failed, falling back to polling");
                            } else {
                                tracing::debug!(error = %e, "no native sync probe; polling");
                            }
                            if let Some(ref mut client) = self.native_client {
                                client.disconnect();
                            }
                            self.native_available = Some(false);
                            self.connect_attempts += 1;
                        }
                    }
                }
                self.polling_engine.wait_for_idle(driver, device_id).await
            }
        }
    }

    /// Web: after an action, the page's render settle (DOM mutations and
    /// animations, see the web driver's `settle_on_device`) within the
    /// settle cap, then the network wait in what is left of it. Rendering,
    /// not the network, causes most web wait flakes (Romano et al., ICSE
    /// 2021). `sync.mode: none` keeps the network wait only (the old
    /// behaviour); `polling` uses the tree-polling engine.
    async fn web_wait_for_idle(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> Result<()> {
        let config = self.polling_engine.config_mut().clone();
        let (reaction_ms, cap_ms) = web_timings(&config);
        let cap = Duration::from_millis(cap_ms.max(1));
        let start = std::time::Instant::now();
        match self.mode {
            atalanta_common::SyncMode::None => {}
            atalanta_common::SyncMode::Polling => {
                // Tree settle, then the network wait in what is left of the cap.
                self.acted = false;
                let settled = self.polling_engine.wait_for_idle(driver, device_id).await;
                let stats = self.polling_engine.last_stats();
                let left = cap.saturating_sub(start.elapsed());
                self.network_idle(driver, device_id, start, left).await;
                self.polling_engine.set_stats(stats);
                return settled;
            }
            atalanta_common::SyncMode::Auto | atalanta_common::SyncMode::Native => {
                if self.acted {
                    // The page answers when it is quiet; a page that can't
                    // (mid-navigation) just gets the network wait.
                    let settled = driver
                        .settle_on_device(device_id, reaction_ms, config.quiet(), cap_ms)
                        .await;
                    if settled.is_some_and(|s| !s.settled) {
                        tracing::debug!("page still changing after {cap:?}; moving on");
                    }
                }
            }
        }
        let left = cap
            .saturating_sub(start.elapsed())
            .max(Duration::from_millis(1));
        self.network_idle(driver, device_id, start, left).await;
        Ok(())
    }

    /// Wait for idle with a selector-specific key for prediction.
    pub async fn wait_for_idle_keyed(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
        key: &str,
    ) -> Result<()> {
        if self.web {
            return self.wait_for_idle(driver, device_id).await;
        }
        self.polling_engine.clear_stats();
        match self.mode {
            atalanta_common::SyncMode::None => Ok(()),
            atalanta_common::SyncMode::Native => {
                if self.native_or_rejected(driver, device_id).await? {
                    return Ok(());
                }
                self.polling_engine
                    .wait_for_idle_keyed(driver, device_id, key)
                    .await
            }
            atalanta_common::SyncMode::Polling => {
                self.polling_engine
                    .wait_for_idle_keyed(driver, device_id, key)
                    .await
            }
            atalanta_common::SyncMode::Auto => {
                if self.try_native(driver, device_id).await {
                    match self.native_wait_for_idle(driver, device_id).await {
                        Ok(()) => {
                            self.connect_attempts = 0;
                            self.native_worked = true;
                            return Ok(());
                        }
                        Err(e) => {
                            if self.native_worked {
                                warn!(error = %e, "native sync failed, falling back to polling");
                            } else {
                                tracing::debug!(error = %e, "no native sync probe; polling");
                            }
                            if let Some(ref mut client) = self.native_client {
                                client.disconnect();
                            }
                            self.native_available = Some(false);
                            self.connect_attempts += 1;
                        }
                    }
                }
                self.polling_engine
                    .wait_for_idle_keyed(driver, device_id, key)
                    .await
            }
        }
    }

    /// Why the last wait gave up at the cap, when it did (P9).
    pub fn busy_reason(&self) -> Option<&str> {
        self.polling_engine.busy_reason()
    }

    /// Native mode: the probe's wait. `Ok(false)` when the probe on the
    /// port belongs to another app, so the caller polls instead (m1): the
    /// warning says so, and such a step used to run with no settle at all
    /// and a re-handshake every time.
    async fn native_or_rejected(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> Result<bool> {
        if self.probe_rejected {
            return Ok(false);
        }
        match self.native_wait_for_idle(driver, device_id).await {
            Err(_) if self.probe_rejected => Ok(false),
            other => other.map(|()| true),
        }
    }

    /// What the last polling settle did (default when native sync answered).
    pub fn last_settle(&self) -> crate::sync::SettleStats {
        self.polling_engine.last_stats()
    }

    /// A step that changes nothing itself ran (an assertion, a wait): the
    /// screen may have moved meanwhile, unseen by any settle, so the next
    /// action's reaction floor doesn't compare with the old picture.
    pub fn forget_picture(&mut self) {
        self.polling_engine.forget_picture();
    }

    /// Reset state after mutating actions.
    pub fn invalidate(&mut self) {
        self.polling_engine.invalidate_tree_diff();
        self.acted = true;
    }

    /// Allow native sync to be re-probed. Call after app (re)launch — the
    /// probe lives inside the app process, so its availability changes with
    /// the app lifecycle.
    pub fn reset_native(&mut self) {
        if let Some(ref mut client) = self.native_client {
            // A relaunched app is a new probe: connect and check it again.
            client.disconnect();
            self.native_available = None;
            self.connect_attempts = 0;
            self.handshake_done = false;
            self.probe_rejected = false;
            self.on_suite_port = false;
            self.device_refused_since = if self.device_port_silent {
                std::time::Instant::now().checked_sub(DEVICE_PORT_GRACE)
            } else {
                None
            };
            self.suite_steps = 0;
        }
    }

    /// Point the client at the device's own probe port when the driver
    /// forwards one per device (Android `adb forward tcp:0`), instead of the
    /// suite-wide port every device used to share.
    fn select_probe_port(&mut self, driver: &dyn atalanta_common::PlatformDriver, device_id: &str) {
        if self.suite_port_explicit {
            // m6: `sync.nativePort*` set by the suite (an app that starts
            // its probe on its own port, forwarded by the team) wins.
            return;
        }
        let port = if self.on_suite_port {
            self.suite_port
        } else {
            match driver.sync_probe_port(device_id) {
                Some(p) => p,
                None => return,
            }
        };
        if let Some(client) = self.native_client.as_mut() {
            if client.port() != port {
                debug!(port, device_id, "native sync probe: per-device port");
                let timeout = client.connect_timeout;
                *client = NativeSyncClient {
                    host: client.host.clone(),
                    port,
                    connect_timeout: timeout,
                    stream: None,
                    in_flight: false,
                };
                self.handshake_done = false;
            }
        }
    }

    /// Connect (if needed) and check the probe's `hello` once per
    /// connection. `Ok(false)`: the probe belongs to another app and must
    /// not be used until the next launch. `Err`: no usable probe.
    async fn ensure_probe(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> Result<bool> {
        self.select_probe_port(driver, device_id);
        let client = self
            .native_client
            .as_mut()
            .ok_or_else(|| AtalantaError::Config("Native sync client not available".to_string()))?;
        // MJ-A: while on the suite port, the device's own probe is tried
        // again every few steps, and whenever the suite probe dropped.
        if self.on_suite_port {
            self.suite_steps += 1;
            let recheck = !client.is_connected() || self.suite_steps >= DEVICE_PORT_RECHECK_STEPS;
            if recheck {
                self.suite_steps = 0;
                if let Some(device) = driver.sync_probe_port(device_id) {
                    let mut own = NativeSyncClient {
                        host: client.host.clone(),
                        port: device,
                        connect_timeout: client.connect_timeout,
                        stream: None,
                        in_flight: false,
                    };
                    if own.connect().await {
                        debug!(
                            port = device,
                            "native sync probe: back on the per-device port"
                        );
                        *client = own;
                        self.on_suite_port = false;
                        self.handshake_done = false;
                    }
                }
            }
        }
        if !client.is_connected() {
            self.handshake_done = false;
            if !client.connect().await {
                let suite = self.suite_port;
                let device_port = client.port();
                let can_fall_back = self.suite_fallback
                    && !self.on_suite_port
                    && suite != 0
                    && device_port != suite;
                if !can_fall_back {
                    return Err(AtalantaError::Config(
                        "Cannot connect to native sync probe".to_string(),
                    ));
                }
                // Refused right after a launch means "not up yet": keep
                // trying the device port, within the settle cap, until the
                // grace period has run.
                let first = *self
                    .device_refused_since
                    .get_or_insert_with(std::time::Instant::now);
                let cap = Duration::from_millis(self.polling_engine.config().settle_cap().max(1));
                let start = std::time::Instant::now();
                let mut backoff = Duration::from_millis(50);
                let mut up = false;
                while first.elapsed() < DEVICE_PORT_GRACE && start.elapsed() < cap {
                    let left = DEVICE_PORT_GRACE
                        .saturating_sub(first.elapsed())
                        .min(cap.saturating_sub(start.elapsed()));
                    tokio::time::sleep(backoff.min(left)).await;
                    backoff = (backoff * 2).min(Duration::from_millis(250));
                    if client.connect().await {
                        up = true;
                        break;
                    }
                }
                if !up {
                    if first.elapsed() < DEVICE_PORT_GRACE {
                        return Err(AtalantaError::Config(
                            "native sync probe not up yet on the per-device port".to_string(),
                        ));
                    }
                    self.device_port_silent = true;
                    // Nothing on the per-device port past the grace (iOS
                    // passes it in the launch environment): an older
                    // probe, or one started on an explicit port, is on
                    // the suite's. Its `hello` decides below.
                    let mut fallback = NativeSyncClient {
                        host: client.host.clone(),
                        port: suite,
                        connect_timeout: client.connect_timeout,
                        stream: None,
                        in_flight: false,
                    };
                    if !fallback.connect().await {
                        return Err(AtalantaError::Config(
                            "Cannot connect to native sync probe".to_string(),
                        ));
                    }
                    debug!(
                        from = device_port,
                        port = suite,
                        "native sync probe not on the per-device port; trying the suite's"
                    );
                    *client = fallback;
                    self.on_suite_port = true;
                    self.suite_steps = 0;
                }
            }
        }
        if !self.on_suite_port {
            // Review f5-f6 MINOR-6: the device port answered, so the next
            // refusal (the probe restarting later in this launch) gets a
            // fresh grace before the suite port is tried.
            self.device_refused_since = None;
            self.device_port_silent = false;
        }
        if self.handshake_done {
            return Ok(true);
        }
        let port = client.port();
        let handshake = match client.hello().await {
            Ok(h) => h,
            Err(e) => {
                client.disconnect();
                return Err(e);
            }
        };
        match handshake {
            Handshake::Identified(hello) if self.on_suite_port => {
                // Review f5-f6 MAJOR-4: an identified probe reads
                // `ATALANTA_SYNC_PORT`, so one on the shared suite port
                // while this device has its own port was started outside
                // Atalanta's launch, or runs on another simulator, maybe
                // with the same bundle id. Nothing in its `hello` proves
                // it is this device's app, so it is never latched; only a
                // probe that predates the handshake (and so the per-device
                // port) is used there.
                let app = hello.app().to_string();
                client.disconnect();
                self.on_suite_port = false;
                self.warn_once("probe-suite-other", || {
                    format!(
                        "native sync probe on the shared suite port {port} ({}, pid {}) is not on this device's port, so it can't be told apart from another simulator's; not using it",
                        if app.is_empty() { "no app name" } else { app.as_str() },
                        hello.pid.map_or("?".to_string(), |p| p.to_string())
                    )
                });
                return Err(AtalantaError::Config(
                    "the suite port's probe can't be proven to be this device's".to_string(),
                ));
            }
            Handshake::Identified(hello) => {
                let app = hello.app().to_string();
                if let Some(expected) = self.app_id.clone() {
                    if !hello.is_app(&expected) {
                        client.disconnect();
                        if self.on_suite_port {
                            // MJ-A: whatever is on the shared suite port
                            // (another simulator's app) is never latched;
                            // the device port is tried again next step.
                            self.on_suite_port = false;
                            self.warn_once("probe-suite-other", || {
                                format!(
                                    "native sync probe on the suite port {port} belongs to {app}, not {expected}; not using it"
                                )
                            });
                            return Err(AtalantaError::Config(
                                "the suite port's probe belongs to another app".to_string(),
                            ));
                        }
                        self.probe_rejected = true;
                        self.warn_once("probe-app", || {
                            format!(
                                "native sync probe on port {port} belongs to {app} (pid {}), not {expected}; using tree polling until the next launchApp",
                                hello.pid.map_or("?".to_string(), |p| p.to_string())
                            )
                        });
                        return Ok(false);
                    }
                }
                debug!(app = %app, pid = ?hello.pid, version = ?hello.version, port, "native sync probe identified");
                self.configure_probe(hello.version).await;
                if !hello.unknown.is_empty() {
                    let unknown = hello.unknown.join(", ");
                    self.warn_once("probe-unknown", || {
                        format!("native sync probe can't track on this device: {unknown} (not waited for)")
                    });
                }
            }
            Handshake::Legacy => {
                let app = self.app_id.clone().unwrap_or_else(|| "the app".to_string());
                self.warn_once("probe-legacy", || {
                    format!(
                        "native sync probe on port {port} predates the hello handshake, so Atalanta can't check it belongs to {app}; update AtalantaSyncProbe"
                    )
                });
            }
        }
        self.handshake_done = true;
        Ok(true)
    }

    /// Send the suite's probe settings on a new connection: the
    /// `web.networkIgnore` globs as `ignore_urls` and `sync.maxTimerMs` as
    /// `max_timer_ms` (probe-urls). Nothing is sent when neither is set; a
    /// probe that refuses `config` still syncs.
    ///
    /// `max_timer_ms` goes only to a probe whose `hello` says protocol 3 or
    /// later: an older probe applied it process-wide (for every later run
    /// against that app) and capped it at 1.5 s anyway (review-d7 m3).
    ///
    /// `framework: react_native` sends `framework` to a protocol-4 probe,
    /// which turns on its React Native trackers; an older probe gets a
    /// warning instead (it has none).
    async fn configure_probe(&mut self, version: Option<u64>) {
        let ignore = self
            .probe_ignore
            .clone()
            .unwrap_or_else(network_ignore_regexes);
        let mut framework = match self.framework {
            atalanta_common::Framework::ReactNative => Some("react_native"),
            _ => None,
        };
        if framework.is_some() && version.unwrap_or(0) < PROBE_FRAMEWORK_VERSION {
            framework = None;
            let v = version.map_or("?".to_string(), |v| v.to_string());
            self.warn_once("probe-rn-old", || {
                format!(
                    "native sync probe (protocol {v}) has no React Native tracking (JS timers, mounting); update AtalantaSyncProbe"
                )
            });
        }
        let mut max_timer = self.polling_engine.config().max_timer_ms;
        if max_timer.is_some() && version.unwrap_or(0) < PROBE_MAX_TIMER_VERSION {
            max_timer = None;
            let v = version.map_or("?".to_string(), |v| v.to_string());
            self.warn_once("probe-max-timer-old", || {
                format!(
                    "native sync probe (protocol {v}) predates per-connection sync.maxTimerMs, so it isn't sent; update AtalantaSyncProbe"
                )
            });
        }
        if ignore.is_empty() && max_timer.is_none() && framework.is_none() {
            return;
        }
        let Some(client) = self.native_client.as_mut() else {
            return;
        };
        match client.configure_with(&ignore, max_timer, framework).await {
            Ok(reply) => {
                if !reply.invalid.is_empty() {
                    let invalid = reply.invalid.join(", ");
                    self.warn_once("probe-ignore-invalid", || {
                        format!(
                            "native sync probe refused these web.networkIgnore patterns: {invalid}"
                        )
                    });
                }
                if let Some(fw) = framework {
                    if reply.framework.as_deref() != Some(fw) {
                        self.warn_once("probe-rn-off", || {
                            format!("native sync probe did not turn on {fw} tracking; update AtalantaSyncProbe")
                        });
                    } else if !reply.unknown.is_empty() {
                        let unknown = reply.unknown.join(", ");
                        self.warn_once("probe-rn-unknown", || {
                            format!("native sync probe can't track on this app: {unknown} (not waited for)")
                        });
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "native sync probe config");
                self.warn_once("probe-config", || {
                    format!("native sync probe did not take web.networkIgnore / sync.maxTimerMs ({e}); update AtalantaSyncProbe")
                });
            }
        }
    }

    /// Try to establish native sync connection. Failures are retried up to
    /// MAX_NATIVE_CONNECT_ATTEMPTS before Auto mode settles on polling.
    async fn try_native(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> bool {
        match self.native_available {
            Some(true) => return true,
            Some(false) if self.connect_attempts >= MAX_NATIVE_CONNECT_ATTEMPTS => return false,
            _ => {}
        }

        if self.native_client.is_none() {
            self.native_available = Some(false);
            self.connect_attempts = MAX_NATIVE_CONNECT_ATTEMPTS;
            return false;
        }
        // A connect alone proves nothing on Android: `adb forward` accepts
        // the TCP connection even when no probe is listening in the app.
        // So the attempt counter is only reset by a real idle *answer*
        // (see wait_for_idle); otherwise Auto mode retried forever.
        match self.ensure_probe(driver, device_id).await {
            Ok(true) => {
                self.native_available = Some(true);
                true
            }
            Ok(false) => {
                // Another app's probe: polling until the next launch.
                self.native_available = Some(false);
                self.connect_attempts = MAX_NATIVE_CONNECT_ATTEMPTS;
                false
            }
            Err(e) => {
                debug!(error = %e, "no native sync probe");
                self.connect_attempts += 1;
                self.native_available = Some(false);
                false
            }
        }
    }

    /// Use native probe to wait for idle.
    async fn native_wait_for_idle(
        &mut self,
        driver: &dyn atalanta_common::PlatformDriver,
        device_id: &str,
    ) -> Result<()> {
        if !self.ensure_probe(driver, device_id).await? {
            return Err(AtalantaError::Config(
                "Native sync probe belongs to another app".to_string(),
            ));
        }
        let client = self
            .native_client
            .as_mut()
            .ok_or_else(|| AtalantaError::Config("Native sync client not available".to_string()))?;

        // Bounded by the settle cap like a polling settle: an app with a
        // looping animation never idles, and waiting the whole sync timeout
        // on every step made native mode 10x slower than polling.
        let cfg = self.polling_engine.config();
        let cap_ms = cfg.settle_cap().min(cfg.timeout_ms).max(1);
        // Idle must hold, not be one instantaneous sample (protocol 2).
        let quiet_ms = cfg.quiet().min(PROBE_QUIET_MAX_MS);
        let start = std::time::Instant::now();
        let mut response = client.wait_for_idle_holding(cap_ms, Some(quiet_ms)).await?;
        if !response.idle {
            // Capped: that action's floor is over (s6/s7 m3).
            self.polling_engine.forget_action();
        }
        // P1: "idle" right after an action may predate its reaction. Hold
        // the reaction floor; if the screen starts changing, ask again.
        self.polling_engine.clear_stats();
        let reacted = response.idle
            && self
                .polling_engine
                .reaction_seen(driver, device_id)
                .await
                .unwrap_or(false);
        // A late reaction the probe was asked about again and confirmed
        // is still a settled screen; one whose second ask failed is not.
        let mut confirmed = true;
        if reacted {
            let left = (cap_ms as u128).saturating_sub(start.elapsed().as_millis()) as u64;
            // A probe can't answer idle in less than its hold, so a short
            // remainder reported an idle app as capped (s6/s7 m2): ask for
            // at least the hold and one more sample, past the cap if need be.
            let hold = quiet_ms.max(PROBE_MIN_HOLD_MS) + PROBE_MIN_HOLD_MS / 2;
            if let Some(client) = self.native_client.as_mut() {
                match client
                    .wait_for_idle_holding(left.max(hold), Some(quiet_ms))
                    .await
                {
                    Ok(r) => response = r,
                    // The first answer was idle and the floor has run:
                    // keep that answer rather than drop to a polling settle
                    // that has lost the action (s6/s7 m4). The next wait
                    // reconnects.
                    Err(e) => {
                        confirmed = false;
                        debug!(error = %e, "native sync: second ask failed; keeping the first idle answer")
                    }
                }
            }
            // The floor's picture is from mid-reaction: the next action
            // needs the settled one.
            self.polling_engine.refresh_picture(driver, device_id).await;
        }
        let floor_reads = self.polling_engine.last_stats().reads;
        let waited = start.elapsed();

        if response.idle {
            debug!(
                waited_ms = response.waited_ms.unwrap_or(0),
                "native sync: app is idle"
            );
        } else {
            // Not idle by the cap: carry on (the executor notes it), as a
            // capped polling settle does. An error here made Auto mode drop
            // the probe and pay a polling settle on top.
            debug!(?response.pending, "native sync: still busy at the settle cap");
        }
        // One probe answer plus the floor's reads; "reacted" when the
        // floor saw the screen change (or it couldn't tell, as before).
        self.polling_engine.set_busy((!response.idle).then(|| {
            if response.pending.is_empty() {
                "the in-app probe reported busy".to_string()
            } else {
                format!("busy: {}", response.pending.join(", "))
            }
        }));
        self.polling_engine.set_stats(crate::sync::SettleStats {
            duration: waited,
            reads: 1 + floor_reads,
            reacted: reacted || floor_reads == 0,
            capped: !response.idle,
            stable: response.idle && confirmed,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    async fn spawn_probe(reply: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(reply.as_bytes()).await;
        });
        port
    }

    #[tokio::test]
    async fn probe_error_reply_surfaces_as_error_not_parse_failure() {
        let port = spawn_probe("{\"error\":\"tracker not installed\"}\n").await;
        let mut client = NativeSyncClient::new(port, 1_000);
        assert!(client.connect().await);
        let err = client.status().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("tracker not installed"),
            "expected probe error message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn wait_idle_round_trip() {
        let port = spawn_probe("{\"idle\":true,\"pending\":[],\"waited_ms\":12}\n").await;
        let mut client = NativeSyncClient::new(port, 1_000);
        assert!(client.connect().await);
        let resp = client.wait_for_idle(500).await.unwrap();
        assert!(resp.idle);
        assert_eq!(resp.waited_ms, Some(12));
    }

    #[tokio::test]
    async fn unresponsive_probe_times_out_instead_of_hanging() {
        // Probe accepts the connection but never replies.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            // hold the socket open without replying
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let mut client = NativeSyncClient::new(port, 1_000);
        assert!(client.connect().await);
        // wait_for_idle(0) bounds the round-trip at ~5s; the overall test
        // asserts it errors rather than blocking forever.
        let result = tokio::time::timeout(Duration::from_secs(10), client.wait_for_idle(0)).await;
        let inner = result.expect("send_command must time out on its own");
        assert!(inner.is_err(), "expected timeout error");
        assert!(
            !client.is_connected(),
            "stream must be dropped after timeout"
        );
    }

    #[tokio::test]
    async fn a_cancelled_wait_never_hands_its_reply_to_the_next_request() {
        // Review 01 #1: a `wait_idle` dropped from outside (step watchdog,
        // crash watch) left its reply in the socket; the next request read
        // it as its own. The probe answers each connection's first request
        // late with `idle:false` and every later one at once with
        // `idle:true` plus `waited_ms` naming the connection.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut conn = 0u64;
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                conn += 1;
                let n = conn;
                tokio::spawn(async move {
                    let mut lines = BufReader::new(socket);
                    let mut first = true;
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let reply = if first && n == 1 {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            "{\"idle\":false,\"waited_ms\":1}\n".to_string()
                        } else {
                            format!("{{\"idle\":true,\"waited_ms\":{}}}\n", 100 + n)
                        };
                        first = false;
                        if lines.get_mut().write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let mut client = NativeSyncClient::new(port, 1_000);
        assert!(client.connect().await);
        // Dropped before the late reply arrives.
        let cut =
            tokio::time::timeout(Duration::from_millis(50), client.wait_for_idle(1_000)).await;
        assert!(cut.is_err(), "the wait must still be running when cut");
        assert!(
            !client.is_connected(),
            "an abandoned exchange leaves the connection untrusted"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        let next = client.wait_for_idle(1_000).await.unwrap();
        assert!(next.idle, "read the abandoned request's reply: {next:?}");
        assert_eq!(next.waited_ms, Some(102), "answered on a new connection");
    }

    #[tokio::test]
    async fn a_lost_probe_is_transient_not_a_config_error() {
        let port = spawn_probe("").await;
        let mut client = NativeSyncClient::new(port, 1_000);
        assert!(client.connect().await);
        let e = client.wait_for_idle(100).await.unwrap_err();
        assert!(matches!(e, AtalantaError::SyncProbeLost { .. }), "{e:?}");
        assert!(e.is_transient(), "{e}");
    }

    #[tokio::test]
    async fn a_never_idle_app_costs_the_settle_cap_not_the_timeout() {
        // VEL-064: the probe was asked to wait the full sync timeout.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = asked.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            *seen.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = socket
                .write_all(b"{\"idle\":false,\"pending\":[\"animation\"]}\n")
                .await;
        });
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: port,
            timeout_ms: 10_000,
            settle_cap_ms: Some(300),
            ..Default::default()
        };
        let mut sync = SyncManager::new(&config, atalanta_common::Platform::Android);
        // This probe predates `hello` (it answers the first line with a
        // status): a probe that can't identify itself is an error, so ask
        // the client directly.
        let client = sync.native_client.as_mut().unwrap();
        assert!(client.connect().await);
        sync.handshake_done = true;
        sync.native_wait_for_idle(&Page::default(), "dev")
            .await
            .unwrap();
        assert!(
            asked.lock().unwrap().contains("\"timeout_ms\":300"),
            "{}",
            asked.lock().unwrap()
        );
        assert!(sync.last_settle().capped);
        // P9: the warning can say what kept it busy.
        assert_eq!(sync.busy_reason(), Some("busy: animation"));
    }

    /// A fake probe: answers each request line with `reply(line)` on every
    /// connection, and records the lines.
    struct FakeProbe {
        port: u16,
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeProbe {
        async fn spawn(reply: fn(&str) -> String) -> Self {
            Self::spawn_at(0, reply).await
        }

        async fn spawn_at(port: u16, reply: fn(&str) -> String) -> Self {
            Self::spawn_reply(port, std::sync::Arc::new(reply)).await
        }

        async fn spawn_with(reply: Box<dyn Fn(&str) -> String + Send + Sync>) -> Self {
            Self::spawn_reply(0, std::sync::Arc::from(reply)).await
        }

        async fn spawn_reply(
            port: u16,
            reply: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
        ) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = lines.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = seen.clone();
                    let reply = reply.clone();
                    tokio::spawn(async move {
                        let (read, mut write) = socket.into_split();
                        let mut read = BufReader::new(read);
                        let mut line = String::new();
                        while read.read_line(&mut line).await.unwrap_or(0) > 0 {
                            seen.lock().unwrap().push(line.trim().to_string());
                            let out = reply(line.trim()) + "\n";
                            if write.write_all(out.as_bytes()).await.is_err() {
                                return;
                            }
                            line.clear();
                        }
                    });
                }
            });
            Self { port, lines }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    fn probe_of(bundle: &'static str, idle: bool) -> fn(&str) -> String {
        // fn pointers can't capture: pick from a fixed set.
        match (bundle, idle) {
            ("com.app", true) => |l| {
                if l.contains("hello") {
                    r#"{"hello":true,"bundle":"com.app","pid":11,"version":2,"unknown":[]}"#.into()
                } else {
                    r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
                }
            },
            ("com.app", false) => |l| {
                if l.contains("hello") {
                    r#"{"hello":true,"bundle":"com.app","pid":22,"version":2,"unknown":[]}"#.into()
                } else {
                    r#"{"idle":false,"pending":["network:1"],"waited_ms":300}"#.into()
                }
            },
            ("noname", _) => |l| {
                if l.contains("hello") {
                    r#"{"hello":true,"pid":44,"version":2}"#.into()
                } else {
                    r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
                }
            },
            ("com.other", _) => |l| {
                if l.contains("hello") {
                    r#"{"hello":true,"package":"com.other","pid":33,"version":2}"#.into()
                } else {
                    r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
                }
            },
            _ => |l| {
                // A probe from before `hello`.
                if l.contains("hello") {
                    r#"{"error":"unknown command: hello"}"#.into()
                } else {
                    r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
                }
            },
        }
    }

    fn native_sync(port: u16) -> SyncManager {
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Auto,
            native_port_android: port,
            settle_cap_ms: Some(300),
            reaction_ms: Some(50),
            quiet_ms: Some(20),
            ..Default::default()
        };
        SyncManager::new(&config, atalanta_common::Platform::Android).for_app("com.app")
    }

    #[tokio::test]
    async fn hello_names_the_probes_app_and_pid() {
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let mut client = NativeSyncClient::new(probe.port, 1_000);
        assert!(client.connect().await);
        match client.hello().await.unwrap() {
            Handshake::Identified(h) => {
                assert_eq!(h.app(), "com.app");
                assert_eq!(h.pid, Some(11));
            }
            other => panic!("{other:?}"),
        }
        let other = FakeProbe::spawn(probe_of("com.other", true)).await;
        let mut client = NativeSyncClient::new(other.port, 1_000);
        assert!(client.connect().await);
        match client.hello().await.unwrap() {
            Handshake::Identified(h) => assert_eq!(h.app(), "com.other", "Android `package`"),
            other => panic!("{other:?}"),
        }
    }

    /// P2: the probe's `hello` must name the app under test. Another app's
    /// probe on the port (a second simulator's app, a forward left by
    /// another run) used to answer every settle.
    #[tokio::test]
    async fn a_probe_of_another_app_is_not_trusted() {
        let probe = FakeProbe::spawn(probe_of("com.other", true)).await;
        let page = Page::default();
        let mut sync = native_sync(probe.port);
        sync.invalidate();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert!(
            !probe.lines().iter().any(|l| l.contains("wait_idle")),
            "{:?}",
            probe.lines()
        );
        assert!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "fell back to polling"
        );
        assert!(sync.warned.contains("probe-app"));
        // Not re-probed on every step until the next launch.
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert_eq!(probe.lines().len(), 1, "{:?}", probe.lines());
        sync.reset_native();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert_eq!(probe.lines().len(), 2, "re-checked after a launch");
    }

    /// Backward compatibility: a probe without `hello` still syncs, with a
    /// warning that it can't be checked.
    #[tokio::test]
    async fn a_probe_without_hello_still_works_with_a_warning() {
        let probe = FakeProbe::spawn(probe_of("legacy", true)).await;
        let page = Page::default();
        let mut sync = native_sync(probe.port);
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert!(probe.lines().iter().any(|l| l.contains("wait_idle")));
        assert_eq!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "native answered, no polling"
        );
        assert!(sync.warned.contains("probe-legacy"));
        // The handshake is once per connection.
        sync.wait_for_idle(&page, "dev").await.unwrap();
        let hellos = probe.lines().iter().filter(|l| l.contains("hello")).count();
        assert_eq!(hellos, 1, "{:?}", probe.lines());
    }

    /// P2 / N3: with a per-device forward the client talks to that device's
    /// probe, never to the suite-wide port another device's probe holds.
    #[tokio::test]
    async fn the_device_s_own_probe_port_is_used() {
        let b = FakeProbe::spawn(probe_of("com.app", true)).await;
        let page = Page {
            probe_port: Some(b.port),
            ..Default::default()
        };
        // The suite keeps the default port (the shared one).
        let mut sync = native_sync(atalanta_common::SyncConfig::default().native_port_android);
        sync.invalidate();
        sync.wait_for_idle(&page, "device-b").await.unwrap();
        assert!(b.lines().iter().any(|l| l.contains("wait_idle")));
        assert!(!sync.last_settle().capped, "B is idle");
    }

    /// m6: a suite that sets its own `nativePortAndroid` (an app that starts
    /// its probe on that port, forwarded by the team) is not overridden by
    /// the driver's forward to the default device port.
    #[tokio::test]
    async fn an_explicit_suite_probe_port_wins_over_the_driver_s() {
        let suite = FakeProbe::spawn(probe_of("com.app", true)).await;
        let driver = FakeProbe::spawn(probe_of("com.app", false)).await;
        let page = Page {
            probe_port: Some(driver.port),
            ..Default::default()
        };
        let mut sync = native_sync(suite.port);
        sync.invalidate();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert!(
            driver.lines().is_empty(),
            "the driver's port was used: {:?}",
            driver.lines()
        );
        assert!(suite.lines().iter().any(|l| l.contains("wait_idle")));
    }

    /// m1: Native mode with another app's probe polls, as the warning says,
    /// instead of running every step with no settle and a new handshake.
    #[tokio::test]
    async fn native_mode_polls_when_the_probe_is_another_app_s() {
        let probe = FakeProbe::spawn(probe_of("com.other", true)).await;
        let page = Page::default();
        let mut sync = native(probe.port);
        sync.invalidate();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "no settle at all"
        );
        assert!(sync.warned.contains("probe-app"));
        let trees = page.trees.load(std::sync::atomic::Ordering::SeqCst);
        sync.wait_for_idle_keyed(&page, "dev", "k").await.unwrap();
        assert!(page.trees.load(std::sync::atomic::Ordering::SeqCst) > trees);
        assert_eq!(probe.lines().len(), 1, "re-handshake: {:?}", probe.lines());
        sync.reset_native();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert_eq!(probe.lines().len(), 2, "re-checked after a launch");
    }

    /// m7: an Android app whose process isn't its package
    /// (`android:process`) is still its own probe.
    #[test]
    fn hello_matches_the_package_or_the_process() {
        let hello = |json: &str| serde_json::from_str::<ProbeHello>(json).unwrap();
        let new = hello(r#"{"bundle":"com.app","package":"com.app","process":"com.app.ui"}"#);
        assert!(new.is_app("com.app"));
        assert!(!new.is_app("com.other"));
        let by_process =
            hello(r#"{"bundle":"com.app.ui","package":"com.app.ui","process":"com.app"}"#);
        assert!(by_process.is_app("com.app"), "either name is accepted");
        assert!(hello(r#"{"bundle":"com.app:remote"}"#).is_app("com.app"));
        assert!(!hello(r#"{"bundle":"com.apple"}"#).is_app("com.app"));
        assert!(
            hello(r#"{"bundle":""}"#).is_app("com.app"),
            "an empty name is accepted"
        );
    }

    /// iOS passes the per-simulator port in the launch environment; a probe
    /// that predates it (or an app passing its own port) listens on the
    /// suite's port, and the host used to lose native sync altogether.
    #[tokio::test]
    async fn no_probe_on_the_device_port_falls_back_to_the_suite_port() {
        // Only a probe from before `hello` is used on the suite port
        // (review f5-f6 MAJOR-4).
        let suite = FakeProbe::spawn(probe_of("legacy", true)).await;
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let page = Page {
            probe_port: Some(closed),
            ..Default::default()
        };
        let mut sync = native_sync(suite.port);
        sync.invalidate();
        // The fake suite port is random, not an explicit setting.
        sync.suite_port_explicit = false;
        // An iOS simulator whose device port has refused past the grace.
        sync.suite_fallback = true;
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(idle_asks(&suite) >= 1, "{:?}", suite.lines());
        assert!(!sync.last_settle().capped);
        // It stays there: no new hello per step.
        sync.wait_for_idle(&page, "sim").await.unwrap();
        let hellos = suite.lines().iter().filter(|l| l.contains("hello")).count();
        assert_eq!(hellos, 1, "{:?}", suite.lines());
        assert!(sync.on_suite_port);
        sync.reset_native();
        assert!(
            !sync.on_suite_port,
            "a relaunch tries the device port again"
        );
    }

    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// An iOS-simulator-like manager: the suite port is the default one
    /// (not explicit), the fallback is allowed, the settle cap is 3 s.
    fn ios_like(suite: u16) -> SyncManager {
        let mut sync = native_sync(suite);
        sync.suite_port_explicit = false;
        sync.suite_fallback = true;
        sync.sync_config_mut().settle_cap_ms = Some(3_000);
        sync
    }

    /// MJ-A: right after launchApp the app's listener isn't up yet. The
    /// device port that comes up 300 ms later is used, never the suite's.
    #[tokio::test]
    async fn a_device_port_that_comes_up_late_is_used_not_the_suite_port() {
        let suite = FakeProbe::spawn(probe_of("com.app", true)).await;
        let device = closed_port();
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            FakeProbe::spawn_at(device, probe_of("com.app", true)).await
        });
        let page = Page {
            probe_port: Some(device),
            ..Default::default()
        };
        let mut sync = ios_like(suite.port);
        sync.reset_native();
        sync.invalidate();
        sync.wait_for_idle(&page, "sim").await.unwrap();
        let device = late.await.unwrap();
        assert!(idle_asks(&device) >= 1, "{:?}", device.lines());
        assert!(suite.lines().is_empty(), "{:?}", suite.lines());
        assert!(!sync.on_suite_port);
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(suite.lines().is_empty(), "{:?}", suite.lines());
    }

    /// DV-3 (iPhone 17 Pro simulator, 2026-10-25): an app without the probe
    /// cost the whole 2.5 s grace on every launchApp. Once a grace has run
    /// out with nothing on the device port, a relaunch doesn't wait again;
    /// a probe that answers later clears that.
    #[tokio::test]
    async fn a_relaunch_of_an_app_without_the_probe_does_not_wait_the_grace_again() {
        let device = closed_port();
        let page = Page {
            probe_port: Some(device),
            ..Default::default()
        };
        let mut sync = ios_like(closed_port());
        sync.reset_native();
        // The first launch's grace has run out with the port still closed.
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.invalidate();
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(sync.device_port_silent);

        sync.reset_native();
        sync.invalidate();
        let start = std::time::Instant::now();
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(1_500),
            "waited the grace again: {:?}",
            start.elapsed()
        );

        // The probe comes up after all: it is used, and the next launch gets
        // its grace back.
        let probe = FakeProbe::spawn_at(device, probe_of("com.app", true)).await;
        sync.reset_native();
        sync.invalidate();
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(idle_asks(&probe) >= 1, "{:?}", probe.lines());
        assert!(!sync.device_port_silent);
    }

    /// MJ-A: another simulator's probe on the shared suite port is never
    /// latched on to, even once the device port has refused past the grace.
    #[tokio::test]
    async fn another_simulator_s_probe_on_the_suite_port_is_never_latched() {
        let suite = FakeProbe::spawn(probe_of("com.other", true)).await;
        let page = Page {
            probe_port: Some(closed_port()),
            ..Default::default()
        };
        let mut sync = ios_like(suite.port);
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.invalidate();
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert_eq!(idle_asks(&suite), 0, "{:?}", suite.lines());
        assert!(!sync.on_suite_port, "latched on another app's probe");
        assert!(
            !sync.probe_rejected,
            "the device's own probe may still come"
        );
        assert!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "polled"
        );
    }

    /// Review f5-f6 MAJOR-4: parallel simulators run the same bundle, so
    /// an identified probe on the shared suite port, even one naming this
    /// app (or no app), may be another simulator's: never latched.
    #[tokio::test]
    async fn an_identified_probe_on_the_suite_port_is_never_latched() {
        for who in ["com.app", "noname"] {
            let suite = FakeProbe::spawn(probe_of(who, true)).await;
            let page = Page {
                probe_port: Some(closed_port()),
                ..Default::default()
            };
            let mut sync = ios_like(suite.port);
            sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
            sync.invalidate();
            sync.wait_for_idle(&page, "sim").await.unwrap();
            assert_eq!(idle_asks(&suite), 0, "{who}: {:?}", suite.lines());
            assert!(!sync.on_suite_port, "{who}: latched on the suite port");
            assert!(!sync.probe_rejected, "{who}");
            assert!(
                page.trees.load(std::sync::atomic::Ordering::SeqCst) > 0,
                "{who}: polled"
            );
        }
    }

    /// Review f5-f6 MINOR-6: once the device port connects, a later
    /// refusal in the same launch starts a fresh grace.
    #[tokio::test]
    async fn the_grace_clock_resets_once_the_device_port_connects() {
        let suite = FakeProbe::spawn(probe_of("legacy", true)).await;
        let own = FakeProbe::spawn(probe_of("com.app", true)).await;
        let page = Page {
            probe_port: Some(own.port),
            ..Default::default()
        };
        let mut sync = ios_like(suite.port);
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(idle_asks(&own) >= 1, "{:?}", own.lines());
        assert!(!sync.on_suite_port);
        assert!(
            sync.device_refused_since.is_none(),
            "an old refusal would skip the grace next time"
        );
    }

    /// MJ-A: on the suite port, the device port is tried again every few
    /// steps, and used once it answers.
    #[tokio::test]
    async fn the_device_port_is_rechecked_while_on_the_suite_port() {
        // Only a probe from before `hello` is used on the suite port
        // (review f5-f6 MAJOR-4).
        let suite = FakeProbe::spawn(probe_of("legacy", true)).await;
        let device = closed_port();
        let page = Page {
            probe_port: Some(device),
            ..Default::default()
        };
        let mut sync = ios_like(suite.port);
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.wait_for_idle(&page, "sim").await.unwrap();
        assert!(sync.on_suite_port);
        let own = FakeProbe::spawn_at(device, probe_of("com.app", true)).await;
        for _ in 0..DEVICE_PORT_RECHECK_STEPS {
            sync.wait_for_idle(&page, "sim").await.unwrap();
        }
        assert!(!sync.on_suite_port);
        assert!(idle_asks(&own) >= 1, "{:?}", own.lines());
    }

    /// Review MINOR 10: Android's per-device forward never flips to the
    /// suite port (another device's `adb forward tcp:19401`).
    #[tokio::test]
    async fn android_never_falls_back_to_the_suite_port() {
        let suite = FakeProbe::spawn(probe_of("com.app", true)).await;
        let page = Page {
            probe_port: Some(closed_port()),
            ..Default::default()
        };
        let mut sync = native_sync(suite.port);
        sync.suite_port_explicit = false;
        sync.device_refused_since = Some(std::time::Instant::now() - DEVICE_PORT_GRACE);
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert!(suite.lines().is_empty(), "{:?}", suite.lines());
        assert!(!sync.on_suite_port);
    }

    /// P5: the host asks the probe for idle that holds (`quiet_ms`, at most
    /// 50 ms), and an old probe ignores the extra field.
    #[tokio::test]
    async fn wait_idle_asks_for_idle_that_holds() {
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let page = Page::default();
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: probe.port,
            ..Default::default()
        };
        let mut sync =
            SyncManager::new(&config, atalanta_common::Platform::Android).for_app("com.app");
        sync.wait_for_idle(&page, "dev").await.unwrap();
        let wait = probe
            .lines()
            .into_iter()
            .find(|l| l.contains("wait_idle"))
            .unwrap();
        assert!(wait.contains("\"quiet_ms\":50"), "{wait}");
    }

    fn native(port: u16) -> SyncManager {
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: port,
            reaction_ms: Some(120),
            settle_cap_ms: Some(2_000),
            ..Default::default()
        };
        SyncManager::new(&config, atalanta_common::Platform::Android).for_app("com.app")
    }

    fn idle_asks(probe: &FakeProbe) -> usize {
        probe
            .lines()
            .iter()
            .filter(|l| l.contains("wait_idle"))
            .count()
    }

    #[tokio::test]
    async fn native_idle_before_the_reaction_is_asked_again() {
        // P1: the probe said idle before the tap's reaction started, and
        // the step moved on to a screen still about to change.
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let mut sync = native(probe.port);
        sync.invalidate();
        let page = Page {
            grows: true,
            ..Default::default()
        };
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 2, "{:?}", probe.lines());

        // A screen that doesn't change: one answer, after the floor.
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let mut sync = native(probe.port);
        sync.invalidate();
        let start = std::time::Instant::now();
        sync.wait_for_idle(&Page::default(), "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 1, "{:?}", probe.lines());
        assert!(start.elapsed() >= Duration::from_millis(100));
        // No action since: no floor.
        let start = std::time::Instant::now();
        sync.wait_for_idle(&Page::default(), "d").await.unwrap();
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn the_floor_still_holds_after_a_step_that_reacted() {
        // Review M4: step 1's floor kept a mid-reaction tree as the next
        // "before"; step 2's first read differed from it, so the probe was
        // asked again at once and the floor never held.
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let mut sync = native(probe.port);
        let page = Page {
            grows: true,
            still_from: 2,
            ..Default::default()
        };
        sync.invalidate();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 2, "{:?}", probe.lines());
        // Step 2: an action the screen doesn't react to.
        sync.invalidate();
        let start = std::time::Instant::now();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 3, "{:?}", probe.lines());
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// The `timeout_ms` a request line asks for.
    fn asked_timeout(line: &str) -> u64 {
        line.split("\"timeout_ms\":")
            .nth(1)
            .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
    }

    const HELLO: &str = r#"{"hello":true,"bundle":"com.app","pid":11,"version":2,"unknown":[]}"#;
    const IDLE: &str = r#"{"idle":true,"pending":[],"waited_ms":40}"#;
    const BUSY: &str = r#"{"idle":false,"pending":["network:1"],"waited_ms":300}"#;

    #[tokio::test]
    async fn a_short_remainder_still_lets_the_probe_answer_idle() {
        // s6/s7 m2: the second ask got what was left of the cap (here
        // nothing), shorter than the probe's hold, so an idle app was
        // reported capped.
        let probe = FakeProbe::spawn(|l| {
            if l.contains("hello") {
                HELLO.into()
            } else if asked_timeout(l) < 40 {
                BUSY.into()
            } else {
                IDLE.into()
            }
        })
        .await;
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: probe.port,
            reaction_ms: Some(120),
            settle_cap_ms: Some(200),
            ..Default::default()
        };
        let mut sync =
            SyncManager::new(&config, atalanta_common::Platform::Android).for_app("com.app");
        let page = Page {
            grows: true,
            read_ms: 90,
            ..Default::default()
        };
        sync.invalidate();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 2, "{:?}", probe.lines());
        let second = probe.lines().into_iter().rfind(|l| l.contains("wait_idle"));
        assert!(asked_timeout(&second.unwrap()) >= 40);
        assert!(!sync.last_settle().capped, "{:?}", sync.last_settle());
    }

    #[tokio::test]
    async fn a_capped_answer_ends_the_actions_floor() {
        // s6/s7 m3: after a capped first ask the action stayed pending, and
        // the next (pre-action) wait held a floor for it with tree reads.
        static ASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let probe = FakeProbe::spawn(|l| {
            if l.contains("hello") {
                HELLO.into()
            } else if ASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                BUSY.into()
            } else {
                IDLE.into()
            }
        })
        .await;
        let mut sync = native(probe.port);
        let page = Page::default();
        sync.invalidate();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert!(sync.last_settle().capped);
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert!(!sync.last_settle().capped);
        assert_eq!(page.trees.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failed_second_ask_keeps_the_first_idle_answer() {
        // s6/s7 m4: the error made the wait fail (Auto: a polling settle
        // with the action already taken), and the picture stayed from
        // mid-reaction.
        static ASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let probe = FakeProbe::spawn(|l| {
            if l.contains("hello") {
                HELLO.into()
            } else if ASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                r#"{"error":"probe went away"}"#.into()
            } else {
                IDLE.into()
            }
        })
        .await;
        let mut sync = native(probe.port);
        let page = Page {
            grows: true,
            still_from: 2,
            ..Default::default()
        };
        sync.invalidate();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 2, "{:?}", probe.lines());
        assert!(!sync.last_settle().capped);
        // The settled picture was taken: a no-op action holds the floor.
        sync.invalidate();
        let start = std::time::Instant::now();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 3, "{:?}", probe.lines());
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn a_screen_that_moved_between_steps_still_holds_the_floor() {
        // Review-s8 m-d: a banner slid in while an assertion waited; the
        // next tap's first read differed from the old picture, so the floor
        // reported a reaction at once and the probe was asked again
        // before the tap's handler ran.
        let probe = FakeProbe::spawn(probe_of("com.app", true)).await;
        let mut sync = native(probe.port);
        let page = Page::default();
        sync.invalidate();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 1);
        // An assertion step: the screen moves, no action, no settle.
        page.moved.store(1, std::sync::atomic::Ordering::SeqCst);
        sync.forget_picture();
        // The tap the screen doesn't react to (yet).
        sync.invalidate();
        let start = std::time::Instant::now();
        sync.wait_for_idle(&page, "d").await.unwrap();
        assert_eq!(idle_asks(&probe), 2, "{:?}", probe.lines());
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// A page that settles on request and counts what it was asked.
    #[derive(Default)]
    struct Page {
        /// What `sync_probe_port` answers.
        probe_port: Option<u16>,
        settles: std::sync::atomic::AtomicUsize,
        trees: std::sync::atomic::AtomicUsize,
        /// (reaction, quiet, cap) of each settle asked for.
        asked: std::sync::Mutex<Vec<(u64, u64, u64)>>,
        /// Each tree read differs from the last (an app still reacting).
        grows: bool,
        /// With `grows`: from this read on, the tree stays the same.
        still_from: usize,
        /// How long each tree read takes.
        read_ms: u64,
        /// Bumped by a test: the screen changed with no action.
        moved: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl atalanta_common::PlatformDriver for Page {
        fn sync_probe_port(&self, _: &str) -> Option<u16> {
            self.probe_port
        }
        async fn settle_on_device(
            &self,
            _: &str,
            reaction_ms: u64,
            quiet_ms: u64,
            cap_ms: u64,
        ) -> Option<atalanta_common::DeviceSettle> {
            self.asked
                .lock()
                .unwrap()
                .push((reaction_ms, quiet_ms, cap_ms));
            self.settles
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(atalanta_common::DeviceSettle {
                settled: true,
                reacted: true,
                elapsed_ms: 1,
            })
        }
        async fn get_hierarchy(&self, _: &str) -> Result<atalanta_common::Element> {
            let n = self.trees.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.read_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.read_ms)).await;
            }
            let moved = self.moved.load(std::sync::atomic::Ordering::SeqCst);
            Ok(atalanta_common::Element {
                hint: None,
                platform_id: if self.grows {
                    let n = if self.still_from > 0 {
                        n.min(self.still_from)
                    } else {
                        n
                    };
                    format!("body{n}-{moved}")
                } else {
                    format!("body-{moved}")
                },
                label: None,
                text: None,
                element_type: "body".into(),
                bounds: atalanta_common::Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                enabled: true,
                visible: true,
                children: vec![],
                ..Default::default()
            })
        }
        async fn list_devices(&self) -> Result<Vec<atalanta_common::DeviceInfo>> {
            Ok(vec![])
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
            s: &atalanta_common::Selector,
        ) -> Result<atalanta_common::Element> {
            Err(AtalantaError::Config(format!("{s}")))
        }
        async fn find_elements(
            &self,
            _: &str,
            _: &atalanta_common::Selector,
        ) -> Result<Vec<atalanta_common::Element>> {
            Ok(vec![])
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
            Ok((10, 10))
        }
        async fn get_element_text(&self, _: &str, _: &atalanta_common::Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &atalanta_common::Element) -> Result<bool> {
            Ok(true)
        }
    }

    fn web(mode: atalanta_common::SyncMode) -> SyncManager {
        let config = atalanta_common::SyncConfig {
            mode,
            reaction_ms: Some(250),
            quiet_ms: Some(80),
            ..Default::default()
        };
        SyncManager::new(&config, atalanta_common::Platform::Web)
    }

    /// VEL-067: on the web an action is followed by the page's render
    /// settle, with the suite's timings; `sync.mode` was forced to `none`
    /// there, so only the network was waited for.
    #[tokio::test]
    async fn web_waits_for_the_page_to_settle_after_an_action() {
        let page = Page::default();
        let mut sync = web(atalanta_common::SyncMode::Auto);
        sync.wait_for_idle(&page, "web").await.unwrap();
        assert_eq!(
            page.settles.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing ran"
        );
        sync.invalidate();
        sync.wait_for_idle_keyed(&page, "web", "tap").await.unwrap();
        assert_eq!(page.settles.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (reaction, quiet, _) = page.asked.lock().unwrap()[0];
        assert_eq!((reaction, quiet), (250, 80), "the suite's timings");
        assert_eq!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no tree polling"
        );
    }

    /// NB-8 / SUM-4: a timing set on purpose is used as set on the web,
    /// even when it equals the native default: `settle: { reactionMs: 300,
    /// capMs: 3000 }` (or `sync.*`) was swapped for 30 ms / 1 s because it
    /// looked "unset". Unset timings still get the web's.
    #[tokio::test]
    async fn set_web_timings_are_never_swapped_for_the_web_defaults() {
        let page = Page::default();
        let config = atalanta_common::SyncConfig {
            reaction_ms: Some(300),
            settle_cap_ms: Some(3_000),
            ..Default::default()
        };
        let mut sync = SyncManager::new(&config, atalanta_common::Platform::Web);
        sync.invalidate();
        sync.wait_for_idle(&page, "web").await.unwrap();
        // A step's override is "set" too.
        sync.sync_config_mut().reaction_ms = Some(300);
        sync.sync_config_mut().settle_cap_ms = None;
        sync.invalidate();
        sync.wait_for_idle(&page, "web").await.unwrap();
        let asked = page.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            [(300, 150, 3_000), (300, 150, WEB_SETTLE_CAP_MS)],
            "set values as set; the unset cap is the web's"
        );
        assert_eq!(sync.settle_cap_ms(), WEB_SETTLE_CAP_MS);
    }

    /// M-3: with the default timings, a web action's settle uses the web's
    /// short reaction window and cap (the native 300 ms reaction made every
    /// web action wait that long), and a page that reacts at once costs a
    /// few ms, not the reaction window plus the network wait's.
    #[tokio::test]
    async fn web_defaults_keep_a_static_page_cheap() {
        let page = Page::default();
        let config = atalanta_common::SyncConfig::default();
        let mut sync = SyncManager::new(&config, atalanta_common::Platform::Web);
        let start = std::time::Instant::now();
        for _ in 0..10 {
            sync.invalidate();
            sync.wait_for_idle(&page, "web").await.unwrap();
        }
        let asked = page.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 10);
        assert!(
            asked
                .iter()
                .all(|&(r, _, c)| r == WEB_REACTION_MS && c == WEB_SETTLE_CAP_MS),
            "{asked:?}"
        );
        const { assert!(WEB_REACTION_MS <= 50 && WEB_SETTLE_CAP_MS <= 1_000) };
        // The fake page settles at once; what is left is the network
        // wait's window, already spent by the settle on a real page.
        // Generous: 10 actions well under 10 x 300 ms.
        assert!(
            start.elapsed() < std::time::Duration::from_millis(1_500),
            "{:?}",
            start.elapsed()
        );
    }

    /// `sync.mode` is honoured on the web: `none` keeps the network wait
    /// only, `polling` polls the tree (which starts from the page's settle).
    #[tokio::test]
    async fn web_honours_the_sync_mode() {
        let page = Page::default();
        let mut sync = web(atalanta_common::SyncMode::None);
        sync.invalidate();
        sync.wait_for_idle(&page, "web").await.unwrap();
        assert_eq!(page.settles.load(std::sync::atomic::Ordering::SeqCst), 0);

        let mut sync = web(atalanta_common::SyncMode::Polling);
        sync.invalidate();
        sync.wait_for_idle(&page, "web").await.unwrap();
        assert!(
            page.trees.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "polled the tree"
        );
    }

    /// A probe that takes `config` (protocol 2 with probe-urls).
    fn configurable_probe(l: &str) -> String {
        if l.contains("hello") {
            r#"{"hello":true,"bundle":"com.app","pid":11,"version":3,"unknown":[]}"#.into()
        } else if l.contains("\"config\"") {
            r#"{"ok":true,"invalid":["(bad"]}"#.into()
        } else {
            r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
        }
    }

    /// probe-urls: `web.networkIgnore` globs become probe regexes. A glob
    /// covers the whole URL; anything else is a substring, `?` literal.
    #[test]
    fn network_ignore_globs_become_probe_regexes() {
        assert_eq!(
            ignore_glob_to_regex("*/analytics/*"),
            r"(?-i)^(?>.*?/analytics/).*$"
        );
        assert_eq!(
            ignore_glob_to_regex("api.example.com/poll?x=1"),
            r"(?-i)api\.example\.com/poll\?x=1"
        );
        let re = |p: &str| regex::Regex::new(&rust_regex(&ignore_glob_to_regex(p))).unwrap();
        let glob = re("https://*.example.com/beacon*");
        assert!(glob.is_match("https://cdn.example.com/beacon?id=1"));
        assert!(!glob.is_match("https://cdn.example.org/beacon"));
        assert!(!glob.is_match("xhttps://a.example.com/beacon"), "anchored");
        let sub = re("/poll?x=1");
        assert!(sub.is_match("https://a/poll?x=1&y=2"));
        assert!(!sub.is_match("https://a/pollx=1"), "? is literal");
        assert!(re("(a)[b]{c}|d+$^\\").is_match("x(a)[b]{c}|d+$^\\y"));
    }

    /// The probe regex for the Rust `regex` crate, which has no atomic
    /// groups: `(?>x)` matches what `(?:x)` does here (the earliest match
    /// of each middle segment is never worse for a glob), only faster.
    fn rust_regex(probe: &str) -> String {
        probe.replace("(?>", "(?:")
    }

    /// review-d7 m4: the probes compile case-insensitively, the web glob
    /// is case-sensitive. `(?-i)` makes the probe match what the web does.
    #[test]
    fn probe_regexes_stay_case_sensitive_under_the_probes_flag() {
        let probe = |p: &str| {
            regex::RegexBuilder::new(&rust_regex(&ignore_glob_to_regex(p)))
                .case_insensitive(true)
                .build()
                .unwrap()
        };
        assert!(!probe("*/Track/*").is_match("https://a/api/track/order"));
        assert!(probe("*/Track/*").is_match("https://a/api/Track/order"));
        assert!(!probe("/Track/").is_match("https://a/api/track/order"));
        assert!(probe("/Track/").is_match("https://a/Track/x"));
    }

    /// review-d7 m5: runs of `*` collapse and each middle segment is an
    /// atomic earliest match, so a backtracking engine stays linear-ish;
    /// what matches is exactly the web's `glob_match`.
    #[test]
    fn probe_globs_are_atomic_and_agree_with_the_web() {
        let r = ignore_glob_to_regex("*/api/**/v2/***/poll*");
        assert_eq!(r, r"(?-i)^(?>.*?/api/)(?>.*?/v2/)(?>.*?/poll).*$");
        assert!(!r.contains(".*.*"), "{r}");
        let patterns = [
            "*",
            "**",
            "a*",
            "*a",
            "a*b",
            "*a*",
            "a*a",
            "*ab*ab*",
            "ab*ba*ab",
            "*.png",
            "https://*.example.com/*",
            "*/x/*/x",
            "a**b",
            "*aa*a",
            "a*b*a",
        ];
        let texts = [
            "",
            "a",
            "b",
            "ab",
            "ba",
            "aba",
            "abab",
            "aab",
            "aaab",
            "ababab",
            "abba",
            "abbaab",
            "x.png",
            "png",
            "https://cdn.example.com/beacon",
            "https://example.com/",
            "/x/y/x",
            "/x/x",
            "a/x/b/x",
            "aaa",
            "aaaa",
            "abcab",
            "ab ba ab",
        ];
        for p in patterns {
            let re = regex::Regex::new(&rust_regex(&ignore_glob_to_regex(p))).unwrap();
            for t in texts {
                assert_eq!(
                    re.is_match(t),
                    atalanta_common::glob_match(p, t),
                    "pattern {p:?} on {t:?}: {}",
                    ignore_glob_to_regex(p)
                );
            }
        }
    }

    /// review-d7 m3: a probe whose `hello` says protocol 2 applied
    /// `max_timer_ms` process-wide; it gets only `ignore_urls`, and a
    /// warning says to update it.
    #[tokio::test]
    async fn an_old_probe_gets_no_max_timer() {
        let probe = FakeProbe::spawn(|l| {
            if l.contains("hello") {
                r#"{"hello":true,"bundle":"com.app","pid":11,"version":2,"unknown":[]}"#.into()
            } else if l.contains("\"config\"") {
                r#"{"ok":true}"#.into()
            } else {
                r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
            }
        })
        .await;
        let mut config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: probe.port,
            settle_cap_ms: Some(300),
            reaction_ms: Some(50),
            quiet_ms: Some(20),
            ..Default::default()
        };
        config.max_timer_ms = Some(250);
        let mut sync = SyncManager::new(&config, atalanta_common::Platform::Android)
            .for_app("com.app")
            .with_probe_ignore(vec!["x".to_string()]);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        let lines = probe.lines();
        let cfg = lines
            .iter()
            .find(|l| l.contains("\"config\""))
            .unwrap_or_else(|| panic!("no config: {lines:?}"));
        let sent: serde_json::Value = serde_json::from_str(cfg).unwrap();
        assert!(sent.get("max_timer_ms").is_none(), "{cfg}");
        assert_eq!(sent["ignore_urls"], serde_json::json!(["x"]));
        assert!(sync.warned.contains("probe-max-timer-old"));
    }

    /// probe-urls: after `hello`, the host sends `config` with the ignore
    /// regexes and `sync.maxTimerMs`, on every new connection, before the
    /// first `wait_idle`.
    #[tokio::test]
    async fn the_probe_gets_ignore_urls_and_max_timer_after_hello() {
        let probe = FakeProbe::spawn(configurable_probe).await;
        let mut config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: probe.port,
            settle_cap_ms: Some(300),
            reaction_ms: Some(50),
            quiet_ms: Some(20),
            ..Default::default()
        };
        config.max_timer_ms = Some(250);
        let mut sync = SyncManager::new(&config, atalanta_common::Platform::Android)
            .for_app("com.app")
            .with_probe_ignore(vec!["^.*/analytics/.*$".to_string()]);
        let page = Page::default();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        let lines = probe.lines();
        let at = |what: &str| lines.iter().position(|l| l.contains(what));
        let cfg = at("\"config\"").unwrap_or_else(|| panic!("no config: {lines:?}"));
        assert!(at("hello").unwrap() < cfg, "{lines:?}");
        assert!(cfg < at("wait_idle").unwrap(), "{lines:?}");
        let sent: serde_json::Value = serde_json::from_str(&lines[cfg]).unwrap();
        assert_eq!(
            sent["ignore_urls"],
            serde_json::json!([r"^.*/analytics/.*$"])
        );
        assert_eq!(sent["max_timer_ms"], 250);
        // Once per connection: not again on the next step.
        sync.invalidate();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        let count = |p: &FakeProbe| {
            p.lines()
                .iter()
                .filter(|l| l.contains("\"config\""))
                .count()
        };
        assert_eq!(count(&probe), 1, "{:?}", probe.lines());
        // A new connection (the app relaunched) is configured again.
        sync.reset_native();
        sync.wait_for_idle(&page, "dev").await.unwrap();
        assert_eq!(count(&probe), 2, "{:?}", probe.lines());
        assert!(sync.warned.contains("probe-ignore-invalid"));
    }

    /// Nothing set: no `config` line, as before.
    #[tokio::test]
    async fn no_probe_config_without_settings() {
        let probe = FakeProbe::spawn(configurable_probe).await;
        let mut sync = native_sync(probe.port).with_probe_ignore(Vec::new());
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        assert!(
            !probe.lines().iter().any(|l| l.contains("\"config\"")),
            "{:?}",
            probe.lines()
        );
    }

    /// A probe that refuses `config` still syncs.
    #[tokio::test]
    async fn a_probe_without_config_still_syncs() {
        let probe = FakeProbe::spawn(|l| {
            if l.contains("hello") {
                r#"{"hello":true,"bundle":"com.app","pid":11,"version":2,"unknown":[]}"#.into()
            } else if l.contains("\"config\"") {
                r#"{"error":"unknown command: config"}"#.into()
            } else {
                r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
            }
        })
        .await;
        let mut sync = native_sync(probe.port).with_probe_ignore(vec!["x".into()]);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        assert!(probe.lines().iter().any(|l| l.contains("wait_idle")));
        assert!(sync.warned.contains("probe-config"));
    }

    /// A protocol-4 probe that takes `framework`, replying `reply` to `config`.
    fn rn_probe(
        version: u64,
        reply: &'static str,
    ) -> impl Fn(&str) -> String + Send + Sync + 'static {
        move |l: &str| {
            if l.contains("hello") {
                format!(
                    r#"{{"hello":true,"bundle":"com.app","pid":11,"version":{version},"unknown":[]}}"#
                )
            } else if l.contains("\"config\"") {
                reply.to_string()
            } else {
                r#"{"idle":true,"pending":[],"waited_ms":3}"#.into()
            }
        }
    }

    fn rn_sync(port: u16) -> SyncManager {
        let config = atalanta_common::SyncConfig {
            mode: atalanta_common::SyncMode::Native,
            native_port_android: port,
            settle_cap_ms: Some(300),
            reaction_ms: Some(50),
            quiet_ms: Some(20),
            ..Default::default()
        };
        SyncManager::new(&config, atalanta_common::Platform::Android)
            .for_app("com.app")
            .with_probe_ignore(Vec::new())
            .with_framework(atalanta_common::Framework::ReactNative)
    }

    fn config_line(probe: &FakeProbe) -> Option<serde_json::Value> {
        probe
            .lines()
            .iter()
            .find(|l| l.contains("\"config\""))
            .map(|l| serde_json::from_str(l).unwrap())
    }

    /// P4: `framework: react_native` is sent in `config` to a protocol-4
    /// probe (even with no other setting), after `hello`; what it can't
    /// track is warned about once.
    #[tokio::test]
    async fn react_native_is_sent_to_the_probe_in_config() {
        let f = rn_probe(
            4,
            r#"{"ok":true,"invalid":[],"framework":"react_native","unknown":["rn_mounting"]}"#,
        );
        let probe = FakeProbe::spawn_with(Box::new(f)).await;
        let mut sync = rn_sync(probe.port);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        let sent = config_line(&probe).unwrap_or_else(|| panic!("no config: {:?}", probe.lines()));
        assert_eq!(sent["framework"], "react_native");
        assert!(sent.get("ignore_urls").is_none(), "{sent}");
        assert!(sync.warned.contains("probe-rn-unknown"));
        assert!(!sync.warned.contains("probe-rn-off"));
    }

    /// P4: an older probe has no React Native tracking: nothing is sent and
    /// a warning says to update it. A native app sends no `framework`.
    #[tokio::test]
    async fn an_old_probe_or_a_native_app_gets_no_framework() {
        let probe = FakeProbe::spawn_with(Box::new(rn_probe(3, r#"{"ok":true}"#))).await;
        let mut sync = rn_sync(probe.port);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        assert!(config_line(&probe).is_none(), "{:?}", probe.lines());
        assert!(sync.warned.contains("probe-rn-old"));

        let probe = FakeProbe::spawn_with(Box::new(rn_probe(4, r#"{"ok":true}"#))).await;
        let mut sync = rn_sync(probe.port).with_framework(atalanta_common::Framework::Native);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        assert!(config_line(&probe).is_none(), "{:?}", probe.lines());
        assert!(!sync.warned.contains("probe-rn-old"));
    }

    /// P4: a probe that answers `config` without turning React Native on
    /// (`"framework":null`) still syncs, with a warning.
    #[tokio::test]
    async fn a_probe_that_does_not_turn_react_native_on_is_warned_about() {
        let probe = FakeProbe::spawn_with(Box::new(rn_probe(
            4,
            r#"{"ok":true,"invalid":[],"framework":null,"unknown":[]}"#,
        )))
        .await;
        let mut sync = rn_sync(probe.port);
        sync.wait_for_idle(&Page::default(), "dev").await.unwrap();
        assert!(probe.lines().iter().any(|l| l.contains("wait_idle")));
        assert!(sync.warned.contains("probe-rn-off"));
    }
}
