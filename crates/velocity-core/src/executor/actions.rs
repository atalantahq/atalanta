//! Running one action on the driver, and the exec and http steps.

use super::*;

impl<'a> TestExecutor<'a> {
    /// Execute a concrete action with pre/post sync, dialog handling and
    /// resource profiling.
    pub(super) async fn run_action(
        &mut self,
        action: &Action,
        device_id: &str,
        app_id: &str,
    ) -> (Result<()>, Option<ResourceDelta>) {
        // Pre-action sync (skip for wait, screenshot, and app lifecycle actions)
        let skip_sync = matches!(
            action,
            Action::Wait { .. }
                | Action::Screenshot { .. }
                | Action::LaunchApp { .. }
                | Action::StopApp { .. }
                | Action::Goto { .. }
                | Action::GoBack
                | Action::Refresh
                | Action::SetConfig { .. }
                | Action::MockRequest { .. }
                | Action::ClearMocks
                | Action::Http(_)
                | Action::Exec(_)
        );
        let settle_off = self.step_settle.as_ref().is_some_and(|s| s.skip);
        self.settled_for_lookup = false;
        if !skip_sync && !settle_off {
            // Dismiss any system dialogs before waiting for idle, unless the
            // step itself targets the dialog ("tap: Don't Allow").
            self.dialog_handler
                .dismiss_unless_target(self.driver, device_id, action_selector(action))
                .await;
            // The lookup after a stable settle skips the strict re-read
            // (fd3), which is only sound on a tree read after the settle
            // started. Android's settle compares lite dumps that never fill
            // the lookup cache, so a tree cached by the dialog check above or
            // by an earlier `assertVisible` (TTL 2 s) could be from mid-slide
            // and get tapped at stale coordinates (review-fixes #1). Drop it
            // here: on iOS the settle's own reads refill it (no extra read);
            // on Android the lookup reads once, fresh.
            self.driver.invalidate_hierarchy_cache().await;
            self.selector_engine.invalidate_cache();

            // Collect pre-action metrics concurrently with sync (zero added latency)
            let sync_mgr = &mut self.sync_manager;
            let driver = self.driver;
            let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

            let snap_fut = async {
                match &pkg {
                    Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                    None => None,
                }
            };

            let (sync_result, raw_before) =
                tokio::join!(sync_mgr.wait_for_idle(driver, device_id), snap_fut);
            // Best-effort, like post-action sync: a screen that never stops
            // animating (spinner, carousel) must not fail the step by itself.
            // The element lookup below still waits for its target.
            self.step_settle_ms += self.sync_manager.last_settle().duration.as_millis() as u64;
            self.trace_settle();
            let sync_result = match sync_result {
                // The screen can't be read for a reason no wait fixes ("add
                // a launchApp step"): the action can't do better, so fail
                // with that now instead of after its lookup's whole wait.
                // Native mode's probe errors are config-kind too, and stay
                // best-effort.
                Err(e) if is_config(&e) && !self.sync_manager.probe_errors_surface() => {
                    return (Err(e), None)
                }
                other => other,
            };
            if let Err(e) = &sync_result {
                tracing::debug!("pre-action sync: {e}");
            }
            if sync_result.is_err() || self.sync_manager.last_settle().capped {
                self.note_unsettled("before", action);
            }
            let st = self.sync_manager.last_settle();
            self.settled_for_lookup = sync_result.is_ok() && st.stable && !st.capped;

            if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_before) {
                profiler.record_before(ResourceProfiler::snapshot_from_raw(raw));
            }
        }

        // D-2: whether the test is leaving the suite's app on purpose.
        match action {
            Action::Goto { .. }
            | Action::PressKey {
                key: velocity_common::Key::Home,
            } => self.app_switched = true,
            Action::LaunchApp { app_id: a, .. } => {
                self.app_switched = !a.is_empty() && a != &self.app_id;
            }
            _ => {}
        }
        // Drivers that check actionability themselves (web) follow the
        // step's rule.
        let mode = self.act_mode();
        // Boxed: `execute_action` holds every action's state (~9 KB), which
        // otherwise sits inline in each of `run_action`'s callers.
        let result = velocity_common::actionability::with_mode(
            mode,
            Box::pin(self.execute_action(action, device_id, app_id)),
        )
        .await;
        // Anything but a read may have changed whether the app is alive.
        if !is_read_only(action) {
            self.liveness_seen = None;
        }

        // Post-action sync for actions that modify UI state
        let mutating = is_mutating(action);
        if !mutating {
            // Nothing settled after it, so the screen may have moved
            // (review-s8 m-d).
            self.sync_manager.forget_picture();
        }

