//! The `BrowserProtocol` seam (W5, D-W1): what the driver's logic needs
//! from a browser page, apart from how it is spoken to.
//!
//! The web driver's logic (resolving selectors, the actionability gate and
//! hit-target retry, typing with the focus check, reading an element's
//! live state, cross-origin frames) talks to the page only through this
//! trait. The Chrome DevTools Protocol implementation is [`crate::cdp::Tab`]:
//! Velocity's in-page library (`injected.js`) run through
//! `Runtime.callFunctionOn` in an isolated world, input through
//! `Input.dispatch*Event`. Launching the browser, its contexts, the network
//! (request tracking, mocks) and navigation stay CDP-specific in
//! `driver.rs` and `network.rs`.
//!
//! A WebDriver BiDi implementation (Firefox, Safari through safaridriver)
//! would run the same library with `script.callFunction` in a sandbox
//! realm, and send input with `input.performActions`. It is not written:
//! neither Firefox nor a usable safaridriver is available here to test
//! one against (safaridriver needs "Allow Remote Automation" switched on).
//! The driver logic's own tests run against a fake implementation
//! (`driver.rs`, `fake_protocol` tests), with no browser at all.

use std::time::Duration;

use serde_json::Value;
use velocity_common::Result;

use crate::cdp::{self, Tab, CDP_TIMEOUT};

/// One browser page, as the driver's logic sees it.
#[async_trait::async_trait]
pub trait BrowserProtocol: Send + Sync {
    /// The protocol's name, for messages (`cdp`, `bidi`).
    fn protocol(&self) -> &'static str;

    /// Whether the connection to the browser is still up.
    fn is_alive(&self) -> bool;

    /// A touch device profile: taps and swipes are touches.
    fn touch(&self) -> bool;

    /// Run `op` of the in-page library with `args` in the page's own
    /// document, each round-trip bounded by `limit`.
    async fn call_for(&self, limit: Duration, op: &str, args: Vec<Value>) -> Result<Value>;

    /// [`BrowserProtocol::call_for`] with the default bound.
    async fn call(&self, op: &str, args: Vec<Value>) -> Result<Value> {
        self.call_for(CDP_TIMEOUT, op, args).await
    }

    /// Run `op` on the element `id` (its id first, then `rest`), in the
    /// document that holds it (the page, or a cross-origin frame), with
    /// the reply in the page's coordinates.
    async fn call_el(&self, op: &str, id: &str, rest: Vec<Value>) -> Result<Value>;

    /// Run `op` with `args` in the document that holds the element `id`.
    async fn call_near(
        &self,
        id: &str,
        limit: Duration,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value>;

    /// `find`/`findAll` in each cross-origin frame, replies in the page's
    /// terms.
    async fn call_frames(&self, op: &str, args: Vec<Value>) -> crate::cdp::FrameReplies;

    /// A left click at a viewport point (a tap on a touch profile).
    async fn click(&self, x: f64, y: f64) -> Result<()>;
    async fn double_click(&self, x: f64, y: f64) -> Result<()>;
    async fn long_press(&self, x: f64, y: f64, ms: u64) -> Result<()>;
    /// A pointer drag with the button held.
    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<()>;
    /// `drag` (row 3.1): press, hold, a slow move, a rest, release.
    async fn drag_held(
        &self,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        let _ = (hold_ms, move_ms);
        self.drag(from, to).await
    }
    /// A finger stroke (touch profiles).
    async fn touch_drag(&self, from: (f64, f64), to: (f64, f64)) -> Result<()>;
    /// The mouse wheel at a point.
    async fn wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()>;
    /// One key press by name (`Enter`, `Backspace`, …).
    async fn press_key(&self, key: &str) -> Result<()>;
    /// Text into whatever has the focus, in one go.
    async fn insert_text(&self, text: &str) -> Result<()>;
    /// Text into whatever has the focus as a user types it: a key press
    /// per character where the protocol has one, so `keydown`/`keyup`
    /// listeners see it. [`BrowserProtocol::insert_text`] by default.
    async fn type_text(&self, text: &str) -> Result<()> {
        self.insert_text(text).await
    }
    /// Select all in the focused field and delete it.
    async fn select_all_and_delete(&self) -> Result<()>;
    /// Move the caret to the end of the focused field.
    async fn move_caret_to_end(&self) -> Result<()>;
    /// The viewport as PNG.
    async fn screenshot_png(&self) -> Result<Vec<u8>>;
    /// Which cross-origin frame holds the focus, if one does (a key that
    /// stays equal while the focus stays in that frame). `None` when the
    /// focus is in the page's own documents or can't be told.
    async fn opaque_focus(&self) -> Option<String> {
        None
    }
    /// Give the `<input type=file>` the element `id` stands for (it, or
    /// its `<label>`) the files at `paths` (review 10 #11).
    async fn set_files(&self, id: &str, paths: Vec<String>) -> Result<()> {
        let _ = (id, paths);
        Err(velocity_common::VelocityError::Internal(anyhow::anyhow!(
            "setting a file input's files is not supported over {}",
            self.protocol()
        )))
    }
}

/// The Chrome DevTools Protocol page.
#[async_trait::async_trait]
impl BrowserProtocol for Tab {
    fn protocol(&self) -> &'static str {
        "cdp"
    }

