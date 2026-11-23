//! Natural-language steps: planning, resolution, the verify checks.

use super::*;

impl<'a> TestExecutor<'a> {
    /// Run one natural-language step: resolve it (cache first, then the
    /// planner), execute it, and enforce its `waitUntil` condition.
    ///
    /// A cached resolution that fails is re-planned against the current screen
    /// and retried once in the same run, so a changed UI heals without failing
    /// the test (the re-plan is reported as a warning).
    pub(super) async fn execute_natural(
        &mut self,
        (intent, intent_template): (&str, Option<&str>),
        kind: NaturalKind,
        (data, data_name): (Option<&str>, Option<&str>),
        wait_until: Option<&str>,
        device_id: &str,
        app_id: &str,
    ) -> (Result<()>, Option<ResourceDelta>) {
        let Some(resolver) = self.planner.clone() else {
            return (
                Err(AtalantaError::Config(
                    "natural-language step encountered but no NL planner is configured \
                     (set ANTHROPIC_API_KEY or OPENAI_API_KEY to enable)"
                        .to_string(),
                )),
                None,
            );
        };
        let platform = self.config.platform.unwrap_or(Platform::Ios);
        let scope = self.natural_scope(kind, intent_template.unwrap_or(intent), data.is_some());
        // A retry attempt re-plans: replaying the action that just failed
        // would only fail the same way (N-2).
        let fresh = self.retry_attempt > 0;
        // D-30: `nl.dataPolicy: describe` keeps data values from the model.
        let describe_data = self.config.nl.data_policy == atalanta_common::NlDataPolicy::Describe;
        let step = NaturalStep {
            intent,
            kind,
            data,
            data_name,
            wait_until,
            platform,
            scope: &scope,
            fresh,
            describe_data,
            intent_template,
        };
        let (result, delta) = self
            .resolve_and_run(&resolver, &step, device_id, app_id, NOT_FOUND_ATTEMPTS)
            .await;
        let main = match result {
            Ok(r) => r,
            Err(e) => return (Err(e), delta),
        };

        // waitUntil is a check, not a hint: after the action, the named state
        // must appear (within the usual element timeout).
        if let Some(condition) = wait_until {
            let until_scope = format!("{scope}/until");
            let until = NaturalStep {
                intent: condition,
                kind: NaturalKind::Assert,
                data: None,
                data_name: None,
                wait_until: None,
                intent_template: None,
                platform,
                scope: &until_scope,
                fresh,
                describe_data,
            };
            match self
                .resolve_and_run(&resolver, &until, device_id, app_id, WAIT_UNTIL_ATTEMPTS)
                .await
            {
                (Ok(check), _) => {
                    let staged = resolver.prepare(&until, check.action, check.twins, None);
                    self.stage(staged);
                }
                (Err(e), _) => {
                    // The step's plan didn't reach its goal: don't keep it (N-1).
                    resolver.invalidate(&step).await;
                    // An unmet waitUntil is the test failing its check, not
                    // a configuration problem (VEL-222); a lost device or an
                    // unavailable model API keeps its own variant.
                    let e = match e {
                        AtalantaError::ServiceUnavailable { .. } => e,
                        _ if is_infrastructure(&e) || is_crash(&e) => e,
                        e => AtalantaError::AssertionFailed {
                            expected: format!("waitUntil {condition:?}"),
                            actual: format!("not reached: {e}"),
                            selector: String::new(),
                            screenshot: None,
                        },
                    };
                    return (Err(e), delta);
                }
            }
        }
        // Staged only now that the action and its check both passed (N-1);
        // written once the run confirms it (see `promote_pending`).
        let staged = resolver.prepare(&step, main.action, main.twins, main.effect);
        self.stage(staged);
        (Ok(()), delta)
    }

