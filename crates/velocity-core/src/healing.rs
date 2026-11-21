use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use velocity_common::{Element, Platform, PlatformDriver, Rect, Result, Selector, VelocityError};

/// Configuration for self-healing selectors.
#[derive(Debug, Clone)]
pub struct HealingConfig {
    /// Whether self-healing is enabled.
    pub enabled: bool,
    /// Minimum confidence (0.0–1.0) required to accept a healed match.
    pub confidence_threshold: f64,
    /// Whether to persist healed selector mappings.
    pub persist_healed: bool,
    /// Path to write healed mappings (if persist_healed is true).
    pub persist_path: Option<String>,
    /// Where element snapshots are kept between runs. Without it the healer
    /// only knows elements found earlier in the same process, so a selector
    /// that breaks on a fresh run has nothing to compare against.
    pub snapshot_path: Option<PathBuf>,
    /// Platform under test: decides whether `platform_id` is a stable id
    /// (Android resource-id) and how far "nearby" is in pixels.
    pub platform: Option<Platform>,
    /// Separates snapshots of different apps sharing one snapshot file.
    pub scope: String,
    /// Heals the user rejected (`velocity heal --reject`): never proposed
    /// again (D-7).
    pub rejected_path: Option<PathBuf>,
}

impl Default for HealingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            confidence_threshold: 0.8,
            persist_healed: true,
            persist_path: None,
            snapshot_path: None,
            platform: None,
            scope: String::new(),
            rejected_path: None,
        }
    }
}

/// Where `velocity heal --reject` keeps the rejected heals.
pub const REJECTED_FILE: &str = ".velocity/heal-rejected.json";

/// A heal the user rejected: this selector is never healed to that one
/// again (`.velocity/heal-rejected.json`, D-7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedHeal {
    pub original_selector: String,
    pub healed_selector: String,
    /// The app the heal was rejected for (S5): it is never proposed again
    /// for that app, while another app with the same selector still heals.
    /// `None` (an entry written before S5) applies to every app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_at: Option<String>,
}

/// The rejected heals in `path` (none when it is missing or unreadable).
pub fn load_rejected(path: &std::path::Path) -> Vec<RejectedHeal> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Add `mappings` to the rejected heals in `path`; returns how many were
/// new.
pub fn reject_heals(path: &std::path::Path, mappings: &[HealedMapping]) -> Result<usize> {
    let mut list = load_rejected(path);
    let now = chrono::Utc::now().to_rfc3339();
    let mut added = 0;
    for m in mappings {
        let app_id = m.app_id.clone().filter(|a| !a.is_empty());
        let known = list.iter().any(|r| {
            r.original_selector == m.original_selector
                && r.healed_selector == m.healed_selector
                && (r.app_id.is_none() || r.app_id == app_id)
        });
        if !known {
            list.push(RejectedHeal {
                original_selector: m.original_selector.clone(),
                healed_selector: m.healed_selector.clone(),
                app_id,
                rejected_at: Some(now.clone()),
            });
            added += 1;
        }
    }
    velocity_common::redact::redact_serde(&mut list);
    let json =
        serde_json::to_string_pretty(&list).map_err(|e| VelocityError::io("heal-rejected", e))?;
    velocity_common::fsutil::write_state(path, json.as_bytes())
        .map_err(|e| VelocityError::io(format!("Failed to write {}", path.display()), e))?;
    Ok(added)
}

/// A record of a healed selector match.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HealedMapping {
    /// The original selector that failed.
    pub original_selector: String,
    /// The healed selector that was found.
    pub healed_selector: String,
    /// Confidence score for the match.
    pub confidence: f64,
    /// Attributes of the matched element.
    pub matched_attributes: MatchedAttributes,
    /// Timestamp of when the healing occurred.
    pub healed_at: String,
    /// The test that healed it. Set once that test passes; heals from a
    /// failing test are dropped (a heal is promoted only after the test
    /// it rescued passed, as mabl does).
    #[serde(default)]
    pub test: Option<String>,
    /// The app under test when it healed (the healer's scope), so a
    /// rejection applies to that app only (S5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

impl HealedMapping {
    /// The fix in the user's own selector form: a `text:` selector stays a
    /// `text:` selector with the element's new text (Healenium writes heals
    /// back the same way), instead of switching to whatever attribute the
    /// healer matched on. `None` when the original isn't a single-attribute
    /// selector or the element lacks that attribute.
    pub fn rewrite(&self) -> Option<(String, String, String)> {
        let (key, old) = parse_simple_selector(&self.original_selector)?;
        let m = &self.matched_attributes;
        let new = match key.as_str() {
            "text" | "textContains" => m.text.clone().or_else(|| m.label.clone()),
            "accessibilityId" => m.label.clone(),
            "id" => (!m.platform_id.is_empty()).then(|| {
                // Keep the short form if that's what the user wrote.
                match m.platform_id.rsplit_once(":id/") {
                    Some((_, short)) if !old.contains(":id/") => short.to_string(),
                    _ => m.platform_id.clone(),
                }
            }),
            "className" => Some(m.element_type.clone()),
            _ => None,
        }?;
        (new != old).then_some((key, old, new))
    }
}

/// `text="Log in"` (a selector's display form) → ("text", "Log in").
pub fn parse_simple_selector(s: &str) -> Option<(String, String)> {
    let (key, value) = s.split_once('=')?;
    if !matches!(
        key,
        "id" | "text" | "textContains" | "accessibilityId" | "className"
    ) {
        return None;
    }
    let value: String = serde_json::from_str(value).ok()?;
    Some((key.to_string(), value))
}

/// Attributes of an element that was matched during healing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MatchedAttributes {
    pub platform_id: String,
    pub label: Option<String>,
    pub text: Option<String>,
    pub element_type: String,
    pub bounds: [i32; 4],
}

impl From<&Element> for MatchedAttributes {
    fn from(el: &Element) -> Self {
        Self {
            platform_id: el.platform_id.clone(),
            label: el.label.clone(),
            text: el.text.clone(),
            element_type: el.element_type.clone(),
            bounds: [el.bounds.x, el.bounds.y, el.bounds.width, el.bounds.height],
        }
    }
}

/// A scored candidate element during healing.
#[derive(Debug)]
struct ScoredCandidate {
    element: Element,
    score: f64,
}

/// Self-healing selector engine that attempts to find elements
/// when the original selector fails by computing similarity scores
/// against all visible elements in the hierarchy.
pub struct SelectorHealer {
    config: HealingConfig,
    /// Previous element snapshots keyed by selector string.
    known_elements: Mutex<HashMap<String, ElementSnapshot>>,
    /// Accumulated healed mappings for persistence.
    healed_mappings: Mutex<Vec<HealedMapping>>,
    /// Tests this healer has seen end: their entries in the healed file are
    /// replaced by this healer's, everyone else's are kept.
    settled_tests: Mutex<std::collections::HashSet<String>>,
    /// Snapshots changed since load/save.
    dirty: AtomicBool,
    /// (original, healed) selector pairs never to propose again for this
    /// healer's app (S5: entries rejected for another app don't apply).
    rejected: Vec<(String, String)>,
}

/// Snapshot of an element's attributes plus surrounding context for fingerprinting.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ElementSnapshot {
    platform_id: String,
    label: Option<String>,
    text: Option<String>,
    element_type: String,
    bounds: Rect,
    /// Contextual fingerprint: parent type and sibling labels for structural matching.
    fingerprint: ElementFingerprint,
}

/// Captures structural context around an element for resilient matching.
/// Even if text/ID changes, position within the tree often stays stable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ElementFingerprint {
    /// Parent element type (e.g., "Cell", "ScrollView").
    parent_type: Option<String>,
    /// Parent accessibility label.
    parent_label: Option<String>,
    /// Sibling element types (sorted for stable comparison).
    sibling_types: Vec<String>,
    /// Sibling labels/text (sorted, up to 5 nearest) — Similo's "neighbour text".
    sibling_labels: Vec<String>,
    /// Index among siblings of the same type.
    type_index: Option<usize>,
}

impl ElementFingerprint {
    /// True when no structural context was captured (plain `record_success`).
    /// An empty fingerprint must not participate in scoring, otherwise its
    /// weight acts as a flat confidence penalty on every snapshot comparison.
    fn is_empty(&self) -> bool {
        self.parent_type.is_none()
            && self.parent_label.is_none()
            && self.sibling_types.is_empty()
            && self.type_index.is_none()
    }
}

impl From<&Element> for ElementSnapshot {
    fn from(el: &Element) -> Self {
        Self {
            platform_id: el.platform_id.clone(),
            label: el.label.clone(),
            text: el.text.clone(),
            element_type: el.element_type.clone(),
            bounds: el.bounds,
            fingerprint: ElementFingerprint::default(),
        }
    }
}

impl ElementSnapshot {
    /// Create a snapshot with structural context extracted from the hierarchy.
    fn with_context(element: &Element, hierarchy: &Element) -> Self {
        let fingerprint = extract_fingerprint(element, hierarchy);
        Self {
            platform_id: element.platform_id.clone(),
            label: element.label.clone(),
            text: element.text.clone(),
            element_type: element.element_type.clone(),
            bounds: element.bounds,
            fingerprint,
        }
    }
}

