//! The page's network: requests in flight (for the settle's network-idle
//! wait) and request mocking through CDP's Fetch domain.
//!
//! - **Network idle** (`web.networkIdle`, on by default): every request the
//!   page starts is tracked from `Network.requestWillBeSent` until it
//!   finishes or fails. The settle after an action isn't quiet while one is
//!   open, so a click that fetches data waits for the data, not just for the
//!   DOM to stop moving. Websockets, event streams and pings never count,
//!   nor do requests open longer than `web.networkIdleMaxMs` (long polls) or
//!   matching `web.networkIgnore`.
//! - **Mocks** (`web.mocks`, `web.block`, the `mockRequest` step): with any
//!   rule installed, Fetch pauses each request; a matching rule answers it
//!   (status, headers, JSON/text/file body, optional delay) or fails it, and
//!   everything else continues to the network. Fetch is off when there are
//!   no rules, so unmocked runs pay nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, DisableParams, EnableParams, EventRequestPaused, FailRequestParams,
    FulfillRequestParams, HeaderEntry, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{
    ErrorReason, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent, ResourceType,
};
use chromiumoxide::Page;

use crate::cdp::Tab;
use atalanta_common::{glob_match, AtalantaError, MockRule, Result};
use futures::StreamExt;
use tracing::{debug, warn};

/// One open request: its URL and when it started.
type Open = HashMap<String, (String, Instant)>;

/// Mock rules: the suite's (`web.mocks`/`web.block`) and the running
/// test's (`mockRequest`), each with how many requests it answered.
#[derive(Default)]
struct Rules {
    suite: Vec<(MockRule, u32)>,
    test: Vec<(MockRule, u32)>,
}

impl Rules {
    fn is_empty(&self) -> bool {
        self.suite.is_empty() && self.test.is_empty()
    }

    /// The rule for this request (the test's newest first, then the
    /// suite's), counted as used.
    fn take(&mut self, method: &str, url: &str) -> Option<MockRule> {
        let found = self
            .test
            .iter_mut()
            .rev()
            .chain(self.suite.iter_mut())
            .find(|(r, used)| r.times.is_none_or(|t| *used < t) && r.matches(method, url))?;
        found.1 += 1;
        Some(found.0.clone())
    }

    /// Whether a rule would answer `method url`, without using it up: for
    /// the CORS preflight in front of a mocked request.
    fn would_mock(&self, method: &str, url: &str) -> bool {
        self.test.iter().chain(self.suite.iter()).any(|(r, used)| {
            !r.block && r.times.is_none_or(|t| *used < t) && r.matches(method, url)
        })
    }
}

#[derive(Clone, Default)]
pub struct Network {
    open: Arc<Mutex<Open>>,
    /// Requests that ended, for a while: a cross-origin frame's requests
    /// end on the frames connection (W4), which can be read before the
    /// page's connection reports their start; a late start of one of these
    /// is not counted.
    ended: Arc<Mutex<HashMap<String, Instant>>>,
    rules: Arc<Mutex<Rules>>,
    fetch_on: Arc<std::sync::atomic::AtomicBool>,
    /// Cross-origin frames are attached (W4): their documents' and
    /// requests' ends are seen, so they are counted like any other.
    frames_on: Arc<std::sync::atomic::AtomicBool>,
    /// Child frames' document requests seen on the page's session:
    /// request id -> frame id. A cross-origin frame's document can be
    /// reported started here and never ended here once its frame goes;
    /// the frame's detach ends it (review-fi2 F7).
    frame_docs: Arc<Mutex<HashMap<String, String>>>,
}

/// How long an ended request is remembered (see `Network::ended`).
const ENDED_MEMORY: Duration = Duration::from_secs(30);

/// A frame's requests, from the frames connection (W4).
impl crate::frames::FrameNetwork for Network {
    fn started(&self, request_id: &str, url: &str, resource_type: Option<&str>) {
        let kind = resource_type
            .and_then(|t| serde_json::from_value::<ResourceType>(serde_json::Value::from(t)).ok());
        if counts(kind.as_ref(), url) {
            self.open_request(request_id, url);
        }
    }

    fn finished(&self, request_id: &str) {
        self.end_request(request_id);
    }

