//! HTTP client for WebDriverAgent.
//!
//! Speed choices, each measured on Xcode 26.0.1 / iOS 26.0 (see
//! `docs/research/speed-ios-driver.md`):
//! - Tree reads use the session `/source` with
//!   `excluded_attributes=visible,accessible`: WDA computes those two with an
//!   extra accessibility IPC per node, ~89% of the read (556 → 58 ms). The
//!   parser works visibility out from frames instead.
//! - Session settings turn WDA's own idle and animation waits off
//!   (`waitForIdleTimeout`, `animationCoolOffTimeout` = 0): Atalanta runs its
//!   own settle, and WDA's waits cost up to 2 s per gesture on animated
//!   screens (element click 1,376 → 356–397 ms).
//! - Taps go through W3C `/actions` (≈360 ms vs 488 ms for a zero-length drag).
//!
//! Errors are typed (see `WdaClient::send`):
//! - a refused connection is [`AtalantaError::WdaUnreachable`] (the request
//!   never arrived, so it can be replayed after a restart). So is any broken
//!   connection (reset, closed, timed out) on a read or lookup: repeating
//!   one changes nothing. A read that timed out while WDA's main thread
//!   still answers (`/wda/screen`) is a plain command failure instead: WDA
//!   is busy, not dead, up to twice in a row;
//! - a broken connection on an action is [`AtalantaError::WdaLost`]: the tap
//!   may have landed, so it is never replayed;
//! - WDA's W3C `value.error` code decides the rest: `invalid session id` is
//!   [`AtalantaError::WdaSessionLost`] (the driver opens a new session),
//!   only `no such element` is [`AtalantaError::ElementNotFound`];
//! - a body that isn't JSON is an error with its status and first 200 bytes,
//!   never an empty value (an empty `/source` read as an empty screen).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atalanta_common::{AtalantaError, Result};
use base64::Engine;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};
use tracing::debug;

/// A reference to an element found by WDA.
#[derive(Debug, Clone)]
pub struct WdaElement {
    pub element_id: String,
}

/// Keys per minute when typing. WDA's default is 60, which makes a 20-char
/// email take 20 s; `ATALANTA_IOS_TYPING_FREQUENCY` overrides.
const DEFAULT_TYPING_FREQUENCY: u64 = 180;

/// `ios.accessibilityDeadlineMs` default (D-37): WDA's `accessibilityDeadline`,
/// sent by default only to a device whose WDA Atalanta built from v16.14.1
/// or later (see [`deadline_safe_ref`]): older WDA crashed WKWebView content
/// with it (#1290), and an externally started WDA's version is unknown.
const DEFAULT_ACCESSIBILITY_DEADLINE_MS: u64 = 5000;

/// Whether a WebDriverAgent built from `wda_ref` takes the default deadline:
/// a `vMAJOR.MINOR.PATCH` tag at v16.14.1 or later.
pub(crate) fn deadline_safe_ref(wda_ref: &str) -> bool {
    let v: Option<Vec<u32>> = wda_ref
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect();
    v.is_some_and(|v| v.len() == 3 && v >= vec![16, 14, 1])
}

/// The deadline to send: an explicit `ios.accessibilityDeadlineMs` always
/// wins; otherwise the default only for a WDA known to be built from a
/// v16.14.1+ ref (`wda_ref`, per device).
fn accessibility_deadline_ms(wda_ref: Option<&str>) -> u64 {
    match atalanta_common::env::var("ATALANTA_IOS_ACCESSIBILITY_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(ms) => ms,
        None if wda_ref.is_some_and(deadline_safe_ref) => DEFAULT_ACCESSIBILITY_DEADLINE_MS,
        None => 0,
    }
}
/// `ios.snapshotMaxDepth` default (D-37): Maestro's 60.
const DEFAULT_SNAPSHOT_MAX_DEPTH: u64 = 60;

/// A client's session id and auth as last set, shared with readers that
/// must not wait for the client's lock ([`WdaClient::published`]).
#[derive(Clone)]
pub struct Published(Arc<std::sync::Mutex<(Option<String>, RunnerAuth)>>);

impl Published {
    /// The session id and auth now.
    pub fn get(&self) -> (Option<String>, RunnerAuth) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct WdaClient {
    base_url: String,
    /// The device's UDID, carried into transport errors (review 03 L2);
    /// empty until [`Self::set_device_id`].
    device_id: String,
    session_id: Option<String>,
    /// The session id and auth, readable without the device's request lock
    /// (see [`Self::published`]).
    published: Published,
    client: Client,
    /// The per-call deadline `client` was built with (kept to rebuild it
    /// with other [`RunnerAuth`]).
    timeout: Duration,
    /// What every request carries: the runner's token and, through a
    /// usbmux forward, the `Host` the runner checks (R-1).
    auth: RunnerAuth,
    /// Window size never changes within a session; cached on first ask.
    screen_size: Mutex<Option<(i32, i32)>>,
    /// The status bar's height in points, cached per screen size (a
    /// rotation changes both).
    status_bar: Mutex<Option<((i32, i32), i32)>>,
    /// The runner has no W3C `/actions` route.
    no_actions: AtomicBool,
    /// Timed-out reads classed "busy" in a row, with no answer between
    /// them: past [`MAX_BUSY`] the runner is taken for hung.
    busy_streak: AtomicU32,
    /// Environment for the app this client launches (WDA's `environment`
    /// capability and launch parameter): the per-simulator
    /// `ATALANTA_SYNC_PORT` for the in-app sync probe.
    app_env: Vec<(String, String)>,
    /// Launch arguments (`launchApp: { arguments }`) for the next launch.
    app_args: Vec<String>,
    /// The WebDriverAgent ref this device's runner was built from, when
    /// known (the bootstrap says): decides the `accessibilityDeadline`
    /// default per device.
    wda_ref: Option<String>,
}

/// Consecutive "busy" timeouts before a WDA whose main thread still
/// answers is treated as unreachable all the same (review MAJOR-1).
const MAX_BUSY: u32 = 2;

/// A swipe gesture's stroke time (it flings, like a person's): `ios.swipeMs`.
fn swipe_fling_ms() -> u64 {
    atalanta_common::settings::num("ATALANTA_IOS_SWIPE_MS", 250)
}

/// A scroll's stroke (`ios.scrollMs`, 350 by default): the first 3/7 of
/// the time covers 94% of the way, the rest the last 6%, so it doesn't fling.
fn scroll_ms() -> (u64, u64) {
    let total = atalanta_common::settings::num("ATALANTA_IOS_SCROLL_MS", 350).max(20);
    let fast = total * 3 / 7;
    (fast, total - fast)
}

/// What a request to the runner must carry (R-1): AtalantaRunner refuses
/// a request without its bearer token, or whose `Host` isn't its own
/// loopback address and port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerAuth {
    /// The bearer token the runner was started with: the per-launch one
    /// (`WdaBootstrap`), or `ios.runnerToken`.
    pub token: Option<String>,
    /// `Host` to send instead of the URL's: through a usbmux forward the
    /// local port isn't the runner's (`127.0.0.1:8100` on the device).
    pub host: Option<String>,
}

/// A client for the iOS runner (WebDriverAgent or AtalantaRunner). It never
/// goes through a proxy: the runner is on loopback, a usbmuxd forward or the
/// local network, and `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` set for the
/// AI providers or `http` steps must not capture it (ENT-062).
pub fn runner_client() -> reqwest::ClientBuilder {
    Client::builder().no_proxy()
}

