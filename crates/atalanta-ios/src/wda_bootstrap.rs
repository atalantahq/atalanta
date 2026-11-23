//! WebDriverAgent lifecycle: pinned source, cached build, launch, teardown.
//!
//! - **Pinned, never pulled.** WDA `master` moves fast (13 releases in three
//!   weeks of Sept 2026) and v16.12.11+ adds `lib_TestingInterop.dylib`, which
//!   Xcode 26.0.x simulators don't ship, so tracking `master` broke iOS here.
//!   The ref is picked from what the installed Xcode supports
//!   (`pick_wda_ref`) and can be overridden with `VELOCITY_WDA_REF`.
//! - **Built once per (ref, Xcode build, arch)** into `~/.velocity/wda/build/…`
//!   for the generic simulator destination, so the `.xctestrun` serves every
//!   simulator of that SDK. A later run only launches it (~17 s instead of a
//!   31 s build + 4 s `git pull` + launch).
//! - **One WDA per simulator**, each on its own port ([`pick_port`]). A WDA
//!   already listening is reused only if it belongs to the same simulator
//!   ([`port_owner`]); with `VELOCITY_KEEP_WDA=1` it is left running at exit
//!   so the next run reattaches instead of paying ~17 s to launch it.
//! - **Pipes are always drained.** xcodebuild logs constantly; an undrained
//!   pipe fills (~64 KB) and blocks it, which freezes WDA mid-run. Output goes
//!   to `~/.velocity/wda/logs/<device>.log`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use velocity_common::{Result, VelocityError};

const WDA_REPO: &str = "https://github.com/appium/WebDriverAgent.git";
const WDA_PORT: u16 = 8100;
const WDA_READY_TIMEOUT: Duration = Duration::from_secs(120);
const WDA_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Last WDA tag that builds without `lib_TestingInterop.dylib` (verified on
/// Xcode 26.0.1, 2026-10-06).
const WDA_REF_LEGACY: &str = "v16.12.10";
/// Tag used when the simulator platform ships `lib_TestingInterop.dylib`
/// (newer Xcode 26.x). D-37: v16.14.1 or newer, so the session can set
/// `accessibilityDeadline` and `snapshotMaxDepth` (see
/// `WdaClient::apply_settings`). Not yet verified on such an Xcode (the
/// xcodebuild matrix needs the hardware).
const WDA_REF_TESTING_INTEROP: &str = "v16.14.1";

/// The commit each pinned tag points at (GitHub `git/refs/tags/<tag>`,
/// looked up 2026-10-16). A clone of a pinned tag must check out exactly
/// this commit: a tag moved upstream (or a tampered mirror) fails the
/// bootstrap instead of building unknown code. WDA is cloned with git, not
/// downloaded as an archive, so the commit id is the checksum. A tag set
/// with `VELOCITY_WDA_REF` that isn't listed here is the user's choice and
/// isn't checked.
const WDA_PINNED_COMMITS: &[(&str, &str)] = &[
    (WDA_REF_LEGACY, "00c38220c3e84906c965b996ffc4c12d09fef62f"),
    (
        WDA_REF_TESTING_INTEROP,
        "2e98ac0ad4dbc99ab7c8bb01b7216fde252afb54",
    ),
];

/// The commit `wda_ref` must resolve to, when it is one of the pins.
fn pinned_commit(wda_ref: &str) -> Option<&'static str> {
    WDA_PINNED_COMMITS
        .iter()
        .find(|(tag, _)| *tag == wda_ref)
        .map(|(_, sha)| *sha)
}

/// Check a clone of `wda_ref` checked out `head` (`git rev-parse HEAD`).
fn check_pinned_commit(wda_ref: &str, head: &str) -> Result<()> {
    match pinned_commit(wda_ref) {
        Some(want) if !head.trim().eq_ignore_ascii_case(want) => Err(config(format!(
            "WebDriverAgent {wda_ref} resolved to commit {} but Velocity pins it to {want}: \
             the tag moved upstream or the clone was tampered with. Refusing to build it \
             (set VELOCITY_WDA_REF to another tag to override)",
            head.trim()
        ))),
        _ => Ok(()),
    }
}

/// The file next to a cached build naming what it was built from:
/// `<ref> <commit>` (the commit empty when git couldn't say).
const BUILT_FROM: &str = "velocity-wda-ref";

/// `git rev-parse HEAD` in `dir`, "" when git can't say.
async fn git_head(dir: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether a cached clone of `wda_ref` may be reused: an unpinned ref
/// always; a pinned one only when it is checked out at the pinned commit.
/// A mismatching clone (made before the pins, or replaced locally) is
/// removed so it is cloned again (review MINOR: only fresh clones were
/// checked).
async fn reuse_cached_source(dir: &Path, wda_ref: &str) -> bool {
    if pinned_commit(wda_ref).is_none() {
        return true;
    }
    let head = git_head(dir).await;
    match check_pinned_commit(wda_ref, &head) {
        Ok(()) => true,
        Err(e) => {
            warn!(
                "cached WebDriverAgent source at {}: {e}; cloning it again",
                dir.display()
            );
            let _ = tokio::fs::remove_dir_all(dir).await;
            false
        }
    }
}

/// What a cached build says it was built from (`BUILT_FROM`): (ref, commit).
fn built_from(derived: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(derived.join(BUILT_FROM)).ok()?;
    let mut parts = text.split_whitespace();
    let r = parts.next()?.to_string();
    Some((r, parts.next().unwrap_or("").to_string()))
}

/// `BUILT_FROM`'s third word: the build's MJPEG stream listens where `USE_IP`
/// says ([`patch_mjpeg_bind`] applied).
const MJPEG_USE_IP: &str = "mjpeg-use-ip";
/// The third word when the patch didn't apply (a ref without the line it
/// patches): the stream listens on every interface, as upstream's does.
const MJPEG_ALL: &str = "mjpeg-all-interfaces";

/// What `BUILT_FROM` says about the MJPEG stream (its third word).
fn built_mjpeg(derived: &Path) -> Option<String> {
    let text = std::fs::read_to_string(derived.join(BUILT_FROM)).ok()?;
    text.split_whitespace().nth(2).map(String::from)
}

/// Whether a cached build of `wda_ref` may be reused: an unpinned ref
/// always; a pinned one only when `BUILT_FROM` names the pinned commit. A
/// build from before the MJPEG patch (no third word) is rebuilt once.
fn build_matches_pin(derived: &Path, wda_ref: &str) -> bool {
    let pinned = match pinned_commit(wda_ref) {
        None => true,
        Some(want) => {
            built_from(derived).is_some_and(|(_, commit)| commit.eq_ignore_ascii_case(want))
        }
    };
    pinned && built_mjpeg(derived).is_some()
}

/// The line in WDA's `FBWebServer.m` that creates the MJPEG listener.
const MJPEG_LISTENER: &str =
    "initWithPort:(uint16_t)FBConfiguration.sharedInstance.mjpegServerPort];";
const MJPEG_BIND: &str =
    "\n  // Velocity: listen where USE_IP says (loopback on a simulator), as the HTTP server does.\n  self.screenshotsBroadcaster.interface = FBConfiguration.sharedInstance.bindingIPAddress;";

/// WDA's MJPEG screen stream (`MJPEG_SERVER_PORT`) ignores `USE_IP` and
/// listens on every interface, unauthenticated: on a simulator, which
/// shares the Mac's network, anyone on the LAN could watch the screen (the
/// HTTP server already honours `USE_IP`, set to loopback for simulators).
/// Upstream has no setting for it, so the clone is patched to give the
/// listener the same interface before it is built. `true` when the source
/// has the patch (now or from before); `false` when the line isn't there
/// (another WDA version), and the stream keeps listening everywhere.
fn patch_mjpeg_bind(src: &Path) -> bool {
    let file = src.join("WebDriverAgentLib/Routing/FBWebServer.m");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return false;
    };
    if text.contains("self.screenshotsBroadcaster.interface =") {
        return true;
    }
    let Some(at) = text.find(MJPEG_LISTENER) else {
        warn!(
            "{}: no MJPEG listener line to patch; the iOS screen stream will listen on every interface",
            file.display()
        );
        return false;
    };
    let end = at + MJPEG_LISTENER.len();
    let patched = format!("{}{MJPEG_BIND}{}", &text[..end], &text[end..]);
    std::fs::write(&file, patched).is_ok()
}

/// The build dir an `.xctestrun` came from (`<derived>/Build/Products/x`).
fn derived_of(xctestrun: &Path) -> Option<&Path> {
    xctestrun.parent()?.parent()?.parent()
}

/// Manages the full WDA lifecycle: download, build, launch, health-check, teardown.
pub struct WdaBootstrap {
    cache_dir: PathBuf,
    wda_process: Option<Child>,
    /// The simulator and runner `wda_process` runs, to stop its app too.
    launched: Option<(String, RunnerKind)>,
    port: u16,
    /// A real device: WDA is built signed for `iphoneos` and reached through
    /// a usbmux forward from `port` to the device's [`DEVICE_WDA_PORT`].
    physical: bool,
    forward: Option<crate::usbmux::Forward>,
    /// A token in place of `ios.runnerToken` (tests); `None` reads the
    /// setting at each use.
    token: Option<Option<String>>,
    /// R-1: the random token this device's runner was launched with, when
    /// `ios.runnerToken` isn't set. Kept in [`Self::token_file`] (0600) so
    /// a later Velocity process can reattach to the same runner.
    session_token: Option<String>,
    /// When launches on this device failed, and why the last one did: past
    /// [`CRASH_LOOP_MAX`] within [`CRASH_LOOP_WINDOW`] no more are tried
    /// (a WDA that can't bind its port aborts at once, and was relaunched
    /// every few seconds).
    launch_failures: Vec<std::time::Instant>,
    last_failure: Option<String>,
    /// The WebDriverAgent ref the runner on this device was built from,
    /// when known: set by a bootstrap, and on reattach read from the
    /// marker the launching run left (`VELOCITY_KEEP_WDA`). `None`: not
    /// WebDriverAgent (VelocityRunner) or one Velocity didn't start.
    wda_ref: Option<String>,
}

/// Port WDA listens on inside a real device.
const DEVICE_WDA_PORT: u16 = 8100;

impl Default for WdaBootstrap {
    fn default() -> Self {
        Self::new()
    }
}

impl WdaBootstrap {
    pub fn new() -> Self {
        let cache_dir = dirs_home().join(".velocity").join("wda");
        Self {
            cache_dir,
            wda_process: None,
            launched: None,
            port: WDA_PORT,
            physical: false,
            forward: None,
            token: None,
            session_token: None,
            launch_failures: Vec::new(),
            last_failure: None,
            wda_ref: None,
        }
    }

    /// The WebDriverAgent ref the runner on this device is known to be
    /// built from (see the field).
    pub fn wda_ref(&self) -> Option<&str> {
        self.wda_ref.as_deref()
    }

    /// The marker a launch leaves for `device_id`: the `.xctestrun` it ran,
    /// whose build dir holds `BUILT_FROM`.
    fn launch_marker(&self, device_id: &str) -> PathBuf {
        self.cache_dir
            .join("running")
            .join(format!("{}-{}", sanitize(device_id), self.port))
    }

    /// The ref of the build the marker for `device_id` names.
    fn marked_ref(&self, device_id: &str) -> Option<String> {
        let xctestrun = std::fs::read_to_string(self.launch_marker(device_id)).ok()?;
        let derived = derived_of(Path::new(xctestrun.trim()))?;
        built_from(derived).map(|(r, _)| r)
    }

    fn forget_launch_marker(&self, device_id: &str) {
        let _ = std::fs::remove_file(self.launch_marker(device_id));
    }

    /// Where the per-launch token of `device_id`'s runner is kept: one
    /// runner per device, whatever port a later process forwards from.
    fn token_file(&self, device_id: &str) -> PathBuf {
        self.cache_dir
            .join("running")
            .join(format!("{}.token", sanitize(device_id)))
    }