/// Extract fingerprint by finding the element's parent and siblings in the hierarchy.
fn extract_fingerprint(target: &Element, root: &Element) -> ElementFingerprint {
    if let Some((parent, siblings)) = find_parent_and_siblings(target, root) {
        let mut sibling_types: Vec<String> =
            siblings.iter().map(|s| s.element_type.clone()).collect();
        sibling_types.sort();

        let mut sibling_labels: Vec<String> = siblings
            .iter()
            .filter_map(|s| s.label.clone().or_else(|| s.text.clone()))
            .take(5)
            .collect();
        sibling_labels.sort();

        // Index among same-type children of the parent, counting the target itself —
        // `siblings` excludes it, so it must be computed over parent.children.
        let type_index = parent
            .children
            .iter()
            .filter(|c| c.element_type == target.element_type)
            .position(|c| c.platform_id == target.platform_id && c.bounds == target.bounds);

        ElementFingerprint {
            parent_type: Some(parent.element_type.clone()),
            parent_label: parent.label.clone(),
            sibling_types,
            sibling_labels,
            type_index,
        }
    } else {
        ElementFingerprint::default()
    }
}

/// Walk the tree to find the parent of a target element and its siblings.
fn find_parent_and_siblings<'a>(
    target: &Element,
    current: &'a Element,
) -> Option<(&'a Element, Vec<&'a Element>)> {
    for child in &current.children {
        // Check if this child matches the target
        if child.platform_id == target.platform_id && child.bounds == target.bounds {
            let siblings: Vec<&Element> = current
                .children
                .iter()
                .filter(|c| !(c.platform_id == target.platform_id && c.bounds == target.bounds))
                .collect();
            return Some((current, siblings));
        }
        // Recurse
        if let Some(found) = find_parent_and_siblings(target, child) {
            return Some(found);
        }
    }
    None
}

