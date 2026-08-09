use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::info;
use velocity_common::{
    Element, HealingConfig, Platform, PlatformDriver, Result, Selector, VelocityError,
};

use crate::healing::SelectorHealer;

const MAX_CACHE_SIZE: usize = 256;
const DEFAULT_TTL: Duration = Duration::from_secs(30);

pub struct SelectorEngine {
    cache: HashMap<String, CachedElement>,
    generation: AtomicU64,
    max_size: usize,
    ttl: Duration,
    healer: SelectorHealer,
    /// Heals since the last [`take_heal_notes`](Self::take_heal_notes), for the
    /// test report — a healed lookup passes, but must not pass silently.
    heal_notes: Vec<String>,
    /// The last heal: (the selector that failed, the one it healed to).
    last_heal: Option<(Selector, Selector)>,
    /// Heals made so far: a caller compares it before and after a lookup to
    /// tell whether that lookup healed (review M1).
    heals: u64,
}

#[derive(Clone)]
struct CachedElement {
    element: Element,
    cached_at: Instant,
    source_generation: u64,
}

impl SelectorEngine {
    pub fn new() -> Self {
        Self::with_healing(HealingConfig::default())
    }

    pub fn with_healing(config: HealingConfig) -> Self {
        Self::with_healer(crate::healing::HealingConfig {
            enabled: config.enabled && config.mode != Some(velocity_common::HealingMode::Off),
            confidence_threshold: config.confidence_threshold,
            persist_healed: config.persist_healed,
            ..Default::default()
        })
    }

    /// Healing for one app on one platform, with element snapshots kept in
    /// `.velocity/` between runs when `persist_healed` is on.
    pub fn with_healing_for(config: HealingConfig, platform: Platform, app_id: &str) -> Self {
        Self::with_healing_in(config, platform, app_id, std::path::Path::new(".velocity"))
    }

