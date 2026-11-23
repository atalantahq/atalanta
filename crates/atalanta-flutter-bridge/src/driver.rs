// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::HashMap;

use atalanta_common::selector_match::{find_all_in_tree, find_in_tree};
use atalanta_common::{
    AtalantaError, DeviceInfo, DeviceState, DeviceType, Direction, Element, HealthStatus, Key,
    Platform, PlatformDriver, Rect, Selector,
};
use base64::Engine;
use tokio::sync::RwLock;

use crate::config::{FlutterBridgeConfig, HierarchyMode};
use crate::process::FlutterProcess;
use crate::protocol::{node_key, FlutterCommand, FlutterResponse, Hierarchy, NodeKey, Target};

const DEVICE: &str = "flutter-0";

/// Flutter platform driver that communicates with a Dart test process
/// running `flutter test --machine` + WidgetTester.
pub struct FlutterDriver {
    /// What each device's last successful lookup counted on screen (R5).
    match_counts: atalanta_common::selector_match::MatchCounts,
    platform: Platform,
    config: FlutterBridgeConfig,
    process: RwLock<FlutterProcess>,
    hierarchy_cache: RwLock<Option<Element>>,
    /// Node ids from the last hierarchy fetch, so an action can name the
    /// element it was given. Kept across cache invalidation: the Dart side
    /// checks a node is still mounted and falls back to the id and point.
    node_ids: RwLock<(HashMap<NodeKey, i64>, Option<u64>)>,
    screenshot_cache: RwLock<Option<Vec<u8>>>,
    /// The logical surface size the bridge reported on `init` (VEL-S39).
    surface: RwLock<Option<(i32, i32)>>,
    /// A hash of the tree the last settle saw, to tell whether the next
    /// action changed the screen.
    settled_tree: std::sync::Mutex<Option<u64>>,
}

impl FlutterDriver {
    /// Create a new Flutter driver.
    pub fn new(platform: Platform, config: FlutterBridgeConfig) -> Self {
        let process = FlutterProcess::new(config.clone());
        Self {
            platform,
            match_counts: Default::default(),
            config,
            process: RwLock::new(process),
            hierarchy_cache: RwLock::new(None),
            node_ids: RwLock::new((HashMap::new(), None)),
            screenshot_cache: RwLock::new(None),
            surface: RwLock::new(None),
            settled_tree: std::sync::Mutex::new(None),
        }
    }

    /// The screen actions and hierarchies use: what the bridge reported,
    /// else the configured size (an older bridge).
    async fn surface(&self) -> (i32, i32) {
        self.surface
            .read()
            .await
            .unwrap_or((self.config.width as i32, self.config.height as i32))
    }

