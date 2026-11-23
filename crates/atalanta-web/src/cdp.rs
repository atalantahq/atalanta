use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atalanta_common::{AtalantaError, Result};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::CreateIsolatedWorldParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    CallArgument, CallFunctionOnParams, ExecutionContextId,
};
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::frame::UTILITY_WORLD_NAME;
use chromiumoxide::js::EvaluationResult;
use chromiumoxide::page::Page as CdpPage;
use chromiumoxide::types::{Command, CommandResponse};
use serde_json::Value;

/// Upper bound on one CDP round-trip. chromiumoxide's own 30 s request
/// timeout runs inside its handler, so it never fires once the handler has
/// stopped (Chrome died); without this, every later call hung until the step
/// watchdog (VEL-020).
pub const CDP_TIMEOUT: Duration = Duration::from_secs(10);

/// The most a lookup spends in the page's cross-origin frames
/// ([`Tab::call_frames`]), all frames together.
pub const FRAMES_BUDGET: Duration = Duration::from_secs(2);
/// The least time one frame gets of [`FRAMES_BUDGET`] (many frames).
const FRAME_MIN_SHARE: Duration = Duration::from_millis(250);

/// What a lookup in the cross-origin frames ([`Tab::call_frames`]) got.
#[derive(Debug, Default, Clone)]
pub struct FrameReplies {
    /// Each frame's reply that matched, in the page's terms.
    pub found: Vec<Value>,
    /// The frames not asked or that didn't answer (`frame 2 (url)`):
    /// whether they hold a match is unknown (review-fi2 F1).
    pub skipped: Vec<String>,
}

/// The page plus whether its Chrome connection is still up. Every CDP call
/// goes through [`Tab::run`]: it is bounded by [`CDP_TIMEOUT`], and once the
/// connection is gone it fails at once with `BrowserConnectionLost` (a
/// transient error, so the executor restarts the session) instead of
/// hanging or reporting an internal error.
pub struct Tab {
    pub page: CdpPage,
    alive: Arc<AtomicBool>,
    /// A touch device profile (`web.device`, D-W3): taps are touches.
    touch: bool,
    /// The page's cross-origin frames (W4), when attached.
    frames: std::sync::Mutex<Option<Arc<crate::frames::Frames>>>,
    /// The page's renderer crashed ("Aw, Snap!": out of memory,
    /// `chrome://crash`). The browser's socket stays up, so `alive` can't
    /// tell, and every later call failed as an internal error after
    /// [`CDP_TIMEOUT`] (review 10 #6).
    crashed: AtomicBool,
}

/// Kept as `Page` so the helpers below read as before.
pub type Page = Tab;

impl Tab {
    pub fn new(page: CdpPage, alive: Arc<AtomicBool>) -> Self {
        Self {
            page,
            alive,
            touch: false,
            frames: std::sync::Mutex::new(None),
            crashed: AtomicBool::new(false),
        }
    }

    /// The renderer is gone: from now on every call is a lost connection,
    /// a transient error the executor recovers from with a new browser.
    pub fn mark_crashed(&self) {
        self.crashed.store(true, Ordering::SeqCst);
    }

