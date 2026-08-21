//! Cross-origin iframes (W4, D-36 c′): a minimal flat-session CDP layer.
//!
//! With site isolation a cross-origin `<iframe>` (a Stripe card field, an
//! OAuth button, an embedded checkout) runs in its own renderer process as
//! its own DevTools target: the page's script can't see into it, and its
//! network is reported on the frame's own session, not the page's.
//!
//! chromiumoxide auto-attaches those targets (`Target.setAutoAttach` with
//! `flatten: true`), but its handler keeps the child sessions to itself: an
//! event from one reaches the listeners without its `sessionId`, and a
//! command can only go to the page's own session. So the frames get a
//! second DevTools connection of their own, kept as small as it can be:
//!
//! - one WebSocket to the browser endpoint chromiumoxide uses, JSON-RPC
//!   with `sessionId` routing and nothing else (no domain types: commands
//!   are method names and JSON);
//! - attached (flat) to the page's target, with `Target.setAutoAttach`
//!   `{autoAttach, flatten, waitForDebuggerOnStart}` there and in every
//!   frame (nested frames), so each frame is held at its start until its
//!   `Network` domain is on: none of its requests is missed;
//! - per frame: Velocity's library (`injected.js`) in an isolated world of
//!   the frame (`Page.createIsolatedWorld`), cached per document;
//! - the frame's requests feed the page's network-idle wait.
//!
//! Elements found in a frame carry the frame in their id (`@<key>/<id>`),
//! so every later call on them (actionability, state, text, focus) runs
//! in that frame, and their boxes are in the top page's coordinates: the
//! frame's offset (`DOM.getFrameOwner` + `DOM.getBoxModel` in its parent)
//! is added on the way out. Input still goes to the page: Chrome routes a
//! mouse or touch event at a point to the frame under it.
//!
//! Not covered: the element tree (`get_hierarchy`) and layout relations
//! stay the page's own; mocks (`Fetch`) don't apply inside such frames.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;
use velocity_common::{Result, VelocityError};

use crate::cdp::CDP_TIMEOUT;

/// The marker that starts a frame element's id: `@<frame key>/<id>`.
const FRAME_ID_MARK: char = '@';

/// The isolated world the library runs in, in each frame.
const WORLD: &str = "__velocity_frame_world__";

/// Hooks a frame's requests into the page's network-idle wait.
pub trait FrameNetwork: Send + Sync {
    fn started(&self, request_id: &str, url: &str, resource_type: Option<&str>);
    fn finished(&self, request_id: &str);
    /// The frames connection is gone: no frame request's end will be seen
    /// from here on (review-i2 m4).
    fn closed(&self) {}
    /// Frame `frame_id` (its target id) detached: anything of its own
    /// still counted elsewhere ends with it (review-fi2 F7).
    fn frame_gone(&self, _frame_id: &str) {}
}

/// The error text of a command Chrome didn't answer in time.
const NO_ANSWER: &str = "no answer in";

/// Whether `e` is a command that timed out (a hung frame), not one that
/// failed.
pub fn is_timeout(e: &VelocityError) -> bool {
    e.to_string().contains(NO_ANSWER)
}

type Reply = std::result::Result<Value, String>;

/// Whether the browser at `ws_url` (its DevTools endpoint) answers
/// `Browser.getVersion` within `limit`, over a connection of its own:
/// nothing another device holds (the browser's lock, chromiumoxide's
/// handler and its queue) is waited on, so only Chrome itself is timed
/// (review-fi2 F2).
pub async fn browser_answers(ws_url: &str, limit: Duration) -> bool {
    let ping = async {
        let (mut ws, _) = async_tungstenite::tokio::connect_async(ws_url).await.ok()?;
        let msg = json!({ "id": 1, "method": "Browser.getVersion" }).to_string();
        ws.send(Message::text(msg)).await.ok()?;
        while let Some(Ok(m)) = ws.next().await {
            let Ok(text) = m.to_text() else { continue };
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                if v["id"] == 1 {
                    let _ = ws.close(None).await;
                    return Some(v.get("result").is_some());
                }
            }
        }
        None
    };
    matches!(tokio::time::timeout(limit, ping).await, Ok(Some(true)))
}

/// One cross-origin frame.
#[derive(Clone, Debug)]
struct Frame {
    key: u64,
    /// The flat session to the frame's target.
    session: String,
    /// The frame's id (its target's id).
    frame_id: String,
    /// The session of the document that holds its `<iframe>`.
    parent: String,
    /// Its URL when it attached (for messages).
    url: String,
    /// The library's isolated world in the current document.
    context: Option<i64>,
    /// The library is installed in that world.
    ready: bool,
    /// The library's first install in this document runs in the
    /// background ([`Frames::start_install`]): a frame still loading is
    /// not timed against a lookup's share (review-fi2 F1).
    installing: bool,
    /// Lookups in a row it didn't answer in its share.
    overruns: u32,
    /// Hung (it overran [`OVERRUNS_TO_HUNG`] lookups in a row, or a
    /// whole CDP timeout): left out of lookups until then, and pinged
    /// before it is asked again (review-fi2 F1).
    hung_until: Option<Instant>,
}

