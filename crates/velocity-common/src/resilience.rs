use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{ErrorKind, Result, VelocityError};
use crate::traits::{HealthStatus, PlatformDriver};
use crate::types::{DeviceInfo, Direction, Element, Key, LaunchArgs, Selector};

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            backoff_multiplier: 2.0,
        }
    }
}

/// How long a half-open probe may stay unanswered before its slot is
/// given to another caller (an install can legitimately take minutes).
const PROBE_RECLAIM_MS: u64 = 5 * 60 * 1000;

/// Circuit breaker states.
const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Prevents cascade failures when the underlying driver becomes unresponsive.
///
/// v2 enhancements:
/// - Gradual ramp-up in half-open state (allows limited requests before full close)
/// - Tracing events on state transitions for observability
/// - Configurable from YAML
pub struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    last_failure_epoch_ms: AtomicU64,
    half_open_successes: AtomicU32,
    /// Probes admitted while half-open and not yet answered (VEL-225).
    half_open_inflight: AtomicU32,
    /// When the breaker went half-open: a probe whose caller was cancelled
    /// never answers, so its slot is reclaimed after `PROBE_RECLAIM`.
    half_open_since_ms: AtomicU64,
    /// Bumped on every move to half-open, so a permit from an earlier
    /// half-open period never frees a slot of the current one.
    half_open_generation: AtomicU64,
    threshold: u32,
    reset_timeout: Duration,
    half_open_max_requests: u32,
    name: String,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, reset_timeout: Duration) -> Self {
        Self::named("global", threshold, reset_timeout, 2)
    }

    /// Create a named circuit breaker (e.g., per-device).
    pub fn named(name: &str, threshold: u32, reset_timeout: Duration, half_open_max: u32) -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            failure_count: AtomicU32::new(0),
            last_failure_epoch_ms: AtomicU64::new(0),
            half_open_successes: AtomicU32::new(0),
            half_open_inflight: AtomicU32::new(0),
            half_open_since_ms: AtomicU64::new(0),
            half_open_generation: AtomicU64::new(0),
            threshold,
            reset_timeout,
            half_open_max_requests: half_open_max,
            name: name.to_string(),
        }
    }

    /// Whether calls are refused right now. An open breaker whose cooldown
    /// has passed goes half-open; a half-open one is open again once all of
    /// its `half_open_max_requests` probes are in flight.
    pub fn is_open(&self) -> bool {
        self.admit(false, &mut false)
    }

    /// Like [`is_open`](Self::is_open), but a half-open breaker hands out one
    /// of its probe slots: `true` means the call may go ahead. Only
    /// `half_open_max_requests` probes run at once (VEL-225: the doc promised
    /// a limited ramp-up, but every caller was let through).
    ///
    /// The returned permit gives its probe slot back when it is dropped, so
    /// a caller that is cancelled mid-probe (a timeout around the driver
    /// call) no longer leaks the slot and locks the device out.
    pub fn try_acquire(&self) -> Option<ProbePermit<'_>> {
        let mut took = false;
        if self.admit(true, &mut took) {
            return None;
        }
        Some(ProbePermit {
            breaker: self,
            // Read after `admit`, which may have just moved the breaker to
            // half-open and bumped the generation.
            slot: took.then(|| self.half_open_generation.load(Ordering::SeqCst)),
        })
    }

    /// Shared by `is_open` and `try_acquire`: returns whether the call is
    /// refused, taking a probe slot when `take` is set (and then setting
    /// `took`).
    fn admit(&self, take: bool, took: &mut bool) -> bool {
        let state = self.state.load(Ordering::SeqCst);
        if state == CLOSED {
            return false;
        }
        let reset = self.reset_timeout.as_millis() as u64;
        if state == OPEN {
            let last = self.last_failure_epoch_ms.load(Ordering::SeqCst);
            if epoch_ms().saturating_sub(last) <= reset {
                return true;
            }
            // Only one caller moves it to half-open, so the probe count is
            // reset once, not under a probe already admitted.
            if self
                .state
                .compare_exchange(OPEN, HALF_OPEN, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                tracing::info!(breaker = %self.name, "circuit breaker transitioning to half-open");
                self.half_open_successes.store(0, Ordering::SeqCst);
                self.half_open_inflight.store(0, Ordering::SeqCst);
                self.half_open_since_ms.store(epoch_ms(), Ordering::SeqCst);
                self.half_open_generation.fetch_add(1, Ordering::SeqCst);
            }
        }
        if self.state.load(Ordering::SeqCst) != HALF_OPEN {
            return self.state.load(Ordering::SeqCst) == OPEN;
        }
        let since = self.half_open_since_ms.load(Ordering::SeqCst);
        if epoch_ms().saturating_sub(since) > reset.max(PROBE_RECLAIM_MS) {
            // Probes that never answered (their caller was dropped).
            self.half_open_inflight.store(0, Ordering::SeqCst);
            self.half_open_since_ms.store(epoch_ms(), Ordering::SeqCst);
            self.half_open_generation.fetch_add(1, Ordering::SeqCst);
        }
        let max = self.half_open_max_requests.max(1);
        if !take {
            return self.half_open_inflight.load(Ordering::SeqCst) >= max;
        }
        let refused = update(&self.half_open_inflight, |n| (n < max).then_some(n + 1)).is_err();
        *took = !refused;
        refused
    }

    /// A half-open probe has finished (answered or been dropped): free its
    /// slot, unless the breaker has since left that half-open period.
    fn release_probe(&self, generation: u64) {
        if self.half_open_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let _ = update(&self.half_open_inflight, |n| n.checked_sub(1));
    }

    /// Half-open with every probe slot taken.
    fn is_half_open_full(&self) -> bool {
        self.is_half_open()
            && self.half_open_inflight.load(Ordering::SeqCst) >= self.half_open_max_requests.max(1)
    }

    /// Open and still cooling down, without the half-open transition
    /// `is_open` makes. Checked between retry attempts.
    fn is_tripped(&self) -> bool {
        self.state.load(Ordering::SeqCst) == OPEN && self.remaining_open() != Some(Duration::ZERO)
    }

    /// Whether the breaker is half-open (its next failure re-opens it).
    fn is_half_open(&self) -> bool {
        self.state.load(Ordering::SeqCst) == HALF_OPEN
    }

    /// While open: how long until the half-open probe is allowed.
    pub fn remaining_open(&self) -> Option<Duration> {
        if self.state.load(Ordering::SeqCst) != OPEN {
            return None;
        }
        let since = epoch_ms().saturating_sub(self.last_failure_epoch_ms.load(Ordering::SeqCst));
        Some(Duration::from_millis(
            (self.reset_timeout.as_millis() as u64).saturating_sub(since),
        ))
    }

    /// Current state name for diagnostics.
    pub fn state_name(&self) -> &'static str {
        match self.state.load(Ordering::SeqCst) {
            CLOSED => "closed",
            OPEN => "open",
            HALF_OPEN => "half-open",
            _ => "unknown",
        }
    }

    pub fn on_success(&self) {
        let prev_state = self.state.load(Ordering::SeqCst);
        if prev_state == HALF_OPEN {
            let successes = self.half_open_successes.fetch_add(1, Ordering::SeqCst) + 1;
            if successes >= self.half_open_max_requests {
                tracing::info!(
                    breaker = %self.name,
                    successes,
                    "circuit breaker closing after successful half-open ramp-up"
                );
                self.failure_count.store(0, Ordering::SeqCst);
                self.state.store(CLOSED, Ordering::SeqCst);
            }
        } else {
            self.failure_count.store(0, Ordering::SeqCst);
            self.state.store(CLOSED, Ordering::SeqCst);
        }
    }

    pub fn on_failure(&self) {
        let count = self.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_failure_epoch_ms
            .store(epoch_ms(), Ordering::SeqCst);

        let prev_state = self.state.load(Ordering::SeqCst);

        if prev_state == HALF_OPEN {
            // Any failure in half-open goes straight back to open
            tracing::warn!(
                breaker = %self.name,
                "circuit breaker re-opening from half-open after failure"
            );
            self.state.store(OPEN, Ordering::SeqCst);
        } else if count >= self.threshold {
            tracing::warn!(
                breaker = %self.name,
                failures = count,
                threshold = self.threshold,
                "circuit breaker opening"
            );
            self.state.store(OPEN, Ordering::SeqCst);
        }
    }

    /// Failure count for diagnostics.
    pub fn failure_count(&self) -> u32 {
        self.failure_count.load(Ordering::SeqCst)
    }
}