    /// A fresh per-launch token for `device_id`'s runner, written to its
    /// token file (owner-only) before the runner starts.
    fn new_session_token(&mut self, device_id: &str) -> Result<String> {
        let token = random_token()?;
        let path = self.token_file(device_id);
        write_private(&path, &token)
            .map_err(|e| config(format!("write runner token {}: {e}", path.display())))?;
        velocity_common::redact::register_secret(&token);
        self.session_token = Some(token.clone());
        Ok(token)
    }

    /// The token a runner already on `device_id` was launched with, from
    /// its token file (another process, or a kept runner).
    fn load_session_token(&mut self, device_id: &str) {
        if self.session_token.is_some() {
            return;
        }
        let token = std::fs::read_to_string(self.token_file(device_id))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        if let Some(t) = &token {
            velocity_common::redact::register_secret(t);
        }
        self.session_token = token;
    }

    fn forget_session_token(&mut self, device_id: &str) {
        let _ = std::fs::remove_file(self.token_file(device_id));
        self.session_token = None;
    }

    /// What every request to this device's runner carries: the token, and
    /// on a real device the runner's own `Host` (the usbmux forward's
    /// local port is not the one the runner checks).
    pub fn auth(&self) -> crate::wda::RunnerAuth {
        crate::wda::RunnerAuth {
            token: self.token(),
            host: self
                .physical
                .then(|| format!("127.0.0.1:{DEVICE_WDA_PORT}")),
        }
    }

