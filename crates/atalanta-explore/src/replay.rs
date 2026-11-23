//! Replaying a generated test: the deterministic executor alone, with no
//! planner attached, so no model can be called (and the caller checks the
//! usage counters stayed put).

use std::collections::HashMap;
use std::time::Instant;

use atalanta_common::{Platform, PlatformDriver, Result, StepStatus, TestStatus};
use atalanta_core::TestExecutor;
use serde::Serialize;

/// One step of one replay.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStep {
    pub index: usize,
    pub action: String,
    pub passed: bool,
    pub ms: u64,
}

/// One replay of the test.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRun {
    pub passed: bool,
    pub ms: u64,
    pub steps: Vec<ReplayStep>,
    /// The first failing step (0-based among the test's steps).
    pub failed_step: Option<usize>,
    pub error: Option<String>,
}

/// All replays of a test.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    pub runs: Vec<ReplayRun>,
    /// Model calls made while replaying (always 0; checked).
    pub model_calls: u64,
}

impl ReplayReport {
    pub fn passed(&self) -> usize {
        self.runs.iter().filter(|r| r.passed).count()
    }

    pub fn all_passed(&self) -> bool {
        !self.runs.is_empty() && self.runs.iter().all(|r| r.passed)
    }

    pub fn total_ms(&self) -> u64 {
        self.runs.iter().map(|r| r.ms).sum()
    }
}

/// Run the suite's first test once on `device_id`, with `overrides` for
/// its `${VAR}`s, the session's settings (`base`), and no planner;
/// `jitter` (max ms, seed) pauses randomly before each step.
pub async fn replay_once(
    driver: &dyn PlatformDriver,
    yaml: &str,
    device_id: &str,
    platform: Platform,
    overrides: &HashMap<String, String>,
    base: &atalanta_common::SuiteConfig,
    jitter: Option<(u64, u64)>,
) -> Result<ReplayRun> {
    let mut suite = atalanta_core::parse_suite_from_str(yaml)?;
    atalanta_core::env::interpolate_suite(&mut suite, overrides)?;
    // The generated suite has no `config:`; replay with the settings the
    // session explored with.
    let mut config = base.clone();
    config.platform = Some(platform);
    config.artifacts.on_failure = false;
    let test = suite
        .tests
        .first()
        .ok_or_else(|| atalanta_common::AtalantaError::Config("the test has no steps".into()))?;
    let mut executor = TestExecutor::new(driver, config, &suite.app_id);
    // Burn-in (row 1.3): a random pause before each step, so a test that
    // only passes at one speed shows it.
    if let Some((max_ms, seed)) = jitter {
        executor.set_jitter(max_ms, seed);
    }
    let t = Instant::now();
    let result = executor
        .execute_test(test, device_id, &suite.app_id)
        .await?;
    let ms = t.elapsed().as_millis() as u64;
    let steps: Vec<ReplayStep> = result
        .steps
        .iter()
        .map(|s| ReplayStep {
            index: s.step_index,
            action: s.action_name.clone(),
            passed: s.status == StepStatus::Passed,
            ms: s.duration.as_millis() as u64,
        })
        .collect();
    let failed_step = result
        .steps
        .iter()
        .find(|s| s.status == StepStatus::Failed)
        .map(|s| s.step_index);
    Ok(ReplayRun {
        passed: result.status == TestStatus::Passed,
        ms,
        steps,
        failed_step,
        error: result
            .error_message
            .map(|e| atalanta_common::redact::redact(&e).into_owned()),
    })
}