        if let Err(e) = result {
            // The driver refused the target (VEL-274): it didn't act on it
            // anyway, whatever find_actionable's warning said.
            if matches!(e, VelocityError::NotActionable { .. }) {
                if let Some(sel) = action_selector(action) {
                    let prefix = format!("target {sel} ");
                    self.warnings
                        .retain(|w| !(w.starts_with(&prefix) && w.ends_with(ACTED_ANYWAY)));
                }
            }
            return (Err(e), None);
        }
        let mut resource_delta = None;
        if mutating && settle_off {
            // `settle: none`: act and move on (the next lookup still waits
            // for its element).
            self.selector_engine.invalidate_cache();
            self.sync_manager.invalidate();
            self.driver.invalidate_hierarchy_cache().await;
            return (Ok(()), None);
        }
        if matches!(action, Action::ScrollUntilVisible { .. }) {
            // Audit #11: the loop settled after its last swipe, and with no
            // swipe nothing moved. A second settle here saw no change and
            // so waited out the whole reaction floor plus its reads (6-9 s
            // a suite). The sync engine keeps the loop's settled screen, so
            // the next step's wait confirms it with one read; only the
            // lookup caches go, as after any action.
            self.selector_engine.invalidate_cache();
            self.driver.invalidate_hierarchy_cache().await;
            if let Some(pkg) = self.profiler.as_ref().map(|p| p.package().to_string()) {
                if let Ok(raw) = self.driver.collect_resource_metrics(device_id, &pkg).await {
                    let snap = ResourceProfiler::snapshot_from_raw(raw);
                    resource_delta = self.profiler.as_mut().and_then(|p| p.record_after(snap));
                }
            }
            return (Ok(()), resource_delta);
        }
        if mutating {
            self.selector_engine.invalidate_cache();
            self.sync_manager.invalidate();
            self.driver.invalidate_hierarchy_cache().await;

            // Collect post-action metrics concurrently with sync
            let sync_mgr = &mut self.sync_manager;
            let driver = self.driver;
            let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

            let snap_fut = async {
                match &pkg {
                    Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                    None => None,
                }
            };

            // A crashed app leaves the launcher in front, whose live
            // wallpaper never settles: the wait ran to its 5 s cap before the
            // crash was seen. Watch the crash log alongside, from 1 s on (a
            // normal settle is done by then and pays nothing). Dropping the
            // settle is safe: tree-server calls are cancel-safe (fix 34).
            let app_id = self.app_id.clone();
            let crash_watch = async {
                tokio::time::sleep(crash_watch_after()).await;
                loop {
                    if let Some(detail) = driver.crash_report(device_id, &app_id).await {
                        return detail;
                    }
                    tokio::time::sleep(crash_watch_every()).await;
                }
            };
            let settle_or_crash = async {
                tokio::select! {
                    r = sync_mgr.wait_for_idle(driver, device_id) => Ok(r),
                    detail = crash_watch => Err(detail),
                }
            };
            let (outcome, raw_after) = tokio::join!(settle_or_crash, snap_fut);
            let sync_result = match outcome {
                Ok(r) => r,
                Err(detail) => {
                    return (
                        Err(VelocityError::AppCrashed {
                            app_id: self.app_id.clone(),
                            detail,
                        }),
                        None,
                    )
                }
            };
            // Best-effort: a screen that keeps animating doesn't fail the
            // step, but the report says so.
            self.step_settle_ms += self.sync_manager.last_settle().duration.as_millis() as u64;
            self.trace_settle();
            if sync_result.is_err() || self.sync_manager.last_settle().capped {
                // A crashed app leaves the launcher in front, whose live
                // wallpaper never lets the screen settle: the cap is the first
                // sign. Say so here instead of in the next step's wait (the
                // crash test then spent another 1.7 s and more before failing).
                // Only on this slow path, so a normal step pays nothing.
                let app_id = self.app_id.clone();
                if let Some(detail) = self.driver.crash_report(device_id, &app_id).await {
                    return (Err(VelocityError::AppCrashed { app_id, detail }), None);
                }
                self.note_unsettled("after", action);
            }

            if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_after) {
                resource_delta = profiler.record_after(ResourceProfiler::snapshot_from_raw(raw));
            }

