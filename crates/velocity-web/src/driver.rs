use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::page::GetNavigationHistoryParams;
use chromiumoxide::error::CdpError;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::debug;
use velocity_common::{
    selector_match::find_all_ref, DeviceInfo, DeviceState, DeviceType, Direction, Element, Key,
    Platform, Result, Selector, VelocityError,
};

use crate::cdp::{self, Page};
use crate::dom::{json_to_element, tree_from_list};
use crate::protocol::BrowserProtocol;
use crate::selector::{invalid_selector, selector_json, ALL_TEXT_LIMIT};

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
    /// Requests answered or refused without the network (`web.mocks`,
    /// `web.block`), for every test.
    pub mocks: Vec<velocity_common::MockRule>,
    /// Independent browsers (`--parallel N`, D-48): N Chrome processes,
    /// the devices `web-0` … `web-{N-1}`, each with its own profile and
    /// network rules. One browser is the device `web`.
    pub instances: usize,
    /// A mobile-web profile (`web.device`, D-W3): pixel ratio, user agent,
    /// mobile layout and touch. Set it with [`WebDriverConfig::apply_device`],
    /// which also takes its viewport.
    pub device: Option<velocity_common::web_devices::WebDevice>,
    /// How `instances > 1` browsers are made (`web.parallelMode`, W3):
    /// browser contexts in one Chrome (the default), or one Chrome process
    /// each.
    pub parallel_mode: ParallelMode,
}

/// `web.parallelMode`: what each of `--parallel N`'s web devices is (W3).
pub use velocity_common::test_types::WebParallelMode as ParallelMode;

impl WebDriverConfig {
    /// Emulate `device`: its viewport becomes `width`/`height` (set those
    /// after this to override it), and its pixel ratio, user agent, mobile
    /// layout and touch apply to every page.
    pub fn apply_device(&mut self, device: &velocity_common::web_devices::WebDevice) {
        self.width = device.width;
        self.height = device.height;
        self.device = Some(*device);
    }
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
            mocks: Vec::new(),
            instances: 1,
            device: None,
            parallel_mode: ParallelMode::default(),
        }
    }
}

/// One Chrome process: its own profile, and the flag that says whether
/// its DevTools connection is still up. In process mode each device has
/// one; in context mode (W3) the devices share one.
struct Chrome {
    browser: Mutex<Browser>,
    alive: Arc<AtomicBool>,
    /// Its DevTools endpoint, for the health ping ([`Chrome::answers`]).
    ws_url: String,
    /// This Chrome's own profile, removed when it closes.
    profile_dir: PathBuf,
    /// Chrome's pid and profile, registered for the second signal's
    /// `kill_all_groups` (review-d6 M2): Chrome runs in Velocity's own
    /// process group, and `process::exit` skips `kill_on_drop`.
    _registered: Option<velocity_common::process::PidGuard>,
}

/// A Chrome dropped without `close` (a panic, an early return, the daemon
/// going away) still stops and its profile goes, which leaked one
/// `velocity-chrome-*` dir per browser (review-d6 m5).
impl Drop for Chrome {
    fn drop(&mut self) {
        if let Some(child) = self.browser.get_mut().get_mut_child() {
            let _ = child.inner.start_kill();
        }
        // Chrome's exiting helpers can write into the profile after one
        // `remove_dir_all` and bring it back: retry off-thread for about a
        // second, as `remove_profile_dir` does (a drop can't wait).
        let dir = self.profile_dir.clone();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::thread::Builder::new()
            .name("velocity-profile-rm".into())
            .spawn(move || {
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = std::fs::remove_dir_all(&dir);
                }
            });
    }
}

impl Chrome {
    fn pid(&self) -> Option<u32> {
        self.browser
            .try_lock()
            .ok()?
            .get_mut_child()
            .and_then(|c| c.inner.id())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Whether the browser process answers a browser-level call
    /// (`Browser.getVersion`); a hung one keeps its socket open, so
    /// `is_alive` can't tell. The ping goes over a DevTools connection
    /// of its own, so the browser lock other devices hold (a context
    /// created or disposed, a page opened) is not counted (review-fi2 F2);
    /// and it takes [`HUNG_PINGS`] failed pings in a row, each within
    /// [`PING_TIMEOUT`], to call a Chrome that every device shares hung.
    async fn answers(&self) -> bool {
        if !self.is_alive() {
            return false;
        }
        for _ in 0..HUNG_PINGS {
            if crate::frames::browser_answers(&self.ws_url, PING_TIMEOUT).await {
                return true;
            }
            if !self.is_alive() {
                return false;
            }
        }
        false
    }

    /// Asks Chrome to close, but never waits on one that is gone or stuck:
    /// `close` goes through the handler, which is dead once Chrome is, and
    /// waited forever (VEL-020). Then its profile goes.
    async fn close(&self) {
        let mut browser = self.browser.lock().await;
        let closed = self.is_alive()
            && matches!(
                tokio::time::timeout(CLOSE_TIMEOUT, browser.close()).await,
                Ok(Ok(_))
            );
        let exited = closed
            && matches!(
                tokio::time::timeout(CLOSE_TIMEOUT, browser.wait()).await,
                Ok(Ok(_))
            );
        if !exited {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, browser.kill()).await;
        }
        remove_profile_dir(&self.profile_dir).await;
    }
}

/// One device's browser (absent until `prepare()`, or the first call for a
/// device the driver lists): a page in its own Chrome, or in its own
/// browser context of the shared Chrome (W3).
struct BrowserSession {
    chrome: Arc<Chrome>,
    /// The device's browser context in the shared Chrome (context mode).
    context: Option<chromiumoxide::cdp::browser_protocol::browser::BrowserContextId>,
    page: Arc<Page>,
    /// The pages that opened `page`, the first opener first: a popup or
    /// new tab the page opened is driven from then on, and when it closes
    /// its opener is again (review 10 #5).
    openers: Vec<Arc<Page>>,
    /// The tabs `page` and its popups open and close.
    tabs: TabWatch,
    /// This browser's requests and mocks.
    network: crate::network::Network,
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
    /// One browser per device id. Held only to look a session up or swap
    /// it, never across a page call (NB-8).
    sessions: Mutex<HashMap<String, BrowserSession>>,
    /// Context mode (W3): the one Chrome every device's context lives in,
    /// started with the first device and closed by `cleanup`.
    shared: Mutex<Option<Arc<Chrome>>>,
    /// One launch at a time per device, so a device never gets two
    /// browsers; different devices launch side by side.
    launches: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// On-screen matches of each lookup, counted in the same page call.
    match_counts: velocity_common::selector_match::MatchCounts,
}

