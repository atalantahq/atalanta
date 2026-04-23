use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::debug;
use velocity_common::{
    selector_match::{find_all_in_tree, find_in_tree},
    DeviceInfo, DeviceState, DeviceType, Direction, Element, Key, Platform, Result, Selector,
    VelocityError,
};

use crate::cdp;
use crate::dom::{json_to_element, DOM_WALKER_SCRIPT};
use crate::selector::{nth_css_js, selector_to_css, xpath_js};

/// Configuration for the web browser driver.
#[derive(Debug, Clone)]
pub struct WebDriverConfig {
    /// URL to navigate to during `prepare()`. Optional.
    pub start_url: Option<String>,
    /// Viewport width in CSS pixels (default: 1280).
    pub width: u32,
    /// Viewport height in CSS pixels (default: 800).
    pub height: u32,
    /// Path to a Chrome/Chromium binary. Detected from PATH if absent.
    pub chrome_path: Option<String>,
    /// Run Chrome in headless mode (default: true).
    pub headless: bool,
    /// Additional Chrome launch arguments.
    pub extra_args: Vec<String>,
}

impl Default for WebDriverConfig {
    fn default() -> Self {
        Self {
            start_url: None,
            width: 1280,
            height: 800,
            chrome_path: None,
            headless: true,
            extra_args: Vec::new(),
        }
    }
}

/// Internal browser session state (None until `prepare()` is called).
struct BrowserSession {
    browser: Browser,
    page: Arc<Page>,
    /// DOM hierarchy cache with TTL.
    hierarchy_cache: Option<(Element, Instant)>,
    cache_ttl: Duration,
}

/// Web browser driver implementing `PlatformDriver` via Chrome DevTools Protocol.
///
/// Uses `chromiumoxide` (persistent WebSocket CDP connection) for all interactions.
/// CSS and XPath selectors resolve directly in Chrome's engine in one CDP round-trip
/// (~0.5–3 ms). Text-based selectors fall back to the cached DOM hierarchy tree-walk.
pub struct WebDriver {
    config: WebDriverConfig,
    session: Mutex<Option<BrowserSession>>,
}

impl WebDriver {
    pub fn new(config: WebDriverConfig) -> Self {
        Self {
            config,
            session: Mutex::new(None),
        }
    }