    /// [`Self::with_healing_for`] keeping the healer's files (healed
    /// mappings, element snapshots, rejected heals) in `state_dir` instead
    /// of `./.velocity` (tests: a temp dir, never the source tree).
    pub fn with_healing_in(
        config: HealingConfig,
        platform: Platform,
        app_id: &str,
        state_dir: &std::path::Path,
    ) -> Self {
        let persist = config.persist_healed;
        let rejected = std::path::Path::new(crate::healing::REJECTED_FILE)
            .file_name()
            .map(|f| state_dir.join(f));
        Self::with_healer(crate::healing::HealingConfig {
            // `healing.mode: off` (D-7) is `enabled: false`.
            enabled: config.enabled && config.mode != Some(velocity_common::HealingMode::Off),
            confidence_threshold: config.confidence_threshold,
            persist_healed: persist,
            persist_path: Some(
                state_dir
                    .join("healed-selectors.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            snapshot_path: persist.then(|| state_dir.join("healer-snapshots.json")),
            platform: Some(platform),
            scope: app_id.to_string(),
            rejected_path: rejected,
        })
    }

    fn with_healer(healer_config: crate::healing::HealingConfig) -> Self {
        Self {
            cache: HashMap::with_capacity(64),
            generation: AtomicU64::new(0),
            max_size: MAX_CACHE_SIZE,
            ttl: DEFAULT_TTL,
            healer: SelectorHealer::new(healer_config),
            heal_notes: Vec::new(),
            last_heal: None,
            heals: 0,
        }
    }

    /// Increment the generation counter, invalidating all cached entries
    /// that don't match the new generation.
    pub fn invalidate_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Clear the entire cache. Use after app lifecycle events
    /// (launch, stop, screen transitions).
    pub fn invalidate_cache(&mut self) {
        self.cache.clear();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub async fn find_element(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Element> {
        self.find_element_inner(driver, device_id, selector, true)
            .await
    }

    /// Find an element without self-healing fallback. Assertions and
    /// scroll-until-visible must use this: a healed near-match would invert
    /// `assertNotVisible` (heal "Sign In" onto "Sign Up" and report it visible)
    /// and short-circuit scrolling before the real element is on screen.
    pub async fn find_element_no_heal(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Element> {
        self.find_element_inner(driver, device_id, selector, false)
            .await
    }

    async fn find_element_inner(
        &mut self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        selector: &Selector,
        allow_healing: bool,
    ) -> Result<Element> {
        // Per tier ceiling: a loose hit must not answer a stricter lookup.
        let cache_key = format!(
            "{selector}#{:?}",
            velocity_common::selector_match::ceiling()
        );
        let current_gen = self.generation.load(Ordering::SeqCst);

        // Check cache first
        if let Some(cached) = self.cache.get(&cache_key) {
            if self.is_valid(cached, current_gen) {
                // Verify the cached element still exists
                if driver
                    .is_element_visible(device_id, &cached.element)
                    .await
                    .unwrap_or(false)
                {
                    return Ok(cached.element.clone());
                }
                // Element no longer valid, remove from cache
                self.cache.remove(&cache_key);
            } else {
                self.cache.remove(&cache_key);
            }
        }

        // Full tree query
        let result = driver.find_element(device_id, selector).await;

        match result {
            Ok(element) => {
                // Record successful find for future healing reference — with
                // the surrounding hierarchy when the snapshot is missing or
                // stale (rare), so neighbour/parent context is available.
                if self.healer.needs_snapshot(selector, &element) {
                    match driver.get_hierarchy_cached(device_id).await {
                        Ok(tree) => self
                            .healer
                            .record_success_with_context(selector, &element, &tree),
                        Err(_) => self.healer.record_success(selector, &element),
                    }
                }
                if self
                    .last_heal
                    .as_ref()
                    .is_some_and(|(from, _)| from == selector)
                {
                    self.last_heal = None;
                }
                self.cache_element(&cache_key, element.clone(), current_gen);
                Ok(element)
            }
            // Only a missing element is a reason to heal. A lost device or a
            // failed driver call must surface as itself: reporting it as
            // "element not found" blamed the app for the device, and kept
            // it out of quarantine.
            Err(e) if !matches!(e, VelocityError::ElementNotFound { .. }) => Err(e),
            Err(_) => {
                // Primary lookup failed — attempt self-healing
                if !allow_healing {
                    return Err(VelocityError::ElementNotFound {
                        selector: format!("{selector}"),
                        timeout_ms: 0,
                        screenshot: None,
                        hierarchy_snapshot: None,
                    });
                }
                if let Some((healed_element, confidence, healed)) =
                    self.healer.try_heal(driver, device_id, selector).await
                {
                    info!(
                        selector = %velocity_common::redact::redact(&selector.to_string()),
                        confidence = confidence,
                        healed_type = healed_element.element_type,
                        healed_label = healed_element.label.as_deref().unwrap_or(""),
                        "selector healed: using alternate element"
                    );
                    self.heal_notes.push(format!(
                        "selector {selector} not found; healed to {healed} (confidence {confidence:.2}) \
                         — update the selector",
                    ));
                    self.last_heal = Some((selector.clone(), healed));
                    self.heals += 1;
                    self.cache_element(&cache_key, healed_element.clone(), current_gen);
                    Ok(healed_element)
                } else {
                    Err(VelocityError::ElementNotFound {
                        selector: format!("{selector}"),
                        timeout_ms: 0,
                        screenshot: None,
                        hierarchy_snapshot: None,
                    })
                }
            }
        }
    }

    /// Persist any healed mappings to disk (call at end of test run).
    pub fn persist_healed_mappings(&self) -> Result<()> {
        self.healer.persist()
    }

    /// Save element snapshots and healed mappings (best-effort).
    pub fn persist(&self) {
        self.healer.persist_snapshots();
        if let Err(e) = self.healer.persist() {
            tracing::warn!("self-healing: {e}");
        }
    }

    /// The heal notes collected since the last drain, without draining.
    pub fn heal_notes(&self) -> &[String] {
        &self.heal_notes
    }

    /// Drain the heal notes collected since the last call.
    /// What `selector` was last healed to, if it was (N-3: the
    /// actionability wait must re-query the element the heal found, not the
    /// selector that just failed).
    pub fn healed_to(&self, selector: &Selector) -> Option<Selector> {
        self.last_heal
            .as_ref()
            .filter(|(from, _)| from == selector)
            .map(|(_, to)| to.clone())
    }

    /// How many lookups have healed so far.
    pub fn heal_count(&self) -> u64 {
        self.heals
    }

    /// The last heal was proposed, not used (`healing.mode: report`): drop
    /// its "healed to … — update the selector" note (the step's failure
    /// says it instead; a failed test is not "healed", m5), forget it as
    /// the selector's stand-in, and drop the cached element it found. The
    /// mapping stays with the healer, as the patch `velocity heal` applies.
    pub fn unuse_last_heal(&mut self) {
        self.heal_notes.pop();
        self.last_heal = None;
        self.invalidate_cache();
    }

    pub fn take_heal_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.heal_notes)
    }

    /// Get access to the healer for reporting.
    pub fn healer(&self) -> &SelectorHealer {
        &self.healer
    }

    fn cache_element(&mut self, cache_key: &str, element: Element, current_gen: u64) {
        if self.cache.len() >= self.max_size {
            self.evict_oldest();
        }
        self.cache.insert(
            cache_key.to_string(),
            CachedElement {
                element,
                cached_at: Instant::now(),
                source_generation: current_gen,
            },
        );
    }

    fn is_valid(&self, cached: &CachedElement, current_gen: u64) -> bool {
        cached.source_generation == current_gen && cached.cached_at.elapsed() < self.ttl
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest_key) = self
            .cache
            .iter()
            .min_by_key(|(_, v)| v.cached_at)
            .map(|(k, _)| k.clone())
        {
            self.cache.remove(&oldest_key);
        }
    }
}

impl Default for SelectorEngine {
    fn default() -> Self {
        Self::new()
    }
}