    /// Frame `frame_id` detached: its document request, started on the
    /// page's session, ends with it (review-fi2 F7).
    fn frame_gone(&self, frame_id: &str) {
        let ids: Vec<String> = self
            .frame_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, f)| f.as_str() == frame_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.end_request(&id);
        }
    }

    /// The frames connection died: their documents' ends can't be seen
    /// any more, so the VEL-132 heuristic is back on. Left on, every load
    /// with a cross-origin frame was busy until networkIdleMaxMs
    /// (review-i2 m4).
    fn closed(&self) {
        self.frames_on
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Network {
    pub fn new(suite_rules: Vec<MockRule>) -> Self {
        for rule in &suite_rules {
            warn_problems("web.mocks", rule);
        }
        let net = Self::default();
        net.rules.lock().unwrap_or_else(|e| e.into_inner()).suite =
            suite_rules.into_iter().map(|r| (r, 0)).collect();
        net
    }

    /// Watch `tab`'s requests and answer paused ones; turn Fetch on if
    /// the suite has rules.
    pub async fn attach(&self, tab: &Tab) -> Result<()> {
        self.watch_requests(&tab.page).await;
        self.watch_paused(&tab.page).await;
        // `tab` is a fresh page (a new session, or one restarted after
        // Chrome died): Fetch is off there whatever the last page had. The
        // flag stayed `true` from the dead page, so the new one never got
        // `Fetch.enable` and every mock was bypassed.
        self.fetch_on
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let have_rules = !self
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if let Err(e) = self.sync_fetch(tab).await {
            if have_rules {
                // No device id here: `open_page` (which knows `device_id`)
                // wraps this into `DeviceBootFailed`.
                return Err(AtalantaError::Internal(anyhow::anyhow!(
                    "request interception could not start ({e}); web.block/mock rules would not apply, so the launch is refused"
                )));
            }
            warn!("atalanta-web: mocks not installed: {e}");
        }
        Ok(())
    }

    /// Cross-origin frames are tracked from here on (W4).
    pub fn frames_attached(&self) {
        self.frames_on
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Remember that request `id` is frame `frame`'s document.
    fn frame_doc(&self, id: &str, frame: Option<&str>) {
        if let Some(frame) = frame {
            self.frame_docs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string(), frame.to_string());
        }
    }

    fn open_request(&self, id: &str, url: &str) {
        let ended = {
            let mut ended = self.ended.lock().unwrap_or_else(|e| e.into_inner());
            ended.retain(|_, at| at.elapsed() < ENDED_MEMORY);
            ended.contains_key(id)
        };
        if !ended {
            self.open
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string(), (url.to_string(), Instant::now()));
        }
    }

    fn end_request(&self, id: &str) {
        self.frame_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), Instant::now());
    }

    /// Whether a request the settle should wait for is open.
    pub fn busy(&self) -> bool {
        if !setting_on() {
            return false;
        }
        let max = atalanta_common::settings::ms("ATALANTA_WEB_NETWORK_IDLE_MAX_MS", 5000);
        let ignore = ignore_list();
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        // Past the limit an entry is never waited for again: drop it, so
        // the map doesn't grow for the life of the session (long polls,
        // requests whose end is reported elsewhere; NB-9 of web research).
        open.retain(|_, (_, at)| at.elapsed() < max);
        open.values()
            .any(|(url, _)| !ignore.iter().any(|p| glob_or_contains(p, url)))
    }

    /// Open requests' URLs (for a settle that hit its cap).
    pub fn open_urls(&self) -> Vec<String> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|(u, _)| u.clone())
            .collect()
    }

    pub async fn add_test_rule(&self, page: &Tab, rule: MockRule) -> Result<()> {
        warn_problems("mockRequest", &rule);
        self.rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .test
            .push((rule, 0));
        self.sync_fetch(page).await
    }

    pub async fn clear_test_rules(&self, page: &Tab) -> Result<()> {
        self.rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .test
            .clear();
        self.sync_fetch(page).await
    }

    /// Fetch on while there are rules, off when there are none. Through
    /// the `Tab`, so the call is bounded and a dead browser is a lost
    /// connection (a session restart), not an internal error.
    async fn sync_fetch(&self, page: &Tab) -> Result<()> {
        use std::sync::atomic::Ordering;
        let want = !self
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        if want == self.fetch_on.load(Ordering::SeqCst) {
            return Ok(());
        }
        // While mocks are on, a service worker must not answer the page's
        // requests from its own cache or `fetch`: Fetch on this target
        // never sees those, so the mock was bypassed (WA-8).
        // `ATALANTA_WEB_SW=allow` keeps the service worker in the path.
        if !matches!(
            atalanta_common::env::var("ATALANTA_WEB_SW").as_deref(),
            Ok("allow")
        ) {
            use chromiumoxide::cdp::browser_protocol::network::{
                EnableParams as NetworkEnableParams, SetBypassServiceWorkerParams,
            };
            if want {
                // Network must be on for the bypass to apply.
                let _ = page.exec(NetworkEnableParams::default()).await;
            }
            if let Err(e) = page.exec(SetBypassServiceWorkerParams::new(want)).await {
                debug!("atalanta-web: service-worker bypass not set: {e}");
            }
        }
        if want {
            // Test-only: make request interception fail so a web launch with
            // `web.block`/mock rules is refused instead of failing open
            // (`ATALANTA_WEB_TEST_FAIL_FETCH=1`, never set outside tests).
            if atalanta_common::env::var("ATALANTA_WEB_TEST_FAIL_FETCH").is_ok_and(|v| v == "1") {
                return Err(AtalantaError::Internal(anyhow::anyhow!(
                    "test hook: sync_fetch refused"
                )));
            }
            let pattern = RequestPattern {
                url_pattern: Some("*".into()),
                ..Default::default()
            };
            page.exec(EnableParams {
                patterns: Some(vec![pattern]),
                handle_auth_requests: None,
            })
            .await?;
        } else {
            page.exec(DisableParams::default()).await?;
        }
        self.fetch_on.store(want, Ordering::SeqCst);
        Ok(())
    }

    async fn watch_requests(&self, page: &Page) {
        let (Ok(mut sent), Ok(mut done), Ok(mut failed)) = (
            page.event_listener::<EventRequestWillBeSent>().await,
            page.event_listener::<EventLoadingFinished>().await,
            page.event_listener::<EventLoadingFailed>().await,
        ) else {
            warn!("atalanta-web: cannot watch network requests; no network-idle wait");
            return;
        };
        let net = self.clone();
        // A page target's main frame has the target's id. The frame tree
        // may not know it yet while the first navigation starts (under
        // load), and an unknown main frame counted every cross-origin
        // frame's document as in flight until networkIdleMaxMs.
        let main_frame = page.mainframe().await.ok().flatten().or_else(|| {
            Some(chromiumoxide::cdp::browser_protocol::page::FrameId::new(
                page.target_id().inner().clone(),
            ))
        });
        tokio::spawn(async move {
            // The top document's origin, from its own document request.
            let mut top_origin: Option<String> = None;
            loop {
                tokio::select! {
                    Some(e) = sent.next() => {
                        let is_doc = e.r#type == Some(ResourceType::Document);
                        let in_main = main_frame.is_some() && e.frame_id == main_frame;
                        if is_doc && in_main {
                            top_origin = origin_of(&e.request.url);
                        }
                        // A cross-origin frame's document loads in its own
                        // process (an OOPIF): its end is reported on the
                        // frame's own target, never here, so it stayed
                        // "in flight" until networkIdleMaxMs on every load
                        // (VEL-132).
                        // With the frames attached (W4) that end is seen
                        // on the frame's session, so it counts.
                        let oopif = is_doc
                            && !in_main
                            && main_frame.is_some()
                            && origin_of(&e.request.url) != top_origin
                            && !net.frames_on.load(std::sync::atomic::Ordering::SeqCst);
                        let initiator_other = matches!(
                            e.initiator.r#type,
                            chromiumoxide::cdp::browser_protocol::network::InitiatorType::Other
                        );
                        let favicon = browser_favicon(
                            e.r#type.as_ref(),
                            initiator_other,
                            &e.request.url,
                            top_origin.as_deref(),
                        );
                        if !oopif && !favicon && counts(e.r#type.as_ref(), &e.request.url) {
                            net.open_request(e.request_id.inner(), &e.request.url);
                            if is_doc && !in_main {
                                net.frame_doc(e.request_id.inner(), e.frame_id.as_ref().map(|f| f.inner().as_str()));
                            }
                        }
                    }
                    Some(e) = done.next() => net.end_request(e.request_id.inner()),
                    Some(e) = failed.next() => net.end_request(e.request_id.inner()),
                    else => break,
                }
            }
        });
    }

    async fn watch_paused(&self, page: &Page) {
        let mut paused = match page.event_listener::<EventRequestPaused>().await {
            Ok(p) => p,
            Err(e) => {
                warn!("atalanta-web: cannot intercept requests; no mocks: {e}");
                return;
            }
        };
        let rules = self.rules.clone();
        let page = page.clone();
        tokio::spawn(async move {
            while let Some(e) = paused.next().await {
                let headers = &e.request.headers;
                let answer_with = {
                    let mut rules = rules.lock().unwrap_or_else(|e| e.into_inner());
                    match preflight_for(&e.request.method, headers.inner()) {
                        // The browser asks before a cross-origin request
                        // with custom headers or a non-simple method. With
                        // no server behind the mock the preflight failed and
                        // the page never sent the mocked request (VEL-129).
                        // It doesn't use up the rule's `times`.
                        Some(method) if rules.would_mock(&method, &e.request.url) => {
                            Answer::Preflight
                        }
                        _ => match rules.take(&e.request.method, &e.request.url) {
                            Some(rule) => Answer::Mock(rule),
                            None => Answer::Continue,
                        },
                    }
                };
                let page = page.clone();
                // Each answer on its own task: a delayed mock mustn't hold
                // up the requests behind it.
                tokio::spawn(async move {
                    let id = e.request_id.clone();
                    if let Err(err) = answer(&page, &e, answer_with).await {
                        // A paused request nobody answers hangs the page's
                        // fetch forever (VEL-121): fail it instead.
                        warn!("atalanta-web: answering {} failed: {err}", e.request.url);
                        let _ = tokio::time::timeout(
                            ANSWER_TIMEOUT,
                            page.execute(FailRequestParams::new(id, ErrorReason::Failed)),
                        )
                        .await;
                    }
                });
            }
        });
    }
}