    pub fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }

    /// Taps on this page are touches (`Input.dispatchTouchEvent`), as a
    /// touch device's are.
    pub fn with_touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }

    pub fn touch(&self) -> bool {
        self.touch
    }

    /// Track the page's cross-origin frames through `frames` (W4).
    pub fn set_frames(&self, frames: Arc<crate::frames::Frames>) {
        *self.frames.lock().unwrap_or_else(|e| e.into_inner()) = Some(frames);
    }

    /// The page's cross-origin frames, while their connection is up.
    pub fn frames(&self) -> Option<Arc<crate::frames::Frames>> {
        self.frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|f| f.is_alive())
    }

    /// Run `op` on the element `id` (its id first, then `rest`), in the
    /// document that holds it: the page, or a cross-origin frame (W4),
    /// whose reply comes back in the page's coordinates.
    pub async fn call_el(&self, op: &str, id: &str, rest: Vec<Value>) -> Result<Value> {
        let inner = crate::frames::Frames::split(id).map_or(id, |(_, i)| i);
        let mut args = vec![Value::from(inner)];
        args.extend(rest);
        self.call_near(id, CDP_TIMEOUT, op, args).await
    }

    /// Run `op` with `args` in the document that holds the element `id`
    /// (`disarm` and `caretToEnd` after a click on it).
    pub async fn call_near(
        &self,
        id: &str,
        limit: Duration,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        let Some((key, _)) = crate::frames::Frames::split(id) else {
            return self.call_for(limit, op, args).await;
        };
        let frames = self.frames();
        let Some(frames) = frames.filter(|f| f.keys().contains(&key)) else {
            // The frame went with its element.
            return Ok(match op {
                "textIs" => serde_json::json!({ "gone": true }),
                "act" => serde_json::json!({ "ok": false, "reason": "element detached from DOM" }),
                "disarm" => serde_json::json!({ "blocked": null }),
                _ => Value::Null,
            });
        };
        let force = op == "act" && args.get(2).and_then(Value::as_str) == Some("force");
        // The frame's scroll, the call and its offset share one limit: a
        // frame that hangs costs `limit`, not four CDP timeouts per send
        // (review-i2 M2).
        let work = async {
            if op == "act" || op == "reveal" {
                // The frame's own scrolling can't bring its <iframe> into
                // the page's view.
                let _ = frames.reveal_frame(key).await;
            }
            let v = frames.call(key, op, args).await?;
            Ok::<_, AtalantaError>(match op {
                "find" | "findAll" | "reveal" | "act" => {
                    let offset = frames.offset(key).await?;
                    let mut v = crate::frames::Frames::to_page(op, key, offset, v);
                    if op == "act" && v["ok"] == true && !force {
                        // The frame's hit test can't see the page: a banner
                        // over the <iframe> takes the click (review-i2 M1).
                        let (x, y) = (v["x"].as_f64(), v["y"].as_f64());
                        if let (Some(x), Some(y)) = (x, y) {
                            match frames.intruder(key, x, y).await {
                                Ok(Some(who)) => {
                                    v = serde_json::json!({
                                        "ok": false,
                                        "reason": format!("obscured by {who}"),
                                    });
                                }
                                Ok(None) => {}
                                Err(e) => tracing::debug!(
                                    "atalanta-web: frame {key}: no page hit test: {e}"
                                ),
                            }
                        }
                    }
                    v
                }
                _ => v,
            })
        };
        tokio::time::timeout(limit, work).await.map_err(|_| {
            cdp_err(format!(
                "a cross-origin frame did not answer {op} in {limit:?}"
            ))
        })?
    }

    /// `op` (`find` or `findAll`) in each cross-origin frame, replies in
    /// the page's terms, and the frames that weren't asked or didn't
    /// answer (their content is unknown). Frames that fail (navigating)
    /// are left out. The lookup has [`FRAMES_BUDGET`], split across the
    /// frames, so a stuck ad frame can't stall every lookup (review-i2
    /// M2); but only the frame's own reply is timed against its share
    /// (review-fi2 F1):
    ///
    /// - the library's first install in a document runs in the
    ///   background: a frame still loading is waited for once, then
    ///   reported skipped until the install is done;
    /// - the frame's offset (`DOM.getBoxModel` on the page) is not the
    ///   frame's time;
    /// - a frame is left out as hung only after
    ///   [`OVERRUNS_TO_HUNG`](crate::frames::OVERRUNS_TO_HUNG) overruns in
    ///   a row, and only for a few seconds: then it is pinged.
    pub async fn call_frames(&self, op: &str, args: Vec<Value>) -> FrameReplies {
        let mut out = FrameReplies::default();
        let Some(frames) = self.frames() else {
            return out;
        };
        let keys = frames.keys();
        if keys.is_empty() {
            return out;
        }
        use crate::frames::FrameState;
        let now = tokio::time::Instant::now;
        let deadline = now() + FRAMES_BUDGET;
        let asked = frames.askable().max(1) as u32;
        let share = (FRAMES_BUDGET / asked).max(FRAME_MIN_SHARE);
        for key in keys {
            let left = deadline.saturating_duration_since(now());
            let skip = |out: &mut FrameReplies, why: &str| {
                tracing::debug!("atalanta-web: frame {key} skipped: {why}");
                out.skipped.push(frames.describe(key));
            };
            if left.is_zero() {
                skip(&mut out, "the frame lookups are out of time");
                continue;
            }
            match frames.state(key) {
                None => continue,
                Some(FrameState::Hung) => {
                    skip(&mut out, "it is hung");
                    continue;
                }
                Some(FrameState::Recheck) => {
                    if frames.ping(key, crate::frames::PING_LIMIT.min(left)).await {
                        frames.answered(key);
                    } else {
                        frames.mark_hung(key, "no answer to a ping");
                        skip(&mut out, "it is still hung");
                        continue;
                    }
                }
                Some(FrameState::Installing) => {
                    skip(&mut out, "its document is still loading");
                    continue;
                }
                Some(FrameState::Bare) => {
                    frames.start_install(key);
                    if !frames.wait_ready(key, share.min(left)).await {
                        skip(&mut out, "its document is still loading");
                        continue;
                    }
                }
                Some(FrameState::Ready) => {}
            }
            let limit = share.min(deadline.saturating_duration_since(now()));
            if limit.is_zero() {
                skip(&mut out, "the frame lookups are out of time");
                continue;
            }
            let v = match tokio::time::timeout(limit, frames.call(key, op, args.clone())).await {
                Ok(Ok(v)) => {
                    frames.answered(key);
                    v
                }
                Ok(Err(e)) if crate::frames::is_timeout(&e) => {
                    frames.mark_hung(key, &e.to_string());
                    skip(&mut out, "no answer");
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::debug!("atalanta-web: frame {key}: {e}");
                    continue;
                }
                Err(_) => {
                    frames.overran(key, &format!("{op} took over {limit:?}"));
                    skip(&mut out, "no answer in its share");
                    continue;
                }
            };
            if v.is_null() || v.as_array().is_some_and(|a| a.is_empty()) {
                continue;
            }
            // Where the frame is on the page: the page's time, not the
            // frame's, so it never counts against the frame.
            let left = deadline
                .saturating_duration_since(now())
                .max(FRAME_MIN_SHARE);
            match tokio::time::timeout(left, frames.offset(key)).await {
                Ok(Ok(offset)) => out
                    .found
                    .push(crate::frames::Frames::to_page(op, key, offset, v)),
                Ok(Err(e)) if !crate::frames::is_timeout(&e) => {
                    tracing::debug!("atalanta-web: frame {key}: {e}")
                }
                _ => skip(&mut out, "its place on the page is unknown"),
            }
        }
        out
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Run one CDP future under [`CDP_TIMEOUT`].
    pub async fn run<T>(
        &self,
        fut: impl Future<Output = std::result::Result<T, CdpError>>,
    ) -> Result<T> {
        self.run_for(CDP_TIMEOUT, fut).await
    }

    /// [`Tab::run`] with its own limit (health checks, navigations).
    pub async fn run_for<T>(
        &self,
        limit: Duration,
        fut: impl Future<Output = std::result::Result<T, CdpError>>,
    ) -> Result<T> {
        if !self.is_alive() {
            return Err(lost("the browser connection is closed"));
        }
        if self.is_crashed() {
            return Err(lost(RENDERER_CRASHED));
        }
        match tokio::time::timeout(limit, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) if !self.is_alive() || is_connection_error(&e) => {
                self.alive.store(false, Ordering::SeqCst);
                Err(lost(e))
            }
            // The crash event may come after the call's own error.
            Ok(Err(e)) if self.is_crashed() || is_crash_error(&e.to_string()) => {
                self.mark_crashed();
                Err(lost(format!("{RENDERER_CRASHED} ({e})")))
            }
            Ok(Err(e)) => Err(cdp_err(e)),
            Err(_) if !self.is_alive() => Err(lost("the browser connection is closed")),
            Err(_) if self.is_crashed() => Err(lost(RENDERER_CRASHED)),
            Err(_) => Err(AtalantaError::Internal(anyhow::anyhow!(
                "Chrome did not answer a DevTools call within {limit:?}"
            ))),
        }
    }

    pub async fn eval(&self, js: impl Into<String>) -> Result<EvaluationResult> {
        let js: String = js.into();
        self.run(self.page.evaluate(js)).await
    }

    pub async fn exec<C: Command>(&self, cmd: C) -> Result<CommandResponse<C::Response>> {
        self.run(self.page.execute(cmd)).await
    }

    /// Run `op` of Atalanta's in-page library (`injected.js`) with `args`,
    /// in the isolated world, under [`CDP_TIMEOUT`].
    pub async fn call(&self, op: &str, args: Vec<Value>) -> Result<Value> {
        self.call_for(CDP_TIMEOUT, op, args).await
    }

    /// [`Tab::call`] with its own limit per round-trip (the settle).
    ///
    /// The library runs in an isolated world (chromiumoxide's utility
    /// world, one per document) through `Runtime.callFunctionOn`, with
    /// the arguments as JSON values. Scripts used to be spliced together
    /// and evaluated in the page's own world, where a page that patched
    /// `querySelectorAll` or `JSON` broke every query and could read or
    /// forge Atalanta's element ids (research P4). The library is
    /// installed on first use in each document.
    pub async fn call_for(&self, limit: Duration, op: &str, args: Vec<Value>) -> Result<Value> {
        let call_args = [Value::from(op), Value::Array(args)];
        let mut last = None;
        for attempt in 0..4 {
            if attempt > 0 {
                // A navigation is replacing the document: give the new
                // world a moment to appear.
                tokio::time::sleep(Duration::from_millis(25 * attempt)).await;
            }
            let ctx = self.isolated_context().await?;
            match self.call_in(limit, ctx, DISPATCH, &call_args).await {
                Ok(v) if v.get("velMissing").is_some() => {
                    let install = format!("function(){{{LIBRARY}\nreturn true;}}");
                    match self.call_in(CDP_TIMEOUT, ctx, &install, &[]).await {
                        Err(e) if stale_context(&e) => last = Some(e),
                        Err(e) => return Err(e),
                        Ok(_) => {}
                    }
                }
                Ok(v) => return Ok(v),
                Err(e) if stale_context(&e) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| cdp_err("the page's script context never became ready")))
    }

    async fn call_in(
        &self,
        limit: Duration,
        ctx: ExecutionContextId,
        function: &str,
        args: &[Value],
    ) -> Result<Value> {
        let mut call = CallFunctionOnParams::new(function);
        call.execution_context_id = Some(ctx);
        call.arguments = Some(
            args.iter()
                .map(|v| CallArgument::builder().value(v.clone()).build())
                .collect(),
        );
        call.return_by_value = Some(true);
        call.await_promise = Some(true);
        let result = self
            .run_for(limit, self.page.evaluate_function(call))
            .await?;
        // `undefined` has no value; to callers it is the same as `null`.
        Ok(result.value().cloned().unwrap_or(Value::Null))
    }

    /// `DOM.setFileInputFiles` on the file input the element `id` stands
    /// for, in the page or a same-origin frame (a cross-origin frame's
    /// inputs are refused). Chrome fires `input` and `change` itself.
    pub async fn set_files(&self, id: &str, paths: Vec<String>) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
        if crate::frames::Frames::split(id).is_some() {
            return Err(cdp_err(
                "a file input inside a cross-origin frame can't be given files",
            ));
        }
        // Installs the library in this document if it isn't yet.
        if self.call("fileInput", vec![id.into()]).await?.is_null() {
            return Err(cdp_err("the element is not a file input"));
        }
        let ctx = self.isolated_context().await?;
        let mut call = CallFunctionOnParams::new(
            "function(id){return globalThis.__atalanta.fileInputElement(id);}",
        );
        call.execution_context_id = Some(ctx);
        call.arguments = Some(vec![CallArgument::builder().value(id).build()]);
        call.return_by_value = Some(false);
        let reply = self.exec(call).await?;
        let object = reply
            .result
            .result
            .object_id
            .clone()
            .ok_or_else(|| cdp_err("the file input left the page"))?;
        let mut params = SetFileInputFilesParams::new(paths);
        params.object_id = Some(object);
        self.exec(params).await?;
        Ok(())
    }

    /// The current document's isolated world. chromiumoxide creates one per
    /// document (and tracks its id); right after a navigation it may not be
    /// known yet, so it is asked for (Chrome returns the same world).
    async fn isolated_context(&self) -> Result<ExecutionContextId> {
        if let Some(id) = self.run(self.page.secondary_execution_context()).await? {
            return Ok(id);
        }
        let frame = self
            .run(self.page.mainframe())
            .await?
            .ok_or_else(|| cdp_err("the page has no main frame"))?;
        let mut params = CreateIsolatedWorldParams::new(frame);
        params.world_name = Some(UTILITY_WORLD_NAME.into());
        params.grant_univeral_access = Some(true);
        Ok(self.exec(params).await?.result.execution_context_id)
    }
}