            // Check for system dialogs after the action.
            // Skip for coordinate-based actions (TapXY, InputRaw) — these target
            // canvas/WebView elements that never trigger system permission dialogs.
            let can_trigger_dialog =
                !matches!(action, Action::TapXY { .. } | Action::InputRaw { .. });
            if can_trigger_dialog {
                self.dialog_handler
                    .dismiss_if_present(self.driver, device_id)
                    .await;
            }
        }
        (Ok(()), resource_delta)
    }

    pub(super) async fn execute_action(
        &mut self,
        action: &Action,
        device_id: &str,
        app_id: &str,
    ) -> Result<()> {
        match action {
            Action::MockRequest { rule } => {
                self.mocked = true;
                self.driver.add_mock(device_id, rule.clone()).await
            }
            Action::ClearMocks => self.driver.clear_mocks(device_id).await,
            Action::Http(req) => self.run_http(req).await,
            Action::Exec(e) => self.run_exec(e, device_id).await,
            Action::SetConfig { values } => {
                // Remember what the test started with, once: the test's end
                // puts it back (`restore_test_config`).
                if self.config_before.is_none() {
                    self.config_before = Some((
                        self.config.sync.clone(),
                        velocity_common::env::settings_snapshot(),
                    ));
                }
                let mut named = velocity_common::env::settings_snapshot();
                for (k, v) in values {
                    check_set_config(k, v).map_err(VelocityError::Config)?;
                    if let Some(s) = velocity_common::settings::find(k) {
                        named.insert(s.env.to_string(), v.clone());
                    } else {
                        set_sync_field(&mut self.config.sync, k, v)
                            .map_err(VelocityError::Config)?;
                    }
                }
                velocity_common::env::set_settings(named);
                *self.sync_manager.sync_config_mut() = self.config.sync.clone();
                Ok(())
            }
            Action::AssertSystemState {
                media_playing,
                media_title,
                in_call,
            } => {
                // Polled like any assertion: playback or a call can start a
                // moment after the tap that caused it.
                let deadline = self.assertion_deadline();
                let mut attempt = 0usize;
                loop {
                    let state = self.driver.system_state(device_id).await?;
                    let problems = system_state_mismatch(
                        &state,
                        *media_playing,
                        media_title.as_deref(),
                        *in_call,
                    );
                    if problems.is_empty() {
                        return Ok(());
                    }
                    let pause = next_pause(
                        attempt,
                        deadline,
                        Duration::from_millis(self.element_timeout_ms()),
                    );
                    attempt += 1;
                    let Some(delay) = pause else {
                        return Err(VelocityError::AssertionFailed {
                            expected: problems.join("; "),
                            actual: format!(
                                "media playing={} titles={:?}, in call={}",
                                state.media_playing, state.media_titles, state.in_call
                            ),
                            selector: "system state".into(),
                            screenshot: None,
                        });
                    };
                    tokio::time::sleep(delay).await;
                }
            }
            Action::LaunchApp {
                app_id: override_id,
                clear_state,
                arguments,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                // A stronger reset (reinstall / snapshot) replaces the clear;
                // if it can't run, fall back to the plain clear.
                let mut clear = *clear_state;
                if clear && self.config.reset.strategy != velocity_common::ResetStrategy::Clear {
                    match self
                        .driver
                        .reset_app(device_id, id, &self.config.reset)
                        .await
                    {
                        Ok(done) => {
                            tracing::debug!(?done, "app reset");
                            clear = false;
                        }
                        Err(e) => {
                            let note = format!(
                                "reset strategy {:?} unavailable ({e}); used clear instead",
                                self.config.reset.strategy
                            );
                            if !self.warnings.contains(&note) {
                                self.warnings.push(note);
                            }
                        }
                    }
                }
                let result = self
                    .driver
                    .launch_app_with(device_id, id, clear, arguments)
                    .await;
                if result.is_ok() {
                    // Another app in front: the suite's app is no longer
                    // the one a batched next test would find.
                    self.app_stopped = id != self.app_id;
                    if id == self.app_id {
                        // A fresh process: an earlier crash is history.
                        self.sticky_crash = None;
                    }
                    // The probe binds its port inside the freshly launched app —
                    // re-probe native sync instead of trusting a pre-launch failure.
                    self.sync_manager.reset_native();
                }
                result
            }
            Action::StopApp {
                app_id: override_id,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                let result = self.driver.stop_app(device_id, id).await;
                if result.is_ok() && id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::Tap { selector } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                match self.driver.tap(device_id, &element).await {
                    // A row under a floating bottom bar (iOS 26 Settings'
                    // search bar) was refused with "scroll it into view
                    // first": scroll its list once, slowly, and tap again.
                    Err(e) if under_bottom_bar(&e) => {
                        if !self.nudge_into_view(device_id, &element).await {
                            return Err(e);
                        }
                        let element = self.find_actionable(device_id, selector, timeout).await?;
                        self.driver.tap(device_id, &element).await
                    }
                    other => other,
                }
            }
            Action::DoubleTap { selector } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.double_tap(device_id, &element).await
            }
            Action::LongPress {
                selector,
                duration_ms,
            } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                let ms = duration_ms.unwrap_or(1000);
                self.driver.long_press(device_id, &element, ms).await
            }
            Action::Drag {
                from,
                to,
                hold_ms,
                move_ms,
            } => {
                let timeout = self.element_timeout_ms();
                let a = self.find_actionable(device_id, from, timeout).await?;
                let b = self.find_container(device_id, to, timeout).await?;
                self.driver
                    .drag(
                        device_id,
                        a.bounds.center(),
                        b.bounds.center(),
                        hold_ms.unwrap_or(DRAG_HOLD_MS),
                        move_ms.unwrap_or(DRAG_MOVE_MS),
                    )
                    .await
            }
            Action::InputText { selector, text } => {
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.input_text(device_id, &element, text).await
            }
            Action::ClearText { selector } => {
                // Like inputText: RN's clearText taps the field first, so it
                // must be actionable, not just present (review MINOR 11).
                let timeout = self.element_timeout_ms();
                let element = self.find_actionable(device_id, selector, timeout).await?;
                self.driver.clear_text(device_id, &element).await
            }
            Action::AssertVisible { selector } => {
                // Waits for visible, not just present (Playwright's toBeVisible,
                // Maestro's assertVisible): on the web a matching node is often
                // in the DOM but still hidden for a moment — a screen that the
                // next frame shows — and failing on the first read lost that race.
                let timeout = self.element_timeout_ms();
                let deadline = deadline_in(timeout);
                let mut attempt = 0usize;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    self.loose_found = false;
                    let element = self
                        .find_element_smart(device_id, selector, left.as_millis() as u64, false)
                        .await?;
                    // D-10: one part of a merged label ("Home, tab, 1 of 4")
                    // is not the element the assertion names: fail, naming
                    // the label it was part of.
                    if self.loose_found {
                        self.loose_found = false;
                        return Err(VelocityError::AssertionFailed {
                            expected: "visible".to_string(),
                            actual: format!(
                                "only part of the merged label {:?} matched ({}); assert \
                                 the whole label, or use textContains to mean a part",
                                merged_label(&element),
                                crate::healing::describe_element(&element)
                            ),
                            selector: format!("{selector}"),
                            screenshot: None,
                        });
                    }
                    if self.driver.is_element_visible(device_id, &element).await? {
                        return Ok(());
                    }
                    let pause = next_pause(attempt, deadline, Duration::from_millis(timeout));
                    attempt += 1;
                    let Some(delay) = pause else {
                        return Err(VelocityError::AssertionFailed {
                            expected: "visible".to_string(),
                            actual: format!("present but not visible after {timeout}ms"),
                            selector: format!("{selector}"),
                            screenshot: None,
                        });
                    };
                    tokio::time::sleep(delay).await;
                    self.selector_engine.invalidate_cache();
                    self.driver.invalidate_hierarchy_cache().await;
                }
            }
            Action::AssertNotVisible { selector } => {
                // Passes as soon as the element is gone; waits (up to the sync
                // timeout) while it's still there — a spinner or toast that is
                // on its way out shouldn't fail the step.
                // Never heal here: healing onto a similar element would make a
                // legitimately-removed element look "still visible".
                let deadline = self.assertion_deadline();
                let mut poll = Duration::from_millis(100);
                // Only a successful read without the element means "gone".
                // A lookup that failed (a garbled dump, a dead session) says
                // nothing about the screen: keep reading, and if no read
                // works by the deadline, fail with that error.
                let mut unreadable = None;
                let gone = loop {
                    let gone = match self
                        .selector_engine
                        .find_element_no_heal(self.driver, device_id, selector)
                        .await
                    {
                        Err(e) if is_not_found(&e) => true,
                        // A lost device or a crashed app is not "the element is gone".
                        Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                            return Err(e)
                        }
                        Err(e) => {
                            unreadable = Some(e);
                            false
                        }
                        Ok(element) => {
                            unreadable = None;
                            !self.driver.is_element_visible(device_id, &element).await?
                        }
                    };
                    if gone || !self.poll_pause(&mut poll, deadline).await {
                        break gone;
                    }
                };
                if let (false, Some(e)) = (gone, unreadable) {
                    return Err(e);
                }
                // A crashed app shows none of its elements: that's not a pass.
                if gone {
                    if let Some(e) = self.check_alive(device_id).await {
                        return Err(e);
                    }
                    // Another app's screen lacks the element too: that is no
                    // pass (D-2, VEL-014), unless the step says so
                    // (`appInFront: false`) or the test itself left the app
                    // (openLink/goto, another app's launchApp, the home key).
                    if let Some(velocity_common::Liveness::Background(front)) = &self.liveness_seen
                    {
                        let front = front.as_deref().unwrap_or("another app").to_string();
                        if self.step_app_in_front == Some(false) || self.app_switched {
                            let note = format!(
                                "assertNotVisible {selector}: the app under test wasn't in front ({front} was); \
                                 the element was checked on that screen"
                            );
                            if !self.warnings.contains(&note) {
                                self.warnings.push(note);
                            }
                        } else {
                            return Err(VelocityError::AssertionFailed {
                                expected: format!(
                                    "not visible in {} (the app under test)",
                                    self.app_id
                                ),
                                actual: format!(
                                    "{front} was in front, so the check proves nothing; \
                                     add `appInFront: false` to the step if that is intended"
                                ),
                                selector: format!("{selector}"),
                                screenshot: None,
                            });
                        }
                    }
                }
                if !gone {
                    return Err(VelocityError::AssertionFailed {
                        expected: "not visible".to_string(),
                        actual: "visible".to_string(),
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::AssertText { selector, expected } => {
                // Text often settles after the element appears (a counter, a
                // price loading in), so keep reading until it matches.
                let timeout = self.element_timeout_ms();
                self.loose_found = false;
                self.find_element_smart(device_id, selector, timeout, false)
                    .await?;
                // Found only as part of a merged label at the deadline: read
                // that element, not one the polling tier never sees (which
                // waited a second timeout and failed with "": review-s8 m-b).
                let tier = if self.loose_found {
                    velocity_common::selector_match::Tier::Loose
                } else {
                    velocity_common::selector_match::ceiling()
                };
                let deadline = self.assertion_deadline();
                let mut poll = Duration::from_millis(100);
                let mut actual = String::new();
                let matched = loop {
                    let found = velocity_common::selector_match::with_ceiling(
                        tier,
                        self.selector_engine
                            .find_element_no_heal(self.driver, device_id, selector),
                    )
                    .await;
                    match found {
                        Ok(element) => {
                            // The driver compares: on web both sides are
                            // normalised alike (M-1).
                            let (hit, text) = self
                                .driver
                                .element_text_matches(device_id, &element, expected)
                                .await?;
                            actual = text;
                            if hit {
                                break true;
                            }
                        }
                        // A lost device is not a wrong text.
                        Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                            return Err(e)
                        }
                        Err(_) => {}
                    }
                    if !self.poll_pause(&mut poll, deadline).await {
                        break false;
                    }
                };
                if !matched {
                    return Err(VelocityError::AssertionFailed {
                        expected: expected.clone(),
                        actual,
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::ScrollUntilVisible {
                selector,
                direction,
                max_scrolls,
                within,
            } => {
                // `direction` is where the content scrolls (Maestro's meaning:
                // `down` reveals what is below), so the finger moves the other
                // way. It used to be passed to `swipe`, whose direction is the
                // finger's: `down` dragged the list back to the top and nothing
                // below the fold was ever found (VelocityLab, Android and iOS).
                let finger = finger_for_scroll(*direction);
                let started = std::time::Instant::now();
                let mut last_aimed: Option<velocity_common::Rect> = None;
                for _ in 0..*max_scrolls {
                    // The container first: bringing it on screen can scroll
                    // the page (web's `reveal`), and the target read before
                    // that was aimed at with stale bounds (review M-1).
                    let within_area = match within {
                        Some(container) => Some(self.scroll_area(device_id, container).await?),
                        None => None,
                    };
                    // No healing while scrolling — a healed near-match would
                    // stop the scroll before the real element is on screen.
                    let found = self
                        .selector_engine
                        .find_element_no_heal(self.driver, device_id, selector)
                        .await;
                    if let Err(e) = &found {
                        if is_infrastructure(e) || is_crash(e) || is_config(e) {
                            return found.map(|_| ());
                        }
                    }
                    let mut aimed = None;
                    if let Ok(element) = found {
                        let visible = self
                            .driver
                            .is_element_visible(device_id, &element)
                            .await
                            .unwrap_or(false);
                        // Already in the tree but off screen (iOS lists keep
                        // ~20 rows past each edge): drag straight to it. A
                        // fling moves more than a screen, so a row could pass
                        // from below the screen to above it between two
                        // reads and never be "visible" (the scroll ran past
                        // row_150 to the end).
                        let area = match within_area {
                            Some(area) => Some(area),
                            None => self.screen_rect(device_id).await,
                        };
                        aimed = area.and_then(|a| aim_at(&a, &element.bounds, *direction, visible));
                        if visible && aimed.is_none() {
                            return Ok(());
                        }
                        // VEL-068: the last drag didn't move it (a fixed
                        // header, a screen that can't scroll, the end of a
                        // list): it is as far in as it will get.
                        if visible && aimed.is_some() && last_aimed == Some(element.bounds) {
                            return Ok(());
                        }
                        last_aimed = aimed.map(|_| element.bounds);
                    }
                    self.selector_engine.invalidate_cache();
                    if let Some((a, b)) = aimed {
                        self.driver.scroll_coords(device_id, a, b).await?;
                        let _ = self
                            .sync_manager
                            .wait_for_idle(self.driver, device_id)
                            .await;
                        continue;
                    }
                    match within_area {
                        // Drag the named container, not the middle of the
                        // screen: on a two-pane tablet the middle is the other
                        // pane (a POS tablet's Settings list never moved).
                        Some(area) => {
                            let (a, b) = line_across(&area, finger);
                            self.driver.scroll_coords(device_id, a, b).await?;
                        }
                        None => self.driver.scroll(device_id, finger).await?,
                    }
                    let _ = self
                        .sync_manager
                        .wait_for_idle(self.driver, device_id)
                        .await;
                }
                // The last scroll may have revealed it: look once more.
                self.selector_engine.invalidate_cache();
                self.driver.invalidate_hierarchy_cache().await;
                match self
                    .selector_engine
                    .find_element_no_heal(self.driver, device_id, selector)
                    .await
                {
                    Ok(element) => {
                        if self
                            .driver
                            .is_element_visible(device_id, &element)
                            .await
                            .unwrap_or(false)
                        {
                            return Ok(());
                        }
                    }
                    Err(e) if is_infrastructure(&e) || is_crash(&e) || is_config(&e) => {
                        return Err(e)
                    }
                    Err(_) => {}
                }
                Err(VelocityError::ElementNotFound {
                    selector: format!("{selector} ({max_scrolls} scrolls {direction:?})"),
                    timeout_ms: started.elapsed().as_millis() as u64,
                    screenshot: None,
                    hierarchy_snapshot: None,
                })
            }
            Action::Swipe {
                direction,
                from,
                to,
                within,
            } => {
                if let (Some(from), Some(to)) = (from, to) {
                    self.driver.swipe_coords(device_id, *from, *to).await
                } else if let Some(container) = within {
                    // Across the element (a carousel above the fold): the
                    // middle of the screen missed it and nothing moved.
                    let timeout = self.element_timeout_ms();
                    let el = self.find_container(device_id, container, timeout).await?;
                    let dir = direction.unwrap_or(Direction::Down);
                    let (a, b) = if is_slider(&el) {
                        slider_stroke(&el.bounds, dir)
                    } else {
                        line_across(&el.bounds, dir)
                    };
                    self.driver.swipe_coords(device_id, a, b).await
                } else {
                    let dir = direction.unwrap_or(Direction::Down);
                    // One stroke on every device driver: each drew its own
                    // (Android 40%, iOS/RN half, Flutter a third of the
                    // screen), so a suite scrolled further on one platform
                    // than another (VEL-S53). Web scrolls the page by its
                    // own rule; a driver with no screen size keeps its own.
                    let size = if self.config.platform == Some(Platform::Web) {
                        None
                    } else {
                        self.driver.screen_size(device_id).await.ok()
                    };
                    match size {
                        Some((w, h)) if w > 0 && h > 0 => {
                            let (a, b) = default_swipe_line(w, h, dir);
                            self.driver.swipe_coords(device_id, a, b).await
                        }
                        _ => self.driver.swipe(device_id, dir).await,
                    }
                }
            }
            Action::Screenshot { filename } => {
                use velocity_common::ScreenshotPolicy;
                let policy = self.config.artifacts.screenshots;
                if policy == ScreenshotPolicy::Never {
                    return Ok(());
                }
                let data = self.driver.screenshot(device_id).await?;
                // The file name only: `../x.png` or an absolute path wrote
                // outside the artifacts dir (VEL-197).
                let fname = filename
                    .as_deref()
                    .and_then(|f| std::path::Path::new(f).file_name())
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("screenshot_{}.png", chrono_millis()));
                let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
                if policy == ScreenshotPolicy::OnFailure {
                    self.held_shots.push((path, data));
                    return Ok(());
                }
                // Best-effort write; don't fail the step if artifacts dir doesn't exist
                write_artifact(&path, data).await;
                Ok(())
            }
            // Both leave the app stopped on purpose (see the trait): its
            // exit is not a crash, and the next test must launch it.
            Action::SaveState { name } => {
                let result = self.driver.save_app_state(device_id, app_id, name).await;
                if result.is_ok() && app_id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::RestoreState { name } => {
                let result = self.driver.restore_app_state(device_id, app_id, name).await;
                if result.is_ok() && app_id == self.app_id {
                    self.app_stopped = true;
                }
                result
            }
            Action::AssertScreenshot {
                name,
                threshold,
                cluster_px,
                mask,
                tolerance,
                comparator,
                crop,
            } => {
                let shot = ScreenshotStep {
                    threshold: *threshold,
                    cluster_px: *cluster_px,
                    mask,
                    rule: crate::visual::PixelRule {
                        comparator: comparator.unwrap_or_default(),
                        tolerance: tolerance
                            .unwrap_or(crate::visual::PixelRule::default().tolerance),
                    },
                    crop: crop.as_ref(),
                };
                self.assert_screenshot(device_id, name, shot).await
            }
            Action::PressKey { key } => self.driver.press_key(device_id, key.clone()).await,
            Action::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                Ok(())
            }
            Action::RunFlow { .. } => {
                // Flows should be resolved before execution
                Err(VelocityError::Config(
                    "runFlow encountered during execution; flows must be resolved first"
                        .to_string(),
                ))
            }
            Action::TapXY { x, y } => self.driver.tap_xy(device_id, *x, *y).await,
            Action::InputRaw { text } => self.driver.input_raw(device_id, text).await,
            Action::Goto { url } => self.driver.navigate(device_id, url).await,
            Action::GoBack => self.driver.go_back(device_id).await,
            Action::Refresh => self.driver.refresh_page(device_id).await,
            // Natural steps are resolved to a concrete action before execute_action
            // is reached; seeing one here means resolution was skipped.
            Action::Natural { .. } => Err(VelocityError::Internal(anyhow::anyhow!(
                "internal: natural-language step reached executor unresolved"
            ))),
        }
    }

    /// An `http` step: sent through the test's session (its cookie jar),
    /// and what it captured kept for the test's later steps.
    /// `exec` (row 3.2): an allowed program on the host, no shell; its exit
    /// code and output checked, its output captured. The allow-list is
    /// checked again here, whatever validated the suite.
    pub(super) async fn run_exec(
        &mut self,
        e: &velocity_common::exec::ExecStep,
        device_id: &str,
    ) -> Result<()> {
        if let Some(why) = e.refused_by(self.config.exec.as_ref()) {
            return Err(VelocityError::Config(why));
        }
        let argv = e.run.argv();
        // A relative program path (`./scripts/seed.sh`) is the one the
        // allow-list names from the run's directory: resolve it here, before
        // `cwd` (which can be interpolated) moves the child elsewhere and
        // picks another file of the same name (audit 2026-11 EXEC-2).
        let program = {
            let p = std::path::Path::new(&argv[0]);
            if e.cwd.is_some() && p.is_relative() && p.components().count() > 1 {
                std::env::current_dir()
                    .map(|d| d.join(p))
                    .unwrap_or_else(|_| p.to_path_buf())
            } else {
                p.to_path_buf()
            }
        };
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&argv[1..]).env("VELOCITY_DEVICE_ID", device_id);
        if let Some(dir) = &e.cwd {
            cmd.current_dir(dir);
        }
        let limit =
            std::time::Duration::from_millis(self.step_timeout_ms.unwrap_or(EXEC_TIMEOUT_MS));
        // Its own process group, killed whole on timeout, on cancel and on
        // a second Ctrl-C: `kill_on_drop` alone killed only `sh` and left
        // the script's children running (review 01 #3, VEL-074).
        let out = match velocity_common::process::output_with_timeout(&mut cmd, limit).await {
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                return Err(VelocityError::Config(format!(
                    "exec `{}` did not finish within {} ms (timeoutMs)",
                    argv[0],
                    limit.as_millis()
                )))
            }
            Err(err) => return Err(VelocityError::Config(format!("exec `{}`: {err}", argv[0]))),
            Ok(o) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = |s: &str| {
            let t: String = s
                .chars()
                .rev()
                .take(600)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            velocity_common::redact::redact(t.trim()).into_owned()
        };
        let want = e.expect.exit_code.unwrap_or(0);
        let code = out.status.code();
        if code != Some(want) {
            return Err(VelocityError::AssertionFailed {
                expected: format!("exec `{}` to exit {want}", argv[0]),
                actual: format!(
                    "exit {}: {}",
                    code.map_or("by a signal".to_string(), |c| c.to_string()),
                    tail(if stderr.trim().is_empty() {
                        &stdout
                    } else {
                        &stderr
                    })
                ),
                selector: format!("exec {}", argv[0]),
                screenshot: None,
            });
        }
        if let Some(needle) = &e.expect.contains {
            if !stdout.contains(needle.as_str()) && !stderr.contains(needle.as_str()) {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("exec `{}` output containing {needle:?}", argv[0]),
                    actual: tail(&stdout),
                    selector: format!("exec {}", argv[0]),
                    screenshot: None,
                });
            }
        }
        if let Some(name) = &e.capture {
            if velocity_common::redact::is_secret_name(name) {
                velocity_common::redact::register_secret(&stdout);
            }
            self.captured.insert(name.clone(), stdout);
        }
        Ok(())
    }

    pub(super) async fn run_http(&mut self, req: &velocity_common::HttpRequest) -> Result<()> {
        if self.http.is_none() {
            let config = self.config.http.clone().unwrap_or_default();
            self.http = Some(velocity_api::HttpSession::new(&config)?);
        }
        let session = self.http.as_ref().expect("made above");
        let timeout = session.request_timeout(self.step_timeout_ms);
        let passed = session.run(req, timeout).await?;
        tracing::info!("{}", passed.summary);
        self.captured.extend(passed.captured);
        Ok(())
    }
}