    /// Cache WDA under `dir` instead of `~/.velocity/wda` (tests).
    #[cfg(test)]
    fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    /// Send `token` (not `ios.runnerToken`) on this bootstrap's probes.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = Some(token);
        self
    }

    /// The runner's token: `ios.runnerToken` when set, else the random
    /// one this device's runner was launched with (R-1). Sent as a bearer
    /// token on every probe this bootstrap makes (`/status` readiness,
    /// reattach, stop): VelocityRunner answers 401 to anything without it.
    fn token(&self) -> Option<String> {
        self.token
            .clone()
            .unwrap_or_else(|| runner_token().or_else(|| self.session_token.clone()))
    }

    /// A probe request to the runner, carrying the token and `Host` every
    /// request needs (wifi-runner, review BLOCKER 1; R-1).
    fn probe_get(&self, url: &str) -> reqwest::RequestBuilder {
        crate::wda::runner_client()
            .build()
            .unwrap_or_default()
            .get(url)
            .headers(crate::wda::runner_headers(&self.auth()))
            .timeout(Duration::from_secs(3))
    }

    /// Whether anything answers HTTP on the port, even a refusal (a
    /// runner started with a token this process doesn't have).
    async fn answers(&self) -> bool {
        self.probe_get(&format!("{}/status", self.base_url()))
            .send()
            .await
            .is_ok()
    }

    /// Whether a runner already answering on a real device may be reused.
    /// A cabled (or unlisted) phone: yes, as before. A Wi-Fi-only one:
    /// only under [`wifi_policy`], and only when a bare `/status` (no
    /// bearer header) is refused with 401, proof the runner was started
    /// with a token and checks it.
    async fn check_reattach(
        &self,
        udid: &str,
        usb: Option<bool>,
        allow: bool,
        velocity_runner: bool,
    ) -> Result<()> {
        if usb != Some(false) {
            return Ok(());
        }
        wifi_policy(udid, usb, allow, self.token().as_deref(), velocity_runner)?;
        let bare = crate::wda::runner_client()
            .build()
            .unwrap_or_default()
            .get(format!("{}/status", self.base_url()))
            .headers(crate::wda::runner_headers(&crate::wda::RunnerAuth {
                token: None,
                ..self.auth()
            }))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .ok();
        if bare == Some(401) {
            return Ok(());
        }
        Err(config(format!(
            "iPhone {udid} is connected over Wi-Fi only, and the runner already on it \
             answers without the bearer token (it was started without ios.runnerToken, \
             e.g. while cabled with VELOCITY_KEEP_WDA): refusing to reuse it. Connect \
             the cable, or stop the runner on the phone so it restarts with the token"
        )))
    }

    /// Target a real device instead of a simulator.
    pub fn physical(mut self, on: bool) -> Self {
        self.physical = on;
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the runner on a real device answers on its own port (the
    /// forward's listener accepts either way); `None` off a device.
    pub async fn probe(&self) -> Option<bool> {
        let forward = self.forward.as_ref()?;
        Some(forward.probe().await == crate::usbmux::Liveness::Alive)
    }

    /// Check if WDA is already running and responsive on the expected port.
    pub async fn is_running(&self) -> bool {
        let url = format!("{}/status", self.base_url());
        match self.probe_get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Which runner answers on the port, if any: VelocityRunner says so in
    /// its `/status` (`build.version` = `velocity-runner-…`).
    ///
    /// `Some((kind, version, false))` when the port answers 200 but not as a
    /// runner: something else holds it (VEL-S30).
    async fn running_kind(&self) -> Option<(RunnerKind, String, bool)> {
        let url = format!("{}/status", self.base_url());
        let resp = self.probe_get(&url).send().await.ok()?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Only VelocityRunner checks a token: one started with a token
            // this process doesn't have (its token file is gone) can't be
            // used, only replaced (R-1). No version, so it never matches.
            return Some((RunnerKind::Velocity, String::new(), true));
        }
        let resp = Some(resp).filter(|r| r.status().is_success())?;
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let version = body["value"]["build"]["version"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Some((
            RunnerKind::from_status(&body),
            version,
            is_runner_status(&body),
        ))
    }

    /// Terminate `kind`'s runner app on simulator `device_id` and wait for
    /// the port to free up.
    async fn stop_runner(&self, device_id: &str, kind: RunnerKind) {
        self.stop_runner_via(
            &crate::simctl::Simctl::new(),
            stop_timeout(),
            device_id,
            kind,
        )
        .await
    }

    /// [`Self::stop_runner`] through `simctl` (tests pass a stub rather than
    /// set `VELOCITY_XCRUN_PATH` while other tests read the env).
    async fn stop_runner_via(
        &self,
        simctl: &crate::simctl::Simctl,
        timeout: Duration,
        device_id: &str,
        kind: RunnerKind,
    ) {
        // Bounded, and through `VELOCITY_XCRUN_PATH` like every other simctl
        // call: a hung simctl kept the run from ever exiting (VEL-350).
        if let Err(e) = simctl
            .terminate_within(device_id, kind.bundle_id(), timeout)
            .await
        {
            warn!(device_id, "stopping {}: {e}", kind.name());
        }
        for _ in 0..50 {
            if !self.answers().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        warn!(
            "{} still answering on port {} after terminate",
            kind.name(),
            self.port
        );
    }

    /// Ensure WDA is running. If already running, returns immediately.
    /// Otherwise fetches the pinned source and builds it (each only once per
    /// machine and Xcode), then launches it.
    pub async fn ensure_running(&mut self, device_id: &str) -> Result<()> {
        self.load_session_token(device_id);
        if self.physical && self.forward.is_none() {
            self.forward =
                Some(crate::usbmux::forward(device_id, self.port, DEVICE_WDA_PORT).await?);
        }
        if self.physical && self.is_running().await {
            // review-fixes #2: a runner kept from a cabled run
            // (`VELOCITY_KEEP_WDA`) may have started without a token; once
            // the phone is reachable only over Wi-Fi it must pass the same
            // policy as a fresh launch, and prove it checks the token.
            let usb = crate::usbmux::connection(device_id).await;
            self.check_reattach(device_id, usb, allow_wifi_runner(), velocity_runner())
                .await?;
            self.wda_ref = if velocity_runner() {
                None
            } else {
                self.marked_ref(device_id)
            };
            info!(
                "WDA already running on the device (forwarded to port {})",
                self.port
            );
            return Ok(());
        }
        if let Some((running, version, runner_like)) = self.running_kind().await {
            // A VelocityRunner from older source keeps answering until stopped.
            let version_ok = running == RunnerKind::Wda || version == RunnerKind::current_build();
            let owner = port_owner(self.port).await;
            if !runner_like && owner.is_none() {
                // Any HTTP 200 used to pass as WebDriverAgent, and every
                // command then failed against the wrong server (VEL-S30).
                return Err(config(format!(
                    "port {} answers HTTP but not as WebDriverAgent or VelocityRunner: \
                     another program holds it; stop it or set VELOCITY_WDA_URL",
                    self.port
                )));
            }
            match owner {
                Some(owner) if owner != device_id => {
                    return Err(config(format!(
                        "port {} is serving WebDriverAgent for simulator {owner}, not {device_id}",
                        self.port
                    )));
                }
                _ if running == RunnerKind::requested() && version_ok => {
                    info!("{} already running on port {}", running.name(), self.port);
                    // Reattached: the ref is what the launching run
                    // recorded, if a Velocity run launched it.
                    self.wda_ref = (running == RunnerKind::Wda)
                        .then(|| self.marked_ref(device_id))
                        .flatten();
                    return Ok(());
                }
                _ => {
                    // The other runner holds the port: `VELOCITY_IOS_RUNNER`
                    // picks the runner, so stop it and start the right one.
                    info!(
                        "{} ({version}) is running on port {}; stopping it for {} ({})",
                        running.name(),
                        self.port,
                        RunnerKind::requested().name(),
                        RunnerKind::current_build()
                    );
                    // On a real device the relaunch below replaces it.
                    if !self.physical {
                        self.stop_runner(device_id, running).await;
                    }
                }
            }
        }
        self.check_crash_loop(device_id)?;
        let xcode = xcode_info().await?;
        // One process at a time fetches and builds a given runner: two
        // runs bootstrapping at once raced on the same temp and build dirs
        // (VEL-149). The second waits, then finds the cache filled.
        let wda_ref = if velocity_runner() {
            RunnerKind::current_build()
        } else {
            velocity_common::env::var("VELOCITY_WDA_REF")
                .unwrap_or_else(|_| pick_wda_ref(&xcode.developer_dir).to_string())
        };
        if !velocity_runner() && pinned_commit(&wda_ref).is_none() {
            // `ios.wdaRef` can come from a suite's `config.settings`: say
            // that what gets built is not a commit Velocity checked
            // (security review #12).
            warn!(
                wda_ref,
                "WebDriverAgent {wda_ref} is not one of Velocity's pinned tags: its commit is \
                 not checked before it is built and run (unset ios.wdaRef / VELOCITY_WDA_REF \
                 to use the pinned one)"
            );
        }
        let lock = cache_lock(&self.cache_dir, &wda_ref).await?;
        let src = if velocity_runner() {
            info!(runner = %wda_ref, xcode = %xcode.build, "Velocity runner not running — bootstrapping");
            self.ensure_runner_source(&wda_ref)?
        } else {
            info!(wda_ref, xcode = %xcode.build, "WDA not running — bootstrapping");
            self.ensure_source(&wda_ref).await?
        };
        let xctestrun = self.ensure_built(&src, &wda_ref, &xcode).await?;
        drop(lock);
        self.forget_launch_marker(device_id);
        self.wda_ref = None;
        if !self.physical {
            self.free_port(device_id).await?;
        }
        if let Err(e) = self.launch_and_wait_ready(&xctestrun, device_id).await {
            self.launch_failures.push(std::time::Instant::now());
            self.last_failure = Some(e.to_string());
            return Err(e);
        }
        self.launch_failures.clear();
        if !velocity_runner() {
            self.wda_ref = Some(wda_ref.clone());
            let marker = self.launch_marker(device_id);
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&marker, xctestrun.display().to_string());
        }
        info!("WDA is ready on port {}", self.port);
        Ok(())
    }

    /// Refuse another launch after [`CRASH_LOOP_MAX`] failed ones within
    /// [`CRASH_LOOP_WINDOW`]: relaunching a runner that can't come up
    /// (its port taken, a broken build) only crash-loops it.
    fn check_crash_loop(&mut self, device_id: &str) -> Result<()> {
        let now = std::time::Instant::now();
        self.launch_failures
            .retain(|t| now.duration_since(*t) < CRASH_LOOP_WINDOW);
        if self.launch_failures.len() < CRASH_LOOP_MAX {
            return Ok(());
        }
        Err(VelocityError::RunnerFailing {
            device_id: device_id.to_string(),
            detail: format!(
                "it failed to come up {} times within {} s; not relaunching it. Last failure: {}",
                self.launch_failures.len(),
                CRASH_LOOP_WINDOW.as_secs(),
                self.last_failure.as_deref().unwrap_or("unknown")
            ),
        })
    }

    /// Make sure nothing listens on the port before a runner is launched
    /// there: WebDriverAgent aborts when it can't bind, and was relaunched
    /// into the same taken port over and over. A runner of this simulator
    /// (either kind, any build, one this process has no token for) is
    /// stopped; anything else is reported, never killed.
    async fn free_port(&mut self, device_id: &str) -> Result<()> {
        let mut pids = listening_pids(self.port).await;
        if pids.is_empty() {
            return Ok(());
        }
        if owner_of(&pids).await.as_deref() == Some(device_id) {
            for kind in [RunnerKind::Wda, RunnerKind::Velocity] {
                self.stop_runner(device_id, kind).await;
            }
            pids = listening_pids(self.port).await;
            if pids.is_empty() {
                return Ok(());
            }
        }
        let holders = describe_pids(&pids).await;
        Err(VelocityError::RunnerFailing {
            device_id: device_id.to_string(),
            detail: format!(
                "port {} is held by {holders}; stop it, or set VELOCITY_WDA_URL",
                self.port
            ),
        })
    }

    /// Write the embedded Velocity runner into `~/.velocity/wda/src/<name>`.
    ///
    /// Written into a per-process temp dir and renamed into place, and a
    /// cached copy counts only when every file is there: a write cut short
    /// used to leave a project that never built again (VEL-148).
    fn ensure_runner_source(&self, name: &str) -> Result<PathBuf> {
        let dir = self.cache_dir.join("src").join(sanitize(name));
        let complete = |d: &Path| {
            crate::runner_src::FILES
                .iter()
                .all(|(p, _)| d.join(p).is_file())
        };
        if complete(&dir) {
            return Ok(dir);
        }
        let tmp = partial_path(&dir);
        let _ = std::fs::remove_dir_all(&tmp);
        for (path, contents) in crate::runner_src::FILES {
            let p = tmp.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| config(format!("create {}: {e}", parent.display())))?;
            }
            std::fs::write(&p, contents)
                .map_err(|e| config(format!("write {}: {e}", p.display())))?;
        }
        // An incomplete copy from an interrupted write goes first.
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = std::fs::remove_dir_all(&tmp);
            // Another process put a complete copy there first.
            if !complete(&dir) {
                return Err(config(format!("move runner source: {e}")));
            }
        }
        Ok(dir)
    }

    /// Clone `wda_ref` (shallow) into `~/.velocity/wda/src/<ref>` unless it's
    /// already there. A pinned tag never changes, so there's nothing to pull.
    async fn ensure_source(&self, wda_ref: &str) -> Result<PathBuf> {
        let dir = self.cache_dir.join("src").join(sanitize(wda_ref));
        if dir.join("WebDriverAgent.xcodeproj").exists() && reuse_cached_source(&dir, wda_ref).await
        {
            debug!("WDA {wda_ref} source cached at {}", dir.display());
            return Ok(dir);
        }
        info!("Downloading WebDriverAgent {wda_ref}...");
        std::fs::create_dir_all(dir.parent().unwrap_or(&self.cache_dir))
            .map_err(|e| config(format!("create {}: {e}", self.cache_dir.display())))?;
        let tmp = partial_path(&dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let mut git = Command::new("git");
        git.args(["clone", "--depth", "1", "--branch", wda_ref, WDA_REPO])
            .arg(&tmp)
            .stdin(Stdio::null());
        let output = velocity_common::process::output_with_timeout(
            &mut git,
            velocity_common::process::LONG_TIMEOUT,
        )
        .await
        .map_err(|e| config(format!("Failed to run git clone: {e}")))?;
        if !output.status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(config(format!(
                "Failed to clone WebDriverAgent {wda_ref}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if pinned_commit(wda_ref).is_some() {
            let head = git_head(&tmp).await;
            if let Err(e) = check_pinned_commit(wda_ref, &head) {
                let _ = tokio::fs::remove_dir_all(&tmp).await;
                return Err(e);
            }
        }
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            // Another process finished the same clone first.
            if !dir.join("WebDriverAgent.xcodeproj").exists() {
                return Err(config(format!("move WDA source: {e}")));
            }
        }
        Ok(dir)
    }

    /// Build for the generic simulator destination into a cache keyed by
    /// (ref, Xcode build, arch); return the `.xctestrun`. Skips the build when
    /// a previous one is there.
    async fn ensure_built(&self, src: &Path, wda_ref: &str, xcode: &XcodeInfo) -> Result<PathBuf> {
        let signing = if self.physical {
            Some(device_signing()?)
        } else {
            None
        };
        let key = match &signing {
            None => format!(
                "{}-{}-{}",
                sanitize(wda_ref),
                sanitize(&xcode.build),
                std::env::consts::ARCH
            ),
            Some(s) => format!(
                "{}-{}-iphoneos-{}",
                sanitize(wda_ref),
                sanitize(&xcode.build),
                sanitize(&s.team)
            ),
        };
        let derived = self.cache_dir.join("build").join(&key);
        // A pinned ref's build counts only when it says it was built from
        // the pinned commit (one from before the pins is rebuilt once).
        let trusted = velocity_runner() || build_matches_pin(&derived, wda_ref);
        if let (true, Some(xctestrun)) = (trusted, find_xctestrun(&derived)) {
            debug!("WDA build cached: {}", xctestrun.display());
            return Ok(xctestrun);
        }
        // An .xctestrun without its runner app (a build cut short, a
        // cleaned Products folder) was trusted forever (VEL-289): rebuild.
        let _ = tokio::fs::remove_dir_all(&derived).await;
        info!("Building {wda_ref} for Xcode {} (once)...", xcode.build);
        let mjpeg = if velocity_runner() || !patch_mjpeg_bind(src) {
            MJPEG_ALL
        } else {
            MJPEG_USE_IP
        };
        let (project, scheme) = if velocity_runner() {
            ("VelocityRunner.xcodeproj", "VelocityRunner")
        } else {
            ("WebDriverAgent.xcodeproj", "WebDriverAgentRunner")
        };
        let mut build = Command::new("xcodebuild");
        build.args([
            "build-for-testing",
            "-project",
            project,
            "-scheme",
            scheme,
            "-destination",
            if signing.is_some() {
                "generic/platform=iOS"
            } else {
                "generic/platform=iOS Simulator"
            },
            "-quiet",
        ]);
        if let Some(s) = &signing {
            // Signed with the user's team; the runner needs a bundle id the
            // team can provision (WDA's own is taken).
            build.args([
                "-allowProvisioningUpdates".to_string(),
                format!("DEVELOPMENT_TEAM={}", s.team),
                "CODE_SIGN_STYLE=Automatic".to_string(),
                format!("PRODUCT_BUNDLE_IDENTIFIER={}", s.bundle_id),
            ]);
            // An App Store Connect API key lets xcodebuild create the
            // certificate, register the device and fetch the profile itself,
            // with no Apple ID signed into Xcode (CI Macs). Apple's public
            // API, not the Xcode account login.
            if let Some(key) = crate::asc::AscKeyRef::from_env()? {
                build
                    .arg("-authenticationKeyPath")
                    .arg(&key.path)
                    .args(["-authenticationKeyID", &key.key_id])
                    .args(["-authenticationKeyIssuerID", &key.issuer_id]);
            }
        }
        build
            .arg("-derivedDataPath")
            .arg(&derived)
            .current_dir(src)
            .stdin(Stdio::null());
        let output = velocity_common::process::output_with_timeout(
            &mut build,
            velocity_common::process::LONG_TIMEOUT,
        )
        .await
        .map_err(|e| config(format!("Failed to run xcodebuild: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail: Vec<&str> = stderr
                .lines()
                .filter(|l| l.contains("error"))
                .take(5)
                .collect();
            return Err(config(format!(
                "WDA {wda_ref} failed to build with Xcode {} (set VELOCITY_WDA_REF to another tag):\n{}",
                xcode.build,
                if detail.is_empty() { stderr.trim().to_string() } else { detail.join("\n") }
            )));
        }
        let xctestrun = find_xctestrun(&derived)
            .ok_or_else(|| config("WDA build produced no .xctestrun file".to_string()))?;
        let commit = if velocity_runner() {
            String::new()
        } else {
            git_head(src).await
        };
        let commit = if commit.is_empty() {
            "-".into()
        } else {
            commit
        };
        let _ = std::fs::write(
            derived.join(BUILT_FROM),
            format!("{wda_ref} {commit} {mjpeg}\n"),
        );
        if velocity_runner() {
            // ~100 MB a build: off the async workers.
            let (cache, current) = (self.cache_dir.clone(), wda_ref.to_string());
            let _ =
                tokio::task::spawn_blocking(move || prune_runner_builds(&cache, &current)).await;
        }
        Ok(xctestrun)
    }

    /// Launch the cached build on `device_id` and wait until `/status`
    /// answers. Both output pipes are drained for the life of the process.
    async fn launch_and_wait_ready(&mut self, xctestrun: &Path, device_id: &str) -> Result<()> {
        info!(
            "Launching {} on device {device_id} (port {})...",
            if velocity_runner() {
                "the Velocity runner"
            } else {
                "WebDriverAgent"
            },
            self.port
        );
        let destination = if self.physical {
            format!("id={device_id}")
        } else {
            format!("platform=iOS Simulator,id={device_id}")
        };
        // On a device WDA listens on its own port; the forward maps ours to it.
        let wda_port = if self.physical {
            DEVICE_WDA_PORT
        } else {
            self.port
        };
        // R-1: a new token for every launch (unless `ios.runnerToken` is
        // set), simulators included; the runner refuses every request
        // without it.
        if self.token.is_none() && runner_token().is_none() {
            self.new_session_token(device_id)?;
        }
        let usb = if self.physical {
            crate::usbmux::connection(device_id).await
        } else {
            None
        };
        // R-4: a cabled device's runner binds the phone's loopback, which
        // the usbmux forward reaches; `VELOCITY_IOS_DEVICE_LOOPBACK=0`
        // binds every interface instead. Only a Wi-Fi-only phone (behind
        // `ios.allowWifiRunner`) must listen on its Wi-Fi address.
        let loopback = !self.physical || (usb != Some(false) && device_loopback());
        // wifi-runner: a runner reached over Wi-Fi only listens on the
        // phone's Wi-Fi address, so it needs the opt-in and a token.
        if self.physical {
            wifi_policy(
                device_id,
                usb,
                allow_wifi_runner(),
                self.token().as_deref(),
                velocity_runner(),
            )?;
            let wifi = usb == Some(false);
            if let Some(w) =
                open_runner_warning(wifi, loopback, self.token().is_some(), velocity_runner())
            {
                tracing::warn!("{w}");
            }
        }
        let keep = keep_wda();
        let mut cmd = Command::new("xcodebuild");
        if keep {
            // Its own process group: Ctrl-C in this terminal must not stop
            // a WDA that later runs will reattach to.
            #[cfg(unix)]
            cmd.process_group(0);
        }
        let mut child = cmd
            .arg("test-without-building")
            .arg("-xctestrun")
            .arg(xctestrun)
            .args([
                "-destination",
                &destination,
                "-collect-test-diagnostics",
                "never",
            ])
            .envs(runner_env(wda_port, loopback, self.token()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(!keep)
            .spawn()
            .map_err(|e| config(format!("Failed to launch WDA: {e}")))?;

        let log_path = self
            .cache_dir
            .join("logs")
            .join(format!("{}.log", sanitize(device_id)));
        rotate_log(&log_path, MAX_LOG_BYTES);
        let (ready_tx, mut ready_rx) = watch::channel(false);
        let (cause_tx, cause_rx) = watch::channel(None);
        let (running_tx, running_rx) = watch::channel(false);
        spawn_drain(
            child.stdout.take(),
            log_path.clone(),
            ready_tx.clone(),
            cause_tx.clone(),
            running_tx.clone(),
        );
        spawn_drain(
            child.stderr.take(),
            log_path.clone(),
            ready_tx,
            cause_tx,
            running_tx,
        );
        self.wda_process = Some(child);
        if !self.physical {
            self.launched = Some((device_id.to_string(), RunnerKind::requested()));
        }

        let deadline = tokio::time::Instant::now() + WDA_READY_TIMEOUT;
        let mut warned_runner_started = false;
        let mut runner_started_at: Option<Instant> = None;
        loop {
            // A fatal startup cause (the drain matched it in the log) wins
            // over everything else: fail fast instead of waiting out the
            // iOS automation timeout.
            let fatal = cause_rx.borrow().clone().filter(|c| *c != LOCKED);
            if let Some(cause) = fatal {
                return Err(VelocityError::RunnerFailing {
                    device_id: device_id.to_string(),
                    detail: format!("{cause}; see {}", log_path.display()),
                });
            }
            if self.is_running().await {
                return Ok(());
            }
            if let Some(child) = self.wda_process.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    // This run's lines only (the log file keeps earlier
                    // runs): give the readers a moment to finish the pipes.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let cause = cause_rx.borrow().clone();
                    let detail = match cause {
                        Some(cause) => format!(
                            "exited before becoming ready ({status}); {cause}; see {}",
                            log_path.display()
                        ),
                        None => format!(
                            "exited before becoming ready ({status}); see {}",
                            log_path.display()
                        ),
                    };
                    return Err(VelocityError::RunnerFailing {
                        device_id: device_id.to_string(),
                        detail,
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(VelocityError::RunnerFailing {
                    device_id: device_id.to_string(),
                    detail: format!(
                        "failed to start within {}s on port {}{}; see {}",
                        WDA_READY_TIMEOUT.as_secs(),
                        self.port,
                        cause_rx
                            .borrow()
                            .as_ref()
                            .map(|c| format!("; {c}"))
                            .unwrap_or_default(),
                        log_path.display()
                    ),
                });
            }
            // Physical device: if the runner logged "Running tests..." but
            // still isn't serving 20 s later, warn once (the MCP/agent call
            // timeout of 55 s is shorter than iOS's ~60 s automation
            // timeout, so this is what an agent sees).
            if self.physical && !*running_rx.borrow() {
                // not yet logged "Running tests..."
            } else if self.physical && !warned_runner_started {
                if runner_started_at.is_none() {
                    runner_started_at = Some(Instant::now());
                } else if let Some(started) = runner_started_at {
                    if Instant::now() - started >= Duration::from_secs(20) {
                        warn!(
                            "the runner started on the phone but isn't serving yet; if this lasts, \
                             enable Settings → Developer → Enable UI Automation and keep the phone unlocked"
                        );
                        warned_runner_started = true;
                    }
                }
            }
            // Wake early on a log readiness marker, else poll.
            let _ = tokio::time::timeout(WDA_POLL_INTERVAL, ready_rx.changed()).await;
        }
    }

    /// Stop the WDA process if we started it.
    pub async fn stop(&mut self) {
        if keep_wda() {
            // Left running for the next run to reattach to.
            self.wda_process = None;
            return;
        }
        if let Some(mut child) = self.wda_process.take() {
            debug!("Stopping WDA process");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        // Killing xcodebuild leaves the runner app answering for a while;
        // a run starting meanwhile would adopt it and lose it mid-test.
        if let Some((device_id, kind)) = self.launched.take() {
            self.forget_launch_marker(&device_id);
            self.stop_runner(&device_id, kind).await;
            self.forget_session_token(&device_id);
        }
        self.wda_ref = None;
    }

    /// Restart a runner that answers `/status` but nothing else (its main
    /// thread is wedged, review MAJOR-1): [`Self::ensure_running`] alone
    /// would adopt it again. Stops our process and the runner app, then
    /// starts a fresh one.
    pub async fn restart(&mut self, device_id: &str) -> Result<()> {
        if let Some(mut child) = self.wda_process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.launched = None;
        self.forget_launch_marker(device_id);
        self.wda_ref = None;
        if !self.physical {
            if let Some((kind, _, _)) = self.running_kind().await {
                self.stop_runner(device_id, kind).await;
            }
        }
        self.ensure_running(device_id).await
    }

    /// Whether the WDA process we launched has exited (crashed).
    pub fn has_exited(&mut self) -> bool {
        match self.wda_process.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            None => false,
        }
    }

    /// Get the WDA base URL.
    pub fn base_url(&self) -> String {
        // 127.0.0.1, not `localhost` (::1 first on some setups): IOS-N7.
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for WdaBootstrap {
    fn drop(&mut self) {
        if keep_wda() {
            return;
        }
        if let Some(ref mut child) = self.wda_process {
            let _ = child.start_kill();
        }
    }
}

/// The runner's environment. WDA reads `USE_PORT` and `USE_IP`;
/// xcodebuild forwards `TEST_RUNNER_`-prefixed variables into the test
/// runner process.
///
/// On a simulator the runner shares the Mac's network stack, so without
/// `USE_IP` WDA (and VelocityRunner) listened on every interface of the Mac,
/// unauthenticated (VEL-152, IOS-N2). A real device binds its loopback
/// when `loopback` (cabled, the default since R-4), else every interface of
/// the iPhone, Wi-Fi included (`VELOCITY_IOS_DEVICE_LOOPBACK=0`, or a
/// Wi-Fi-only phone under `ios.allowWifiRunner`).
fn launch_env(port: u16, loopback: bool) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("USE_PORT", port.to_string()),
        ("TEST_RUNNER_USE_PORT", port.to_string()),
        // WDA's MJPEG screen stream (the inspector console's iOS mirror):
        // its own port per device, where every WDA used to take 9100.
        ("MJPEG_SERVER_PORT", mjpeg_port(port).to_string()),
        (
            "TEST_RUNNER_MJPEG_SERVER_PORT",
            mjpeg_port(port).to_string(),
        ),
        (
            "TEST_RUNNER_VELOCITY_RUNNER_BUILD",
            RunnerKind::current_build(),
        ),
    ];
    if loopback {
        env.push(("USE_IP", "127.0.0.1".into()));
        env.push(("TEST_RUNNER_USE_IP", "127.0.0.1".into()));
    }
    env
}

/// The MJPEG screen-stream port of the WDA listening on `wda_port`.
pub fn mjpeg_port(wda_port: u16) -> u16 {
    // Not +1000: WDA 8100 would take 9100, every older WDA's default.
    wda_port.saturating_add(1100)
}

/// `ios.allowWifiRunner: true` (`VELOCITY_IOS_ALLOW_WIFI_RUNNER=1`): run
/// the runner on an iPhone usbmuxd lists over Wi-Fi only.
fn allow_wifi_runner() -> bool {
    matches!(
        velocity_common::env::var("VELOCITY_IOS_ALLOW_WIFI_RUNNER")
            .as_deref()
            .map(str::trim),
        Ok("1" | "true" | "on" | "yes")
    )
}

/// Shortest bearer token accepted for a Wi-Fi runner.
pub(crate) const MIN_TOKEN_LEN: usize = 16;

/// `ios.runnerToken` (`VELOCITY_IOS_RUNNER_TOKEN`): a fixed bearer token
/// in place of the random per-launch one (a runner at `VELOCITY_WDA_URL`
/// started by hand needs the same value as its `VELOCITY_RUNNER_TOKEN`).
/// Masked in every log and report once read.
pub(crate) fn runner_token() -> Option<String> {
    let t = velocity_common::env::var("VELOCITY_IOS_RUNNER_TOKEN").ok()?;
    let t = t.trim().to_string();
    if t.is_empty() {
        return None;
    }
    velocity_common::redact::register_secret(&t);
    Some(t)
}

/// The runner's token variable (xcodebuild hands `TEST_RUNNER_` variables
/// to the runner without the prefix).
fn token_env(token: Option<String>) -> Vec<(&'static str, String)> {
    token
        .map(|t| vec![("TEST_RUNNER_VELOCITY_RUNNER_TOKEN", t)])
        .unwrap_or_default()
}

/// 128 random bits as hex: the per-launch runner token (R-1).
fn random_token() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| config("no randomness for the runner token".into()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write `text` to `path`, readable by this user only.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // An older file keeps its mode through `open`.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

/// Everything the runner is launched with: port and bind address
/// ([`launch_env`]) and the token. Every runner gets the token, simulators
/// included (R-1: a web page can reach the Mac's loopback); WebDriverAgent
/// ignores it.
fn runner_env(port: u16, loopback: bool, token: Option<String>) -> Vec<(&'static str, String)> {
    let mut env = launch_env(port, loopback);
    env.extend(token_env(token));
    env
}

/// The warning for a cabled iPhone whose runner listens on every interface
/// (`VELOCITY_IOS_DEVICE_LOOPBACK=0`) and checks no token (WebDriverAgent):
/// anyone on the phone's Wi-Fi network can drive it while it runs.
fn open_runner_warning(
    wifi: bool,
    loopback: bool,
    token: bool,
    velocity_runner: bool,
) -> Option<String> {
    if wifi || loopback || (token && velocity_runner) {
        return None;
    }
    let why = if token {
        "WebDriverAgent checks no token"
    } else {
        "it was started without a token"
    };
    Some(format!(
        "the runner on this cabled iPhone listens on all of its interfaces, Wi-Fi \
         included (VELOCITY_IOS_DEVICE_LOOPBACK=0), and {why}: anyone on that network \
         can drive the phone while it runs. Unset VELOCITY_IOS_DEVICE_LOOPBACK to bind \
         loopback only, or use ios.runner: velocity"
    ))
}

/// wifi-runner (V1): a runner on a cabled iPhone is reached through
/// usbmuxd; on a Wi-Fi-only one it listens on the phone's Wi-Fi address,
/// where anyone on the network can drive the phone. That needs
/// `ios.allowWifiRunner: true` and a bearer token, which only
/// VelocityRunner checks (WebDriverAgent has no authentication): the
/// per-launch one, or an `ios.runnerToken` of at least [`MIN_TOKEN_LEN`].
/// `usb` is [`crate::usbmux::connection`]: `None` (not listed) is left to
/// the forward's own error.
fn wifi_policy(
    udid: &str,
    usb: Option<bool>,
    allow: bool,
    token: Option<&str>,
    velocity_runner: bool,
) -> Result<()> {
    if usb != Some(false) {
        return Ok(());
    }
    if !allow {
        return Err(config(format!(
            "iPhone {udid} is connected over Wi-Fi only: connect a cable. The runner \
             would listen on the phone's Wi-Fi address, where anyone on the network \
             could drive it (to allow it anyway: ios.allowWifiRunner: true, with \
             ios.runner: velocity and an ios.runnerToken)"
        )));
    }
    if !velocity_runner {
        return Err(config(
            "ios.allowWifiRunner needs ios.runner: velocity: WebDriverAgent has no \
             authentication, so its runner can't be reached safely over Wi-Fi"
                .into(),
        ));
    }
    if token.is_none_or(|t| t.chars().count() < MIN_TOKEN_LEN) {
        return Err(config(format!(
            "ios.allowWifiRunner needs ios.runnerToken (VELOCITY_IOS_RUNNER_TOKEN), a \
             bearer token of {MIN_TOKEN_LEN}+ characters the runner requires on every request"
        )));
    }
    Ok(())
}

/// Whether the runner on a USB-connected iPhone binds its loopback (the
/// default since R-4: the usbmux forward reaches it there).
/// `VELOCITY_IOS_DEVICE_LOOPBACK=0` binds every interface instead, for a
/// phone whose usbmuxd doesn't deliver to loopback.
fn device_loopback() -> bool {
    loopback_setting(
        velocity_common::env::var("VELOCITY_IOS_DEVICE_LOOPBACK")
            .ok()
            .as_deref(),
    )
}

/// [`device_loopback`] for the variable's value: only an explicit "off"
/// turns it off.
fn loopback_setting(value: Option<&str>) -> bool {
    !matches!(value.map(str::trim), Some("0" | "false" | "off" | "no"))
}

/// How long stopping a runner app may take (`VELOCITY_IOS_STOP_TIMEOUT_MS`).
fn stop_timeout() -> Duration {
    velocity_common::settings::ms("VELOCITY_IOS_STOP_TIMEOUT_MS", 10_000)
}

/// `VELOCITY_IOS_RUNNER=velocity`: Velocity's own runner instead of WDA.
pub(crate) fn velocity_runner() -> bool {
    velocity_common::env::var("VELOCITY_IOS_RUNNER").as_deref() == Ok("velocity")
}

/// The two runners Velocity drives iOS with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerKind {
    Wda,
    Velocity,
}

impl RunnerKind {
    /// The one `VELOCITY_IOS_RUNNER` asks for.
    fn requested() -> Self {
        if velocity_runner() {
            Self::Velocity
        } else {
            Self::Wda
        }
    }

    fn from_status(status: &serde_json::Value) -> Self {
        // (Only meaningful for a body `is_runner_status` accepts.)
        let version = status["value"]["build"]["version"].as_str().unwrap_or("");
        if version.starts_with("velocity-runner") {
            Self::Velocity
        } else {
            Self::Wda
        }
    }

    /// The VelocityRunner build this Velocity embeds.
    fn current_build() -> String {
        format!("velocity-runner-{}", crate::runner_src::hash())
    }

    fn name(self) -> &'static str {
        match self {
            Self::Wda => "WebDriverAgent",
            Self::Velocity => "VelocityRunner",
        }
    }

    /// The test-runner app on the simulator.
    fn bundle_id(self) -> &'static str {
        match self {
            Self::Wda => "com.facebook.WebDriverAgentRunner.xctrunner",
            Self::Velocity => "dev.velocity.runner.xctrunner",
        }
    }
}

/// Signing for a WDA build on a real device.
struct DeviceSigning {
    team: String,
    bundle_id: String,
}

/// `VELOCITY_IOS_TEAM_ID` (required) and `VELOCITY_WDA_BUNDLE_ID` (default
/// `com.<team>.velocity.WebDriverAgentRunner`).
fn device_signing() -> Result<DeviceSigning> {
    let team = velocity_common::env::var("VELOCITY_IOS_TEAM_ID").map_err(|_| {
        config(
            "a real iPhone needs WebDriverAgent signed with your Apple team: \
             set VELOCITY_IOS_TEAM_ID (Xcode → Settings → Accounts → Team ID); \
             with no Apple ID in Xcode, also set VELOCITY_ASC_KEY_PATH / _KEY_ID / \
             _ISSUER_ID (an App Store Connect API key)"
                .to_string(),
        )
    })?;
    let bundle_id = velocity_common::env::var("VELOCITY_WDA_BUNDLE_ID")
        .unwrap_or_else(|_| format!("com.{}.velocity.WebDriverAgentRunner", team.to_lowercase()));
    Ok(DeviceSigning { team, bundle_id })
}

/// Leave WDA running at exit so the next run reattaches (`VELOCITY_KEEP_WDA=1`).
fn keep_wda() -> bool {
    matches!(
        velocity_common::env::var("VELOCITY_KEEP_WDA").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Whether a `/status` body is WebDriverAgent's or VelocityRunner's: both
/// answer `{"value": {"ready": …, "build": {…}}}`; WDA's message names it.
fn is_runner_status(body: &serde_json::Value) -> bool {
    let v = &body["value"];
    v["ready"].is_boolean()
        || v["build"].is_object()
        || v["message"]
            .as_str()
            .is_some_and(|m| m.contains("WebDriverAgent") || m.contains("VelocityRunner"))
}

/// First WDA port for simulators; simulator *n* gets `BASE_PORT + n`.
pub const BASE_PORT: u16 = WDA_PORT;
const PORT_RANGE: u16 = 32;

/// The port `device_id`'s WDA should use: the one its WDA already listens on
/// (reattach), else the first port in range nobody listens on and that
/// isn't in `taken` (ports this process gave to other simulators).
pub async fn pick_port(device_id: &str, taken: &[u16]) -> u16 {
    let mut free = None;
    for port in BASE_PORT..BASE_PORT + PORT_RANGE {
        if taken.contains(&port) {
            continue;
        }
        let pids = listening_pids(port).await;
        if pids.is_empty() {
            free.get_or_insert(port);
            continue;
        }
        if owner_of(&pids).await.as_deref() == Some(device_id) {
            return port;
        }
    }
    if free.is_none() {
        // Every port is taken: the base port's holder is checked (and
        // refused when it isn't this device's runner) by `ensure_running`.
        warn!(
            device_id,
            "no free WebDriverAgent port in {BASE_PORT}..{}",
            BASE_PORT + PORT_RANGE
        );
    }
    free.unwrap_or(BASE_PORT)
}

/// Failed launches within [`CRASH_LOOP_WINDOW`] after which a device's
/// runner is no longer relaunched.
const CRASH_LOOP_MAX: usize = 3;
const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(120);

/// `pid <n> (<command>)` for each of `pids`, for an error message.
async fn describe_pids(pids: &[String]) -> String {
    let mut out = Vec::new();
    for pid in pids {
        let mut cmd = Command::new("ps");
        cmd.args(["-o", "command=", "-p", pid]).stdin(Stdio::null());
        let command =
            velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(5))
                .await
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
        let command: String = command.chars().take(120).collect();
        out.push(if command.is_empty() {
            format!("pid {pid}")
        } else {
            format!("pid {pid} ({command})")
        });
    }
    out.join(", ")
}

/// The simulator whose WDA listens on `port`, if any.
pub async fn port_owner(port: u16) -> Option<String> {
    owner_of(&listening_pids(port).await).await
}

async fn listening_pids(port: u16) -> Vec<String> {
    let mut cmd = Command::new("lsof");
    cmd.args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .stdin(Stdio::null());
    velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(5))
        .await
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// A simulator app runs from `…/CoreSimulator/Devices/<UDID>/…`, so the
/// listening process's path names its simulator.
async fn owner_of(pids: &[String]) -> Option<String> {
    for pid in pids {
        let mut cmd = Command::new("ps");
        cmd.args(["-o", "command=", "-p", pid]).stdin(Stdio::null());
        if let Ok(o) =
            velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(5)).await
        {
            if let Some(udid) = udid_in_path(&String::from_utf8_lossy(&o.stdout)) {
                return Some(udid);
            }
        }
    }
    None
}

fn udid_in_path(command: &str) -> Option<String> {
    let rest = command.split("/CoreSimulator/Devices/").nth(1)?;
    let udid: String = rest.chars().take_while(|c| *c != '/').collect();
    let looks_like_udid =
        udid.len() == 36 && udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    looks_like_udid.then_some(udid)
}

/// Read a pipe to EOF, appending lines to `log`, and flag readiness markers.
/// A runner log grows by every launch's output: at most this before it is
/// moved to `<name>.log.1` (VEL-287).
const MAX_LOG_BYTES: u64 = 8 << 20;

/// Move `log` to `<log>.1` (replacing the previous one) once it is over
/// `max` bytes, so a device's log keeps this launch and roughly the last.
fn rotate_log(log: &Path, max: u64) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > max) {
        let mut old = log.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(log, PathBuf::from(old));
    }
}

fn spawn_drain<R>(
    pipe: Option<R>,
    log: PathBuf,
    ready: watch::Sender<bool>,
    cause: watch::Sender<Option<String>>,
    running: watch::Sender<bool>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(pipe) = pipe else { return };
    tokio::spawn(async move {
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .await
            .ok();
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("ServerURLHere") || line.contains("WebDriverAgent started") {
                let _ = ready.send(true);
            }
            if line.contains("Running tests...") {
                let _ = running.send(true);
            }
            if let Some(c) = startup_cause(&line) {
                let _ = cause.send(Some(c.to_string()));
            }
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
            }
        }
    });
}