impl SelectorHealer {
    pub fn new(config: HealingConfig) -> Self {
        let known = match (&config.snapshot_path, config.enabled) {
            (Some(path), true) => SnapshotFile::load(path).entries,
            _ => HashMap::new(),
        };
        Self {
            known_elements: Mutex::new(known),
            healed_mappings: Mutex::new(Vec::new()),
            settled_tests: Mutex::new(Default::default()),
            dirty: AtomicBool::new(false),
            rejected: config
                .rejected_path
                .as_deref()
                .map(load_rejected)
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    r.app_id.is_none()
                        || config.scope.is_empty()
                        || r.app_id.as_deref() == Some(config.scope.as_str())
                })
                .map(|r| (r.original_selector, r.healed_selector))
                .collect(),
            config,
        }
    }

    fn key(&self, selector: &Selector) -> String {
        let platform = self
            .config
            .platform
            .map(|p| p.to_string())
            .unwrap_or_default();
        // Keyed by the selector with secrets masked: an interpolated
        // password must not end up in the snapshot file (VEL-065).
        let selector = selector.to_string();
        let selector = velocity_common::redact::redact(&selector);
        format!("{platform}|{}|{selector}", self.config.scope)
    }

    /// Whether `element`, just found by `selector`, should be (re)recorded with
    /// hierarchy context: no snapshot yet, a contextless one, or the element
    /// changed. Steady state is `false`, so recording costs no extra dumps.
    pub fn needs_snapshot(&self, selector: &Selector, element: &Element) -> bool {
        if !self.config.enabled {
            return false;
        }
        let Ok(known) = self.known_elements.lock() else {
            return false;
        };
        match known.get(&self.key(selector)) {
            None => true,
            Some(snap) => {
                let (sx, sy) = snap.bounds.center();
                let (ex, ey) = element.bounds.center();
                snap.fingerprint.is_empty()
                    || snap.text != element.text
                    || snap.label != element.label
                    || snap.element_type != element.element_type
                    || (sx - ex).abs() + (sy - ey).abs() > 50
            }
        }
    }

    /// Record a successful element find so we have attributes for future healing.
    pub fn record_success(&self, selector: &Selector, element: &Element) {
        if !self.config.enabled {
            return;
        }
        let key = self.key(selector);
        if let Ok(mut known) = self.known_elements.lock() {
            // Never replace a snapshot that has context with one that doesn't.
            if known.get(&key).is_some_and(|s| !s.fingerprint.is_empty()) {
                return;
            }
            known.insert(key, ElementSnapshot::from(element));
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Record a successful element find with full hierarchy context for fingerprinting.
    pub fn record_success_with_context(
        &self,
        selector: &Selector,
        element: &Element,
        hierarchy: &Element,
    ) {
        if !self.config.enabled {
            return;
        }
        let key = self.key(selector);
        if let Ok(mut known) = self.known_elements.lock() {
            known.insert(key, ElementSnapshot::with_context(element, hierarchy));
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Attempt to heal a failed selector by searching the hierarchy.
    ///
    /// Returns the healed element, its confidence score and the healed
    /// selector, or None if no match above the confidence threshold was
    /// found.
    pub async fn try_heal(
        &self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        selector: &Selector,
    ) -> Option<(Element, f64, Selector)> {
        if !self.config.enabled {
            return None;
        }

        let hierarchy = match driver.get_hierarchy(device_id).await {
            Ok(h) => h,
            Err(_) => return None,
        };
        let screen = driver
            .screen_size(device_id)
            .await
            .ok()
            .map(|(width, height)| Rect {
                x: 0,
                y: 0,
                width,
                height,
            });
        self.heal_in_screen(selector, &hierarchy, screen.as_ref())
    }

    /// Find the best stand-in for `selector` in `hierarchy`, if one clearly
    /// qualifies. Pure: no device access, so it's directly testable.
    #[cfg(test)]
    fn heal_in(
        &self,
        selector: &Selector,
        hierarchy: &Element,
    ) -> Option<(Element, f64, Selector)> {
        self.heal_in_screen(selector, hierarchy, None)
    }

    /// [`heal_in`](Self::heal_in) among what is on `screen` only (m-11):
    /// iOS reports off-screen list rows, which were candidates, and a twin
    /// of the target below the fold blocked a heal as "ambiguous".
    fn heal_in_screen(
        &self,
        selector: &Selector,
        hierarchy: &Element,
        screen: Option<&Rect>,
    ) -> Option<(Element, f64, Selector)> {
        if !self.config.enabled {
            return None;
        }
        // Logged at the default level: never with a secret in it (VEL-066).
        let selector_str = velocity_common::redact::redact(&selector.to_string()).into_owned();
        let platform = self.config.platform;

        // Flatten the hierarchy into visible leaf/actionable elements
        let mut candidates = Vec::new();
        flatten_elements(hierarchy, screen, &mut candidates);

        if candidates.is_empty() {
            return None;
        }

        // An `index:` or a relation whose plain selector still matches means
        // the element is there but not where the test said (fewer rows than
        // `index`, an Edit above Shipping rather than below it). Healing onto
        // the match would drop the constraint and act on the wrong row.
        if constraint_unmet(selector, hierarchy) {
            warn!(
                selector = %selector_str,
                "self-healing: the selector matches but its index or relation doesn't hold; not healing"
            );
            return None;
        }

        // Score each candidate
        let known = self.known_elements.lock().ok()?;
        let snapshot = known.get(&self.key(selector));

        let mut scored: Vec<ScoredCandidate> = candidates
            .into_iter()
            .filter(|el| keeps_constraints(selector, el, hierarchy, snapshot))
            .map(|el| {
                let score = compute_similarity(selector, snapshot, &el, Some(hierarchy), platform);
                ScoredCandidate { element: el, score }
            })
            .filter(|sc| sc.score > 0.0)
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut ranked = scored.into_iter();
        let best = ranked.next()?;

        // A heal is only trusted when it clearly leads: two near-equal
        // candidates (e.g. a list of identical "Add" buttons) means the score
        // can't tell them apart, and picking one is a coin flip that passes
        // the step on the wrong element.
        if let Some(runner_up) = ranked.next() {
            // Same bounds = the same on-screen thing (e.g. a label and its
            // clickable wrapper), not a real ambiguity.
            let distinct = runner_up.element.bounds != best.element.bounds;
            if distinct && best.score - runner_up.score < AMBIGUITY_MARGIN {
                warn!(
                    selector = %selector_str,
                    best = best.score,
                    runner_up = runner_up.score,
                    "self-healing: ambiguous match, not healing"
                );
                return None;
            }
        }

        if best.score < self.config.confidence_threshold {
            warn!(
                selector = %selector_str,
                best_score = best.score,
                threshold = self.config.confidence_threshold,
                "self-healing: best match below confidence threshold"
            );
            return None;
        }

        // Heal by substitution (Healenium's rule): put the healed leaf back
        // into the test's selector and run the real matcher over the screen.
        // Only a selector that then singles out this element is a heal; an
        // `index:` or relation the winner doesn't satisfy is not dropped.
        let (healed, element) = match substitute(selector, &best.element, platform) {
            // The selector already names this element: it wasn't missing but
            // hidden or not actionable (an iPhone 12, 2026-11-21: `id="Edit"`
            // behind a sheet "healed" to `id="Edit"`, and the step waited
            // its timeout again under it). Nothing to heal.
            Some(healed) if healed.to_string() == selector.to_string() => {
                debug!(
                    selector = %selector_str,
                    "self-healing: the match is the selector's own element; not healing"
                );
                return None;
            }
            Some(healed) => match singles_out(hierarchy, &healed, &best.element, screen) {
                Some(found) => (healed, found),
                None => {
                    warn!(
                        selector = %selector_str,
                        healed = %velocity_common::redact::redact(&healed.to_string()),
                        "self-healing: the healed selector doesn't single out the match; not healing"
                    );
                    return None;
                }
            },
            // A CSS/XPath selector has no form to rewrite: the old rule.
            None if matches!(selector, Selector::Css(_) | Selector::XPath(_)) => {
                (selector.clone(), best.element.clone())
            }
            None => return None,
        };
        let healed_selector = healed.to_string();
        // Rejected by the user (`velocity heal --reject`): never again.
        let original = selector.to_string();
        if self
            .rejected
            .iter()
            .any(|(from, to)| *from == original && *to == healed_selector)
        {
            warn!(
                selector = %selector_str,
                healed = %velocity_common::redact::redact(&healed_selector),
                "self-healing: this heal was rejected (heal-rejected.json); not healing"
            );
            return None;
        }
        info!(
            original = %selector_str,
            healed = %velocity_common::redact::redact(&healed_selector),
            confidence = best.score,
            "self-healing: selector healed successfully"
        );

        // Record the healing
        drop(known); // release lock before acquiring another
        self.record_healing(
            &selector.to_string(),
            &healed_selector,
            best.score,
            &element,
        );

        Some((element, best.score, healed))
    }

    fn record_healing(&self, original: &str, healed: &str, confidence: f64, element: &Element) {
        let mapping = HealedMapping {
            original_selector: original.to_string(),
            healed_selector: healed.to_string(),
            confidence,
            matched_attributes: MatchedAttributes::from(element),
            healed_at: chrono::Utc::now().to_rfc3339(),
            test: None,
            app_id: Some(self.config.scope.clone()).filter(|s| !s.is_empty()),
        };

        if let Ok(mut mappings) = self.healed_mappings.lock() {
            mappings.push(mapping);
        }
    }

    /// The test that made the pending heals ended: keep them, tagged with
    /// it, if it passed; drop them if it failed.
    pub fn settle_heals(&self, test: &str, passed: bool) {
        if let Ok(mut settled) = self.settled_tests.lock() {
            settled.insert(test.to_string());
        }
        if let Ok(mut mappings) = self.healed_mappings.lock() {
            if passed {
                for m in mappings.iter_mut().filter(|m| m.test.is_none()) {
                    m.test = Some(test.to_string());
                }
            } else {
                mappings.retain(|m| m.test.is_some());
            }
        }
    }

    /// Get all accumulated healed mappings (for persistence/reporting).
    pub fn healed_mappings(&self) -> Vec<HealedMapping> {
        self.healed_mappings
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    /// Persist healed mappings to disk as JSON.
    pub fn persist(&self) -> Result<()> {
        if !self.config.persist_healed {
            return Ok(());
        }

        let path = self
            .config
            .persist_path
            .as_deref()
            .unwrap_or("velocity-healed-selectors.json");

        // Only heals whose test passed (see `settle_heals`).
        let mut ours: Vec<HealedMapping> = self
            .healed_mappings()
            .into_iter()
            .filter(|m| m.test.is_some())
            .collect();
        // Secrets typed into a field or interpolated into a selector stay
        // out of the file (VEL-065).
        velocity_common::redact::redact_serde(&mut ours);
        let settled = self
            .settled_tests
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        if ours.is_empty() && settled.is_empty() {
            return Ok(());
        }

        // One healer per device actor, all writing this file: merge under a
        // lock rather than each overwriting the others' heals.
        static FILE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let on_disk: Vec<HealedMapping> = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut mappings: Vec<HealedMapping> = on_disk
            .into_iter()
            .filter(|m| m.test.as_ref().is_some_and(|t| !settled.contains(t)))
            .collect();
        mappings.extend(ours);
        if mappings.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }

        let json = serde_json::to_string_pretty(&mappings)
            .map_err(|e| VelocityError::io("Failed to serialize healed mappings", e))?;
        velocity_common::fsutil::write_state(std::path::Path::new(path), json.as_bytes()).map_err(
            |e| VelocityError::io(format!("Failed to write healed mappings to {path}"), e),
        )?;

        info!(
            path,
            count = mappings.len(),
            "persisted healed selector mappings"
        );
        Ok(())
    }

    /// Save element snapshots (merged with what other processes wrote) if any
    /// changed. Best-effort; a failed save only costs healing context.
    pub fn persist_snapshots(&self) {
        let Some(path) = &self.config.snapshot_path else {
            return;
        };
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let Ok(known) = self.known_elements.lock() else {
            return;
        };
        let mut file = SnapshotFile::load(path);
        for (k, v) in known.iter() {
            let mut v = v.clone();
            velocity_common::redact::redact_serde(&mut v);
            file.entries.insert(k.clone(), v);
        }
        if let Err(e) = file.save(path) {
            warn!(
                "self-healing: failed to save snapshots to {}: {e}",
                path.display()
            );
        }
    }

    /// Whether healing is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

/// On-disk element snapshots.
#[derive(Default, Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    entries: HashMap<String, ElementSnapshot>,
}

const SNAPSHOT_VERSION: u32 = 1;

impl SnapshotFile {
    fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .filter(|f| f.version == SNAPSHOT_VERSION)
            .unwrap_or(Self {
                version: SNAPSHOT_VERSION,
                entries: HashMap::new(),
            })
    }

    fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        velocity_common::fsutil::write_state(
            path,
            &serde_json::to_vec(self).map_err(std::io::Error::other)?,
        )
    }
}

/// Minimum lead the best heal candidate needs over the runner-up.
const AMBIGUITY_MARGIN: f64 = 0.05;

fn flatten_elements(element: &Element, screen: Option<&Rect>, out: &mut Vec<Element>) {
    if element.visible
        && !element.bounds.is_empty()
        && screen.is_none_or(|s| element.bounds.intersects(s))
    {
        // Include elements that have some identifying information
        if element.text.is_some() || element.label.is_some() || !element.platform_id.is_empty() {
            out.push(element.clone());
        }
    }
    for child in &element.children {
        flatten_elements(child, screen, out);
    }
}

/// Similo weights (Nass, Alégroth & Feldt; reference implementation
/// `WidgetLocator.java`): stable attributes count 1.5, weak ones 0.5.
const STABLE: f64 = 1.5;
const WEAK: f64 = 0.5;

/// Compute a similarity score (0.0–1.0) between a selector/snapshot and a candidate element.
/// The hierarchy is used for fingerprint context matching.
fn compute_similarity(
    selector: &Selector,
    snapshot: Option<&ElementSnapshot>,
    candidate: &Element,
    hierarchy: Option<&Element>,
    platform: Option<Platform>,
) -> f64 {
    let mut score = 0.0;
    let mut max_score = 0.0;

    // Score based on the selector type (what we were looking for). The test's
    // own selector is the strongest statement of intent, so it gets a stable
    // attribute's weight.
    match selector {
        Selector::Id(id) => {
            max_score += STABLE;
            // On the web `platform_id` is Velocity's own `data-vel-id`; the
            // DOM id is in `label` (when there is no aria-label). Comparing
            // with the random vel-id made id healing impossible there.
            let mut best = identifier_similarity(id, &candidate.platform_id);
            if platform == Some(Platform::Web) {
                if let Some(label) = candidate.label.as_deref() {
                    best = best.max(identifier_similarity(id, label));
                }
            }
            score += best * STABLE;
        }
        // `text:` matches the text or the label (iOS keeps a button's
        // visible string in the label): score the better of the two (N-5).
        Selector::Text(text) => {
            max_score += STABLE;
            let best = [candidate.text.as_deref(), candidate.label.as_deref()]
                .into_iter()
                .flatten()
                .map(|t| string_similarity(text, t))
                .fold(0.0, f64::max);
            score += best * STABLE;
        }
        Selector::TextContains(sub) => {
            max_score += STABLE;
            let best = [candidate.text.as_deref(), candidate.label.as_deref()]
                .into_iter()
                .flatten()
                .map(|t| {
                    if t.to_lowercase().contains(&sub.to_lowercase()) {
                        1.0
                    } else {
                        string_similarity(sub, t) * 0.7
                    }
                })
                .fold(0.0, f64::max);
            score += best * STABLE;
        }
        Selector::AccessibilityId(aid) => {
            max_score += STABLE;
            let candidate_label = candidate.label.as_deref().unwrap_or("");
            score += identifier_similarity(aid, candidate_label) * STABLE;
        }
        Selector::ClassName(cls) => {
            max_score += STABLE;
            score += string_similarity(cls, &candidate.element_type) * STABLE;
        }
        Selector::Compound(selectors) => {
            // A state filter is a constraint (`keeps_constraints`), not a
            // likeness: it neither adds nor costs score.
            for sub_sel in selectors.iter().filter(|s| !s.is_state_filter()) {
                let sub_score = compute_similarity(sub_sel, None, candidate, hierarchy, platform);
                max_score += STABLE;
                score += sub_score * STABLE;
            }
        }
        Selector::Index {
            selector: inner, ..
        } => {
            return compute_similarity(inner, snapshot, candidate, hierarchy, platform);
        }
        // Heal on what the target looks like; its position relative to the
        // anchor is re-checked when the healed selector is used.
        Selector::Relative { target, .. } => {
            return compute_similarity(target, snapshot, candidate, hierarchy, platform);
        }
        // CSS/XPath selectors are web-only and don't contribute to mobile healing scores.
        // Fall back to snapshot-based comparison below (score stays 0 for selector part).
        Selector::Css(_) | Selector::XPath(_) => {}
        // A regex is already a loose match: it is never healed (D-11), so
        // it scores nothing here and fails `keeps_constraints`.
        Selector::TextMatches(_) | Selector::IdMatches(_) => {}
        // S1: the role is what the element is (kept by `keeps_constraints`);
        // the name heals like a text.
        Selector::Role(role) => {
            max_score += STABLE;
            if velocity_common::role::role_of(candidate) == Some(role.trim()) {
                score += STABLE;
            }
        }
        Selector::Name(name) => {
            max_score += STABLE;
            let have = velocity_common::role::accessible_name(candidate).unwrap_or_default();
            score += string_similarity(name, &have) * STABLE;
        }
        Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => {}
    }

    // Compare against the element as it was last seen. An attribute the
    // snapshot had but the candidate lacks scores 0 (Similo compares every
    // attribute the target had), so a lookalike missing the old text loses.
    if let Some(snap) = snapshot {
        // The attribute the selector names was already scored above; scoring
        // the snapshot's copy too would count one change twice (a renamed id
        // would sink an otherwise identical element).
        let (by_id, by_text, by_label) = match selector {
            Selector::Id(_) => (true, false, false),
            Selector::Text(_) | Selector::TextContains(_) => (false, true, false),
            Selector::AccessibilityId(_) => (false, false, true),
            _ => (false, false, false),
        };
        let mut add = |weight: f64, similarity: f64| {
            max_score += weight;
            score += weight * similarity;
        };
        let stable_ids = platform == Some(Platform::Android);
        if stable_ids && !by_id && !snap.platform_id.is_empty() {
            add(
                STABLE,
                f64::from(u8::from(
                    snap.platform_id
                        .eq_ignore_ascii_case(&candidate.platform_id),
                )),
            );
        }
        add(
            STABLE,
            f64::from(u8::from(snap.element_type == candidate.element_type)),
        );
        if let Some(t) = snap.text.as_ref().filter(|_| !by_text) {
            add(
                STABLE,
                string_similarity(t, candidate.text.as_deref().unwrap_or("")),
            );
        }
        if let Some(l) = snap.label.as_ref().filter(|_| !by_label) {
            add(
                STABLE,
                string_similarity(l, candidate.label.as_deref().unwrap_or("")),
            );
        }
        let near = if platform == Some(Platform::Web) {
            100.0
        } else {
            300.0
        };
        add(
            WEAK,
            spatial_similarity_within(&snap.bounds, &candidate.bounds, near),
        );
        add(WEAK, ratio(area(&snap.bounds), area(&candidate.bounds)));
        add(WEAK, ratio(aspect(&snap.bounds), aspect(&candidate.bounds)));

        // Structural context — only when the snapshot captured it; charging
        // its weight for an empty fingerprint would cap every score.
        if !snap.fingerprint.is_empty() {
            if let Some(h) = hierarchy {
                let cand_fp = extract_fingerprint(candidate, h);
                let fp = &snap.fingerprint;
                if !fp.sibling_labels.is_empty() {
                    add(
                        STABLE,
                        word_overlap(&fp.sibling_labels, &cand_fp.sibling_labels),
                    );
                }
                add(WEAK, fingerprint_similarity(fp, candidate, hierarchy));
            }
        }
    }

    if max_score == 0.0 {
        return 0.0;
    }

    score / max_score
}

/// `selector` with its leaf (the plain part under any `index:` or
/// relation's target) rewritten to name `el`, in the same form the test
/// wrote it: a `text:` stays a `text:` with the element's text. Fields of a
/// compound that `el` already satisfies are kept. `None` when the leaf has
/// no such form (CSS, XPath) or `el` lacks the attribute.
fn substitute(selector: &Selector, el: &Element, platform: Option<Platform>) -> Option<Selector> {
    use velocity_common::selector_match::matches_selector;
    let keep_or_rewrite = |leaf: &Selector| {
        if matches_selector(el, leaf, None) {
            Some(leaf.clone())
        } else {
            rewrite_leaf(leaf, el, platform)
        }
    };
    match selector {
        Selector::Index { selector, index } => Some(Selector::Index {
            selector: Box::new(substitute(selector, el, platform)?),
            index: *index,
        }),
        Selector::Relative {
            target,
            relation,
            anchor,
        } => Some(Selector::Relative {
            target: Box::new(substitute(target, el, platform)?),
            relation: *relation,
            anchor: anchor.clone(),
        }),
        Selector::Compound(parts) => Some(Selector::Compound(
            parts.iter().map(keep_or_rewrite).collect::<Option<_>>()?,
        )),
        leaf => keep_or_rewrite(leaf),
    }
}

/// A leaf selector of `leaf`'s kind naming `el`.
fn rewrite_leaf(leaf: &Selector, el: &Element, platform: Option<Platform>) -> Option<Selector> {
    let some = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    let id = || (!el.platform_id.is_empty()).then(|| el.platform_id.clone());
    Some(match leaf {
        Selector::Text(_) => Selector::Text(some(&el.text).or_else(|| some(&el.label))?),
        Selector::TextContains(_) => {
            Selector::TextContains(some(&el.text).or_else(|| some(&el.label))?)
        }
        // On the web `platform_id` is Velocity's own node id; the DOM id
        // is the label.
        Selector::Id(_) if platform == Some(Platform::Web) => {
            Selector::Id(some(&el.label).or_else(id)?)
        }
        Selector::Id(_) => Selector::Id(id()?),
        Selector::AccessibilityId(_) => Selector::AccessibilityId(some(&el.label).or_else(id)?),
        Selector::ClassName(_) => Selector::ClassName(el.element_type.clone()),
        _ => return None,
    })
}

/// What `selector` resolves to on `hierarchy` when that is `el` alone (or
/// the same thing: its label inside it, its wrapper around it). A relation
/// counts only the matches tied for nearest, as an action would.
fn singles_out(
    hierarchy: &Element,
    selector: &Selector,
    el: &Element,
    screen: Option<&Rect>,
) -> Option<Element> {
    use velocity_common::selector_match::{ambiguity, find_all_ref};
    if ambiguity(hierarchy, selector, screen) != 1 {
        return None;
    }
    let found = *find_all_ref(hierarchy, selector, screen).first()?;
    let inside = |a: &Rect, b: &Rect| {
        a.x >= b.x
            && a.y >= b.y
            && a.x + a.width <= b.x + b.width
            && a.y + a.height <= b.y + b.height
    };
    (found.bounds == el.bounds
        || inside(&found.bounds, &el.bounds)
        || inside(&el.bounds, &found.bounds))
    .then(|| found.clone())
}

/// Some part of `selector` (an `index:` or a relation's target) still matches
/// in `hierarchy`, so the selector failed on its constraint, not on a rename.
fn constraint_unmet(selector: &Selector, hierarchy: &Element) -> bool {
    use velocity_common::selector_match::find_all_ref;
    match selector {
        Selector::Index {
            selector: inner, ..
        } => !find_all_ref(hierarchy, inner, None).is_empty() || constraint_unmet(inner, hierarchy),
        Selector::Relative { target, .. } => {
            !find_all_ref(hierarchy, target, None).is_empty() || constraint_unmet(target, hierarchy)
        }
        _ => false,
    }
}

/// Whether `candidate` may stand in for `selector`: a relation must still
/// hold against its anchor, and a text or id must not name a different
/// value — other digits ("Pay $10.00" / "Pay $100.00", "row_1" / "row_2"),
/// or, unless the snapshot shows it is the same element, another word in
/// the same place ("Size M" / "Size L"). Edit distance alone rated those
/// 0.83 and the test tapped the wrong option and passed.
fn keeps_constraints(
    selector: &Selector,
    candidate: &Element,
    hierarchy: &Element,
    snapshot: Option<&ElementSnapshot>,
) -> bool {
    let same_element = snapshot.is_some_and(|s| {
        (!s.platform_id.is_empty() && s.platform_id == candidate.platform_id)
            || spatial_similarity_within(&s.bounds, &candidate.bounds, 300.0) >= 0.9
    });
    let value_ok = |wanted: &str, found: Option<&str>| {
        let found = found.unwrap_or("");
        digit_runs(wanted) == digit_runs(found)
            && (same_element || !one_word_swapped(wanted, found))
    };
    match selector {
        Selector::Text(t) => value_ok(t, candidate.text.as_deref().or(candidate.label.as_deref())),
        Selector::Id(id) => value_ok(
            id.rsplit(":id/").next().unwrap_or(id),
            Some(candidate.platform_id.rsplit(":id/").next().unwrap_or("")),
        ),
        Selector::AccessibilityId(aid) => value_ok(aid, candidate.label.as_deref()),
        Selector::TextContains(sub) => {
            let found = digit_runs(candidate.text.as_deref().unwrap_or(""));
            digit_runs(sub).iter().all(|d| found.contains(d))
        }
        Selector::Compound(all) => all
            .iter()
            .all(|s| keeps_constraints(s, candidate, hierarchy, snapshot)),
        Selector::Index {
            selector: inner, ..
        } => keeps_constraints(inner, candidate, hierarchy, snapshot),
        Selector::Relative {
            target,
            relation,
            anchor,
        } => {
            keeps_constraints(target, candidate, hierarchy, snapshot)
                && velocity_common::selector_match::stands_in(
                    hierarchy, candidate, *relation, anchor, None,
                )
        }
        Selector::ClassName(_) | Selector::Css(_) | Selector::XPath(_) => true,
        Selector::TextMatches(_) | Selector::IdMatches(_) => false,
        // A heal never changes what kind of element it is, or its state:
        // `{role: button, checked: true}` never heals to a link or to an
        // unchecked box.
        Selector::Role(_)
        | Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => {
            velocity_common::selector_match::matches_selector(candidate, selector, None)
        }
        Selector::Name(name) => value_ok(
            name,
            velocity_common::role::accessible_name(candidate).as_deref(),
        ),
    }
}

/// The runs of digits in `s`, in order: an amount, a count, a row number.
fn digit_runs(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Same words but one, and that one is not a respelling of the other
/// ("Size M" / "Size L", "Sign in" / "Sign up"; not "Color" / "Colour").
fn one_word_swapped(a: &str, b: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || c == '_' || c == '-')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let (a, b) = (words(a), words(b));
    if a.len() != b.len() || a.len() < 2 {
        return false;
    }
    let diff: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).collect();
    diff.len() == 1 && string_similarity(diff[0].0, diff[0].1) < 0.75
}

