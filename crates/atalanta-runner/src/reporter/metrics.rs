//! `run-metrics.json`: the numbers a farm operator graphs, next to
//! `results.json`. Per device: how many tests it ran, how long it was busy
//! with them, retries and infrastructure failures; for the run: wall time,
//! device utilisation, retries used and tests served from the cache.
//!
//! Derived from the results only, so it says nothing about lease waits or
//! install time (those are in the debug log's spans).

use std::collections::BTreeMap;
use std::time::Duration;

use atalanta_common::{AtalantaError, Result, SuiteResult, TestStatus};
use serde::Serialize;

/// The `run-metrics.json` format version.
pub const METRICS_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct DeviceMetrics {
    pub tests: usize,
    pub passed: usize,
    pub failed: usize,
    pub flaky: usize,
    /// Time spent running tests (every attempt), in ms.
    pub busy_ms: u64,
    /// `busy_ms` over the run's wall time, 0..1.
    pub utilisation: f64,
    pub retries: u32,
    pub infrastructure_failures: usize,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct RunMetrics {
    pub format_version: u32,
    pub wall_ms: u64,
    pub tests: usize,
    /// Retries used across the run.
    pub retries_used: u32,
    /// Tests reported from the result cache, not run.
    pub from_cache: usize,
    /// Failures the quarantine list forgave.
    pub quarantined: usize,
    pub infrastructure_failures: usize,
    pub internal_errors: usize,
    /// Mean of the devices' utilisation.
    pub utilisation: f64,
    /// Keyed by device id; tests with no device (not run) are left out.
    pub devices: BTreeMap<String, DeviceMetrics>,
}

pub fn metrics(result: &SuiteResult, wall: Duration) -> RunMetrics {
    let wall_ms = u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);
    let mut devices: BTreeMap<String, DeviceMetrics> = BTreeMap::new();
    for t in &result.tests {
        if t.from_cache {
            continue;
        }
        let Some(d) = &t.device else { continue };
        let m = devices.entry(d.clone()).or_default();
        m.tests += 1;
        match t.status {
            TestStatus::Passed => m.passed += 1,
            TestStatus::Retried => m.flaky += 1,
            TestStatus::Failed => m.failed += 1,
            TestStatus::Skipped => {}
        }
        m.busy_ms += u64::try_from(t.duration.as_millis()).unwrap_or(u64::MAX);
        m.retries += t.retries;
        m.infrastructure_failures += usize::from(t.infrastructure_failure);
    }
    for m in devices.values_mut() {
        m.utilisation = ratio(m.busy_ms, wall_ms);
    }
    let utilisation = if devices.is_empty() {
        0.0
    } else {
        devices.values().map(|m| m.utilisation).sum::<f64>() / devices.len() as f64
    };
    RunMetrics {
        format_version: METRICS_VERSION,
        wall_ms,
        tests: result.tests.len(),
        retries_used: result.tests.iter().map(|t| t.retries).sum(),
        from_cache: result.tests.iter().filter(|t| t.from_cache).count(),
        quarantined: result
            .tests
            .iter()
            .filter(|t| t.quarantined.is_some())
            .count(),
        infrastructure_failures: result
            .tests
            .iter()
            .filter(|t| t.infrastructure_failure)
            .count(),
        internal_errors: result.tests.iter().filter(|t| t.internal_error).count(),
        utilisation,
        devices,
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    // Rounded to 3 places: a dashboard value, not a measurement.
    ((part as f64 / whole as f64).min(1.0) * 1000.0).round() / 1000.0
}

/// Write `run-metrics.json` at `path`.
pub fn write(result: &SuiteResult, wall: Duration, path: &str) -> Result<()> {
    let json = serde_json::to_string_pretty(&metrics(result, wall))
        .map_err(|e| AtalantaError::io("failed to serialize run metrics", e))?;
    atalanta_common::fsutil::write_atomic(std::path::Path::new(path), json.as_bytes())
        .map_err(|e| AtalantaError::io(format!("failed to write run metrics at {path}"), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::TestResult;

    fn ran(name: &str, device: &str, ms: u64, status: TestStatus) -> TestResult {
        let mut t = TestResult::failed(name, String::new(), Duration::from_millis(ms));
        t.status = status;
        t.device = Some(device.into());
        t
    }

    #[test]
    fn per_device_busy_time_retries_and_utilisation() {
        let mut flaky = ran("c", "emu-1", 2_000, TestStatus::Retried);
        flaky.retries = 1;
        let mut infra = ran("d", "emu-2", 1_000, TestStatus::Failed);
        infra.infrastructure_failure = true;
        let mut cached = ran("e", "emu-2", 0, TestStatus::Passed);
        cached.from_cache = true;
        let result = SuiteResult {
            total: 5,
            passed: 2,
            failed: 1,
            skipped: 0,
            retried: 1,
            duration: Duration::from_secs(10),
            tests: vec![
                ran("a", "emu-1", 3_000, TestStatus::Passed),
                ran("b", "emu-2", 4_000, TestStatus::Passed),
                flaky,
                infra,
                cached,
            ],
            shard_index: None,
            shard_total: None,
        };
        let m = metrics(&result, Duration::from_secs(10));
        assert_eq!(m.retries_used, 1);
        assert_eq!(m.from_cache, 1);
        assert_eq!(m.infrastructure_failures, 1);
        let e1 = &m.devices["emu-1"];
        assert_eq!(
            (e1.tests, e1.flaky, e1.busy_ms, e1.retries),
            (2, 1, 5_000, 1)
        );
        assert_eq!(e1.utilisation, 0.5);
        let e2 = &m.devices["emu-2"];
        assert_eq!((e2.tests, e2.infrastructure_failures), (2, 1));
        assert_eq!(m.utilisation, 0.5);
    }
}