impl WebDriver {
    pub fn new(config: WebDriverConfig) -> Self {
        Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            shared: Mutex::new(None),
            launches: std::sync::Mutex::new(HashMap::new()),
            match_counts: Default::default(),
        }
    }

    /// The devices this driver serves: `web` for one browser, `web-0` …
    /// `web-{N-1}` for `instances: N` (D-48).
    pub fn device_ids(&self) -> Vec<String> {
        match self.config.instances.max(1) {
            1 => vec!["web".to_string()],
            n => (0..n).map(|i| format!("web-{i}")).collect(),
        }
    }

    /// How many browsers are running (tests only).
    #[doc(hidden)]
    pub async fn debug_browsers(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// The Chrome process ids of the running browsers (tests only).
    #[doc(hidden)]
    pub async fn debug_browser_pids(&self) -> Vec<u32> {
        let sessions = self.sessions.lock().await;
        let mut pids: Vec<u32> = sessions.values().filter_map(|s| s.chrome.pid()).collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Hold the shared Chrome's browser lock for `hold`, as a slow context
    /// disposal or page creation by another device does (tests only).
    #[doc(hidden)]
    pub async fn debug_hold_shared_browser(&self, hold: Duration) {
        let chrome = self.shared.lock().await.clone();
        if let Some(chrome) = chrome {
            let _browser = chrome.browser.lock().await;
            tokio::time::sleep(hold).await;
        }
    }

    /// `device_id`'s Chrome process id, if its browser runs (tests only).
    #[doc(hidden)]
    pub async fn debug_browser_pid(&self, device_id: &str) -> Option<u32> {
        let sessions = self.sessions.lock().await;
        sessions.get(device_id).and_then(|s| s.chrome.pid())
    }

    /// `device_id`'s browser context id, in context mode (tests only).
    #[doc(hidden)]
    pub async fn debug_browser_context(&self, device_id: &str) -> Option<String> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(device_id)
            .and_then(|s| s.context.as_ref())
            .map(|c| c.inner().clone())
    }

    /// Whether this driver's devices are contexts of one Chrome (W3).
    fn uses_contexts(&self) -> bool {
        self.config.parallel_mode == ParallelMode::Contexts && self.config.instances > 1
    }

    /// The device the device-less debug helpers use: `web`, else the
    /// first running browser.
    async fn debug_device(&self) -> String {
        let sessions = self.sessions.lock().await;
        if sessions.contains_key("web") {
            return "web".into();
        }
        let mut ids: Vec<&String> = sessions.keys().collect();
        ids.sort();
        ids.first()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "web".into())
    }

    /// How many elements the page's id registry holds (tests only).
    #[doc(hidden)]
    pub async fn debug_live_ids(&self) -> Result<u64> {
        let page = self.page_handle(&self.debug_device().await).await?;
        Ok(page.call("liveIds", vec![]).await?.as_u64().unwrap_or(0))
    }

    /// Runs the in-page library's `op` on the element `id` (tests only).
    #[doc(hidden)]
    pub async fn debug_call_el(
        &self,
        op: &str,
        id: &str,
        rest: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let page = self.page_handle(&self.debug_device().await).await?;
        page.call_el(op, id, rest).await
    }

    /// Runs `js` in the page's own world (tests only).
    #[doc(hidden)]
    pub async fn debug_eval_main(&self, js: &str) -> Result<serde_json::Value> {
        self.debug_eval_on(&self.debug_device().await, js).await
    }

    /// Runs `js` in `device_id`'s page's own world (tests only).
    #[doc(hidden)]
    pub async fn debug_eval_on(&self, device_id: &str, js: &str) -> Result<serde_json::Value> {
        let page = self.page_handle(device_id).await?;
        Ok(page.eval(js).await?.value().cloned().unwrap_or_default())
    }

    fn no_session(&self, device_id: &str) -> VelocityError {
        VelocityError::Config(format!(
            "No browser session for {device_id:?} — call prepare() first (this driver's devices: {})",
            self.device_ids().join(", ")
        ))
    }

    fn launch_lock(&self, device_id: &str) -> Arc<Mutex<()>> {
        self.launches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device_id.to_string())
            .or_default()
            .clone()
    }

    /// Fetch a fresh DOM hierarchy from the page (one CDP round-trip).
    async fn href(page: &Page) -> Option<String> {
        page.eval("location.href").await.ok()?.into_value().ok()
    }

    /// Polls `location.href` and `document.readyState` until `done` holds or
    /// `limit` passes. Evaluation errors (mid-navigation) count as not done.
    async fn wait_until(page: &Page, limit: Duration, done: impl Fn(&str, &str) -> bool) {
        let start = Instant::now();
        while start.elapsed() < limit {
            // A dead browser never gets there.
            if !page.is_alive() {
                return;
            }
            let state: Option<Vec<String>> = match page
                .run_for(
                    Duration::from_secs(2),
                    page.page.evaluate("[location.href, document.readyState]"),
                )
                .await
            {
                Ok(v) => v.into_value().ok(),
                Err(_) => None,
            };
            if let Some([href, ready]) = state.as_deref() {
                if done(href, ready) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        debug!("navigation wait gave up after {limit:?}");
    }

    async fn fetch_hierarchy(page: &dyn BrowserProtocol) -> Result<Element> {
        Self::fetch_hierarchy_with(page, WALK_TEXT_LIMIT).await
    }

    /// The element tree with each node's text cut to `limit` characters
    /// (0: whole).
    async fn fetch_hierarchy_with(page: &dyn BrowserProtocol, limit: usize) -> Result<Element> {
        let list = page.call("walk", vec![limit.into()]).await?;
        list.as_array()
            .and_then(|l| tree_from_list(l))
            .ok_or_else(|| {
                VelocityError::Internal(anyhow::anyhow!(
                    "the page has no <body> to read the element tree from"
                ))
            })
    }

    /// The first element `query` (a [`selector_json`]) matches in the page,
    /// a visible one if any; `null` (or `undefined`) is "not found", never
    /// an internal error, which made a missing element retry and trip the
    /// circuit breaker.
    /// Also returns how many matches are on screen, counted in the same
    /// call (R5).
    async fn capture(
        page: &dyn BrowserProtocol,
        query: serde_json::Value,
        selector: &Selector,
    ) -> Result<(Element, Option<usize>)> {
        let mut json = page.call("find", vec![query.clone()]).await?;
        if let Some(reason) = invalid_selector(&json) {
            return Err(Self::invalid(selector, reason));
        }
        // Not on the page, or only hidden there: the cross-origin frames
        // (W4), a shown match first.
        if json.is_null() || json["visible"] == false {
            let replies = page.call_frames("find", vec![query]).await;
            let found = replies.found;
            let pick = found
                .iter()
                .position(|v| v["visible"] != false)
                .or(if json.is_null() { Some(0) } else { None });
            if let Some(v) = pick.and_then(|i| found.into_iter().nth(i)) {
                json = v;
            }
            // No shown match, and a frame that didn't answer may hold one:
            // that is unknown, not "not found", or `assertNotVisible`
            // passed while the frame showed the element (review-fi2 F1).
            if (json.is_null() || json["visible"] == false) && !replies.skipped.is_empty() {
                return Err(Self::frames_unknown(selector, &replies.skipped));
            }
        }
        if json.is_null() {
            return Err(Self::not_found(selector));
        }
        let n = json["n"].as_u64().map(|n| n as usize);
        json_to_element(&json)
            .map(|e| (e, n))
            .ok_or_else(|| Self::not_found(selector))
    }

    /// Every element `query` matches, captured.
    async fn capture_all(
        page: &dyn BrowserProtocol,
        query: serde_json::Value,
        selector: &Selector,
    ) -> Result<Vec<Element>> {
        let args = vec![query, ALL_TEXT_LIMIT.into()];
        let json = page.call("findAll", args.clone()).await?;
        if let Some(reason) = invalid_selector(&json) {
            return Err(Self::invalid(selector, reason));
        }
        let mut all: Vec<Element> = json
            .as_array()
            .map(|a| a.iter().filter_map(json_to_element).collect())
            .unwrap_or_default();
        // Then the cross-origin frames' (W4).
        for found in page.call_frames("findAll", args).await.found {
            all.extend(
                found
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(json_to_element),
            );
        }
        Ok(all)
    }

    /// The Velocity ids of what `selector` matches, in order. Everything but
    /// a layout relation resolves in the page. A relation needs boxes and
    /// nesting, so it is matched on the element tree by the shared matcher,
    /// with its target and anchor resolved in the page first: the tree only
    /// knows tag names and labels, so `className`/`id` meant something else
    /// there (VEL-128). Hidden and empty elements never match (VEL-127).
    fn ids_for<'a>(
        page: &'a dyn BrowserProtocol,
        tree: &'a Element,
        selector: &'a Selector,
    ) -> futures::future::BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            if let Some(query) = selector_json(selector) {
                return Ok(Self::capture_all(page, query, selector)
                    .await?
                    .into_iter()
                    .map(|e| e.platform_id)
                    .collect());
            }
            Ok(match selector {
                Selector::Relative {
                    target,
                    relation,
                    anchor,
                } => {
                    let targets = Self::ids_for(page, tree, target).await?;
                    let anchors = Self::ids_for(page, tree, anchor).await?;
                    relative_ids(tree, &targets, *relation, &anchors)
                }
                Selector::Index { selector, index } => {
                    let ids = Self::ids_for(page, tree, selector).await?;
                    // Its on-screen matches in reading order, as the page
                    // counts any other index and explore counted it when it
                    // wrote it (review 10 #1): rendered, a box, and part of
                    // it in the viewport (`vb`).
                    let found: Vec<&Element> = ids
                        .iter()
                        .filter_map(|id| find_by_id(tree, id))
                        .filter(|e| {
                            e.visible
                                && !e.bounds.is_empty()
                                && e.visible_bounds.is_none_or(|v| !v.is_empty())
                        })
                        .collect();
                    velocity_common::selector_match::in_reading_order(found)
                        .into_iter()
                        .nth(*index)
                        .map(|e| e.platform_id.clone())
                        .into_iter()
                        .collect()
                }
                Selector::Compound(parts) => {
                    let mut ids: Option<Vec<String>> = None;
                    for part in parts {
                        let these = Self::ids_for(page, tree, part).await?;
                        ids = Some(match ids {
                            None => these,
                            Some(prev) => prev.into_iter().filter(|i| these.contains(i)).collect(),
                        });
                    }
                    ids.unwrap_or_default()
                }
                // D-11: the regex runs in Rust with the devices' semantics.
                // `textMatches` on the element tree (text, aria-label or
                // id, placeholder), shown elements only, innermost first in
                // tree order; `idMatches` on the page's ids and test ids.
                Selector::TextMatches(_) => velocity_common::selector_match::find_all_tree_order(
                    tree,
                    selector,
                    Some(&page_rect()),
                )
                .into_iter()
                .map(|e| e.platform_id.clone())
                .collect(),
                Selector::IdMatches(pattern) => {
                    let all = page.call("idsOf", vec![]).await?;
                    all.as_array()
                        .into_iter()
                        .flatten()
                        .filter(|n| {
                            n["ids"].as_array().is_some_and(|ids| {
                                ids.iter().filter_map(|i| i.as_str()).any(|i| {
                                    velocity_common::selector_match::pattern_matches(pattern, i)
                                })
                            })
                        })
                        .filter_map(|n| n["v"].as_str().map(str::to_string))
                        .collect()
                }
                _ => Vec::new(),
            })
        })
    }

    /// Every element `selector` (one with a relation in it) matches.
    async fn resolve_on_tree(
        page: &dyn BrowserProtocol,
        selector: &Selector,
    ) -> Result<Vec<Element>> {
        // `textMatches` is a whole match: it must see the whole text, not
        // the first 200 characters (review MINOR 10).
        let limit = if has_text_matches(selector) {
            0
        } else {
            WALK_TEXT_LIMIT
        };
        let tree = Self::fetch_hierarchy_with(page, limit).await?;
        let ids = Self::ids_for(page, &tree, selector).await?;
        Ok(ids
            .iter()
            .filter_map(|id| find_by_id(&tree, id).cloned())
            .collect())
    }

    /// A selector the browser can't parse. Not "no such element": that made
    /// every `assertNotVisible` with a typo pass (VEL-338).
    fn invalid(selector: &Selector, reason: String) -> VelocityError {
        VelocityError::InvalidSelector {
            test_name: String::new(),
            step_index: 0,
            reason: format!("{selector}: {reason}"),
        }
    }

    /// No match where the lookup could look, but some cross-origin frames
    /// didn't answer: whether the element is there is unknown. Not an
    /// `ElementNotFound`, so `assertNotVisible` keeps reading (and fails
    /// with this at its deadline) instead of passing; a test failure, not
    /// a device error, so it never trips the circuit breaker
    /// (review-fi2 F1).
    fn frames_unknown(selector: &Selector, skipped: &[String]) -> VelocityError {
        VelocityError::AssertionFailed {
            expected: "every cross-origin frame to answer the lookup".into(),
            actual: format!(
                "{selector} not found in the page or the frames that answered; \
                 not searched (no answer in time): {}",
                skipped.join(", ")
            ),
            selector: format!("{selector}"),
            screenshot: None,
        }
    }

    fn not_found(selector: &Selector) -> VelocityError {
        VelocityError::ElementNotFound {
            selector: format!("{selector}"),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        }
    }

    /// Resolve a selector to an Element in one call into the page (a
    /// relation also reads the tree, see [`Self::ids_for`]).
    /// With the number of matches on screen (R5).
    async fn resolve_element(
        page: &dyn BrowserProtocol,
        selector: &Selector,
    ) -> Result<(Element, Option<usize>)> {
        match selector_json(selector) {
            Some(query) => Self::capture(page, query, selector).await,
            None => {
                // A shown match first, as the in-page `pick` does.
                let all = Self::resolve_on_tree(page, selector).await?;
                let on_screen = |e: &Element| {
                    e.visible
                        && !e.bounds.is_empty()
                        && e.visible_bounds.is_none_or(|v| !v.is_empty())
                };
                let n = all.iter().filter(|e| on_screen(e)).count();
                let shown = all
                    .iter()
                    .position(|e| e.visible && !e.bounds.is_empty())
                    .unwrap_or(0);
                all.into_iter()
                    .nth(shown)
                    .map(|e| (e, Some(n)))
                    .ok_or_else(|| Self::not_found(selector))
            }
        }
    }

    /// Playwright-style actionability gate for an already-resolved element.
    ///
    /// Each attempt (`act` in `injected.js`) scrolls the element into view,
    /// checks it is visible and enabled, waits two animation frames for its
    /// box to hold still, and hit-tests its centre; it returns the fresh,
    /// post-scroll click point. A blocked attempt is retried with backoff
    /// (Playwright's `[0, 20, 100, 100, 500]` ms), and a covered element is
    /// scrolled to the next alignment (end, center, start) to get out from
    /// under a sticky header or banner. The common case is one round-trip.
    ///
    /// The step's actionability rule decides the rest (D-3, WA-15):
    /// strict (a suite's default) also fails an element still moving at
    /// the deadline, one whose click lands on an ancestor (an overlay), and
    /// one inside an `aria-disabled` subtree; lenient (`actionability:
    /// lenient`, and direct driver calls) taps those with a warning;
    /// `force: true` checks nothing.
    async fn actionable_point(page: &dyn BrowserProtocol, element: &Element) -> Result<(f64, f64)> {
        const BACKOFF_MS: [u64; 5] = [0, 20, 100, 100, 500];
        let deadline = Instant::now() + ACTIONABLE_WAIT;
        let mode = act_mode_arg();
        let mut align = 0u64;
        let mut tries = 0usize;
        let warn = |v: &serde_json::Value| {
            for w in v["warn"].as_array().into_iter().flatten() {
                tracing::warn!(
                    "velocity-web: tapping <{}>: {}",
                    element.element_type,
                    w.as_str().unwrap_or_default()
                );
            }
        };
        let point = |v: &serde_json::Value| {
            (
                v["x"].as_f64().unwrap_or(0.0),
                v["y"].as_f64().unwrap_or(0.0),
            )
        };
        let (reason, last) = loop {
            let v = page
                .call_el("act", &element.platform_id, vec![align.into(), mode.into()])
                .await?;
            if v["ok"].as_bool().unwrap_or(false) {
                warn(&v);
                return Ok(point(&v));
            }
            let reason = v["reason"].as_str().unwrap_or("not actionable").to_string();
            if reason.starts_with("obscured") || reason == "outside viewport" {
                align += 1;
            }
            let wait = Duration::from_millis(BACKOFF_MS[tries.min(BACKOFF_MS.len() - 1)]);
            tries += 1;
            if Instant::now() + wait >= deadline {
                break (reason, v);
            }
            tokio::time::sleep(wait).await;
        };
        if last["soft"].as_bool().unwrap_or(false) {
            warn(&last);
            tracing::warn!(
                "velocity-web: <{}> was still moving after {ACTIONABLE_WAIT:?}; tapped it where it is",
                element.element_type
            );
            return Ok(point(&last));
        }
        Err(not_actionable(element, &reason))
    }

    /// Clicks (or double-clicks, or long-presses) `element` once it is
    /// actionable, with the in-page hit-target check armed for the click
    /// itself: when something else is under the pointer by the time the
    /// press arrives (a banner or toast inserted between the check and
    /// the click, or by the pointer's arrival), the click is blocked in the
    /// page and the whole sequence is tried again (WA-2).
    ///
    /// For an element in a cross-origin frame (W4) the listener is in the
    /// frame, and a click something on the top page took never reaches it:
    /// a click the frame didn't see is blocked too (review-i2 M1).
    /// Returns where it clicked.
    async fn pointer_action(
        page: &dyn BrowserProtocol,
        element: &Element,
        kind: Pointer,
    ) -> Result<(f64, f64)> {
        const MAX_BLOCKED: usize = 3;
        let id = element.platform_id.as_str();
        let in_frame = crate::frames::Frames::split(id).is_some();
        // The page only waits for a click it hasn't seen when the target
        // is in a frame (it says `nested`).
        let disarm_args = vec![serde_json::Value::from(FRAME_CLICK_WAIT_MS)];
        let mut intruder = String::new();
        for _ in 0..MAX_BLOCKED {
            let (x, y) = Self::actionable_point(page, element).await?;
            // `force: true` clicks wherever the target is, whatever covers it.
            let forced = velocity_common::actionability::mode()
                == velocity_common::actionability::ActMode::Force;
            let armed = !forced
                && page
                    .call_el("arm", id, vec![])
                    .await?
                    .as_bool()
                    .unwrap_or(false);
            let done = match kind {
                Pointer::Click => page.click(x, y).await,
                Pointer::Double => page.double_click(x, y).await,
                Pointer::Long(ms) => page.long_press(x, y, ms).await,
            };
            // The click may have navigated: no answer (or a new document's
            // library, never armed) means nothing was blocked.
            let verdict = if armed {
                page.call_near(id, DISARM_TIMEOUT, "disarm", disarm_args.clone())
                    .await
                    .unwrap_or_default()
            } else {
                serde_json::Value::Null
            };
            done?;
            // An old library (or a frame that navigated) says nothing of
            // `seen`: nothing was blocked.
            // A same-origin frame too: a banner on the page over its
            // <iframe> takes the click, and the frame sees nothing
            // (review 10 #2).
            let missed = (in_frame || verdict["nested"] == true) && verdict["seen"] == false;
            match verdict["blocked"].as_str() {
                None if missed => {
                    debug!(
                        "velocity-web: the click on <{}> never reached its frame; retrying",
                        element.element_type
                    );
                    intruder = "something outside its frame (the click never reached the frame)"
                        .to_string();
                }
                None => return Ok((x, y)),
                Some(who) => {
                    debug!(
                        "velocity-web: the click on <{}> would have landed on {who}; retrying",
                        element.element_type
                    );
                    intruder = who.to_string();
                }
            }
        }
        Err(not_actionable(
            element,
            &format!("the click kept landing on {intruder}: the page moved under the pointer"),
        ))
    }

    /// `inputText`: a `<select>` chooses an option; anything else is
    /// focused by an actionability-gated click, the caret goes to the end,
    /// the focus is checked (D-5) and the text inserted in one go.
    async fn type_into(
        page: &dyn BrowserProtocol,
        device_id: &str,
        element: &Element,
        text: &str,
    ) -> Result<()> {
        // A <select> (or its label) chooses the option whose value or label
        // is `text` (D-14): no click, which would open the popup, and
        // typing never reached it (VEL-133).
        let id = element.platform_id.as_str();
        // An <input type=file> (or its label) gets the files whose paths
        // are `text`, one per line, as Playwright's `setInputFiles`: typed
        // text never reached it, and the step failed with "focus is on
        // <input>, which takes no text" (review 10 #11).
        let file = page.call_el("fileInput", id, vec![]).await?;
        if file.is_object() {
            return Self::set_files(page, device_id, element, &file, text).await;
        }
        if page.call_el("isSelect", id, vec![]).await?.as_bool() == Some(true) {
            Self::actionable_point(page, element).await?;
            let v = page.call_el("selectOption", id, vec![text.into()]).await?;
            if let Some(why) = v["disabled"].as_str() {
                return Err(not_actionable(element, why));
            }
            if let Some(err) = v["error"].as_str() {
                return Err(VelocityError::CommandFailed {
                    device_id: device_id.into(),
                    command: "inputText".into(),
                    detail: err.to_string(),
                });
            }
            return Ok(());
        }
        // Review MINOR 10: which opaque (cross-origin) frame had the focus
        // before the click. Focus that stays in the same one is focus the
        // click didn't move: typing would go into that frame's field.
        // A target inside an attached cross-origin frame (W4) has its focus
        // checked in that frame below; this check is for targets outside.
        let in_frame = crate::frames::Frames::split(&element.platform_id).is_some();
        let frame_before = if in_frame {
            None
        } else {
            page.opaque_focus().await
        };
        // Focus via an actionability-gated click, then insert atomically.
        let (x, y) = Self::pointer_action(page, element, Pointer::Click).await?;
        // Unless that frame is the target's own: the click landed on its
        // <iframe> (a card widget's container clicked on the frame it
        // holds, review-i1-i3 M2). Holding the <iframe> isn't enough: a
        // click beside it may have moved nothing (review-fi2 F6).
        if frame_before.is_some()
            && element.element_type != "iframe"
            && element.element_type != "frame"
            && focus_check_enabled()
            && page.opaque_focus().await == frame_before
            && page
                .call_el("focusWithin", id, vec![x.into(), y.into()])
                .await?
                .as_bool()
                != Some(true)
        {
            return Err(not_editable(element, Some("iframe")));
        }
        // The click put the caret under the pointer; type at the end. A
        // date, time, colour or range input is set by value instead: typed
        // text never reached a date picker's `.value` (NB-2).
        // In the element's own document: a field in a cross-origin frame
        // has its focus there (W4).
        let focus = page
            .call_near(id, cdp::CDP_TIMEOUT, "caretToEnd", vec![text.into()])
            .await?;
        if let Some(err) = focus["error"].as_str() {
            return Err(VelocityError::CommandFailed {
                device_id: device_id.into(),
                command: "inputText".into(),
                detail: err.to_string(),
            });
        }
        if focus["mode"] == "set" {
            return Ok(());
        }
        if focus["moveEnd"] == true {
            // No selection API (type=email/number): the caret stayed where
            // the click put it, mid-value (NB-7). Move it with the editing
            // command instead.
            page.move_caret_to_end().await?;
        }
        if focus["editable"] == false && focus_check_enabled() {
            // D-5 (VEL-133): typing into something that takes no text did
            // nothing and passed. Fail, as Playwright's `fill` does.
            return Err(not_editable(element, focus["tag"].as_str()));
        }
        // Key by key, as a user types (React's controlled inputs see that
        // too), so `keydown`/`keyup` handlers run (review 10 #7).
        page.type_text(text).await
    }

    /// `inputText` on a file input: the paths in `text` (one per line,
    /// relative ones from the working directory) must exist; then the
    /// input gets them. `file` is the page's `fileInput` reply.
    async fn set_files(
        page: &dyn BrowserProtocol,
        device_id: &str,
        element: &Element,
        file: &serde_json::Value,
        text: &str,
    ) -> Result<()> {
        if file["disabled"] == true {
            return Err(not_actionable(element, "disabled"));
        }
        let paths = file_paths(text)?;
        if paths.len() > 1 && file["multiple"] != true {
            return Err(VelocityError::CommandFailed {
                device_id: device_id.into(),
                command: "inputText".into(),
                detail: format!(
                    "the file input takes one file (no `multiple`), and {} were given",
                    paths.len()
                ),
            });
        }
        page.set_files(&element.platform_id, paths).await
    }

    /// After a navigation: a Flutter web app's semantics tree is enabled
    /// once (its "Enable accessibility" placeholder is clicked), so its
    /// `Semantics(identifier:)` ids and labels exist to be found, as
    /// Maestro's `ensureSemantics` does. `VELOCITY_WEB_FLUTTER_SEMANTICS=off`
    /// leaves it alone.
    async fn enable_flutter_semantics(page: &Page) {
        if matches!(
            velocity_common::env::var("VELOCITY_WEB_FLUTTER_SEMANTICS").as_deref(),
            Ok("off") | Ok("0") | Ok("false")
        ) {
            return;
        }
        match page.call("flutterSemantics", vec![]).await {
            Ok(v) if v.as_bool() == Some(true) => {
                debug!("velocity-web: enabled Flutter web semantics")
            }
            Ok(_) => {}
            Err(e) => debug!("velocity-web: Flutter semantics check failed: {e}"),
        }
    }

    /// Load `url` as a new document: the load event, else `readyState` past
    /// "loading", all within one `NAV_TIMEOUT`. A refused or never-loading
    /// page is a runtime failure, not a configuration error: it was exit 2
    /// and never retried, and a slow launch took up to 30 s (VEL-123).
    async fn load(page: &Page, url: &str) -> Result<()> {
        let start = Instant::now();
        match tokio::time::timeout(NAV_TIMEOUT, page.page.goto(url)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(e)) if !page.is_alive() || cdp::is_connection_error(&e) => {
                return Err(cdp::lost(e))
            }
            Err(_) if !page.is_alive() => return Err(cdp::lost("navigation never finished")),
            Ok(Err(CdpError::ChromeMessage(m))) => {
                return Err(VelocityError::Internal(anyhow::anyhow!(
                    "could not load {url}: {m}"
                )))
            }
            Ok(Err(e)) => debug!("goto({url}): {e}; checking readyState"),
            Err(_) => debug!("goto({url}) gave no load event in {NAV_TIMEOUT:?}"),
        }
        let left = NAV_TIMEOUT.saturating_sub(start.elapsed());
        Self::wait_until(page, left, |_, ready| ready != "loading").await;
        let ready: Option<String> = page.eval("document.readyState").await?.into_value().ok();
        if ready.as_deref() == Some("loading") {
            return Err(VelocityError::Internal(anyhow::anyhow!(
                "{url} did not finish loading within {NAV_TIMEOUT:?}"
            )));
        }
        Ok(())
    }

    /// `launchApp clearState`: every cookie and the app origin's storage,
    /// through the browser rather than the page. Clearing only the current
    /// page's local/session storage left cookies, and cleared nothing at all
    /// after `stopApp` (the page is `about:blank`) (VEL-266).
    async fn clear_browser_state(page: &Page, app_id: &str) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::network::ClearBrowserCookiesParams;
        use chromiumoxide::cdp::browser_protocol::storage::ClearDataForOriginParams;
        page.exec(ClearBrowserCookiesParams::default()).await?;
        let current = Self::href(page).await.unwrap_or_default();
        let mut origins: Vec<String> = [app_id, current.as_str()]
            .into_iter()
            .filter_map(web_origin)
            .collect();
        origins.dedup();
        for origin in origins {
            page.exec(ClearDataForOriginParams::new(origin, "all"))
                .await?;
        }
        // Session storage (per tab) and file:// pages aren't covered above.
        let _ = page
            .eval("try{localStorage.clear();sessionStorage.clear();}catch(e){}")
            .await;
        Ok(())
    }

    /// The element's live `{visible, text}`, `None` once it left the page.
    /// The captured element is a snapshot; reading it back served a
    /// vanished or changed element as still there (VEL-057).
    async fn live_state(
        page: &dyn BrowserProtocol,
        element: &Element,
    ) -> Result<Option<serde_json::Value>> {
        let v = page.call_el("state", &element.platform_id, vec![]).await?;
        Ok((!v.is_null()).then_some(v))
    }

    /// `device_id`'s page handle, cloned out so the sessions lock isn't
    /// held across the `await`s that follow (network round-trips,
    /// actionability polling). A device the driver lists gets its browser
    /// on first use: the parallel runner leases `web-3` without preparing it.
    /// [`Self::page_handle`], for the live screencast (`crate::live`).
    pub(crate) async fn page_for_live(&self, device_id: &str) -> Result<Arc<Page>> {
        self.page_handle(device_id).await
    }

    async fn page_handle(&self, device_id: &str) -> Result<Arc<Page>> {
        let follow = match self.sessions.lock().await.get_mut(device_id) {
            Some(s) => match s.tabs.next_change(&s.page, &s.openers) {
                None => return Ok(s.page.clone()),
                Some(change) => Some((change, s.chrome.clone(), s.network.clone())),
            },
            None => None,
        };
        if let Some((change, chrome, network)) = follow {
            return self.follow_tab(device_id, change, &chrome, &network).await;
        }
        if !self.device_ids().iter().any(|d| d == device_id) {
            return Err(self.no_session(device_id));
        }
        let lock = self.launch_lock(device_id);
        let _launching = lock.lock().await;
        if let Some(s) = self.sessions.lock().await.get(device_id) {
            return Ok(s.page.clone());
        }
        let session = self.launch(device_id).await?;
        let page = session.page.clone();
        self.sessions
            .lock()
            .await
            .insert(device_id.to_string(), session);
        Ok(page)
    }

    /// Drive the tab `change` says: a new one the page opened, or the
    /// opener of one that closed. The sessions lock is not held while the
    /// new tab is set up.
    async fn follow_tab(
        &self,
        device_id: &str,
        change: TabChange,
        chrome: &Arc<Chrome>,
        network: &crate::network::Network,
    ) -> Result<Arc<Page>> {
        let opened = match change {
            TabChange::Opened(id) => {
                let found = self.adopt_tab(chrome, &id, network).await;
                if found.is_none() {
                    // Not a page that can be driven (closed already, or
                    // never attached): forget it.
                    if let Some(s) = self.sessions.lock().await.get(device_id) {
                        s.tabs.forget(&id);
                    }
                }
                found
            }
            TabChange::Closed => None,
        };
        let mut sessions = self.sessions.lock().await;
        let s = sessions
            .get_mut(device_id)
            .ok_or_else(|| self.no_session(device_id))?;
        match opened {
            Some(tab) => {
                tracing::warn!(
                    "velocity-web: the page opened a new tab ({}); following it",
                    tab.page.url().await.ok().flatten().unwrap_or_default()
                );
                let opener = std::mem::replace(&mut s.page, tab);
                s.openers.push(opener);
            }
            None => {
                // Back to the newest opener still open.
                while s.tabs.is_gone(&s.page) {
                    let Some(opener) = s.openers.pop() else { break };
                    tracing::warn!("velocity-web: the tab closed; back to the one that opened it");
                    s.page = opener;
                }
            }
        }
        s.hierarchy_cache = None;
        Ok(s.page.clone())
    }

    /// The tab `id` set up to be driven as `device_id`'s page is; `None`
    /// when it can't be had.
    async fn adopt_tab(
        &self,
        chrome: &Chrome,
        id: &str,
        network: &crate::network::Network,
    ) -> Option<Arc<Page>> {
        use chromiumoxide::cdp::browser_protocol::target::TargetId;
        // chromiumoxide attaches a new target on its own, a moment after
        // Chrome announces it.
        let deadline = Instant::now() + Duration::from_secs(2);
        let page = loop {
            let got = {
                let browser = chrome.browser.lock().await;
                tokio::time::timeout(
                    PING_TIMEOUT,
                    browser.get_page(TargetId::from(id.to_string())),
                )
                .await
            };
            match got {
                Ok(Ok(p)) => break p,
                failed if Instant::now() >= deadline => {
                    debug!("velocity-web: the new tab {id} can't be driven: {failed:?}");
                    return None;
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        Some(self.set_up_tab(chrome, page, network).await)
    }

    /// `device_id`'s network rules and requests, if its browser runs.
    async fn network(&self, device_id: &str) -> Option<crate::network::Network> {
        self.sessions
            .lock()
            .await
            .get(device_id)
            .map(|s| s.network.clone())
    }

    /// Start a browser for `device_id`: its own Chrome process and
    /// profile, or (context mode, W3) a fresh browser context in the
    /// shared Chrome.
    async fn launch(&self, device_id: &str) -> Result<BrowserSession> {
        if !self.uses_contexts() {
            let chrome = self.start_chrome(device_id).await?;
            return match self.open_page(&chrome, None, device_id).await {
                Ok((page, network, tabs)) => Ok(Self::session(chrome, None, page, network, tabs)),
                Err(e) => {
                    chrome.close().await;
                    Err(e)
                }
            };
        }
        let chrome = self.shared_chrome(device_id).await?;
        use chromiumoxide::cdp::browser_protocol::target::CreateBrowserContextParams;
        let context = {
            let browser = chrome.browser.lock().await;
            match tokio::time::timeout(
                cdp::CDP_TIMEOUT,
                browser.create_browser_context(CreateBrowserContextParams::default()),
            )
            .await
            {
                Ok(Ok(id)) => id,
                failed => {
                    let detail = match failed {
                        Ok(Err(e)) => e.to_string(),
                        _ => format!("no answer in {:?}", cdp::CDP_TIMEOUT),
                    };
                    return Err(VelocityError::DeviceBootFailed {
                        id: device_id.into(),
                        reason: format!("Failed to create a browser context: {detail}"),
                    });
                }
            }
        };
        match self
            .open_page(&chrome, Some(context.clone()), device_id)
            .await
        {
            Ok((page, network, tabs)) => {
                Ok(Self::session(chrome, Some(context), page, network, tabs))
            }
            Err(e) => {
                Self::dispose_context(&chrome, context).await;
                Err(e)
            }
        }
    }

    fn session(
        chrome: Arc<Chrome>,
        context: Option<chromiumoxide::cdp::browser_protocol::browser::BrowserContextId>,
        page: Arc<Page>,
        network: crate::network::Network,
        tabs: TabWatch,
    ) -> BrowserSession {
        BrowserSession {
            chrome,
            context,
            page,
            openers: Vec::new(),
            tabs,
            network,
            hierarchy_cache: None,
            cache_ttl: Duration::from_millis(1500),
        }
    }

    /// The shared Chrome of context mode, started on first use and again
    /// once the one there has died (a restart after a crash).
    ///
    /// A shared Chrome that is up but doesn't answer a browser-level ping
    /// (hung, its socket still open) is killed and replaced too: before,
    /// every device's context creation timed out against it, test after
    /// test (review-i2 m2).
    async fn shared_chrome(&self, device_id: &str) -> Result<Arc<Chrome>> {
        let mut shared = self.shared.lock().await;
        if let Some(c) = shared.as_ref().filter(|c| c.is_alive()) {
            if c.answers().await {
                return Ok(c.clone());
            }
            tracing::warn!(
                "velocity-web: the shared Chrome doesn't answer; killing it and starting another"
            );
        }
        if let Some(dead) = shared.take() {
            dead.close().await;
        }
        let chrome = self.start_chrome(device_id).await?;
        *shared = Some(chrome.clone());
        Ok(chrome)
    }

    /// Dispose of a device's browser context: its pages close and its
    /// cookies, storage and cache go with it.
    async fn dispose_context(
        chrome: &Chrome,
        context: chromiumoxide::cdp::browser_protocol::browser::BrowserContextId,
    ) {
        if !chrome.is_alive() {
            return;
        }
        let browser = chrome.browser.lock().await;
        if let Err(e) =
            tokio::time::timeout(CLOSE_TIMEOUT, browser.dispose_browser_context(context)).await
        {
            debug!("velocity-web: disposing a browser context: {e}");
        }
    }

    /// Start one Chrome process with a profile of its own.
    async fn start_chrome(&self, device_id: &str) -> Result<Arc<Chrome>> {
        // The viewport too: chromiumoxide otherwise emulates 800x600 on every
        // page, so pages rendered smaller than `screen_size` reports and
        // screenshot masks landed in the wrong place.
        let device = self.config.device;
        let touch = device.is_some_and(|d| d.touch);
        let mut builder = BrowserConfig::builder()
            .window_size(self.config.width, self.config.height)
            .viewport(chromiumoxide::handler::viewport::Viewport {
                width: self.config.width,
                height: self.config.height,
                device_scale_factor: Some(device.map_or(1.0, |d| d.dpr)),
                emulating_mobile: device.is_some_and(|d| d.mobile),
                has_touch: touch,
                ..Default::default()
            });

        if !self.config.headless {
            builder = builder.with_head();
        }

        // chromiumoxide ignores certificate errors unless told otherwise
        // (`Security.setIgnoreCertificateErrors` on every tab), so a forged
        // certificate on a hostile network loaded silently and the next
        // secret was typed into the attacker's page (audit 2026-11 WEB-1).
        if !insecure_certs() {
            builder = builder.respect_https_errors();
        }

        if let Some(ref path) = self.config.chrome_path {
            builder = builder.chrome_executable(path);
        }

        for arg in &self.config.extra_args {
            builder = match chrome_arg(arg) {
                (key, Some(value)) => builder.arg((key, value)),
                (key, None) => builder.arg(key),
            };
        }

        // A profile of its own: chromiumoxide's default is one fixed
        // `$TMPDIR/chromiumoxide-runner`, and Chrome's singleton lock on it
        // made a second concurrent run fail to launch (VEL-117).
        let profile_dir = new_profile_dir().map_err(|e| VelocityError::DeviceBootFailed {
            id: device_id.into(),
            reason: format!("cannot create a Chrome profile directory: {e}"),
        })?;
        builder = builder.user_data_dir(&profile_dir);

        let cfg = builder
            .build()
            .map_err(|e| VelocityError::Config(format!("Chrome config error: {e}")))?;

        let (mut browser, mut handler) = match Browser::launch(cfg).await {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile_dir);
                return Err(VelocityError::DeviceBootFailed {
                    id: device_id.into(),
                    reason: format!("Failed to launch Chrome: {e}"),
                });
            }
        };
        let registered = browser
            .get_mut_child()
            .and_then(|c| c.inner.id())
            .map(|pid| velocity_common::process::register_pid(pid, Some(profile_dir.clone())));

        // The CDP event handler must be polled on a background task. When it
        // stops (the socket closed: Chrome exited or was killed), every
        // later call fails fast as a lost connection instead of hanging.
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e @ (CdpError::Ws(_) | CdpError::Io(_))) = event {
                    debug!("velocity-web: DevTools connection closed: {e}");
                    break;
                }
            }
            flag.store(false, Ordering::SeqCst);
        });
        let ws_url = browser.websocket_address().clone();
        Ok(Arc::new(Chrome {
            browser: Mutex::new(browser),
            alive,
            ws_url,
            profile_dir,
            _registered: registered,
        }))
    }

    /// A page of `device_id`'s browser made ready to drive: touch and user
    /// agent as the profile says, JS dialogs answered, crashes watched, the
    /// network's requests and mocks, and its cross-origin frames.
    async fn set_up_tab(
        &self,
        chrome: &Chrome,
        page: chromiumoxide::Page,
        network: &crate::network::Network,
    ) -> Arc<Page> {
        let device = self.config.device;
        let touch = device.is_some_and(|d| d.touch);
        let page = Arc::new(Page::new(page, chrome.alive.clone()).with_touch(touch));
        // chromiumoxide turns touch emulation on for every page, whatever
        // the viewport says, so a 1280x800 desktop page reported
        // `maxTouchPoints > 0` and `ontouchstart`, and sites served their
        // touch UI (NB-4, WA-5). On only for a touch profile (D-W3).
        {
            use chromiumoxide::cdp::browser_protocol::emulation::SetTouchEmulationEnabledParams;
            let mut params = SetTouchEmulationEnabledParams::new(touch);
            if touch {
                params.max_touch_points = Some(5);
            }
            if let Err(e) = page.exec(params).await {
                debug!("velocity-web: could not set touch emulation: {e}");
            }
        }
        if let Some(d) = device {
            use chromiumoxide::cdp::browser_protocol::emulation::SetUserAgentOverrideParams;
            if let Err(e) = page
                .exec(SetUserAgentOverrideParams::new(d.user_agent))
                .await
            {
                tracing::warn!("velocity-web: {}: user agent not set: {e}", d.name);
            }
        }
        spawn_dialog_handler(&page).await;
        spawn_crash_watch(&page).await;
        network.attach(&page).await;
        if frames_enabled() {
            // Cross-origin frames (W4): their own connection, attached
            // before the first navigation so no frame is missed.
            let ws = chrome.browser.lock().await.websocket_address().clone();
            match crate::frames::Frames::connect(&ws, page.page.target_id().inner()).await {
                Ok(frames) => {
                    frames.set_network(Arc::new(network.clone()));
                    network.frames_attached();
                    page.set_frames(frames);
                }
                Err(e) => debug!("velocity-web: cross-origin frames not tracked: {e}"),
            }
        }
        page
    }

    /// Open `device_id`'s page in `chrome` (in `context`, if given), set it
    /// up (touch, user agent, dialogs, network) and load the start URL.
    async fn open_page(
        &self,
        chrome: &Chrome,
        context: Option<chromiumoxide::cdp::browser_protocol::browser::BrowserContextId>,
        device_id: &str,
    ) -> Result<(Arc<Page>, crate::network::Network, TabWatch)> {
        use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
        let mut params = CreateTargetParams::new("about:blank");
        params.browser_context_id = context.clone();
        let opened = {
            let browser = chrome.browser.lock().await;
            tokio::time::timeout(cdp::CDP_TIMEOUT, browser.new_page(params)).await
        };
        let page = match opened {
            Ok(Ok(p)) => p,
            failed => {
                let detail = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => format!("no answer in {:?}", cdp::CDP_TIMEOUT),
                };
                return Err(VelocityError::DeviceBootFailed {
                    id: device_id.into(),
                    reason: format!("Failed to open page: {detail}"),
                });
            }
        };

        let network = crate::network::Network::new(self.config.mocks.clone());
        let tabs = TabWatch::default();
        let page = self.set_up_tab(chrome, page, &network).await;
        tabs.watch(chrome, &page).await;
        watch_downloads(chrome, context, &page).await;

        // Navigate to start URL if configured.
        if let Some(ref url) = self.config.start_url {
            if let Err(e) = Self::load(&page, url).await {
                let _ = page.page.clone().close().await;
                return Err(e);
            }
        }

        debug!("velocity-web: Chrome session ready for {device_id}");
        Ok((page, network, tabs))
    }

    /// Close a device's browser: its context (context mode, the shared
    /// Chrome stays for the other devices), or its Chrome and profile.
    async fn close(s: BrowserSession) {
        match s.context {
            Some(ctx) => Self::dispose_context(&s.chrome, ctx).await,
            None => s.chrome.close().await,
        }
    }

    /// Replace `device_id`'s browser (if any) with a fresh one.
    async fn relaunch(&self, device_id: &str) -> Result<()> {
        let lock = self.launch_lock(device_id);
        let _launching = lock.lock().await;
        // A second `prepare` replaces the session: close the old browser
        // and remove its profile, which leaked when it was just dropped.
        let old = self.sessions.lock().await.remove(device_id);
        if let Some(old) = old {
            Self::close(old).await;
        }
        let session = self.launch(device_id).await?;
        self.sessions
            .lock()
            .await
            .insert(device_id.to_string(), session);
        Ok(())
    }
}