    async fn screen_rect(&self) -> Rect {
        let (width, height) = self.surface().await;
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Send a command to the Flutter test process.
    async fn send_cmd(&self, cmd: &FlutterCommand) -> atalanta_common::Result<FlutterResponse> {
        let mut process = self.process.write().await;
        // A timed-out command drops the connection; open a fresh one.
        if !process.is_running() {
            process.start().await.map_err(|e| {
                AtalantaError::Internal(anyhow::anyhow!("Flutter bridge reconnect failed: {e}"))
            })?;
        }
        process.send(cmd).await.map_err(|e| {
            AtalantaError::Internal(anyhow::anyhow!("Flutter bridge command failed: {}", e))
        })
    }

    /// Send a command; an error reply fails it as `op`. Two codes are about
    /// the request, not a device command that ran and failed: `unsupported`
    /// (a key the bridge can't send) is a platform limit, exit 2 and never
    /// retried, and `unknown_command` (an older Dart package) says what to
    /// update. The bridge's message is redacted like a command's output.
    async fn call(
        &self,
        cmd: &FlutterCommand,
        op: &str,
    ) -> atalanta_common::Result<Option<serde_json::Value>> {
        match self.send_cmd(cmd).await? {
            FlutterResponse::Error {
                message,
                code: Some(code),
            } if code == "unsupported" => Err(AtalantaError::Unsupported(format!(
                "{op}: {} (Flutter headless)",
                atalanta_common::redact::redact(&message)
            ))),
            FlutterResponse::Error {
                code: Some(code), ..
            } if code == "unknown_command" => Err(AtalantaError::Config(format!(
                "the Flutter bridge doesn't know `{op}`: update the atalanta_flutter_helper \
                 Dart package (crates/atalanta-flutter-bridge/dart) to this Atalanta's version"
            ))),
            reply => reply.into_result(op, DEVICE),
        }
    }

    /// Run an action. The Dart side settles (bounded) before it answers, so
    /// no extra pump follows (VEL-280). The UI may have changed even when
    /// the action failed, so the caches go either way.
    async fn act(&self, cmd: &FlutterCommand, op: &str) -> atalanta_common::Result<()> {
        let result = self.call(cmd, op).await;
        self.invalidate_cache().await;
        result.map(|_| ())
    }

    /// The target sent with an element action.
    async fn target(&self, element: &Element) -> Target {
        let (x, y) = element.bounds.center();
        let ids = self.node_ids.read().await;
        let node = ids.0.get(&node_key(element)).copied();
        Target {
            node,
            id: (!element.platform_id.is_empty()).then(|| element.platform_id.clone()),
            generation: node.and(ids.1),
            x,
            y,
        }
    }

    /// Invalidate cached hierarchy and screenshot so the next query fetches fresh data.
    async fn invalidate_cache(&self) {
        *self.hierarchy_cache.write().await = None;
        *self.screenshot_cache.write().await = None;
    }

    /// Mount the app afresh (`clear` also runs the app's reset hook).
    async fn init(&self, clear: bool) -> atalanta_common::Result<()> {
        let data = self
            .call(
                &FlutterCommand::Init {
                    target: self.config.target.clone(),
                    width: self.config.width,
                    height: self.config.height,
                    clear,
                    dpr: self.config.device_pixel_ratio,
                },
                "init",
            )
            .await?;
        if let Some(p) = data.as_ref().and_then(|d| d.get("protocol")) {
            tracing::debug!("Flutter bridge protocol {p}");
        }
        let size = data.as_ref().and_then(|d| d.get("size")).and_then(|s| {
            let dim = |k| s.get(k)?.as_i64().and_then(|v| i32::try_from(v).ok());
            Some((dim("width")?, dim("height")?)).filter(|(w, h)| *w > 0 && *h > 0)
        });
        if let Some((w, h)) = size {
            tracing::debug!("Flutter test surface {w}x{h}");
        }
        *self.surface.write().await = size;
        self.invalidate_cache().await;
        Ok(())
    }

    /// Fetch and cache the hierarchy from the Flutter process.
    async fn fetch_hierarchy(&self) -> atalanta_common::Result<Element> {
        {
            let cache = self.hierarchy_cache.read().await;
            if let Some(ref cached) = *cache {
                return Ok(cached.clone());
            }
        }

        let mode = match self.config.hierarchy_mode() {
            HierarchyMode::Widgets => None,
            HierarchyMode::Semantics => Some("semantics".to_string()),
        };
        let value = self
            .call(&FlutterCommand::GetHierarchy { mode }, "get_hierarchy")
            .await?
            .ok_or_else(|| {
                AtalantaError::Internal(anyhow::anyhow!("GetHierarchy returned no data"))
            })?;
        let hierarchy = match Hierarchy::from_value(value) {
            Ok(h) => h,
            Err(e) => {
                // The Dart side has already replaced its snapshot: node ids
                // from the previous one could name other elements now.
                *self.node_ids.write().await = (HashMap::new(), None);
                return Err(AtalantaError::Internal(anyhow::anyhow!(
                    "Failed to parse hierarchy: {}",
                    e
                )));
            }
        };
        *self.node_ids.write().await = (hierarchy.nodes, hierarchy.generation);
        *self.hierarchy_cache.write().await = Some(hierarchy.root.clone());
        Ok(hierarchy.root)
    }

    /// Fetch and cache a screenshot from the Flutter process.
    async fn fetch_screenshot(&self) -> atalanta_common::Result<Vec<u8>> {
        {
            let cache = self.screenshot_cache.read().await;
            if let Some(ref cached) = *cache {
                return Ok(cached.clone());
            }
        }

        let value = self
            .call(&FlutterCommand::Screenshot, "screenshot")
            .await?
            .ok_or_else(|| {
                AtalantaError::Internal(anyhow::anyhow!("Screenshot returned no data"))
            })?;
        let b64 = value.as_str().ok_or_else(|| {
            AtalantaError::Internal(anyhow::anyhow!("Screenshot data is not a string"))
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| {
                AtalantaError::Internal(anyhow::anyhow!(
                    "Failed to decode screenshot base64: {}",
                    e
                ))
            })?;
        *self.screenshot_cache.write().await = Some(bytes.clone());
        Ok(bytes)
    }
}

// --- PlatformDriver implementation ---

#[async_trait::async_trait]
impl PlatformDriver for FlutterDriver {
    async fn invalidate_hierarchy_cache(&self) {
        self.invalidate_cache().await;
    }

    /// The Dart side settles every action before it answers and nothing
    /// moves between commands (virtual time only advances when it pumps),
    /// so polling for a change after an action could only wait out the
    /// reaction window. One read (cached for the sync engine's own) tells
    /// whether the action changed the screen.
    async fn settle_on_device(
        &self,
        _device_id: &str,
        _reaction_ms: u64,
        _quiet_ms: u64,
        _cap_ms: u64,
    ) -> Option<atalanta_common::DeviceSettle> {
        use std::hash::{Hash, Hasher};
        let tree = self.fetch_hierarchy().await.ok()?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_vec(&tree).ok()?.hash(&mut hasher);
        let hash = hasher.finish();
        let before = self
            .settled_tree
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(hash);
        Some(atalanta_common::DeviceSettle {
            settled: true,
            reacted: before != Some(hash),
            elapsed_ms: 0,
        })
    }

    async fn prepare(&self, _device_id: &str) -> atalanta_common::Result<()> {
        let mut process = self.process.write().await;
        process.start().await.map_err(|e| {
            AtalantaError::Internal(anyhow::anyhow!("Flutter process start failed: {}", e))
        })?;

        drop(process);

        // Boot the app widget.
        self.init(false).await?;
        tracing::info!("Flutter bridge initialized");
        Ok(())
    }

    async fn cleanup(&self) {
        let mut process = self.process.write().await;
        process.stop().await;
    }

    async fn health_check(&self) -> HealthStatus {
        let process = self.process.read().await;
        if process.is_running() {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
        }
    }

    async fn restart_session(&self) -> atalanta_common::Result<()> {
        self.cleanup().await;
        self.invalidate_cache().await;
        self.prepare(DEVICE).await
    }

    async fn list_devices(&self) -> atalanta_common::Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo {
            id: DEVICE.to_string(),
            name: format!(
                "Flutter Headless ({}x{})",
                self.config.width, self.config.height
            ),
            platform: self.platform,
            state: DeviceState::Booted,
            os_version: Some("flutter".to_string()),
            device_type: DeviceType::Simulator,
        }])
    }

    async fn boot_device(&self, device_id: &str) -> atalanta_common::Result<()> {
        self.prepare(device_id).await
    }

    async fn shutdown_device(&self, _device_id: &str) -> atalanta_common::Result<()> {
        self.cleanup().await;
        Ok(())
    }

    async fn install_app(&self, _device_id: &str, _app_path: &str) -> atalanta_common::Result<()> {
        // Flutter apps are loaded via the test harness target, no separate install step.
        Ok(())
    }

    async fn launch_app(
        &self,
        device_id: &str,
        _app_id: &str,
        clear_state: bool,
    ) -> atalanta_common::Result<()> {
        let process = self.process.read().await;
        if !process.is_running() {
            drop(process);
            self.prepare(device_id).await?;
            if !clear_state {
                return Ok(());
            }
        } else {
            drop(process);
        }
        // A launch starts the app afresh; clearState also resets its state
        // (VEL-141).
        self.init(clear_state).await
    }

    async fn stop_app(&self, _device_id: &str, _app_id: &str) -> atalanta_common::Result<()> {
        // Unmount the app but keep the bridge serving, so the next launch
        // works (VEL-140).
        self.act(&FlutterCommand::Reset, "stop_app").await
    }

    async fn find_element(
        &self,
        device_id: &str,
        selector: &Selector,
    ) -> atalanta_common::Result<Element> {
        let hierarchy = self.fetch_hierarchy().await?;
        let screen = self.screen_rect().await;

        let found = find_in_tree(&hierarchy, selector, Some(&screen));
        if found.is_some() {
            let n = atalanta_common::selector_match::ambiguity(&hierarchy, selector, Some(&screen));
            self.match_counts.record(device_id, selector, n);
        } else {
            self.match_counts.clear(device_id);
        }
        found.ok_or_else(|| AtalantaError::ElementNotFound {
            selector: format!("{:?}", selector),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        })
    }

    fn last_match_count(&self, device_id: &str, selector: &Selector) -> Option<usize> {
        self.match_counts.get(device_id, selector)
    }

    async fn find_elements(
        &self,
        _device_id: &str,
        selector: &Selector,
    ) -> atalanta_common::Result<Vec<Element>> {
        let hierarchy = self.fetch_hierarchy().await?;
        let screen = self.screen_rect().await;

        let mut results = Vec::new();
        find_all_in_tree(&hierarchy, selector, Some(&screen), &mut results);
        Ok(results)
    }

    async fn get_hierarchy(&self, _device_id: &str) -> atalanta_common::Result<Element> {
        self.fetch_hierarchy().await
    }

    async fn tap(&self, _device_id: &str, element: &Element) -> atalanta_common::Result<()> {
        let target = self.target(element).await;
        let cmd = FlutterCommand::Tap {
            x: target.x,
            y: target.y,
            target: Some(target),
        };
        self.act(&cmd, "tap").await
    }

    async fn double_tap(&self, _device_id: &str, element: &Element) -> atalanta_common::Result<()> {
        let target = self.target(element).await;
        let cmd = FlutterCommand::DoubleTap {
            x: target.x,
            y: target.y,
            target: Some(target),
        };
        self.act(&cmd, "double_tap").await
    }

    async fn long_press(
        &self,
        _device_id: &str,
        element: &Element,
        duration_ms: u64,
    ) -> atalanta_common::Result<()> {
        let target = self.target(element).await;
        let cmd = FlutterCommand::LongPress {
            x: target.x,
            y: target.y,
            duration_ms,
            target: Some(target),
        };
        self.act(&cmd, "long_press").await
    }

    async fn input_text(
        &self,
        _device_id: &str,
        element: &Element,
        text: &str,
    ) -> atalanta_common::Result<()> {
        // The Dart side types into the text field under the target, never
        // the first one on screen (VEL-024).
        let cmd = FlutterCommand::InputText {
            text: text.to_string(),
            target: Some(self.target(element).await),
        };
        self.act(&cmd, "input_text").await
    }

    async fn clear_text(&self, _device_id: &str, element: &Element) -> atalanta_common::Result<()> {
        let cmd = FlutterCommand::ClearText {
            target: Some(self.target(element).await),
        };
        self.act(&cmd, "clear_text").await
    }

    async fn swipe(&self, _device_id: &str, direction: Direction) -> atalanta_common::Result<()> {
        let (width, height) = self.surface().await;
        let mid_x = width / 2;
        let mid_y = height / 2;
        let distance = height / 3;

        let (from_x, from_y, to_x, to_y) = match direction {
            Direction::Up => (mid_x, mid_y + distance / 2, mid_x, mid_y - distance / 2),
            Direction::Down => (mid_x, mid_y - distance / 2, mid_x, mid_y + distance / 2),
            Direction::Left => (mid_x + distance / 2, mid_y, mid_x - distance / 2, mid_y),
            Direction::Right => (mid_x - distance / 2, mid_y, mid_x + distance / 2, mid_y),
        };

        let cmd = FlutterCommand::Swipe {
            from_x,
            from_y,
            to_x,
            to_y,
        };
        self.act(&cmd, "swipe").await
    }

    async fn swipe_coords(
        &self,
        _device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
    ) -> atalanta_common::Result<()> {
        let cmd = FlutterCommand::Swipe {
            from_x: from.0,
            from_y: from.1,
            to_x: to.0,
            to_y: to.1,
        };
        self.act(&cmd, "swipe").await
    }

    async fn press_key(&self, _device_id: &str, key: Key) -> atalanta_common::Result<()> {
        // The sidecar answers `unsupported` for a key it can't send.
        let cmd = FlutterCommand::PressKey {
            key: key.name().to_string(),
        };
        self.act(&cmd, "press_key").await
    }

    async fn screenshot(&self, _device_id: &str) -> atalanta_common::Result<Vec<u8>> {
        self.fetch_screenshot().await
    }

    async fn screen_size(&self, _device_id: &str) -> atalanta_common::Result<(i32, i32)> {
        Ok(self.surface().await)
    }

    async fn get_element_text(
        &self,
        _device_id: &str,
        element: &Element,
    ) -> atalanta_common::Result<String> {
        Ok(element.text.clone().unwrap_or_default())
    }

    async fn is_element_visible(
        &self,
        _device_id: &str,
        element: &Element,
    ) -> atalanta_common::Result<bool> {
        // The shared rule (D-1): rendered, a box, on the surface.
        let shown = atalanta_common::selector_match::shown_on(element, &self.screen_rect().await);
        if !shown || !self.config.hit_test_visibility() {
            return Ok(shown);
        }
        // Opt-in: visible means a tap at its centre would reach it.
        let target = self.target(element).await;
        let data = self
            .call(&FlutterCommand::HitTest { target }, "hit_test")
            .await?;
        Ok(data
            .and_then(|d| d.get("hittable").and_then(|h| h.as_bool()))
            .unwrap_or(true))
    }

    async fn collect_resource_metrics(
        &self,
        _device_id: &str,
        _package: &str,
    ) -> atalanta_common::Result<(u64, u64, u64, f32)> {
        Err(AtalantaError::Config(
            "Resource profiling not available on Flutter headless bridge".into(),
        ))
    }
}