/// Admission through a [`CircuitBreaker`]. When it was handed out while the
/// breaker was half-open it holds one probe slot, which is given back on
/// drop: whether the probe answered or its future was cancelled.
pub struct ProbePermit<'a> {
    breaker: &'a CircuitBreaker,
    /// The half-open generation of the slot this permit holds, if any.
    slot: Option<u64>,
}

impl ProbePermit<'_> {
    /// Whether this permit holds a half-open probe slot.
    pub fn is_probe(&self) -> bool {
        self.slot.is_some()
    }
}

impl Drop for ProbePermit<'_> {
    fn drop(&mut self) {
        if let Some(generation) = self.slot.take() {
            self.breaker.release_probe(generation);
        }
    }
}

/// Milliseconds since the epoch. 64-bit: a u32 wraps every 49.7 days, and
/// a breaker that was open across the wrap would never re-close.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A driver wrapper that automatically retries transient failures
/// and trips a circuit breaker on sustained failures.
pub struct ResilientDriver {
    inner: Arc<dyn PlatformDriver>,
    retry_policy: RetryPolicy,
    /// One breaker per device: one sick emulator must not block the others.
    /// Calls not tied to a device (listing devices) use the "" breaker.
    breakers: std::sync::Mutex<std::collections::HashMap<String, Arc<CircuitBreaker>>>,
    /// How long a device's breaker stays open before one probe.
    breaker_reset: Duration,
}