#[async_trait::async_trait]
impl velocity_common::PlatformDriver for WebDriver {
    // ──────────────────────────────────────────────
    // Lifecycle
    // ──────────────────────────────────────────────

    async fn prepare(&self, device_id: &str) -> Result<()> {
        self.relaunch(device_id).await
    }

    async fn network_busy(&self, device_id: &str) -> bool {
        self.network(device_id).await.is_some_and(|n| n.busy())
    }

    /// The page's own account of whether it has settled after an action:
    /// DOM mutations (a MutationObserver) and running finite animations,
    /// checked every frame (`settle` in `injected.js`). Web had no UI
    /// settle at all, only the network wait, so a step after a render
    /// without a request acted on the old screen (VEL-067).
    async fn settle_on_device(
        &self,
        device_id: &str,
        reaction_ms: u64,
        quiet_ms: u64,
        cap_ms: u64,
    ) -> Option<velocity_common::DeviceSettle> {
        let page = self.page_handle(device_id).await.ok()?;
        let limit = Duration::from_millis(cap_ms) + cdp::CDP_TIMEOUT;
        let args = vec![reaction_ms.into(), quiet_ms.into(), cap_ms.into()];
        match page.call_for(limit, "settle", args).await {
            Ok(v) => Some(velocity_common::DeviceSettle {
                settled: v["settled"].as_bool().unwrap_or(false),
                reacted: v["reacted"].as_bool().unwrap_or(false),
                elapsed_ms: v["elapsed"].as_u64().unwrap_or(0),
            }),
            // Mid-navigation (the document went away under the probe):
            // the caller falls back to what it does without one.
            Err(e) => {
                debug!("velocity-web: settle probe failed: {e}");
                None
            }
        }
    }

