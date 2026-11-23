//! A live screencast of a page, for the inspector console's mirror.
//!
//! `Page.startScreencast` sends a JPEG each time the page repaints (and
//! nothing while it doesn't), which is exactly what a mirror wants. The
//! frames go to a bounded channel: a viewer that falls behind loses frames
//! (Chrome waits for the ack before sending the next, so it never queues).
//!
//! A page has one screencast: a test recording its video (`artifacts.video`)
//! on the same page at the same time would share it, and whichever stops
//! first stops both. The console does not run tests on a page it mirrors
//! (a console run starts its own browser in its own process).

use std::sync::Arc;

use atalanta_common::{AtalantaError, Result};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
    StopScreencastParams,
};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::cdp::Page;
use crate::WebDriver;

/// One screencast frame.
#[derive(Debug, Clone)]
pub struct LiveFrame {
    pub jpeg: Vec<u8>,
    /// The page's size in CSS pixels (what taps are given in).
    pub width: u32,
    pub height: u32,
}

/// A running screencast; dropping it stops it.
pub struct LiveScreencast {
    pub frames: mpsc::Receiver<LiveFrame>,
    task: tokio::task::JoinHandle<()>,
    page: Arc<Page>,
}

impl Drop for LiveScreencast {
    fn drop(&mut self) {
        self.task.abort();
        let page = self.page.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = page.exec(StopScreencastParams::default()).await;
            });
        }
    }
}

impl WebDriver {
    /// Start a screencast of `device_id`'s page, at most `max_side` pixels
    /// on its longer side.
    pub async fn live_screencast(&self, device_id: &str, max_side: u32) -> Result<LiveScreencast> {
        let page = self.page_for_live(device_id).await?;
        let mut events = page
            .page
            .event_listener::<EventScreencastFrame>()
            .await
            .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("screencast events: {e}")))?;
        let (tx, frames) = mpsc::channel(4);
        let p = page.clone();
        let task = tokio::spawn(async move {
            use base64::Engine;
            while let Some(e) = events.next().await {
                let _ = p.exec(ScreencastFrameAckParams::new(e.session_id)).await;
                let Ok(jpeg) =
                    base64::engine::general_purpose::STANDARD.decode(AsRef::<str>::as_ref(&e.data))
                else {
                    continue;
                };
                let frame = LiveFrame {
                    jpeg,
                    width: e.metadata.device_width as u32,
                    height: e.metadata.device_height as u32,
                };
                match tx.try_send(frame) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });
        let params = StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(70)
            .max_width(max_side as i64)
            .max_height(max_side as i64)
            .every_nth_frame(1)
            .build();
        page.exec(params).await?;
        Ok(LiveScreencast { frames, task, page })
    }
}