impl ResilientDriver {
    pub fn new(inner: Arc<dyn PlatformDriver>) -> Self {
        Self {
            inner,
            retry_policy: RetryPolicy::default(),
            breakers: std::sync::Mutex::new(std::collections::HashMap::new()),
            breaker_reset: crate::settings::ms("VELOCITY_BREAKER_RESET_MS", 30_000),
        }
    }

    /// How long an open breaker waits before letting one probe through.
    pub fn with_breaker_reset(mut self, reset: Duration) -> Self {
        self.breaker_reset = reset;
        self
    }

    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    pub fn inner(&self) -> &Arc<dyn PlatformDriver> {
        &self.inner
    }

    /// The breaker for `device_id`, created on first use.
    pub fn breaker(&self, device_id: &str) -> Arc<CircuitBreaker> {
        let mut map = self.breakers.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(device_id.to_string())
            .or_insert_with(|| {
                Arc::new(CircuitBreaker::named(
                    if device_id.is_empty() {
                        "global"
                    } else {
                        device_id
                    },
                    crate::settings::num("VELOCITY_BREAKER_THRESHOLD", 5) as u32,
                    self.breaker_reset,
                    2,
                ))
            })
            .clone()
    }

    async fn with_retry<F, Fut, T>(&self, device_id: &str, operation: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let breaker = self.breaker(device_id);
        // Held across the call: dropping it (also when this future is
        // cancelled) frees a half-open probe slot (M-1).
        let _permit = self.wait_if_open(device_id, &breaker).await?;

        let max = self.retry_policy.max_retries;
        let mut backoff = self.retry_policy.initial_backoff;

        for attempt in 0..=max {
            match operation().await {
                Ok(v) => {
                    breaker.on_success();
                    return Ok(v);
                }
                Err(e) => {
                    let kind = e.kind();
                    tracing::debug!(device_id, attempt, ?kind, error = %e, "driver call failed");
                    let retries_for_kind = kind.max_retries().min(max);

                    // A failed probe re-opens a half-open breaker at once;
                    // retrying it would keep calling a device that is gone.
                    if attempt < retries_for_kind && !kind.is_permanent() && !breaker.is_half_open()
                    {
                        // Try session restart for session-related errors
                        if kind == ErrorKind::SessionExpired || kind == ErrorKind::ConnectionLost {
                            let _ = self.inner.restart_session().await;
                        }

                        tokio::time::sleep(backoff).await;
                        backoff = Duration::from_secs_f64(
                            (backoff.as_secs_f64() * self.retry_policy.backoff_multiplier)
                                .min(self.retry_policy.max_backoff.as_secs_f64()),
                        );
                        // VEL-224: the breaker counts one failure per call,
                        // not per attempt, and once another call has opened
                        // it the retries stop instead of hitting the device.
                        if breaker.is_tripped() {
                            return Err(VelocityError::CircuitOpen {
                                device_id: device_id.to_string(),
                            });
                        }
                        continue;
                    }

                    // A permanent error (element not found, assertion failed)
                    // means the driver answered: it's a test failure, not a
                    // sign the device is unhealthy. Counting it would let one
                    // missing element — polled a few times — open the breaker
                    // and fail every remaining test.
                    if kind.is_permanent() {
                        breaker.on_success();
                    } else {
                        breaker.on_failure();
                    }
                    return Err(e);
                }
            }
        }

        unreachable!()
    }
}

impl ResilientDriver {
    /// An open breaker waits out its cooldown, restarts the session and lets
    /// the half-open probe through, instead of failing the call at once. Failing
    /// at once turned one bad moment into a failed suite: every later test's
    /// first call hit the open breaker within the same second (VelocityLab:
    /// 29 of 31 tests failed in 0.0 s after one transient launch error). A
    /// device that is really gone still fails: the probe re-opens the breaker.
    ///
    /// A half-open breaker whose probe slots are all taken is not restarted:
    /// that would pull the session out from under the probes in flight and
    /// re-open the breaker. The caller waits briefly for a slot instead.
    async fn wait_if_open<'b>(
        &self,
        device_id: &str,
        breaker: &'b CircuitBreaker,
    ) -> Result<ProbePermit<'b>> {
        if let Some(permit) = breaker.try_acquire() {
            return Ok(permit);
        }
        if breaker.is_half_open_full() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return breaker
                .try_acquire()
                .ok_or_else(|| VelocityError::CircuitOpen {
                    device_id: device_id.to_string(),
                });
        }
        let wait = breaker.remaining_open().unwrap_or_default();
        tracing::warn!(
            device_id,
            wait_ms = wait.as_millis() as u64,
            "device unresponsive: waiting out the circuit breaker before one probe"
        );
        tokio::time::sleep(wait + Duration::from_millis(50)).await;
        let _ = self.inner.restart_session().await;
        breaker
            .try_acquire()
            .ok_or_else(|| VelocityError::CircuitOpen {
                device_id: device_id.to_string(),
            })
    }

    /// Run an action exactly once. Actions (tap, type, swipe, launch…) aren't
    /// idempotent: one that timed out may still have happened, so retrying
    /// could tap twice or type text twice. Only the breaker is consulted.
    async fn with_once<F, Fut, T>(&self, device_id: &str, operation: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let breaker = self.breaker(device_id);
        let _permit = self.wait_if_open(device_id, &breaker).await?;
        let result = operation().await;
        if let Err(e) = &result {
            tracing::debug!(device_id, kind = ?e.kind(), error = %e, "driver action failed");
        }
        match &result {
            Ok(_) => breaker.on_success(),
            Err(e) if e.kind().is_permanent() => breaker.on_success(),
            Err(_) => breaker.on_failure(),
        }
        result
    }
}