    async fn add_mock(&self, device_id: &str, rule: velocity_common::MockRule) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        let network = self
            .network(device_id)
            .await
            .ok_or_else(|| self.no_session(device_id))?;
        network.add_test_rule(&page, rule).await
    }

    async fn clear_mocks(&self, device_id: &str) -> Result<()> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(device_id)
                .map(|s| (s.page.clone(), s.network.clone()))
        };
        match session {
            Some((page, network)) => network.clear_test_rules(&page).await,
            None => Ok(()),
        }
    }

    /// Every browser closes, side by side, and its profile goes.
    ///
    /// Context mode: the contexts aren't disposed one by one (each waited
    /// on the shared Chrome's lock, so N stuck ones took N x 3 s, past the
    /// stop grace: review-i2 m3); closing the shared Chrome right after
    /// takes them all. Everything together is bounded by
    /// `CLEANUP_TIMEOUT`; a Chrome left then is killed when dropped.
    async fn cleanup(&self) {
        let all: Vec<BrowserSession> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        let shared = self.shared.lock().await.take();
        let own: Vec<BrowserSession> = all
            .into_iter()
            .filter(|s| s.context.is_none() || shared.is_none())
            .collect();
        let close_all = async {
            futures::future::join_all(own.into_iter().map(Self::close)).await;
            // Context mode: the shared Chrome goes, and its contexts with it.
            if let Some(chrome) = &shared {
                chrome.close().await;
            }
        };
        if tokio::time::timeout(CLEANUP_TIMEOUT, close_all)
            .await
            .is_err()
        {
            // Dropped, each Chrome left is killed and its profile removed.
            tracing::warn!(
                "velocity-web: browsers still closing after {CLEANUP_TIMEOUT:?}; killed"
            );
        }
    }

    /// Healthy when every running browser answers; no browser at all is
    /// unhealthy, as before.
    async fn health_check(&self) -> velocity_common::HealthStatus {
        let pages: Vec<Arc<Page>> = self
            .sessions
            .lock()
            .await
            .values()
            .map(|s| s.page.clone())
            .collect();
        if pages.is_empty() {
            return velocity_common::HealthStatus::Unhealthy;
        }
        let answers = futures::future::join_all(
            pages
                .iter()
                .map(|p| p.run_for(HEALTH_TIMEOUT, p.page.evaluate("1+1"))),
        )
        .await;
        if answers.iter().all(|a| a.is_ok()) {
            velocity_common::HealthStatus::Healthy
        } else {
            velocity_common::HealthStatus::Unhealthy
        }
    }

    /// One device's browser: answering if it runs, ready to start if not.
    async fn device_ready(&self, device_id: &str) -> bool {
        let page = self
            .sessions
            .lock()
            .await
            .get(device_id)
            .map(|s| s.page.clone());
        match page {
            Some(p) => p
                .run_for(HEALTH_TIMEOUT, p.page.evaluate("1+1"))
                .await
                .is_ok(),
            None => self.device_ids().iter().any(|d| d == device_id),
        }
    }

    /// Restarts the browsers whose connection died, or that don't answer
    /// a ping within `PING_TIMEOUT` (a hung renderer keeps its socket
    /// open, and with several browsers it was never restarted: review-d6
    /// m2). Not the others: with several devices that would pull the page
    /// out from under tests running on them. With one browser that answers,
    /// it is restarted as before (a session that answers but is stuck).
    async fn restart_session(&self) -> Result<()> {
        let pages: Vec<(String, Arc<Page>)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .map(|(id, s)| (id.clone(), s.page.clone()))
                .collect()
        };
        let restart: Vec<String> = match pages.len() {
            0 => vec![self.device_ids()[0].clone()],
            1 => vec![pages[0].0.clone()],
            _ => {
                let answers = futures::future::join_all(pages.iter().map(|(_, p)| async move {
                    p.is_alive()
                        && p.run_for(PING_TIMEOUT, p.page.evaluate("1+1"))
                            .await
                            .is_ok()
                }))
                .await;
                pages
                    .iter()
                    .zip(answers)
                    .filter(|(_, ok)| !ok)
                    .map(|((id, _), _)| id.clone())
                    .collect()
            }
        };
        for id in restart {
            self.relaunch(&id).await?;
        }
        Ok(())
    }

    // ──────────────────────────────────────────────
    // Device management (virtual — browser = the "device")
    // ──────────────────────────────────────────────

    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self
            .device_ids()
            .into_iter()
            .map(|id| DeviceInfo {
                name: if id == "web" {
                    "Chrome Browser".to_string()
                } else {
                    format!("Chrome Browser ({id})")
                },
                id,
                platform: Platform::Web,
                state: DeviceState::Booted,
                os_version: None,
                device_type: DeviceType::Unknown,
            })
            .collect())
    }

    async fn boot_device(&self, _device_id: &str) -> Result<()> {
        Ok(()) // Browser is launched in prepare()
    }

    async fn shutdown_device(&self, device_id: &str) -> Result<()> {
        let session = self.sessions.lock().await.remove(device_id);
        if let Some(s) = session {
            Self::close(s).await;
        }
        Ok(())
    }

    // ──────────────────────────────────────────────
    // App lifecycle (URL navigation)
    // ──────────────────────────────────────────────

    async fn install_app(&self, _device_id: &str, _app_path: &str) -> Result<()> {
        Ok(()) // URLs don't need installation
    }

    /// The page's screencast and console (`crate::evidence`). The page
    /// lives as long as the browser session, so a launch keeps watching it.
    async fn start_evidence(
        &self,
        device_id: &str,
        req: &velocity_common::EvidenceRequest,
    ) -> Option<Box<dyn velocity_common::EvidenceSession>> {
        if !req.video.on() && !req.logs.on() {
            return None;
        }
        let page = self.page_handle(device_id).await.ok()?;
        crate::evidence::WebEvidence::start(page, req)
            .await
            .map(|e| Box::new(e) as Box<dyn velocity_common::EvidenceSession>)
    }

    async fn launch_app(&self, device_id: &str, app_id: &str, clear_state: bool) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        if clear_state {
            Self::clear_browser_state(&page, app_id).await?;
        }
        // A launch is a fresh load. When only the fragment differs from the
        // page we're on (hash-routed apps), `goto` is a same-document
        // navigation: no load event, so it timed out after 30 s on every
        // test after the first (VEL-331). Leave the document first.
        let current = Self::href(&page).await.unwrap_or_default();
        if same_document(&current, app_id) {
            Self::load(&page, "about:blank").await?;
        }
        Self::load(&page, app_id).await?;
        Self::enable_flutter_semantics(&page).await;
        Ok(())
    }

    async fn stop_app(&self, device_id: &str, _app_id: &str) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        Self::load(&page, "about:blank").await
    }

    // ──────────────────────────────────────────────
    // Web navigation (new trait methods)
    // ──────────────────────────────────────────────

    async fn current_url(&self, device_id: &str) -> Option<String> {
        let page = self.page_handle(device_id).await.ok()?;
        Self::href(&page).await
    }

    async fn navigate(&self, device_id: &str, url: &str) -> Result<()> {
        // The page handle, not the session lock: held across the 20 s
        // navigation wait, it made `health_check` queue behind a slow page
        // and report a healthy browser unhealthy (NB-8, WA-9).
        let page = self.page_handle(device_id).await?;
        let current = Self::href(&page).await.unwrap_or_default();
        // As a link would: `/nav.html` or `b.html` against the page, and in
        // the form `location.href` will take (percent-encoded), so the wait
        // below can see it arrive. A relative URL went to `goto` as is and
        // waited out NAV_TIMEOUT without navigating (VEL-271), and a
        // fragment the browser re-encodes was never "arrived" (VEL-125).
        let resolved = page.call("resolveUrl", vec![url.into()]).await?;
        let url = match resolved.as_str() {
            Some(u) => u.to_string(),
            None => {
                return Err(VelocityError::Config(format!(
                "goto {url:?}: not a URL, and there is no page ({current}) to resolve it against"
            )))
            }
        };
        let url = url.as_str();
        if same_document(&current, url) {
            // A fragment-only change fires no load event, so goto() would wait
            // for one that never comes (puppeteer#257). Navigate in-page instead.
            let js = format!("location.href = {}", serde_json::Value::from(url));
            page.eval(js).await.map_err(|e| match e {
                VelocityError::BrowserConnectionLost { .. } => e,
                e => VelocityError::Config(format!("navigate({url}) failed: {e}")),
            })?;
            Self::wait_until(&page, NAV_TIMEOUT, |href, _| href == url).await;
            if !page.is_alive() {
                return Err(cdp::lost("the browser connection closed during navigation"));
            }
            return Ok(());
        }
        // A new document: the same single deadline as `launchApp`. A
        // refused page is a runtime failure and a dropped socket a lost
        // connection, never a config error (exit 2, no retry); and the wait
        // no longer took up to twice NAV_TIMEOUT and passed while the page
        // was still loading.
        Self::load(&page, url).await?;
        Self::enable_flutter_semantics(&page).await;
        Ok(())
    }

    async fn go_back(&self, device_id: &str) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        // Nothing to go back to: `history.back()` does nothing, and the wait
        // for a new URL ran out its 20 s and passed (VEL-271). The tab's
        // own history knows, cross-origin entries included: the Navigation
        // API's `canGoBack` only sees same-origin ones, so a back from an
        // OAuth or payment page failed (M-2). Without an answer, wait as
        // before.
        let at_start = page
            .exec(GetNavigationHistoryParams::default())
            .await
            .ok()
            .map(|h| {
                // The tab's initial blank page doesn't count as a page.
                let current = usize::try_from(h.result.current_index).unwrap_or(0);
                h.result
                    .entries
                    .iter()
                    .take(current)
                    .all(|e| e.url.is_empty() || e.url == "about:blank")
            });
        if at_start == Some(true) {
            return Err(VelocityError::CommandFailed {
                device_id: device_id.into(),
                command: "goBack".into(),
                detail: "there is no earlier page in this tab's history".into(),
            });
        }
        let before = Self::href(&page).await.unwrap_or_default();
        nav_eval(&page, "history.back()").await?;
        // Wait for the URL to change (same-document or not), then for the load.
        Self::wait_until(&page, NAV_TIMEOUT, |href, ready| {
            href != before && ready != "loading"
        })
        .await;
        if !page.is_alive() {
            return Err(cdp::lost("the browser connection closed during back"));
        }
        Ok(())
    }

    async fn refresh_page(&self, device_id: &str) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        nav_eval(&page, "location.reload()").await?;
        tokio::time::timeout(NAV_TIMEOUT, page.page.wait_for_navigation())
            .await
            .ok();
        Self::wait_until(&page, NAV_TIMEOUT, |_, ready| ready == "complete").await;
        if !page.is_alive() {
            return Err(cdp::lost("the browser connection closed during the reload"));
        }
        Ok(())
    }

    // ──────────────────────────────────────────────
    // Element queries
    // ──────────────────────────────────────────────

    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
        // A miss (or a lookup that counted nothing) leaves no count behind:
        // a stale one made a later heal of the same selector "ambiguous"
        // (review-fixes #6).
        self.match_counts.clear(device_id);
        let page = self.page_handle(device_id).await?;
        let (el, n) = Self::resolve_element(&*page, selector).await?;
        if let Some(n) = n {
            self.match_counts.record(device_id, selector, n);
        }
        Ok(el)
    }

    fn last_match_count(&self, device_id: &str, selector: &Selector) -> Option<usize> {
        self.match_counts.get(device_id, selector)
    }

    async fn find_elements(&self, device_id: &str, selector: &Selector) -> Result<Vec<Element>> {
        let page = self.page_handle(device_id).await?;
        match selector_json(selector) {
            Some(query) => Self::capture_all(&*page, query, selector).await,
            None => Self::resolve_on_tree(&*page, selector).await,
        }
    }

    async fn get_hierarchy(&self, device_id: &str) -> Result<Element> {
        let page = self.page_handle(device_id).await?;
        Self::fetch_hierarchy(&*page).await
    }

    async fn invalidate_hierarchy_cache(&self) {
        for s in self.sessions.lock().await.values_mut() {
            s.hierarchy_cache = None;
        }
    }

    /// Override with TTL-cached hierarchy to avoid redundant CDP round-trips.
    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        // Check cache while holding the lock.
        {
            let sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get(device_id) {
                if let Some((ref tree, ref ts)) = s.hierarchy_cache {
                    if ts.elapsed() < s.cache_ttl {
                        debug!("velocity-web: hierarchy cache hit");
                        return Ok(tree.clone());
                    }
                }
            }
        }
        // Cache miss — fetch from page then update cache.
        let tree = self.get_hierarchy(device_id).await?;
        if let Some(s) = self.sessions.lock().await.get_mut(device_id) {
            s.hierarchy_cache = Some((tree.clone(), Instant::now()));
        }
        Ok(tree)
    }

    // ──────────────────────────────────────────────
    // Actions
    // ──────────────────────────────────────────────

    async fn tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        // An <option> of a closed drop-down has no box to click (its popup
        // is native): it is chosen, as `inputText` on the <select> does.
        // It failed as "zero size" (review 10 #11).
        if element.element_type == "option" {
            let v = page
                .call_el("tapOption", &element.platform_id, vec![])
                .await?;
            if let Some(why) = v["disabled"].as_str() {
                return Err(not_actionable(element, why));
            }
            if v["ok"] == true {
                return Ok(());
            }
        }
        Self::pointer_action(&*page, element, Pointer::Click)
            .await
            .map(drop)
    }

    async fn double_tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        Self::pointer_action(&*page, element, Pointer::Double)
            .await
            .map(drop)
    }

    async fn long_press(&self, device_id: &str, element: &Element, duration_ms: u64) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        Self::pointer_action(&*page, element, Pointer::Long(duration_ms))
            .await
            .map(drop)
    }

    async fn tap_xy(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        // Raw coordinate tap — no element to gate on, so no actionability check.
        let page = self.page_handle(device_id).await?;
        page.click(x as f64, y as f64).await
    }

    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        Self::type_into(&*page, device_id, element, text).await
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()> {
        // Focus via an actionability-gated click, then select-all + delete.
        self.tap(device_id, element).await?;
        let page = self.page_handle(device_id).await?;
        page.select_all_and_delete().await
    }

    async fn input_raw(&self, device_id: &str, text: &str) -> Result<()> {
        // For web, input_raw types into whatever is currently focused.
        let page = self.page_handle(device_id).await?;
        page.type_text(text).await
    }

    /// `swipe: up` is the finger moving up, as on a phone: the content
    /// follows it, so the page scrolls down (D-W4). It was `scrollBy(0,
    /// -300)`, the page moving up, the opposite of every device and of this
    /// driver's own `scroll` (VEL-265). The wheel at the viewport centre,
    /// so the scroller under it moves, not only the window.
    ///
    /// On a touch profile (`web.device`) it is a finger stroke through the
    /// middle of the viewport, 40% of its height (or width) long (W2).
    async fn swipe(&self, device_id: &str, direction: Direction) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        if page.touch() {
            let (from, to) = touch_stroke(self.config.width, self.config.height, direction);
            page.touch_drag(from, to).await?;
            tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
            return Ok(());
        }
        self.scroll(device_id, direction).await
    }

    /// A finger stroke from `from` to `to` (`swipe within:` or `from`/`to`).
    /// Over something that scrolls along the stroke the content follows the
    /// finger, so it is the wheel at `from` by the stroke's length, like
    /// `scroll_coords`: a mouse drag there selects text and scrolls nothing,
    /// and the step passed (VEL-347). Anywhere else (a range slider, a
    /// Swiper carousel, a sortable list, a canvas) it is a pointer drag,
    /// which is what those widgets listen for; they ignore the wheel.
    ///
    /// On a touch profile it is always the finger itself (W2): a touch
    /// stroke scrolls what it is over and drags what listens for touches.
    async fn swipe_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        if page.touch() {
            let f = (f64::from(from.0), f64::from(from.1));
            let t = (f64::from(to.0), f64::from(to.1));
            page.touch_drag(f, t).await?;
            tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
            return Ok(());
        }
        let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
        let scrolls = page
            .call(
                "scrollsAlong",
                vec![from.0.into(), from.1.into(), vertical.into()],
            )
            .await?
            .as_bool()
            .unwrap_or(false);
        if scrolls {
            self.scroll_coords(device_id, from, to).await
        } else {
            page.drag(from, to).await
        }
    }

    async fn drag(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        page.drag_held(from, to, hold_ms, move_ms).await?;
        tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
        Ok(())
    }

    /// `scrollUntilVisible` (and `swipe`) on the web: the wheel, at the
    /// viewport centre. `finger` is a touch direction (up shows what's
    /// below), so the page moves the other way.
    async fn scroll(&self, device_id: &str, finger: Direction) -> Result<()> {
        let (dx, dy) = match finger {
            Direction::Up => (0.0, WHEEL_STEP),
            Direction::Down => (0.0, -WHEEL_STEP),
            Direction::Left => (WHEEL_STEP, 0.0),
            Direction::Right => (-WHEEL_STEP, 0.0),
        };
        let (w, h) = (f64::from(self.config.width), f64::from(self.config.height));
        let page = self.page_handle(device_id).await?;
        page.wheel(w / 2.0, h / 2.0, dx, dy).await?;
        tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
        Ok(())
    }

    /// A scroll stroke from `from` to `to` (inside a `within:` container, or
    /// an aimed drag): the wheel at `from`, by the stroke's length. A mouse
    /// drag doesn't scroll a `<div>`, so `within:` lists never moved.
    async fn scroll_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        let page = self.page_handle(device_id).await?;
        page.wheel(
            f64::from(from.0),
            f64::from(from.1),
            f64::from(from.0 - to.0),
            f64::from(from.1 - to.1),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
        Ok(())
    }

    async fn press_key(&self, device_id: &str, key: Key) -> Result<()> {
        let key_str = match key {
            // `back` is the browser's back button, as it is Android's
            // (D-9). It sent Backspace, so `pressBack` deleted a character
            // and never left the page (VEL-270).
            Key::Back => return self.go_back(device_id).await,
            Key::Enter => "Enter",
            Key::Home => "Home",
            Key::Backspace => "Backspace",
            Key::Tab => "Tab",
            Key::Escape => "Escape",
            Key::Delete => "Delete",
            Key::ArrowUp => "ArrowUp",
            Key::ArrowDown => "ArrowDown",
            Key::ArrowLeft => "ArrowLeft",
            Key::ArrowRight => "ArrowRight",
            // A browser has no volume buttons: it was skipped silently.
            Key::VolumeUp | Key::VolumeDown => {
                return Err(VelocityError::Config(format!(
                    "pressKey: {} has no browser equivalent",
                    key.name()
                )))
            }
            _ => {
                return Err(VelocityError::Config(format!(
                    "pressKey: {} is not supported on the web",
                    key.name()
                )))
            }
        };
        let page = self.page_handle(device_id).await?;
        page.press_key(key_str).await
    }

    // ──────────────────────────────────────────────
    // Screen
    // ──────────────────────────────────────────────

    async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        let page = self.page_handle(device_id).await?;
        page.screenshot_png().await
    }

    /// `assertScreenshot`'s capture: animations finished or paused and the
    /// caret hidden for the capture, then resumed, so two captures of the
    /// same screen don't differ by when they were taken (VT-8). The plain
    /// `screenshot` step keeps the page as it is.
    async fn screenshot_for_compare(&self, device_id: &str) -> Result<Vec<u8>> {
        let page = self.page_handle(device_id).await?;
        let frozen = page.call("freeze", vec![]).await.is_ok();
        let png = page.screenshot_png().await;
        if frozen {
            let _ = page.call("unfreeze", vec![]).await;
        }
        png
    }

    /// The element scrolled into view (if it isn't), with its fresh bounds:
    /// `scrollUntilVisible within:` a container below the fold aimed at
    /// its pre-scroll box, off screen, and never scrolled it (VEL-069).
    async fn reveal(&self, device_id: &str, element: &Element) -> Result<Element> {
        let page = self.page_handle(device_id).await?;
        let v = page.call_el("reveal", &element.platform_id, vec![]).await?;
        Ok(json_to_element(&v).unwrap_or_else(|| element.clone()))
    }

    async fn screen_size(&self, _device_id: &str) -> Result<(i32, i32)> {
        Ok((self.config.width as i32, self.config.height as i32))
    }

    // ──────────────────────────────────────────────
    // Element state
    // ──────────────────────────────────────────────

    /// The element's text now, in full. Falls back to the captured text
    /// when the element has left the page.
    async fn get_element_text(&self, device_id: &str, element: &Element) -> Result<String> {
        let page = self.page_handle(device_id).await?;
        match Self::live_state(&*page, element).await? {
            Some(v) => Ok(v["text"].as_str().unwrap_or_default().to_string()),
            None => Ok(element.text.clone().unwrap_or_default()),
        }
    }

    /// `assertText` on the page: `expected` normalised as the page text is
    /// (whitespace runs incl. NBSP / U+202F, zero-width characters, NFC),
    /// and the element's text as rendered or as written, so CSS
    /// `text-transform` doesn't fail it (B-1, M-1). A detached element is
    /// judged by its captured text.
    async fn element_text_matches(
        &self,
        device_id: &str,
        element: &Element,
        expected: &str,
    ) -> Result<(bool, String)> {
        let page = self.page_handle(device_id).await?;
        let v = page
            .call_el("textIs", &element.platform_id, vec![expected.into()])
            .await?;
        if v["gone"].as_bool() == Some(true) {
            let captured = element.text.clone().unwrap_or_default();
            let want = v["norm"].as_str().unwrap_or(expected);
            let hit = captured == expected || captured == want;
            return Ok((hit, captured));
        }
        Ok((
            v["match"].as_bool().unwrap_or(false),
            v["text"].as_str().unwrap_or_default().to_string(),
        ))
    }

    /// Whether the element is still in the page and visible now, not when
    /// it was captured: the selector cache otherwise kept serving an element
    /// that had gone (VEL-057).
    ///
    /// Visible is the devices' rule (D-1): rendered (`opacity: 0` is
    /// hidden), a box, and on screen: in the viewport and not clipped away
    /// by an overflow ancestor, by any pixel or `visibility.minArea` of its
    /// box. `web.visibility: page` restores the old rule (anywhere on the
    /// page). Actions still scroll their target into view first.
    async fn is_element_visible(&self, device_id: &str, element: &Element) -> Result<bool> {
        let page = self.page_handle(device_id).await?;
        let opts = serde_json::json!({
            "page": velocity_common::env::var("VELOCITY_WEB_VISIBILITY")
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("page")),
            "minArea": velocity_common::selector_match::min_visible_area(),
        });
        let v = page
            .call_el("state", &element.platform_id, vec![opts])
            .await?;
        Ok(!v.is_null() && v["visible"].as_bool().unwrap_or(false))
    }
}
/// How a JavaScript dialog of `kind` (`alert`, `confirm`, `prompt`,
/// `beforeunload`, CDP's names) is answered: accept or dismiss, and the
/// text an accepted prompt gets. Alerts and beforeunload are accepted
/// (there is nothing to choose, and navigation should proceed). Confirm
/// and prompt follow `web.dialogs` (`VELOCITY_WEB_DIALOGS`): `dismiss` by
/// default, so a test never triggers a destructive action it did not ask
/// for; `accept` lets a suite test a confirm-guarded delete (review 10
/// #9). `web.promptText` (`VELOCITY_WEB_PROMPT_TEXT`) is what an accepted
/// prompt returns.
fn dialog_answer(kind: &str) -> (bool, Option<String>) {
    match kind {
        "confirm" | "prompt" => {
            let accept = matches!(
                velocity_common::env::var("VELOCITY_WEB_DIALOGS")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "accept" | "1" | "true"
            );
            let text = (accept && kind == "prompt")
                .then(|| velocity_common::env::var("VELOCITY_WEB_PROMPT_TEXT").unwrap_or_default());
            (accept, text)
        }
        _ => (true, None),
    }
}

