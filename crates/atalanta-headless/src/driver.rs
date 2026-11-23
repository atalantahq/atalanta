use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use atalanta_common::selector_match::{find_all_in_tree, find_in_tree};
use atalanta_common::{
    AtalantaError, DeviceInfo, Direction, Element, HealthStatus, Key, Platform, PlatformDriver,
    Rect, Selector,
};
use tokio::sync::RwLock;

use crate::android::inflate::AndroidInflater;
use crate::android::resources::ResourceTable;
use crate::config::HeadlessConfig;
use crate::ios::inflate::IosInflater;
use crate::ios::resources::IosResourceLoader;
use crate::ios::xib::XibParser;
use crate::session::HeadlessSession;
use crate::text::TextMeasurer;

/// The lower-cased extension of `path`, so `LAYOUT.XML` loads like `layout.xml`.
fn lower_ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// This process's headless device. Per process, so two headless runs on one
/// machine don't contend for one device lease (VEL-052).
pub fn headless_device_id() -> String {
    format!("headless-{}", std::process::id())
}

/// The error for a lookup before any `launchApp`: a config error that is not
/// retried, instead of 30 s of device-failure retries (VEL-255).
fn not_launched() -> AtalantaError {
    AtalantaError::Config("No app launched on the headless device; add a launchApp step".into())
}

/// A layout that can't be parsed or inflated is the user's input being wrong,
/// a config error (exit 2), not an internal one (exit 5).
fn android_inflate_error(e: crate::android::inflate::InflateError) -> AtalantaError {
    AtalantaError::Config(format!("Layout inflate failed: {e}"))
}

/// Log what the layout holds that headless renders empty (N7).
fn warn_all(inflater: &AndroidInflater) {
    for w in inflater.take_warnings() {
        tracing::warn!("{w}");
    }
}

fn ios_inflate_error(e: crate::ios::inflate::IosInflateError) -> AtalantaError {
    AtalantaError::Config(format!("XIB inflate failed: {e}"))
}

/// Headless rendering driver implementing PlatformDriver.
///
/// Renders Android XML layouts and iOS XIB/Storyboard files to pixels
/// without requiring an emulator or simulator. Uses CPU-only rendering
/// via tiny-skia and Taffy flexbox layout.
pub struct HeadlessDriver {
    /// What each device's last successful lookup counted on screen (R5).
    match_counts: atalanta_common::selector_match::MatchCounts,
    platform: Platform,
    config: HeadlessConfig,
    sessions: RwLock<HashMap<String, HeadlessSession>>,
    text_measurer: Arc<TextMeasurer>,
    /// Attributes already warned about as not drawn (D-47): once each.
    unrendered: crate::unrendered::Warned,
}

impl HeadlessDriver {
    /// Create a new headless driver for the given platform.
    pub fn new(platform: Platform, config: HeadlessConfig) -> Self {
        Self {
            platform,
            match_counts: Default::default(),
            config,
            sessions: RwLock::new(HashMap::new()),
            text_measurer: Arc::new(TextMeasurer::new()),
            unrendered: Default::default(),
        }
    }

    /// Load and inflate a layout for the given app.
    async fn inflate_layout(
        &self,
        session: &mut HeadlessSession,
        _app_id: &str,
    ) -> Result<(), AtalantaError> {
        let app_path: Option<String> = session
            .app_path
            .clone()
            .or_else(|| self.config.app_path.clone());

        let initial_layout: Option<String> = self.config.initial_layout.clone();

        match self.platform {
            Platform::Android => {
                self.inflate_android(session, app_path.as_deref(), initial_layout.as_deref())
                    .await
            }
            Platform::Ios => {
                self.inflate_ios(session, app_path.as_deref(), initial_layout.as_deref())
                    .await
            }
            Platform::Web => Err(AtalantaError::Config(
                "HeadlessDriver does not support web platform; use atalanta-web instead".into(),
            )),
        }
    }

