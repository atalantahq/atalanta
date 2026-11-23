//! The Atalanta inspector and console: an axum server with a web UI and
//! API for looking at a device's screen and element tree, building
//! selectors, running and exploring tests as `atalanta` child processes
//! (`console`), and reviewing failed `assertScreenshot` baselines
//! (`/review`). Internal to the `atalanta` CLI (`atalanta inspect`,
//! `atalanta baselines review`); its API has no stability promise.

pub mod console;
pub mod embedded;
pub mod no_device;
pub mod routes;
pub mod security;
pub mod selector_gen;
pub mod server;
pub mod snapshot;
pub mod state;
pub mod ws;
pub mod yaml_gen;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use atalanta_common::PlatformDriver;
use tokio::net::TcpListener;

use security::HostPolicy;
use state::AppState;

pub struct InspectorServer {
    state: AppState,
}

impl InspectorServer {
    pub fn new(
        driver: Arc<dyn PlatformDriver>,
        device_id: Option<String>,
        app_id: Option<String>,
    ) -> Self {
        Self {
            state: AppState::new(driver, device_id, app_id),
        }
    }

    /// The per-session token `/api` requests must carry.
    pub fn token(&self) -> &str {
        &self.state.token
    }

    /// The URL to open the UI with: it carries the token, which the server
    /// turns into a session cookie.
    pub fn url(&self, addr: SocketAddr) -> String {
        let host = match addr.ip() {
            ip if ip.is_loopback() || ip.is_unspecified() => "localhost".to_string(),
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        format!("http://{host}:{}/?token={}", addr.port(), self.state.token)
    }

    /// An inspector with no device, serving only the screenshot review
    /// page for `results_dir` (`atalanta baselines review --serve`).
    pub fn review_only(results_dir: impl Into<PathBuf>) -> Self {
        let mut s =
            Self::new(Arc::new(no_device::NoDevice), None, None).with_results_dir(results_dir);
        s.state.review_only = true;
        s
    }

    /// The screenshot review page's URL (with the token, like [`Self::url`]).
    pub fn review_url(&self, addr: SocketAddr) -> String {
        self.url(addr)
            .replacen("/?token=", &format!("{}?token=", server::REVIEW_PATH), 1)
    }

    /// Where the screenshot review page finds failed `assertScreenshot`
    /// comparisons (default `atalanta-results`).
    pub fn with_results_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state.results_dir = dir.into();
        self
    }

    /// Make the review page's Accept write into this `atalanta-baselines`
    /// folder rather than the absolute path the run recorded (CI artifacts).
    pub fn with_baselines_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.state.baselines_dir = dir;
        self
    }

    /// The console's hooks: live video, leases, diagnostics, explore, the
    /// suite launcher and the project folder (see [`console`]).
    pub fn with_console(mut self, hooks: console::ConsoleHooks) -> Self {
        self.state.console = hooks;
        self
    }

    /// `device_id` was already prepared by the caller (WDA started).
    pub fn with_prepared(mut self, device_id: &str) -> Self {
        self.state.prepared.get_mut().insert(device_id.to_string());
        self
    }

    /// Directory recorded flows are saved under (default `./flows`).
    pub fn with_flows_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state.flows_dir = Some(dir.into());
        self
    }

    /// Bind to the loopback interface on `port`.
    pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
        Self::bind_on(IpAddr::V4(Ipv4Addr::LOCALHOST), port).await
    }

    /// Bind to `ip:port`. Anything but loopback exposes the device to the
    /// network (still behind the token), so callers should make it opt-in.
    ///
    /// A port in use, or an address this host can't bind, is a config error
    /// (exit 2), not an internal one (VEL-171).
    pub async fn bind_on(ip: IpAddr, port: u16) -> anyhow::Result<TcpListener> {
        let addr = SocketAddr::new(ip, port);
        TcpListener::bind(addr).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse
            | std::io::ErrorKind::AddrNotAvailable
            | std::io::ErrorKind::PermissionDenied => {
                anyhow::Error::new(atalanta_common::AtalantaError::Config(format!(
                    "Can't listen on {addr}: {e}; pick another --port or --host"
                )))
            }
            _ => e.into(),
        })
    }

    pub async fn start(self, port: u16) -> anyhow::Result<()> {
        let listener = Self::bind(port).await?;

        self.start_with_listener(listener).await
    }

    pub async fn start_with_listener(self, listener: TcpListener) -> anyhow::Result<()> {
        self.start_with_listener_until(listener, std::future::pending())
            .await
    }

    /// Serve until `shutdown` resolves, then stop gracefully: stop
    /// accepting, Ctrl-C the console's running tasks (and cancel the queued
    /// ones), wait up to [`SHUTDOWN_GRACE`] for their children to end, and
    /// release the console's device leases. Children still alive after that
    /// are killed with their process groups as the runtime drops.
    pub async fn start_with_listener_until(
        mut self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> anyhow::Result<()> {
        let addr = listener.local_addr()?;
        self.state.host_policy = HostPolicy::for_bind_addr(addr.ip());
        let state = Arc::new(self.state);
        let router = server::build_router(state.clone());

        tracing::info!("Inspector server listening on {addr}");

        let served = tokio::select! {
            r = axum::serve(listener, router) => Some(r),
            _ = shutdown => None,
        };
        // The listener went with the serve future: nothing new arrives.
        if served.is_none() {
            tracing::info!("Inspector shutting down");
        }
        if !state.tasks.shutdown(SHUTDOWN_GRACE).await {
            tracing::warn!("console tasks still running after {SHUTDOWN_GRACE:?}; killing them");
            atalanta_common::process::kill_all_groups();
        }
        state.control.release_all().await;
        if let Some(r) = served {
            r?;
        }
        Ok(())
    }
}

/// How long a shutdown waits for console tasks to stop after Ctrl-C.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(25);