/// If `line` names a known reason the runner can't start on a device,
/// return the user-facing cause (readiness signalling stays on its own
/// channel). Substring match, case-insensitive.
/// Not fatal on its own: xcodebuild may wait for the unlock and go on, so
/// it is only named when the start fails some other way.
const LOCKED: &str = "the device is locked: unlock it and keep the screen on";

fn startup_cause(line: &str) -> Option<&'static str> {
    let l = line.to_lowercase();
    if l.contains("timed out while enabling automation mode")
        || l.contains("failed to initialize for ui testing")
    {
        return Some(
            "the iPhone did not allow UI automation: on the phone turn on Settings \
             → Developer → Enable UI Automation, unlock it, and enter the passcode \
             if asked; then run again",
        );
    }
    if l.contains("developer mode") && l.contains("disabled") {
        return Some(
            "Developer Mode is off on the device: Settings → Privacy & Security \
             → Developer Mode",
        );
    }
    if l.contains("invalid code signature")
        || l.contains("has not been explicitly trusted")
        || l.contains("untrusted developer")
    {
        return Some(
            "the device does not trust the runner's signing certificate: \
             Settings → General → VPN & Device Management → trust it",
        );
    }
    if l.contains("device is locked") || l.contains("passcode protected") {
        return Some(LOCKED);
    }
    None
}