    async fn inflate_android(
        &self,
        session: &mut HeadlessSession,
        app_path: Option<&str>,
        initial_layout: Option<&str>,
    ) -> Result<(), AtalantaError> {
        // If we have an APK, extract and inflate from it
        if let Some(apk_path) = app_path {
            let path = Path::new(apk_path);
            if lower_ext(path).as_deref() == Some("apk") {
                let loader = crate::android::apk::ApkLoader::from_path(path)
                    .map_err(|e| AtalantaError::Config(format!("APK load failed: {e}")))?;

                let layout_name = initial_layout.unwrap_or("activity_main");
                let layout_data = loader.get_layout(layout_name).ok_or_else(|| {
                    AtalantaError::Config(format!("Layout '{}' not found in APK", layout_name))
                })?;

                let inflater = AndroidInflater::new(
                    loader.resources.clone().with_density(self.config.density),
                );
                let tree = inflater
                    .inflate_binary(layout_data)
                    .map_err(android_inflate_error)?;
                self.unrendered.note(inflater.take_unrendered());
                warn_all(&inflater);

                session.load_tree(tree).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                })?;

                return Ok(());
            }

            // Plain XML file path
            if lower_ext(path).as_deref() == Some("xml") {
                let xml = std::fs::read_to_string(path).map_err(|e| {
                    AtalantaError::Config(format!("Failed to read XML '{}': {}", path.display(), e))
                })?;

                let inflater =
                    AndroidInflater::new(ResourceTable::empty().with_density(self.config.density));
                let tree = inflater.inflate_xml(&xml).map_err(android_inflate_error)?;
                self.unrendered.note(inflater.take_unrendered());
                warn_all(&inflater);

                session.load_tree(tree).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                })?;

                return Ok(());
            }

            // Anything else would load a blank screen on which every negative
            // assertion passes (VEL-110).
            return Err(AtalantaError::Config(format!(
                "Headless Android appPath '{}' is not an .apk or .xml layout",
                path.display()
            )));
        }

        // No app path or layout — create empty screen
        let root = crate::render_tree::RenderNode::container("View").with_style(
            crate::render_tree::NodeStyle {
                width: crate::render_tree::Dimension::length(self.config.width as f32),
                height: crate::render_tree::Dimension::length(self.config.height as f32),
                background_color: crate::render_tree::Color::WHITE,
                ..Default::default()
            },
        );

        session
            .load_tree(root)
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e)))?;

        Ok(())
    }

    async fn inflate_ios(
        &self,
        session: &mut HeadlessSession,
        app_path: Option<&str>,
        initial_layout: Option<&str>,
    ) -> Result<(), AtalantaError> {
        if let Some(path_str) = app_path {
            let path = Path::new(path_str);

            if let Some(layout_path) = self.resolve_ios_layout_path(path, initial_layout)? {
                let xml = std::fs::read_to_string(&layout_path).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!(
                        "Failed to read iOS layout '{}': {}",
                        layout_path.display(),
                        e
                    ))
                })?;

                match lower_ext(&layout_path).as_deref() {
                    Some("xib") => {
                        let doc = XibParser::parse(&xml)
                            .map_err(|e| AtalantaError::Config(format!("XIB parse failed: {e}")))?;

                        let inflater = IosInflater::new(IosResourceLoader::empty());
                        let tree = inflater.inflate(&doc).map_err(ios_inflate_error)?;
                        self.unrendered.note(inflater.take_unrendered());

                        session.load_tree(tree).map_err(|e| {
                            AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                        })?;

                        return Ok(());
                    }
                    Some("storyboard") => {
                        let doc =
                            crate::ios::storyboard::StoryboardParser::parse(&xml).map_err(|e| {
                                AtalantaError::Config(format!("Storyboard parse failed: {e}"))
                            })?;

                        let inflater = IosInflater::new(IosResourceLoader::empty());
                        let tree = inflater.inflate(&doc).map_err(ios_inflate_error)?;
                        self.unrendered.note(inflater.take_unrendered());

                        session.load_tree(tree).map_err(|e| {
                            AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                        })?;

                        return Ok(());
                    }
                    Some(other) => {
                        return Err(AtalantaError::Config(format!(
                            "Unsupported iOS layout extension '{}'. Expected .xib or .storyboard.",
                            other
                        )));
                    }
                    None => {}
                }
            }

            if lower_ext(path).as_deref() == Some("xib") {
                let xml = std::fs::read_to_string(path).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Failed to read XIB: {}", e))
                })?;

                let doc = XibParser::parse(&xml)
                    .map_err(|e| AtalantaError::Config(format!("XIB parse failed: {e}")))?;

                let inflater = IosInflater::new(IosResourceLoader::empty());
                let tree = inflater.inflate(&doc).map_err(ios_inflate_error)?;
                self.unrendered.note(inflater.take_unrendered());

                session.load_tree(tree).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                })?;

                return Ok(());
            }

            // Storyboard file
            if lower_ext(path).as_deref() == Some("storyboard") {
                let xml = std::fs::read_to_string(path).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Failed to read storyboard: {}", e))
                })?;

                let doc = crate::ios::storyboard::StoryboardParser::parse(&xml)
                    .map_err(|e| AtalantaError::Config(format!("Storyboard parse failed: {e}")))?;

                let inflater = IosInflater::new(IosResourceLoader::empty());
                let tree = inflater.inflate(&doc).map_err(ios_inflate_error)?;
                self.unrendered.note(inflater.take_unrendered());

                session.load_tree(tree).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e))
                })?;

                return Ok(());
            }

            // A missing path, a compiled .app or a directory without layouts would
            // otherwise load a blank screen (VEL-110).
            return Err(AtalantaError::Config(format!(
                "Headless iOS appPath '{}' is not a .xib/.storyboard file or a directory containing one",
                path.display()
            )));
        }

        // Default empty screen
        let root = crate::render_tree::RenderNode::container("UIView").with_style(
            crate::render_tree::NodeStyle {
                width: crate::render_tree::Dimension::length(self.config.width as f32),
                height: crate::render_tree::Dimension::length(self.config.height as f32),
                background_color: crate::render_tree::Color::WHITE,
                ..Default::default()
            },
        );

        session
            .load_tree(root)
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Render failed: {}", e)))?;

        Ok(())
    }

    fn resolve_ios_layout_path(
        &self,
        app_path: &Path,
        initial_layout: Option<&str>,
    ) -> Result<Option<PathBuf>, AtalantaError> {
        if app_path.is_file() {
            return Ok(Some(app_path.to_path_buf()));
        }

        if !app_path.is_dir() {
            return Ok(None);
        }

        if let Some(layout_name) = initial_layout {
            return Self::find_ios_layout(app_path, layout_name)
                .map(Some)
                .ok_or_else(|| {
                    AtalantaError::Config(format!(
                        "iOS layout '{}' not found under {}",
                        layout_name,
                        app_path.display()
                    ))
                });
        }

        if let Some(layout_name) = Self::detect_ios_main_layout(app_path) {
            return Ok(Self::find_ios_layout(app_path, &layout_name));
        }

        Ok(None)
    }

    fn detect_ios_main_layout(app_path: &Path) -> Option<String> {
        let info_plist = app_path.join("Info.plist");
        let plist = plist::Value::from_file(&info_plist).ok()?;
        let dict = plist.as_dictionary()?;

        dict.get("UIMainStoryboardFile")
            .and_then(|value| value.as_string())
            .or_else(|| {
                dict.get("NSMainNibFile")
                    .and_then(|value| value.as_string())
            })
            .map(ToOwned::to_owned)
    }

    fn find_ios_layout(root: &Path, layout_name: &str) -> Option<PathBuf> {
        let path = Path::new(layout_name);
        let mut candidates = Vec::new();

        if path.extension().is_some() {
            candidates.push(layout_name.to_string());
        } else {
            candidates.push(format!("{}.xib", layout_name));
            candidates.push(format!("{}.storyboard", layout_name));
        }

        Self::find_ios_layout_recursive(root, &candidates)
    }

    fn find_ios_layout_recursive(root: &Path, candidates: &[String]) -> Option<PathBuf> {
        let entries = std::fs::read_dir(root).ok()?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = Self::find_ios_layout_recursive(&path, candidates) {
                    return Some(found);
                }
                continue;
            }

            // A file name that isn't UTF-8 can't be a candidate; keep looking (VEL-S21).
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if candidates.iter().any(|candidate| candidate == file_name) {
                return Some(path);
            }
        }

        None
    }
}