    fn require_session<'a>(
        session: &'a Option<BrowserSession>,
    ) -> Result<&'a BrowserSession> {
        session.as_ref().ok_or_else(|| {
            VelocityError::Config("No browser session — call prepare() first".into())
        })
    }

    /// Fetch a fresh DOM hierarchy from the page (one CDP round-trip).
    async fn fetch_hierarchy(page: &Page) -> Result<Element> {
        let result = page
            .evaluate(DOM_WALKER_SCRIPT)
            .await
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        let json: serde_json::Value = result
            .into_value()
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        json_to_element(&json).ok_or_else(|| {
            VelocityError::Config("DOM walker returned null — page body may be empty".into())
        })
    }

    /// Resolve a selector to an Element using the fastest available path:
    /// 1. CSS/XPath/Id/AccessibilityId/ClassName → direct CDP query (no tree needed)
    /// 2. Text/TextContains → cached DOM tree walk
    /// 3. Index → CDP querySelectorAll + Nth pick
    async fn resolve_element(page: &Arc<Page>, selector: &Selector) -> Result<Element> {
        // Fast path: selectors that map to CSS.
        if let Ok(css) = selector_to_css(selector) {
            let js = format!(
                r#"(function(){{
                    var n = document.querySelector('{css}');
                    if (!n) return null;
                    if (!n.dataset.velId) n.dataset.velId = Math.random().toString(36).slice(2,10);
                    var rect = n.getBoundingClientRect();
                    var s = window.getComputedStyle(n);
                    return {{
                        velId:     n.dataset.velId,
                        tag:       n.tagName.toLowerCase(),
                        domId:     n.id || null,
                        text:      (n.textContent||'').trim().slice(0,200)||null,
                        ariaLabel: n.getAttribute('aria-label'),
                        x: Math.round(rect.x), y: Math.round(rect.y),
                        w: Math.round(rect.width), h: Math.round(rect.height),
                        visible:   s.display !== 'none' && s.visibility !== 'hidden'
                    }};
                }})()"#,
                css = css.replace('\'', "\\'")
            );
            let result = page
                .evaluate(js)
                .await
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            let json: serde_json::Value = result
                .into_value()
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            if json.is_null() {
                return Err(VelocityError::ElementNotFound {
                    selector: format!("{selector}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                });
            }
            return json_to_element(&json).ok_or_else(|| VelocityError::ElementNotFound {
                selector: format!("{selector}"),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            });
        }

        // XPath path.
        if let Selector::XPath(xpath) = selector {
            let js = xpath_js(xpath);
            let result = page
                .evaluate(js)
                .await
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            let json: serde_json::Value = result
                .into_value()
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            if json.is_null() {
                return Err(VelocityError::ElementNotFound {
                    selector: format!("{selector}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                });
            }
            return json_to_element(&json).ok_or_else(|| VelocityError::ElementNotFound {
                selector: format!("{selector}"),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            });
        }

        // Index path: resolve inner selector to CSS, then pick Nth.
        if let Selector::Index { selector: inner, index } = selector {
            if let Ok(css) = selector_to_css(inner) {
                let js = nth_css_js(&css, *index);
                let result = page
                    .evaluate(js)
                    .await
                    .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
                let json: serde_json::Value = result
                    .into_value()
                    .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
                if !json.is_null() {
                    if let Some(el) = json_to_element(&json) {
                        return Ok(el);
                    }
                }
            }
            // Fallback: fetch hierarchy and do index selection in tree.
            let tree = Self::fetch_hierarchy(page).await?;
            return find_in_tree(&tree, selector, None).ok_or_else(|| {
                VelocityError::ElementNotFound {
                    selector: format!("{selector}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                }
            });
        }

        // Text / TextContains fallback: walk the DOM hierarchy tree.
        let tree = Self::fetch_hierarchy(page).await?;
        find_in_tree(&tree, selector, None).ok_or_else(|| VelocityError::ElementNotFound {
            selector: format!("{selector}"),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        })
    }
}

#[async_trait::async_trait]
impl velocity_common::PlatformDriver for WebDriver {
    // ──────────────────────────────────────────────
    // Lifecycle
    // ──────────────────────────────────────────────

    async fn prepare(&self, _device_id: &str) -> Result<()> {
        let mut builder = BrowserConfig::builder()
            .window_size(self.config.width, self.config.height);

        if !self.config.headless {
            builder = builder.with_head();
        }

        if let Some(ref path) = self.config.chrome_path {
            builder = builder.chrome_executable(path);
        }

        for arg in &self.config.extra_args {
            builder = builder.arg(arg.as_str());
        }

        let cfg = builder
            .build()
            .map_err(|e| VelocityError::Config(format!("Chrome config error: {e}")))?;

        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| VelocityError::Config(format!("Failed to launch Chrome: {e}")))?;

        // The CDP event handler must be polled on a background task.
        tokio::spawn(async move {
            while let Some(_event) = handler.next().await {}
        });

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| VelocityError::Config(format!("Failed to open page: {e}")))?;

        // Navigate to start URL if configured.
        if let Some(ref url) = self.config.start_url {
            page.goto(url.as_str())
                .await
                .map_err(|e| VelocityError::Config(format!("Start URL navigation failed: {e}")))?;
            page.wait_for_navigation().await.ok();
        }

        let mut session = self.session.lock().await;
        *session = Some(BrowserSession {
            browser,
            page: Arc::new(page),
            hierarchy_cache: None,
            cache_ttl: Duration::from_millis(1500),
        });

        debug!("velocity-web: Chrome session ready");
        Ok(())
    }

    async fn cleanup(&self) {
        let mut session = self.session.lock().await;
        if let Some(mut s) = session.take() {
            let _ = s.browser.close().await;
        }
    }

    async fn health_check(&self) -> velocity_common::HealthStatus {
        let session = self.session.lock().await;
        let Ok(s) = Self::require_session(&session) else {
            return velocity_common::HealthStatus::Unhealthy;
        };
        match s.page.evaluate("1+1").await {
            Ok(_) => velocity_common::HealthStatus::Healthy,
            Err(_) => velocity_common::HealthStatus::Unhealthy,
        }
    }

    async fn restart_session(&self) -> Result<()> {
        self.cleanup().await;
        self.prepare("web").await
    }

    // ──────────────────────────────────────────────
    // Device management (virtual — browser = the "device")
    // ──────────────────────────────────────────────

    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo {
            id: "web".to_string(),
            name: "Chrome Browser".to_string(),
            platform: Platform::Web,
            state: DeviceState::Booted,
            os_version: None,
            device_type: DeviceType::Unknown,
        }])
    }

    async fn boot_device(&self, _device_id: &str) -> Result<()> {
        Ok(()) // Browser is launched in prepare()
    }

    async fn shutdown_device(&self, _device_id: &str) -> Result<()> {
        self.cleanup().await;
        Ok(())
    }

    // ──────────────────────────────────────────────
    // App lifecycle (URL navigation)
    // ──────────────────────────────────────────────

    async fn install_app(&self, _device_id: &str, _app_path: &str) -> Result<()> {
        Ok(()) // URLs don't need installation
    }

    async fn launch_app(&self, _device_id: &str, app_id: &str, clear_state: bool) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        if clear_state {
            // Clear cookies and storage.
            let _ = s
                .page
                .evaluate("localStorage.clear(); sessionStorage.clear();")
                .await;
        }
        s.page
            .goto(app_id)
            .await
            .map_err(|e| VelocityError::Config(format!("launch_app navigation failed: {e}")))?;
        s.page.wait_for_navigation().await.ok();
        Ok(())
    }

    async fn stop_app(&self, _device_id: &str, _app_id: &str) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        s.page.goto("about:blank").await.ok();
        Ok(())
    }

    // ──────────────────────────────────────────────
    // Web navigation (new trait methods)
    // ──────────────────────────────────────────────

    async fn navigate(&self, _device_id: &str, url: &str) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        s.page
            .goto(url)
            .await
            .map_err(|e| VelocityError::Config(format!("navigate() failed: {e}")))?;
        s.page.wait_for_navigation().await.ok();
        Ok(())
    }

    async fn go_back(&self, _device_id: &str) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        s.page.evaluate("history.back()").await.ok();
        // Allow time for navigation to complete.
        drop(session);
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    async fn refresh_page(&self, _device_id: &str) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        s.page.evaluate("location.reload()").await.ok();
        s.page.wait_for_navigation().await.ok();
        Ok(())
    }

    // ──────────────────────────────────────────────
    // Element queries
    // ──────────────────────────────────────────────

    async fn find_element(&self, _device_id: &str, selector: &Selector) -> Result<Element> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        Self::resolve_element(&s.page, selector).await
    }

    async fn find_elements(&self, _device_id: &str, selector: &Selector) -> Result<Vec<Element>> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;

        // For CSS-expressible selectors use querySelectorAll.
        if let Ok(css) = selector_to_css(selector) {
            let js = format!(
                r#"(function(){{
                    var nodes = document.querySelectorAll('{css}');
                    return Array.from(nodes).map(function(n){{
                        if (!n.dataset.velId) n.dataset.velId = Math.random().toString(36).slice(2,10);
                        var rect = n.getBoundingClientRect();
                        var st = window.getComputedStyle(n);
                        return {{
                            velId:     n.dataset.velId,
                            tag:       n.tagName.toLowerCase(),
                            domId:     n.id || null,
                            text:      (n.textContent||'').trim().slice(0,200)||null,
                            ariaLabel: n.getAttribute('aria-label'),
                            x: Math.round(rect.x), y: Math.round(rect.y),
                            w: Math.round(rect.width), h: Math.round(rect.height),
                            visible:   st.display !== 'none' && st.visibility !== 'hidden'
                        }};
                    }});
                }})()"#,
                css = css.replace('\'', "\\'")
            );
            let result = s
                .page
                .evaluate(js)
                .await
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            let json: serde_json::Value = result
                .into_value()
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
            if let Some(arr) = json.as_array() {
                return Ok(arr.iter().filter_map(json_to_element).collect());
            }
            return Ok(vec![]);
        }

        // Fallback: walk the tree.
        let tree = Self::fetch_hierarchy(&s.page).await?;
        let mut results = Vec::new();
        find_all_in_tree(&tree, selector, None, &mut results);
        Ok(results)
    }

    async fn get_hierarchy(&self, _device_id: &str) -> Result<Element> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        Self::fetch_hierarchy(&s.page).await
    }

    /// Override with TTL-cached hierarchy to avoid redundant CDP round-trips.
    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        // Check cache while holding the lock.
        {
            let session = self.session.lock().await;
            let s = Self::require_session(&session)?;
            if let Some((ref tree, ref ts)) = s.hierarchy_cache {
                if ts.elapsed() < s.cache_ttl {
                    debug!("velocity-web: hierarchy cache hit");
                    return Ok(tree.clone());
                }
            }
        }
        // Cache miss — fetch from page then update cache.
        let tree = self.get_hierarchy(device_id).await?;
        {
            let mut session = self.session.lock().await;
            if let Some(ref mut s) = *session {
                s.hierarchy_cache = Some((tree.clone(), Instant::now()));
            }
        }
        Ok(tree)
    }

    // ──────────────────────────────────────────────
    // Actions
    // ──────────────────────────────────────────────

    async fn tap(&self, _device_id: &str, element: &Element) -> Result<()> {
        let (cx, cy) = element.bounds.center();
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::click(&s.page, cx as f64, cy as f64).await
    }

    async fn double_tap(&self, _device_id: &str, element: &Element) -> Result<()> {
        let (cx, cy) = element.bounds.center();
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::double_click(&s.page, cx as f64, cy as f64).await
    }

    async fn long_press(
        &self,
        _device_id: &str,
        element: &Element,
        duration_ms: u64,
    ) -> Result<()> {
        let (cx, cy) = element.bounds.center();
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::long_press(&s.page, cx as f64, cy as f64, duration_ms).await
    }

    async fn tap_xy(&self, _device_id: &str, x: i32, y: i32) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::click(&s.page, x as f64, y as f64).await
    }

    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        // Focus element first.
        self.tap(device_id, element).await?;
        // Re-acquire session for insertText.
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        // Input.insertText inserts a full string atomically — works with React controlled inputs.
        use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
        s.page
            .execute(InsertTextParams::new(text))
            .await
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        Ok(())
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()> {
        // Focus element.
        self.tap(device_id, element).await?;
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        // Select all then delete.
        cdp::select_all(&s.page).await?;
        cdp::dispatch_key(&s.page, "Delete").await
    }

    async fn input_raw(&self, device_id: &str, text: &str) -> Result<()> {
        // For web, input_raw inserts text into whatever is currently focused.
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
        s.page
            .execute(InsertTextParams::new(text))
            .await
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        let _ = device_id;
        Ok(())
    }

    async fn swipe(&self, _device_id: &str, direction: Direction) -> Result<()> {
        let (dx, dy): (i32, i32) = match direction {
            Direction::Up => (0, -300),
            Direction::Down => (0, 300),
            Direction::Left => (-300, 0),
            Direction::Right => (300, 0),
        };
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        s.page
            .evaluate(format!("window.scrollBy({dx}, {dy})"))
            .await
            .ok();
        Ok(())
    }

    async fn swipe_coords(
        &self,
        _device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
    ) -> Result<()> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::drag(&s.page, from, to).await
    }

    async fn press_key(&self, _device_id: &str, key: Key) -> Result<()> {
        let key_str = match key {
            Key::Enter => "Enter",
            Key::Back => "Backspace",
            Key::Home => "Home",
            // Volume keys have no browser equivalent — silently skip.
            Key::VolumeUp | Key::VolumeDown => return Ok(()),
        };
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        cdp::dispatch_key(&s.page, key_str).await
    }

    // ──────────────────────────────────────────────
    // Screen
    // ──────────────────────────────────────────────

    async fn screenshot(&self, _device_id: &str) -> Result<Vec<u8>> {
        let session = self.session.lock().await;
        let s = Self::require_session(&session)?;
        use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
        use chromiumoxide::page::ScreenshotParams;
        let bytes = s
            .page
            .screenshot(ScreenshotParams::builder().format(CaptureScreenshotFormat::Png).build())
            .await
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        Ok(bytes)
    }

    async fn screen_size(&self, _device_id: &str) -> Result<(i32, i32)> {
        Ok((self.config.width as i32, self.config.height as i32))
    }

    // ──────────────────────────────────────────────
    // Element state
    // ──────────────────────────────────────────────

    async fn get_element_text(&self, _device_id: &str, element: &Element) -> Result<String> {
        Ok(element.text.clone().unwrap_or_default())
    }

    async fn is_element_visible(&self, _device_id: &str, element: &Element) -> Result<bool> {
        Ok(element.visible && !element.bounds.is_empty())
    }
}
