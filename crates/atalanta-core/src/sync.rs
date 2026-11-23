//! Waiting for the UI to settle after an action.
//!
//! The policy (constants from Playwright, Maestro 2026, UiAutomator 2.4
//! `waitForStable` and Lighthouse; see `docs/research/PLAN.md`, A-1):
//!
//! ```text
//! after an action:
//!   reacted = the tree changed, or 300 ms passed with no change
//!   settled = reacted, then no change for 150 ms, sampled every 25 ms
//!             (sleeping exactly the quiet time still owed, not blindly)
//!   cap     = 3 s, then warn and continue: settle alone never fails a step
//! not settled while a loading spinner is visible that wasn't there before
//! ```
//!
//! The old rule — three identical trees, 200 ms apart and backing off — cost
//! about 1 s after every action even on a screen that never moved, and
//! 1.4 s on iOS where a read is ~60 ms.
//!
//! A screen confirmed settled with no action since is re-confirmed with a
//! single read (the pre-action wait of step N+1 right after step N's
//! post-action wait), however long ago that was: the read must show that
//! very screen (audit #5).

use std::time::Duration;
// tokio's clock: the real one, except under a paused test clock, where a
// settle's reads no longer depend on how busy the machine is.
use tokio::time::Instant;

use atalanta_common::{AtalantaError, Element, PlatformDriver, Result, SyncConfig};

use crate::tree_diff::TreeDiff;

/// How often the tree is sampled while waiting.
const SAMPLE: Duration = Duration::from_millis(25);

/// What the last wait did, for traces and warnings.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettleStats {
    pub duration: Duration,
    pub reads: u32,
    /// The tree changed after the action (vs. the reaction floor expiring).
    pub reacted: bool,
    /// Gave up at the cap.
    pub capped: bool,
    /// Ended on a screen seen to hold still: two identical trees in a row
    /// (bounds included), the device's own settle, or the native probe's
    /// idle answer that no late reaction overturned. An element read from
    /// it needs no second read to prove it isn't moving (D-3, review M4).
    pub stable: bool,
}

pub struct AdaptiveSyncEngine {
    config: SyncConfig,
    /// Hash of the last tree read: the pre-action screen, once an action ran.
    last_hash: Option<u64>,
    /// Loading spinners on the last settled screen (ones already there
    /// before an action don't hold up the next settle).
    last_spinners: usize,
    /// When an action last ran (set by [`Self::invalidate_tree_diff`]).
    action_at: Option<Instant>,
    /// The screen last confirmed settled, with no action since: a fresh
    /// read showing it again is settled at any age. It used to expire after
    /// 2 s of wall clock, so the wait after any slow step (a `wait:`, an
    /// iOS tap) paid the whole quiet window on an unchanged screen: ~2 s
    /// per occurrence on iOS (speed audit #5).
    settled_hash: Option<u64>,
    /// `last_hash` may not be the screen the next action starts from (a
    /// step that ran no settle, or a reaction cut short), so the reaction
    /// floor must not compare with it.
    picture_stale: bool,
    last_stats: SettleStats,
    /// Why the last wait gave up at the cap, in words (P9).
    busy: Option<String>,
}

impl AdaptiveSyncEngine {
    pub fn new(config: SyncConfig) -> Self {
        Self {
            config,
            last_hash: None,
            last_spinners: 0,
            action_at: None,
            settled_hash: None,
            picture_stale: false,
            last_stats: SettleStats::default(),
            busy: None,
        }
    }

    /// An action just ran: the next wait must see it through. The last tree
    /// stays as the "before" picture.
    pub fn invalidate_tree_diff(&mut self) {
        self.action_at = Some(Instant::now());
        self.settled_hash = None;
    }

    /// The screen may have changed since the last picture without a settle
    /// seeing it (a step with no action: a banner slid in while an
    /// assertion waited). The next reaction floor then compares with its
    /// own first read instead: a stale picture made it report "reacted" at
    /// once, before the action's handler had run (review-s8 m-d).
    pub fn forget_picture(&mut self) {
        self.picture_stale = true;
    }