/// Atalanta's in-page library, see [`Tab::call`].
pub(crate) const LIBRARY: &str = include_str!("injected.js");

/// Calls one library function; says so when the library isn't in this
/// document yet.
pub(crate) const DISPATCH: &str = "function(op,args){var v=globalThis.__atalanta;\
if(!v)return{velMissing:true};return v[op].apply(null,args);}";

/// Errors that mean the execution context went away under the call (the
/// document was replaced): worth a retry in the new one.
fn stale_context(e: &AtalantaError) -> bool {
    let m = e.to_string();
    m.contains("Cannot find context") || m.contains("context was destroyed")
}

/// Errors that mean the DevTools socket or the channel to its handler is
/// gone, as opposed to a command Chrome answered with an error.
pub(crate) fn is_connection_error(e: &CdpError) -> bool {
    matches!(
        e,
        CdpError::Ws(_) | CdpError::Io(_) | CdpError::ChannelSendError(_) | CdpError::NoResponse
    )
}

const RENDERER_CRASHED: &str = "the page's renderer crashed";

/// A DevTools error that says the page's target is gone: its renderer
/// crashed, or its session went with it.
pub(crate) fn is_crash_error(msg: &str) -> bool {
    msg.contains("Target crashed") || msg.contains("Session with given id not found")
}