    /// For an indexed selector, how many elements its base matches on the
    /// current screen (the "twins" an index picks among).
    pub(super) async fn twin_count(&mut self, device_id: &str, action: &Action) -> Option<usize> {
        let Some(Selector::Index { selector, .. }) = action_selector(action) else {
            return None;
        };
        let tree = self.driver.get_hierarchy_cached(device_id).await.ok()?;
        self.audit_screen(device_id, &tree);
        let (w, h) = self.driver.screen_size(device_id).await.ok()?;
        let screen = atalanta_common::Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        Some(atalanta_common::selector_match::match_count(
            &tree,
            selector,
            Some(&screen),
        ))
    }

    /// Run `action` for a natural step and observe it: the twin count before,
    /// and whether the screen reacted after (for act steps).
    pub(super) async fn run_observed(
        &mut self,
        action: &Action,
        kind: NaturalKind,
        device_id: &str,
        app_id: &str,
    ) -> (Result<Resolved>, Option<ResourceDelta>) {
        // D-17: a planned or cached `goto` stays on http(s) and on the
        // current page's origin unless the suite allows otherwise.
        if let Action::Goto { url } = action {
            let current = self.driver.current_url(device_id).await;
            if let Err(why) = atalanta_common::natural::check_goto(
                url,
                current.as_deref(),
                self.config.nl.allow_cross_origin,
            ) {
                return (Err(AtalantaError::Config(why)), None);
            }
        }
        let twins = self.twin_count(device_id, action).await;
        let (result, delta) = self.run_action(action, device_id, app_id).await;
        let effect = (kind == NaturalKind::Act && is_mutating(action))
            .then(|| self.sync_manager.last_settle().reacted);
        (
            result.map(|()| Resolved {
                action: action.clone(),
                twins,
                effect,
            }),
            delta,
        )
    }