    /// The wait after the last action ended without seeing it through (the
    /// probe was still busy at the cap): don't hold the next wait to that
    /// action's reaction floor, and don't trust the picture (s6/s7 m3).
    pub fn forget_action(&mut self) {
        self.action_at = None;
        self.picture_stale = true;
    }

    /// Native sync's reaction floor (P1): the in-app probe can answer "idle"
    /// before an action's reaction has started (XCTest's own quiescence is
    /// off, so a tap's call returns before the app has dispatched it). After
    /// an action, read the tree until it differs from the pre-action picture
    /// (or the first read, when there is none) or `reactionMs` since the
    /// action has passed. True when the screen changed: the reaction is
    /// under way and the probe must be asked again.
    pub async fn reaction_seen(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
    ) -> Result<bool> {
        let Some(at) = self.action_at.take() else {
            return Ok(false);
        };
        let react_by = at + Duration::from_millis(self.config.reaction()).min(self.cap());
        let before = self.last_hash.filter(|_| !self.picture_stale);
        let mut first = None;
        loop {
            let tree = driver.get_hierarchy_for_compare(device_id).await?;
            self.last_stats.reads += 1;
            let h = TreeDiff::hash_element(&tree);
            self.last_hash = Some(h);
            let base = *first.get_or_insert(before.unwrap_or(h));
            if h != base {
                // Mid-reaction: the settled picture comes later.
                self.picture_stale = true;
                self.last_stats.reacted = true;
                return Ok(true);
            }
            let now = Instant::now();
            if now >= react_by {
                self.picture_stale = false;
                return Ok(false);
            }
            tokio::time::sleep(SAMPLE.min(react_by - now)).await;
        }
    }

    /// Take the settled screen as the next action's "before" picture. After
    /// a reaction, [`reaction_seen`](Self::reaction_seen) kept a tree from
    /// mid-reaction; the next action's first read then always differed from
    /// it, so the floor stopped holding after the first step that reacted
    /// (review M4). One read, only for steps that reacted.
    pub async fn refresh_picture(&mut self, driver: &dyn PlatformDriver, device_id: &str) {
        if let Ok(tree) = driver.get_hierarchy_for_compare(device_id).await {
            self.last_stats.reads += 1;
            self.last_hash = Some(TreeDiff::hash_element(&tree));
            self.picture_stale = false;
        }
    }

    /// Get a reference to the sync config.
    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// What the last wait did.
    pub fn last_stats(&self) -> SettleStats {
        self.last_stats
    }

    /// Record a wait this engine didn't run (a native-sync wait).
    pub fn set_stats(&mut self, stats: SettleStats) {
        self.last_stats = stats;
    }

    /// Forget the last wait's stats (a native-sync wait doesn't run this
    /// engine, and must not report the previous polling wait).
    pub fn clear_stats(&mut self) {
        self.last_stats = SettleStats::default();
        self.busy = None;
    }

    /// Why the last wait hit the cap ("a loading spinner showing", the
    /// native probe's pending work), when it did.
    pub fn busy_reason(&self) -> Option<&str> {
        self.busy.as_deref()
    }

    /// Record why a wait this engine didn't run gave up.
    pub fn set_busy(&mut self, why: Option<String>) {
        self.busy = why;
    }

    /// Wait for the UI to settle.
    pub async fn wait_for_idle(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
    ) -> Result<()> {
        use tracing::Instrument;
        let start = Instant::now();
        let span = tracing::debug_span!("settle", reads = tracing::field::Empty);
        let result = self
            .settle(driver, device_id, start)
            .instrument(span.clone())
            .await;
        span.record("reads", self.last_stats.reads);
        if result.is_ok() && !self.last_stats.capped && self.last_stats.reads > 0 {
            // The last read is the settled screen.
            self.picture_stale = false;
        }
        self.last_stats.duration = start.elapsed();
        let st = self.last_stats;
        tracing::debug!(
            ms = st.duration.as_millis() as u64,
            reads = st.reads,
            reacted = st.reacted,
            capped = st.capped,
            "settle"
        );
        result
    }

    /// Same as [`Self::wait_for_idle`]; the key was used by a removed
    /// duration predictor and is kept for callers.
    pub async fn wait_for_idle_keyed(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        _key: &str,
    ) -> Result<()> {
        self.wait_for_idle(driver, device_id).await
    }