pub fn lost(detail: impl std::fmt::Display) -> AtalantaError {
    AtalantaError::BrowserConnectionLost {
        detail: detail.to_string(),
    }
}

fn cdp_err(e: impl std::fmt::Display) -> AtalantaError {
    AtalantaError::Internal(anyhow::anyhow!("{e}"))
}

/// One left-button mouse event at `(x, y)`. `buttons` is the mask of
/// buttons held after it (1 while pressed), so `pointerdown`/`mousedown`
/// handlers see the left button down, as a real click reports it.
fn mouse(
    kind: DispatchMouseEventType,
    x: f64,
    y: f64,
    click_count: i64,
) -> DispatchMouseEventParams {
    let pressed = kind == DispatchMouseEventType::MousePressed;
    let mut b = DispatchMouseEventParams::builder()
        .r#type(kind.clone())
        .x(x)
        .y(y)
        .buttons(if pressed { 1 } else { 0 });
    if kind == DispatchMouseEventType::MouseMoved {
        b = b.button(MouseButton::None);
    } else {
        b = b.button(MouseButton::Left).click_count(click_count);
    }
    b.build().unwrap()
}

/// The pointer arrives at `(x, y)` before it presses, as a user's does:
/// without the move no `pointerover`/`mouseenter` fired and `:hover` never
/// applied, so hover-revealed controls (row actions, menus) were never
/// there to click (NB-5, Playwright `input.ts`).
async fn move_to(page: &Page, x: f64, y: f64) -> Result<()> {
    page.exec(mouse(DispatchMouseEventType::MouseMoved, x, y, 0))
        .await?;
    Ok(())
}