/// `Authorization: Bearer <token>` (marked sensitive, so it is never
/// logged) and the `Host` override, as default headers for every request.
pub(crate) fn runner_headers(auth: &RunnerAuth) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(Ok(mut v)) = auth
        .token
        .as_deref()
        .map(|t| reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")))
    {
        v.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    if let Some(Ok(v)) = auth
        .host
        .as_deref()
        .map(reqwest::header::HeaderValue::from_str)
    {
        headers.insert(reqwest::header::HOST, v);
    }
    headers
}

impl WdaClient {
    /// Record the WebDriverAgent ref this device's runner was built from
    /// (`None`: unknown, or not WebDriverAgent).
    pub fn set_wda_ref(&mut self, wda_ref: Option<&str>) {
        self.wda_ref = wda_ref.map(str::to_string);
    }

    /// See [`Self::set_wda_ref`].
    pub fn wda_ref(&self) -> Option<&str> {
        self.wda_ref.as_deref()
    }

    /// Name the device this client drives, for its error messages.
    pub fn set_device_id(&mut self, device_id: &str) {
        self.device_id = device_id.to_string();
    }

    /// The UDID set by [`Self::set_device_id`] (empty if never set).
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn new(base_url: &str) -> Self {
        // A hard cap per call: XCTest can retry an accessibility IPC
        // timeout for minutes (appium#19382).
        Self::with_timeout(base_url, Duration::from_secs(30))
    }

    /// A client whose calls give up after `timeout` (tests use a short one).
    /// It sends `ios.runnerToken` when set (a runner at `ATALANTA_WDA_URL`
    /// that Atalanta didn't start); [`Self::set_auth`] replaces it with the
    /// token the bootstrap launched the runner with.
    pub fn with_timeout(base_url: &str, timeout: Duration) -> Self {
        let auth = RunnerAuth {
            token: crate::wda_bootstrap::runner_token(),
            host: None,
        };
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            device_id: String::new(),
            session_id: None,
            published: Published(Arc::new(std::sync::Mutex::new((None, auth.clone())))),
            client: Self::http(timeout, &auth),
            timeout,
            auth,
            screen_size: Mutex::new(None),
            status_bar: Mutex::new(None),
            no_actions: AtomicBool::new(false),
            busy_streak: AtomicU32::new(0),
            app_env: Vec::new(),
            app_args: Vec::new(),
            wda_ref: None,
        }
    }

    fn http(timeout: Duration, auth: &RunnerAuth) -> Client {
        runner_client()
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(60))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5).min(timeout))
            .timeout(timeout)
            .default_headers(runner_headers(auth))
            .build()
            .unwrap_or_else(|_| runner_client().build().unwrap_or_default())
    }

    /// Send `auth` on every request from now on (the runner was launched,
    /// or relaunched, with a new token).
    pub fn set_auth(&mut self, auth: RunnerAuth) {
        if auth != self.auth {
            self.client = Self::http(self.timeout, &auth);
            self.auth = auth;
            self.publish();
        }
    }

    /// A handle that always reads this client's current session id and
    /// auth. The crash watcher polls through it: it used to take the
    /// device's request lock every tick, so a 30 s `/source` held crash
    /// detection up for as long (review 01 #9).
    pub fn published(&self) -> Published {
        self.published.clone()
    }

    fn publish(&self) {
        *self.published.0.lock().unwrap_or_else(|e| e.into_inner()) =
            (self.session_id.clone(), self.auth.clone());
    }

    /// What every request carries (see [`Self::set_auth`]).
    pub fn auth(&self) -> &RunnerAuth {
        &self.auth
    }

    /// Launch apps with this environment from now on.
    pub fn set_app_env(&mut self, env: Vec<(String, String)>) {
        self.app_env = env;
    }

    /// Launch apps with these arguments (`-name value` words). Set on every
    /// launch, so a launch without arguments doesn't inherit the last one's.
    pub fn set_app_args(&mut self, args: Vec<String>) {
        self.app_args = args;
    }

    /// The launch environment as a JSON object, when there is one.
    fn app_env_json(&self) -> Option<Value> {
        (!self.app_env.is_empty()).then(|| {
            Value::Object(
                self.app_env
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect(),
            )
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_session_id(&mut self, id: String) {
        self.session_id = Some(id);
        self.publish();
    }

    pub fn set_session_id_opt(&mut self, id: Option<String>) {
        self.session_id = id;
        self.publish();
        *self.screen_size.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.status_bar.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn session_url(&self) -> Result<String> {
        let sid = self
            .session_id
            .as_deref()
            .ok_or_else(|| AtalantaError::Config("No active WDA session".to_string()))?;
        Ok(format!("{}/session/{}", self.base_url, sid))
    }

    fn extract_error(body: &Value) -> String {
        body.get("value")
            .and_then(|v| v.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown WDA error")
            .to_string()
    }

    /// Send a request; `Ok(reply)` once WDA answered with JSON, whatever
    /// its status. A transport failure or a body that isn't JSON is an
    /// error (see the module docs).
    async fn send(&self, req: RequestBuilder, what: &str) -> Result<Reply> {
        let (client, req) = req.build_split();
        let req = req.map_err(|e| self.failed(what, chain(&e)))?;
        let replay = replay_safe(req.method(), req.url().path());
        let resp = match client.execute(req).await {
            Ok(r) => r,
            Err(e) => return Err(self.broken(what, e, replay).await),
        };
        let status = resp.status().as_u16();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return Err(self.broken(what, e, replay).await),
        };
        self.busy_streak.store(0, Ordering::Relaxed);
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| {
            self.failed(
                what,
                format!(
                    "unparsable WDA response (HTTP {status}, {e}): {}",
                    snippet(&bytes)
                ),
            )
        })?;
        if !(200..300).contains(&status) {
            debug!(what, status, "WDA request failed");
        }
        Ok(Reply { status, body })
    }

    /// Send and require success; returns the body's `value`.
    async fn call(&self, req: RequestBuilder, what: &str) -> Result<Value> {
        let reply = self.send(req, what).await?;
        if !reply.ok() {
            return Err(self.w3c_error(what, &reply));
        }
        Ok(reply.body.get("value").cloned().unwrap_or(Value::Null))
    }

    /// The error a failed reply stands for, by its W3C `value.error` code
    /// (stable across WDA versions, unlike the message text).
    fn w3c_error(&self, what: &str, reply: &Reply) -> AtalantaError {
        let code = reply.code();
        let detail = match reply.body["value"]["message"].as_str() {
            Some(m) => m.to_string(),
            None => format!(
                "HTTP {}: {}",
                reply.status,
                snippet(reply.body.to_string().as_bytes())
            ),
        };
        match code {
            "invalid session id" => AtalantaError::WdaSessionLost {
                test_name: format!("WDA {what} at {}", self.base_url),
                attempt: 1,
                max: 1,
            },
            "no such element" => AtalantaError::ElementNotFound {
                selector: what.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            },
            "invalid selector" => AtalantaError::InvalidSelector {
                test_name: String::new(),
                step_index: 0,
                reason: format!("WDA {what}: {detail}"),
            },
            // AtalantaRunner answers 500 with the XCTest issues an action
            // recorded (P1); typing with nothing focused is a test failure,
            // not a flaky command to retry.
            _ if detail.contains("has keyboard focus") => AtalantaError::AssertionFailed {
                expected: format!("a focused text input for {what}"),
                actual: detail,
                selector: String::new(),
                screenshot: None,
            },
            _ => match crashed_app(&detail) {
                Some(app_id) => AtalantaError::AppCrashed { app_id, detail },
                None => self.failed(what, detail),
            },
        }
    }

    fn failed(&self, what: &str, detail: String) -> AtalantaError {
        AtalantaError::CommandFailed {
            device_id: self.base_url.clone(),
            command: format!("WDA {what}"),
            detail,
        }
    }

    /// [`Self::transport_error`], except that a read that timed out while
    /// WDA's main thread still answers is a busy WDA, not a dead one: a
    /// plain command failure (one retry upstream), so the session is not
    /// re-created and the app not relaunched (review M1; appium#19382 is
    /// the slow-but-alive case the timeout exists for).
    ///
    /// The probe is `/wda/screen`, which both WDA and AtalantaRunner serve
    /// on the main thread. `/status` is no good: the runner answers it on
    /// its network queue, so it answers while the main thread is wedged and
    /// a hung runner would be "busy" forever (review MAJOR-1). And busy is
    /// capped: after [`MAX_BUSY`] in a row it is unreachable, so the driver
    /// recovers it.
    async fn broken(&self, what: &str, e: reqwest::Error, replay: bool) -> AtalantaError {
        if replay && e.is_timeout() && self.main_thread_answers().await {
            let streak = self.busy_streak.fetch_add(1, Ordering::Relaxed) + 1;
            if streak <= MAX_BUSY {
                return self.failed(
                    what,
                    format!(
                        "WDA busy: no answer in time, though it still answers ({})",
                        chain(&e)
                    ),
                );
            }
            self.busy_streak.store(0, Ordering::Relaxed);
            return AtalantaError::WdaUnreachable {
                device_id: self.device_id.clone(),
                url: self.base_url.clone(),
                detail: format!(
                    "{what}: no answer {streak} times in a row though WDA still answers; \
                     taking it for hung ({})",
                    chain(&e)
                ),
            };
        }
        self.transport_error(what, e, replay)
    }

    /// Whether a route served on WDA's main thread (`/wda/screen`) answers
    /// within 3 s. Any HTTP answer counts: an old WDA without the route
    /// still answers 404 from the same thread.
    async fn main_thread_answers(&self) -> bool {
        let url = format!("{}/wda/screen", self.base_url);
        self.client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    /// A request that got no answer. Refused: it never arrived. Broken
    /// mid-way (reset, closed, timed out): a read is as good as never sent,
    /// an action may have run (IOS-N3: these used to be plain command
    /// failures, so a WDA that hung or died mid-request was never restarted).
    fn transport_error(&self, what: &str, e: reqwest::Error, replay: bool) -> AtalantaError {
        let url = self.base_url.clone();
        let device_id = self.device_id.clone();
        if e.is_connect() {
            AtalantaError::WdaUnreachable {
                device_id,
                url,
                detail: format!("{what}: connection refused ({})", chain(&e)),
            }
        } else if replay {
            AtalantaError::WdaUnreachable {
                device_id,
                url,
                detail: format!("{what}: connection lost ({})", chain(&e)),
            }
        } else {
            AtalantaError::WdaLost {
                device_id,
                url,
                detail: format!("{what}: {}", chain(&e)),
            }
        }
    }

    /// Create a session (launching `bundle_id` when it isn't empty) and apply
    /// Atalanta's session settings. Returns the session id.
    pub async fn create_session(&mut self, bundle_id: &str) -> Result<String> {
        let url = format!("{}/session", self.base_url);
        let mut always = json!({});
        if !bundle_id.is_empty() {
            always["bundleId"] = json!(bundle_id);
            // The session launches the app before `apply_settings` can turn
            // WDA's idle wait off: without these WDA waited up to 10 s for
            // the app to go quiet first (4.4 s for Settings on an iOS 26
            // simulator, DV-2). Atalanta settles after the launch itself.
            always["shouldWaitForQuiescence"] = json!(false);
            always["waitForIdleTimeout"] = json!(0);
            if let Some(env) = self.app_env_json() {
                always["environment"] = env;
            }
            if !self.app_args.is_empty() {
                always["arguments"] = json!(self.app_args);
            }
        }
        let payload = json!({ "capabilities": { "alwaysMatch": always } });
        debug!(bundle_id, "creating WDA session");

        let reply = self
            .send(self.client.post(&url).json(&payload), "create session")
            .await?;
        if !reply.ok() {
            return Err(self.failed("create session", Self::extract_error(&reply.body)));
        }
        let body = reply.body;
        let session_id = body
            .get("value")
            .and_then(|v| v.get("sessionId"))
            .or_else(|| body.get("sessionId"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                // WDA answered something else: its trouble, not the config's
                // (VEL-146).
                self.failed(
                    "create session",
                    format!(
                        "no sessionId in the answer: {}",
                        snippet(body.to_string().as_bytes())
                    ),
                )
            })?
            .to_string();

        debug!(session_id = %session_id, "WDA session created");
        self.set_session_id_opt(Some(session_id.clone()));
        if let Err(e) = self.apply_settings(bundle_id).await {
            debug!("WDA settings not applied: {e}");
        }
        Ok(session_id)
    }

    /// Session settings: WDA's own waits off (Atalanta settles itself),
    /// faster typing, and (D-37) a bounded accessibility wait and snapshot
    /// depth:
    /// - `accessibilityDeadline` (seconds, from `ios.accessibilityDeadlineMs`,
    ///   default 5 s): WDA's default 0 lets one stuck AX query hang the
    ///   whole read. Sent by default only to the v16.14.1+ WDA the
    ///   bootstrap built (older WDA crashed WKWebView content, #1290);
    ///   an explicit value always applies, 0 never sends it.
    /// - `snapshotMaxDepth` (`ios.snapshotMaxDepth`, default 60, Maestro's
    ///   value; WDA's own default is 50), so WDA and AtalantaRunner cut a
    ///   deep tree at the same depth.
    async fn apply_settings(&self, bundle_id: &str) -> Result<()> {
        let frequency = atalanta_common::env::var("ATALANTA_IOS_TYPING_FREQUENCY")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TYPING_FREQUENCY);
        let mut settings = json!({
            "waitForIdleTimeout": 0,
            "animationCoolOffTimeout": 0,
            "maxTypingFrequency": frequency,
        });
        let deadline_ms = accessibility_deadline_ms(self.wda_ref.as_deref());
        if deadline_ms > 0 {
            settings["accessibilityDeadline"] = json!(deadline_ms as f64 / 1000.0);
        }
        let depth = atalanta_common::settings::num(
            "ATALANTA_IOS_SNAPSHOT_MAX_DEPTH",
            DEFAULT_SNAPSHOT_MAX_DEPTH,
        );
        if depth > 0 {
            settings["snapshotMaxDepth"] = json!(depth);
        }
        if !bundle_id.is_empty() {
            settings["defaultActiveApplication"] = json!(bundle_id);
        }
        let url = format!("{}/appium/settings", self.session_url()?);
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "settings": settings })),
            "settings",
        )
        .await
        .map(|_| ())
    }

    /// WebDriverAgent's MJPEG screen stream: frames per second, scale (in
    /// percent of the screen) and JPEG quality. Session settings.
    pub async fn set_mjpeg(&self, fps: u32, scaling: u32, quality: u32) -> Result<()> {
        let url = format!("{}/appium/settings", self.session_url()?);
        let settings = json!({
            "mjpegServerFramerate": fps,
            "mjpegScalingFactor": scaling,
            "mjpegServerScreenshotQuality": quality,
        });
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "settings": settings })),
            "settings",
        )
        .await
        .map(|_| ())
    }

    /// AtalantaRunner only: the installed apps, one of which is in front
    /// when the session has no app of its own. Sessionless (the runner
    /// keeps them across sessions).
    pub async fn set_app_candidates(&self, bundles: &[String]) -> Result<()> {
        let url = format!("{}/appium/settings", self.base_url);
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "settings": { "atalantaCandidates": bundles } })),
            "settings",
        )
        .await
        .map(|_| ())
    }

    /// Delete the current WDA session.
    pub async fn delete_session(&mut self) -> Result<()> {
        let url = self.session_url()?;
        debug!("deleting WDA session");
        self.call(self.client.delete(&url), "delete session")
            .await?;
        self.set_session_id_opt(None);
        Ok(())
    }

    /// Find a single element matching the given strategy and value.
    pub async fn find_element(&self, using: &str, value: &str) -> Result<WdaElement> {
        let url = format!("{}/element", self.session_url()?);
        debug!(using, value, "WDA find element");
        // Only WDA's `no such element` means it isn't there (VEL-144): a
        // lost session or a dead WDA used to read as "not found" too.
        let found = self
            .call(
                self.client
                    .post(&url)
                    .json(&json!({ "using": using, "value": value })),
                "find element",
            )
            .await
            .map_err(|e| match e {
                AtalantaError::ElementNotFound { .. } => AtalantaError::ElementNotFound {
                    selector: format!("{using}={value}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                },
                other => other,
            })?;
        // A reply without an id is WDA misbehaving, not the suite's
        // configuration: a config error now fails the lookup at once.
        let element_id =
            extract_element_id(&found).map_err(|detail| self.failed("find element", detail))?;
        Ok(WdaElement { element_id })
    }

    /// Find all elements matching the given strategy and value.
    pub async fn find_elements(&self, using: &str, value: &str) -> Result<Vec<WdaElement>> {
        let url = format!("{}/elements", self.session_url()?);
        debug!(using, value, "WDA find elements");
        let value = self
            .call(
                self.client
                    .post(&url)
                    .json(&json!({ "using": using, "value": value })),
                "find elements",
            )
            .await?;
        Ok(value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| extract_element_id(v).ok())
                    .map(|element_id| WdaElement { element_id })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Click on an element by its element ID.
    pub async fn click(&self, element_id: &str) -> Result<()> {
        let url = format!("{}/element/{}/click", self.session_url()?, element_id);
        debug!(element_id, "WDA click");
        self.call(self.client.post(&url).json(&json!({})), "click")
            .await
            .map(|_| ())
    }

    /// Send keys (type text) into an element.
    pub async fn send_keys(&self, element_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/element/{}/value", self.session_url()?, element_id);
        debug!(element_id, "WDA send keys");
        self.call(
            self.client.post(&url).json(&json!({ "value": keys(text) })),
            "send keys",
        )
        .await
        .map(|_| ())
    }

    /// An element's frame, `(x, y, width, height)` in points.
    pub async fn element_rect(&self, element_id: &str) -> Result<(f64, f64, f64, f64)> {
        let url = format!("{}/element/{}/rect", self.session_url()?, element_id);
        let v = self.call(self.client.get(&url), "element rect").await?;
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        Ok((n("x"), n("y"), n("width"), n("height")))
    }

    /// An element's `value` attribute (a picker wheel's current value).
    pub async fn element_value(&self, element_id: &str) -> Result<String> {
        let url = format!(
            "{}/element/{}/attribute/value",
            self.session_url()?,
            element_id
        );
        let v = self.call(self.client.get(&url), "element value").await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    /// Turn a picker wheel one step, `order` "next" or "previous" (WDA's
    /// `/wda/pickerwheel/:uuid/select`).
    pub async fn picker_step(&self, element_id: &str, order: &str) -> Result<()> {
        let url = format!(
            "{}/wda/pickerwheel/{}/select",
            self.session_url()?,
            element_id
        );
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "order": order, "offset": 0.15 })),
            "picker wheel step",
        )
        .await
        .map(|_| ())
    }

    /// Clear an element's text content.
    pub async fn clear(&self, element_id: &str) -> Result<()> {
        let url = format!("{}/element/{}/clear", self.session_url()?, element_id);
        debug!(element_id, "WDA clear");
        self.call(self.client.post(&url).json(&json!({})), "clear")
            .await
            .map(|_| ())
    }

    /// Get the visible text of an element.
    pub async fn get_text(&self, element_id: &str) -> Result<String> {
        let url = format!("{}/element/{}/text", self.session_url()?, element_id);
        let value = self.call(self.client.get(&url), "get text").await?;
        Ok(value.as_str().unwrap_or("").to_string())
    }

    /// Check whether an element is currently displayed.
    pub async fn is_displayed(&self, element_id: &str) -> Result<bool> {
        let url = format!("{}/element/{}/displayed", self.session_url()?, element_id);
        let reply = self.send(self.client.get(&url), "is displayed").await?;
        if !reply.ok() {
            // Gone from the screen since the lookup (the UI animates while a
            // wait polls): not displayed. Anything else is an error (VEL-336).
            return match reply.code() {
                "stale element reference" | "no such element" => Ok(false),
                _ => Err(self.w3c_error("is displayed", &reply)),
            };
        }
        Ok(reply.body["value"].as_bool().unwrap_or(false))
    }

    /// The XML hierarchy. Without the costly `visible`/`accessible`
    /// attributes unless `ATALANTA_IOS_FULL_SOURCE=1`.
    pub async fn get_source(&self) -> Result<String> {
        let base = match self.session_url() {
            Ok(s) => s,
            Err(_) => self.base_url.clone(),
        };
        let url = if atalanta_common::env::var("ATALANTA_IOS_FULL_SOURCE").as_deref() == Ok("1") {
            format!("{base}/source")
        } else {
            format!("{base}/source?excluded_attributes=visible,accessible")
        };
        let value = self.call(self.client.get(&url), "source").await?;
        // Anything but XML text is an unreadable screen, not an empty one
        // (VEL-145).
        match value {
            Value::String(xml) => Ok(xml),
            other => Err(self.failed(
                "source",
                format!(
                    "source is not XML text: {}",
                    snippet(other.to_string().as_bytes())
                ),
            )),
        }
    }

    /// The hierarchy with WDA's `visible` (XCTest's own visibility, not
    /// the frame): asked for only while a modal layer is up (see
    /// `IosDriver::with_visibility`), as it costs ~10x a plain read.
    pub async fn get_source_with_visibility(&self) -> Result<String> {
        let base = match self.session_url() {
            Ok(s) => s,
            Err(_) => self.base_url.clone(),
        };
        let url = format!("{base}/source?excluded_attributes=accessible");
        match self.call(self.client.get(&url), "source").await? {
            Value::String(xml) => Ok(xml),
            other => Err(self.failed(
                "source",
                format!(
                    "source is not XML text: {}",
                    snippet(other.to_string().as_bytes())
                ),
            )),
        }
    }

    /// The system alert in front, if any: its text and button labels.
    ///
    /// Permission prompts and "Open in …?" belong to SpringBoard, so they are
    /// not in the app's `/source`, and the dialog handler never saw them (they
    /// then blocked every later test). WDA finds them across processes; one
    /// cheap request when there is none (404 "no such alert").
    pub async fn alert(&self) -> Option<(String, Vec<String>)> {
        let base = self.session_url().unwrap_or_else(|_| self.base_url.clone());
        let reply = self
            .send(self.client.get(format!("{base}/alert/text")), "alert text")
            .await
            .ok()?;
        if !reply.ok() {
            return None;
        }
        let text = reply
            .body
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let buttons = self
            .call(
                self.client.get(format!("{base}/wda/alert/buttons")),
                "alert buttons",
            )
            .await
            .ok()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        Some((text, buttons))
    }

    /// The bundle id of the app in front (`/wda/activeAppInfo`); `None`
    /// when the runner can't tell (AtalantaRunner).
    pub async fn active_bundle(&self) -> Option<String> {
        let reply = self
            .send(
                self.client
                    .get(format!("{}/wda/activeAppInfo", self.base_url)),
                "active app",
            )
            .await
            .ok()?;
        if !reply.ok() {
            return None;
        }
        reply.body["value"]["bundleId"].as_str().map(String::from)
    }

    /// `XCUIApplication.state` of `bundle_id` (`/wda/apps/state`; WDA and
    /// AtalantaRunner): 1 not running, 2 suspended, 3 background,
    /// 4 foreground. `None` when the runner can't tell (an older runner
    /// without the route, or any failure).
    pub async fn app_state(&self, bundle_id: &str) -> Option<u8> {
        let base = self.session_url().unwrap_or_else(|_| self.base_url.clone());
        let reply = self
            .send(
                self.client
                    .post(format!("{base}/wda/apps/state"))
                    .json(&json!({ "bundleId": bundle_id })),
                "app state",
            )
            .await
            .ok()?;
        if !reply.ok() {
            return None;
        }
        reply.body["value"]
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| (1..=4).contains(n))
    }

    /// Press the system alert's button labelled `name`.
    pub async fn press_alert_button(&self, name: &str) -> Result<()> {
        let base = self.session_url().unwrap_or_else(|_| self.base_url.clone());
        self.call(
            self.client
                .post(format!("{base}/alert/accept"))
                .json(&json!({ "name": name })),
            "alert accept",
        )
        .await
        .map(|_| ())
    }

    /// Take a screenshot via WDA, returning decoded PNG bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let url = format!("{}/screenshot", self.base_url);
        let value = self.call(self.client.get(&url), "screenshot").await?;
        let b64 = value
            .as_str()
            .ok_or_else(|| self.failed("screenshot", "no base64 data in the answer".to_string()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| self.failed("screenshot", format!("undecodable base64: {e}")))
    }

    /// The app's frame as the tree just read it (`width` × `height` in
    /// points). A cached window size the other way round is from before a
    /// rotation (an app that turns itself, or a reused session on a
    /// simulator rotated since) and is dropped, so the back swipe, direction
    /// swipes and the status bar are measured on the screen as it is now
    /// (review 09 #9). An empty frame says nothing.
    pub fn note_app_frame(&self, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let mut cached = self.screen_size.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_some_and(|(w, h)| (w > h) != (width > height)) {
            *cached = None;
        }
    }

    /// The window size in points, cached for the session (until a tree
    /// read shows the app turned: [`Self::note_app_frame`]).
    pub async fn get_screen_size(&self) -> Result<(i32, i32)> {
        if let Some(size) = *self.screen_size.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(size);
        }
        let url = format!("{}/window/size", self.session_url()?);
        let value = self.call(self.client.get(&url), "window size").await?;
        // WDA may send points as floats (393.0): as_i64 read those as
        // missing and the size fell back to an iPhone 14's, for the whole
        // session (second-pass review). A size that is really missing is an
        // error, not a guess.
        let dim = |k: &str| {
            value[k]
                .as_f64()
                .map(|v| v.round() as i32)
                .filter(|v| *v > 0)
        };
        let (Some(w), Some(h)) = (dim("width"), dim("height")) else {
            return Err(AtalantaError::CommandFailed {
                device_id: self.device_id.clone(),
                command: "window size".into(),
                detail: format!("WDA gave no usable window size: {value}"),
            });
        };
        let size = (w, h);
        *self.screen_size.lock().unwrap_or_else(|e| e.into_inner()) = Some(size);
        Ok(size)
    }

    /// D-38 (`ios.verifyTap`): what AtalantaRunner says is drawn at
    /// (`x`, `y`), from XCTest's private hit test. `Ok(None)` when it can't
    /// say: WebDriverAgent has no such route, or the private API is
    /// missing on this iOS. Replay-safe (it only reads).
    pub async fn hit_test(&self, x: f64, y: f64) -> Result<Option<HitTest>> {
        let url = format!("{}/atalanta/hitTest", self.session_url()?);
        let reply = self
            .send(
                self.client.post(&url).json(&json!({ "x": x, "y": y })),
                "hit test",
            )
            .await?;
        if !reply.ok() {
            let code = reply.code();
            if matches!(code, "unknown command" | "unknown method")
                || (code.is_empty() && reply.status == 404)
            {
                return Ok(None);
            }
            return Err(self.w3c_error("hit test", &reply));
        }
        Ok(HitTest::from_value(&reply.body["value"]))
    }

    /// The status bar's height in points: WDA's `/wda/screen` (SpringBoard's
    /// status bar), else an estimate from the `screen` size. Cached for the
    /// session per screen size: a rotation changes it (review M2).
    pub async fn status_bar_height(&self, screen: (i32, i32)) -> i32 {
        if let Some((at, h)) = *self.status_bar.lock().unwrap_or_else(|e| e.into_inner()) {
            if at == screen {
                return h;
            }
        }
        let url = format!("{}/wda/screen", self.base_url);
        let reported = self
            .call(self.client.get(&url), "screen")
            .await
            .ok()
            .and_then(|v| v["statusBarSize"]["height"].as_f64())
            .filter(|h| h.is_finite() && *h >= 0.0 && *h < 200.0)
            .map(|h| h.round() as i32);
        let h = if crate::hit::landscape_phone(screen.0, screen.1) {
            // SpringBoard reports its bar whatever the app shows; landscape
            // iPhones hide it.
            0
        } else {
            reported.unwrap_or_else(|| crate::hit::estimated_status_bar(screen.0, screen.1))
        };
        *self.status_bar.lock().unwrap_or_else(|e| e.into_inner()) = Some((screen, h));
        h
    }

    /// `drag` (row 3.1): press, hold `hold_ms`, move over `move_ms`, rest
    /// at the end, lift. W3C actions; without them `dragfromtoforduration`,
    /// whose duration is the press before the move.
    pub async fn drag_hold(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        if !self.no_actions.load(Ordering::Relaxed) {
            let url = format!("{}/actions", self.session_url()?);
            let payload = json!({ "actions": [{
                "type": "pointer", "id": "finger1",
                "parameters": { "pointerType": "touch" },
                "actions": [
                    { "type": "pointerMove", "duration": 0, "x": from.0, "y": from.1 },
                    { "type": "pointerDown" },
                    { "type": "pause", "duration": hold_ms },
                    { "type": "pointerMove", "duration": move_ms, "x": to.0, "y": to.1 },
                    { "type": "pause", "duration": 250 },
                    { "type": "pointerUp" }
                ]
            }]});
            let reply = self
                .send(self.client.post(&url).json(&payload), "drag")
                .await?;
            if reply.ok() {
                return Ok(());
            }
            let detail = Self::extract_error(&reply.body);
            let unsupported = matches!(reply.code(), "unknown command" | "unknown method")
                || detail.contains("nhandled endpoint")
                || detail.contains("nknown command");
            if !unsupported {
                return Err(self.w3c_error("drag", &reply));
            }
            self.no_actions.store(true, Ordering::Relaxed);
        }
        let url = format!("{}/wda/dragfromtoforduration", self.session_url()?);
        let payload = json!({
            "fromX": from.0, "fromY": from.1, "toX": to.0, "toY": to.1,
            "duration": hold_ms as f64 / 1000.0
        });
        self.call(self.client.post(&url).json(&payload), "drag")
            .await
            .map(|_| ())
    }

    /// Perform a swipe (drag) gesture from one point to another over a duration.
    pub async fn swipe(
        &self,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        duration: f64,
        fling: bool,
    ) -> Result<()> {
        let moves = (from_x - to_x).abs() + (from_y - to_y).abs() > 1.0;
        if moves && !self.no_actions.load(Ordering::Relaxed) {
            // W3C actions: WDA synthesizes the stroke at our speed.
            // `dragfromtoforduration` drags at XCTest's own slow pace, 1.38 s
            // a scroll (two thirds of each scroll's time). A fast stroke flings
            // (iOS then scrolls on for ~2.5 s, even after a pause at the end),
            // so the last 6% of the way is slow: the finger lifts at ~100 px/s,
            // the list stops ~0.2 s later, and each scroll moves a known
            // distance. Measured: 0.75 s a drag, 9 rows, no fling.
            let url = format!("{}/actions", self.session_url()?);
            // A swipe gesture keeps its fling (a pager turns only on one);
            // a scroll ends slowly so the content stops where the drag ends.
            let stroke = if fling {
                vec![
                    json!({ "type": "pointerMove", "duration": swipe_fling_ms(), "x": to_x, "y": to_y }),
                ]
            } else {
                vec![
                    json!({ "type": "pointerMove", "duration": scroll_ms().0,
                            "x": from_x + (to_x - from_x) * 0.94, "y": from_y + (to_y - from_y) * 0.94 }),
                    json!({ "type": "pointerMove", "duration": scroll_ms().1, "x": to_x, "y": to_y }),
                ]
            };
            let mut steps = vec![
                json!({ "type": "pointerMove", "duration": 0, "x": from_x, "y": from_y }),
                json!({ "type": "pointerDown" }),
            ];
            steps.extend(stroke);
            steps.push(json!({ "type": "pointerUp" }));
            let payload = json!({ "actions": [{
                "type": "pointer", "id": "finger1",
                "parameters": { "pointerType": "touch" },
                "actions": steps
            }]});
            let reply = self
                .send(self.client.post(&url).json(&payload), "swipe")
                .await?;
            if reply.ok() {
                return Ok(());
            }
            let detail = Self::extract_error(&reply.body);
            let unsupported = matches!(reply.code(), "unknown command" | "unknown method")
                || detail.contains("nhandled endpoint")
                || detail.contains("nknown command");
            if !unsupported {
                return Err(self.w3c_error("swipe", &reply));
            }
            debug!("no W3C actions ({detail}); using dragfromtoforduration");
            self.no_actions.store(true, Ordering::Relaxed);
        }
        let url = format!("{}/wda/dragfromtoforduration", self.session_url()?);
        let payload = json!({
            "fromX": from_x,
            "fromY": from_y,
            "toX": to_x,
            "toY": to_y,
            "duration": duration
        });
        debug!(from_x, from_y, to_x, to_y, duration, "WDA swipe");
        self.call(self.client.post(&url).json(&payload), "swipe")
            .await
            .map(|_| ())
    }

    /// Tap at screen coordinates through W3C actions.
    pub async fn tap_at(&self, x: f64, y: f64) -> Result<()> {
        self.taps_at(x, y, 1).await
    }

    /// `count` taps at one point in a single W3C action sequence, so a double
    /// tap's interval doesn't include an HTTP round trip.
    pub async fn taps_at(&self, x: f64, y: f64, count: usize) -> Result<()> {
        let url = format!("{}/actions", self.session_url()?);
        debug!(x, y, count, "WDA tap");
        self.call(
            self.client.post(&url).json(&tap_actions(x, y, count)),
            "tap",
        )
        .await
        .map(|_| ())
    }

    /// Type text into the currently focused element (session-level keys).
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let url = format!("{}/wda/keys", self.session_url()?);
        debug!("WDA type text (session-level)");
        self.call(
            self.client.post(&url).json(&json!({ "value": keys(text) })),
            "type text",
        )
        .await
        .map(|_| ())
    }

    /// The device pasteboard's text ("" when it holds none).
    pub async fn get_pasteboard(&self) -> Result<String> {
        let url = format!("{}/wda/getPasteboard", self.session_url()?);
        let v = self
            .call(
                self.client
                    .post(&url)
                    .json(&json!({ "contentType": "plaintext" })),
                "get pasteboard",
            )
            .await?;
        let b64 = v.as_str().unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap_or_default();
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Put `text` on the device pasteboard.
    pub async fn set_pasteboard(&self, text: &str) -> Result<()> {
        let url = format!("{}/wda/setPasteboard", self.session_url()?);
        let content = base64::engine::general_purpose::STANDARD.encode(text);
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "content": content, "contentType": "plaintext" })),
            "set pasteboard",
        )
        .await
        .map(|_| ())
    }

    /// Paste into the focused field: Cmd+V as a hardware key (no edit menu,
    /// no "Allow Paste" prompt). Needs Xcode 15+. `prime` first sends F13,
    /// a no-op: the first hardware key after an app launch is dropped.
    pub async fn paste(&self, prime: bool) -> Result<()> {
        let url = format!("{}/wda/element/0/keyboardInput", self.session_url()?);
        // XCUIKeyModifierCommand = 1 << 4.
        let mut keys = vec![json!({ "key": "v", "modifierFlags": 16 })];
        if prime {
            keys.insert(0, json!("XCUIKeyboardKeyF13"));
        }
        self.call(
            self.client.post(&url).json(&json!({ "keys": keys })),
            "paste",
        )
        .await
        .map(|_| ())
    }

    /// Select all of the focused field's text (Cmd+A), so the next key
    /// replaces it. `prime` as in [`Self::paste`].
    pub async fn select_all(&self, prime: bool) -> Result<()> {
        let url = format!("{}/wda/element/0/keyboardInput", self.session_url()?);
        let mut keys = vec![json!({ "key": "a", "modifierFlags": 16 })];
        if prime {
            keys.insert(0, json!("XCUIKeyboardKeyF13"));
        }
        self.call(
            self.client.post(&url).json(&json!({ "keys": keys })),
            "select all",
        )
        .await
        .map(|_| ())
    }

    /// Put `text` in the focused field through the pasteboard, then empty
    /// the pasteboard (the simulator's is shared with the Mac). Reading it
    /// first to restore it isn't possible: WDA brings itself to the front
    /// to read (~2.5 s), and the field loses focus.
    pub async fn paste_text(&self, text: &str, prime: bool) -> Result<()> {
        self.set_pasteboard(text).await?;
        let pasted = self.paste(prime).await;
        let _ = self.set_pasteboard("").await;
        pasted
    }

    /// Press a hardware button (e.g. "home", "volumeUp").
    pub async fn press_button(&self, button: &str) -> Result<()> {
        let url = format!("{}/wda/pressButton", self.session_url()?);
        debug!(button, "WDA press button");
        self.call(
            self.client.post(&url).json(&json!({ "name": button })),
            "press button",
        )
        .await
        .map(|_| ())
    }

    /// Launch (or bring to the foreground) an app through the session.
    pub async fn launch_app(&self, bundle_id: &str) -> Result<()> {
        let url = format!("{}/wda/apps/launch", self.session_url()?);
        // No WDA idle wait before returning (DV-2, as in `create_session`).
        let mut body = json!({ "bundleId": bundle_id, "shouldWaitForQuiescence": false });
        if let Some(env) = self.app_env_json() {
            body["environment"] = env;
        }
        if !self.app_args.is_empty() {
            body["arguments"] = json!(self.app_args);
        }
        self.call(self.client.post(&url).json(&body), "launch app")
            .await
            .map(|_| ())
    }

    /// Terminate an app through the session (real devices have no simctl).
    pub async fn terminate_app(&self, bundle_id: &str) -> Result<()> {
        let url = format!("{}/wda/apps/terminate", self.session_url()?);
        self.call(
            self.client
                .post(&url)
                .json(&json!({ "bundleId": bundle_id })),
            "terminate app",
        )
        .await
        .map(|_| ())
    }

    /// Open a URL on the device (deep links on real devices).
    pub async fn open_url(&self, url: &str) -> Result<()> {
        let endpoint = format!("{}/url", self.session_url()?);
        self.call(
            self.client.post(&endpoint).json(&json!({ "url": url })),
            "open url",
        )
        .await
        .map(|_| ())
    }

    /// Check if WDA is healthy and responsive.
    pub async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/status", self.base_url);
        match self
            .client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(r) => Ok(r.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// The session WDA currently holds (WDA keeps one at a time), from
    /// `/status`. A WDA left running by an earlier run still has its session.
    pub async fn active_session_id(&self) -> Option<String> {
        let url = format!("{}/status", self.base_url);
        let reply = self
            .send(
                self.client.get(&url).timeout(Duration::from_secs(3)),
                "status",
            )
            .await
            .ok()?;
        if !reply.ok() {
            return None;
        }
        reply
            .body
            .get("sessionId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    /// Re-apply Atalanta's settings, pointing queries at `bundle_id`.
    pub async fn retarget(&self, bundle_id: &str) -> Result<()> {
        self.apply_settings(bundle_id).await
    }

    /// Whether the current session still exists on the server (WDA
    /// restarts drop every session).
    pub async fn session_alive(&self) -> bool {
        let Ok(url) = self.session_url() else {
            return false;
        };
        self.send(
            self.client.get(&url).timeout(Duration::from_secs(3)),
            "session",
        )
        .await
        .is_ok_and(|r| r.ok())
    }
}

/// A JSON answer from WDA, any status.
struct Reply {
    status: u16,
    body: Value,
}

/// Who draws at a point, and what is there (D-38).
#[derive(Debug, Clone, PartialEq)]
pub struct HitTest {
    /// The process that owns the point.
    pub owner: HitOwner,
    /// The deepest node of the owner's snapshot holding the point
    /// (pass-through nodes skipped, where the runner can tell).
    pub element: Option<HitElement>,
    /// The element XCTest's own hit test returned, as found in the owner's
    /// snapshot (its path and chain from the root); `None` when the runner
    /// couldn't place it.
    pub ax: Option<HitElement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitOwner {
    /// The app in front.
    App,
    /// SpringBoard: the status bar, a system alert, the home screen.
    SpringBoard,
    /// Some other process (a remote view, a share sheet).
    Other,
}

/// A node the hit test reports.
pub type HitElement = crate::hit::HitNode;

/// One reported node: `{type, label, identifier, x, y, width, height,
/// path?, chain?, accessible?, interactive?, hittable?}`.
fn hit_node(e: &Value) -> Option<HitElement> {
    if !e.is_object() {
        return None;
    }
    let n = |k: &str| e[k].as_i64().unwrap_or(0) as i32;
    Some(HitElement {
        element_type: e["type"].as_str().unwrap_or("Other").to_string(),
        label: e["label"].as_str().unwrap_or("").to_string(),
        identifier: e["identifier"].as_str().unwrap_or("").to_string(),
        bounds: atalanta_common::Rect {
            x: n("x"),
            y: n("y"),
            width: n("width"),
            height: n("height"),
        },
        path: e["path"].as_array().and_then(|a| {
            a.iter()
                .map(|i| i.as_u64().map(|i| i as usize))
                .collect::<Option<Vec<_>>>()
        }),
        chain: e["chain"]
            .as_array()
            .map(|a| a.iter().filter_map(hit_node).collect())
            .unwrap_or_default(),
        accessible: e["accessible"].as_bool(),
        interactive: e["interactive"].as_bool(),
        hittable: e["hittable"].as_bool(),
    })
}

impl HitTest {
    /// The runner's `value`; `None` for `available: false`.
    fn from_value(v: &Value) -> Option<Self> {
        if v["available"].as_bool() != Some(true) {
            debug!(reason = %v["reason"], "hit test unavailable");
            return None;
        }
        let owner = match v["owner"].as_str() {
            Some("app") => HitOwner::App,
            Some("springboard") => HitOwner::SpringBoard,
            _ => HitOwner::Other,
        };
        Some(Self {
            owner,
            element: hit_node(&v["element"]),
            ax: hit_node(&v["hit"]),
        })
    }
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The W3C error code (`value.error`), "" when there is none.
    fn code(&self) -> &str {
        self.body["value"]["error"].as_str().unwrap_or("")
    }
}

/// Whether a request changes nothing on the device, so a copy of it that
/// may have arrived can be sent again: reads, element lookups and settings.
fn replay_safe(method: &reqwest::Method, path: &str) -> bool {
    *method == reqwest::Method::GET
        || (*method == reqwest::Method::POST
            && (path.ends_with("/element")
                || path.ends_with("/elements")
                || path.ends_with("/appium/settings")
                || path.ends_with("/atalanta/hitTest")))
}

/// The first 200 bytes of a body, for an error message.
fn snippet(bytes: &[u8]) -> String {
    let cut = bytes.len().min(200);
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    if bytes.len() > cut {
        s.push('…');
    }
    if s.is_empty() {
        s.push_str("(empty body)");
    }
    s
}

/// An error with its causes ("error sending request: connection reset").
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

/// Text as the per-character array WDA expects.
fn keys(text: &str) -> Vec<String> {
    text.chars().map(|c| c.to_string()).collect()
}

/// W3C pointer actions for `count` taps at (x, y). A 50 ms press is as fast
/// as no pause (measured 360 vs 369 ms) and reads as a tap to every
/// recognizer, including React Native's touchables.
fn tap_actions(x: f64, y: f64, count: usize) -> Value {
    let mut actions = vec![json!({ "type": "pointerMove", "duration": 0, "x": x, "y": y })];
    for i in 0..count.max(1) {
        if i > 0 {
            actions.push(json!({ "type": "pause", "duration": 60 }));
        }
        actions.push(json!({ "type": "pointerDown", "button": 0 }));
        actions.push(json!({ "type": "pause", "duration": 50 }));
        actions.push(json!({ "type": "pointerUp", "button": 0 }));
    }
    json!({
        "actions": [{
            "type": "pointer",
            "id": "finger1",
            "parameters": { "pointerType": "touch" },
            "actions": actions,
        }]
    })
}

/// Extract the element ID from a WDA element response value.
/// WDA returns element IDs under a key like "ELEMENT" or "element-6066-...".
fn extract_element_id(value: &Value) -> std::result::Result<String, String> {
    // W3C WebDriver uses the key "element-6066-11e4-a52e-4f735466cecf"
    const W3C_ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

    if let Some(id) = value.get(W3C_ELEMENT_KEY).and_then(|v| v.as_str()) {
        return Ok(id.to_string());
    }

    // Legacy JSONWP uses "ELEMENT"
    if let Some(id) = value.get("ELEMENT").and_then(|v| v.as_str()) {
        return Ok(id.to_string());
    }

    // Try any string value in the object
    if let Some(obj) = value.as_object() {
        for (_, v) in obj {
            if let Some(id) = v.as_str() {
                return Ok(id.to_string());
            }
        }
    }

    Err(format!(
        "Could not extract element ID from WDA response: {value}"
    ))
}

/// The bundle id in WDA's "application under test with bundle id 'X' is not
/// running, possibly crashed" error. As a plain command failure it was
/// retried, opened the circuit breaker in ~200 ms and then waited out the
/// step timeout, so a crash read as an unresponsive device.
fn crashed_app(detail: &str) -> Option<String> {
    if !detail.contains("is not running") {
        return None;
    }
    let rest = detail.split("bundle id '").nth(1)?;
    Some(rest.split('\'').next()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(t: &str) -> RunnerAuth {
        RunnerAuth {
            token: Some(t.into()),
            host: None,
        }
    }

    /// Review 09 #9: a tree read the other way round drops the cached
    /// window size; the same orientation (or an empty frame) keeps it.
    #[test]
    fn a_rotated_app_frame_drops_the_cached_window_size() {
        let c = WdaClient::new("http://127.0.0.1:1");
        let cached = || *c.screen_size.lock().unwrap();
        *c.screen_size.lock().unwrap() = Some((402, 874));
        c.note_app_frame(402, 874);
        c.note_app_frame(0, 0);
        assert_eq!(cached(), Some((402, 874)));
        c.note_app_frame(874, 402);
        assert_eq!(cached(), None);
    }

    /// wifi-runner: the token goes out as a sensitive bearer header.
    #[test]
    fn the_runner_token_is_a_sensitive_bearer_header() {
        let h = runner_headers(&token("0123456789abcdef"));
        let v = &h[reqwest::header::AUTHORIZATION];
        assert_eq!(v.to_str().unwrap(), "Bearer 0123456789abcdef");
        assert!(v.is_sensitive());
        assert!(runner_headers(&RunnerAuth::default()).is_empty());
        assert!(runner_headers(&token("bad\nvalue")).is_empty());
    }

    /// R-1: through a usbmux forward the runner checks `Host` against its
    /// own port, not the forward's: the override reaches the wire, with
    /// the token, and `set_auth` swaps both.
    #[tokio::test]
    async fn the_host_override_and_token_reach_the_runner() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = heads.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase());
                let body = r#"{"value":{"ready":true}}"#;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        let mut c = WdaClient::new(&format!("http://127.0.0.1:{port}"));
        c.set_auth(RunnerAuth {
            token: Some("t0ken-0123456789".into()),
            host: Some("127.0.0.1:8100".into()),
        });
        assert!(c.health_check().await.unwrap());
        c.set_auth(token("other-0123456789"));
        assert!(c.health_check().await.unwrap());
        let heads = heads.lock().unwrap().clone();
        assert!(
            heads[0].contains("\r\nhost: 127.0.0.1:8100\r\n"),
            "{}",
            heads[0]
        );
        assert!(heads[0].contains("authorization: bearer t0ken-0123456789"));
        assert!(
            heads[1].contains(&format!("\r\nhost: 127.0.0.1:{port}\r\n")),
            "{}",
            heads[1]
        );
        assert!(heads[1].contains("authorization: bearer other-0123456789"));
    }

    #[test]
    fn a_crash_message_names_the_app() {
        let msg = "The application under test with bundle id 'com.atalanta.lab' is not running, possibly crashed";
        assert_eq!(crashed_app(msg).as_deref(), Some("com.atalanta.lab"));
        assert_eq!(crashed_app("Element not found"), None);
    }

    #[test]
    fn test_extract_element_id_w3c() {
        let value = json!({
            "element-6066-11e4-a52e-4f735466cecf": "abc-123"
        });
        assert_eq!(extract_element_id(&value).unwrap(), "abc-123");
    }

    #[test]
    fn test_extract_element_id_legacy() {
        let value = json!({ "ELEMENT": "def-456" });
        assert_eq!(extract_element_id(&value).unwrap(), "def-456");
    }

    #[test]
    fn test_extract_element_id_missing() {
        let value = json!({});
        assert!(extract_element_id(&value).is_err());
    }

    /// Review 01 #9: the crash watcher reads the session through this
    /// handle, never the device's request lock.
    #[test]
    fn the_published_session_follows_the_client() {
        let mut client = WdaClient::new("http://127.0.0.1:1");
        let seen = client.published();
        assert_eq!(seen.get().0, None);
        client.set_session_id("s1".into());
        assert_eq!(seen.get().0.as_deref(), Some("s1"));
        client.set_auth(RunnerAuth {
            token: Some("t".into()),
            host: None,
        });
        assert_eq!(seen.get().1.token.as_deref(), Some("t"));
        client.set_session_id_opt(None);
        assert_eq!(seen.get().0, None);
    }

    #[test]
    fn test_wda_client_no_session() {
        let client = WdaClient::new("http://localhost:8100");
        assert!(client.session_id().is_none());
        assert!(client.session_url().is_err());
    }

    #[test]
    fn test_wda_client_session_url() {
        let mut client = WdaClient::new("http://localhost:8100");
        client.set_session_id("test-session-123".to_string());
        assert_eq!(
            client.session_url().unwrap(),
            "http://localhost:8100/session/test-session-123"
        );
    }

    #[test]
    fn test_wda_client_trailing_slash() {
        let client = WdaClient::new("http://localhost:8100/");
        let mut client = client;
        client.set_session_id("s1".to_string());
        assert_eq!(
            client.session_url().unwrap(),
            "http://localhost:8100/session/s1"
        );
    }

    #[test]
    fn double_tap_is_one_sequence_with_two_presses() {
        let v = tap_actions(10.0, 20.0, 2);
        let steps = v["actions"][0]["actions"].as_array().unwrap();
        let downs = steps.iter().filter(|s| s["type"] == "pointerDown").count();
        assert_eq!(downs, 2);
        assert_eq!(steps[0]["type"], "pointerMove");
        assert_eq!(steps[0]["x"], 10.0);
        assert_eq!(v["actions"][0]["parameters"]["pointerType"], "touch");
    }

    // DV-2: WDA waited for the app to go quiet inside the launch request
    // (4.4 s for Settings on an iOS 26 simulator); Atalanta settles itself.
    #[tokio::test]
    async fn a_launch_skips_wdas_idle_wait() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers, then the body Content-Length announces.
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            let ok = r#"{"value":null,"sessionId":"s"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{ok}",
                ok.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&req).to_string()
        });
        let mut client = WdaClient::new(&url);
        client.set_session_id("s".into());
        client.launch_app("com.example.app").await.unwrap();
        let req = server.await.unwrap();
        let body: Value = serde_json::from_str(req.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["bundleId"], "com.example.app");
        assert_eq!(body["shouldWaitForQuiescence"], false, "{body}");
    }

    #[tokio::test]
    async fn a_refused_connection_is_wda_unreachable() {
        // Port 9 (discard) is closed on any dev machine.
        let mut client = WdaClient::new("http://127.0.0.1:9");
        client.set_session_id("s".into());
        let err = client.get_source().await.unwrap_err();
        assert!(
            matches!(err, AtalantaError::WdaUnreachable { .. }),
            "{err:?}"
        );
        assert_eq!(err.kind(), atalanta_common::ErrorKind::ConnectionLost);
    }
}