    pub fn config_mut(&mut self) -> &mut SyncConfig {
        &mut self.config
    }

    fn cap(&self) -> Duration {
        Duration::from_millis(self.config.settle_cap().min(self.config.timeout_ms))
    }

    async fn settle(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        start: Instant,
    ) -> Result<()> {
        self.last_stats = SettleStats::default();
        let quiet = Duration::from_millis(self.config.quiet());
        let reaction = Duration::from_millis(self.config.reaction());
        let deadline = start + self.cap();

        // Settled before and nothing ran since: one read showing the same
        // screen confirms it, at any age. The read is a fresh compare read
        // (never a lookup cache, see the trait), taken now, so an old
        // verdict can't vouch for a screen it never saw; any other tree
        // falls through to the full wait.
        if let Some(settled) = self.settled_hash.filter(|_| self.action_at.is_none()) {
            if !driver.network_busy(device_id).await {
                let tree = driver.get_hierarchy_for_compare(device_id).await?;
                self.last_stats.reads = 1;
                let hash = TreeDiff::hash_element(&tree);
                self.last_hash = Some(hash);
                if hash == settled {
                    self.last_stats.stable = true;
                    return Ok(());
                }
                self.settled_hash = None;
            }
        }

        // The device can watch for the settle itself (Android: accessibility
        // events): one call instead of a dozen tree reads, then one read so
        // the next wait has a "before" picture.
        //
        // A spinner turning doesn't raise the events it watches, so a
        // screen whose new spinner is all that moves reads as settled
        // there: that case goes on to the polling wait below, within what
        // is left of the cap.
        let mut spinner_hold = None;
        if self.action_at.is_some() {
            if let Some(r) = driver
                .settle_on_device(
                    device_id,
                    self.config.reaction(),
                    self.config.quiet(),
                    self.cap().as_millis() as u64,
                )
                .await
            {
                self.action_at = None;
                let tree = driver.get_hierarchy_for_compare(device_id).await?;
                self.last_stats.reads += 1;
                self.last_stats.reacted = r.reacted;
                self.last_hash = Some(TreeDiff::hash_element(&tree));
                let prior = self.last_spinners;
                self.last_spinners = count_spinners(&tree);
                if r.settled && self.last_spinners <= prior {
                    self.settled_hash = self.last_hash;
                    self.last_stats.stable = true;
                    return Ok(());
                }
                if !r.settled {
                    self.last_stats.capped = true;
                    return Err(AtalantaError::SyncTimeout {
                        timeout_ms: self.cap().as_millis() as u64,
                        stable_count: 0,
                        required: 1,
                    });
                }
                spinner_hold = Some(prior);
            }
        }

        let before = self.last_hash;
        let action_at = self.action_at.take();
        // Without an action to react to, the screen only needs to be quiet.
        let mut reacted = action_at.is_none();
        let react_by = action_at.unwrap_or(start) + reaction;
        let mut spinner_baseline = if action_at.is_some() {
            Some(self.last_spinners)
        } else {
            // After a device settle that a new spinner held up: the
            // spinners before the action are the baseline.
            spinner_hold
        };
        let mut last_change = Instant::now();
        let mut hash = before;
        self.busy = None;
        let mut changes = 0u32;

        loop {
            let tree = driver.get_hierarchy_for_compare(device_id).await?;
            let now = Instant::now();
            self.last_stats.reads += 1;
            let h = TreeDiff::hash_element(&tree);
            // The same tree as the read before: held still between the two.
            let same = hash == Some(h);
            if hash.is_some_and(|prev| prev != h) {
                last_change = now;
                changes += 1;
            }
            hash = Some(h);
            if !reacted && before.is_some_and(|b| b != h) {
                reacted = true;
                self.last_stats.reacted = true;
            }
            let spinners = count_spinners(&tree);
            let baseline = *spinner_baseline.get_or_insert(spinners);
            let loading = spinners > baseline;
            if loading {
                // Still loading: not quiet yet.
                last_change = now;
            }
            // Requests in flight (web): the screen will change when they
            // land, so it isn't quiet, and a request the action started is
            // its reaction.
            let requests = driver.network_busy(device_id).await;
            if requests {
                last_change = now;
                if !reacted {
                    reacted = true;
                    self.last_stats.reacted = true;
                }
            }
            if !reacted && now >= react_by {
                reacted = true;
            }
            let quiet_for = now.saturating_duration_since(last_change);
            if reacted && quiet_for >= quiet {
                self.last_hash = hash;
                self.last_spinners = spinners;
                self.settled_hash = hash;
                self.last_stats.stable = same;
                return Ok(());
            }
            if now >= deadline {
                self.last_hash = hash;
                self.last_spinners = spinners;
                self.last_stats.capped = true;
                self.busy = Some(if loading {
                    "a loading spinner still showing".to_string()
                } else if requests {
                    "network requests still in flight".to_string()
                } else if !reacted {
                    "no reaction to the action yet".to_string()
                } else {
                    format!(
                        "the screen changed {changes} times in {} reads",
                        self.last_stats.reads
                    )
                });
                return Err(AtalantaError::SyncTimeout {
                    timeout_ms: self.cap().as_millis() as u64,
                    stable_count: 0,
                    required: 1,
                });
            }
            // Waiting for a reaction: sample, to see it early. Confirming
            // quiet: sleep exactly the quiet time still owed.
            let nap = if reacted {
                (quiet - quiet_for).max(SAMPLE)
            } else {
                SAMPLE
            };
            tokio::time::sleep(nap.min(deadline.saturating_duration_since(now))).await;
        }
    }
}