/// `Input.dispatchTouchEvent` as the protocol wants it: `touchEnd` must
/// send an empty `touchPoints`, which chromiumoxide's own params leave out
/// (and Chrome then refuses the call as invalid).
#[derive(Debug, serde::Serialize)]
struct TouchEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "touchPoints")]
    touch_points: Vec<Value>,
}

impl chromiumoxide::types::Method for TouchEvent {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "Input.dispatchTouchEvent".into()
    }
}

impl chromiumoxide::types::Command for TouchEvent {
    type Response = Value;
}

/// One finger down at `(x, y)` (`touchStart`), or lifted (`touchEnd`).
async fn touch(page: &Page, down: bool, x: f64, y: f64) -> Result<()> {
    let event = if down {
        TouchEvent {
            kind: "touchStart",
            touch_points: vec![serde_json::json!({ "x": x, "y": y })],
        }
    } else {
        TouchEvent {
            kind: "touchEnd",
            touch_points: Vec::new(),
        }
    };
    page.exec(event).await?;
    Ok(())
}

/// Steps of the finger's move in [`touch_drag`], and the time between them
/// (about a frame, so the page sees a stroke rather than a jump).
const TOUCH_DRAG_STEPS: i32 = 10;
const TOUCH_DRAG_STEP_MS: u64 = 16;
/// How long the finger rests at the end of a [`touch_drag`] before it lifts:
/// long enough that the browser sees no atalanta and starts no fling, so a
/// swipe scrolls by the stroke's length and no further, every time.
const TOUCH_DRAG_REST_MS: u64 = 120;

/// A finger stroke from `from` to `to` on a touch profile (W2):
/// `touchStart`, a `touchMove` per step, a rest, `touchEnd`. The browser
/// makes its own gesture of it, as on a phone: a scroller under the finger
/// follows it, and `touchmove`/`pointermove` handlers (carousels, pull to
/// refresh, sortable lists) see every step. The wheel a swipe sent before
/// fired no touch events at all, so touch widgets never moved.
pub async fn touch_drag(page: &Page, from: (f64, f64), to: (f64, f64)) -> Result<()> {
    touch_stroke(from, to, |e| async move { page.exec(e).await.map(drop) }).await
}

/// [`touch_drag`]'s events, each through `send`. When one fails (a
/// navigation mid-stroke, the CDP limit), the finger is lifted with a
/// `touchCancel` before the error returns: left down, the next tap's
/// `touchStart` was a second finger, or dropped (review-i2 m7).
async fn touch_stroke<F, Fut>(from: (f64, f64), to: (f64, f64), mut send: F) -> Result<()>
where
    F: FnMut(TouchEvent) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let point = |x: f64, y: f64| vec![serde_json::json!({ "x": x, "y": y, "id": 0 })];
    let mut events = vec![(
        0,
        TouchEvent {
            kind: "touchStart",
            touch_points: point(from.0, from.1),
        },
    )];
    for i in 1..=TOUCH_DRAG_STEPS {
        let t = f64::from(i) / f64::from(TOUCH_DRAG_STEPS);
        events.push((
            TOUCH_DRAG_STEP_MS,
            TouchEvent {
                kind: "touchMove",
                touch_points: point(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t),
            },
        ));
    }
    events.push((
        TOUCH_DRAG_REST_MS,
        TouchEvent {
            kind: "touchMove",
            touch_points: point(to.0, to.1),
        },
    ));
    events.push((
        0,
        TouchEvent {
            kind: "touchEnd",
            touch_points: Vec::new(),
        },
    ));
    for (wait, event) in events {
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        if let Err(e) = send(event).await {
            let cancel = TouchEvent {
                kind: "touchCancel",
                touch_points: Vec::new(),
            };
            if let Err(c) = send(cancel).await {
                tracing::debug!("atalanta-web: touchCancel after a failed stroke: {c}");
            }
            return Err(e);
        }
    }
    Ok(())
}