/// Short forms UI ids use for the same word (`login_btn` / `login_button`).
const ID_ABBREVIATIONS: &[(&str, &str)] = &[
    ("btn", "button"),
    ("txt", "text"),
    ("lbl", "label"),
    ("img", "image"),
    ("msg", "message"),
    ("pwd", "password"),
    ("pw", "password"),
    ("nav", "navigation"),
    ("cfg", "config"),
    ("desc", "description"),
    ("num", "number"),
    ("qty", "quantity"),
    ("addr", "address"),
    ("tv", "textview"),
    ("et", "edittext"),
    ("iv", "imageview"),
    ("cb", "checkbox"),
    ("ic", "icon"),
];

/// The words of an id: split on punctuation and camelCase, lower-cased,
/// abbreviations expanded, an Android `pkg:id/` prefix dropped.
fn id_tokens(id: &str) -> Vec<String> {
    let id = id.rsplit(":id/").next().unwrap_or(id);
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in id.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut words: Vec<String> = words
        .into_iter()
        .map(|w| {
            ID_ABBREVIATIONS
                .iter()
                .find(|(short, _)| *short == w)
                .map_or(w, |(_, long)| (*long).to_string())
        })
        .collect();
    words.sort();
    words
}

/// [`string_similarity`] for ids, also 0.95 when both name the same words
/// (`login_btn` / `login_button` / `btnLogin`): a rename like that scored
/// 0.75 by edit distance, under the threshold. Only the same word set
/// counts, so `nav_logout` stays far from `nav_login`.
fn identifier_similarity(a: &str, b: &str) -> f64 {
    let plain = string_similarity(a, b);
    let (ta, tb) = (id_tokens(a), id_tokens(b));
    if !ta.is_empty() && ta == tb {
        plain.max(0.95)
    } else {
        plain
    }
}