/// Answers every JavaScript dialog as soon as it opens, as
/// [`dialog_answer`] says. An open alert, confirm or prompt blocks the
/// page's JS thread, so every later CDP `Runtime.evaluate` (each find, tap
/// and assert) would hang until the step timed out. Chrome reports a
/// dialog opened in a cross-origin frame on the page's own session too,
/// so this answers those as well (pinned by `tests/web_dialogs.rs`; the
/// frames' sessions never see it, review 10 #9).
async fn spawn_dialog_handler(page: &Arc<Page>) {
    use chromiumoxide::cdp::browser_protocol::page::{
        EventJavascriptDialogOpening, HandleJavaScriptDialogParams,
    };
    let mut events = match page
        .page
        .event_listener::<EventJavascriptDialogOpening>()
        .await
    {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!("velocity-web: cannot watch JS dialogs: {e}");
            return;
        }
    };
    let page = page.clone();
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            let kind = event.r#type.as_ref().to_string();
            let (accept, text) = dialog_answer(&kind);
            tracing::info!(
                "velocity-web: {} JS {kind} \"{}\"",
                if accept { "accepted" } else { "dismissed" },
                event.message
            );
            let mut params = HandleJavaScriptDialogParams::new(accept);
            params.prompt_text = text;
            if let Err(e) = page.exec(params).await {
                debug!("velocity-web: answering the JS dialog failed: {e}");
            }
        }
    });
}