/// What to do with one paused request.
enum Answer {
    Continue,
    Preflight,
    Mock(MockRule),
}

/// Bound on one Fetch command: a call into a dead browser never returns.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// One Fetch command, bounded by [`ANSWER_TIMEOUT`].
async fn send<C: chromiumoxide::types::Command>(page: &Page, cmd: C) -> Result<()> {
    match tokio::time::timeout(ANSWER_TIMEOUT, page.execute(cmd)).await {
        Ok(r) => r.map(|_| ()).map_err(cdp_err),
        Err(_) => Err(cdp_err(format!(
            "no answer from Chrome in {ANSWER_TIMEOUT:?}"
        ))),
    }
}

/// A request header's value, by case-insensitive name.
fn header<'a>(headers: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))?
        .1
        .as_str()
}

/// The method a CORS preflight asks about; `None` for any other request.
fn preflight_for(method: &str, headers: &serde_json::Value) -> Option<String> {
    if !method.eq_ignore_ascii_case("OPTIONS") {
        return None;
    }
    header(headers, "origin")?;
    header(headers, "access-control-request-method").map(str::to_string)
}

/// The 204 a CORS-aware server sends for a preflight: whatever the browser
/// asked for, allowed (Playwright answers mocked preflights the same way).
fn preflight_headers(req: &serde_json::Value) -> Vec<HeaderEntry> {
    let mut h = vec![
        HeaderEntry::new(
            "access-control-allow-origin",
            header(req, "origin").unwrap_or("*"),
        ),
        HeaderEntry::new("access-control-allow-credentials", "true"),
        HeaderEntry::new("access-control-max-age", "600"),
        HeaderEntry::new("vary", "Origin"),
    ];
    if let Some(m) = header(req, "access-control-request-method") {
        h.push(HeaderEntry::new("access-control-allow-methods", m));
    }
    if let Some(x) = header(req, "access-control-request-headers") {
        h.push(HeaderEntry::new("access-control-allow-headers", x));
    }
    h
}

