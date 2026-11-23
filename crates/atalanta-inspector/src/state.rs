// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atalanta_common::PlatformDriver;
use tokio::sync::{Notify, RwLock};

use crate::console::control::Control;
use crate::console::live::Hubs;
use crate::console::tasks::Tasks;
use crate::console::ConsoleHooks;
use crate::security::{self, HostPolicy};
use crate::snapshot::Snapshot;

/// A pushed screenshot: its content hash and PNG bytes.
pub type CachedScreenshot = (u64, Arc<Vec<u8>>);

/// Shared application state for the inspector server.
pub struct AppState {
    pub driver: Arc<dyn PlatformDriver>,
    pub current_device: RwLock<Option<String>>,
    /// App package ID for resource profiling (set via API when known).
    pub app_id: RwLock<Option<String>>,
    /// Per-session secret every `/api` request must present.
    pub token: String,
    /// Which `Host` headers the server answers to.
    pub host_policy: HostPolicy,
    /// `/api/flow/save` only creates files inside this directory; `None`
    /// means `flows/` under the working directory.
    pub flows_dir: Option<PathBuf>,
    /// Where the screenshot review page looks for failed `assertScreenshot`
    /// comparisons (default `atalanta-results`).
    pub results_dir: PathBuf,
    /// The page's Accept writes into this `atalanta-baselines` folder (same
    /// `<platform>/<W>x<H>/<file>` inside) instead of the path the run
    /// recorded: for results downloaded from a CI runner.
    pub baselines_dir: Option<PathBuf>,
    /// `atalanta baselines review --serve`: only `/review` and
    /// `/api/baselines*` are routed. The review URL is meant to be shared,
    /// and it used to reach the whole console (start runs, save flows).
    pub review_only: bool,
    /// The hierarchy each device's page was last sent, by device id.
    pub snapshots: RwLock<HashMap<String, Arc<Snapshot>>>,
    /// The last screenshot pushed per device: (content hash, PNG bytes).
    pub screenshots: RwLock<HashMap<String, CachedScreenshot>>,
    snapshot_version: AtomicU64,
    /// Woken by every device action, so the live view stops backing off.
    pub activity: Notify,
    /// The console's pluggable parts (live video, leases, diagnostics,
    /// explore, the suite launcher, the project folder).
    pub console: ConsoleHooks,
    /// The device leases this console holds.
    pub control: Control,
    /// The task queue (suite runs, explore sessions).
    pub tasks: Tasks,
    /// Shared pushed/polled mirror frames per device.
    pub hubs: Hubs,
    /// Devices `prepare`d by this server (WDA started, browser launched).
    pub prepared: tokio::sync::Mutex<std::collections::HashSet<String>>,
}

impl AppState {
    pub fn new(
        driver: Arc<dyn PlatformDriver>,
        device_id: Option<String>,
        app_id: Option<String>,
    ) -> Self {
        Self {
            driver,
            current_device: RwLock::new(device_id),
            app_id: RwLock::new(app_id),
            token: security::new_token(),
            host_policy: HostPolicy::default(),
            flows_dir: None,
            results_dir: PathBuf::from("atalanta-results"),
            baselines_dir: None,
            review_only: false,
            snapshots: RwLock::new(HashMap::new()),
            screenshots: RwLock::new(HashMap::new()),
            snapshot_version: AtomicU64::new(0),
            activity: Notify::new(),
            console: ConsoleHooks::default(),
            control: Control::default(),
            tasks: Tasks::default(),
            hubs: Hubs::default(),
            prepared: Default::default(),
        }
    }

    /// Store `tree` as `device_id`'s current snapshot under a fresh version.
    /// The same tree (by `hash`) as the device's current snapshot keeps that
    /// snapshot and its version: versions belong to the server, not to each
    /// socket, so two tabs (or a reconnect) don't take the version from each
    /// other and get 409 forever (review m13).
    pub async fn publish_snapshot(
        &self,
        device_id: &str,
        tree: atalanta_common::types::Element,
        hash: u64,
    ) -> Arc<Snapshot> {
        let mut snapshots = self.snapshots.write().await;
        if let Some(current) = snapshots.get(device_id).filter(|s| s.hash == hash) {
            return current.clone();
        }
        let version = self.snapshot_version.fetch_add(1, Ordering::Relaxed) + 1;
        let snap = Arc::new(Snapshot {
            version,
            device_id: device_id.to_string(),
            tree,
            hash,
        });
        snapshots.insert(device_id.to_string(), snap.clone());
        snap
    }

    /// The snapshot sent as `version`, if it is still a device's current one.
    pub async fn snapshot(&self, version: u64) -> Option<Arc<Snapshot>> {
        self.snapshots
            .read()
            .await
            .values()
            .find(|s| s.version == version)
            .cloned()
    }

    /// Something was done to a device: refresh the live view now.
    pub fn poke(&self) {
        self.activity.notify_waiters();
    }

    pub async fn device_id(&self) -> Option<String> {
        self.current_device.read().await.clone()
    }

    pub async fn require_device_id(&self) -> anyhow::Result<String> {
        self.device_id()
            .await
            .ok_or_else(|| anyhow::anyhow!("No device selected"))
    }

    pub async fn app_id(&self) -> Option<String> {
        self.app_id.read().await.clone()
    }

    pub async fn set_app_id(&self, app_id: Option<String>) {
        *self.app_id.write().await = app_id;
    }

    pub async fn clear_app_id(&self) {
        self.set_app_id(None).await;
    }
}