struct XcodeInfo {
    developer_dir: PathBuf,
    build: String,
}

async fn xcode_info() -> Result<XcodeInfo> {
    let out = |args: &'static [&'static str]| async move {
        let mut cmd = Command::new(args[0]);
        cmd.args(&args[1..]).stdin(Stdio::null());
        velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(30))
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let developer_dir = out(&["xcode-select", "-p"])
        .await
        .map(PathBuf::from)
        .ok_or_else(|| config("Xcode not found (xcode-select -p failed)".to_string()))?;
    let version = out(&["xcodebuild", "-version"]).await.unwrap_or_default();
    let build = version
        .lines()
        .find_map(|l| l.strip_prefix("Build version "))
        .unwrap_or("unknown")
        .trim()
        .to_string();
    Ok(XcodeInfo {
        developer_dir,
        build,
    })
}

/// The WDA tag the installed Xcode can build: newer tags copy
/// `lib_TestingInterop.dylib` from the simulator platform, so they're only
/// usable where that file exists.
fn pick_wda_ref(developer_dir: &Path) -> &'static str {
    let interop = developer_dir
        .join("Platforms/iPhoneSimulator.platform/Developer/usr/lib/lib_TestingInterop.dylib");
    if interop.exists() {
        WDA_REF_TESTING_INTEROP
    } else {
        WDA_REF_LEGACY
    }
}

