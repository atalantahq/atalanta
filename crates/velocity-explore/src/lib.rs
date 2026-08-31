//! `velocity explore`: AI explores once, deterministic replay runs CI.
//!
//! An agent works through a goal ("open Network & internet") on a real
//! device, one operation per turn, and the session is written out as a
//! plain Velocity YAML test with real assertions. That test then replays
//! at native speed with no model calls; when a replay fails, one repair
//! round re-explores from the step before the failure.
//!
//! The model picks; code decides. Each turn the model returns an operation
//! and an element *number* from the screen list ([`velocity_nl::GoalStep`]);
//! code builds the selector (`robust_selector`: id > accessibility label >
//! unique text), fills typed values from the run's own values by name, and
//! judges success with the checker, which must cite real elements.
//!
//! Library use (the MCP server, the console):
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use velocity_explore::*;
//! # async fn demo(driver: Arc<dyn velocity_common::PlatformDriver>,
//! #   planner: Arc<dyn velocity_nl::NaturalPlanner>,
//! #   checker: Arc<dyn velocity_nl::ScreenChecker>) -> velocity_common::Result<()> {
//! let opts = ExploreOptions::new("open Network & internet", "com.android.settings",
//!     "emulator-5554", velocity_common::Platform::Android);
//! let job = ExploreJob::new(driver, planner, Some(checker), opts)
//!     .with_events(Box::new(|e| eprintln!("{}", serde_json::to_string(e).unwrap())));
//! let handle = job.handle(); // handle.inject("use the search box".into()); handle.stop();
//! let result = job.run().await?;
//! println!("{}", result.yaml.unwrap_or_default());
//! # let _ = handle; Ok(()) }
//! ```

pub mod demos;
pub mod events;
pub mod expect;
pub mod job;
pub mod replay;
pub mod screen;
/// Public: `velocity explore --script` plays it.
pub mod scripted;
pub mod session;
/// A fake app for this crate's integration tests; not part of the API.
#[doc(hidden)]
pub mod testkit;
pub mod values;
pub mod yaml;

pub use events::{EventSink, ExploreEvent, InstructionSource};
pub use expect::{ExpectCheck, ExpectOutcome, ExpectationCounts, PostAssertion};
pub use job::{ExploreJob, JobResult};
pub use replay::{replay_once, ReplayReport, ReplayRun};
pub use session::{
    ExploreHandle, ExploreOptions, ExploreReport, ExploreSession, LaunchMode, Outcome,
    RecordedStep, ScreenshotMode, ValueKind, ValueSlot,
};
pub use yaml::{generate, generate_nl, GenOptions, Generated};