/// CORS headers for a mocked response the rule didn't set itself. With an
/// `Origin` it is echoed with credentials allowed: `*` is refused for a
/// request sent with cookies (`credentials: 'include'`) (VEL-129).
fn cors_headers(req: &serde_json::Value, headers: &mut Vec<HeaderEntry>) {
    let has = |h: &[HeaderEntry], name: &str| h.iter().any(|x| x.name.eq_ignore_ascii_case(name));
    if has(headers, "access-control-allow-origin") {
        return;
    }
    match header(req, "origin") {
        Some(origin) => {
            // A cross-origin page can only read safelisted response headers
            // unless they're exposed, and `*` doesn't work with credentials:
            // name the rule's own (`x-total-count`, ...).
            let exposed: Vec<String> = headers
                .iter()
                .map(|h| h.name.to_ascii_lowercase())
                .filter(|n| !n.starts_with("access-control-") && n != "content-type")
                .collect();
            if !exposed.is_empty() && !has(headers, "access-control-expose-headers") {
                headers.push(HeaderEntry::new(
                    "access-control-expose-headers",
                    exposed.join(", "),
                ));
            }
            headers.push(HeaderEntry::new("access-control-allow-origin", origin));
            if !has(headers, "access-control-allow-credentials") {
                headers.push(HeaderEntry::new("access-control-allow-credentials", "true"));
            }
            if !has(headers, "vary") {
                headers.push(HeaderEntry::new("vary", "Origin"));
            }
        }
        None => headers.push(HeaderEntry::new("access-control-allow-origin", "*")),
    }
}