/// Visible loading spinners (Maestro's "is loading" check): iOS activity
/// indicators (always indeterminate) and elements the driver marks
/// [`Element::indeterminate`] (Android: a `ProgressBar` with no range,
/// tree-server protocol 18). A plain Android `ProgressBar` may be a
/// determinate meter (battery level, a Wi-Fi scan bar that stays up), and
/// holding every settle on one cost the full cap on ordinary Settings
/// screens, so the class alone doesn't count. Only a spinner that appeared
/// after the action holds the settle, and never past its cap.
fn count_spinners(el: &Element) -> usize {
    let me = usize::from(
        el.visible
            && !el.bounds.is_empty()
            && (el.indeterminate || el.element_type.contains("ActivityIndicator")),
    );
    me + el.children.iter().map(count_spinners).sum::<usize>()
}

// Keep the old name as an alias for backwards compatibility in executor.rs
pub type SmartPollingSyncEngine = AdaptiveSyncEngine;

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::{DeviceInfo, Direction, Element, Key, Rect, Selector};

    fn hash_element(element: &Element) -> u64 {
        TreeDiff::hash_element(element)
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_element(text: &str) -> Element {
        Element {
            hint: None,
            platform_id: "el1".to_string(),
            label: None,
            text: Some(text.to_string()),
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn hash_same_elements_equal() {
        let a = make_element("hello");
        let b = make_element("hello");
        assert_eq!(hash_element(&a), hash_element(&b));
    }

    #[test]
    fn hash_different_elements_differ() {
        let a = make_element("hello");
        let b = make_element("world");
        assert_ne!(hash_element(&a), hash_element(&b));
    }

    struct MockDriver {
        call_count: AtomicUsize,
        elements: Vec<Element>,
        /// What `settle_on_device` answers (`None`: no device settle).
        device_settle: Option<atalanta_common::DeviceSettle>,
    }

    #[async_trait::async_trait]
    impl PlatformDriver for MockDriver {
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
            Ok(make_element("mock"))
        }
        async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
            Ok(vec![])
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let el = if idx < self.elements.len() {
                self.elements[idx].clone()
            } else {
                self.elements.last().unwrap().clone()
            };
            Ok(el)
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
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
            Ok((1080, 1920))
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            Ok(true)
        }
        async fn settle_on_device(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: u64,
        ) -> Option<atalanta_common::DeviceSettle> {
            self.device_settle
        }
    }

    fn driver(elements: Vec<Element>) -> MockDriver {
        MockDriver {
            call_count: AtomicUsize::new(0),
            elements,
            device_settle: None,
        }
    }

    fn reads(d: &MockDriver) -> usize {
        d.call_count.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn an_action_that_changes_nothing_settles_at_the_reaction_floor() {
        let d = driver(vec![make_element("same")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        engine.wait_for_idle(&d, "d").await.unwrap();
        let stats = engine.last_stats();
        assert!(!stats.reacted && !stats.capped);
        assert!(
            stats.duration >= Duration::from_millis(250)
                && stats.duration < Duration::from_millis(700),
            "{stats:?}"
        );
    }

    #[tokio::test]
    async fn a_reaction_settles_after_the_quiet_window() {
        // Before the action: "a". Right after it: "b", which then holds.
        let d = driver(vec![
            make_element("a"),
            make_element("a"),
            make_element("b"),
        ]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        engine.wait_for_idle(&d, "d").await.unwrap();
        let stats = engine.last_stats();
        assert!(stats.reacted, "{stats:?}");
        // Quiet window (150 ms), not the old 3 × 200 ms backoff.
        assert!(stats.duration < Duration::from_millis(400), "{stats:?}");
    }

    #[tokio::test]
    async fn a_screen_that_never_settles_stops_at_the_cap() {
        let d = driver(
            (0..1000)
                .map(|i| make_element(&format!("changing-{i}")))
                .collect(),
        );
        let config = SyncConfig {
            settle_cap_ms: Some(200),
            ..Default::default()
        };
        let mut engine = AdaptiveSyncEngine::new(config);
        let result = engine.wait_for_idle(&d, "d").await;
        assert!(matches!(result, Err(AtalantaError::SyncTimeout { .. })));
        let stats = engine.last_stats();
        assert!(stats.capped);
        assert!(stats.duration < Duration::from_millis(400), "{stats:?}");
        // P9: say why.
        let why = engine.busy_reason().unwrap_or_default();
        assert!(why.contains("screen changed"), "{why}");
    }

    #[tokio::test]
    async fn recently_settled_screen_confirms_with_one_read() {
        let d = driver(vec![make_element("stable")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        let first = reads(&d);
        assert!(first >= 2, "a cold wait needs a quiet window, got {first}");
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert_eq!(reads(&d) - first, 1, "a warm wait needs a single read");
    }

    #[tokio::test]
    async fn an_action_ends_the_carry_over() {
        let d = driver(vec![make_element("stable")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        let before = reads(&d);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert!(
            reads(&d) - before >= 2,
            "after an action the full wait runs"
        );
    }

    #[tokio::test]
    async fn carry_over_still_catches_a_change() {
        let mut els = vec![make_element("a"); 20];
        els.extend(vec![make_element("b"); 20]);
        let d = driver(els);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        // Skip ahead so the next read differs.
        d.call_count.store(20, Ordering::SeqCst);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert!(reads(&d) - 20 > 1, "a changed screen must be re-confirmed");
    }

    /// Audit #5: the carry-over expired after 2 s of wall clock, so the
    /// wait after a long `wait:` paid the whole quiet window again.
    #[tokio::test(start_paused = true)]
    async fn a_settled_screen_carries_over_at_any_age() {
        let d = driver(vec![make_element("stable")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        let before = reads(&d);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert_eq!(reads(&d) - before, 1, "one fresh read confirms it");
        let stats = engine.last_stats();
        assert!(stats.stable && stats.duration.is_zero(), "{stats:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_old_verdict_does_not_vouch_for_another_screen() {
        let mut els = vec![make_element("a"); 20];
        els.extend(vec![make_element("b"); 20]);
        let d = driver(els);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        d.call_count.store(20, Ordering::SeqCst);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert!(reads(&d) - 20 > 1, "a changed screen gets the full wait");
        // ...which then carries over in its turn.
        let before = reads(&d);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert_eq!(reads(&d) - before, 1);
    }

    fn with_spinner(text: &str) -> Element {
        let mut root = make_element(text);
        root.children.push(Element {
            element_type: "ActivityIndicator".into(),
            ..make_element("spinner")
        });
        root
    }

    #[tokio::test]
    async fn a_spinner_that_appears_after_the_action_holds_the_settle() {
        let d = driver(vec![
            make_element("form"),
            make_element("form"),
            with_spinner("form"),
        ]);
        let config = SyncConfig {
            settle_cap_ms: Some(500),
            ..Default::default()
        };
        let mut engine = AdaptiveSyncEngine::new(config);
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        let result = engine.wait_for_idle(&d, "d").await;
        assert!(result.is_err(), "still loading at the cap");
    }

    fn with_progress_bar(text: &str, indeterminate: bool) -> Element {
        let mut root = make_element(text);
        root.children.push(Element {
            element_type: "ProgressBar".into(),
            indeterminate,
            ..make_element("progress")
        });
        root
    }

    /// Review 09: an Android spinner (a ProgressBar with no range, marked
    /// by the tree server) holds the settle like an iOS activity
    /// indicator, up to the cap and no further; a determinate meter
    /// doesn't.
    #[tokio::test]
    async fn an_android_indeterminate_spinner_holds_the_settle_up_to_the_cap() {
        let mut screens = vec![make_element("form"), make_element("form")];
        screens.extend(vec![with_progress_bar("form", true); 1000]);
        let d = driver(screens);
        let config = SyncConfig {
            settle_cap_ms: Some(300),
            ..Default::default()
        };
        let mut engine = AdaptiveSyncEngine::new(config);
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        let result = engine.wait_for_idle(&d, "d").await;
        assert!(result.is_err(), "still loading at the cap");
        let stats = engine.last_stats();
        assert!(stats.capped, "{stats:?}");
        assert!(stats.duration < Duration::from_millis(700), "{stats:?}");
        let why = engine.busy_reason().unwrap_or_default();
        assert!(why.contains("spinner"), "{why}");
    }

    /// The device's event settle can't see a spinner turn: a new one sends
    /// the wait on to tree polling, still bounded by the cap.
    #[tokio::test]
    async fn a_device_settle_with_a_new_spinner_polls_on_to_the_cap() {
        let mut screens = vec![make_element("form"), make_element("form")];
        screens.extend(vec![with_progress_bar("form", true); 1000]);
        let mut d = driver(screens);
        let config = SyncConfig {
            settle_cap_ms: Some(300),
            ..Default::default()
        };
        let mut engine = AdaptiveSyncEngine::new(config);
        engine.wait_for_idle(&d, "d").await.unwrap();
        d.device_settle = Some(atalanta_common::DeviceSettle {
            settled: true,
            reacted: true,
            elapsed_ms: 10,
        });
        engine.invalidate_tree_diff();
        let before = reads(&d);
        let result = engine.wait_for_idle(&d, "d").await;
        assert!(result.is_err(), "still loading at the cap");
        assert!(reads(&d) - before > 2, "polled after the device settle");
        assert!(engine.last_stats().duration < Duration::from_millis(700));
    }

    #[tokio::test]
    async fn a_device_settle_without_a_new_spinner_needs_one_read() {
        let mut d = driver(vec![make_element("form")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        d.device_settle = Some(atalanta_common::DeviceSettle {
            settled: true,
            reacted: false,
            elapsed_ms: 10,
        });
        engine.invalidate_tree_diff();
        let before = reads(&d);
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert_eq!(reads(&d) - before, 1);
    }

    #[tokio::test]
    async fn an_android_determinate_meter_does_not_hold_the_settle() {
        let mut screens = vec![make_element("form"), make_element("form")];
        screens.extend(vec![with_progress_bar("form", false); 1000]);
        let d = driver(screens);
        let config = SyncConfig {
            settle_cap_ms: Some(2_000),
            ..Default::default()
        };
        let mut engine = AdaptiveSyncEngine::new(config);
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert!(!engine.last_stats().capped);
    }

    #[tokio::test]
    async fn a_spinner_already_on_screen_does_not_hold_the_settle() {
        // A permanent progress bar (e.g. a points meter) must not cost the
        // cap on every step.
        let d = driver(vec![with_spinner("home")]);
        let mut engine = AdaptiveSyncEngine::new(SyncConfig::default());
        engine.wait_for_idle(&d, "d").await.unwrap();
        engine.invalidate_tree_diff();
        engine.wait_for_idle(&d, "d").await.unwrap();
        assert!(!engine.last_stats().capped);
    }
}