#[async_trait::async_trait]
impl PlatformDriver for ResilientDriver {
    async fn prepare(&self, device_id: &str) -> Result<()> {
        let id = device_id.to_string();
        self.with_retry(device_id, || {
            let id = id.clone();
            let inner = self.inner.clone();
            async move { inner.prepare(&id).await }
        })
        .await
    }

    async fn cleanup(&self) {
        self.inner.cleanup().await;
    }

    async fn health_check(&self) -> HealthStatus {
        let any_open = self
            .breakers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|b| b.is_open());
        if any_open {
            return HealthStatus::Unhealthy;
        }
        self.inner.health_check().await
    }

    async fn restart_session(&self) -> Result<()> {
        self.inner.restart_session().await
    }

    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.with_retry("", || {
            let inner = self.inner.clone();
            async move { inner.list_devices().await }
        })
        .await
    }

    async fn list_devices_strict(&self) -> Result<Vec<DeviceInfo>> {
        // No retry: a caller of the strict listing wants an answer now.
        self.inner.list_devices_strict().await
    }

    async fn boot_device(&self, device_id: &str) -> Result<()> {
        let id = device_id.to_string();
        self.with_retry(device_id, || {
            let id = id.clone();
            let inner = self.inner.clone();
            async move { inner.boot_device(&id).await }
        })
        .await
    }

    async fn shutdown_device(&self, device_id: &str) -> Result<()> {
        self.inner.shutdown_device(device_id).await
    }

    async fn install_app(&self, device_id: &str, app_path: &str) -> Result<()> {
        let id = device_id.to_string();
        let path = app_path.to_string();
        self.with_retry(device_id, || {
            let id = id.clone();
            let path = path.clone();
            let inner = self.inner.clone();
            async move { inner.install_app(&id, &path).await }
        })
        .await
    }

    async fn install_app_if_changed(
        &self,
        device_id: &str,
        app_path: &str,
        app_id: &str,
        hash: &str,
    ) -> Result<crate::InstallOutcome> {
        let (id, path, app, hash) = (
            device_id.to_string(),
            app_path.to_string(),
            app_id.to_string(),
            hash.to_string(),
        );
        self.with_retry(device_id, || {
            let (id, path, app, hash) = (id.clone(), path.clone(), app.clone(), hash.clone());
            let inner = self.inner.clone();
            async move { inner.install_app_if_changed(&id, &path, &app, &hash).await }
        })
        .await
    }

    async fn launch_app(&self, device_id: &str, app_id: &str, clear_state: bool) -> Result<()> {
        let did = device_id.to_string();
        let aid = app_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let aid = aid.clone();
            let inner = self.inner.clone();
            async move { inner.launch_app(&did, &aid, clear_state).await }
        })
        .await
    }

    async fn launch_app_with(
        &self,
        device_id: &str,
        app_id: &str,
        clear_state: bool,
        args: &LaunchArgs,
    ) -> Result<()> {
        let did = device_id.to_string();
        let aid = app_id.to_string();
        let args = args.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let aid = aid.clone();
            let args = args.clone();
            let inner = self.inner.clone();
            async move { inner.launch_app_with(&did, &aid, clear_state, &args).await }
        })
        .await
    }

    async fn stop_app(&self, device_id: &str, app_id: &str) -> Result<()> {
        self.inner.stop_app(device_id, app_id).await
    }

    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
        let did = device_id.to_string();
        let sel = selector.clone();
        self.with_retry(device_id, || {
            let did = did.clone();
            let sel = sel.clone();
            let inner = self.inner.clone();
            async move { inner.find_element(&did, &sel).await }
        })
        .await
    }

    async fn find_elements(&self, device_id: &str, selector: &Selector) -> Result<Vec<Element>> {
        let did = device_id.to_string();
        let sel = selector.clone();
        self.with_retry(device_id, || {
            let did = did.clone();
            let sel = sel.clone();
            let inner = self.inner.clone();
            async move { inner.find_elements(&did, &sel).await }
        })
        .await
    }

    async fn get_hierarchy(&self, device_id: &str) -> Result<Element> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.get_hierarchy(&did).await }
        })
        .await
    }

    // Forwarded so the inner driver's hierarchy cache is used; without these
    // the trait defaults would turn every cached read into a fresh dump.
    fn has_hierarchy_cache(&self) -> bool {
        self.inner.has_hierarchy_cache()
    }

    fn last_match_count(&self, device_id: &str, selector: &Selector) -> Option<usize> {
        self.inner.last_match_count(device_id, selector)
    }

    fn sync_probe_port(&self, device_id: &str) -> Option<u16> {
        self.inner.sync_probe_port(device_id)
    }

    async fn app_fingerprint(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.inner.app_fingerprint(device_id, app_id).await
    }

    async fn crash_report(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.inner.crash_report(device_id, app_id).await
    }

    async fn pending_crash(&self, device_id: &str, app_id: &str) -> Option<String> {
        self.inner.pending_crash(device_id, app_id).await
    }

    async fn copy_crash_logs(
        &self,
        device_id: &str,
        app_id: &str,
        dest: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        self.inner.copy_crash_logs(device_id, app_id, dest).await
    }

    async fn start_evidence(
        &self,
        device_id: &str,
        req: &crate::evidence::EvidenceRequest,
    ) -> Option<Box<dyn crate::evidence::EvidenceSession>> {
        self.inner.start_evidence(device_id, req).await
    }

    async fn app_liveness(&self, device_id: &str, app_id: &str) -> crate::traits::Liveness {
        self.inner.app_liveness(device_id, app_id).await
    }

    async fn system_bars(&self, device_id: &str) -> Option<Vec<crate::types::Rect>> {
        self.inner.system_bars(device_id).await
    }

    async fn freeze_status_bar(&self, device_id: &str) -> bool {
        self.inner.freeze_status_bar(device_id).await
    }

    async fn unfreeze_status_bar(&self) {
        self.inner.unfreeze_status_bar().await
    }

    async fn save_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        self.inner.save_app_state(device_id, app_id, name).await
    }

    async fn restore_app_state(&self, device_id: &str, app_id: &str, name: &str) -> Result<()> {
        self.inner.restore_app_state(device_id, app_id, name).await
    }

    async fn get_hierarchy_for_compare(&self, device_id: &str) -> Result<Element> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.get_hierarchy_for_compare(&did).await }
        })
        .await
    }

    async fn settle_on_device(
        &self,
        device_id: &str,
        reaction_ms: u64,
        quiet_ms: u64,
        cap_ms: u64,
    ) -> Option<crate::types::DeviceSettle> {
        self.inner
            .settle_on_device(device_id, reaction_ms, quiet_ms, cap_ms)
            .await
    }

    async fn get_hierarchy_cached(&self, device_id: &str) -> Result<Element> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.get_hierarchy_cached(&did).await }
        })
        .await
    }

    async fn invalidate_hierarchy_cache(&self) {
        self.inner.invalidate_hierarchy_cache().await;
    }

    async fn device_ready(&self, device_id: &str) -> bool {
        self.inner.device_ready(device_id).await
    }

    async fn set_stress(&self, device_id: &str, on: bool) -> bool {
        self.inner.set_stress(device_id, on).await
    }

    async fn reset_app(
        &self,
        device_id: &str,
        app_id: &str,
        reset: &crate::test_types::ResetConfig,
    ) -> Result<crate::test_types::ResetStrategy> {
        self.inner.reset_app(device_id, app_id, reset).await
    }

    async fn collect_resource_metrics(
        &self,
        device_id: &str,
        package: &str,
    ) -> Result<(u64, u64, u64, f32)> {
        self.inner
            .collect_resource_metrics(device_id, package)
            .await
    }

    async fn tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.tap(&did, &el).await }
        })
        .await
    }

    async fn double_tap(&self, device_id: &str, element: &Element) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.double_tap(&did, &el).await }
        })
        .await
    }

    async fn long_press(&self, device_id: &str, element: &Element, duration_ms: u64) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.long_press(&did, &el, duration_ms).await }
        })
        .await
    }

    async fn touch_forced(
        &self,
        device_id: &str,
        element: &Element,
        touch: crate::traits::Touch,
    ) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.touch_forced(&did, &el, touch).await }
        })
        .await
    }

    async fn input_text(&self, device_id: &str, element: &Element, text: &str) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        let txt = text.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let txt = txt.clone();
            let inner = self.inner.clone();
            async move { inner.input_text(&did, &el, &txt).await }
        })
        .await
    }

    async fn clear_text(&self, device_id: &str, element: &Element) -> Result<()> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.clear_text(&did, &el).await }
        })
        .await
    }

    async fn swipe(&self, device_id: &str, direction: Direction) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.swipe(&did, direction).await }
        })
        .await
    }

    async fn swipe_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.swipe_coords(&did, from, to).await }
        })
        .await
    }

    async fn drag(
        &self,
        device_id: &str,
        from: (i32, i32),
        to: (i32, i32),
        hold_ms: u64,
        move_ms: u64,
    ) -> Result<()> {
        // A gesture half done can't be replayed: once, like swipe_coords.
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.drag(&did, from, to, hold_ms, move_ms).await }
        })
        .await
    }

    async fn network_busy(&self, device_id: &str) -> bool {
        self.inner.network_busy(device_id).await
    }

    async fn add_mock(&self, device_id: &str, rule: crate::types::MockRule) -> Result<()> {
        self.inner.add_mock(device_id, rule).await
    }

    async fn clear_mocks(&self, device_id: &str) -> Result<()> {
        self.inner.clear_mocks(device_id).await
    }

    async fn system_state(&self, device_id: &str) -> Result<crate::types::SystemState> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.system_state(&did).await }
        })
        .await
    }

    async fn scroll(&self, device_id: &str, finger: Direction) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.scroll(&did, finger).await }
        })
        .await
    }

    async fn scroll_coords(&self, device_id: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.scroll_coords(&did, from, to).await }
        })
        .await
    }

    async fn press_key(&self, device_id: &str, key: Key) -> Result<()> {
        let did = device_id.to_string();
        let k = key.clone();
        self.with_once(device_id, || {
            let did = did.clone();
            let k = k.clone();
            let inner = self.inner.clone();
            async move { inner.press_key(&did, k).await }
        })
        .await
    }

    async fn tap_xy(&self, device_id: &str, x: i32, y: i32) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.tap_xy(&did, x, y).await }
        })
        .await
    }

    async fn input_raw(&self, device_id: &str, text: &str) -> Result<()> {
        let did = device_id.to_string();
        let t = text.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let t = t.clone();
            let inner = self.inner.clone();
            async move { inner.input_raw(&did, &t).await }
        })
        .await
    }

    async fn screenshot(&self, device_id: &str) -> Result<Vec<u8>> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.screenshot(&did).await }
        })
        .await
    }

    async fn screenshot_for_compare(&self, device_id: &str) -> Result<Vec<u8>> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.screenshot_for_compare(&did).await }
        })
        .await
    }

    async fn screen_size(&self, device_id: &str) -> Result<(i32, i32)> {
        let did = device_id.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.screen_size(&did).await }
        })
        .await
    }

    async fn reveal(&self, device_id: &str, element: &Element) -> Result<Element> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_retry(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.reveal(&did, &el).await }
        })
        .await
    }

    async fn get_element_text(&self, device_id: &str, element: &Element) -> Result<String> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_retry(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.get_element_text(&did, &el).await }
        })
        .await
    }

    async fn element_text_matches(
        &self,
        device_id: &str,
        element: &Element,
        expected: &str,
    ) -> Result<(bool, String)> {
        let did = device_id.to_string();
        let el = element.clone();
        let want = expected.to_string();
        self.with_retry(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let want = want.clone();
            let inner = self.inner.clone();
            async move { inner.element_text_matches(&did, &el, &want).await }
        })
        .await
    }

    async fn is_element_visible(&self, device_id: &str, element: &Element) -> Result<bool> {
        let did = device_id.to_string();
        let el = element.clone();
        self.with_retry(device_id, || {
            let did = did.clone();
            let el = el.clone();
            let inner = self.inner.clone();
            async move { inner.is_element_visible(&did, &el).await }
        })
        .await
    }

    async fn current_url(&self, device_id: &str) -> Option<String> {
        self.inner.current_url(device_id).await
    }

    async fn navigate(&self, device_id: &str, url: &str) -> Result<()> {
        let did = device_id.to_string();
        let u = url.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let u = u.clone();
            let inner = self.inner.clone();
            async move { inner.navigate(&did, &u).await }
        })
        .await
    }

    async fn go_back(&self, device_id: &str) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.go_back(&did).await }
        })
        .await
    }

    async fn refresh_page(&self, device_id: &str) -> Result<()> {
        let did = device_id.to_string();
        self.with_once(device_id, || {
            let did = did.clone();
            let inner = self.inner.clone();
            async move { inner.refresh_page(&did).await }
        })
        .await
    }
}

