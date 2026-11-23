//! Natural-language test steps for Atalanta.
//!
//! Lets tests be written in plain English (passmark-style) —
//! `- "Tap the login button"` or `- assert: "the welcome screen is visible"` —
//! and resolves each step to a concrete [`atalanta_common::Action`] at run time
//! using an LLM planner. The planner only picks an operation and the number of
//! an element from the screen list; code builds the selector, checks it, and
//! fills in the test's values ([`grounding`]). The first resolution for a step
//! is cached, so reruns replay instantly without another model call, and a
//! cached step that stops working is re-planned against the changed UI
//! (self-healing). A plan is cached only after the run verified it, and
//! the cache key holds no test data: values are filled in on replay.
//!
//! Two providers sit behind [`NaturalPlanner`]: Anthropic ([`anthropic`])
//! and any OpenAI-compatible endpoint ([`openai`]); [`AiSettings`] picks
//! one from the environment and the suite's `config.ai`. The optional AI
//! checker (`verify:`) is in [`checker`].
//!
//! ```no_run
//! # use atalanta_nl::{AiSettings, NaturalResolver};
//! # async fn demo() -> atalanta_common::Result<()> {
//! if let Some(ai) = AiSettings::from_env()? {
//!     let resolver = NaturalResolver::new(ai.planner()?, ".atalanta/nl-cache.json")
//!         .with_checker(ai.checker()?);
//!     # let _ = resolver;
//! }
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

pub mod anthropic;
pub mod bedrock;
pub mod cache;
pub mod checker;
pub mod eval;
pub mod gcp;
pub mod goal;
pub mod grounding;
pub mod openai;
pub mod planner;
pub mod provider;
pub mod resolver;
pub mod sigv4;
#[cfg(test)]
pub(crate) mod testutil;

pub use anthropic::AnthropicPlanner;
pub use atalanta_common::natural::{CachedHit, NaturalStepResolver};
pub use bedrock::BedrockPlanner;
pub use cache::NlCache;
pub use checker::{verify_claim, CheckAnswer, CheckRequest, ScreenChecker};
pub use goal::{Expectation, GoalOp, GoalOutcome, GoalRequest, GoalStep};
pub use openai::{Auth, OpenAiOptions, OpenAiPlanner, OutputMode};
pub use planner::{
    summarize_screen, NaturalPlanner, Op, PlanRequest, PlannedStep, Price, ScreenIndex, Usage,
};
pub use provider::{find_preset, AiSettings, Caps, Preset, Wire, PRESETS};
pub use resolver::{
    plan_step, plan_step_on_screen, plan_step_outcome, step_shape, CacheMode, NaturalResolver,
    NaturalStep, PendingPlan, PlanFailure, StepKeys,
};