/// Lookups in a row a frame may overrun its share before it counts as
/// hung: one slow call (a long task while loading) is not a hang.
pub const OVERRUNS_TO_HUNG: u32 = 3;
/// How long a hung frame is left out of lookups before it is pinged.
pub const HUNG_FOR: Duration = Duration::from_secs(3);
/// What a hung frame's ping (`Runtime.evaluate("1")`) may take.
pub const PING_LIMIT: Duration = Duration::from_millis(300);

/// Where a frame stands for a lookup ([`Frames::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    /// The library is in its document: ask it.
    Ready,
    /// No library yet and no install running: start one.
    Bare,
    /// The first install is still running (a frame still loading).
    Installing,
    /// Hung, and not long enough ago to try it again.
    Hung,
    /// Hung, long enough ago: ping it before asking.
    Recheck,
}

/// The page's cross-origin frames and the connection to them.
pub struct Frames {
    out: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    next_key: AtomicU64,
    /// The flat session to the page's own target.
    page_session: Mutex<String>,
    frames: Mutex<Vec<Frame>>,
    alive: AtomicBool,
    network: Mutex<Option<Arc<dyn FrameNetwork>>>,
    /// Requests in flight in each frame: request id -> the frame's
    /// session. Ended at the frame's detach (review-i2 m5).
    requests: Mutex<HashMap<String, String>>,
}