async fn answer(page: &Page, e: &EventRequestPaused, what: Answer) -> Result<()> {
    let id = e.request_id.clone();
    let rule = match what {
        Answer::Continue => return send(page, ContinueRequestParams::new(id)).await,
        Answer::Preflight => {
            debug!("atalanta-web: CORS preflight for mocked {}", e.request.url);
            let mut params = FulfillRequestParams::new(id, 204);
            params.response_headers = Some(preflight_headers(e.request.headers.inner()));
            return send(page, params).await;
        }
        Answer::Mock(rule) => rule,
    };
    if let Some(ms) = rule.delay_ms {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    if rule.block {
        debug!("atalanta-web: blocked {}", e.request.url);
        return send(
            page,
            FailRequestParams::new(id, ErrorReason::BlockedByClient),
        )
        .await;
    }
    let (body, content_type) = mock_body(&rule)?;
    let mut headers: Vec<HeaderEntry> = rule
        .headers
        .iter()
        .map(|(k, v)| HeaderEntry::new(k.clone(), v.clone()))
        .collect();
    let has = |h: &[HeaderEntry], name: &str| h.iter().any(|x| x.name.eq_ignore_ascii_case(name));
    if let Some(ct) = content_type {
        if !has(&headers, "content-type") {
            headers.push(HeaderEntry::new("content-type", ct));
        }
    }
    // A mocked cross-origin API must still pass CORS.
    cors_headers(e.request.headers.inner(), &mut headers);
    debug!(
        "atalanta-web: mocked {} {} -> {}",
        e.request.method,
        e.request.url,
        rule.status.unwrap_or(200)
    );
    let mut params = FulfillRequestParams::new(id, rule.status.unwrap_or(200) as i64);
    params.response_headers = Some(headers);
    params.body = Some(
        base64::engine::general_purpose::STANDARD
            .encode(body)
            .into(),
    );
    send(page, params).await
}

/// What is wrong with a mock rule, found when it is loaded rather than when
/// a request first hits it (and fails, VEL-121) or never matches.
pub fn mock_problems(rule: &MockRule) -> Vec<String> {
    let mut out = Vec::new();
    if rule.url.is_empty() {
        out.push("an empty `url` matches every request".to_string());
    }
    if let Some(s) = rule.status {
        if !(100..=599).contains(&s) {
            out.push(format!("status {s} is not an HTTP status"));
        }
    }
    let bodies = [
        rule.json.is_some(),
        rule.body.is_some(),
        rule.file.is_some(),
    ];
    if bodies.iter().filter(|b| **b).count() > 1 {
        out.push("more than one of `json`, `body` and `file`; the first one wins".to_string());
    }
    if rule.block && (bodies.contains(&true) || rule.status.is_some()) {
        out.push("`block` fails the request, so its response is never sent".to_string());
    }
    if let Some(f) = &rule.file {
        if let Err(e) = std::fs::metadata(f) {
            out.push(format!("file {f}: {e}"));
        }
    }
    out
}

/// Logs [`mock_problems`]. A warning, not an error: such a rule used to
/// load fine and only fail the requests it answers.
fn warn_problems(source: &str, rule: &MockRule) {
    for p in mock_problems(rule) {
        warn!("atalanta-web: {source} rule for {:?}: {p}", rule.url);
    }
}

/// The body a rule answers with, and its default content type.
fn mock_body(rule: &MockRule) -> Result<(Vec<u8>, Option<&'static str>)> {
    if let Some(j) = &rule.json {
        let text = serde_json::to_vec(j).map_err(|e| AtalantaError::Config(e.to_string()))?;
        return Ok((text, Some("application/json")));
    }
    if let Some(b) = &rule.body {
        return Ok((b.clone().into_bytes(), Some("text/plain; charset=utf-8")));
    }
    if let Some(f) = &rule.file {
        let bytes =
            std::fs::read(f).map_err(|e| AtalantaError::Config(format!("mock file {f}: {e}")))?;
        return Ok((bytes, content_type_for(f)));
    }
    Ok((Vec::new(), None))
}

fn content_type_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Whether a request of this type to this URL is one the settle waits for.
/// `scheme://host[:port]` of a URL, `None` when it has none.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        host.to_ascii_lowercase()
    ))
}