    pub(super) async fn resolve_and_run(
        &mut self,
        resolver: &Arc<dyn NaturalStepResolver>,
        step: &NaturalStep<'_>,
        device_id: &str,
        app_id: &str,
        attempts: u32,
    ) -> (Result<Resolved>, Option<ResourceDelta>) {
        if let Some(hit) = resolver.cached_hit(step).await {
            // Fast path: no model call, and no screen capture unless the
            // selector is indexed.
            let cached = hit.action.clone();
            let twins_now = self.twin_count(device_id, &cached).await;
            let (result, delta) = match (hit.twins, twins_now) {
                // Identity check (N-4): the rows an index picks among changed,
                // so it may now hit a different "twin".
                (Some(was), Some(now)) if was != now => (
                    Err(AtalantaError::Config(format!(
                        "the {was} matching elements it chose among are now {now}"
                    ))),
                    None,
                ),
                _ => {
                    self.run_observed(&cached, step.kind, device_id, app_id)
                        .await
                }
            };
            // Effect check (N-5): it changed the screen when planned and
            // changes nothing now. That fails the step, and no second,
            // re-planned action runs in it (D-17, VEL-349): the first one
            // already acted. The next attempt plans afresh.
            if let Ok(r) = &result {
                if hit.effect == Some(true) && r.effect == Some(false) {
                    resolver.invalidate(step).await;
                    return (
                        Err(AtalantaError::AssertionFailed {
                            expected: format!(
                                "{} to change the screen, as it did when it was planned",
                                describe(&cached)
                            ),
                            actual: "no visible effect; not re-planned in this step \
                                     (the cache entry was dropped, so the next attempt plans it afresh)"
                                .into(),
                            selector: action_selector(&cached)
                                .map(|s| s.to_string())
                                .unwrap_or_default(),
                            screenshot: None,
                        }),
                        delta,
                    );
                }
            }
            let first_err = match result {
                Ok(r) => {
                    tracing::info!(intent = step.intent, "atalanta-nl: cache hit");
                    return (Ok(r), delta);
                }
                Err(e) => e,
            };
            // A failed natural-language assertion is never re-planned onto
            // the current screen (D-17, VEL-213): a re-plan would look for
            // whatever is there and could pass on another element. The
            // entry is dropped, so the next attempt plans it afresh.
            if step.kind == NaturalKind::Assert {
                tracing::info!(intent = step.intent, reason = %first_err, "atalanta-nl: cached assertion failed; not re-planning");
                resolver.invalidate(step).await;
                return (
                    Err(annotate(
                        &cached,
                        first_err,
                        Some("a failed assertion is not re-planned onto the current screen"),
                    )),
                    delta,
                );
            }
            tracing::info!(intent = step.intent, reason = %first_err, "atalanta-nl: cached step failed; re-planning");
            resolver.invalidate(step).await;
            // N-8: re-plan with the old resolution as a hint.
            let hint = format!(
                "This step previously resolved to {} and that no longer works: {first_err}. \
                 Prefer the same element if it is still on screen.",
                describe(&cached)
            );
            let replanned = match self
                .plan_natural(resolver, step, device_id, attempts, Some(&hint))
                .await
            {
                Ok(a) => a,
                Err(e) => return (Err(replan_failed(&cached, first_err, e)), None),
            };
            if format!("{replanned:?}") == format!("{cached:?}") && hit.twins == twins_now {
                // Same answer on the current screen: the UI didn't move, the
                // step genuinely failed. Don't run it twice.
                return (Err(annotate(&cached, first_err, None)), None);
            }
            let (result, delta) = self
                .run_observed(&replanned, step.kind, device_id, app_id)
                .await;
            return match result {
                Ok(r) => {
                    self.warnings.push(format!(
                        "natural step {:?}: cached {} stopped working ({first_err}); \
                         re-planned to {}",
                        step.intent,
                        describe(&cached),
                        describe(&replanned)
                    ));
                    (Ok(r), delta)
                }
                Err(e) => (Err(annotate(&replanned, e, None)), None),
            };
        }

        tracing::info!(
            intent = step.intent,
            reason = if step.fresh {
                "retry attempt"
            } else {
                "not cached"
            },
            "atalanta-nl: planning"
        );
        let action = match self
            .plan_natural(resolver, step, device_id, attempts, None)
            .await
        {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("natural-language resolution failed: {e}");
                return (Err(planning_error(e, msg)), None);
            }
        };
        let (result, delta) = self
            .run_observed(&action, step.kind, device_id, app_id)
            .await;
        match result {
            Ok(r) => (Ok(r), delta),
            Err(e) => (Err(annotate(&action, e, None)), None),
        }
    }

    pub(super) async fn plan_natural(
        &mut self,
        resolver: &Arc<dyn NaturalStepResolver>,
        step: &NaturalStep<'_>,
        device_id: &str,
        attempts: u32,
        hint: Option<&str>,
    ) -> Result<Action> {
        let mut attempt = 1;
        loop {
            self.settle(device_id).await;
            let hierarchy = self.driver.get_hierarchy_cached(device_id).await?;
            self.trace.record_tree(&hierarchy);
            self.audit_screen(device_id, &hierarchy);
            // Ground in the rect the driver matches in (VEL-018).
            let screen =
                self.driver
                    .screen_size(device_id)
                    .await
                    .ok()
                    .map(|(w, h)| atalanta_common::Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    });
            match resolver
                .plan_on_screen(step, &hierarchy, screen.as_ref(), hint)
                .await
            {
                Err(AtalantaError::ElementNotFound { .. }) if attempt < attempts => {
                    attempt += 1;
                    tokio::time::sleep(NOT_FOUND_RETRY_DELAY).await;
                    self.driver.invalidate_hierarchy_cache().await;
                }
                other => return other,
            }
        }
    }

    /// `Test name#n` — the cache scope for the n-th occurrence of a natural
    /// step in the running test.
    /// The cache scope of a natural step: the test and how many times this
    /// step (by its written shape, quoted values blanked, data presence;
    /// never its values) came before in the test.
    pub(super) fn natural_scope(
        &mut self,
        kind: NaturalKind,
        written: &str,
        has_data: bool,
    ) -> String {
        let key = format!(
            "{kind:?}\u{1f}{}\u{1f}{has_data}",
            atalanta_common::natural::step_shape(written.trim()),
        );
        let n = self.natural_seen.entry(key).or_insert(0);
        let scope = format!("{}#{n}", self.current_test);
        *n += 1;
        scope
    }

    /// Hold a verified-so-far plan of the running step for the cache.
    pub(super) fn stage(&mut self, plan: Option<PendingPlan>) {
        if let Some(plan) = plan {
            let step = self.current_step;
            self.nl_pending.push((step, plan));
        }
    }

    /// Commit the pending plans staged before step `before` (all of them
    /// for `None`): a later step passing confirms the screen they led to.
    pub(super) async fn promote_pending(&mut self, before: Option<usize>) {
        let Some(resolver) = self.planner.clone() else {
            self.nl_pending.clear();
            return;
        };
        let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.nl_pending)
            .into_iter()
            .partition(|(i, _)| before.is_none_or(|b| *i < b));
        self.nl_pending = keep;
        if !ready.is_empty() {
            resolver
                .commit_pending(ready.into_iter().map(|(_, p)| p).collect())
                .await;
        }
    }

    /// The claim to check after `step` under `verify: checkpoints` or
    /// `strict`: its `checkpoint:` claim, or for an `assert:` step (and a
    /// natural step with `checkpoint: true`) its `waitUntil` or intent.
    pub(super) fn checkpoint_claim(&self, step: &Step) -> Option<String> {
        if !self.verify_level.checks_checkpoints() {
            return None;
        }
        let own = match &step.action {
            Action::Natural {
                intent, wait_until, ..
            } => Some(wait_until.clone().unwrap_or_else(|| intent.clone())),
            _ => None,
        };
        match step.modifiers.checkpoint.as_deref() {
            Some("") => own,
            Some(claim) => Some(claim.to_string()),
            None => match &step.action {
                Action::Natural {
                    kind: NaturalKind::Assert,
                    ..
                } => own,
                _ => None,
            },
        }
    }

    /// Ask the AI checker whether `claim` holds on the current screen.
    /// Code decides: see `atalanta_nl::checker`.
    pub(super) async fn verify_screen(
        &mut self,
        device_id: &str,
        at: &str,
        claim: &str,
    ) -> atalanta_common::VerifyCheck {
        let unavailable = |reason: String| atalanta_common::VerifyCheck {
            at: at.to_string(),
            claim: claim.to_string(),
            outcome: atalanta_common::VerifyOutcome::Error,
            reason,
            evidence: vec![],
        };
        let Some(resolver) = self.planner.clone() else {
            return unavailable(
                "no AI checker is configured (set ANTHROPIC_API_KEY or OPENAI_API_KEY)".into(),
            );
        };
        self.settle(device_id).await;
        let hierarchy = match self.driver.get_hierarchy_cached(device_id).await {
            Ok(h) => h,
            Err(e) => return unavailable(format!("could not read the screen: {e}")),
        };
        self.audit_screen(device_id, &hierarchy);
        let screen =
            self.driver
                .screen_size(device_id)
                .await
                .ok()
                .map(|(w, h)| atalanta_common::Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                });
        // A failed capture leaves the check to the tree alone: say so
        // rather than judging blind in silence (review 03 M3).
        let shot = match self.driver.screenshot(device_id).await {
            Ok(png) => Some(png),
            Err(e) => {
                tracing::warn!(at, error = %e, "screenshot failed; verifying from the tree only");
                None
            }
        };
        let platform = self.config.platform.unwrap_or(Platform::Ios);
        let check = resolver
            .check(
                at,
                claim,
                &hierarchy,
                screen.as_ref(),
                platform,
                shot.as_deref(),
            )
            .await;
        tracing::info!(at, outcome = %check.outcome, "atalanta-nl: verify");
        check
    }
}

/// `tap text="Login"` — an action with its selector, for messages.
/// A natural step's resolution that worked, with what was observed.
pub(super) struct Resolved {
    pub(super) action: Action,
    pub(super) twins: Option<usize>,
    pub(super) effect: Option<bool>,
}