/// A finger stroke across `b` in `finger`'s direction, from 80% to 20% of
/// the element along that axis, through its middle.
/// A slider (Android SeekBar or Slider, iOS Slider, the web's range input
/// or `role=slider`).
pub(super) fn is_slider(el: &velocity_common::Element) -> bool {
    let t = el.element_type.to_ascii_lowercase();
    t.contains("seekbar") || t.contains("slider") || el.role.as_deref() == Some("slider")
}

/// A swipe along a slider goes to its far end: the 20%-80% stroke of
/// [`line_across`] could never set it to its maximum or minimum, so
/// "brightness to max" ended at 80% of the track (AndroidWorld rerun,
/// 2026-11-08). From a fifth in to two points past the far edge, where the
/// slider clamps.
pub(super) fn slider_stroke(
    b: &velocity_common::Rect,
    finger: Direction,
) -> ((i32, i32), (i32, i32)) {
    let (x, y, w, h) = (
        i64::from(b.x),
        i64::from(b.y),
        i64::from(b.width),
        i64::from(b.height),
    );
    let (cx, cy) = (to_i32(x + w / 2), to_i32(y + h / 2));
    match finger {
        Direction::Right => ((to_i32(x + w / 5), cy), (to_i32(x + w + 2), cy)),
        Direction::Left => ((to_i32(x + w * 4 / 5), cy), (to_i32(x - 2), cy)),
        Direction::Up => ((cx, to_i32(y + h * 4 / 5)), (cx, to_i32(y - 2))),
        Direction::Down => ((cx, to_i32(y + h / 5)), (cx, to_i32(y + h + 2))),
    }
}