/// A finger tap at `(x, y)` on a touch profile: touchstart, touchend, and
/// the click the browser makes of them, as on a phone (Playwright's
/// `tap`). A mouse click there fired no touch events, so touch handlers
/// never ran (D-W3).
async fn tap(page: &Page, x: f64, y: f64, hold: Option<Duration>) -> Result<()> {
    touch(page, true, x, y).await?;
    if let Some(hold) = hold {
        tokio::time::sleep(hold).await;
    }
    touch(page, false, x, y).await
}

/// Dispatch a left-click (move, mousePressed, mouseReleased) at `(x, y)`;
/// a tap on a touch profile.
pub async fn click(page: &Page, x: f64, y: f64) -> Result<()> {
    if page.touch() {
        return tap(page, x, y, None).await;
    }
    move_to(page, x, y).await?;
    page.exec(mouse(DispatchMouseEventType::MousePressed, x, y, 1))
        .await?;
    page.exec(mouse(DispatchMouseEventType::MouseReleased, x, y, 1))
        .await?;
    Ok(())
}

/// Dispatch a double-click at `(x, y)`; two taps on a touch profile.
pub async fn double_click(page: &Page, x: f64, y: f64) -> Result<()> {
    if page.touch() {
        tap(page, x, y, None).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        return tap(page, x, y, None).await;
    }
    move_to(page, x, y).await?;
    for click_count in [1, 2] {
        page.exec(mouse(
            DispatchMouseEventType::MousePressed,
            x,
            y,
            click_count,
        ))
        .await?;
        page.exec(mouse(
            DispatchMouseEventType::MouseReleased,
            x,
            y,
            click_count,
        ))
        .await?;
    }
    Ok(())
}

/// Hold mousedown for `duration_ms` then release (long press); a held
/// finger on a touch profile.
pub async fn long_press(page: &Page, x: f64, y: f64, duration_ms: u64) -> Result<()> {
    if page.touch() {
        return tap(page, x, y, Some(Duration::from_millis(duration_ms))).await;
    }
    move_to(page, x, y).await?;
    page.exec(mouse(DispatchMouseEventType::MousePressed, x, y, 1))
        .await?;
    tokio::time::sleep(Duration::from_millis(duration_ms)).await;
    page.exec(mouse(DispatchMouseEventType::MouseReleased, x, y, 1))
        .await?;
    Ok(())
}

/// Steps of the move between press and release in [`drag`].
const DRAG_STEPS: i32 = 10;

/// A mouse drag from `from` to `to`: press, a few moves with the button
/// held (`buttons`, so `pointermove` reports it pressed), release. One jump
/// is too coarse for widgets that track the pointer (sliders, carousels).
pub async fn drag(page: &Page, from: (i32, i32), to: (i32, i32)) -> Result<()> {
    drag_held(page, from, to, 0, 0).await
}