/// The browser's own `/favicon.ico` fetch: no page waits on it, and its
/// end is often never reported to the page's session, so it stayed "in
/// flight" until networkIdleMaxMs on about one load in seven (C7: the
/// residual WA-10 flake was this request, not the iframe). Only that one:
/// of type `Other` (or none), started by the browser (initiator `other`,
/// or none), at the top document's origin root. An `<img>` or `fetch()`
/// of some `favicon.ico` is the page's own and is waited for (review-i1-i3
/// m4).
fn browser_favicon(
    kind: Option<&ResourceType>,
    initiator_other: bool,
    url: &str,
    top_origin: Option<&str>,
) -> bool {
    let url = url.split('#').next().unwrap_or(url);
    matches!(kind, None | Some(ResourceType::Other))
        && initiator_other
        && top_origin.is_some_and(|o| url == format!("{o}/favicon.ico"))
}

fn counts(kind: Option<&ResourceType>, url: &str) -> bool {
    if url.starts_with("data:") || url.starts_with("blob:") {
        return false;
    }
    !matches!(
        kind,
        Some(ResourceType::WebSocket | ResourceType::EventSource | ResourceType::Ping)
    )
}

fn setting_on() -> bool {
    !matches!(
        atalanta_common::env::var("ATALANTA_WEB_NETWORK_IDLE").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn ignore_list() -> Vec<String> {
    atalanta_common::env::var("ATALANTA_WEB_NETWORK_IGNORE")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `web.networkIgnore` patterns read like mock URLs: `*` globs, `?` is
/// literal (VEL-076).
fn glob_or_contains(pattern: &str, url: &str) -> bool {
    if pattern.contains('*') {
        glob_match(pattern, url)
    } else {
        url.contains(pattern)
    }
}

fn cdp_err(e: impl std::fmt::Display) -> AtalantaError {
    AtalantaError::Internal(anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review-fi2 F7: a child frame's document, started on the page's
    /// session, ends when that frame detaches; other frames' don't.
    #[test]
    fn a_gone_frames_document_request_ends() {
        use crate::frames::FrameNetwork;
        let net = Network::default();
        net.open_request("doc1", "https://ads.test/a.html");
        net.frame_doc("doc1", Some("F1"));
        net.open_request("doc2", "https://pay.test/b.html");
        net.frame_doc("doc2", Some("F2"));
        assert_eq!(net.open.lock().unwrap().len(), 2);
        net.frame_gone("F1");
        let open = net.open.lock().unwrap();
        assert!(!open.contains_key("doc1"));
        assert!(open.contains_key("doc2"));
    }

    /// P8: broken mock rules are reported when they are loaded.
    #[test]
    fn broken_mock_rules_are_reported_at_load() {
        let ok = MockRule {
            url: "/api".into(),
            json: Some(serde_json::json!({})),
            ..Default::default()
        };
        assert!(mock_problems(&ok).is_empty());
        let bad = MockRule {
            url: String::new(),
            status: Some(1000),
            body: Some("x".into()),
            file: Some("/nonexistent/atalanta-mock.json".into()),
            ..Default::default()
        };
        let p = mock_problems(&bad).join("\n");
        for want in [
            "empty `url`",
            "status 1000",
            "more than one",
            "/nonexistent",
        ] {
            assert!(p.contains(want), "{want}: {p}");
        }
        let blocked = MockRule {
            url: "/ads".into(),
            block: true,
            status: Some(200),
            ..Default::default()
        };
        assert!(mock_problems(&blocked)[0].contains("block"));
    }

    #[test]
    fn the_newest_test_rule_wins_and_times_run_out() {
        let mut r = Rules::default();
        r.suite.push((
            MockRule {
                url: "/api/users".into(),
                status: Some(200),
                ..Default::default()
            },
            0,
        ));
        r.test.push((
            MockRule {
                url: "/api/users".into(),
                status: Some(500),
                times: Some(1),
                ..Default::default()
            },
            0,
        ));
        let u = "http://h/api/users";
        assert_eq!(r.take("GET", u).unwrap().status, Some(500));
        assert_eq!(
            r.take("GET", u).unwrap().status,
            Some(200),
            "times: 1 used up"
        );
        assert!(r.take("GET", "http://h/other").is_none());
    }

    #[test]
    fn streams_and_inline_urls_never_hold_the_settle() {
        assert!(counts(Some(&ResourceType::Fetch), "http://h/a"));
        assert!(counts(None, "http://h/a"));
        assert!(!counts(Some(&ResourceType::WebSocket), "ws://h"));
        assert!(!counts(Some(&ResourceType::EventSource), "http://h/s"));
        assert!(!counts(Some(&ResourceType::Fetch), "data:text/plain,x"));
    }

    #[test]
    fn mock_bodies_carry_their_content_type() {
        let j = MockRule {
            url: "x".into(),
            json: Some(serde_json::json!({ "a": 1 })),
            ..Default::default()
        };
        let (b, ct) = mock_body(&j).unwrap();
        assert_eq!(
            (b.as_slice(), ct),
            (br#"{"a":1}"#.as_slice(), Some("application/json"))
        );
        assert_eq!(content_type_for("users.JSON"), Some("application/json"));
    }

    fn names(h: &[HeaderEntry]) -> Vec<(String, String)> {
        h.iter()
            .map(|x| (x.name.to_ascii_lowercase(), x.value.clone()))
            .collect()
    }

    #[test]
    fn a_preflight_for_a_mocked_request_is_answered_without_using_it() {
        let req = serde_json::json!({
            "Origin": "http://127.0.0.1:8000",
            "Access-Control-Request-Method": "POST",
            "Access-Control-Request-Headers": "content-type,x-token",
        });
        assert_eq!(preflight_for("OPTIONS", &req).as_deref(), Some("POST"));
        assert_eq!(preflight_for("GET", &req), None);
        assert_eq!(
            preflight_for("OPTIONS", &serde_json::json!({ "Origin": "x" })),
            None,
            "a plain OPTIONS is no preflight"
        );
        let h = names(&preflight_headers(&req));
        for want in [
            ("access-control-allow-origin", "http://127.0.0.1:8000"),
            ("access-control-allow-methods", "POST"),
            ("access-control-allow-headers", "content-type,x-token"),
            ("access-control-allow-credentials", "true"),
        ] {
            assert!(
                h.contains(&(want.0.into(), want.1.into())),
                "{want:?} in {h:?}"
            );
        }

        let mut r = Rules::default();
        r.test.push((
            MockRule {
                url: "/api/save".into(),
                method: Some("POST".into()),
                times: Some(1),
                ..Default::default()
            },
            0,
        ));
        let u = "http://api.test/api/save";
        assert!(r.would_mock("POST", u));
        assert!(!r.would_mock("DELETE", u));
        assert!(
            r.take("POST", u).is_some(),
            "the preflight didn't use the one time"
        );
        assert!(!r.would_mock("POST", u));
    }

    /// review-i2 m4: the frames connection going clears the flag that
    /// turned the VEL-132 heuristic off.
    #[test]
    fn a_closed_frames_connection_clears_frames_on() {
        use crate::frames::FrameNetwork;
        let net = Network::default();
        net.frames_attached();
        assert!(net.frames_on.load(std::sync::atomic::Ordering::SeqCst));
        net.closed();
        assert!(!net.frames_on.load(std::sync::atomic::Ordering::SeqCst));
    }

    // C7: the browser's favicon fetch is not the page's network.
    #[test]
    fn the_favicon_fetch_does_not_count() {
        let top = Some("http://127.0.0.1:5000");
        let other = Some(&ResourceType::Other);
        assert!(browser_favicon(
            None,
            true,
            "http://127.0.0.1:5000/favicon.ico",
            top
        ));
        assert!(browser_favicon(
            other,
            true,
            "http://127.0.0.1:5000/favicon.ico",
            top
        ));
        // review-i1-i3 m4: the page's own requests for some favicon.ico
        // are waited for: an <img>, a fetch, a script-started one, another
        // origin's, one under a path or with a query.
        let img = Some(&ResourceType::Image);
        let fetch = Some(&ResourceType::Fetch);
        let u = "http://127.0.0.1:5000/favicon.ico";
        assert!(!browser_favicon(img, false, u, top));
        assert!(!browser_favicon(fetch, false, u, top));
        assert!(!browser_favicon(other, false, u, top));
        assert!(!browser_favicon(
            other,
            true,
            "https://site.test/favicon.ico",
            top
        ));
        assert!(!browser_favicon(
            fetch,
            false,
            "http://127.0.0.1:5000/api/x/favicon.ico",
            top
        ));
        assert!(!browser_favicon(
            other,
            true,
            "http://127.0.0.1:5000/favicon.ico?v=2",
            top
        ));
        assert!(!browser_favicon(other, true, u, None));
        assert!(counts(img, "https://site.test/favicon.ico"));
        assert!(counts(fetch, "http://127.0.0.1:5000/api/x/favicon.ico"));
        assert!(counts(
            Some(&ResourceType::Image),
            "https://example.com/img/favicon.png"
        ));
        assert!(counts(
            Some(&ResourceType::Fetch),
            "https://example.com/api/items"
        ));
    }

    /// Review MINOR-9: a cross-origin page can read the rule's own
    /// headers; without `Access-Control-Expose-Headers` it saw `null`.
    #[test]
    fn a_mocks_own_headers_are_exposed_cross_origin() {
        let mut h = vec![
            HeaderEntry::new("X-Total-Count", "3"),
            HeaderEntry::new("content-type", "application/json"),
            HeaderEntry::new("etag", "abc"),
        ];
        cors_headers(&serde_json::json!({ "origin": "http://a.test" }), &mut h);
        assert!(
            names(&h).contains(&(
                "access-control-expose-headers".into(),
                "x-total-count, etag".into()
            )),
            "{:?}",
            names(&h)
        );
        // Nothing to expose, or the rule exposes its own: left alone.
        let mut h = vec![HeaderEntry::new("content-type", "text/plain")];
        cors_headers(&serde_json::json!({ "origin": "http://a.test" }), &mut h);
        assert!(!names(&h)
            .iter()
            .any(|(n, _)| n == "access-control-expose-headers"));
        let mut h = vec![
            HeaderEntry::new("x-a", "1"),
            HeaderEntry::new("Access-Control-Expose-Headers", "x-a"),
        ];
        cors_headers(&serde_json::json!({ "origin": "http://a.test" }), &mut h);
        assert_eq!(
            names(&h)
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("access-control-expose-headers"))
                .count(),
            1
        );
    }

    #[test]
    fn mocked_responses_echo_the_origin_so_credentialed_requests_pass() {
        let mut h = Vec::new();
        cors_headers(&serde_json::json!({ "origin": "http://a.test" }), &mut h);
        let h = names(&h);
        assert!(h.contains(&("access-control-allow-origin".into(), "http://a.test".into())));
        assert!(h.contains(&("access-control-allow-credentials".into(), "true".into())));

        let mut h = Vec::new();
        cors_headers(&serde_json::json!({}), &mut h);
        assert_eq!(
            names(&h),
            [("access-control-allow-origin".into(), "*".into())]
        );

        let mut h = vec![HeaderEntry::new(
            "Access-Control-Allow-Origin",
            "http://b.test",
        )];
        cors_headers(&serde_json::json!({ "Origin": "http://a.test" }), &mut h);
        assert_eq!(h.len(), 1, "the rule's own CORS headers are kept");
    }
}