#[cfg(test)]
mod second_pass_tests {
    use super::*;

    async fn serve(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// A window size WDA writes with a fractional part, or leaves out, is
    /// not an iPhone 14 (390x844).
    #[tokio::test]
    async fn second_pass_screen_size_float_is_not_defaulted() {
        let url = serve(r#"{"value":{"width":393.0,"height":852.0}}"#).await;
        let mut c = WdaClient::new(&url);
        c.set_session_id("s".into());
        let size = c.get_screen_size().await;
        assert_eq!(size.ok(), Some((393, 852)), "fell back to 390x844");
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn the_deadline_defaults_on_only_for_a_v16_14_1_or_later_wda() {
        // An explicit value is read from the environment: none here.
        assert_eq!(accessibility_deadline_ms(None), 0, "unknown WDA");
        assert_eq!(accessibility_deadline_ms(Some("v16.12.10")), 0);
        assert_eq!(accessibility_deadline_ms(Some("master")), 0);
        for r in ["v16.14.1", "v16.15.0", "v17.0.0"] {
            assert_eq!(
                accessibility_deadline_ms(Some(r)),
                DEFAULT_ACCESSIBILITY_DEADLINE_MS,
                "{r}"
            );
        }
        // Per client: one device's interop WDA says nothing of another's.
        let mut a = WdaClient::new("http://127.0.0.1:9");
        let b = WdaClient::new("http://127.0.0.1:9");
        a.set_wda_ref(Some("v16.14.1"));
        assert_eq!(accessibility_deadline_ms(a.wda_ref()), 5000);
        assert_eq!(accessibility_deadline_ms(b.wda_ref()), 0);
    }
}