pub(super) fn line_across(
    b: &velocity_common::Rect,
    finger: Direction,
) -> ((i32, i32), (i32, i32)) {
    // In i64: hostile bounds overflowed i32 (VEL-231, review m6).
    let (x, y, w, h) = (
        i64::from(b.x),
        i64::from(b.y),
        i64::from(b.width),
        i64::from(b.height),
    );
    let (cx, cy) = (to_i32(x + w / 2), to_i32(y + h / 2));
    let (near_x, far_x) = (to_i32(x + w / 5), to_i32(x + w * 4 / 5));
    let (near_y, far_y) = (to_i32(y + h / 5), to_i32(y + h * 4 / 5));
    match finger {
        Direction::Left => ((far_x, cy), (near_x, cy)),
        Direction::Right => ((near_x, cy), (far_x, cy)),
        Direction::Up => ((cx, far_y), (cx, near_y)),
        Direction::Down => ((cx, near_y), (cx, far_y)),
    }
}

/// The stroke of a `swipe` with only a direction, on a `w`×`h` screen:
/// centred, half the width across, 40% of the height up or down (30%–70%:
/// from 25%, a landscape phone's swipe started on the toolbar above the
/// list and nothing scrolled). Android's and headless's line.
pub(super) fn default_swipe_line(w: i32, h: i32, finger: Direction) -> ((i32, i32), (i32, i32)) {
    let (cx, cy) = (w / 2, h / 2);
    let (dx, dy) = (w / 4, h / 5);
    match finger {
        Direction::Up => ((cx, cy + dy), (cx, cy - dy)),
        Direction::Down => ((cx, cy - dy), (cx, cy + dy)),
        Direction::Left => ((cx + dx, cy), (cx - dx, cy)),
        Direction::Right => ((cx - dx, cy), (cx + dx, cy)),
    }
}