/// The simulator runner build cache for one Xcode, as `velocity doctor`
/// reports it: which runner and ref the next run would use, and whether
/// it is already built (else the first iOS run builds it once).
#[derive(Debug, Clone, PartialEq)]
pub struct RunnerCache {
    /// `WebDriverAgent` or `VelocityRunner` (`VELOCITY_IOS_RUNNER=velocity`).
    pub runner: &'static str,
    /// The WDA tag (or `velocity-runner-<hash>`) the build is keyed by.
    pub wda_ref: String,
    /// `<cache>/build/<ref>-<xcode build>-<arch>`.
    pub dir: PathBuf,
    /// A complete, trusted build is there.
    pub built: bool,
    /// This Xcode's simulator platform ships `lib_TestingInterop.dylib`
    /// (so it needs the newer pinned tag).
    pub testing_interop: bool,
    /// Whether the ref is one of Velocity's pins, and what is known about
    /// it on Xcode (`None` for an unpinned `VELOCITY_WDA_REF`).
    pub pin_note: Option<&'static str>,
}

/// [`RunnerCache`] for the Xcode at `developer_dir` (`xcode-select -p`)
/// whose build is `xcode_build` (`17A400`), with the cache under
/// `cache_dir` (`~/.velocity/wda`). Reads the file system only.
pub fn runner_cache(cache_dir: &Path, developer_dir: &Path, xcode_build: &str) -> RunnerCache {
    let testing_interop = pick_wda_ref(developer_dir) == WDA_REF_TESTING_INTEROP;
    let (runner, wda_ref) = if velocity_runner() {
        ("VelocityRunner", RunnerKind::current_build())
    } else {
        (
            "WebDriverAgent",
            velocity_common::env::var("VELOCITY_WDA_REF")
                .unwrap_or_else(|_| pick_wda_ref(developer_dir).to_string()),
        )
    };
    let dir = cache_dir.join("build").join(format!(
        "{}-{}-{}",
        sanitize(&wda_ref),
        sanitize(xcode_build),
        std::env::consts::ARCH
    ));
    let trusted = velocity_runner() || build_matches_pin(&dir, &wda_ref);
    let built = trusted && find_xctestrun(&dir).is_some();
    let pin_note = if velocity_runner() {
        Some("built from the source embedded in this Velocity")
    } else if wda_ref == WDA_REF_LEGACY {
        Some("pinned; verified on Xcode 26.0.1 (no lib_TestingInterop.dylib)")
    } else if wda_ref == WDA_REF_TESTING_INTEROP {
        Some("pinned for Xcodes with lib_TestingInterop.dylib; not yet verified on one")
    } else {
        None
    };
    RunnerCache {
        runner,
        wda_ref,
        dir,
        built,
        testing_interop,
        pin_note,
    }
}

/// `~/.velocity/wda`: where runner sources and builds are cached.
pub fn default_cache_dir() -> PathBuf {
    dirs_home().join(".velocity").join("wda")
}

/// The build's `.xctestrun`, when the runner app it points at is there too
/// (`Build/Products/<config>-<sdk>/<scheme>-Runner.app`).
fn find_xctestrun(derived: &Path) -> Option<PathBuf> {
    let products = derived.join("Build").join("Products");
    let entries: Vec<PathBuf> = std::fs::read_dir(&products)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    let has_runner_app = entries.iter().filter(|p| p.is_dir()).any(|d| {
        std::fs::read_dir(d).ok().is_some_and(|mut it| {
            it.any(|e| {
                e.ok().is_some_and(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.ends_with("-Runner.app") && e.path().is_dir()
                })
            })
        })
    });
    if !has_runner_app {
        return None;
    }
    entries
        .into_iter()
        .find(|p| p.extension().is_some_and(|x| x == "xctestrun"))
}

/// A temp sibling of `dir` unique to this process and to `dir`'s whole
/// name: `with_extension("partial")` cut `v9.3.2` to `v9.3.partial`, which
/// every v9.3.x shared (VEL-290).
fn partial_path(dir: &Path) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.with_file_name(format!("{name}.partial-{}", std::process::id()))
}

/// An exclusive lock on `<cache>/locks/<key>.lock`, held until the file is
/// dropped (released by the OS if the process dies).
async fn cache_lock(cache_dir: &Path, key: &str) -> Result<std::fs::File> {
    let dir = cache_dir.join("locks");
    let path = dir.join(format!("{}.lock", sanitize(key)));
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        f.lock()?;
        Ok::<_, std::io::Error>(f)
    })
    .await
    .map_err(|e| config(format!("runner cache lock: {e}")))?
    .map_err(|e| config(format!("runner cache lock: {e}")))
}