/// The tabs a device's page opened and closed (review 10 #5): a popup
/// (`window.open`, `target="_blank"`, a sign-in or payment window) was a
/// new target nobody drove, so the step that opened it "did nothing" and
/// the next one looked for its content on the old page.
#[derive(Clone, Default)]
struct TabWatch(Arc<std::sync::Mutex<Tabs>>);

#[derive(Default)]
struct Tabs {
    /// The device's pages: the first and every tab opened from one of them.
    ours: std::collections::HashSet<String>,
    /// Tabs opened and not driven yet, oldest first.
    opened: Vec<String>,
    /// Tabs of ours that closed.
    gone: std::collections::HashSet<String>,
}

/// What [`TabWatch::next_change`] found.
enum TabChange {
    /// A tab to drive.
    Opened(String),
    /// The driven tab closed.
    Closed,
}

impl TabWatch {
    fn lock(&self) -> std::sync::MutexGuard<'_, Tabs> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watch `page`'s browser for tabs `page` (or one of its tabs) opens
    /// and for ours that close.
    async fn watch(&self, chrome: &Chrome, page: &Page) {
        use chromiumoxide::cdp::browser_protocol::target::{
            EventTargetCreated, EventTargetDestroyed,
        };
        self.lock()
            .ours
            .insert(page.page.target_id().inner().clone());
        let (created, destroyed) = {
            let browser = chrome.browser.lock().await;
            (
                browser.event_listener::<EventTargetCreated>().await,
                browser.event_listener::<EventTargetDestroyed>().await,
            )
        };
        let (Ok(mut created), Ok(mut destroyed)) = (created, destroyed) else {
            debug!("velocity-web: new tabs are not followed (no target events)");
            return;
        };
        // Weak, so a device's watch ends with its session even while the
        // shared Chrome lives on.
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    ev = created.next() => {
                        let Some(ev) = ev else { break };
                        let Some(tabs) = weak.upgrade() else { break };
                        let info = &ev.target_info;
                        let mut t = tabs.lock().unwrap_or_else(|e| e.into_inner());
                        let mine = info.opener_id.as_ref().is_some_and(|o| t.ours.contains(o.inner()));
                        if info.r#type == "page" && mine {
                            let id = info.target_id.inner().clone();
                            t.ours.insert(id.clone());
                            t.opened.push(id);
                        }
                    }
                    ev = destroyed.next() => {
                        let Some(ev) = ev else { break };
                        let Some(tabs) = weak.upgrade() else { break };
                        let mut t = tabs.lock().unwrap_or_else(|e| e.into_inner());
                        let id = ev.target_id.inner();
                        if t.ours.contains(id) {
                            t.opened.retain(|o| o != id);
                            t.gone.insert(id.clone());
                        }
                    }
                }
            }
        });
    }

    /// What to switch to before driving `page`: the oldest tab opened and
    /// not driven yet, or (`page` closed) its opener.
    fn next_change(&self, page: &Page, openers: &[Arc<Page>]) -> Option<TabChange> {
        let mut t = self.lock();
        if !t.opened.is_empty() {
            return Some(TabChange::Opened(t.opened.remove(0)));
        }
        let gone = t.gone.contains(page.page.target_id().inner());
        (gone && !openers.is_empty()).then_some(TabChange::Closed)
    }

    fn is_gone(&self, page: &Page) -> bool {
        self.lock().gone.contains(page.page.target_id().inner())
    }

    fn forget(&self, id: &str) {
        self.lock().ours.remove(id);
    }
}

/// Marks `page` crashed when its renderer goes ("Aw, Snap!"), so its next
/// call fails at once as a lost connection, which the executor recovers
/// from with a new browser, instead of each one waiting out
/// `CDP_TIMEOUT` and failing as an internal error (review 10 #6).
async fn spawn_crash_watch(page: &Arc<Page>) {
    use chromiumoxide::cdp::browser_protocol::inspector::{EnableParams, EventTargetCrashed};
    let mut events = match page.page.event_listener::<EventTargetCrashed>().await {
        Ok(events) => events,
        Err(e) => {
            debug!("velocity-web: cannot watch for renderer crashes: {e}");
            return;
        }
    };
    if let Err(e) = page.exec(EnableParams::default()).await {
        debug!("velocity-web: Inspector.enable: {e}");
    }
    let page = Arc::downgrade(page);
    tokio::spawn(async move {
        if events.next().await.is_some() {
            if let Some(page) = page.upgrade() {
                tracing::warn!("velocity-web: the page's renderer crashed");
                page.mark_crashed();
            }
        }
    });
}

/// A configured Chrome argument (`--disable-web-security`,
/// `--js-flags=--expose-gc`) as chromiumoxide's key and value. chromiumoxide
/// puts `--` in front of the key itself, so the argument as written reached
/// Chrome as `----disable-web-security` and was ignored.
fn chrome_arg(arg: &str) -> (&str, Option<&str>) {
    let bare = arg.trim().trim_start_matches('-');
    match bare.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (bare, None),
    }
}

/// Runs a script that starts a navigation. The page may unload before it
/// answers, so other errors are fine; a lost browser connection is not, and
/// was swallowed with them, so the step passed on a dead browser.
async fn nav_eval(page: &Page, js: &str) -> Result<()> {
    match page.eval(js).await {
        Err(e @ VelocityError::BrowserConnectionLost { .. }) => Err(e),
        _ => Ok(()),
    }
}

/// The finger's stroke for a `swipe` in `finger`'s direction on a touch
/// profile: through the viewport's centre, 40% of the viewport long.
fn touch_stroke(width: u32, height: u32, finger: Direction) -> ((f64, f64), (f64, f64)) {
    let (w, h) = (f64::from(width), f64::from(height));
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (dx, dy) = (w * 0.2, h * 0.2);
    match finger {
        Direction::Up => ((cx, cy + dy), (cx, cy - dy)),
        Direction::Down => ((cx, cy - dy), (cx, cy + dy)),
        Direction::Left => ((cx + dx, cy), (cx - dx, cy)),
        Direction::Right => ((cx - dx, cy), (cx + dx, cy)),
    }
}