/// The finger's direction for a scroll that moves the content `scroll`:
/// the opposite one (to see what is below, drag the content up).
pub(super) fn finger_for_scroll(scroll: Direction) -> Direction {
    match scroll {
        Direction::Down => Direction::Up,
        Direction::Up => Direction::Down,
        Direction::Left => Direction::Right,
        Direction::Right => Direction::Left,
    }
}

/// `v` as a screen coordinate, clamped into i32.
pub(super) fn to_i32(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// What `assertSystemState` expected and didn't see (empty: all good). A
/// title matches a playing session's title that contains it.
pub(super) fn system_state_mismatch(
    state: &velocity_common::SystemState,
    media_playing: Option<bool>,
    media_title: Option<&str>,
    in_call: Option<bool>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(want) = media_playing {
        if state.media_playing != want {
            out.push(format!("media playing={want}"));
        }
    }
    if let Some(t) = media_title {
        if !state.media_titles.iter().any(|x| x.contains(t)) {
            out.push(format!("media playing with title {t:?}"));
        }
    }
    if let Some(want) = in_call {
        if state.in_call != want {
            out.push(format!("in call={want}"));
        }
    }
    out
}

/// `sync.timeoutMs`, `sync.reactionMs`, `sync.quietMs`, `sync.settleCapMs`.
pub(super) fn set_sync_field(
    sync: &mut velocity_common::SyncConfig,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let field = key.strip_prefix("sync.").ok_or_else(|| {
        format!("setConfig: `{key}` isn't a setting or a sync timing (`velocity settings` lists settings)")
    })?;
    // The key first: `sync.mode: none` said "`none` isn't milliseconds"
    // when the problem is that `mode` can't change mid-test (VEL-218).
    let ms = |field: &str| -> std::result::Result<u64, String> {
        // The key first: `sync.mode: none` said "`none` isn't milliseconds".
        if !matches!(
            field,
            "timeoutMs"
                | "timeout_ms"
                | "reactionMs"
                | "reaction_ms"
                | "quietMs"
                | "quiet_ms"
                | "settleCapMs"
                | "settle_cap_ms"
        ) {
            return Err(format!(
                "setConfig: sync.{field} can't change mid-test (timeoutMs, reactionMs, quietMs, settleCapMs can)"
            ));
        }
        value
            .trim()
            .parse()
            .map_err(|_| format!("setConfig.{key}: `{value}` isn't milliseconds"))
    };
    let v = ms(field)?;
    match field {
        "timeoutMs" | "timeout_ms" => sync.timeout_ms = v,
        "reactionMs" | "reaction_ms" => sync.reaction_ms = Some(v),
        "quietMs" | "quiet_ms" => sync.quiet_ms = Some(v),
        _ => sync.settle_cap_ms = Some(v),
    }
    Ok(())
}