/// [`drag`], held: a pause of `hold_ms` after the press, the moves spread
/// over `move_ms`, and a rest before the release, for lists that pick a row
/// up on a long press (row 3.1).
pub async fn drag_held(
    page: &Page,
    from: (i32, i32),
    to: (i32, i32),
    hold_ms: u64,
    move_ms: u64,
) -> Result<()> {
    let (fx, fy) = (f64::from(from.0), f64::from(from.1));
    let (tx, ty) = (f64::from(to.0), f64::from(to.1));
    let event = |kind: DispatchMouseEventType, x: f64, y: f64| {
        let mut b = DispatchMouseEventParams::builder()
            .r#type(kind.clone())
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .buttons(if kind == DispatchMouseEventType::MouseReleased {
                0
            } else {
                1
            });
        if kind != DispatchMouseEventType::MouseMoved {
            b = b.click_count(1);
        }
        b.build().unwrap()
    };

    page.exec(event(DispatchMouseEventType::MousePressed, fx, fy))
        .await?;
    if hold_ms > 0 {
        tokio::time::sleep(Duration::from_millis(hold_ms)).await;
    }
    let pause = move_ms / DRAG_STEPS as u64;
    for i in 1..=DRAG_STEPS {
        let t = f64::from(i) / f64::from(DRAG_STEPS);
        page.exec(event(
            DispatchMouseEventType::MouseMoved,
            fx + (tx - fx) * t,
            fy + (ty - fy) * t,
        ))
        .await?;
        if pause > 0 {
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    }
    if hold_ms > 0 || move_ms > 0 {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    page.exec(event(DispatchMouseEventType::MouseReleased, tx, ty))
        .await?;
    Ok(())
}

/// A mouse-wheel scroll of (`dx`, `dy`) CSS px at (`x`, `y`): scrolls the
/// innermost scrollable under the point, like a person's wheel or trackpad.
/// A press-move-release drag selects text and scrolls nothing.
pub async fn wheel(page: &Page, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
    page.exec(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(x)
            .y(y)
            .delta_x(dx)
            .delta_y(dy)
            .build()
            .unwrap(),
    )
    .await?;
    Ok(())
}

/// What Chrome needs to treat a synthetic key like a real one: the
/// `code`, the Windows virtual key code and, for printing keys, the text.
/// The values are Puppeteer's/Playwright's US keyboard layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDef {
    pub key: &'static str,
    pub code: &'static str,
    pub key_code: i64,
    pub text: Option<&'static str>,
    /// The editing command macOS Chrome binds to the key. On macOS key
    /// bindings come from the OS, not the renderer, so a synthetic
    /// Backspace without it deletes nothing (Playwright's macEditingCommands).
    pub mac_command: Option<&'static str>,
}

const fn kd(
    key: &'static str,
    key_code: i64,
    text: Option<&'static str>,
    mac_command: Option<&'static str>,
) -> KeyDef {
    KeyDef {
        key,
        code: key,
        key_code,
        text,
        mac_command,
    }
}

/// The keys `pressKey` can send (their `code` is their key name).
const KEYS: [KeyDef; 11] = [
    kd("Enter", 13, Some("\r"), Some("insertNewline")),
    kd("Backspace", 8, None, Some("deleteBackward")),
    kd("Tab", 9, Some("\t"), Some("insertTab")),
    kd("Escape", 27, None, Some("cancelOperation")),
    kd("Delete", 46, None, Some("deleteForward")),
    kd("Home", 36, None, Some("scrollToBeginningOfDocument")),
    kd("End", 35, None, Some("scrollToEndOfDocument")),
    kd("ArrowLeft", 37, None, Some("moveLeft")),
    kd("ArrowUp", 38, None, Some("moveUp")),
    kd("ArrowRight", 39, None, Some("moveRight")),
    kd("ArrowDown", 40, None, Some("moveDown")),
];

/// The definition for a key name, `None` for a key Atalanta doesn't know.
pub fn key_def(key: &str) -> Option<KeyDef> {
    KEYS.iter().find(|d| d.key == key).copied()
}

/// The keyDown/keyUp pair for one key press, the way Puppeteer's
/// `keyboard.press` builds it: `keyDown` with text for a printing key (so
/// Enter submits a form and types a newline), `rawKeyDown` otherwise, and the
/// code and virtual key code on both. A bare `key` (what this sent before)
/// reached page listeners as `code="" keyCode=0` and Chrome's default
/// actions ignored it (VEL-021).
pub fn key_events(def: &KeyDef, mac: bool) -> [DispatchKeyEventParams; 2] {
    let down_type = if def.text.is_some() {
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    // No `nativeVirtualKeyCode`: it is the OS's own key code (Return is 36
    // on macOS, not 13), and neither Playwright nor Puppeteer send one (NB-3).
    let mut down = DispatchKeyEventParams::builder()
        .r#type(down_type)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code);
    if let Some(t) = def.text {
        down = down.text(t).unmodified_text(t);
    }
    if mac {
        // Commands that insert text are left to the key's own `text`:
        // sent as well, Enter could insert twice (Playwright filters them
        // out the same way, `crInput.ts`).
        if let Some(c) = def.mac_command.filter(|c| !c.starts_with("insert")) {
            down = down.commands(vec![c.to_string()]);
        }
    }
    let up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code);
    [down.build().unwrap(), up.build().unwrap()]
}

/// Press one key (keyDown + keyUp) with its full definition.
pub async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    let def = key_def(key).ok_or_else(|| {
        AtalantaError::Config(format!("atalanta-web: no key definition for '{key}'"))
    })?;
    for ev in key_events(&def, cfg!(target_os = "macos")) {
        page.exec(ev).await?;
    }
    Ok(())
}

/// Longest text typed key by key; longer text is inserted in one go
/// (two DevTools round trips per key would make a pasted paragraph slow).
pub const PER_KEY_MAX: usize = 256;

/// The US-layout key for a printable ASCII character, as Puppeteer's
/// `keyboard.type` sends it: its `code` and Windows virtual key code.
/// `None` for anything else (text outside ASCII, a newline, a tab).
pub fn char_key(c: char) -> Option<(String, i64)> {
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCT: [(&str, &str, i64); 11] = [
        (";:", "Semicolon", 186),
        ("=+", "Equal", 187),
        (",<", "Comma", 188),
        ("-_", "Minus", 189),
        (".>", "Period", 190),
        ("/?", "Slash", 191),
        ("`~", "Backquote", 192),
        ("[{", "BracketLeft", 219),
        ("\\|", "Backslash", 220),
        ("]}", "BracketRight", 221),
        ("'\"", "Quote", 222),
    ];
    if c.is_ascii_alphabetic() {
        let up = c.to_ascii_uppercase();
        return Some((format!("Key{up}"), up as i64));
    }
    if c.is_ascii_digit() {
        return Some((format!("Digit{c}"), c as i64));
    }
    if c == ' ' {
        return Some(("Space".into(), 32));
    }
    if let Some(d) = SHIFTED_DIGITS.find(c) {
        return Some((format!("Digit{d}"), 48 + d as i64));
    }
    PUNCT
        .iter()
        .find(|(chars, _, _)| chars.contains(c))
        .map(|(_, code, vk)| (code.to_string(), *vk))
}