/// Anywhere on the page, but visible and not empty: without a screen the
/// shared matcher took hidden and zero-size elements too (VEL-127).
fn page_rect() -> velocity_common::Rect {
    velocity_common::Rect {
        x: -(1 << 28),
        y: -(1 << 28),
        width: 1 << 29,
        height: 1 << 29,
    }
}

/// The element `id` in `tree`.
fn find_by_id<'a>(tree: &'a Element, id: &str) -> Option<&'a Element> {
    if tree.platform_id == id {
        return Some(tree);
    }
    tree.children.iter().find_map(|c| find_by_id(c, id))
}

/// Matches `relation` on `tree` between elements already resolved in the
/// page (`targets`, `anchors`), with the shared matcher that every driver
/// uses, so the ranking (nearest first) and nesting rules are the same.
/// Each node of a copy of the tree is marked by set: a target's label and
/// an anchor's type become markers, so `accessibilityId` and `className`
/// on the copy select exactly those sets.
fn relative_ids(
    tree: &Element,
    targets: &[String],
    relation: velocity_common::Relation,
    anchors: &[String],
) -> Vec<String> {
    const TARGET: &str = "\u{1}velocity-target";
    const ANCHOR: &str = "\u{1}velocity-anchor";
    fn mark(e: &Element, t: &[String], a: &[String]) -> Element {
        Element {
            hint: None,
            platform_id: e.platform_id.clone(),
            label: t.contains(&e.platform_id).then(|| TARGET.to_string()),
            text: None,
            element_type: if a.contains(&e.platform_id) {
                ANCHOR.to_string()
            } else {
                String::new()
            },
            bounds: e.bounds,
            enabled: e.enabled,
            visible: e.visible,
            children: e.children.iter().map(|c| mark(c, t, a)).collect(),
            ..Default::default()
        }
    }
    let marked = mark(tree, targets, anchors);
    let rel = Selector::Relative {
        target: Box::new(Selector::AccessibilityId(TARGET.into())),
        relation,
        anchor: Box::new(Selector::ClassName(ANCHOR.into())),
    };
    find_all_ref(&marked, &rel, Some(&page_rect()))
        .into_iter()
        .map(|e| e.platform_id.clone())
        .collect()
}

/// The step's actionability rule, as `act` in `injected.js` takes it.
fn act_mode_arg() -> &'static str {
    use velocity_common::actionability::{mode, ActMode};
    match mode() {
        ActMode::Strict => "strict",
        ActMode::Lenient => "lenient",
        ActMode::Force => "force",
    }
}

/// The error for an element the page refuses to act on (disabled,
/// covered, gone): an assertion-class failure, not a missing element.
fn not_actionable(element: &Element, reason: &str) -> VelocityError {
    VelocityError::NotActionable {
        selector: format!("<{}>", element.element_type),
        reason: reason.to_string(),
    }
}

/// The kinds of pointer action [`WebDriver::pointer_action`] performs.
#[derive(Clone, Copy)]
enum Pointer {
    Click,
    Double,
    Long(u64),
}

/// How long to wait for the hit-target verdict after a click.
const DISARM_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a cross-origin frame's library waits for a click to arrive
/// before it says it never did (see [`WebDriver::pointer_action`]).
const FRAME_CLICK_WAIT_MS: u64 = 500;

/// Text kept per node in the element tree (`get_hierarchy`): the tree
/// repeats each element's text in all its ancestors.
const WALK_TEXT_LIMIT: usize = 200;

/// Whether `selector` has a `textMatches` anywhere in it.
fn has_text_matches(selector: &Selector) -> bool {
    match selector {
        Selector::TextMatches(_) => true,
        Selector::Index { selector, .. } => has_text_matches(selector),
        Selector::Compound(parts) => parts.iter().any(has_text_matches),
        Selector::Relative { target, anchor, .. } => {
            has_text_matches(target) || has_text_matches(anchor)
        }
        _ => false,
    }
}
/// How long a tap waits for its target to become actionable.
const ACTIONABLE_WAIT: Duration = Duration::from_millis(1000);

/// One `scroll` step of the wheel, in CSS px.
const WHEEL_STEP: f64 = 400.0;
/// A frame or two for the scroll to land before the next read.
const WHEEL_SETTLE_MS: u64 = 50;

/// Upper bound on any single navigation wait, below the default step timeout.
const NAV_TIMEOUT: Duration = Duration::from_secs(20);
/// `health_check`'s round-trip limit: a healthy page answers in ms.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// Each of `close` and the wait for Chrome to exit, before `kill`.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// The most `cleanup` takes, all browsers together: under the runner's
/// 20 s stop grace (D-20).
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(12);
/// `restart_session`'s ping of each browser, when there are several.
const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// Failed pings in a row before a Chrome is called hung and replaced
/// (review-fi2 F2): in context mode it holds every device's browser.
const HUNG_PINGS: u32 = 2;

/// `scheme://host[:port]` of an http(s) URL; `None` for anything else.
fn web_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

/// Profile dirs made by this process (unique within it).
static PROFILES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A new Chrome profile dir under the temp dir, created here, exclusively
/// and private (0700): a directory someone else made first with a guessable
/// name (a shared `/tmp`) is never used, so they can't read the profile's
/// cookies and storage.
fn new_profile_dir() -> std::io::Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    loop {
        let n = PROFILES.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "velocity-chrome-{}-{n}-{nanos:08x}",
            std::process::id()
        ));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => continue,
            Err(e) => return Err(e),
        }
    }
}

/// True when `target` differs from `current` only in its fragment, so the
/// browser changes it in place without loading a new document.
fn same_document(current: &str, target: &str) -> bool {
    let base = |u: &str| u.split('#').next().unwrap_or(u).to_string();
    target.contains('#') && !current.is_empty() && base(current) == base(target)
}

/// Remove Chrome's profile dir. Chrome's helpers can still write into it
/// while they exit, recreating it after a single `remove_dir_all` (a
/// `profile_dir` flake under load): retry until it stays gone, at most
/// about 1 s.
async fn remove_profile_dir(dir: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_000);
    loop {
        let _ = std::fs::remove_dir_all(dir);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if !dir.exists() || std::time::Instant::now() >= deadline {
            return;
        }
    }
}

/// The paths an `inputText` on a file input names: one per line, blank
/// lines skipped, relative ones from the working directory. A path that
/// isn't a file is a config error, before anything is sent.
fn file_paths(text: &str) -> Result<Vec<String>> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let p = std::path::Path::new(l);
            let p = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            if p.is_file() {
                Ok(p.to_string_lossy().into_owned())
            } else {
                Err(VelocityError::Config(format!(
                    "inputText on a file input: no file at {}",
                    p.display()
                )))
            }
        })
        .collect::<Result<_>>()?;
    if paths.is_empty() {
        return Err(VelocityError::Config(
            "inputText on a file input needs a file path (one per line)".into(),
        ));
    }
    Ok(paths)
}

/// `web.insecureCerts` (`VELOCITY_WEB_INSECURE_CERTS`): accept any TLS
/// certificate. Off by default.
fn insecure_certs() -> bool {
    insecure_certs_from(
        velocity_common::env::var("VELOCITY_WEB_INSECURE_CERTS")
            .ok()
            .as_deref(),
    )
}

fn insecure_certs_from(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim(), "1" | "true" | "on"))
}

/// `web.downloads` (`VELOCITY_WEB_DOWNLOADS`): a folder downloads are
/// saved in, or `None` (unset, `deny`, `off`) to refuse them. A download
/// in headless Chrome used to go into the temporary profile, deleted on
/// close, or nowhere (review 10 #11).
fn downloads_dir() -> Option<PathBuf> {
    let v = velocity_common::env::var("VELOCITY_WEB_DOWNLOADS").ok()?;
    let v = v.trim();
    if v.is_empty() || matches!(v, "deny" | "off" | "0" | "false") {
        return None;
    }
    let dir = std::env::current_dir().unwrap_or_default().join(v);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "velocity-web: downloads refused: can't make {}: {e}",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Sets `context`'s download behaviour ([`downloads_dir`]) and says, for
/// each download `page` starts, what became of it.
async fn watch_downloads(
    chrome: &Chrome,
    context: Option<chromiumoxide::cdp::browser_protocol::browser::BrowserContextId>,
    page: &Arc<Page>,
) {
    use chromiumoxide::cdp::browser_protocol::browser::{
        EventDownloadWillBegin, SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
    };
    let dir = downloads_dir();
    let mut params = SetDownloadBehaviorParams::new(if dir.is_some() {
        SetDownloadBehaviorBehavior::Allow
    } else {
        SetDownloadBehaviorBehavior::Deny
    });
    params.browser_context_id = context.clone();
    params.download_path = dir.as_ref().map(|d| d.to_string_lossy().into_owned());
    params.events_enabled = Some(true);
    let events = {
        let browser = chrome.browser.lock().await;
        match tokio::time::timeout(cdp::CDP_TIMEOUT, browser.execute(params)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => debug!("velocity-web: download behaviour not set: {e}"),
            Err(_) => debug!("velocity-web: download behaviour not set: no answer"),
        }
        browser.event_listener::<EventDownloadWillBegin>().await
    };
    let Ok(mut events) = events else {
        return;
    };
    let frame = page.page.target_id().inner().clone();
    let shared = context.is_some();
    let weak = Arc::downgrade(page);
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if weak.upgrade().is_none() {
                break;
            }
            // In a shared Chrome, only this page's own downloads.
            if shared && ev.frame_id.inner() != &frame {
                continue;
            }
            match &dir {
                Some(d) => tracing::warn!(
                    "velocity-web: the page downloads {} into {}",
                    ev.suggested_filename,
                    d.display()
                ),
                None => tracing::warn!(
                    "velocity-web: the page started a download of {} ({}), which was refused; \
                     set web.downloads (VELOCITY_WEB_DOWNLOADS) to a folder to keep downloads",
                    ev.suggested_filename,
                    ev.url
                ),
            }
        }
    });
}

/// `web.frames` (`VELOCITY_WEB_FRAMES`): `off` leaves cross-origin
/// iframes alone (W4), as before: they are opaque boxes.
fn frames_enabled() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_WEB_FRAMES")
            .as_deref()
            .map(str::trim),
        Ok("0" | "off" | "false")
    )
}

/// D-5: `inputText` whose click left the focus on something that takes no
/// text (a label, a div, nothing at all).
/// `web.focusCheck` (`VELOCITY_WEB_FOCUS_CHECK`): `off` lets `inputText`
/// type wherever the focus is, as before D-5.
fn focus_check_enabled() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_WEB_FOCUS_CHECK")
            .as_deref()
            .map(str::trim),
        Ok("0" | "off" | "false")
    )
}

/// The focused cross-origin frame, as a key that names the same frame on
/// two reads (its index among the frames of each document on the way,
/// and its `src`), or `None` when the focus is readable (or nowhere).
/// Read in the page's main world, the way `injected.js`'s `focused()`
/// walks shadow roots and same-origin frames.
pub(crate) async fn opaque_focus(page: &Page) -> Option<String> {
    const JS: &str = r#"(() => {
  let e = document.activeElement; const path = [];
  const isFrame = (n) => n && (n.tagName === 'IFRAME' || n.tagName === 'FRAME');
  for (let i = 0; i < 64; i++) {
    if (e && e.shadowRoot && e.shadowRoot.activeElement) { e = e.shadowRoot.activeElement; continue; }
    if (!isFrame(e)) return null;
    const frames = Array.from(e.ownerDocument.querySelectorAll('iframe,frame'));
    path.push(frames.indexOf(e));
    let d = null;
    try { d = e.contentDocument; } catch (x) { d = null; }
    if (d && d.activeElement) { e = d.activeElement; continue; }
    return JSON.stringify([path, e.src || '']);
  }
  return null;
})()"#;
    page.eval(JS)
        .await
        .ok()?
        .into_value::<Option<String>>()
        .ok()?
}

fn not_editable(element: &Element, focus_tag: Option<&str>) -> VelocityError {
    VelocityError::AssertionFailed {
        expected: format!(
            "<{}> focused and editable for inputText",
            element.element_type
        ),
        actual: format!(
            "the focus is on {}, which takes no text; select the <input>, <textarea> \
             or contenteditable element itself (or web.focusCheck: off)",
            focus_tag
                .map(|t| format!("<{t}>"))
                .unwrap_or_else(|| "nothing".into())
        ),
        selector: element
            .label
            .clone()
            .unwrap_or_else(|| element.platform_id.clone()),
        screenshot: None,
    }
}

/// W5: the driver's logic against a fake [`BrowserProtocol`], no browser:
/// the seam is real, and what goes through it is checked on its own.
#[cfg(test)]
mod fake_protocol {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::{json, Value};
    use velocity_common::{Element, Result, Selector, VelocityError};

    use super::{Pointer, WebDriver};
    use crate::protocol::BrowserProtocol;