/// Every VelocityRunner source changes its build key, so each Velocity
/// update left a ~100 MB build behind (VEL-288). Remove the
/// `velocity-runner-*` builds and sources of other runner versions than
/// `current` (any Xcode, any destination), unless touched within a day (a
/// run of another Velocity version may be using one).
fn prune_runner_builds(cache_dir: &Path, current: &str) {
    let day = Duration::from_secs(24 * 3600);
    let keep = sanitize(current);
    for sub in ["build", "src"] {
        let Ok(entries) = std::fs::read_dir(cache_dir.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("velocity-runner-") || name.starts_with(&keep) {
                continue;
            }
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if old {
                debug!("pruning old runner cache {}", e.path().display());
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn config(msg: String) -> VelocityError {
    VelocityError::Config(msg)
}

fn dirs_home() -> PathBuf {
    velocity_common::host::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_runners_listen_on_loopback_only() {
        // VEL-152 / IOS-N2: without USE_IP, WDA listened on every interface.
        let has =
            |env: &[(&str, String)], k: &str| env.iter().any(|(n, v)| *n == k && v == "127.0.0.1");
        let sim = launch_env(8101, true);
        assert!(
            has(&sim, "USE_IP") && has(&sim, "TEST_RUNNER_USE_IP"),
            "{sim:?}"
        );
        assert!(sim.iter().any(|(n, v)| *n == "USE_PORT" && v == "8101"));
        assert!(sim
            .iter()
            .any(|(n, v)| *n == "TEST_RUNNER_MJPEG_SERVER_PORT" && v == "9201"));
        // Real devices bind every interface only when not `loopback`
        // (VELOCITY_IOS_DEVICE_LOOPBACK=0, or a Wi-Fi-only phone).
        let dev = launch_env(8100, false);
        assert!(!dev.iter().any(|(n, _)| n.ends_with("USE_IP")), "{dev:?}");
        assert!(WdaBootstrap::new().with_port(8105).base_url() == "http://127.0.0.1:8105");
    }

    /// wifi-runner: a Wi-Fi-only iPhone fails with "connect a cable"
    /// unless allowed, and allowing it needs VelocityRunner and a token.
    #[test]
    fn a_wifi_only_iphone_needs_a_cable_or_the_opt_in_with_a_token() {
        let token = "0123456789abcdef";
        // Cabled, or not listed: no change.
        for usb in [Some(true), None] {
            wifi_policy("U", usb, false, None, false).unwrap();
        }
        let e = wifi_policy("U", Some(false), false, Some(token), true).unwrap_err();
        assert!(matches!(e, VelocityError::Config(_)), "{e}");
        assert!(e.to_string().contains("connect a cable"), "{e}");
        let e = wifi_policy("U", Some(false), true, Some(token), false).unwrap_err();
        assert!(e.to_string().contains("ios.runner: velocity"), "{e}");
        for bad in [None, Some("short")] {
            let e = wifi_policy("U", Some(false), true, bad, true).unwrap_err();
            assert!(e.to_string().contains("ios.runnerToken"), "{e}");
        }
        wifi_policy("U", Some(false), true, Some(token), true).unwrap();
        assert_eq!(
            token_env(Some(token.into())),
            [("TEST_RUNNER_VELOCITY_RUNNER_TOKEN", token.to_string())]
        );
        assert!(token_env(None).is_empty());
    }

    #[tokio::test]
    async fn stopping_a_runner_is_bounded_and_uses_the_xcrun_override() {
        // VEL-350: `xcrun simctl terminate` had no deadline and ignored
        // VELOCITY_XCRUN_PATH; a hung simctl kept the run from exiting.
        let dir = std::env::temp_dir().join(format!("vel-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("called");
        let stub = dir.join("xcrun");
        crate::write_stub(
            &stub,
            &format!("echo \"$@\" > '{}'\nexec sleep 30", marker.display()),
        );
        // A port nothing listens on, so the wait for it to free is instant.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let b = WdaBootstrap::new().with_port(port);
        let started = std::time::Instant::now();
        let simctl = crate::simctl::Simctl::with_xcrun(stub.to_str().unwrap());
        b.stop_runner_via(&simctl, Duration::from_secs(2), "SIM-UDID", RunnerKind::Wda)
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let called = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(called.contains("simctl terminate SIM-UDID"), "{called:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_tells_velocity_runner_from_wda() {
        let ours = serde_json::json!({"value": {"message": "VelocityRunner ready",
            "build": {"version": "velocity-runner-1"}}});
        let wda = serde_json::json!({"value": {"ready": true,
            "build": {"productBundleIdentifier": "com.facebook.WebDriverAgentRunner"}}});
        assert_eq!(RunnerKind::from_status(&ours), RunnerKind::Velocity);
        assert_eq!(RunnerKind::from_status(&wda), RunnerKind::Wda);
        assert_eq!(
            RunnerKind::from_status(&serde_json::Value::Null),
            RunnerKind::Wda
        );
    }

    /// D-37: the interop pin is v16.14.1 or newer, and every pin carries
    /// the commit its clone must check out.
    #[test]
    fn the_wda_pins_are_checked_against_their_commits() {
        let v: Vec<u32> = WDA_REF_TESTING_INTEROP
            .trim_start_matches('v')
            .split('.')
            .map(|n| n.parse().unwrap())
            .collect();
        assert!(v >= vec![16, 14, 1], "{WDA_REF_TESTING_INTEROP}");
        for tag in [WDA_REF_LEGACY, WDA_REF_TESTING_INTEROP] {
            let sha = pinned_commit(tag).unwrap();
            assert_eq!(sha.len(), 40, "{tag}");
            assert!(check_pinned_commit(tag, sha).is_ok());
            assert!(check_pinned_commit(tag, &format!("{}\n", sha.to_uppercase())).is_ok());
            let e = check_pinned_commit(tag, "0123456789abcdef0123456789abcdef01234567")
                .unwrap_err()
                .to_string();
            assert!(e.contains(sha) && e.contains("moved upstream"), "{e}");
            assert!(
                check_pinned_commit(tag, "").is_err(),
                "an unreadable HEAD fails"
            );
        }
        assert_eq!(
            pinned_commit("v9.9.9"),
            None,
            "a VELOCITY_WDA_REF tag of the user's own isn't checked"
        );
        assert!(check_pinned_commit("v9.9.9", "anything").is_ok());
    }

    #[test]
    fn runner_cache_reports_the_ref_this_xcode_needs_and_whether_it_is_built() {
        if velocity_runner() || velocity_common::env::var("VELOCITY_WDA_REF").is_ok() {
            return;
        }
        let root = std::env::temp_dir().join(format!("vel-runner-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("Developer");
        std::fs::create_dir_all(&dev).unwrap();
        let cache = root.join("wda");
        let c = runner_cache(&cache, &dev, "17A400");
        assert_eq!(c.wda_ref, WDA_REF_LEGACY);
        assert!(!c.built && !c.testing_interop);
        assert!(c.pin_note.unwrap().contains("verified"));
        // A complete build stamped with the pinned commit counts.
        let products = c.dir.join("Build/Products");
        std::fs::create_dir_all(
            products.join("Debug-iphonesimulator/WebDriverAgentRunner-Runner.app"),
        )
        .unwrap();
        std::fs::write(products.join("x.xctestrun"), "").unwrap();
        assert!(!runner_cache(&cache, &dev, "17A400").built, "no pin stamp");
        std::fs::write(
            c.dir.join(BUILT_FROM),
            format!(
                "{WDA_REF_LEGACY} {} {MJPEG_USE_IP}",
                pinned_commit(WDA_REF_LEGACY).unwrap()
            ),
        )
        .unwrap();
        assert!(runner_cache(&cache, &dev, "17A400").built);
        assert!(!runner_cache(&cache, &dev, "17B100").built, "another Xcode");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn picks_legacy_tag_without_testing_interop() {
        let dir = std::env::temp_dir().join(format!("vel-xcode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(pick_wda_ref(&dir), WDA_REF_LEGACY);
        let lib = dir.join("Platforms/iPhoneSimulator.platform/Developer/usr/lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("lib_TestingInterop.dylib"), b"").unwrap();
        assert_eq!(pick_wda_ref(&dir), WDA_REF_TESTING_INTEROP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_xctestrun_in_products() {
        let dir = std::env::temp_dir().join(format!("vel-dd-{}", std::process::id()));
        let products = dir.join("Build/Products");
        std::fs::create_dir_all(&products).unwrap();
        assert!(find_xctestrun(&dir).is_none());
        std::fs::write(
            products.join("WebDriverAgentRunner_iphonesimulator26.0-arm64.xctestrun"),
            b"",
        )
        .unwrap();
        std::fs::create_dir_all(
            products.join("Debug-iphonesimulator/WebDriverAgentRunner-Runner.app"),
        )
        .unwrap();
        assert!(find_xctestrun(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_simulator_in_a_process_path() {
        let cmd = "/Users/me/Library/Developer/CoreSimulator/Devices/290403AA-C40F-404B-9CB0-DAFB6A943142/data/Containers/Bundle/Application/X/WebDriverAgentRunner-Runner.app/WebDriverAgentRunner-Runner";
        assert_eq!(
            udid_in_path(cmd).as_deref(),
            Some("290403AA-C40F-404B-9CB0-DAFB6A943142")
        );
        assert_eq!(udid_in_path("/usr/bin/python3 -m http.server 8100"), None);
        assert_eq!(udid_in_path("/x/CoreSimulator/Devices/short/y"), None);
    }

    #[test]
    fn sanitizes_cache_keys() {
        assert_eq!(sanitize("v16.12.10"), "v16.12.10");
        assert_eq!(sanitize("feature/x y"), "feature_x_y");
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("velocity-boot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_interrupted_runner_source_is_rewritten_whole() {
        // VEL-148: a project file without the Swift source counted as cached.
        let cache = scratch("src");
        let mut b = WdaBootstrap::new();
        b.cache_dir = cache.clone();
        let dir = cache.join("src").join("velocity-runner-x");
        std::fs::create_dir_all(dir.join("VelocityRunner.xcodeproj")).unwrap();
        assert_eq!(b.ensure_runner_source("velocity-runner-x").unwrap(), dir);
        for (p, contents) in crate::runner_src::FILES {
            assert_eq!(
                &std::fs::read_to_string(dir.join(p)).unwrap(),
                contents,
                "{p}"
            );
        }
        let left: Vec<_> = std::fs::read_dir(cache.join("src"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(left.len(), 1, "no temp dir left");
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn temp_dirs_are_per_tag_and_per_process() {
        // VEL-290: v9.3.1 and v9.3.2 both cloned into "v9.3.partial".
        let a = partial_path(Path::new("/c/src/v9.3.1"));
        let b = partial_path(Path::new("/c/src/v9.3.2"));
        assert_ne!(a, b);
        assert!(a
            .to_string_lossy()
            .ends_with(&format!("v9.3.1.partial-{}", std::process::id())));
    }

    #[test]
    fn a_build_without_its_runner_app_is_not_a_cache_hit() {
        // VEL-289: any .xctestrun was trusted forever.
        let d = scratch("xctestrun");
        let products = d.join("Build").join("Products");
        std::fs::create_dir_all(&products).unwrap();
        std::fs::write(products.join("R_iphonesimulator.xctestrun"), "x").unwrap();
        assert_eq!(find_xctestrun(&d), None);
        std::fs::create_dir_all(products.join("Debug-iphonesimulator/VelocityRunner-Runner.app"))
            .unwrap();
        assert_eq!(
            find_xctestrun(&d),
            Some(products.join("R_iphonesimulator.xctestrun"))
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_runner_log_is_rotated_past_its_cap() {
        // VEL-287: the per-device log grew by every launch, forever.
        let d = scratch("log");
        let log = d.join("SIM.log");
        std::fs::write(&log, vec![b'x'; 100]).unwrap();
        rotate_log(&log, 1000);
        assert!(log.exists(), "under the cap: kept");
        rotate_log(&log, 50);
        assert!(!log.exists());
        assert_eq!(std::fs::read(d.join("SIM.log.1")).unwrap().len(), 100);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn old_runner_builds_of_other_versions_are_pruned() {
        // VEL-288: every runner version kept its build forever.
        let d = scratch("prune");
        let old = std::time::SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
        let mk = |sub: &str, name: &str, aged: bool| {
            let p = d.join(sub).join(name);
            std::fs::create_dir_all(&p).unwrap();
            if aged {
                std::fs::File::open(&p).unwrap().set_modified(old).unwrap();
            }
            p
        };
        let stale = mk("build", "velocity-runner-aaa-17A1-arm64", true);
        let stale_src = mk("src", "velocity-runner-aaa", true);
        let recent = mk("build", "velocity-runner-bbb-17A1-arm64", false);
        let current = mk("build", "velocity-runner-ccc-17A1-iphoneos-T", true);
        let wda = mk("build", "v9.3.2-17A1-arm64", true);
        prune_runner_builds(&d, "velocity-runner-ccc");
        assert!(!stale.exists() && !stale_src.exists());
        assert!(recent.exists() && current.exists() && wda.exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_bootstrap_waits_for_the_first() {
        // VEL-149: two processes bootstrapping at once raced on one cache.
        let d = scratch("lock");
        let first = cache_lock(&d, "velocity-runner-x").await.unwrap();
        let d2 = d.clone();
        let second = tokio::spawn(async move { cache_lock(&d2, "velocity-runner-x").await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!second.is_finished(), "waits while the first holds it");
        // Another key is independent.
        drop(cache_lock(&d, "v9.3.2").await.unwrap());
        drop(first);
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }

    /// fi56: a kept WDA (`VELOCITY_KEEP_WDA`) that a run reattaches to
    /// is known by the ref its launching run recorded next to the cached
    /// build; one with no record is unknown (no default deadline).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reattached_wda_keeps_the_ref_it_was_built_from() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let body = r#"{"value":{"ready":true,"message":"WebDriverAgent is ready to accept commands"},"sessionId":null}"#;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        let udid = "00000000-0000-0000-0000-0000000000AB";
        let cache = scratch("reattach-ref");
        let mut b = WdaBootstrap::new()
            .with_port(port)
            .with_cache_dir(cache.clone());
        b.ensure_running(udid).await.unwrap();
        assert_eq!(b.wda_ref(), None, "no record: unknown");
        // What a launch leaves: the ref next to the build, a marker naming it.
        let derived = cache.join("build/v16.14.1-X-arm64");
        let products = derived.join("Build/Products");
        std::fs::create_dir_all(&products).unwrap();
        std::fs::write(derived.join(BUILT_FROM), "v16.14.1 2e98ac0 mjpeg-use-ip\n").unwrap();
        let marker = b.launch_marker(udid);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(
            &marker,
            products.join("WDA.xctestrun").display().to_string(),
        )
        .unwrap();
        let mut again = WdaBootstrap::new()
            .with_port(port)
            .with_cache_dir(cache.clone());
        again.ensure_running(udid).await.unwrap();
        assert_eq!(again.wda_ref(), Some("v16.14.1"));
        assert!(crate::wda::deadline_safe_ref(again.wda_ref().unwrap()));
        // Another device on another port knows nothing of it.
        let other = WdaBootstrap::new()
            .with_port(port + 1)
            .with_cache_dir(cache.clone());
        assert_eq!(other.marked_ref(udid), None);
        std::fs::remove_dir_all(cache).unwrap();
    }

    /// fi56 (review MINOR): only fresh clones were checked against the
    /// pinned commit; a cached clone of a pinned tag is checked too, and
    /// removed (to be cloned again) when it is at another commit.
    #[tokio::test]
    async fn a_cached_clone_of_a_pinned_tag_must_be_at_the_pinned_commit() {
        let dir = scratch("cached-src");
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(dir.join("WebDriverAgent.xcodeproj")).unwrap();
        std::fs::write(dir.join("README"), "not WDA").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "x"]);
        // An unpinned ref is the user's choice: reused as is.
        assert!(reuse_cached_source(&dir, "v9.9.9").await);
        assert!(dir.is_dir());
        // The pinned tag at another commit: not reused, and removed.
        assert!(!reuse_cached_source(&dir, WDA_REF_TESTING_INTEROP).await);
        assert!(!dir.exists());
        // A cached build of a pinned ref counts only with the pinned commit.
        let derived = scratch("cached-build");
        assert!(built_from(&derived).is_none());
        std::fs::write(derived.join(BUILT_FROM), "v16.14.1 abc\n").unwrap();
        assert_eq!(
            built_from(&derived),
            Some(("v16.14.1".to_string(), "abc".to_string()))
        );
        assert!(!build_matches_pin(&derived, WDA_REF_TESTING_INTEROP));
        assert!(
            !build_matches_pin(&derived, "v9.9.9"),
            "no MJPEG word: from before the patch"
        );
        std::fs::write(derived.join(BUILT_FROM), "v16.14.1 abc mjpeg-use-ip\n").unwrap();
        assert!(build_matches_pin(&derived, "v9.9.9"));
        std::fs::write(
            derived.join(BUILT_FROM),
            "v16.14.1 2e98ac0ad4dbc99ab7c8bb01b7216fde252afb54\n",
        )
        .unwrap();
        assert!(
            !build_matches_pin(&derived, WDA_REF_TESTING_INTEROP),
            "from before the MJPEG patch: rebuilt once"
        );
        std::fs::write(
            derived.join(BUILT_FROM),
            "v16.14.1 2e98ac0ad4dbc99ab7c8bb01b7216fde252afb54 mjpeg-use-ip\n",
        )
        .unwrap();
        assert!(build_matches_pin(&derived, WDA_REF_TESTING_INTEROP));
        std::fs::write(
            derived.join(BUILT_FROM),
            "v9.9.9 abc mjpeg-all-interfaces\n",
        )
        .unwrap();
        assert!(
            build_matches_pin(&derived, "v9.9.9"),
            "unpatchable: not rebuilt each time"
        );
        std::fs::remove_dir_all(derived).unwrap();
    }

    /// The MJPEG listener gets `USE_IP`'s interface, once; a source without
    /// the line is left alone.
    #[test]
    fn the_mjpeg_listener_is_patched_to_honour_use_ip() {
        let src = scratch("mjpeg-patch");
        let file = src.join("WebDriverAgentLib/Routing/FBWebServer.m");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let upstream = "  self.screenshotsBroadcaster = [[FBTCPSocket alloc]\n                                 initWithPort:(uint16_t)FBConfiguration.sharedInstance.mjpegServerPort];\n  self.mjpegServer.socket = self.screenshotsBroadcaster;\n";
        std::fs::write(&file, upstream).unwrap();
        assert!(patch_mjpeg_bind(&src));
        let once = std::fs::read_to_string(&file).unwrap();
        assert!(once.contains(
            "mjpegServerPort];\n  // Velocity: listen where USE_IP says (loopback on a simulator), as the HTTP server does.\n  self.screenshotsBroadcaster.interface = FBConfiguration.sharedInstance.bindingIPAddress;\n  self.mjpegServer.socket"
        ), "{once}");
        assert!(patch_mjpeg_bind(&src), "already patched");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), once, "not twice");
        std::fs::write(&file, "something else\n").unwrap();
        assert!(!patch_mjpeg_bind(&src));
        std::fs::remove_dir_all(src).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_port_answering_plain_http_is_not_taken_for_a_runner() {
        // VEL-S30: any 200 on /status passed as WebDriverAgent.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        let mut b = WdaBootstrap::new().with_port(port);
        let e = b
            .ensure_running("00000000-0000-0000-0000-0000000000AB")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not as WebDriverAgent"), "{e}");
        assert!(is_runner_status(
            &serde_json::json!({"value": {"ready": true, "message": "x"}})
        ));
        assert!(is_runner_status(
            &serde_json::json!({"value": {"message": "WebDriverAgent is ready to accept commands"}})
        ));
        assert!(!is_runner_status(&serde_json::json!({"status": "ok"})));
    }

    /// A fake VelocityRunner started with `token`: 401 on any request
    /// without `Authorization: Bearer <token>`, `/status` otherwise.
    async fn fake_runner_with_token(token: &'static str) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let want = format!("authorization: bearer {}", token.to_ascii_lowercase());
                let (status, body) = if head.contains(&want) {
                    (
                        "200 OK",
                        r#"{"value":{"ready":true,"message":"VelocityRunner ready","build":{"version":"velocity-runner-x"}}}"#,
                    )
                } else {
                    (
                        "401 Unauthorized",
                        r#"{"value":{"error":"unauthorized","message":"a bearer token is required"}}"#,
                    )
                };
                let out = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes()).await;
            }
        });
        port
    }

    /// Review BLOCKER 1: a runner started with a token answers 401 to a
    /// bare `/status`, so the readiness wait and the reattach check never
    /// saw it. Every bootstrap probe now carries the bearer token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probes_carry_the_runner_token() {
        let token = "0123456789abcdef-token";
        let port = fake_runner_with_token(token).await;
        let without = WdaBootstrap::new().with_port(port).with_token(None);
        assert!(!without.is_running().await, "the fake requires the token");
        let b = WdaBootstrap::new()
            .with_port(port)
            .with_token(Some(token.into()));
        assert!(b.is_running().await, "readiness check sends the token");
        let (kind, version, runner_like) = b.running_kind().await.expect("reattach check");
        assert_eq!(kind, RunnerKind::Velocity);
        assert_eq!(version, "velocity-runner-x");
        assert!(runner_like);
    }

    /// R-1: every runner gets the token, simulators included (before:
    /// only real devices, and only with `ios.runnerToken` set).
    #[test]
    fn every_runner_gets_the_token() {
        let tok = "0123456789abcdef".to_string();
        let has_token = |env: &[(&str, String)]| {
            env.iter()
                .any(|(k, v)| *k == "TEST_RUNNER_VELOCITY_RUNNER_TOKEN" && *v == tok)
        };
        assert!(has_token(&runner_env(
            DEVICE_WDA_PORT,
            false,
            Some(tok.clone())
        )));
        assert!(
            has_token(&runner_env(8100, true, Some(tok.clone()))),
            "simulator"
        );
        assert!(!runner_env(DEVICE_WDA_PORT, true, None)
            .iter()
            .any(|(k, _)| k.ends_with("RUNNER_TOKEN")));
    }

    /// R-1: without `ios.runnerToken`, a launch makes a random 128-bit
    /// token, keeps it owner-only for a later process to reattach with,
    /// and sends it (with a device's own `Host`) on every probe.
    #[test]
    fn the_per_launch_token_is_random_private_and_reloaded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("vel-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = WdaBootstrap::new().with_cache_dir(dir.clone());
        let t1 = a.new_session_token("SIM-1").unwrap();
        assert!(
            t1.len() == 32 && t1.bytes().all(|b| b.is_ascii_hexdigit()),
            "{t1}"
        );
        let file = a.token_file("SIM-1");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // Another process reattaching reads the same token.
        let mut b = WdaBootstrap::new().with_cache_dir(dir.clone());
        b.load_session_token("SIM-1");
        if runner_token().is_none() {
            assert_eq!(b.token().as_deref(), Some(t1.as_str()));
            assert_eq!(b.auth().host, None, "a simulator's Host is the URL's");
        }
        let t2 = a.new_session_token("SIM-1").unwrap();
        assert_ne!(t1, t2, "a new launch, a new token");
        a.forget_session_token("SIM-1");
        assert!(!file.exists());
        let dev = WdaBootstrap::new().physical(true).with_token(Some(t2));
        assert_eq!(
            dev.auth().host.as_deref(),
            Some("127.0.0.1:8100"),
            "the runner's own port, not the forward's"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R-1: a runner answering 401 was started with a token this process
    /// doesn't have: taken for a VelocityRunner of no known version, so it
    /// is replaced, never reused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_with_an_unknown_token_is_replaced() {
        let port = fake_runner_with_token("someone-elses-token").await;
        let b = WdaBootstrap::new()
            .with_port(port)
            .with_token(Some("0123456789abcdef".into()));
        assert!(!b.is_running().await);
        assert!(b.answers().await);
        assert_eq!(
            b.running_kind().await,
            Some((RunnerKind::Velocity, String::new(), true))
        );
    }

    /// A runner that failed to come up 3 times within 2 minutes is not
    /// launched again: the device error names the last failure (exit 3,
    /// never retried). WDA that can't bind its port aborts at once, and
    /// was relaunched every few seconds.
    #[test]
    fn a_crash_looping_runner_is_not_relaunched() {
        let mut b = WdaBootstrap::new();
        for _ in 0..CRASH_LOOP_MAX - 1 {
            b.launch_failures.push(std::time::Instant::now());
        }
        b.check_crash_loop("SIM").unwrap();
        b.launch_failures.push(std::time::Instant::now());
        b.last_failure = Some("WDA exited before becoming ready (signal: 6)".into());
        let e = b.check_crash_loop("SIM").unwrap_err();
        assert!(matches!(e, VelocityError::RunnerFailing { .. }), "{e:?}");
        assert_eq!(e.exit_code(), 3);
        assert!(!e.is_transient());
        assert!(e.to_string().contains("signal: 6"), "{e}");
        // Old failures age out.
        b.launch_failures = vec![std::time::Instant::now() - CRASH_LOOP_WINDOW; 5];
        b.check_crash_loop("SIM").unwrap();
    }

    /// A runner is never launched into a taken port: a holder that isn't
    /// this simulator's runner is named, not killed.
    #[tokio::test]
    async fn a_port_held_by_another_process_is_reported_not_launched_into() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut b = WdaBootstrap::new().with_port(port);
        let e = b.free_port("SIM-UDID").await.unwrap_err();
        assert!(matches!(e, VelocityError::RunnerFailing { .. }), "{e:?}");
        let pid = std::process::id().to_string();
        assert!(e.to_string().contains(&format!("pid {pid}")), "{e}");
        drop(l);
        b.free_port("SIM-UDID").await.unwrap();
    }

    /// R-4: a cabled iPhone's runner binds loopback unless
    /// `VELOCITY_IOS_DEVICE_LOOPBACK=0` (it used to need `=1`).
    #[test]
    fn device_loopback_is_the_default() {
        for (v, want) in [
            (None, true),
            (Some("1"), true),
            (Some("0"), false),
            (Some("off"), false),
        ] {
            assert_eq!(loopback_setting(v), want, "{v:?}");
        }
    }

    #[test]
    fn a_cabled_runner_without_a_token_warns_that_it_listens_everywhere() {
        let w = open_runner_warning(false, false, false, true).expect("warns");
        assert!(
            w.contains("all of its interfaces") && w.contains("VELOCITY_IOS_DEVICE_LOOPBACK"),
            "{w}"
        );
        assert!(open_runner_warning(false, false, true, false)
            .is_some_and(|w| w.contains("WebDriverAgent checks no token")));
        assert_eq!(open_runner_warning(false, false, true, true), None);
        assert_eq!(
            open_runner_warning(false, true, false, true),
            None,
            "loopback"
        );
        assert_eq!(
            open_runner_warning(true, false, true, true),
            None,
            "wifi path"
        );
    }

    #[test]
    fn startup_cause_matches_known_failures() {
        assert_eq!(
            startup_cause("Running tests... timed out while enabling automation mode."),
            Some(
                "the iPhone did not allow UI automation: on the phone turn on Settings \
                 → Developer → Enable UI Automation, unlock it, and enter the passcode \
                 if asked; then run again"
            )
        );
        assert_eq!(
            startup_cause("Failed to initialize for UI testing: the device said no"),
            Some(
                "the iPhone did not allow UI automation: on the phone turn on Settings \
                 → Developer → Enable UI Automation, unlock it, and enter the passcode \
                 if asked; then run again"
            )
        );
        assert_eq!(
            startup_cause("Developer Mode is disabled on this device"),
            Some(
                "Developer Mode is off on the device: Settings → Privacy & Security \
                  → Developer Mode"
            )
        );
        assert_eq!(
            startup_cause("invalid code signature for the runner"),
            Some(
                "the device does not trust the runner's signing certificate: \
                 Settings → General → VPN & Device Management → trust it"
            )
        );
        assert_eq!(
            startup_cause("the app has not been explicitly trusted"),
            Some(
                "the device does not trust the runner's signing certificate: \
                 Settings → General → VPN & Device Management → trust it"
            )
        );
        assert_eq!(
            startup_cause("Untrusted Developer: tap to trust"),
            Some(
                "the device does not trust the runner's signing certificate: \
                 Settings → General → VPN & Device Management → trust it"
            )
        );
        assert_eq!(
            startup_cause("the device is locked"),
            Some("the device is locked: unlock it and keep the screen on")
        );
        assert_eq!(
            startup_cause("passcode protected, cannot start"),
            Some("the device is locked: unlock it and keep the screen on")
        );
        // case-insensitive
        assert_eq!(
            startup_cause("TIMED OUT WHILE ENABLING AUTOMATION MODE"),
            Some(
                "the iPhone did not allow UI automation: on the phone turn on Settings \
                 → Developer → Enable UI Automation, unlock it, and enter the passcode \
                 if asked; then run again"
            )
        );
    }

    #[test]
    fn startup_cause_ignores_ordinary_lines() {
        assert_eq!(startup_cause("WebDriverAgent started on port 8100"), None);
        assert_eq!(startup_cause("Running tests..."), None);
        assert_eq!(startup_cause("ServerURLHere: http://127.0.0.1:8100"), None);
        assert_eq!(
            startup_cause("something about developer mode on this phone"),
            None
        );
        assert_eq!(startup_cause("unrelated: screen locked? no"), None);
    }

    /// A fake runner that answers `/status` to anyone (started without a
    /// token).
    async fn fake_open_runner() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await;
                let body = r#"{"value":{"ready":true,"message":"VelocityRunner ready","build":{"version":"velocity-runner-x"}}}"#;
                let out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes()).await;
            }
        });
        port
    }

    /// review-fixes #2: reattaching to a runner on a Wi-Fi-only iPhone runs
    /// the Wi-Fi policy and refuses a runner that answers without the
    /// token (one kept from a cabled run); a cabled phone reattaches as
    /// before. Before, `ensure_running` reused any runner that answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reattach_over_wifi_needs_the_policy_and_a_runner_that_checks_the_token() {
        let token = "0123456789abcdef-token";
        let udid = "00008130-001451402EC1001C";
        let open = WdaBootstrap::new()
            .with_port(fake_open_runner().await)
            .with_token(Some(token.into()));
        assert!(open.is_running().await);
        open.check_reattach(udid, Some(true), false, true)
            .await
            .expect("cabled: reused as before");
        let e = open
            .check_reattach(udid, Some(false), true, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to reuse"), "{e}");
        let e = open
            .check_reattach(udid, Some(false), false, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("connected over Wi-Fi only"), "{e}");
        let guarded = WdaBootstrap::new()
            .with_port(fake_runner_with_token(token).await)
            .with_token(Some(token.into()));
        guarded
            .check_reattach(udid, Some(false), true, true)
            .await
            .expect("a runner that checks the token is reused");
        let e = guarded
            .check_reattach(udid, Some(false), true, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("ios.runner: velocity"), "{e}");
    }
}