/// Type `text` into the focused element as a user would: printable ASCII
/// as a key press each (`keydown` with the character, `keypress`, the
/// input, `keyup`), so a page that filters on `keyup` (an autocomplete, a
/// search-as-you-type box) reacts; `Input.insertText` fired `input` only
/// (review 10 #7). Anything else (accented letters, CJK, emoji, a newline)
/// is inserted as text, as Puppeteer's `sendCharacter` does, so a newline
/// never presses Enter and submits a form. Text longer than
/// [`PER_KEY_MAX`], or with `ATALANTA_WEB_TYPING=insert`, is inserted in
/// one go, as before.
pub async fn type_text(page: &Page, text: &str) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
    let per_key = text.chars().count() <= PER_KEY_MAX
        && std::env::var("ATALANTA_WEB_TYPING").map_or(true, |v| v != "insert");
    if !per_key {
        page.exec(InsertTextParams::new(text)).await?;
        return Ok(());
    }
    let mut run = String::new();
    for c in text.chars() {
        let Some((code, vk)) = char_key(c) else {
            run.push(c);
            continue;
        };
        if !run.is_empty() {
            page.exec(InsertTextParams::new(std::mem::take(&mut run)))
                .await?;
        }
        let key = c.to_string();
        let down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .key(key.clone())
            .code(code.clone())
            .windows_virtual_key_code(vk)
            .text(key.clone())
            .unmodified_text(key.clone())
            .build()
            .map_err(cdp_err)?;
        let up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key(key)
            .code(code)
            .windows_virtual_key_code(vk)
            .build()
            .map_err(cdp_err)?;
        page.exec(down).await?;
        page.exec(up).await?;
    }
    if !run.is_empty() {
        page.exec(InsertTextParams::new(run)).await?;
    }
    Ok(())
}

/// Selects everything in the focused field, then deletes it.
///
/// Uses CDP editing `commands` rather than a Ctrl+A chord: on macOS Chrome
/// ignores Ctrl+A (select-all there is Cmd+A), so the chord left the old text
/// in place (puppeteer#1313). Editing commands behave the same on every OS
/// and fire the normal `input` events.
pub async fn select_all_and_delete(page: &Page) -> Result<()> {
    for (key, command) in [("a", "selectAll"), ("Backspace", "deleteBackward")] {
        page.exec(
            DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::RawKeyDown)
                .key(key)
                .commands(vec![command.to_string()])
                .build()
                .unwrap(),
        )
        .await?;
        page.exec(
            DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key(key)
                .build()
                .unwrap(),
        )
        .await?;
    }
    Ok(())
}

/// Moves the caret to the end of the focused field with the editing
/// command, for fields with no selection API (`type=email`, `number`),
/// so typed text is appended (NB-7).
pub async fn move_to_end(page: &Page) -> Result<()> {
    page.exec(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::RawKeyDown)
            .key("End")
            .commands(vec!["moveToEndOfDocument".to_string()])
            .build()
            .unwrap(),
    )
    .await?;
    page.exec(
        DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .key("End")
            .build()
            .unwrap(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// review-i2 m7: a stroke whose `touchMove` fails lifts the finger
    /// (`touchCancel`) before it returns the error; one that goes through
    /// ends with `touchEnd` and no cancel.
    #[tokio::test]
    async fn a_failed_touch_stroke_lifts_the_finger() {
        let sent = std::sync::Mutex::new(Vec::<&'static str>::new());
        let r = touch_stroke((10.0, 10.0), (10.0, 100.0), |e| {
            let n = {
                let mut s = sent.lock().unwrap();
                s.push(e.kind);
                s.len()
            };
            async move {
                if n == 4 {
                    Err(lost("navigated mid-stroke"))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(r.is_err());
        let kinds = sent.lock().unwrap().clone();
        assert_eq!(
            kinds,
            [
                "touchStart",
                "touchMove",
                "touchMove",
                "touchMove",
                "touchCancel"
            ]
        );
        let ok = std::sync::Mutex::new(Vec::<&'static str>::new());
        touch_stroke((0.0, 0.0), (0.0, 1.0), |e| {
            ok.lock().unwrap().push(e.kind);
            async { Ok(()) }
        })
        .await
        .unwrap();
        let kinds = ok.lock().unwrap().clone();
        assert_eq!(kinds.last(), Some(&"touchEnd"));
        assert!(!kinds.contains(&"touchCancel"));
    }
}