impl Frames {
    /// Connect to `ws_url` (the browser's DevTools endpoint) and attach to
    /// the page target `target_id`; from here on its cross-origin frames
    /// are tracked.
    pub async fn connect(ws_url: &str, target_id: &str) -> Result<Arc<Frames>> {
        let (ws, _) =
            tokio::time::timeout(CDP_TIMEOUT, async_tungstenite::tokio::connect_async(ws_url))
                .await
                .map_err(|_| err("no answer from the DevTools endpoint"))?
                .map_err(err)?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let frames = Arc::new(Frames::with_sender(out));
        tokio::spawn(async move {
            while let Some(m) = outgoing.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            let _ = sink.close(None).await;
        });
        // The reader holds only a weak handle: the connection closes when
        // the page's `Tab` (the last strong one) goes.
        let weak = Arc::downgrade(&frames);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Some(frames) = weak.upgrade() else { break };
                let Ok(text) = msg.to_text() else { continue };
                if let Ok(v) = serde_json::from_str::<Value>(text) {
                    frames.on_message(v);
                }
            }
            if let Some(frames) = weak.upgrade() {
                frames.shut();
            }
        });

        let attached = frames
            .send(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| err("Target.attachToTarget gave no session"))?
            .to_string();
        *lock(&frames.page_session) = session.clone();
        frames
            .send(Some(&session), "Target.setAutoAttach", auto_attach())
            .await?;
        Ok(frames)
    }

    fn with_sender(out: mpsc::UnboundedSender<Message>) -> Frames {
        Frames {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_key: AtomicU64::new(1),
            page_session: Mutex::new(String::new()),
            frames: Mutex::new(Vec::new()),
            alive: AtomicBool::new(true),
            network: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
        }
    }

    /// Where the frame's requests go (the page's network-idle wait).
    pub fn set_network(&self, network: Arc<dyn FrameNetwork>) {
        *lock(&self.network) = Some(network);
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// How many cross-origin frames the page has now.
    pub fn count(&self) -> usize {
        lock(&self.frames).len()
    }

    /// The frames' keys, in the order they attached.
    pub fn keys(&self) -> Vec<u64> {
        lock(&self.frames).iter().map(|f| f.key).collect()
    }

    /// Where frame `key` stands for a lookup; `None` once it is gone.
    pub fn state(&self, key: u64) -> Option<FrameState> {
        let frames = lock(&self.frames);
        let f = frames.iter().find(|f| f.key == key)?;
        Some(match f.hung_until {
            Some(t) if Instant::now() < t => FrameState::Hung,
            Some(_) => FrameState::Recheck,
            None if f.ready && f.context.is_some() => FrameState::Ready,
            None if f.installing => FrameState::Installing,
            None => FrameState::Bare,
        })
    }

    /// How many frames a lookup will ask now (the ones not hung).
    pub fn askable(&self) -> usize {
        let now = Instant::now();
        lock(&self.frames)
            .iter()
            .filter(|f| f.hung_until.is_none_or(|t| now >= t))
            .count()
    }

    /// `frame 2 (https://js.stripe.com/…)`, for messages.
    pub fn describe(&self, key: u64) -> String {
        match lock(&self.frames).iter().find(|f| f.key == key) {
            Some(f) if !f.url.is_empty() => format!("frame {key} ({})", f.url),
            _ => format!("frame {key}"),
        }
    }

    /// Frame `key` answered a lookup: whatever it overran before was
    /// slowness, not a hang.
    pub fn answered(&self, key: u64) {
        if let Some(f) = lock(&self.frames).iter_mut().find(|f| f.key == key) {
            f.overruns = 0;
            f.hung_until = None;
        }
    }

    /// Frame `key` didn't answer a lookup in its share. After
    /// [`OVERRUNS_TO_HUNG`] in a row it counts as hung: left out of
    /// lookups for [`HUNG_FOR`], then pinged (one warning).
    pub fn overran(&self, key: u64, what: &str) {
        let mut frames = lock(&self.frames);
        let Some(f) = frames.iter_mut().find(|f| f.key == key) else {
            return;
        };
        f.overruns += 1;
        if f.overruns >= OVERRUNS_TO_HUNG && f.hung_until.is_none() {
            f.hung_until = Some(Instant::now() + HUNG_FOR);
            tracing::warn!(
                "velocity-web: cross-origin frame {key} did not answer {} lookups in a row \
                 ({what}); left out of lookups while it doesn't answer a ping",
                f.overruns
            );
        }
    }

    /// Frame `key` gave a hang's evidence (no answer in a whole CDP
    /// timeout, or to a ping): left out of lookups for [`HUNG_FOR`] more.
    pub fn mark_hung(&self, key: u64, what: &str) {
        let mut frames = lock(&self.frames);
        if let Some(f) = frames.iter_mut().find(|f| f.key == key) {
            if f.hung_until.is_none() {
                tracing::warn!(
                    "velocity-web: cross-origin frame {key} did not answer ({what}); \
                     left out of lookups while it doesn't answer a ping"
                );
            }
            f.overruns = f.overruns.max(OVERRUNS_TO_HUNG);
            f.hung_until = Some(Instant::now() + HUNG_FOR);
        }
    }

    /// Whether frame `key`'s renderer answers a trivial evaluation within
    /// `limit` (a hung frame's re-check).
    pub async fn ping(&self, key: u64, limit: Duration) -> bool {
        let Some(frame) = self.frame(key) else {
            return false;
        };
        let ping = self.send(
            Some(&frame.session),
            "Runtime.evaluate",
            json!({ "expression": "1", "returnByValue": true }),
        );
        matches!(tokio::time::timeout(limit, ping).await, Ok(Ok(_)))
    }

    /// Install the library in frame `key`'s document in the background,
    /// unless that is done or running. A frame still loading takes its
    /// time without a lookup timing it; one that doesn't answer the
    /// install in a whole CDP timeout is hung.
    pub fn start_install(self: &Arc<Self>, key: u64) {
        {
            let mut frames = lock(&self.frames);
            let Some(f) = frames.iter_mut().find(|f| f.key == key) else {
                return;
            };
            if f.installing || (f.ready && f.context.is_some()) {
                return;
            }
            f.installing = true;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let done = me.install(key).await;
            let mut frames = lock(&me.frames);
            if let Some(f) = frames.iter_mut().find(|f| f.key == key) {
                f.installing = false;
                match &done {
                    Ok(ctx) if f.context == Some(*ctx) => f.ready = true,
                    _ => {}
                }
            }
            drop(frames);
            match done {
                Err(e) if is_timeout(&e) => me.mark_hung(key, &e.to_string()),
                Err(e) => debug!("velocity-web: frame {key}: installing the library: {e}"),
                Ok(_) => {}
            }
        });
    }

    /// Waits up to `limit` for frame `key`'s background install; whether
    /// the library is there now.
    pub async fn wait_ready(&self, key: u64, limit: Duration) -> bool {
        let end = tokio::time::Instant::now() + limit;
        loop {
            match self.state(key) {
                Some(FrameState::Ready) => return true,
                Some(FrameState::Installing) if tokio::time::Instant::now() < end => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                _ => return false,
            }
        }
    }

    /// The library in frame `key`'s document: its world, created and
    /// the library installed if need be.
    async fn install(&self, key: u64) -> Result<i64> {
        let frame = self
            .frame(key)
            .ok_or_else(|| err("the frame is gone (it navigated or was removed)"))?;
        let ctx = self.context(&frame).await?;
        // Whether the library is there, and the document's URL (the
        // target's is still empty when it attaches) for messages.
        let v = self
            .call_fn(
                &frame,
                ctx,
                "function(){return [!!globalThis.__velocity, String(location.href)];}",
                &[],
            )
            .await?;
        if let Some(url) = v[1].as_str() {
            if let Some(f) = lock(&self.frames).iter_mut().find(|f| f.key == key) {
                f.url = url.to_string();
            }
        }
        if v[0] != Value::Bool(true) {
            let install = format!("function(){{{}\nreturn true;}}", crate::cdp::LIBRARY);
            if let Err(e) = self.call_fn(&frame, ctx, &install, &[]).await {
                if !is_timeout(&e) {
                    self.forget_context(key);
                }
                return Err(e);
            }
        }
        Ok(ctx)
    }

    fn set_ready(&self, key: u64, ctx: i64) {
        if let Some(f) = lock(&self.frames)
            .iter_mut()
            .find(|f| f.key == key && f.context == Some(ctx))
        {
            f.ready = true;
        }
    }

    /// `(frame key, id in the frame)` of a frame element's id.
    pub fn split(id: &str) -> Option<(u64, &str)> {
        let rest = id.strip_prefix(FRAME_ID_MARK)?;
        let (key, inner) = rest.split_once('/')?;
        Some((key.parse().ok()?, inner))
    }

    fn shut(&self) {
        self.alive.store(false, Ordering::SeqCst);
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.send(Err("the frames connection closed".into()));
        }
        lock(&self.frames).clear();
        lock(&self.requests).clear();
        // The page's network wait counts cross-origin frame documents only
        // while their ends can be seen here.
        if let Some(net) = lock(&self.network).clone() {
            net.closed();
        }
    }

    /// One command on `session` (the browser's own with `None`).
    async fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        if !self.is_alive() {
            return Err(err("the frames connection is closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if self.out.send(Message::text(msg.to_string())).is_err() {
            lock(&self.pending).remove(&id);
            return Err(err("the frames connection is closed"));
        }
        match tokio::time::timeout(CDP_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(err(format!("{method}: {e}"))),
            Ok(Err(_)) => Err(err("the frames connection closed")),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(err(format!("{method}: {NO_ANSWER} {CDP_TIMEOUT:?}")))
            }
        }
    }

    /// Fire and forget (the reader task can't wait on its own replies).
    fn post(self: &Arc<Self>, session: String, method: &'static str, params: Value) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.send(Some(&session), method, params).await {
                debug!("velocity-web: frames: {e}");
            }
        });
    }

    fn on_message(self: &Arc<Self>, v: Value) {
        if let Some(id) = v["id"].as_u64() {
            if let Some(tx) = lock(&self.pending).remove(&id) {
                let reply = match v.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(v["result"].clone()),
                };
                let _ = tx.send(reply);
            }
            return;
        }
        let method = v["method"].as_str().unwrap_or_default();
        let session = v["sessionId"].as_str().unwrap_or_default();
        let p = &v["params"];
        match method {
            // Our own attach to the page comes without a session: it is
            // the page, not a child of it.
            "Target.attachedToTarget" if !session.is_empty() => self.on_attached(session, p),
            "Target.detachedFromTarget" => {
                let gone = p["sessionId"].as_str().unwrap_or_default();
                if gone == *lock(&self.page_session) {
                    // The page itself went (closed, its context disposed).
                    self.shut();
                    return;
                }
                let (gone_sessions, gone_frames): (Vec<String>, Vec<String>) = {
                    let mut frames = lock(&self.frames);
                    let gone_frames = frames
                        .iter()
                        .filter(|f| f.session == gone || f.parent == gone)
                        .map(|f| (f.session.clone(), f.frame_id.clone()))
                        .unzip();
                    frames.retain(|f| f.session != gone && f.parent != gone);
                    gone_frames
                };
                // Their requests in flight end with them: no end of theirs
                // will ever be reported (review-i2 m5).
                let ended: Vec<String> = {
                    let mut requests = lock(&self.requests);
                    let ended = requests
                        .iter()
                        .filter(|(_, s)| gone_sessions.contains(s))
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    for id in &ended {
                        requests.remove(id);
                    }
                    ended
                };
                if let Some(net) = lock(&self.network).clone() {
                    for id in &ended {
                        net.finished(id);
                    }
                    // Their documents, reported started on the page's
                    // session (review-fi2 F7).
                    for frame in &gone_frames {
                        net.frame_gone(frame);
                    }
                }
            }
            "Network.requestWillBeSent" if self.is_frame_session(session) => {
                let id = p["requestId"].as_str().unwrap_or_default();
                lock(&self.requests).insert(id.to_string(), session.to_string());
                if let Some(net) = lock(&self.network).clone() {
                    net.started(
                        id,
                        p["request"]["url"].as_str().unwrap_or_default(),
                        p["type"].as_str(),
                    );
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed"
                if self.is_frame_session(session) =>
            {
                let id = p["requestId"].as_str().unwrap_or_default();
                lock(&self.requests).remove(id);
                if let Some(net) = lock(&self.network).clone() {
                    net.finished(id);
                }
            }
            "Runtime.executionContextsCleared" => {
                // A new document: a new world, and a frame that hung gets
                // another chance.
                for f in lock(&self.frames).iter_mut() {
                    if f.session == session {
                        f.context = None;
                        f.ready = false;
                        f.overruns = 0;
                        f.hung_until = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn is_frame_session(&self, session: &str) -> bool {
        lock(&self.frames).iter().any(|f| f.session == session)
    }

    /// A target attached under the page or one of its frames: a frame is
    /// tracked; anything else (a worker) is let go. Either way it is
    /// released (it waits for the debugger until then), after `Network`
    /// is on in a frame, so its first request is seen.
    fn on_attached(self: &Arc<Self>, parent: &str, p: &Value) {
        let Some(session) = p["sessionId"].as_str().map(str::to_string) else {
            return;
        };
        let info = &p["targetInfo"];
        let waiting = p["waitingForDebugger"].as_bool().unwrap_or(false);
        if info["type"].as_str() != Some("iframe") {
            let me = self.clone();
            tokio::spawn(async move {
                if waiting {
                    let _ = me
                        .send(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}))
                        .await;
                }
                let _ = me
                    .send(
                        None,
                        "Target.detachFromTarget",
                        json!({ "sessionId": session }),
                    )
                    .await;
            });
            return;
        }
        let frame = Frame {
            key: self.next_key.fetch_add(1, Ordering::SeqCst),
            session: session.clone(),
            frame_id: info["targetId"].as_str().unwrap_or_default().to_string(),
            parent: parent.to_string(),
            url: info["url"].as_str().unwrap_or_default().to_string(),
            context: None,
            ready: false,
            installing: false,
            overruns: 0,
            hung_until: None,
        };
        debug!(
            "velocity-web: cross-origin frame {} attached: {}",
            frame.key,
            info["url"].as_str().unwrap_or_default()
        );
        lock(&self.frames).push(frame);
        let me = self.clone();
        tokio::spawn(async move {
            let s = Some(session.as_str());
            for (method, params) in [
                ("Network.enable", json!({})),
                ("Runtime.enable", json!({})),
                ("Target.setAutoAttach", auto_attach()),
            ] {
                if let Err(e) = me.send(s, method, params).await {
                    debug!("velocity-web: frame setup: {e}");
                }
            }
            if waiting {
                me.post(session, "Runtime.runIfWaitingForDebugger", json!({}));
            }
        });
    }

    fn frame(&self, key: u64) -> Option<Frame> {
        lock(&self.frames).iter().find(|f| f.key == key).cloned()
    }

    /// The library's isolated world in `frame`'s current document.
    async fn context(&self, frame: &Frame) -> Result<i64> {
        if let Some(c) = frame.context {
            return Ok(c);
        }
        let v = self
            .send(
                Some(&frame.session),
                "Page.createIsolatedWorld",
                json!({ "frameId": frame.frame_id, "worldName": WORLD, "grantUniveralAccess": true }),
            )
            .await?;
        let ctx = v["executionContextId"]
            .as_i64()
            .ok_or_else(|| err("Page.createIsolatedWorld gave no context"))?;
        for f in lock(&self.frames).iter_mut() {
            if f.key == frame.key {
                f.context = Some(ctx);
                f.ready = false;
            }
        }
        Ok(ctx)
    }

    fn forget_context(&self, key: u64) {
        for f in lock(&self.frames).iter_mut() {
            if f.key == key {
                f.context = None;
                f.ready = false;
            }
        }
    }

    async fn call_fn(
        &self,
        frame: &Frame,
        ctx: i64,
        function: &str,
        args: &[Value],
    ) -> Result<Value> {
        let v = self
            .send(
                Some(&frame.session),
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": function,
                    "executionContextId": ctx,
                    "arguments": args.iter().map(|a| json!({ "value": a })).collect::<Vec<_>>(),
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(ex) = v.get("exceptionDetails") {
            let what = ex["exception"]["description"]
                .as_str()
                .or_else(|| ex["text"].as_str())
                .unwrap_or("exception");
            return Err(err(format!("{LIBRARY_ERROR}: {what}")));
        }
        Ok(v["result"]["value"].clone())
    }

    /// Run `op` of the library in frame `key` with `args`, installing the
    /// library in its document first if need be. The reply is as the
    /// frame's document sees it: see [`Frames::to_page`].
    pub async fn call(&self, key: u64, op: &str, args: Vec<Value>) -> Result<Value> {
        let call_args = [Value::from(op), Value::Array(args)];
        let mut last = None;
        for attempt in 0..4 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(25 * attempt)).await;
            }
            let frame = self
                .frame(key)
                .ok_or_else(|| err("the frame is gone (it navigated or was removed)"))?;
            // A frame that doesn't answer (stuck in a long task) isn't
            // asked again: each try would wait the whole CDP timeout
            // (review-i2 M2). Nor is a library error retried: the page
            // fails at once on one too.
            let ctx = match self.context(&frame).await {
                Ok(c) => c,
                Err(e) if is_timeout(&e) => return Err(e),
                Err(e) => {
                    last = Some(e);
                    continue;
                }
            };
            match self
                .call_fn(&frame, ctx, crate::cdp::DISPATCH, &call_args)
                .await
            {
                Ok(v) if v.get("velMissing").is_some() => {
                    let install = format!("function(){{{}\nreturn true;}}", crate::cdp::LIBRARY);
                    match self.call_fn(&frame, ctx, &install, &[]).await {
                        Err(e) if is_timeout(&e) => return Err(e),
                        Err(e) => {
                            self.forget_context(key);
                            last = Some(e);
                        }
                        Ok(_) => {}
                    }
                }
                Ok(v) => {
                    self.set_ready(key, ctx);
                    return Ok(v);
                }
                Err(e) if is_timeout(&e) || is_library_error(&e) => return Err(e),
                Err(e) => {
                    // The document went: a new world in the new one.
                    self.forget_context(key);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| err("the frame's script context never became ready")))
    }

    /// Frame `key`'s `<iframe>`s from its own up to the top page's: for
    /// each, the session of the document holding it, its node there and
    /// where its content box starts in that document's viewport (CSS px).
    async fn owners(&self, key: u64) -> Result<Vec<(String, Value, (f64, f64))>> {
        let mut out = Vec::new();
        let mut frame = self.frame(key).ok_or_else(|| err("the frame is gone"))?;
        for _ in 0..16 {
            let owner = self
                .send(
                    Some(&frame.parent),
                    "DOM.getFrameOwner",
                    json!({ "frameId": frame.frame_id }),
                )
                .await?;
            let node = owner["backendNodeId"].clone();
            let model = self
                .send(
                    Some(&frame.parent),
                    "DOM.getBoxModel",
                    json!({ "backendNodeId": node }),
                )
                .await?;
            let q = &model["model"]["content"];
            let at = (q[0].as_f64().unwrap_or(0.0), q[1].as_f64().unwrap_or(0.0));
            out.push((frame.parent.clone(), node, at));
            let parent = frame.parent.clone();
            match lock(&self.frames)
                .iter()
                .find(|f| f.session == parent)
                .cloned()
            {
                Some(p) => frame = p,
                None => break,
            }
        }
        Ok(out)
    }

    /// Where frame `key`'s viewport starts in the top page's viewport (CSS
    /// px): its `<iframe>`'s content box, plus its parent frame's offset.
    pub async fn offset(&self, key: u64) -> Result<(f64, f64)> {
        Ok(self
            .owners(key)
            .await?
            .iter()
            .fold((0.0, 0.0), |t, (_, _, q)| (t.0 + q.0, t.1 + q.1)))
    }

    /// What a pointer at (`x`, `y`, top-page px) hits on the way into frame
    /// `key`: `None` when it reaches the frame (it lands on the frame's
    /// `<iframe>` in every document on the way), else the element that
    /// takes it instead (`<div>`), such as a cookie banner over the frame
    /// (review-i2 M1). The frame's own hit test can't see the page.
    pub async fn intruder(&self, key: u64, x: f64, y: f64) -> Result<Option<String>> {
        let owners = self.owners(key).await?;
        // From the top page down; `o`: where the current document's
        // viewport starts in the top page's.
        let mut o = (0.0, 0.0);
        for (session, node, q) in owners.iter().rev() {
            let (px, py) = (x - o.0, y - o.1);
            o = (o.0 + q.0, o.1 + q.1);
            let hit = self
                .send(
                    Some(session),
                    "DOM.getNodeForLocation",
                    json!({
                        "x": px.round() as i64,
                        "y": py.round() as i64,
                        "includeUserAgentShadowDOM": false,
                        "ignorePointerEventsNone": true,
                    }),
                )
                .await?;
            let got = &hit["backendNodeId"];
            if got != node {
                let tag = self
                    .send(
                        Some(session),
                        "DOM.describeNode",
                        json!({ "backendNodeId": got }),
                    )
                    .await
                    .ok()
                    .and_then(|d| d["node"]["localName"].as_str().map(str::to_string))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "something else".into());
                return Ok(Some(format!("<{tag}>")));
            }
        }
        Ok(None)
    }

    /// Scrolls frame `key`'s `<iframe>` (and its parents') into the view of
    /// the document holding it, so a point in it is on screen.
    pub async fn reveal_frame(&self, key: u64) -> Result<()> {
        let mut frame = self.frame(key).ok_or_else(|| err("the frame is gone"))?;
        for _ in 0..16 {
            let owner = self
                .send(
                    Some(&frame.parent),
                    "DOM.getFrameOwner",
                    json!({ "frameId": frame.frame_id }),
                )
                .await?;
            let _ = self
                .send(
                    Some(&frame.parent),
                    "DOM.scrollIntoViewIfNeeded",
                    json!({ "backendNodeId": owner["backendNodeId"] }),
                )
                .await;
            let parent = frame.parent.clone();
            match lock(&self.frames)
                .iter()
                .find(|f| f.session == parent)
                .cloned()
            {
                Some(p) => frame = p,
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// `op`'s reply from frame `key` in the top page's terms: element ids
    /// carry the frame, boxes and points move by its offset.
    pub fn to_page(op: &str, key: u64, (dx, dy): (f64, f64), mut v: Value) -> Value {
        fn shift(n: &mut Value, key: u64, dx: f64, dy: f64) {
            if let Some(id) = n["velId"].as_str() {
                n["velId"] = format!("{FRAME_ID_MARK}{key}/{id}").into();
            }
            for (k, d) in [("x", dx), ("y", dy)] {
                if let Some(c) = n[k].as_f64() {
                    n[k] = ((c + d).round() as i64).into();
                }
            }
            if let Some(vb) = n.get_mut("vb").and_then(Value::as_array_mut) {
                for (i, d) in [(0, dx), (1, dy)] {
                    if let Some(c) = vb.get(i).and_then(Value::as_f64) {
                        vb[i] = ((c + d).round() as i64).into();
                    }
                }
            }
        }
        match op {
            "find" | "reveal" if v.is_object() => shift(&mut v, key, dx, dy),
            "findAll" => {
                for n in v.as_array_mut().into_iter().flatten() {
                    shift(n, key, dx, dy);
                }
            }
            "act" => {
                for (k, d) in [("x", dx), ("y", dy)] {
                    if let Some(c) = v[k].as_f64() {
                        v[k] = ((c + d).round() as i64).into();
                    }
                }
            }
            _ => {}
        }
        v
    }
}

/// Auto-attach to child targets, flat, each held at its start until
/// released (`Runtime.runIfWaitingForDebugger`).
fn auto_attach() -> Value {
    json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a script error of the library in a frame says.
const LIBRARY_ERROR: &str = "script error in a cross-origin frame";

fn is_library_error(e: &VelocityError) -> bool {
    e.to_string().contains(LIBRARY_ERROR)
}

fn err(e: impl std::fmt::Display) -> VelocityError {
    VelocityError::Internal(anyhow::anyhow!("cross-origin frames: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ids_split_and_replies_move_to_the_page() {
        assert_eq!(Frames::split("@3/1a.xyz"), Some((3, "1a.xyz")));
        assert_eq!(Frames::split("1a.xyz"), None);
        assert_eq!(Frames::split("@x/1"), None);
        let v = Frames::to_page(
            "find",
            3,
            (50.0, 100.0),
            json!({"velId": "1.a", "x": 20, "y": 20, "w": 40, "h": 20, "vb": [20, 20, 10, 20]}),
        );
        assert_eq!(v["velId"], "@3/1.a");
        assert_eq!((v["x"].as_i64(), v["y"].as_i64()), (Some(70), Some(120)));
        assert_eq!(v["vb"], json!([70, 120, 10, 20]));
        assert_eq!(v["w"], 40);
        let a = Frames::to_page("act", 3, (50.0, 100.0), json!({"ok": true, "x": 1, "y": 2}));
        assert_eq!((a["x"].as_i64(), a["y"].as_i64()), (Some(51), Some(102)));
        let s = Frames::to_page("state", 3, (50.0, 100.0), json!({"visible": true}));
        assert_eq!(s, json!({"visible": true}));
    }

    #[derive(Default)]
    struct Rec {
        finished: Mutex<Vec<String>>,
        gone: Mutex<Vec<String>>,
        closed: AtomicBool,
    }

    impl FrameNetwork for Rec {
        fn started(&self, _: &str, _: &str, _: Option<&str>) {}
        fn finished(&self, id: &str) {
            lock(&self.finished).push(id.to_string());
        }
        fn closed(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
        fn frame_gone(&self, frame_id: &str) {
            lock(&self.gone).push(frame_id.to_string());
        }
    }

    fn attached(f: &Arc<Frames>, session: &str) {
        f.on_message(json!({
            "method": "Target.attachedToTarget", "sessionId": "PAGE",
            "params": {"sessionId": session, "waitingForDebugger": false,
                       "targetInfo": {"type": "iframe", "targetId": format!("T{session}"), "url": "https://x.test/"}}
        }));
    }

    fn request(f: &Arc<Frames>, session: &str, id: &str) {
        f.on_message(json!({
            "method": "Network.requestWillBeSent", "sessionId": session,
            "params": {"requestId": id, "request": {"url": "https://x.test/a.js"}, "type": "Script"}
        }));
    }

    /// review-i2 m5 and m4: a frame that detaches ends its requests in
    /// flight (they'd stay "busy" until networkIdleMaxMs), and the
    /// connection closing tells the page's network.
    #[tokio::test]
    async fn a_detached_frame_ends_its_requests_and_closing_is_reported() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let f = Arc::new(Frames::with_sender(tx));
        *lock(&f.page_session) = "PAGE".into();
        let rec = Arc::new(Rec::default());
        f.set_network(rec.clone());
        attached(&f, "S1");
        attached(&f, "S2");
        request(&f, "S1", "r1");
        request(&f, "S1", "r2");
        request(&f, "S2", "r3");
        f.on_message(json!({"method": "Network.loadingFinished", "sessionId": "S1", "params": {"requestId": "r2"}}));
        assert_eq!(*lock(&rec.finished), ["r2"]);
        f.on_message(json!({"method": "Target.detachedFromTarget", "sessionId": "PAGE", "params": {"sessionId": "S1"}}));
        assert_eq!(
            *lock(&rec.finished),
            ["r2", "r1"],
            "S1's request in flight ended"
        );
        // And its document, counted on the page's session (review-fi2 F7).
        assert_eq!(*lock(&rec.gone), ["TS1"]);
        assert_eq!(f.count(), 1);
        assert!(!rec.closed.load(Ordering::SeqCst));
        f.on_message(
            json!({"method": "Target.detachedFromTarget", "params": {"sessionId": "PAGE"}}),
        );
        assert!(rec.closed.load(Ordering::SeqCst), "the network wasn't told");
        assert!(!f.is_alive());
    }

    /// review-fi2 F1: one overrun is slowness, not a hang. A frame counts
    /// as hung only after [`OVERRUNS_TO_HUNG`] lookups in a row, an answer
    /// in between resets the count, and the mark expires (then a ping
    /// decides) or goes with a new document.
    #[tokio::test]
    async fn a_frame_is_hung_only_after_overruns_in_a_row() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let f = Arc::new(Frames::with_sender(tx));
        *lock(&f.page_session) = "PAGE".into();
        attached(&f, "S1");
        attached(&f, "S2");
        let keys = f.keys();
        assert_eq!(f.state(keys[0]), Some(FrameState::Bare));
        f.overran(keys[0], "test");
        f.overran(keys[0], "test");
        f.answered(keys[0]);
        f.overran(keys[0], "test");
        f.overran(keys[0], "test");
        assert_eq!(f.state(keys[0]), Some(FrameState::Bare), "not in a row");
        f.overran(keys[0], "test");
        assert_eq!(f.state(keys[0]), Some(FrameState::Hung));
        assert_eq!(f.askable(), 1);
        assert_eq!(f.keys(), keys, "still there for its own elements");
        // The mark expires: the frame is re-checked, not skipped for good.
        for fr in lock(&f.frames).iter_mut() {
            fr.hung_until = Some(Instant::now());
        }
        assert_eq!(f.state(keys[0]), Some(FrameState::Recheck));
        f.mark_hung(keys[0], "test");
        assert_eq!(f.state(keys[0]), Some(FrameState::Hung));
        f.on_message(
            json!({"method": "Runtime.executionContextsCleared", "sessionId": "S1", "params": {}}),
        );
        assert_eq!(f.state(keys[0]), Some(FrameState::Bare));
        assert_eq!(f.askable(), 2);
        assert!(is_timeout(&err(format!(
            "Runtime.callFunctionOn: {NO_ANSWER} 10s"
        ))));
        assert!(!is_timeout(&err("Cannot find context with specified id")));
        assert!(is_library_error(&err(format!(
            "{LIBRARY_ERROR}: SyntaxError"
        ))));
    }
}