    /// Answers each library op from a script, in order (the last answer
    /// repeats), and records what the driver did.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<HashMap<String, VecDeque<Value>>>,
        frames: Vec<Value>,
        /// What `call_frames` reports as skipped (frames that didn't answer).
        skipped: Vec<String>,
        calls: Mutex<Vec<(String, Vec<Value>)>>,
        clicks: Mutex<Vec<(f64, f64)>>,
        typed: Mutex<Vec<String>>,
        /// What `opaque_focus` reports (the same before and after).
        opaque: Option<String>,
    }

    impl Fake {
        fn answer(self, op: &str, values: Vec<Value>) -> Self {
            self.answers
                .lock()
                .unwrap()
                .insert(op.to_string(), values.into());
            self
        }

        fn next(&self, op: &str, args: Vec<Value>) -> Value {
            self.calls.lock().unwrap().push((op.to_string(), args));
            let mut a = self.answers.lock().unwrap();
            match a.get_mut(op) {
                Some(q) if q.len() > 1 => q.pop_front().unwrap(),
                Some(q) => q.front().cloned().unwrap_or(Value::Null),
                None => Value::Null,
            }
        }

        fn ops(&self, op: &str) -> Vec<Vec<Value>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(o, _)| o == op)
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl BrowserProtocol for Fake {
        fn protocol(&self) -> &'static str {
            "fake"
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn touch(&self) -> bool {
            false
        }
        async fn call_for(&self, _: Duration, op: &str, args: Vec<Value>) -> Result<Value> {
            Ok(self.next(op, args))
        }
        async fn call_el(&self, op: &str, id: &str, rest: Vec<Value>) -> Result<Value> {
            let mut args = vec![Value::from(id)];
            args.extend(rest);
            Ok(self.next(op, args))
        }
        async fn call_near(
            &self,
            _: &str,
            _: Duration,
            op: &str,
            args: Vec<Value>,
        ) -> Result<Value> {
            Ok(self.next(op, args))
        }
        async fn call_frames(&self, _: &str, _: Vec<Value>) -> crate::cdp::FrameReplies {
            crate::cdp::FrameReplies {
                found: self.frames.clone(),
                skipped: self.skipped.clone(),
            }
        }
        async fn click(&self, x: f64, y: f64) -> Result<()> {
            self.clicks.lock().unwrap().push((x, y));
            Ok(())
        }
        async fn double_click(&self, x: f64, y: f64) -> Result<()> {
            self.click(x, y).await
        }
        async fn long_press(&self, x: f64, y: f64, _: u64) -> Result<()> {
            self.click(x, y).await
        }
        async fn drag(&self, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn touch_drag(&self, _: (f64, f64), _: (f64, f64)) -> Result<()> {
            Ok(())
        }
        async fn wheel(&self, _: f64, _: f64, _: f64, _: f64) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn insert_text(&self, text: &str) -> Result<()> {
            self.typed.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn select_all_and_delete(&self) -> Result<()> {
            Ok(())
        }
        async fn move_caret_to_end(&self) -> Result<()> {
            Ok(())
        }
        async fn screenshot_png(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn opaque_focus(&self) -> Option<String> {
            self.opaque.clone()
        }
    }

    fn button() -> Element {
        Element {
            platform_id: "7.abc".into(),
            element_type: "button".into(),
            ..Default::default()
        }
    }

    /// The gate scrolls a covered element to the next alignment and
    /// clicks at the point it reports; a click the page blocked (a toast
    /// landed on the target) is tried again at the fresh point.
    #[tokio::test]
    async fn the_gate_realigns_and_a_blocked_click_is_retried() {
        let fake = Fake::default()
            .answer(
                "act",
                vec![
                    json!({"ok": false, "reason": "obscured by <header>"}),
                    json!({"ok": true, "x": 10, "y": 20}),
                    json!({"ok": true, "x": 30, "y": 40}),
                ],
            )
            .answer("arm", vec![json!(true)])
            .answer(
                "disarm",
                vec![json!({"blocked": "div#toast"}), json!({"blocked": null})],
            );
        WebDriver::pointer_action(&fake, &button(), Pointer::Click)
            .await
            .unwrap();
        assert_eq!(*fake.clicks.lock().unwrap(), [(10.0, 20.0), (30.0, 40.0)]);
        let aligns: Vec<Value> = fake.ops("act").iter().map(|a| a[1].clone()).collect();
        assert_eq!(aligns, [json!(0), json!(1), json!(0)]);
        assert_eq!(fake.ops("act")[0][0], "7.abc");
    }

    /// A match only in a cross-origin frame is the one found (W4).
    #[tokio::test]
    async fn a_frame_match_is_found_when_the_page_has_none() {
        let fake = Fake {
            frames: vec![json!({"velId": "@1/9.x", "tag": "button", "x": 70, "y": 120, "w": 10, "h": 10, "visible": true})],
            ..Default::default()
        }
        .answer("find", vec![Value::Null]);
        let sel = Selector::Css("#pay".into());
        let (el, _) = WebDriver::resolve_element(&fake, &sel).await.unwrap();
        assert_eq!(el.platform_id, "@1/9.x");
        assert_eq!((el.bounds.x, el.bounds.y), (70, 120));
        let none = Fake::default().answer("find", vec![Value::Null]);
        assert!(matches!(
            WebDriver::resolve_element(&none, &sel).await,
            Err(VelocityError::ElementNotFound { .. })
        ));
    }

    /// review-fi2 F1: nothing on the page, and a frame didn't answer: the
    /// lookup is unknown (an error naming the frame), not "not found",
    /// which `assertNotVisible` took for a pass. A hidden page match with
    /// a skipped frame is unknown too; a shown one is found as before.
    #[tokio::test]
    async fn a_skipped_frame_makes_a_miss_unknown_not_not_found() {
        let skipped = vec!["frame 2 (http://localhost:9/pay.html)".to_string()];
        let sel = Selector::Css("#err".into());
        for page in [
            Value::Null,
            json!({"velId": "3.a", "tag": "div", "visible": false}),
        ] {
            let fake = Fake {
                skipped: skipped.clone(),
                ..Default::default()
            }
            .answer("find", vec![page]);
            let err = WebDriver::resolve_element(&fake, &sel)
                .await
                .expect_err("unknown");
            assert!(
                !matches!(err, VelocityError::ElementNotFound { .. }),
                "{err}"
            );
            assert!(
                err.to_string()
                    .contains("frame 2 (http://localhost:9/pay.html)"),
                "{err}"
            );
            assert!(err.kind().is_permanent(), "never trips the breaker");
        }
        let shown = Fake {
            skipped,
            ..Default::default()
        }
        .answer("find", vec![json!({"velId": "3.a", "tag": "div", "x": 1, "y": 1, "w": 5, "h": 5, "visible": true})]);
        assert!(WebDriver::resolve_element(&shown, &sel).await.is_ok());
    }

    fn in_frame(kind: &str) -> Element {
        Element {
            platform_id: "@1/9.x".into(),
            element_type: kind.into(),
            ..Default::default()
        }
    }

    /// review-i2 M1: a click on an element in a cross-origin frame that
    /// the frame never saw (something on the top page took it) is blocked
    /// and retried, then fails; one the frame saw passes.
    #[tokio::test]
    async fn a_frame_click_the_frame_never_saw_fails() {
        let script = |seen: bool| {
            Fake::default()
                .answer("act", vec![json!({"ok": true, "x": 10, "y": 20})])
                .answer("arm", vec![json!(true)])
                .answer("disarm", vec![json!({"blocked": null, "seen": seen})])
        };
        let missed = script(false);
        let e = WebDriver::pointer_action(&missed, &in_frame("button"), Pointer::Click)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("never reached the frame"), "{e}");
        assert_eq!(missed.clicks.lock().unwrap().len(), 3);
        // The frame's library waits a moment for the click to arrive.
        assert_eq!(missed.ops("disarm")[0], [json!(super::FRAME_CLICK_WAIT_MS)]);
        let seen = script(true);
        WebDriver::pointer_action(&seen, &in_frame("button"), Pointer::Click)
            .await
            .unwrap();
        assert_eq!(seen.clicks.lock().unwrap().len(), 1);
        // A top-document element's verdict has no `seen` to check.
        let page = Fake::default()
            .answer("act", vec![json!({"ok": true, "x": 10, "y": 20})])
            .answer("arm", vec![json!(true)])
            .answer("disarm", vec![json!({"blocked": null, "seen": false})]);
        WebDriver::pointer_action(&page, &button(), Pointer::Click)
            .await
            .unwrap();
        assert_eq!(page.clicks.lock().unwrap().len(), 1);
        // review 10 #2: one in a same-origin frame (`nested`) has: a
        // banner on the page over its <iframe> took the click.
        let nested = Fake::default()
            .answer("act", vec![json!({"ok": true, "x": 10, "y": 20})])
            .answer("arm", vec![json!(true)])
            .answer(
                "disarm",
                vec![json!({"blocked": null, "seen": false, "nested": true})],
            );
        let e = WebDriver::pointer_action(&nested, &button(), Pointer::Click)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("never reached the frame"), "{e}");
    }

    /// The opaque-frame focus check of `inputText` (review MINOR 10 as
    /// integrated, review-i2 m1, review-i1-i3 M2).
    #[tokio::test]
    async fn the_opaque_frame_focus_check_branches() {
        let script = |opaque: Option<&str>, within: bool| {
            Fake {
                opaque: opaque.map(str::to_string),
                ..Default::default()
            }
            .answer("isSelect", vec![json!(false)])
            .answer("act", vec![json!({"ok": true, "x": 5, "y": 5})])
            .answer("arm", vec![json!(false)])
            .answer("focusWithin", vec![json!(within)])
            .answer(
                "caretToEnd",
                vec![json!({"editable": null, "tag": "iframe", "mode": "type"})],
            )
        };
        // A target in an attached cross-origin frame: no page-level check
        // (its focus is checked in its frame).
        let frame_target = script(Some("card"), false);
        WebDriver::type_into(&frame_target, "web", &in_frame("input"), "42")
            .await
            .unwrap();
        assert!(frame_target.ops("focusWithin").is_empty());
        assert_eq!(*frame_target.typed.lock().unwrap(), ["42"]);
        // A page target while the focus stays in the same opaque frame,
        // which isn't the target's: refused.
        let label = Element {
            element_type: "div".into(),
            ..button()
        };
        let refused = script(Some("card"), false);
        let e = WebDriver::type_into(&refused, "web", &label, "Ada")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("<iframe>"), "{e}");
        assert!(refused.typed.lock().unwrap().is_empty());
        // ... but the target's own (it holds the <iframe>, or the click
        // landed in it): typed (review-i1-i3 M2).
        let container = script(Some("card"), true);
        WebDriver::type_into(&container, "web", &label, "4242")
            .await
            .unwrap();
        assert_eq!(
            container.ops("focusWithin")[0],
            [json!("7.abc"), json!(5.0), json!(5.0)]
        );
        assert_eq!(*container.typed.lock().unwrap(), ["4242"]);
        // No opaque focus before the click: no check.
        let none = script(None, false);
        WebDriver::type_into(&none, "web", &label, "x")
            .await
            .unwrap();
        assert!(none.ops("focusWithin").is_empty());
    }

    /// `inputText`: typed in one go when the focus takes text, refused
    /// (D-5) when it doesn't.
    #[tokio::test]
    async fn typing_checks_the_focus() {
        let script = |editable: bool| {
            Fake::default()
                .answer("isSelect", vec![json!(false)])
                .answer("act", vec![json!({"ok": true, "x": 5, "y": 5})])
                .answer("arm", vec![json!(false)])
                .answer(
                    "caretToEnd",
                    vec![json!({"editable": editable, "tag": "div", "mode": "type"})],
                )
        };
        let ok = script(true);
        WebDriver::type_into(&ok, "web", &button(), "hello")
            .await
            .unwrap();
        assert_eq!(*ok.typed.lock().unwrap(), ["hello"]);
        assert_eq!(ok.protocol(), "fake");
        let refused = script(false);
        let e = WebDriver::type_into(&refused, "web", &button(), "hello")
            .await
            .unwrap_err();
        assert!(matches!(e, VelocityError::AssertionFailed { .. }), "{e}");
        assert!(refused.typed.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn certificate_errors_are_respected_unless_the_suite_opts_out() {
        // Audit 2026-11 WEB-1.
        assert!(!super::insecure_certs_from(None));
        assert!(!super::insecure_certs_from(Some("0")));
        assert!(!super::insecure_certs_from(Some("")));
        assert!(super::insecure_certs_from(Some("1")));
        assert!(super::insecure_certs_from(Some("true")));
    }
    use super::{chrome_arg, remove_profile_dir, same_document, touch_stroke};
    use velocity_common::Direction;

    /// W2: a touch swipe is the finger moving the named way, through the
    /// middle, 40% of the viewport long.
    #[test]
    fn touch_strokes_follow_the_finger() {
        assert_eq!(
            touch_stroke(400, 800, Direction::Up),
            ((200.0, 560.0), (200.0, 240.0))
        );
        assert_eq!(
            touch_stroke(400, 800, Direction::Left),
            ((280.0, 400.0), (120.0, 400.0))
        );
    }

    /// Flake fix: Chrome's exiting helpers write into the profile dir after
    /// the first `remove_dir_all`, and the dir survived `cleanup()`.
    #[tokio::test]
    async fn a_profile_dir_written_to_while_removed_is_still_gone() {
        let dir = std::env::temp_dir().join(format!("velocity-rm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while std::time::Instant::now() < end {
                let _ = std::fs::create_dir_all(writer_dir.join("Default"));
                let _ = std::fs::write(writer_dir.join("Default/Preferences"), b"{}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let start = std::time::Instant::now();
        remove_profile_dir(&dir).await;
        writer.join().unwrap();
        assert!(!dir.exists(), "the profile dir survived");
        assert!(start.elapsed() < std::time::Duration::from_millis(1_500));
    }

    #[test]
    fn chrome_args_lose_their_dashes() {
        assert_eq!(chrome_arg("--no-sandbox"), ("no-sandbox", None));
        assert_eq!(
            chrome_arg("--js-flags=--expose-gc"),
            ("js-flags", Some("--expose-gc"))
        );
        assert_eq!(chrome_arg("lang=fr"), ("lang", Some("fr")));
    }

    #[test]
    fn fragment_only_changes_are_same_document() {
        let page = "http://127.0.0.1:47900/index.html";
        assert!(same_document(page, &format!("{page}#login")));
        assert!(same_document(
            &format!("{page}#home"),
            &format!("{page}#login")
        ));
        assert!(!same_document(page, page));
        assert!(!same_document(
            &format!("{page}#home"),
            "http://127.0.0.1:47900/other.html#x"
        ));
        assert!(!same_document("", &format!("{page}#home")));
    }
}