    fn is_alive(&self) -> bool {
        Tab::is_alive(self)
    }

    fn touch(&self) -> bool {
        Tab::touch(self)
    }

    async fn call_for(&self, limit: Duration, op: &str, args: Vec<Value>) -> Result<Value> {
        Tab::call_for(self, limit, op, args).await
    }

    async fn call_el(&self, op: &str, id: &str, rest: Vec<Value>) -> Result<Value> {
        Tab::call_el(self, op, id, rest).await
    }

    async fn call_near(
        &self,
        id: &str,
        limit: Duration,
        op: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        Tab::call_near(self, id, limit, op, args).await
    }

    async fn call_frames(&self, op: &str, args: Vec<Value>) -> crate::cdp::FrameReplies {
        Tab::call_frames(self, op, args).await
    }

    async fn opaque_focus(&self) -> Option<String> {
        crate::driver::opaque_focus(self).await
    }

    async fn set_files(&self, id: &str, paths: Vec<String>) -> Result<()> {
        Tab::set_files(self, id, paths).await
    }

    async fn click(&self, x: f64, y: f64) -> Result<()> {
        cdp::click(self, x, y).await
    }

    async fn double_click(&self, x: f64, y: f64) -> Result<()> {
        cdp::double_click(self, x, y).await
    }

    async fn long_press(&self, x: f64, y: f64, ms: u64) -> Result<()> {
        cdp::long_press(self, x, y, ms).await
    }

    async fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        cdp::drag(self, from, to).await
    }

    async fn drag_held(
        &self,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        cdp::drag_held(self, from, to, hold_ms, move_ms).await
    }

    async fn touch_drag(&self, from: (f64, f64), to: (f64, f64)) -> Result<()> {
        cdp::touch_drag(self, from, to).await
    }

    async fn wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        cdp::wheel(self, x, y, dx, dy).await
    }

    async fn press_key(&self, key: &str) -> Result<()> {
        cdp::dispatch_key(self, key).await
    }

    async fn insert_text(&self, text: &str) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
        self.exec(InsertTextParams::new(text)).await?;
        Ok(())
    }

    async fn type_text(&self, text: &str) -> Result<()> {
        cdp::type_text(self, text).await
    }

    async fn select_all_and_delete(&self) -> Result<()> {
        cdp::select_all_and_delete(self).await
    }

    async fn move_caret_to_end(&self) -> Result<()> {
        cdp::move_to_end(self).await
    }

    async fn screenshot_png(&self) -> Result<Vec<u8>> {
        use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
        use chromiumoxide::page::ScreenshotParams;
        self.run(
            self.page.screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .build(),
            ),
        )
        .await
    }
}
