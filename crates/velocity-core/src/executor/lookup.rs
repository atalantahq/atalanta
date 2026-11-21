//! Finding elements: smart lookup, healing, actionability, ambiguity, scroll areas.

use super::actions::to_i32;
use super::*;

impl<'a> TestExecutor<'a> {
    /// Find an element, waiting for it to appear (up to `timeout_ms`).
    ///
    /// Waiting on the element itself is the fix the flaky-test literature
    /// keeps landing on — UI sync can report "idle" while a network response
    /// is still on its way, so a single lookup after sync is a race. The first
    /// lookup reuses the tree the sync just captured; retries back off from
    /// 100ms to 500ms and force a fresh hierarchy each time. Polling never
    /// heals — an element that hasn't rendered yet must be waited for, not
    /// healed onto a similar element from the previous screen. Healing runs
    /// once, only after the wait is exhausted.
    pub(super) async fn find_element_smart(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
        allow_healing: bool,
    ) -> Result<Element> {
        let deadline = deadline_in(timeout_ms);
        let mut attempt = 0usize;
        let mut crash_checked = Instant::now();
        self.lookup_waited = false;

        loop {
            // I3: a crash the driver already knows of (a device's crash
            // watcher) ends the wait at once, at no cost.
            if let Some(e) = self.pending_crash(device_id).await {
                return Err(e);
            }
            // A crashed app never shows the element: stop waiting as soon as
            // the crash log says so (the Android crash test took 16 s, the
            // whole element timeout, to say "crashed"). At most once a second,
            // so ordinary waits barely pay for it.
            if attempt > 0 && crash_checked.elapsed() >= crash_check_every() {
                crash_checked = Instant::now();
                let app_id = self.app_id.clone();
                if let Some(detail) = self.driver.crash_report(device_id, &app_id).await {
                    return Err(VelocityError::AppCrashed { app_id, detail });
                }
            }
            if attempt > 0 {
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                // A dialog can appear while we wait (Playwright re-runs its
                // pre-checks on every retry); its read primes the cache the
                // lookup then uses.
                self.dialog_handler
                    .dismiss_unless_target(self.driver, device_id, Some(selector))
                    .await;
            }
            match self
                .selector_engine
                .find_element_no_heal(self.driver, device_id, selector)
                .await
            {
                Ok(el) => {
                    self.trace.record_match(&el);
                    self.trace_cached_tree(device_id, !el.bounds.is_empty())
                        .await;
                    return Ok(el);
                }
                // The device is gone, or the setup is wrong: waiting for the
                // element is pointless.
                Err(e) if is_infrastructure(&e) || is_config(&e) => return Err(e),
                Err(e) => {
                    let pause = next_pause(attempt, deadline, Duration::from_millis(timeout_ms));
                    attempt += 1;
                    self.lookup_waited = true;
                    let Some(delay) = pause else {
                        // One last look that may match a part of a merged
                        // label (RN's "Points, 120, View rewards"): only now,
                        // so it never beats the real target to the screen.
                        if let Some(el) = self.find_loosely(device_id, selector).await? {
                            return Ok(el);
                        }
                        if allow_healing {
                            let heals = self.selector_engine.heal_count();
                            let healed = self
                                .selector_engine
                                .find_element(self.driver, device_id, selector)
                                .await
                                .map_err(|e| with_timeout(e, timeout_ms));
                            if let Ok(el) = &healed {
                                // `find_element` looks the selector up first
                                // and heals only on a miss: a target that
                                // arrived just now is the real one (M1).
                                let did_heal = self.selector_engine.heal_count() > heals;
                                // `healing.mode: report` (D-7): the heal is
                                // proposed, not used.
                                if did_heal
                                    && self.heal_mode == velocity_common::HealingMode::Report
                                {
                                    let err = self.heal_proposed(selector, timeout_ms);
                                    self.selector_engine.unuse_last_heal();
                                    return Err(err);
                                }
                                self.trace.record_match(el);
                            }
                            return healed;
                        }
                        return Err(with_timeout(e, timeout_ms));
                    };
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// The failure for a heal that `healing.mode: report` doesn't use: the
    /// patch it proposes and the command that applies it (D-7).
    pub(super) fn heal_proposed(&self, selector: &Selector, timeout_ms: u64) -> VelocityError {
        let suite = self.suite_file.as_deref().unwrap_or("<suite.yaml>");
        // This step's heal (M1): the last mapping from this selector to the
        // one it just healed to, never another step's or test's.
        let from = selector.to_string();
        let to = self
            .selector_engine
            .healed_to(selector)
            .map(|s| s.to_string());
        let mapping = self
            .selector_engine
            .healer()
            .healed_mappings()
            .into_iter()
            .rev()
            .find(|m| m.original_selector == from && Some(&m.healed_selector) == to.as_ref());
        let patch = match &mapping {
            Some(m) => match m.rewrite() {
                Some((key, old, new)) => format!(
                    "{key}: {old:?} → {key}: {new:?} (confidence {:.2})",
                    m.confidence
                ),
                None => format!(
                    "{} → {} (confidence {:.2}; edit by hand)",
                    m.original_selector, m.healed_selector, m.confidence
                ),
            },
            None => "the healed selector".to_string(),
        };
        let patch = velocity_common::redact::redact(&patch).into_owned();
        let reject = reject_hint(&from, suite);
        VelocityError::ElementNotFound {
            selector: format!(
                "{selector} (healing.mode is report: the heal was not used. Proposed patch: \
                 {patch}. Accept it with `velocity heal --accept --config {suite}`, or {reject})"
            ),
            timeout_ms,
            screenshot: None,
            hierarchy_snapshot: None,
        }
    }

    /// `selector` matched against parts of merged labels too (the widest
    /// tier), once, with a warning when that found what the stricter tiers
    /// didn't. Under a wider ceiling than the step's it is a no-op.
    pub(super) async fn find_loosely(
        &mut self,
        device_id: &str,
        selector: &Selector,
    ) -> Result<Option<Element>> {
        use velocity_common::selector_match::{ceiling, with_ceiling, Tier};
        if ceiling() >= Tier::Loose {
            return Ok(None);
        }
        self.selector_engine.invalidate_cache();
        self.driver.invalidate_hierarchy_cache().await;
        let found = with_ceiling(
            Tier::Loose,
            self.selector_engine
                .find_element_no_heal(self.driver, device_id, selector),
        )
        .await;
        match found {
            Ok(el) => {
                let note = format!(
                    "{selector} matched only part of an element's text or label \
                     ({}); use textContains or the whole label to say so",
                    crate::healing::describe_element(&el)
                );
                if !self.warnings.contains(&note) {
                    self.warnings.push(note);
                }
                self.loose_found = true;
                self.trace.record_match(&el);
                Ok(Some(el))
            }
            Err(e) if is_infrastructure(&e) || is_crash(&e) => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// The actionability rule for the running step (D-3): `force: true`
    /// turns the checks off, `actionability: lenient` makes them warn.
    pub(super) fn act_mode(&self) -> velocity_common::actionability::ActMode {
        use velocity_common::actionability::ActMode;
        if self.step_force {
            ActMode::Force
        } else if self.config.actionability == velocity_common::Actionability::Lenient {
            ActMode::Lenient
        } else {
            ActMode::Strict
        }
    }

    /// Find the target of a tap, press or text entry and check it can take
    /// the action (D-3): enabled, and at the same place in two reads in a
    /// row (a moving target gets the tap at its old position). Waits up to
    /// `actionableWaitMs` (2 s); then the step fails as `NotActionable`,
    /// or, with `actionability: lenient`, acts anyway with a warning (the
    /// rule before D-3, when only an unsettled screen asked for the second
    /// read). `force: true` skips both checks.
    pub(super) async fn find_actionable(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
    ) -> Result<Element> {
        self.find_actionable_as(device_id, selector, timeout_ms, true)
            .await
    }

    /// A `within:` container to drag: it must hold still, but a scroll
    /// view that reports `enabled=false` is still dragged (with a warning).
    pub(super) async fn find_container(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
    ) -> Result<Element> {
        self.find_actionable_as(device_id, selector, timeout_ms, false)
            .await
    }

    pub(super) async fn find_actionable_as(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
        must_be_enabled: bool,
    ) -> Result<Element> {
        use velocity_common::actionability::ActMode;
        self.loose_found = false;
        // Only the step's first lookup may lean on its settle.
        let settled = std::mem::take(&mut self.settled_for_lookup);
        let mut el = self
            .find_element_smart(device_id, selector, timeout_ms, true)
            .await?;
        // Counted before the bounds check: a WDA hit (no bounds) can be
        // ambiguous too (R5).
        self.note_if_ambiguous(device_id, selector, &el).await?;
        // Elements from a direct query (no bounds) have nothing to check.
        if el.bounds.is_empty() {
            return Ok(el);
        }
        let mode = self.act_mode();
        if mode == ActMode::Force {
            return Ok(el);
        }
        // Strict: proof the target holds still (VEL-072, P13). The settle
        // that just saw two identical trees in a row (or the probe's idle)
        // is that proof, when the lookup hit at once on the screen it left:
        // a second read is a full `/source` on iOS, ~540 ms a tap (review
        // M4). Only a capped or overturned settle, none at all, or a lookup
        // that had to wait asks for the re-read. Web checks stability in
        // the page (`act`'s two-frame check), so not here too. Lenient:
        // only when the screen hadn't settled by the cap.
        let proven_still = settled && !self.lookup_waited && !self.loose_found;
        let web = self.config.platform == Some(Platform::Web);
        let mut need_stable = match mode {
            ActMode::Strict => !proven_still && !web,
            _ => self.sync_manager.last_settle().capped,
        };
        if el.enabled && !need_stable {
            return Ok(el);
        }
        let limit = Instant::now() + actionable_wait().min(Duration::from_millis(timeout_ms));
        // A healed target is waited for under the selector it healed to: the
        // original one just failed, so re-querying it read as "gone" (N-3).
        let healed = self.selector_engine.healed_to(selector);
        let selector = healed.as_ref().unwrap_or(selector);
        // The first re-read comes a frame later; waits for a disabled
        // target back off as before.
        let mut attempt = if el.enabled { 0 } else { 1 };
        loop {
            tokio::time::sleep(retry_delay(attempt)).await;
            attempt += 1;
            self.selector_engine.invalidate_cache();
            self.driver.invalidate_hierarchy_cache().await;
            // A target found in the widest tier is re-read in it.
            let max = if self.loose_found {
                velocity_common::selector_match::Tier::Loose
            } else {
                velocity_common::selector_match::ceiling()
            };
            let vanished = match velocity_common::selector_match::with_ceiling(
                max,
                self.selector_engine
                    .find_element_no_heal(self.driver, device_id, selector),
            )
            .await
            {
                Ok(again) => {
                    if bounds_close(&again.bounds, &el.bounds) {
                        need_stable = false;
                    }
                    el = again;
                    false
                }
                Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => return Err(e),
                Err(_) => true,
            };
            let enabled_ok = el.enabled || !must_be_enabled;
            if enabled_ok && !need_stable && !vanished {
                if !el.enabled {
                    let note = format!("target {selector} was disabled{ACTED_ANYWAY}");
                    if !self.warnings.contains(&note) {
                        self.warnings.push(note);
                    }
                }
                return Ok(el);
            }
            if Instant::now() >= limit {
                // Never act on an element that is no longer on screen: its
                // old bounds now hold something else.
                if vanished {
                    return Err(VelocityError::ElementNotFound {
                        selector: format!(
                            "{selector} (gone or hidden, e.g. behind a sheet, while waiting \
                             for it to be actionable)"
                        ),
                        timeout_ms,
                        screenshot: None,
                        hierarchy_snapshot: None,
                    });
                }
                let why = if !enabled_ok {
                    "was disabled"
                } else {
                    "was still moving"
                };
                if mode == ActMode::Strict {
                    let waited = actionable_wait().min(Duration::from_millis(timeout_ms));
                    let reason = if enabled_ok {
                        // n1: say what was wrong. Two reads in a row with the
                        // same box (±1 px) pass, so a view caught at rest
                        // even once is tapped.
                        format!(
                            "it was still moving after {} ms: its position or size changed \
                             between every two reads (a continuously animating view: a \
                             pulsing button, a marquee, a carousel?). If tapping it mid-motion \
                             is fine, add `force: true` to the step, or set `actionability: \
                             lenient` in the suite config",
                            waited.as_millis()
                        )
                    } else {
                        format!(
                            "it {why} after {} ms. If the view works anyway (a custom or \
                             React Native view that reports enabled=false), add `force: true` \
                             to the step, or set `actionability: lenient` in the suite config",
                            waited.as_millis()
                        )
                    };
                    return Err(VelocityError::NotActionable {
                        selector: selector.to_string(),
                        reason,
                    });
                }
                let note = format!("target {selector} {why}{ACTED_ANYWAY}");
                if !self.warnings.contains(&note) {
                    self.warnings.push(note);
                }
                return Ok(el);
            }
        }
    }

    /// Warn when a bare selector matched several elements on screen: the
    /// action went to the first in tree order, which may not be the one
    /// meant. In strict mode (`selectors.strict`) it fails the step instead.
    /// Both name ready-made `childOf:` / `index:` selectors for each match.
    ///
    /// R5 (D-6): the count comes from the driver that resolves, on every
    /// driver: the shared matcher over the driver's own tree where that is
    /// how it resolves, the page itself on the web (and for an iOS element
    /// WDA found without a tree match), so the count is the one the lookup
    /// saw. It used to run only with a tree cache, and counted the web's
    /// element tree, where `className` and `id` mean something else.
    pub(super) async fn note_if_ambiguous(
        &mut self,
        device_id: &str,
        selector: &Selector,
        found: &Element,
    ) -> Result<()> {
        if matches!(selector, Selector::Index { .. }) {
            return Ok(());
        }
        let strict = self.strict_selectors();
        let Some((n, matches, picks)) = self.ambiguous_matches(device_id, selector, found).await
        else {
            return Ok(());
        };
        if n <= 1 {
            return Ok(());
        }
        let picks: Vec<String> = picks
            .iter()
            .map(|p| format!("{{ {} }}", selector_fields(p)))
            .collect();
        let pick_hint = if picks.is_empty() {
            "Add index:, or below:/childOf: to pick one".to_string()
        } else {
            format!("Pick one with {}", picks.join(" or "))
        };
        if strict {
            let listed: Vec<String> = matches
                .iter()
                .take(5)
                .map(|e| {
                    format!(
                        "{} at ({}, {})",
                        crate::healing::describe_element(e),
                        e.bounds.x,
                        e.bounds.y
                    )
                })
                .collect();
            return Err(VelocityError::AssertionFailed {
                expected: format!("{selector} to match one element (selectors.strict)"),
                actual: format!("{n} match: {}. {pick_hint}", listed.join("; ")),
                selector: selector.to_string(),
                screenshot: None,
            });
        }
        let mut note = format!(
            "{selector} matched {n} elements on screen; used {}. {pick_hint}",
            velocity_common::selector_match::BARE_PICK_RULE
        );
        if self.strict_selectors_unset() {
            note.push_str(&format!(" {STRICT_STAGING}"));
        }
        if !self.warnings.contains(&note) {
            self.warnings.push(note);
        }
        Ok(())
    }

    /// How many elements an action on `selector` could equally mean, those
    /// matches, and a selector picking each (see [`Self::note_if_ambiguous`]).
    pub(super) async fn ambiguous_matches(
        &self,
        device_id: &str,
        selector: &Selector,
        found: &Element,
    ) -> Option<(usize, Vec<Element>, Vec<Selector>)> {
        use velocity_common::{selector_match as sm, Relation};
        let web = self.config.platform == Some(Platform::Web);
        let wda_hit = found.bounds.is_empty();
        if web || wda_hit || !self.driver.has_hierarchy_cache() {
            // Count what the driver's own lookup finds: one query, as the
            // resolve was. A layout relation picks the nearest, so its
            // other matches aren't a blind choice.
            if matches!(selector, Selector::Relative { relation, .. }
                if !matches!(relation, Relation::ChildOf | Relation::ContainsChild))
            {
                return None;
            }
            // Review MAJOR 8: the count of the lookup that just resolved
            // (web counts on screen in that same page call; headless,
            // Flutter and RN on the tree they read), so a clear selector
            // costs no second lookup. Without one, only strict mode looks
            // again (and a WDA hit, which has no tree).
            let counted = if wda_hit {
                None
            } else {
                self.driver.last_match_count(device_id, selector)
            };
            match counted {
                Some(n) if n <= 1 => return Some((n, Vec::new(), Vec::new())),
                Some(_) => {}
                None if !wda_hit && !self.strict_selectors() => return None,
                None => {}
            }
            let all: Vec<Element> = self
                .driver
                .find_elements(device_id, selector)
                .await
                .ok()?
                .into_iter()
                .filter(|e| e.visible && (wda_hit || !e.bounds.is_empty()))
                .collect();
            if all.len() <= 1 || web || wda_hit {
                // No tree to name a container from: an index per match.
                let picks = (0..all.len().min(5))
                    .map(|i| Selector::Index {
                        selector: Box::new(selector.clone()),
                        index: i,
                    })
                    .collect();
                // A count above what the page holds now is stale (an
                // earlier screen's): trust the fresh list (review-fixes #6).
                let n = counted.map_or(all.len(), |n| n.min(all.len()));
                return Some((n, all, picks));
            }
            // Ambiguous on a driver that resolves on its tree: read it
            // once more for container suggestions (only now, so a clear
            // selector costs no extra read).
        }
        let tree = self.driver.get_hierarchy_cached(device_id).await.ok()?;
        let (w, h) = self.driver.screen_size(device_id).await.ok()?;
        let screen = velocity_common::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let n = sm::ambiguity(&tree, selector, Some(&screen));
        if n <= 1 {
            return Some((n, Vec::new(), Vec::new()));
        }
        let matches = sm::find_all_ref(&tree, selector, Some(&screen))
            .into_iter()
            .cloned()
            .collect();
        let picks = sm::suggest_picks(&tree, selector, Some(&screen), 5);
        Some((n, matches, picks))
    }

    pub(super) async fn screen_rect(&mut self, device_id: &str) -> Option<velocity_common::Rect> {
        self.driver
            .screen_size(device_id)
            .await
            .ok()
            .map(|(w, h)| velocity_common::Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            })
    }

    /// The on-screen part of a `within:` container, for the strokes of a
    /// scroll inside it. A container below the fold is brought onto the
    /// screen first (VEL-069: the wheel or drag went to off-screen
    /// coordinates and nothing ever moved), and its bounds are read again
    /// after the actionability check, which may itself have scrolled it in
    /// (web scrolls a target into view).
    pub(super) async fn scroll_area(
        &mut self,
        device_id: &str,
        container: &Selector,
    ) -> Result<velocity_common::Rect> {
        let timeout = self.element_timeout_ms();
        let found = self.find_container(device_id, container, timeout).await?;
        // Drivers that can scroll an element into view do it first (web,
        // VEL-069); the rest return it unchanged and the drags below apply.
        let mut bounds = self.driver.reveal(device_id, &found).await?.bounds;
        let Some(screen) = self.screen_rect(device_id).await else {
            return Ok(bounds);
        };
        // Drags to bring it in happen once per step: a `scrollUntilVisible`
        // asks for the area on every scroll (review m5).
        let drags = if self.area_dragged {
            0
        } else {
            CONTAINER_DRAGS
        };
        for attempt in 0..=drags {
            if attempt > 0 || !bounds.is_empty() {
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                match self
                    .selector_engine
                    .find_element_no_heal(self.driver, device_id, container)
                    .await
                {
                    Ok(el) => {
                        // A drag that moved nothing won't move it next time.
                        if attempt > 0 && el.bounds == bounds {
                            break;
                        }
                        bounds = el.bounds
                    }
                    Err(e) if is_infrastructure(&e) || is_crash(&e) => return Err(e),
                    Err(_) => {}
                }
            }
            if bounds.is_empty() {
                return Ok(bounds);
            }
            if let Some(shown) = shown_part(&screen, &bounds) {
                return Ok(shown);
            }
            if attempt == drags {
                break;
            }
            self.area_dragged = true;
            // Off screen on some axis: drag the page to bring its middle in.
            let (_, cy) = bounds.center();
            let axis = if cy < screen.y || cy > screen.y + screen.height {
                Direction::Down
            } else {
                Direction::Right
            };
            let Some((a, b)) = aim_at(&screen, &bounds, axis, false) else {
                break;
            };
            self.driver.scroll_coords(device_id, a, b).await?;
            let _ = self
                .sync_manager
                .wait_for_idle(self.driver, device_id)
                .await;
        }
        // Never stroke off the screen (VEL-069): what shows of it, or fail.
        intersection(&screen, &bounds).ok_or_else(|| VelocityError::NotActionable {
            selector: container.to_string(),
            reason: "the within: container is off screen and dragging didn't bring it in".into(),
        })
    }

    /// Scroll the list holding `element` up by about two rows, slowly (a
    /// drag from the middle of the list, never from the bar, and no fling),
    /// so a row a bottom bar covers comes out from under it. False when the
    /// element isn't in a scrolling container (nothing to scroll: a map or a
    /// canvas would pan instead) or the drag failed.
    pub(super) async fn nudge_into_view(&mut self, device_id: &str, element: &Element) -> bool {
        let Ok(tree) = self.driver.get_hierarchy_cached(device_id).await else {
            return false;
        };
        let Some(list) = scroll_container_of(&tree, element) else {
            return false;
        };
        let b = list.bounds;
        let x = element.bounds.center().0;
        let from_y = b.y + b.height * 45 / 100;
        let shift = (element.bounds.height * 2 + 80).min(b.height / 3);
        let moved = self
            .driver
            .drag(device_id, (x, from_y), (x, from_y - shift), 0, 600)
            .await
            .is_ok();
        self.selector_engine.invalidate_cache();
        self.driver.invalidate_hierarchy_cache().await;
        moved
    }
}

/// The part of `b` on `screen`, when enough of it shows to stroke across:
/// on each axis, at least a quarter of the screen or the whole of `b`.
pub(super) fn shown_part(
    screen: &velocity_common::Rect,
    b: &velocity_common::Rect,
) -> Option<velocity_common::Rect> {
    let x0 = b.x.max(screen.x);
    let y0 = b.y.max(screen.y);
    let x1 =
        b.x.saturating_add(b.width)
            .min(screen.x.saturating_add(screen.width));
    let y1 =
        b.y.saturating_add(b.height)
            .min(screen.y.saturating_add(screen.height));
    let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    let enough = |shown: i32, whole: i32, len: i32| shown >= whole.min(len / 4).max(1);
    (enough(w, b.width, screen.width) && enough(h, b.height, screen.height)).then_some(
        velocity_common::Rect {
            x: x0,
            y: y0,
            width: w,
            height: h,
        },
    )
}

/// A drag that brings `target`, off `area` along the scroll axis, to the
/// middle of `area`: the finger moves the gap (at most 60% of the area, so
/// the stroke stays inside it; a longer gap takes several drags). `None`
/// when the target is empty or already inside the area on that axis.
pub(super) fn aim_at(
    area: &velocity_common::Rect,
    target: &velocity_common::Rect,
    scroll: Direction,
    on_screen: bool,
) -> Option<((i32, i32), (i32, i32))> {
    if target.width <= 0 || target.height <= 0 {
        return None;
    }
    let vertical = matches!(scroll, Direction::Up | Direction::Down);
    let w = i64::from;
    let (lo, len, t) = if vertical {
        (
            w(area.y),
            w(area.height),
            w(target.y) + w(target.height) / 2,
        )
    } else {
        (w(area.x), w(area.width), w(target.x) + w(target.width) / 2)
    };
    // On screen but in the outer band: under a navigation bar or the home
    // indicator. A tap there hit the status bar, which scrolls an iOS list
    // back to the top ("Item 137" tapped at y=65). Bring it in first.
    let band = if on_screen { len * 12 / 100 } else { 0 };
    if len <= 0 || (t >= lo + band && t <= lo + len - band) {
        return None;
    }
    let mid = lo + len / 2;
    let gap = (t - mid).clamp(-len * 3 / 5, len * 3 / 5);
    let (a, b) = (to_i32(mid + gap / 2), to_i32(mid - gap / 2));
    let cross = if vertical {
        to_i32(w(area.x) + w(area.width) / 2)
    } else {
        to_i32(w(area.y) + w(area.height) / 2)
    };
    Some(if vertical {
        ((cross, a), (cross, b))
    } else {
        ((a, cross), (b, cross))
    })
}

/// The part of `b` on `screen`, if any.
pub(super) fn intersection(
    screen: &velocity_common::Rect,
    b: &velocity_common::Rect,
) -> Option<velocity_common::Rect> {
    let x0 = b.x.max(screen.x);
    let y0 = b.y.max(screen.y);
    let x1 =
        b.x.saturating_add(b.width)
            .min(screen.x.saturating_add(screen.width));
    let y1 =
        b.y.saturating_add(b.height)
            .min(screen.y.saturating_add(screen.height));
    (x1 > x0 && y1 > y0).then(|| velocity_common::Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// How find_actionable's "acted anyway" warning ends.
pub(super) const ACTED_ANYWAY: &str = "; acted on it anyway";

/// Drags allowed to bring a `within:` container onto the screen.
pub(super) const CONTAINER_DRAGS: usize = 5;

/// A tap the driver refused because a bar at the bottom of the screen (a
/// toolbar or tab bar) covers the target.
pub(super) fn under_bottom_bar(e: &VelocityError) -> bool {
    matches!(e, VelocityError::AssertionFailed { actual, .. }
        if actual.starts_with("covered by the toolbar")
            || actual.starts_with("covered by the tab bar"))
}

/// The innermost scrolling container (a table, collection or scroll view)
/// that holds `element`'s centre.
pub(super) fn scroll_container_of<'t>(tree: &'t Element, element: &Element) -> Option<&'t Element> {
    let (cx, cy) = element.bounds.center();
    let holds = |n: &Element| {
        let b = n.bounds;
        cx >= b.x && cx < b.x + b.width && cy >= b.y && cy < b.y + b.height
    };
    let scrolls = |n: &Element| {
        matches!(
            n.element_type.as_str(),
            "Table"
                | "CollectionView"
                | "ScrollView"
                | "XCUIElementTypeTable"
                | "XCUIElementTypeCollectionView"
                | "XCUIElementTypeScrollView"
        )
    };
    // The deepest one: walk everything (a container's own frame may not
    // hold its children's, e.g. a root reported with a zero frame).
    let mut found: Option<(usize, &Element)> = None;
    let mut stack = vec![(tree, 0usize)];
    while let Some((n, depth)) = stack.pop() {
        if scrolls(n) && holds(n) && found.is_none_or(|(d, _)| depth >= d) {
            found = Some((depth, n));
        }
        stack.extend(n.children.iter().map(|c| (c, depth + 1)));
    }
    found.map(|(_, n)| n)
}