#[async_trait::async_trait]
impl PlatformDriver for HeadlessDriver {
    async fn prepare(&self, device_id: &str) -> atalanta_common::Result<()> {
        let sessions = self.sessions.read().await;
        if !sessions.contains_key(device_id) {
            drop(sessions);
            self.boot_device(device_id).await?;
        }
        Ok(())
    }

    async fn cleanup(&self) {}

    async fn health_check(&self) -> HealthStatus {
        HealthStatus::Healthy
    }

    async fn restart_session(&self) -> atalanta_common::Result<()> {
        Ok(())
    }

    async fn list_devices(&self) -> atalanta_common::Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo {
            id: headless_device_id(),
            name: format!(
                "Headless Renderer ({}x{})",
                self.config.width, self.config.height
            ),
            platform: self.platform,
            state: atalanta_common::DeviceState::Booted,
            os_version: Some("headless".to_string()),
            device_type: atalanta_common::DeviceType::Simulator,
        }])
    }

    async fn boot_device(&self, device_id: &str) -> atalanta_common::Result<()> {
        let session = HeadlessSession::new(self.config.clone(), self.text_measurer.clone())
            .map_err(|e| AtalantaError::Config(format!("Session creation failed: {e}")))?;

        self.sessions
            .write()
            .await
            .insert(device_id.to_string(), session);

        Ok(())
    }

    async fn shutdown_device(&self, device_id: &str) -> atalanta_common::Result<()> {
        self.sessions.write().await.remove(device_id);
        Ok(())
    }

    async fn install_app(&self, device_id: &str, app_path: &str) -> atalanta_common::Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(device_id) {
            session.app_path = Some(app_path.to_string());
        }
        Ok(())
    }

    async fn launch_app(
        &self,
        device_id: &str,
        app_id: &str,
        _clear_state: bool,
    ) -> atalanta_common::Result<()> {
        // Ensure session exists
        {
            let sessions = self.sessions.read().await;
            if !sessions.contains_key(device_id) {
                drop(sessions);
                self.boot_device(device_id).await?;
            }
        }

        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(device_id)
            .ok_or_else(|| AtalantaError::DeviceNotFound {
                id: device_id.to_string(),
                available: vec![headless_device_id()],
            })?;

        // A launch (with or without clearState) starts at the top.
        session.scroll.reset();
        self.inflate_layout(session, app_id).await?;
        Ok(())
    }

    /// The app is gone from the screen until the next launch (VEL-357).
    async fn stop_app(&self, device_id: &str, _app_id: &str) -> atalanta_common::Result<()> {
        if let Some(session) = self.sessions.write().await.get_mut(device_id) {
            session.stop();
        }
        Ok(())
    }

    /// A rendered layout is the app, in front; it can't crash or go to the
    /// background. Before a launch there is nothing to say.
    async fn app_liveness(&self, device_id: &str, _app_id: &str) -> atalanta_common::Liveness {
        let sessions = self.sessions.read().await;
        match sessions.get(device_id) {
            Some(s) if s.element_tree.is_some() => atalanta_common::Liveness::Foreground,
            _ => atalanta_common::Liveness::Unknown,
        }
    }

    /// A render has no status or navigation bar over it.
    async fn system_bars(&self, _device_id: &str) -> Option<Vec<atalanta_common::Rect>> {
        Some(Vec::new())
    }

    /// Every action renders before it returns, so the screen is settled the
    /// moment the sync engine asks: without this it polls out the whole
    /// reaction window (~300 ms) after each step for a change that can't come.
    async fn settle_on_device(
        &self,
        device_id: &str,
        _reaction_ms: u64,
        _quiet_ms: u64,
        _cap_ms: u64,
    ) -> Option<atalanta_common::DeviceSettle> {
        let reacted = self
            .sessions
            .write()
            .await
            .get_mut(device_id)
            .is_some_and(|s| s.take_reacted());
        Some(atalanta_common::DeviceSettle {
            settled: true,
            reacted,
            elapsed_ms: 0,
        })
    }

    async fn find_element(
        &self,
        device_id: &str,
        selector: &Selector,
    ) -> atalanta_common::Result<Element> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(device_id).ok_or_else(not_launched)?;

        let hierarchy = session.get_hierarchy().ok_or_else(not_launched)?;

        let screen = Rect {
            x: 0,
            y: 0,
            width: self.config.width as i32,
            height: self.config.height as i32,
        };

        let found = find_in_tree(hierarchy, selector, Some(&screen));
        if found.is_some() {
            let n = atalanta_common::selector_match::ambiguity(hierarchy, selector, Some(&screen));
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
        device_id: &str,
        selector: &Selector,
    ) -> atalanta_common::Result<Vec<Element>> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(device_id).ok_or_else(not_launched)?;

        let hierarchy = session.get_hierarchy().ok_or_else(not_launched)?;

        let screen = Rect {
            x: 0,
            y: 0,
            width: self.config.width as i32,
            height: self.config.height as i32,
        };

        let mut results = Vec::new();
        find_all_in_tree(hierarchy, selector, Some(&screen), &mut results);
        Ok(results)
    }

    async fn get_hierarchy(&self, device_id: &str) -> atalanta_common::Result<Element> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(device_id).ok_or_else(not_launched)?;

        session.get_hierarchy().cloned().ok_or_else(not_launched)
    }

    async fn tap(&self, _device_id: &str, _element: &Element) -> atalanta_common::Result<()> {
        // No-op for static layouts
        Ok(())
    }

    async fn double_tap(
        &self,
        _device_id: &str,
        _element: &Element,
    ) -> atalanta_common::Result<()> {
        Ok(())
    }

    async fn long_press(
        &self,
        _device_id: &str,
        _element: &Element,
        _duration_ms: u64,
    ) -> atalanta_common::Result<()> {
        Ok(())
    }

    async fn input_text(
        &self,
        device_id: &str,
        element: &Element,
        text: &str,
    ) -> atalanta_common::Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(device_id).ok_or_else(not_launched)?;
        // Typing appends to what the field holds, as on the device drivers
        // (D-16, VEL-348); `clearText` empties it first.
        session
            .append_text(element, text)
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Failed to update text: {}", e)))
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> atalanta_common::Result<()> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(device_id).ok_or_else(not_launched)?;
        session
            .update_text(element, "")
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Failed to clear text: {}", e)))
    }

    /// `direction` is the finger's, as on the device drivers: `Up` shows what is below.
    async fn swipe(&self, device_id: &str, direction: Direction) -> atalanta_common::Result<()> {
        let (from, to) = swipe_line(
            self.config.width as i32,
            self.config.height as i32,
            direction,
        );
        self.swipe_coords(device_id, from, to).await
    }

    /// Scrolls whatever scrolls under `from` by the finger's travel. Before a launch
    /// there is nothing to scroll.
    async fn swipe_coords(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
    ) -> atalanta_common::Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(device_id) {
            if session.render_tree.is_some() {
                session.scroll(from, to).map_err(|e| {
                    AtalantaError::Internal(anyhow::anyhow!("Scroll failed: {}", e))
                })?;
            }
        }
        Ok(())
    }

    /// No keyboard: the older keys are no-ops, as before; the D-9 editing
    /// keys are refused rather than passing silently (review-d6 m7).
    async fn press_key(&self, _device_id: &str, key: Key) -> atalanta_common::Result<()> {
        match key {
            Key::Back | Key::Home | Key::Enter | Key::VolumeUp | Key::VolumeDown => Ok(()),
            other => Err(AtalantaError::Config(format!(
                "pressKey `{}` can't be sent by the headless driver (it has no keyboard)",
                other.name()
            ))),
        }
    }

    async fn screenshot(&self, device_id: &str) -> atalanta_common::Result<Vec<u8>> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(device_id).ok_or_else(not_launched)?;

        session
            .screenshot()
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Screenshot failed: {}", e)))
    }

    async fn screen_size(&self, device_id: &str) -> atalanta_common::Result<(i32, i32)> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(device_id) {
            Ok(session.screen_size())
        } else {
            Ok((self.config.width as i32, self.config.height as i32))
        }
    }

    async fn get_element_text(
        &self,
        _device_id: &str,
        element: &Element,
    ) -> atalanta_common::Result<String> {
        Ok(element.text.clone().unwrap_or_default())
    }

    /// The shared rule (D-1): rendered, a box, on the configured screen.
    /// It was the `visible` flag alone, so an element laid out below the
    /// screen passed assertVisible here and failed it on a device.
    async fn is_element_visible(
        &self,
        device_id: &str,
        element: &Element,
    ) -> atalanta_common::Result<bool> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: self.config.width as i32,
            height: self.config.height as i32,
        };
        let sessions = self.sessions.read().await;
        // An element that is no longer in the tree (after `stopApp` or a
        // relaunch) is not visible, as on the Android driver; its own stale
        // bounds say nothing about what is on screen now.
        Ok(sessions
            .get(device_id)
            .and_then(|s| s.get_hierarchy())
            .and_then(|tree| atalanta_common::selector_match::locate(tree, element))
            .is_some_and(|now| atalanta_common::selector_match::shown_on(now, &screen)))
    }

    async fn collect_resource_metrics(
        &self,
        _device_id: &str,
        _package: &str,
    ) -> atalanta_common::Result<(u64, u64, u64, f32)> {
        Ok((0, 0, 0, 0.0))
    }
}

