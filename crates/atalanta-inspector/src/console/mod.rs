//! The console: `atalanta inspect` as a device console with a live mirror
//! (in the spirit of ARTEMIS's web console).
//!
//! - [`mirror`]: the live screen over a WebSocket (H.264 for WebCodecs, or
//!   pushed JPEG/PNG frames), from a [`LiveSource`].
//! - [`control`]: who drives the device. Input takes the device's
//!   cross-process lease ([`Leases`]); watching needs none.
//! - [`tasks`]: the task queue. A suite run (`atalanta run` in a child
//!   process, with live step events) or an explore session (a
//!   `atalanta explore` child, [`tasks::ExploreChild`]).
//! - [`diagnostics`]: the connection wizard's checks ([`Diagnostics`]:
//!   `atalanta doctor`'s, from the CLI).
//! - [`routing`]: one driver over Android, iOS and the web.
//!
//! Every hook has a default that works without a device, so the server's
//! tests run on any host; the CLI plugs the real ones in.

pub mod control;
pub mod diagnostics;
pub mod live;
pub mod mirror;
pub mod mjpeg;
pub mod project;
pub mod routing;
pub mod tasks;

use std::path::PathBuf;
use std::sync::Arc;

pub use control::{LeaseFail, Leases, NoLeases};
pub use diagnostics::{Diagnostics, NoDiagnostics};
pub use live::{FrameFeed, ImageFrame, LiveSource, NoLive};
pub use tasks::{ConsoleTask, ExploreProvider, ExploreUnavailable, TaskCtx, TaskOutcome};

/// The console's pluggable parts.
pub struct ConsoleHooks {
    pub live: Arc<dyn LiveSource>,
    pub leases: Arc<dyn Leases>,
    pub diagnostics: Arc<dyn Diagnostics>,
    pub explore: Arc<dyn ExploreProvider>,
    /// Starts a suite run (`atalanta run`).
    pub launcher: Arc<dyn tasks::SuiteLauncher>,
    /// The project folder: suites are listed from here, run from here, and
    /// may not be outside it.
    pub project_dir: PathBuf,
    /// Where console runs write their artifacts: `<runs_dir>/<task id>/`.
    pub runs_dir: PathBuf,
}

impl Default for ConsoleHooks {
    fn default() -> Self {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            live: Arc::new(NoLive),
            leases: Arc::new(NoLeases),
            diagnostics: Arc::new(NoDiagnostics),
            explore: Arc::new(ExploreUnavailable),
            launcher: Arc::new(tasks::AtalantaBinary::current()),
            runs_dir: project_dir.join("atalanta-results").join("console"),
            project_dir,
        }
    }
}