/// Compute string similarity using Levenshtein-like normalized distance.
fn string_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();

    if a_lower == b_lower {
        return 0.95; // case-insensitive exact match
    }

    let a_chars = a_lower.chars().count() as f64;
    let b_chars = b_lower.chars().count() as f64;

    // Check containment. The score must scale with the length ratio and stay
    // below the confidence threshold for short-in-long matches, or e.g.
    // Text("Delete") would heal onto a "Delete All" button with zero history.
    if b_lower.contains(&a_lower) || a_lower.contains(&b_lower) {
        let shorter = a_chars.min(b_chars);
        let longer = a_chars.max(b_chars);
        return 0.6 + 0.3 * (shorter / longer);
    }

    // Levenshtein distance (char-based; normalizing by byte length would
    // inflate similarity for non-ASCII labels)
    let distance = levenshtein(&a_lower, &b_lower);
    let max_len = a_chars.max(b_chars);
    let similarity = 1.0 - (distance as f64 / max_len);

    similarity.max(0.0)
}

/// 1.0 at the same centre, falling linearly to 0 at `within` pixels
/// (Similo uses 100 CSS px; mobile coordinates are physical pixels, ~3x).
fn spatial_similarity_within(a: &Rect, b: &Rect, within: f64) -> f64 {
    let (ax, ay) = a.center();
    let (bx, by) = b.center();
    let d = (f64::from(ax - bx).powi(2) + f64::from(ay - by).powi(2)).sqrt();
    (1.0 - d / within).max(0.0)
}

fn area(r: &Rect) -> f64 {
    f64::from(r.width.max(0)) * f64::from(r.height.max(0))
}

fn aspect(r: &Rect) -> f64 {
    if r.height <= 0 {
        0.0
    } else {
        f64::from(r.width) / f64::from(r.height)
    }
}

/// min/max, 1.0 when equal; 0 when either is 0.
fn ratio(a: f64, b: f64) -> f64 {
    if a <= 0.0 || b <= 0.0 {
        return if a == b { 1.0 } else { 0.0 };
    }
    a.min(b) / a.max(b)
}

/// Jaccard overlap of the words in two lists of labels.
fn word_overlap(a: &[String], b: &[String]) -> f64 {
    let words = |v: &[String]| -> std::collections::HashSet<String> {
        v.iter()
            .flat_map(|s| s.split_whitespace())
            .map(str::to_lowercase)
            .collect()
    };
    let (wa, wb) = (words(a), words(b));
    let union = wa.union(&wb).count();
    if union == 0 {
        return 0.0;
    }
    wa.intersection(&wb).count() as f64 / union as f64
}

/// Compute spatial similarity between two rects (0.0–1.0).
#[cfg(test)]
fn spatial_similarity(a: &Rect, b: &Rect) -> f64 {
    let (ax, ay) = a.center();
    let (bx, by) = b.center();

    let dx = (ax - bx).abs() as f64;
    let dy = (ay - by).abs() as f64;
    let distance = (dx * dx + dy * dy).sqrt();

    // Normalize against a reference distance (half screen diagonal ~1000px)
    let max_distance = 1000.0;
    (1.0 - distance / max_distance).max(0.0)
}

/// Compute fingerprint similarity: how well does the candidate's structural
/// context (parent, siblings) match the snapshot's recorded context.
fn fingerprint_similarity(
    fingerprint: &ElementFingerprint,
    candidate: &Element,
    hierarchy: Option<&Element>,
) -> f64 {
    let hierarchy = match hierarchy {
        Some(h) => h,
        None => return 0.0,
    };

    let candidate_fp = extract_fingerprint(candidate, hierarchy);
    let mut score = 0.0;
    let mut max_score = 0.0;

    // Parent type match
    if let (Some(ref snap_parent), Some(ref cand_parent)) =
        (&fingerprint.parent_type, &candidate_fp.parent_type)
    {
        max_score += 1.0;
        if snap_parent == cand_parent {
            score += 1.0;
        }
    }

    // Parent label match
    if let (Some(ref snap_label), Some(ref cand_label)) =
        (&fingerprint.parent_label, &candidate_fp.parent_label)
    {
        max_score += 0.5;
        score += string_similarity(snap_label, cand_label) * 0.5;
    }

    // Sibling types overlap (Jaccard similarity)
    if !fingerprint.sibling_types.is_empty() || !candidate_fp.sibling_types.is_empty() {
        max_score += 0.5;
        // As sets on both sides: counting duplicates on one side only let
        // four same-type siblings score 4/1, past the cap, so a wrong
        // element could clear the confidence threshold.
        let a: std::collections::HashSet<&str> = fingerprint
            .sibling_types
            .iter()
            .map(String::as_str)
            .collect();
        let b: std::collections::HashSet<&str> = candidate_fp
            .sibling_types
            .iter()
            .map(String::as_str)
            .collect();
        let union = a.union(&b).count();
        if union > 0 {
            score += (a.intersection(&b).count() as f64 / union as f64) * 0.5;
        }
    }

    // Type index match (same position among same-type siblings)
    if let (Some(snap_idx), Some(cand_idx)) = (fingerprint.type_index, candidate_fp.type_index) {
        max_score += 0.3;
        if snap_idx == cand_idx {
            score += 0.3;
        }
    }

    if max_score == 0.0 {
        return 0.0;
    }
    score / max_score
}

/// Simple Levenshtein distance.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for (i, row) in dp.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    for (j, val) in dp[0].iter_mut().enumerate().take(n + 1) {
        *val = j;
    }

    for i in 1..=m {
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }

    dp[m][n]
}