/// The line a directional swipe draws, the Android driver's: through the middle of
/// the screen, over 40% of its height or half its width.
fn swipe_line(w: i32, h: i32, direction: Direction) -> ((i32, i32), (i32, i32)) {
    let (cx, cy) = (w / 2, h / 2);
    let (dx, dy) = (w / 4, h / 5);
    match direction {
        Direction::Up => ((cx, cy + dy), (cx, cy - dy)),
        Direction::Down => ((cx, cy - dy), (cx, cy + dy)),
        Direction::Left => ((cx + dx, cy), (cx - dx, cy)),
        Direction::Right => ((cx - dx, cy), (cx + dx, cy)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::Liveness;

    #[tokio::test]
    async fn editing_keys_are_refused_and_old_keys_stay_no_ops() {
        let driver = HeadlessDriver::new(Platform::Android, HeadlessConfig::default());
        let e = driver
            .press_key("headless-0", Key::Backspace)
            .await
            .unwrap_err();
        assert!(matches!(e, AtalantaError::Config(_)), "{e}");
        driver.press_key("headless-0", Key::Back).await.unwrap();
    }

    #[tokio::test]
    async fn a_launched_render_is_alive_in_front_with_no_system_bars() {
        let driver = HeadlessDriver::new(Platform::Android, HeadlessConfig::default());
        assert_eq!(
            driver.app_liveness("headless-0", "app").await,
            Liveness::Unknown,
            "nothing launched yet"
        );
        driver.launch_app("headless-0", "app", false).await.unwrap();
        assert_eq!(
            driver.app_liveness("headless-0", "app").await,
            Liveness::Foreground
        );
        assert_eq!(driver.system_bars("headless-0").await, Some(vec![]));
    }

    /// Rendering is synchronous: settled at once, "reacted" only after a
    /// redraw (a tap on a static layout changes nothing).
    #[tokio::test]
    async fn settles_at_once_and_reacts_only_to_a_redraw() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r##"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent">
                <EditText android:id="@+id/name" android:layout_width="match_parent" android:layout_height="48dp"/>
            </LinearLayout>"##,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        let dev = "headless-0";
        let settle = || driver.settle_on_device(dev, 300, 100, 5000);
        driver.launch_app(dev, "app", false).await.unwrap();
        let launched = settle().await.unwrap();
        assert!(launched.settled && launched.reacted, "{launched:?}");

        let field = driver
            .find_element(dev, &Selector::Id("name".into()))
            .await
            .unwrap();
        driver.tap(dev, &field).await.unwrap();
        let tapped = settle().await.unwrap();
        assert!(tapped.settled && !tapped.reacted, "{tapped:?}");

        driver.input_text(dev, &field, "Ada").await.unwrap();
        let typed = settle().await.unwrap();
        assert!(typed.settled && typed.reacted, "{typed:?}");
    }

    /// review-fixes #6: a missed lookup forgets the device's last count, so
    /// a later check of the selector never reads an earlier screen's count.
    #[tokio::test]
    async fn a_missed_lookup_leaves_no_match_count_behind() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r##"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent">
                <Button android:id="@+id/a" android:text="Add"/>
                <Button android:id="@+id/b" android:text="Add"/>
            </LinearLayout>"##,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let add = Selector::Text("Add".into());
        driver.find_element("headless-0", &add).await.unwrap();
        assert_eq!(driver.last_match_count("headless-0", &add), Some(2));
        let gone = Selector::Text("Remove".into());
        assert!(driver.find_element("headless-0", &gone).await.is_err());
        assert_eq!(driver.last_match_count("headless-0", &add), None);
    }

    #[tokio::test]
    async fn attributes_it_cannot_draw_warn_once_each() {
        // D-47: parsed but not drawn used to be silent.
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r##"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent" android:background="#FFFFFF">
                <Button android:id="@+id/a" android:text="A" android:elevation="4dp" android:background="@drawable/pill"/>
                <Button android:id="@+id/b" android:text="B" android:elevation="2dp" android:textStyle="bold|italic"/>
                <TextView android:text="plain" android:alpha="0.5"/>
            </LinearLayout>"##,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let seen: Vec<String> = driver
            .unrendered
            .0
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        assert_eq!(
            seen,
            [
                "android:background (drawable)",
                "android:elevation",
                "android:textStyle italic"
            ]
        );
        // Drawn attributes (a colour background, alpha) don't warn, and a
        // relaunch warns about nothing new.
        driver.launch_app("headless-0", "app", false).await.unwrap();
        assert!(driver.unrendered.note(seen.clone()).is_empty());
        assert_eq!(
            crate::unrendered::Warned::default()
                .note(["android:elevation".to_string()])
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn font_scale_enlarges_text() {
        // D-47: `headless.fontScale`.
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent"><TextView android:id="@+id/t" android:text="Hello" android:textSize="16sp" android:layout_width="wrap_content" android:layout_height="wrap_content"/></LinearLayout>"#,
        )
        .unwrap();
        let mut sizes = Vec::new();
        for font_scale in [1.0, 2.0] {
            let driver = HeadlessDriver::new(
                Platform::Android,
                HeadlessConfig {
                    app_path: Some(xml.display().to_string()),
                    font_scale,
                    ..Default::default()
                },
            );
            driver.launch_app("headless-0", "app", false).await.unwrap();
            let t = driver
                .find_element("headless-0", &Selector::Id("t".into()))
                .await
                .unwrap();
            sizes.push((t.bounds.width, t.bounds.height));
        }
        assert!(sizes[1].1 > sizes[0].1 * 3 / 2, "{sizes:?}");
    }

    fn driver_for(platform: Platform, app_path: &str) -> HeadlessDriver {
        HeadlessDriver::new(
            platform,
            HeadlessConfig {
                app_path: Some(app_path.to_string()),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn unsupported_app_path_is_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let aab = dir.path().join("app.aab");
        std::fs::write(&aab, b"x").unwrap();
        for (platform, path) in [
            (Platform::Android, aab.display().to_string()),
            (Platform::Android, dir.path().display().to_string()),
            (Platform::Android, "${LAYOUT:-layout.xml}".to_string()),
            (
                Platform::Android,
                dir.path().join("nothere.xml").display().to_string(),
            ),
            (
                Platform::Ios,
                dir.path().join("Missing.app").display().to_string(),
            ),
            (Platform::Ios, dir.path().display().to_string()),
        ] {
            let err = driver_for(platform, &path)
                .launch_app("headless-0", "app", false)
                .await
                .expect_err(&path);
            assert!(
                matches!(&err, AtalantaError::Config(m) if m.contains(&path)),
                "{path}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn visibility_min_area_applies_to_assertions() {
        // D-1: the shared rule, `visibility.minArea` included: a view far
        // taller than the screen has only a small share of it on screen.
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:orientation="vertical" android:layout_width="match_parent" android:layout_height="match_parent"><Button android:id="@+id/tall" android:text="Tall" android:layout_width="match_parent" android:layout_height="20000dp"/></LinearLayout>"#,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let tall = driver
            .find_element("headless-0", &Selector::Text("Tall".into()))
            .await
            .unwrap();
        assert!(driver
            .is_element_visible("headless-0", &tall)
            .await
            .unwrap());
        let mut set = std::collections::HashMap::new();
        set.insert(
            "ATALANTA_VISIBILITY_MIN_AREA".to_string(),
            "0.9".to_string(),
        );
        atalanta_common::env::set_settings(set);
        let shown = driver
            .is_element_visible("headless-0", &tall)
            .await
            .unwrap();
        atalanta_common::env::set_settings(std::collections::HashMap::new());
        assert!(!shown, "{:?}", tall.bounds);
    }

    #[tokio::test]
    async fn app_path_extensions_are_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("LAYOUT.XML");
        std::fs::write(
            &xml,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent"><Button android:id="@+id/go" android:text="Go"/></LinearLayout>"#,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let tree = driver.get_hierarchy("headless-0").await.unwrap();
        assert_eq!(tree.children[0].text.as_deref(), Some("Go"));
    }

    #[tokio::test]
    async fn an_element_gone_from_the_screen_is_not_visible() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent"><Button android:id="@+id/go" android:text="Go"/></LinearLayout>"#,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let go = driver
            .find_element("headless-0", &Selector::Id("go".into()))
            .await
            .unwrap();
        assert!(driver.is_element_visible("headless-0", &go).await.unwrap());
        driver.stop_app("headless-0", "app").await.unwrap();
        assert!(!driver.is_element_visible("headless-0", &go).await.unwrap());
    }

    #[tokio::test]
    async fn an_unparseable_layout_or_screen_size_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        for (platform, name, body) in [
            (
                Platform::Android,
                "bad.xml",
                "<LinearLayout><TextView></LinearLayout>",
            ),
            (Platform::Android, "empty.xml", "<!-- nothing -->"),
            (Platform::Ios, "bad.xib", "<view><label></view>"),
            (Platform::Android, "bad.apk", "not a zip"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            let err = driver_for(platform, &path.display().to_string())
                .launch_app("headless-0", "app", false)
                .await
                .expect_err(name);
            assert!(matches!(err, AtalantaError::Config(_)), "{name}: {err:?}");
        }
        let zero = HeadlessDriver::new(
            Platform::Android,
            HeadlessConfig {
                width: 0,
                ..Default::default()
            },
        );
        let err = zero
            .launch_app("headless-0", "app", false)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err:?}");
    }

    #[tokio::test]
    async fn the_same_element_can_be_typed_into_twice() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent"><EditText android:id="@+id/name" android:layout_width="match_parent" android:layout_height="48dp"/></LinearLayout>"#,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let name = driver
            .find_element("headless-0", &Selector::Id("name".into()))
            .await
            .unwrap();
        driver.input_text("headless-0", &name, "a").await.unwrap();
        driver.input_text("headless-0", &name, "b").await.unwrap();
        let field = driver
            .find_element("headless-0", &Selector::Id("name".into()))
            .await
            .unwrap();
        assert_eq!(field.text.as_deref(), Some("ab"));
    }

    #[tokio::test]
    async fn frame_layout_children_overlap_instead_of_stacking() {
        let dir = tempfile::tempdir().unwrap();
        let xml = dir.path().join("layout.xml");
        std::fs::write(
            &xml,
            r#"<FrameLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent">
                <LinearLayout android:id="@+id/content" android:layout_width="match_parent" android:layout_height="match_parent"/>
                <TextView android:id="@+id/overlay" android:text="Loading" android:layout_width="match_parent" android:layout_height="match_parent"/>
                <Button android:id="@+id/fab" android:text="FAB" android:layout_width="wrap_content" android:layout_height="wrap_content" android:layout_gravity="bottom|end"/>
            </FrameLayout>"#,
        )
        .unwrap();
        let driver = driver_for(Platform::Android, &xml.display().to_string());
        driver.launch_app("headless-0", "app", false).await.unwrap();
        let find = |id: &'static str| {
            let driver = &driver;
            async move {
                driver
                    .find_element("headless-0", &Selector::Id(id.into()))
                    .await
                    .unwrap()
            }
        };
        let content = find("content").await;
        let overlay = find("overlay").await;
        assert_eq!(overlay.bounds, content.bounds);
        assert_eq!((overlay.bounds.y, overlay.bounds.height), (0, 1920));
        assert!(overlay.visible);
        let fab = find("fab").await;
        assert_eq!(fab.bounds.x + fab.bounds.width, 1080, "{:?}", fab.bounds);
        assert_eq!(fab.bounds.y + fab.bounds.height, 1920, "{:?}", fab.bounds);
    }
}