/// `AtomicU32::fetch_update` as a compare-exchange loop: that method is
/// deprecated in newer Rust (renamed `try_update`, which the MSRV lacks).
fn update(a: &AtomicU32, f: impl Fn(u32) -> Option<u32>) -> std::result::Result<u32, u32> {
    let mut cur = a.load(Ordering::SeqCst);
    loop {
        let Some(next) = f(cur) else { return Err(cur) };
        match a.compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(prev) => return Ok(prev),
            Err(seen) => cur = seen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DeviceInfo, Direction, Key, Selector};

    /// find_element, get_hierarchy and tap always fail with the error the
    /// function builds; every call is counted.
    struct FailingFind(
        fn() -> VelocityError,
        std::sync::atomic::AtomicUsize,
        Duration,
        /// restart_session calls.
        std::sync::atomic::AtomicUsize,
        /// touch_forced calls (it succeeds where tap fails).
        std::sync::atomic::AtomicUsize,
    );

    impl FailingFind {
        fn new(make: fn() -> VelocityError) -> Self {
            Self::slow(make, Duration::ZERO)
        }
        /// get_hierarchy takes `d` before failing.
        fn slow(make: fn() -> VelocityError, d: Duration) -> Self {
            Self(
                make,
                std::sync::atomic::AtomicUsize::new(0),
                d,
                std::sync::atomic::AtomicUsize::new(0),
                std::sync::atomic::AtomicUsize::new(0),
            )
        }
        fn calls(&self) -> usize {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn fail<T>(&self) -> Result<T> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err((self.0)())
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for FailingFind {
        async fn restart_session(&self) -> Result<()> {
            self.3.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![])
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
            self.fail()
        }
        async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
            Ok(vec![])
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            if !self.2.is_zero() {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(self.2).await;
                return Err((self.0)());
            }
            self.fail()
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
            self.fail()
        }
        async fn touch_forced(&self, _: &str, _: &Element, _: crate::traits::Touch) -> Result<()> {
            self.4.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
            Ok(())
        }
        async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
            Ok(())
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str, _: Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1, 1))
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            Ok(true)
        }
    }

    fn not_found() -> VelocityError {
        VelocityError::ElementNotFound {
            selector: "x".into(),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        }
    }

    fn connection_lost() -> VelocityError {
        VelocityError::AdbConnectionLost {
            device_id: "d".into(),
            reason: "gone".into(),
        }
    }

    fn fast(driver: FailingFind) -> ResilientDriver {
        ResilientDriver::new(Arc::new(driver))
            .with_retry_policy(RetryPolicy {
                max_retries: 0,
                initial_backoff: Duration::from_millis(1),
                ..RetryPolicy::default()
            })
            .with_breaker_reset(Duration::from_millis(50))
    }

    #[tokio::test]
    async fn actions_run_once_but_reads_are_retried() {
        let inner = Arc::new(FailingFind::new(connection_lost));
        let d = ResilientDriver::new(inner.clone()).with_retry_policy(RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            ..RetryPolicy::default()
        });
        let el = Element {
            hint: None,
            platform_id: String::new(),
            label: None,
            text: None,
            element_type: String::new(),
            bounds: crate::types::Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        assert!(d.tap("d", &el).await.is_err());
        assert_eq!(
            inner.calls(),
            1,
            "a tap that may have landed is never repeated"
        );
        let _ = d.get_hierarchy("d").await;
        assert!(inner.calls() > 2, "reads are retried on a lost connection");
    }

    #[tokio::test]
    async fn missing_elements_never_open_the_breaker() {
        let d = fast(FailingFind::new(not_found));
        for _ in 0..50 {
            let e = d.find_element("d", &Selector::Text("x".into())).await;
            assert!(matches!(e, Err(VelocityError::ElementNotFound { .. })));
        }
        assert!(!d.breaker("d").is_open());
    }

    #[tokio::test]
    async fn infrastructure_failures_still_open_it() {
        let d = fast(FailingFind::new(connection_lost));
        for _ in 0..10 {
            let _ = d.get_hierarchy("d").await;
        }
        assert!(d.breaker("d").is_open());
        // Once open, a call waits out the cooldown and probes once instead of
        // failing at once; a device that is still gone fails the probe with a
        // device error (exit 3) and the breaker opens again.
        let started = std::time::Instant::now();
        let e = d.get_hierarchy("d").await.unwrap_err();
        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "waited for the half-open probe"
        );
        assert_eq!(e.exit_code(), 3, "{e}");
        assert!(d.breaker("d").is_open());
    }

    #[tokio::test]
    async fn one_failing_device_does_not_block_another() {
        let d = fast(FailingFind::new(connection_lost));
        for _ in 0..10 {
            let _ = d.get_hierarchy("sick").await;
        }
        assert!(d.breaker("sick").is_open());
        assert!(!d.breaker("healthy").is_open());
    }

    // VEL-224: each attempt counted as a failure, so two slow reads opened
    // the breaker; and retries went on hitting a device another call had
    // already found dead.
    #[tokio::test]
    async fn the_breaker_counts_calls_and_stops_retries_once_open() {
        let inner = Arc::new(FailingFind::new(connection_lost));
        let d = ResilientDriver::new(inner.clone())
            .with_retry_policy(RetryPolicy {
                max_retries: 3,
                initial_backoff: Duration::from_millis(1),
                ..RetryPolicy::default()
            })
            .with_breaker_reset(Duration::from_secs(30));
        let _ = d.get_hierarchy("d").await;
        assert!(inner.calls() > 1, "the read was retried");
        assert_eq!(d.breaker("d").failure_count(), 1, "one failed call");

        let inner = Arc::new(FailingFind::new(connection_lost));
        let d = Arc::new(
            ResilientDriver::new(inner.clone())
                .with_retry_policy(RetryPolicy {
                    max_retries: 3,
                    initial_backoff: Duration::from_millis(80),
                    backoff_multiplier: 1.0,
                    ..RetryPolicy::default()
                })
                .with_breaker_reset(Duration::from_secs(30)),
        );
        let call = tokio::spawn({
            let d = d.clone();
            async move { d.get_hierarchy("d").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Other calls open the breaker while this one is backing off.
        for _ in 0..10 {
            d.breaker("d").on_failure();
        }
        let e = call.await.unwrap().unwrap_err();
        assert!(matches!(e, VelocityError::CircuitOpen { .. }), "{e}");
        assert_eq!(inner.calls(), 1, "no retry after the breaker opened");
    }

    // VEL-225: half-open let every caller through, not a bounded number of
    // probes.
    #[tokio::test]
    async fn half_open_admits_a_bounded_number_of_probes() {
        let inner = Arc::new(FailingFind::slow(
            connection_lost,
            Duration::from_millis(100),
        ));
        let d = Arc::new(
            ResilientDriver::new(inner.clone())
                .with_retry_policy(RetryPolicy {
                    max_retries: 0,
                    ..RetryPolicy::default()
                })
                .with_breaker_reset(Duration::from_millis(20)),
        );
        for _ in 0..10 {
            d.breaker("d").on_failure();
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        let calls: Vec<_> = (0..6)
            .map(|_| {
                let d = d.clone();
                tokio::spawn(async move { d.get_hierarchy("d").await })
            })
            .collect();
        for c in calls {
            let _ = c.await;
        }
        assert!(
            inner.calls() <= 3,
            "{} calls reached a half-open device",
            inner.calls()
        );
    }

    /// A breaker on device "d" that is half-open (probes allowed) now.
    async fn half_open(inner: Arc<FailingFind>) -> Arc<ResilientDriver> {
        let d = Arc::new(
            ResilientDriver::new(inner)
                .with_retry_policy(RetryPolicy {
                    max_retries: 0,
                    ..RetryPolicy::default()
                })
                .with_breaker_reset(Duration::from_millis(20)),
        );
        for _ in 0..10 {
            d.breaker("d").on_failure();
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        d
    }

    // M-1: a probe whose caller was cancelled (a timeout around the driver
    // call) kept its slot for 5 minutes; two of them locked the device out.
    #[tokio::test]
    async fn cancelled_probes_give_their_slot_back() {
        let inner = Arc::new(FailingFind::slow(connection_lost, Duration::from_secs(5)));
        let d = half_open(inner.clone()).await;
        for _ in 0..2 {
            let r = tokio::time::timeout(Duration::from_millis(30), d.get_hierarchy("d")).await;
            assert!(r.is_err(), "the probe was cancelled");
        }
        assert_eq!(inner.calls(), 2);
        assert!(!d.breaker("d").is_open(), "both slots are free again");
        let r = tokio::time::timeout(Duration::from_millis(30), d.get_hierarchy("d")).await;
        assert!(
            r.is_err(),
            "admitted as a probe, not refused with CircuitOpen"
        );
        assert_eq!(inner.calls(), 3, "a third probe reached the device");
    }

    // M-1: a caller refused because every half-open slot is taken restarted
    // the session under the probes still in flight.
    #[tokio::test]
    async fn a_full_half_open_breaker_never_restarts_the_session() {
        let inner = Arc::new(FailingFind::slow(
            connection_lost,
            Duration::from_millis(300),
        ));
        let d = half_open(inner.clone()).await;
        let probes: Vec<_> = (0..2)
            .map(|_| {
                let d = d.clone();
                tokio::spawn(async move { d.get_hierarchy("d").await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let e = d.get_hierarchy("d").await.unwrap_err();
        assert!(matches!(e, VelocityError::CircuitOpen { .. }), "{e}");
        assert_eq!(
            inner.3.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no restart_session while probes are in flight"
        );
        for p in probes {
            let _ = p.await;
        }
    }

    /// `force: true` reaches the device driver through this wrapper: the
    /// trait default would turn it back into a plain tap, which the inner
    /// driver's cover check refuses.
    #[tokio::test]
    async fn a_forced_touch_is_forwarded_not_turned_into_a_tap() {
        let inner = Arc::new(FailingFind::new(|| VelocityError::AssertionFailed {
            expected: "reachable".into(),
            actual: "covered".into(),
            selector: String::new(),
            screenshot: None,
        }));
        let d = ResilientDriver::new(inner.clone());
        d.touch_forced("d", &Element::default(), crate::traits::Touch::Tap)
            .await
            .expect("the forced touch reached the inner driver");
        assert_eq!(inner.4.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(inner.calls(), 0, "no plain tap");
    }

    #[test]
    fn breaker_clock_is_64_bit() {
        // A u32 millisecond clock wraps every 49.7 days.
        assert!(epoch_ms() > u64::from(u32::MAX));
    }
}