/// The `n` elements on `hierarchy` that look most like what `selector`
/// asks for, best first, by the same score healing uses (no snapshot: this
/// runs after the fact, e.g. for `describe_failure`). For telling a test
/// author what is on screen instead, not for acting on.
pub fn closest_elements(
    selector: &Selector,
    hierarchy: &Element,
    platform: Option<Platform>,
    n: usize,
) -> Vec<(Element, f64)> {
    let mut candidates = Vec::new();
    flatten_elements(hierarchy, None, &mut candidates);
    let mut scored: Vec<(Element, f64)> = candidates
        .into_iter()
        .map(|el| {
            let score = compute_similarity(selector, None, &el, Some(hierarchy), platform);
            (el, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    // A label and its clickable wrapper share bounds: show the thing once.
    let mut seen = Vec::new();
    scored.retain(|(el, _)| {
        let fresh = !seen.contains(&el.bounds);
        seen.push(el.bounds);
        fresh
    });
    scored.truncate(n);
    scored
}

/// Generate a human-readable selector description for a healed element.
pub fn describe_element(element: &Element) -> String {
    if let Some(ref label) = element.label {
        if !label.is_empty() {
            return format!("accessibilityId={label:?}");
        }
    }
    if let Some(ref text) = element.text {
        if !text.is_empty() {
            return format!("text={text:?}");
        }
    }
    if !element.platform_id.is_empty() {
        return format!("id={:?}", element.platform_id);
    }
    format!(
        "type={:?} at ({},{})",
        element.element_type, element.bounds.x, element.bounds.y
    )
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_text_selector_scores_against_the_label_too() {
        // N-5: iOS keeps a button's visible string in `label`; a renamed
        // label-only button scored 0 on the selector term.
        let mk = |label: Option<&str>, text: Option<&str>| Element {
            hint: None,
            platform_id: String::new(),
            label: label.map(String::from),
            text: text.map(String::from),
            element_type: "Button".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let sel = Selector::Text("Sign In".into());
        let by_label = compute_similarity(&sel, None, &mk(Some("Sign in"), None), None, None);
        let by_text = compute_similarity(&sel, None, &mk(None, Some("Sign in")), None, None);
        assert!(by_label > 0.5, "{by_label}");
        assert!((by_label - by_text).abs() < 1e-9, "{by_label} vs {by_text}");
        let sub = Selector::TextContains("Sign".into());
        assert!(compute_similarity(&sub, None, &mk(Some("Sign in"), None), None, None) > 0.9);
    }

    use super::*;

    #[test]
    fn closest_elements_rank_the_nearest_spelling_first() {
        let screen = login_screen(Some(sign_in_button()));
        let best = closest_elements(&Selector::Text("Sign-in".into()), &screen, None, 3);
        assert!(!best.is_empty());
        assert!(best.len() <= 3);
        assert_eq!(best[0].0.text.as_deref(), Some("Sign in"), "{best:?}");
        assert!(best.windows(2).all(|w| w[0].1 >= w[1].1));
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "hello"), 5);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn test_string_similarity_exact() {
        assert_eq!(string_similarity("hello", "hello"), 1.0);
    }

    #[test]
    fn test_string_similarity_case_insensitive() {
        assert!(string_similarity("Hello", "hello") > 0.9);
    }

    #[test]
    fn test_string_similarity_containment() {
        let score = string_similarity("login", "loginButton");
        assert!(score > 0.7, "containment score was {score}");
    }

    #[test]
    fn renamed_ids_with_the_same_words_heal_and_near_misses_do_not() {
        assert!(identifier_similarity("login_btn", "login_button") >= 0.95);
        assert!(identifier_similarity("btnLogin", "login_button") >= 0.95);
        assert!(identifier_similarity("com.app:id/submit_btn", "com.app:id/submitButton") >= 0.95);
        let t = HealingConfig::default().confidence_threshold;
        assert!(identifier_similarity("nav_logout", "nav_login") < t);
        assert!(identifier_similarity("delete_item_btn", "delete_account_button") < t);
        assert!(identifier_similarity("row_12", "row_13") < 0.95);
    }

    #[test]
    fn test_containment_stays_below_default_threshold_for_short_in_long() {
        // "Delete" is contained in "Delete All" — a healed match here would tap
        // a destructive wrong element. The score must stay below the default
        // confidence threshold (0.8).
        let score = string_similarity("delete", "delete all");
        assert!(
            score < HealingConfig::default().confidence_threshold,
            "Delete/Delete All scored {score}, would heal onto the wrong element"
        );
    }

    #[test]
    fn test_string_similarity_char_based_for_non_ascii() {
        // Same 1-char difference must score the same whether the labels are
        // ASCII or multi-byte; byte-based normalization would inflate the latter.
        let ascii = string_similarity("abcd", "abcx");
        let multibyte = string_similarity("äbcd", "äbcx");
        assert!(
            (ascii - multibyte).abs() < 1e-9,
            "ascii {ascii} vs multibyte {multibyte}"
        );
    }

    #[test]
    fn test_empty_fingerprint_does_not_penalize_snapshot_score() {
        // A snapshot recorded without hierarchy context has an empty
        // fingerprint. Its weight must not be charged to max_score, or every
        // snapshot-based comparison is capped below a perfect score.
        let selector = Selector::Text("Sign In".to_string());
        let element = Element {
            hint: None,
            platform_id: "btn1".to_string(),
            label: Some("Sign In".to_string()),
            text: Some("Sign In".to_string()),
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 100,
                y: 500,
                width: 200,
                height: 50,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let snapshot = ElementSnapshot::from(&element);
        let score = compute_similarity(&selector, Some(&snapshot), &element, None, None);
        assert!(
            score > 0.99,
            "identical element with contextless snapshot scored {score}"
        );
    }

    #[test]
    fn test_type_index_counts_target_position() {
        let make_btn = |id: &str, y: i32| Element {
            hint: None,
            platform_id: id.to_string(),
            label: None,
            text: None,
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let second = make_btn("b2", 50);
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "View".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            enabled: true,
            visible: true,
            children: vec![make_btn("b1", 0), second.clone(), make_btn("b3", 100)],
            ..Default::default()
        };
        let fp = extract_fingerprint(&second, &root);
        assert_eq!(
            fp.type_index,
            Some(1),
            "second same-type child must have index 1"
        );
    }

    #[test]
    fn test_string_similarity_different() {
        let score = string_similarity("apple", "orange");
        assert!(score < 0.5, "different string score was {score}");
    }

    #[test]
    fn test_spatial_similarity_same_position() {
        let r = Rect {
            x: 100,
            y: 200,
            width: 50,
            height: 50,
        };
        assert_eq!(spatial_similarity(&r, &r), 1.0);
    }

    #[test]
    fn test_spatial_similarity_far_apart() {
        let a = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        };
        let b = Rect {
            x: 900,
            y: 900,
            width: 50,
            height: 50,
        };
        let score = spatial_similarity(&a, &b);
        assert!(score < 0.2, "far apart score was {score}");
    }

    #[test]
    fn test_compute_similarity_exact_text_match() {
        let selector = Selector::Text("Sign In".to_string());
        let element = Element {
            hint: None,
            platform_id: "btn1".to_string(),
            label: None,
            text: Some("Sign In".to_string()),
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 100,
                y: 500,
                width: 200,
                height: 50,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let score = compute_similarity(&selector, None, &element, None, None);
        assert_eq!(score, 1.0);
    }

    #[test]
    fn test_compute_similarity_partial_match() {
        let selector = Selector::Text("Sign In".to_string());
        let element = Element {
            hint: None,
            platform_id: "btn1".to_string(),
            label: None,
            text: Some("Sign In Here".to_string()),
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 100,
                y: 500,
                width: 200,
                height: 50,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let score = compute_similarity(&selector, None, &element, None, None);
        assert!(score > 0.7, "partial match score was {score}");
    }

    #[test]
    fn test_flatten_elements() {
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "View".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            enabled: true,
            visible: true,
            children: vec![
                Element {
                    hint: None,
                    platform_id: "btn1".to_string(),
                    label: Some("Login".to_string()),
                    text: Some("Login".to_string()),
                    element_type: "Button".to_string(),
                    bounds: Rect {
                        x: 100,
                        y: 500,
                        width: 200,
                        height: 50,
                    },
                    enabled: true,
                    visible: true,
                    children: vec![],
                    ..Default::default()
                },
                Element {
                    hint: None,
                    platform_id: "".to_string(),
                    label: None,
                    text: None,
                    element_type: "View".to_string(),
                    bounds: Rect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 100,
                    },
                    enabled: true,
                    visible: true,
                    children: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut flat = Vec::new();
        flatten_elements(&root, None, &mut flat);
        // root has platform_id so it's included; btn1 has label/text; the empty View has nothing
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[1].platform_id, "btn1");
    }

    #[test]
    fn test_describe_element_prefers_label() {
        let el = Element {
            hint: None,
            platform_id: "id1".to_string(),
            label: Some("Login Button".to_string()),
            text: Some("Login".to_string()),
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
        };
        let desc = describe_element(&el);
        assert!(desc.contains("Login Button"));
    }

    // ── Heal benchmark: one login screen, changed in the ways apps change ──

    fn widget(id: &str, ty: &str, text: Option<&str>, label: Option<&str>, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: label.map(String::from),
            text: text.map(String::from),
            element_type: ty.into(),
            bounds: Rect {
                x: 140,
                y,
                width: 800,
                height: 120,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    /// The login form; `sign_in` is the button under test (None = removed).
    fn login_screen(sign_in: Option<Element>) -> Element {
        let mut form = vec![
            widget("", "TextView", Some("Welcome back"), None, 200),
            widget("com.app:id/email", "EditText", None, Some("Email"), 400),
            widget(
                "com.app:id/password",
                "EditText",
                None,
                Some("Password"),
                520,
            ),
        ];
        form.extend(sign_in);
        form.push(widget(
            "com.app:id/sign_up",
            "Button",
            Some("Sign up"),
            None,
            860,
        ));
        form.push(widget("", "TextView", Some("Forgot password?"), None, 1000));
        Element {
            children: vec![Element {
                children: form,
                ..widget("com.app:id/form", "LinearLayout", None, None, 0)
            }],
            ..widget("hierarchy", "hierarchy", None, None, 0)
        }
    }

    fn sign_in_button() -> Element {
        widget("com.app:id/sign_in", "Button", Some("Sign in"), None, 700)
    }

    /// A healer that saw the original screen once (as a passing run would).
    fn trained(selector: &Selector, path: Option<PathBuf>) -> SelectorHealer {
        let healer = SelectorHealer::new(HealingConfig {
            snapshot_path: path,
            platform: Some(Platform::Android),
            scope: "com.app".into(),
            ..Default::default()
        });
        let before = login_screen(Some(sign_in_button()));
        healer.record_success_with_context(selector, &sign_in_button(), &before);
        healer
    }

    fn healed_id(healer: &SelectorHealer, selector: &Selector, after: &Element) -> Option<String> {
        healer
            .heal_in(selector, after)
            .map(|(el, ..)| el.platform_id)
    }

    /// iPhone 12, 2026-11-21: a target hidden behind a sheet "healed" to
    /// its own selector. A heal must change the selector.
    #[test]
    fn a_selector_that_already_names_the_match_is_not_healed() {
        let sel = Selector::Id("com.app:id/sign_in".into());
        let healer = trained(&sel, None);
        let screen = login_screen(Some(sign_in_button()));
        assert_eq!(healed_id(&healer, &sel, &screen), None);
    }

    #[test]
    fn device_actors_share_the_healed_file_instead_of_overwriting_it() {
        let dir = std::env::temp_dir().join(format!("velocity-healed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("healed.json");
        let healer = || {
            SelectorHealer::new(HealingConfig {
                persist_path: Some(path.to_string_lossy().into_owned()),
                ..Default::default()
            })
        };
        let tests_in_file = || -> Vec<String> {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "[]".into());
            let m: Vec<HealedMapping> = serde_json::from_str(&text).unwrap();
            let mut t: Vec<String> = m.into_iter().filter_map(|m| m.test).collect();
            t.sort();
            t
        };
        let (a, b) = (healer(), healer());
        a.record_healing("text=\"A\"", "text=\"A2\"", 0.9, &sign_in_button());
        a.settle_heals("login", true);
        a.persist().unwrap();
        b.record_healing("text=\"B\"", "text=\"B2\"", 0.9, &sign_in_button());
        b.settle_heals("checkout", true);
        b.persist().unwrap();
        assert_eq!(tests_in_file(), ["checkout", "login"]);

        // A later run where `login` passes without healing clears its entry.
        let c = healer();
        c.settle_heals("login", true);
        c.persist().unwrap();
        assert_eq!(tests_in_file(), ["checkout"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heals_renamed_text() {
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(Some(Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        }));
        assert_eq!(
            healed_id(&trained(&sel, None), &sel, &after).as_deref(),
            Some("com.app:id/sign_in")
        );
    }

    #[test]
    fn a_rejected_heal_is_never_proposed_again() {
        // D-7: `heal-rejected.json` is a denylist.
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(Some(Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        }));
        let healer = trained(&sel, None);
        assert!(healer.heal_in(&sel, &after).is_some());
        let dir = tempfile::tempdir().unwrap();
        let rejected = dir.path().join("heal-rejected.json");
        assert_eq!(
            reject_heals(&rejected, &healer.healed_mappings()).unwrap(),
            1
        );
        // Rejecting it twice keeps one entry.
        assert_eq!(
            reject_heals(&rejected, &healer.healed_mappings()).unwrap(),
            0
        );
        assert_eq!(load_rejected(&rejected).len(), 1);
        let again = SelectorHealer::new(HealingConfig {
            platform: Some(Platform::Android),
            scope: "com.app".into(),
            rejected_path: Some(rejected),
            ..Default::default()
        });
        again.record_success_with_context(
            &sel,
            &sign_in_button(),
            &login_screen(Some(sign_in_button())),
        );
        assert!(again.heal_in(&sel, &after).is_none());
        assert!(again.healed_mappings().is_empty());
    }

    /// S5: a rejection is scoped to the app it was made for. The same
    /// selector in another app still heals; an entry from before S5 (no
    /// app) still applies everywhere.
    #[test]
    fn a_rejected_heal_is_scoped_to_its_app() {
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(Some(Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        }));
        let healer = trained(&sel, None);
        assert!(healer.heal_in(&sel, &after).is_some());
        let mappings = healer.healed_mappings();
        assert_eq!(mappings[0].app_id.as_deref(), Some("com.app"));
        let dir = tempfile::tempdir().unwrap();
        let rejected = dir.path().join("heal-rejected.json");
        reject_heals(&rejected, &mappings).unwrap();
        assert_eq!(
            load_rejected(&rejected)[0].app_id.as_deref(),
            Some("com.app")
        );
        let heals_in = |scope: &str, path: &std::path::Path| {
            let h = SelectorHealer::new(HealingConfig {
                platform: Some(Platform::Android),
                scope: scope.into(),
                rejected_path: Some(path.to_path_buf()),
                ..Default::default()
            });
            h.record_success_with_context(
                &sel,
                &sign_in_button(),
                &login_screen(Some(sign_in_button())),
            );
            h.heal_in(&sel, &after).is_some()
        };
        assert!(!heals_in("com.app", &rejected), "rejected for this app");
        assert!(heals_in("com.other", &rejected), "another app still heals");
        // Rejecting it for the other app adds a second entry.
        let mut other = mappings.clone();
        other[0].app_id = Some("com.other".into());
        assert_eq!(reject_heals(&rejected, &other).unwrap(), 1);
        assert!(!heals_in("com.other", &rejected));
        // A legacy entry (no app) applies to every app, and covers a new
        // rejection of the same pair.
        let legacy = dir.path().join("legacy.json");
        std::fs::write(
            &legacy,
            format!(
                r#"[{{"original_selector": {:?}, "healed_selector": {:?}}}]"#,
                mappings[0].original_selector, mappings[0].healed_selector
            ),
        )
        .unwrap();
        assert!(!heals_in("com.other", &legacy));
        assert_eq!(reject_heals(&legacy, &mappings).unwrap(), 0);
    }

    #[test]
    fn an_off_screen_twin_neither_blocks_a_heal_nor_is_healed_onto() {
        // R3 m-11: iOS reports list rows below the fold; a twin of the
        // renamed button there made the heal "ambiguous".
        let sel = Selector::Text("Sign in".into());
        let renamed = Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        };
        let mut twin = renamed.clone();
        twin.bounds.y += 3000;
        let mut after = login_screen(Some(renamed));
        after.children[0].children.push(twin);
        let healer = trained(&sel, None);
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 1920,
        };
        let healed = healer.heal_in_screen(&sel, &after, Some(&screen));
        let (el, ..) = healed.expect("heals onto the on-screen button");
        assert!(el.bounds.y < 1920, "{:?}", el.bounds);
    }

    #[test]
    fn heals_renamed_id() {
        let sel = Selector::Id("com.app:id/sign_in".into());
        let after = login_screen(Some(Element {
            platform_id: "com.app:id/login_button".into(),
            ..sign_in_button()
        }));
        assert_eq!(
            healed_id(&trained(&sel, None), &sel, &after).as_deref(),
            Some("com.app:id/login_button")
        );
    }

    #[test]
    fn heals_renamed_and_moved() {
        let sel = Selector::Text("Sign in".into());
        let mut moved = Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        };
        moved.bounds.y += 400;
        let after = login_screen(Some(moved));
        assert_eq!(
            healed_id(&trained(&sel, None), &sel, &after).as_deref(),
            Some("com.app:id/sign_in")
        );
    }

    #[test]
    fn does_not_heal_a_removed_element_onto_a_lookalike() {
        // "Sign in" is gone; "Sign up" is one word away and right below it.
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(None);
        assert_eq!(healed_id(&trained(&sel, None), &sel, &after), None);
    }

    #[test]
    fn refuses_ambiguous_duplicates() {
        let sel = Selector::Text("Add".into());
        let healer = SelectorHealer::new(HealingConfig::default());
        let row = |y| widget("", "Button", Some("Add item"), None, y);
        let after = Element {
            children: vec![row(100), row(300)],
            ..widget("hierarchy", "hierarchy", None, None, 0)
        };
        assert_eq!(healed_id(&healer, &sel, &after), None);
    }

    #[test]
    fn snapshots_persist_across_runs_and_enable_heals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snaps.json");
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(Some(Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        }));

        // A fresh process with no snapshot can't justify the heal…
        let cold = SelectorHealer::new(HealingConfig {
            platform: Some(Platform::Android),
            ..Default::default()
        });
        assert_eq!(healed_id(&cold, &sel, &after), None);

        // …but one that loads the snapshot an earlier run saved can.
        trained(&sel, Some(path.clone())).persist_snapshots();
        let next_run = SelectorHealer::new(HealingConfig {
            snapshot_path: Some(path),
            platform: Some(Platform::Android),
            scope: "com.app".into(),
            ..Default::default()
        });
        assert_eq!(
            healed_id(&next_run, &sel, &after).as_deref(),
            Some("com.app:id/sign_in")
        );
    }

    #[test]
    fn snapshot_is_refreshed_only_when_needed() {
        let sel = Selector::Text("Sign in".into());
        let healer = trained(&sel, None);
        assert!(
            !healer.needs_snapshot(&sel, &sign_in_button()),
            "fresh snapshot"
        );
        let renamed = Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        };
        assert!(healer.needs_snapshot(&sel, &renamed), "changed element");
        assert!(
            healer.needs_snapshot(&Selector::Text("Other".into()), &renamed),
            "unknown selector"
        );
    }

    fn mapping(original: &str, text: Option<&str>, id: &str) -> HealedMapping {
        HealedMapping {
            original_selector: original.into(),
            healed_selector: "accessibilityId=\"x\"".into(),
            confidence: 0.9,
            matched_attributes: MatchedAttributes {
                platform_id: id.into(),
                label: Some("Sign in button".into()),
                text: text.map(String::from),
                element_type: "Button".into(),
                bounds: [0, 0, 10, 10],
            },
            healed_at: String::new(),
            test: None,
            app_id: None,
        }
    }

    #[test]
    fn heals_are_written_back_in_the_users_selector_form() {
        let m = mapping(r#"text="Log in""#, Some("Sign in"), "com.app:id/signin");
        assert_eq!(
            m.rewrite(),
            Some(("text".into(), "Log in".into(), "Sign in".into()))
        );
        let m = mapping(r#"id="login""#, None, "com.app:id/signin");
        assert_eq!(
            m.rewrite(),
            Some(("id".into(), "login".into(), "signin".into())),
            "short id stays short"
        );
        assert_eq!(mapping("compound(...)", Some("x"), "").rewrite(), None);
    }

    #[test]
    fn only_heals_from_passing_tests_are_kept() {
        let healer = SelectorHealer::new(HealingConfig::default());
        healer
            .healed_mappings
            .lock()
            .unwrap()
            .push(mapping("text=\"a\"", Some("b"), ""));
        healer.settle_heals("failing test", false);
        assert!(healer.healed_mappings().is_empty());
        healer
            .healed_mappings
            .lock()
            .unwrap()
            .push(mapping("text=\"a\"", Some("b"), ""));
        healer.settle_heals("passing test", true);
        assert_eq!(
            healer.healed_mappings()[0].test.as_deref(),
            Some("passing test")
        );
    }

    #[test]
    fn many_same_type_siblings_cannot_push_context_similarity_past_one() {
        let button = |label: &str, x: i32| Element {
            hint: None,
            platform_id: String::new(),
            label: Some(label.into()),
            text: None,
            element_type: "Button".into(),
            bounds: Rect {
                x,
                y: 0,
                width: 50,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let row = Element {
            hint: None,
            platform_id: "row".into(),
            label: None,
            text: None,
            element_type: "LinearLayout".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: (0..5).map(|i| button(&format!("b{i}"), i * 60)).collect(),
            ..Default::default()
        };
        let target = row.children[2].clone();
        let fp = extract_fingerprint(&target, &row);
        let score = fingerprint_similarity(&fp, &row.children[3], Some(&row));
        assert!(
            score <= 1.0 + f64::EPSILON,
            "context similarity {score} > 1"
        );
    }

    fn screen(children: Vec<Element>) -> Element {
        Element {
            children,
            ..widget("hierarchy", "hierarchy", None, None, 0)
        }
    }

    #[test]
    fn never_heals_onto_another_option_or_amount() {
        // VEL-008: no snapshot; one character away scored 0.83.
        let healer = SelectorHealer::new(HealingConfig::default());
        let options = screen(vec![
            widget("", "Button", Some("Size L"), None, 100),
            widget("", "Button", Some("Pay $100.00"), None, 400),
            widget("", "Button", Some("Item 12"), None, 700),
        ]);
        for wanted in ["Size M", "Pay $10.00", "Item 13"] {
            let sel = Selector::Text(wanted.into());
            assert_eq!(healed_id(&healer, &sel, &options), None, "{wanted}");
        }
        let sel = Selector::Id("row_1".into());
        let rows = screen(vec![widget("row_2", "Cell", None, None, 100)]);
        assert_eq!(healed_id(&healer, &sel, &rows), None);
        // A respelling of one word still heals.
        let sel = Selector::Text("Select colour".into());
        let renamed = screen(vec![widget("c", "Button", Some("Select color"), None, 100)]);
        assert_eq!(healed_id(&healer, &sel, &renamed).as_deref(), Some("c"));
    }

    #[test]
    fn never_drops_an_index_or_a_relation() {
        let healer = SelectorHealer::new(HealingConfig::default());
        // VEL-009: one Delete on screen, the test wants the third.
        let one = screen(vec![widget("d", "Button", Some("Delete"), None, 100)]);
        let third = Selector::Index {
            selector: Box::new(Selector::Text("Delete".into())),
            index: 2,
        };
        assert_eq!(healed_id(&healer, &third, &one), None);
        // VEL-010: the only Edit is above Shipping, the test wants one below.
        let above = screen(vec![
            widget("e", "Button", Some("Edit"), None, 100),
            widget("s", "TextView", Some("Shipping"), None, 500),
        ]);
        let below = |t: &str| Selector::Relative {
            target: Box::new(Selector::Text(t.into())),
            relation: velocity_common::Relation::Below,
            anchor: Box::new(Selector::Text("Shipping".into())),
        };
        assert_eq!(healed_id(&healer, &below("Edit"), &above), None);
        // A renamed target heals only where the relation holds.
        let renamed = screen(vec![
            widget("e1", "Button", Some("Edit address"), None, 100),
            widget("s", "TextView", Some("Shipping"), None, 500),
            widget("e2", "Button", Some("Edit address"), None, 700),
        ]);
        let sel = below("Edit addresses");
        assert_eq!(healed_id(&healer, &sel, &renamed).as_deref(), Some("e2"));
    }

    #[test]
    fn a_renamed_row_heals_only_onto_the_row_its_index_names() {
        // C-m5: `{text: Delete, index: 1}`, the rows renamed and the list
        // pushed down a row. The snapshot's place now holds row 0, which won
        // on position although the index names the second row.
        let row = |text: &str, y| widget("com.app:id/delete", "Button", Some(text), None, y);
        let sel = Selector::Index {
            selector: Box::new(Selector::Text("Delete".into())),
            index: 1,
        };
        let healer = SelectorHealer::new(HealingConfig {
            platform: Some(Platform::Android),
            ..Default::default()
        });
        let before = screen(vec![row("Delete", 100), row("Delete", 300)]);
        healer.record_success_with_context(&sel, &row("Delete", 300), &before);
        let after = screen(vec![row("Delete row", 300), row("Delete row", 500)]);
        let healed = healer.heal_in(&sel, &after).map(|(el, ..)| el.bounds.y);
        assert_ne!(healed, Some(300), "healed onto row 0 for index 1");
        // Every heal it does make resolves through the whole selector.
        if let Some((el, _, sel)) = healer.heal_in(&sel, &after) {
            let all = velocity_common::selector_match::find_all_ref(&after, &sel, None);
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].bounds, el.bounds);
        }
    }

    #[test]
    fn a_heal_is_reported_as_the_test_s_own_selector_form() {
        let sel = Selector::Text("Sign in".into());
        let after = login_screen(Some(Element {
            text: Some("Log in".into()),
            ..sign_in_button()
        }));
        let (_, _, healed) = trained(&sel, None).heal_in(&sel, &after).unwrap();
        assert_eq!(healed, Selector::Text("Log in".into()));
        // A heal whose selector would match two elements is refused.
        let twin = widget("com.app:id/other", "Button", Some("Log in"), None, 1200);
        let mut two = after.clone();
        two.children[0].children.push(twin);
        assert_eq!(healed_id(&trained(&sel, None), &sel, &two), None);
    }

    #[test]
    fn secrets_stay_out_of_the_heal_files() {
        // VEL-065: snapshots and heals stored typed passwords in clear.
        let secret = "vel065-Pa55w0rd-QzX";
        velocity_common::redact::register_secret(secret);
        let dir = tempfile::tempdir().unwrap();
        let (snaps, heals) = (dir.path().join("snaps.json"), dir.path().join("heals.json"));
        let healer = SelectorHealer::new(HealingConfig {
            snapshot_path: Some(snaps.clone()),
            persist_path: Some(heals.to_string_lossy().into_owned()),
            platform: Some(Platform::Android),
            ..Default::default()
        });
        let field = widget("com.app:id/pw", "EditText", Some(secret), None, 300);
        let sel = Selector::Text(secret.into());
        healer.record_success_with_context(&sel, &field, &screen(vec![field.clone()]));
        healer.record_healing(&sel.to_string(), "text=\"x\"", 0.9, &field);
        healer.settle_heals("login", true);
        healer.persist_snapshots();
        healer.persist().unwrap();
        for f in [&snaps, &heals] {
            let text = std::fs::read_to_string(f).unwrap();
            assert!(!text.contains(secret), "{}: {text}", f.display());
        }
        // The next run (same secret registered) still finds the snapshot.
        let next = SelectorHealer::new(HealingConfig {
            snapshot_path: Some(snaps),
            platform: Some(Platform::Android),
            ..Default::default()
        });
        let known = next.known_elements.lock().unwrap();
        assert!(known.contains_key(&next.key(&sel)), "{:?}", known.keys());
    }

    /// Records every formatted tracing event (fields included).
    struct Recorder(std::sync::Arc<Mutex<String>>);

    impl tracing::Subscriber for Recorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct V<'a>(&'a mut String);
            impl tracing::field::Visit for V<'_> {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!(" {}={v:?}", f.name()));
                }
            }
            event.record(&mut V(&mut self.0.lock().unwrap()));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn heal_logs_never_show_a_secret() {
        // VEL-066: the WARN line printed the interpolated selector.
        let secret = "velSixSixSecretKq";
        velocity_common::redact::register_secret(secret);
        let log = std::sync::Arc::new(Mutex::new(String::new()));
        let healer = SelectorHealer::new(HealingConfig::default());
        // Close enough to be scored, too far to heal: the WARN line.
        let options = screen(vec![widget("", "Button", Some("velSixSix"), None, 100)]);
        tracing::subscriber::with_default(Recorder(log.clone()), || {
            healer.heal_in(&Selector::Text(secret.into()), &options);
        });
        let log = log.lock().unwrap();
        assert!(log.contains("self-healing"), "{log}");
        assert!(!log.contains(secret), "{log}");
    }
}
