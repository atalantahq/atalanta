use std::collections::HashSet;
use std::path::PathBuf;

use tokio::sync::Mutex;
use velocity_common::{Action, Element, Platform, Rect, Result, VelocityError, VerifyCheck};

use crate::cache::{Hit, NlCache};
use crate::checker::ScreenChecker;
use crate::grounding::{ground, GroundingError};
use crate::planner::{
    is_model_failure, NaturalPlanner, PlanRequest, ScreenIndex, UNPARSEABLE_REPLY,
};

pub use velocity_common::natural::{step_shape, NaturalStep, PendingPlan};

/// At most this many quoted values in one step are tried as data slots
/// (2^n candidate keys per lookup).
const MAX_SLOTS: usize = 3;

/// The cache keys of a [`NaturalStep`].
pub trait StepKeys {
    /// The step's cache key with no slots. No test data is in it: not the
    /// `data` value (only whether there is one), not a `${VAR}`'s value.
    fn cache_key(&self) -> String;
    /// The key with the quoted values at `slots` as slots.
    fn key_with(&self, slots: &[usize]) -> String;
    /// Every key the step may be cached under: no slots first, then each
    /// subset of its quoted values as slots.
    fn candidate_keys(&self) -> Vec<(Vec<usize>, String)>;
}

impl StepKeys for NaturalStep<'_> {
    fn key_with(&self, slots: &[usize]) -> String {
        NlCache::key(
            self.platform,
            self.kind,
            &self.template_with(slots),
            self.data.is_some(),
            self.scope,
        )
    }

    fn cache_key(&self) -> String {
        self.key_with(&[])
    }

    fn candidate_keys(&self) -> Vec<(Vec<usize>, String)> {
        let n = self.literals().map_or(0, |l| l.len());
        let n = if n > MAX_SLOTS { 0 } else { n };
        (0u32..(1 << n))
            .map(|mask| {
                let slots: Vec<usize> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
                let key = self.key_with(&slots);
                (slots, key)
            })
            .collect()
    }
}

/// Stands for the step's data in a cached action, filled in on replay.
const DATA_PLACEHOLDER: &str = "\u{1}velocity:data\u{1}";

/// Stands for the step's `i`th quoted value in a cached action.
fn literal_placeholder(i: usize) -> String {
    format!("\u{1}velocity:lit{i}\u{1}")
}

/// How the cache may be used (`VELOCITY_NL_CACHE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Replay, plan on a miss, store what worked.
    Normal,
    /// CI: never call the model. A miss or a broken entry fails the step.
    ReadOnly,
    /// Like read-only, and an entry that would change (a heal) fails too:
    /// the cache in the repo must already be right.
    Frozen,
    /// Normal, then drop the entries no step used this run.
    Prune,
}

impl CacheMode {
    pub fn from_env() -> Self {
        match velocity_common::env::var("VELOCITY_NL_CACHE").as_deref() {
            Ok("read-only") | Ok("readonly") => Self::ReadOnly,
            Ok("frozen") => Self::Frozen,
            Ok("prune") => Self::Prune,
            _ => Self::Normal,
        }
    }

    fn may_plan(self) -> bool {
        matches!(self, Self::Normal | Self::Prune)
    }
}

/// Resolves natural-language steps to concrete [`Action`]s, backed by a planner
/// and a disk cache.
///
/// - [`cached_hit`](Self::cached_hit) is the fast path: no model call and no
///   screen capture. The executor checks it before dumping the hierarchy.
/// - [`plan`](Self::plan) asks the planner against the live screen and grounds
///   the answer in code (re-asking once with feedback if it's unusable). It
///   does **not** cache: the caller [`prepare`](Self::prepare)s a plan once
///   the action and its `waitUntil` check passed and
///   [`commit_pending`](Self::commit_pending)s it once the run confirmed it.
/// - [`invalidate`](Self::invalidate) drops a step whose cached action failed,
///   so it re-plans against the changed UI.
/// - [`check`](Self::check) asks the AI checker whether a claim holds on the
///   screen (`verify:`).
pub struct NaturalResolver {
    planner: Box<dyn NaturalPlanner>,
    checker: Option<Box<dyn ScreenChecker>>,
    cache: Mutex<NlCache>,
    cache_path: PathBuf,
    /// Where each fresh plan's inputs are saved for `velocity nl-eval`.
    traces_dir: Option<PathBuf>,
    mode: CacheMode,
    /// Keys read or written this run (for `prune`).
    used: Mutex<HashSet<String>>,
}

/// The executor's view of the resolver: each method is the inherent one.
#[async_trait::async_trait]
impl velocity_common::natural::NaturalStepResolver for NaturalResolver {
    async fn cached_hit(&self, step: &NaturalStep<'_>) -> Option<Hit> {
        NaturalResolver::cached_hit(self, step).await
    }

    async fn plan_on_screen(
        &self,
        step: &NaturalStep<'_>,
        hierarchy: &Element,
        screen: Option<&Rect>,
        hint: Option<&str>,
    ) -> Result<Action> {
        NaturalResolver::plan_on_screen(self, step, hierarchy, screen, hint).await
    }

    fn prepare(
        &self,
        step: &NaturalStep<'_>,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
    ) -> Option<PendingPlan> {
        NaturalResolver::prepare(self, step, action, twins, effect)
    }

    async fn commit_pending(&self, plans: Vec<PendingPlan>) {
        NaturalResolver::commit_pending(self, plans).await
    }

    async fn invalidate(&self, step: &NaturalStep<'_>) {
        NaturalResolver::invalidate(self, step).await
    }

    async fn check(
        &self,
        at: &str,
        claim: &str,
        hierarchy: &Element,
        screen: Option<&Rect>,
        platform: Platform,
        screenshot: Option<&[u8]>,
    ) -> VerifyCheck {
        NaturalResolver::check(self, at, claim, hierarchy, screen, platform, screenshot).await
    }
}

/// How many times to re-ask the planner after a grounding rejection.
const MAX_REASKS: usize = 1;

impl NaturalResolver {
    /// Build a resolver over `planner`, persisting resolutions to `cache_path`
    /// (loaded now if it already exists). The cache mode comes from
    /// `VELOCITY_NL_CACHE`.
    pub fn new(planner: Box<dyn NaturalPlanner>, cache_path: impl Into<PathBuf>) -> Self {
        let cache_path = cache_path.into();
        let cache = NlCache::load(&cache_path);
        Self {
            planner,
            checker: None,
            cache: Mutex::new(cache),
            cache_path,
            traces_dir: None,
            mode: CacheMode::from_env(),
            used: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_mode(mut self, mode: CacheMode) -> Self {
        self.mode = mode;
        self
    }

    /// The AI checker `verify:` asks.
    pub fn with_checker(mut self, checker: Box<dyn ScreenChecker>) -> Self {
        self.checker = Some(checker);
        self
    }

    /// Whether a checker is configured (`verify:` needs one).
    pub fn has_checker(&self) -> bool {
        self.checker.is_some()
    }

    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    /// Where resolutions are persisted.
    pub fn cache_path(&self) -> &std::path::Path {
        &self.cache_path
    }

    /// Save a trace (step, screen, chosen action) for every fresh plan into
    /// `dir`, so `velocity nl-eval` can re-plan them and measure consistency.
    pub fn with_traces(mut self, dir: impl Into<PathBuf>) -> Self {
        self.traces_dir = Some(dir.into());
        self
    }

    /// The cached action for `step`, if any.
    pub async fn cached(&self, step: &NaturalStep<'_>) -> Option<Action> {
        self.cached_hit(step).await.map(|h| h.action)
    }

    /// The cached resolution for `step` with what was observed when it was
    /// planned; the step's data and quoted values are filled into the
    /// placeholders. An entry bound to another value is a miss.
    pub async fn cached_hit(&self, step: &NaturalStep<'_>) -> Option<Hit> {
        if step.fresh {
            return None;
        }
        let cache = self.cache.lock().await;
        let literals = step.literals().unwrap_or_default();
        for (_, key) in step.candidate_keys() {
            let Some(mut hit) = cache.hit(&key) else {
                continue;
            };
            if !velocity_common::natural::cacheable(step.kind, &hit.action) {
                tracing::warn!(
                    intent = step.intent,
                    "velocity-nl: the cached resolution is not an action this step can ground \
                     to (an edited nl-cache.json?); ignoring it and re-planning"
                );
                continue;
            }
            if let Some(bound) = &hit.bound {
                let now = step.data.map(NlCache::fingerprint);
                if now.as_ref() != Some(bound) {
                    tracing::debug!(
                        intent = step.intent,
                        "velocity-nl: cached step fits another value; re-planning"
                    );
                    continue;
                }
            }
            // The cache file is untrusted: a stored value must be a
            // placeholder or the step's own words, as for a fresh plan.
            if let Err(why) = crate::grounding::check_cached(
                &hit.action,
                step.intent,
                step.kind,
                step.data.is_some(),
                &|t| {
                    t == DATA_PLACEHOLDER
                        || (0..literals.len()).any(|i| t == literal_placeholder(i))
                },
            ) {
                tracing::warn!(
                    intent = step.intent,
                    "velocity-nl: the cached resolution fails the checks a fresh plan passes \
                     ({why}; an edited nl-cache.json?); ignoring it and re-planning"
                );
                continue;
            }
            let Some(action) = fill(&hit.action, step.data, &literals) else {
                continue;
            };
            hit.action = action;
            self.used.lock().await.insert(key);
            return Some(hit);
        }
        None
    }

    /// Plan `step` against `hierarchy` and ground it. Not cached: see
    /// [`prepare`](Self::prepare). `hint` describes a previous resolution
    /// that stopped working, so the model can keep a stable choice.
    pub async fn plan(&self, step: &NaturalStep<'_>, hierarchy: &Element) -> Result<Action> {
        self.plan_with_hint(step, hierarchy, None).await
    }

    pub async fn plan_with_hint(
        &self,
        step: &NaturalStep<'_>,
        hierarchy: &Element,
        hint: Option<&str>,
    ) -> Result<Action> {
        self.plan_on_screen(step, hierarchy, None, hint).await
    }

    /// [`plan_with_hint`](Self::plan_with_hint) against the rect the driver
    /// matches selectors in, so grounding counts twins the way the driver
    /// does (VEL-018). Callers that know the screen size should use this.
    pub async fn plan_on_screen(
        &self,
        step: &NaturalStep<'_>,
        hierarchy: &Element,
        screen: Option<&Rect>,
        hint: Option<&str>,
    ) -> Result<Action> {
        if !self.mode.may_plan() {
            return Err(VelocityError::Config(format!(
                "natural step {:?} has no usable cache entry and planning is off \
                 ({}); run once with a model key set and commit .velocity/nl-cache.json",
                step.intent,
                if self.mode == CacheMode::Frozen {
                    "VELOCITY_NL_CACHE=frozen"
                } else {
                    "VELOCITY_NL_CACHE=read-only, or no ANTHROPIC_API_KEY / OPENAI_API_KEY"
                }
            )));
        }
        let action =
            plan_step_on_screen(self.planner.as_ref(), step, hierarchy, screen, hint).await?;
        if let Some(dir) = &self.traces_dir {
            if let Err(e) = crate::eval::write_trace(dir, step, hierarchy, screen, &action) {
                tracing::debug!("velocity-nl: trace not saved: {e}");
            }
        }
        Ok(action)
    }

    /// What caching `action` for `step` would write, held until the run
    /// verified it (see [`PendingPlan`]). `None` when nothing may be
    /// written: read-only and frozen modes, or a resolution that can't be
    /// stored without a secret. `twins` and `effect` are what the run
    /// observed (see [`Hit`]).
    pub fn prepare(
        &self,
        step: &NaturalStep<'_>,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
    ) -> Option<PendingPlan> {
        if !self.mode.may_plan() {
            return None;
        }
        let plan = generalize(step, action, twins, effect);
        if plan.is_none() {
            // nl-cache.json is meant to be committed (VEL-096).
            tracing::debug!("velocity-nl: not caching a step that carries a secret");
        }
        plan
    }

    /// Write verified plans to the cache (one save for all of them).
    pub async fn commit_pending(&self, plans: impl IntoIterator<Item = PendingPlan>) {
        if !self.mode.may_plan() {
            return;
        }
        let mut cache = self.cache.lock().await;
        let mut any = false;
        for p in plans {
            self.used.lock().await.insert(p.key.clone());
            cache.put_bound(
                p.key,
                &p.template,
                p.platform,
                p.action,
                p.twins,
                p.effect,
                p.bound,
            );
            any = true;
        }
        if any {
            if let Err(e) = cache.save(&self.cache_path) {
                tracing::warn!("velocity-nl: failed to persist cache: {e}");
            }
        }
    }

    /// Cache `action` for `step` at once: [`prepare`](Self::prepare) then
    /// [`commit_pending`](Self::commit_pending). For a caller that has
    /// already verified it.
    pub async fn commit(
        &self,
        step: &NaturalStep<'_>,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
    ) {
        if let Some(plan) = self.prepare(step, action, twins, effect) {
            self.commit_pending([plan]).await;
        }
    }

    /// The cached action if present, otherwise a fresh plan. Never caches
    /// the fresh plan: the caller commits it once it worked.
    pub async fn resolve(&self, step: &NaturalStep<'_>, hierarchy: &Element) -> Result<Action> {
        if let Some(action) = self.cached(step).await {
            return Ok(action);
        }
        self.plan(step, hierarchy).await
    }

    /// Forget a step's cached action so it re-plans next time (self-healing).
    /// In read-only and frozen modes the file is left as it is.
    pub async fn invalidate(&self, step: &NaturalStep<'_>) {
        let mut cache = self.cache.lock().await;
        let mut removed = false;
        for (_, key) in step.candidate_keys() {
            removed |= cache.remove(&key);
        }
        if removed {
            tracing::info!(
                intent = step.intent,
                "velocity-nl: invalidated cached step (self-heal)"
            );
            if self.mode.may_plan() {
                if let Err(e) = cache.save(&self.cache_path) {
                    tracing::warn!("velocity-nl: failed to persist cache: {e}");
                }
            }
        }
    }

    /// Ask the AI checker whether `claim` holds on `hierarchy` (see
    /// [`crate::checker::verify_claim`]). `at` names the point in the test.
    /// Without a checker the outcome is `error`.
    pub async fn check(
        &self,
        at: &str,
        claim: &str,
        hierarchy: &Element,
        screen: Option<&Rect>,
        platform: Platform,
        screenshot: Option<&[u8]>,
    ) -> VerifyCheck {
        match &self.checker {
            Some(c) => {
                crate::checker::verify_claim(
                    c.as_ref(),
                    at,
                    claim,
                    hierarchy,
                    screen,
                    platform,
                    screenshot,
                )
                .await
            }
            None => VerifyCheck {
                at: at.to_string(),
                claim: claim.to_string(),
                outcome: velocity_common::VerifyOutcome::Error,
                reason: "no AI checker is configured (set a model key)".into(),
                evidence: vec![],
            },
        }
    }

    /// End of run. In `prune` mode, drop the entries no step used and
    /// return how many went.
    pub async fn finish(&self) -> usize {
        if self.mode != CacheMode::Prune {
            return 0;
        }
        let used = self.used.lock().await.clone();
        let mut cache = self.cache.lock().await;
        let n = cache.retain_used(&used);
        if n > 0 {
            if let Err(e) = cache.save(&self.cache_path) {
                tracing::warn!("velocity-nl: failed to persist cache: {e}");
            }
        }
        n
    }
}

/// The stored form of a working resolution (see [`NaturalResolver::prepare`]),
/// or `None` when it can't be stored without writing a secret.
///
/// No test data is stored. Every string in the action equal to the step's
/// data becomes a placeholder: typed text, expected text, a URL, and a
/// selector built from the value (`text="Large"` for data `Large`), filled
/// in for the next value on replay. A typed or expected value equal to one
/// of the step's quoted values becomes a slot, and the key is the template
/// with that slot (`type "␁0" into Email`). A selector that holds only part
/// of the data fits only that value: the entry is bound to a fingerprint of
/// it, and a secret one isn't cached (VEL-096, VEL-S17). Regular-expression,
/// CSS and XPath selectors are never rewritten.
fn generalize(
    step: &NaturalStep<'_>,
    action: Action,
    twins: Option<usize>,
    effect: Option<bool>,
) -> Option<PendingPlan> {
    let secret = is_secret_data(step);
    let mut value = serde_json::to_value(&action).ok()?;
    let mut bound = None;
    if let Some(data) = step.data.filter(|d| !d.is_empty()) {
        replace_exact(&mut value, data, DATA_PLACEHOLDER);
        if json_contains(&value, data) {
            // The value is baked into the selector: it only fits this value
            // (passmark never caches those for another one).
            if secret {
                return None;
            }
            bound = Some(NlCache::fingerprint(data));
        }
    }
    let mut action: Action = serde_json::from_value(value).ok()?;
    let mut slots = Vec::new();
    if let Some(literals) = step.literals().filter(|l| l.len() <= MAX_SLOTS) {
        let target = target_only(&action);
        if let Some(text) = data_field(&mut action) {
            if let Some(i) = literals.iter().position(|l| l == text) {
                // A quoted value the selector also names is the target's
                // name, not data: it stays in the key.
                if !target
                    .as_ref()
                    .is_some_and(|t| action_contains(t, literals[i]))
                {
                    *text = literal_placeholder(i);
                    slots.push(i);
                }
            }
        }
    }
    let template = step.template_with(&slots);
    // Last guard: neither the stored action nor the template may carry a
    // registered secret or the step's secret value.
    let stored = serde_json::to_string(&action).ok()?;
    let leaks = |s: &str| {
        velocity_common::redact::redact(s) != s
            || (secret && step.data.is_some_and(|d| !d.is_empty() && s.contains(d)))
    };
    if leaks(&stored) || leaks(&template) {
        return None;
    }
    Some(PendingPlan {
        key: step.key_with(&slots),
        template,
        platform: step.platform,
        action,
        twins,
        effect,
        bound,
    })
}

/// `action` with its value field emptied: only what picks the target.
fn target_only(action: &Action) -> Option<Action> {
    let mut a = action.clone();
    *data_field(&mut a)? = String::new();
    Some(a)
}

/// The cached `action` with the step's data and quoted values filled in,
/// or `None` when it has a slot the step doesn't.
fn fill(action: &Action, data: Option<&str>, literals: &[&str]) -> Option<Action> {
    let mut value = serde_json::to_value(action).ok()?;
    let mut ok = true;
    walk_strings(&mut value, &mut |s| {
        if !s.contains('\u{1}') {
            return;
        }
        if s.contains(DATA_PLACEHOLDER) {
            match data {
                Some(d) => *s = s.replace(DATA_PLACEHOLDER, d),
                None => ok = false,
            }
        }
        for (i, lit) in literals.iter().enumerate() {
            let p = literal_placeholder(i);
            if s.contains(&p) {
                *s = s.replace(&p, lit);
            }
        }
        if s.contains("\u{1}velocity:") {
            ok = false;
        }
    });
    if !ok {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Selector kinds whose string is a pattern, not a value: never rewritten.
const PATTERN_KEYS: [&str; 4] = ["textMatches", "idMatches", "css", "xPath"];

/// Call `f` on every string in `v` outside a pattern selector.
fn walk_strings(v: &mut serde_json::Value, f: &mut impl FnMut(&mut String)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|v| walk_strings(v, f)),
        serde_json::Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                if !PATTERN_KEYS.contains(&k.as_str()) {
                    walk_strings(v, f);
                }
            }
        }
        _ => {}
    }
}

/// Replace every string in `v` equal to `value` by `with`.
fn replace_exact(v: &mut serde_json::Value, value: &str, with: &str) {
    walk_strings(v, &mut |s| {
        if s == value {
            *s = with.to_string();
        }
    });
}

/// Whether any string or key inside `v` contains `needle`.
fn json_contains(v: &serde_json::Value, needle: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains(needle),
        serde_json::Value::Array(a) => a.iter().any(|v| json_contains(v, needle)),
        serde_json::Value::Object(m) => m
            .iter()
            .any(|(k, v)| k.contains(needle) || json_contains(v, needle)),
        _ => false,
    }
}

/// The field of `action` that a step's data fills, if it has one.
fn data_field(action: &mut Action) -> Option<&mut String> {
    match action {
        Action::InputText { text, .. } => Some(text),
        Action::AssertText { expected, .. } => Some(expected),
        Action::Goto { url } => Some(url),
        _ => None,
    }
}

/// Whether any string inside `action` contains `needle`.
fn action_contains(action: &Action, needle: &str) -> bool {
    serde_json::to_value(action).map_or(true, |v| json_contains(&v, needle))
}

/// Whether the step's data is a secret: a registered secret value, or
/// written as a `${VAR}` whose name looks like one.
fn is_secret_data(step: &NaturalStep<'_>) -> bool {
    let registered = step
        .data
        .is_some_and(|d| velocity_common::redact::redact(d) != d);
    let named = step.data_name.is_some_and(|n| {
        n.split("${").skip(1).any(|rest| {
            let name = rest.split([':', '}']).next().unwrap_or("");
            velocity_common::redact::is_secret_name(name)
        })
    });
    registered || named
}

/// Plan one step against `hierarchy` and ground the answer, re-asking once
/// with feedback when grounding rejects it. No caching — the resolver and the
/// planning eval both build on this.
pub async fn plan_step(
    planner: &dyn NaturalPlanner,
    step: &NaturalStep<'_>,
    hierarchy: &Element,
) -> Result<Action> {
    plan_step_with_hint(planner, step, hierarchy, None).await
}

/// [`plan_step`], telling the model what this step resolved to before
/// (N-8: keeps heals stable instead of a fresh, possibly different pick).
pub async fn plan_step_with_hint(
    planner: &dyn NaturalPlanner,
    step: &NaturalStep<'_>,
    hierarchy: &Element,
    hint: Option<&str>,
) -> Result<Action> {
    plan_step_on_screen(planner, step, hierarchy, None, hint).await
}

/// [`plan_step_with_hint`] grounded against `screen`, the rect the driver
/// matches selectors in (VEL-018).
pub async fn plan_step_on_screen(
    planner: &dyn NaturalPlanner,
    step: &NaturalStep<'_>,
    hierarchy: &Element,
    screen_rect: Option<&Rect>,
    hint: Option<&str>,
) -> Result<Action> {
    plan_step_outcome(planner, step, hierarchy, screen_rect, hint)
        .await
        .map_err(PlanFailure::into_error)
}

/// Why planning a step produced no action, kept typed so a caller that
/// scores the planner can't confuse a wrong answer with a dead service
/// (review MJ-1).
#[derive(Debug)]
pub enum PlanFailure {
    /// The planner said the target isn't on screen.
    NotFound(VelocityError),
    /// The model answered, but never usably: grounding rejected every
    /// answer (wrong op, unasked wait, over-long `ms`, disabled or unknown
    /// element, a value not from the step), or the reply was unparseable,
    /// refused or out of tokens. The planner's failure.
    Rejected(VelocityError),
    /// The call itself failed: transport, auth, outage.
    Service(VelocityError),
}

impl PlanFailure {
    /// The error the step fails with.
    pub fn into_error(self) -> VelocityError {
        match self {
            Self::NotFound(e) | Self::Rejected(e) | Self::Service(e) => e,
        }
    }
}

/// [`plan_step_on_screen`], with the failure kind kept.
pub async fn plan_step_outcome(
    planner: &dyn NaturalPlanner,
    step: &NaturalStep<'_>,
    hierarchy: &Element,
    screen_rect: Option<&Rect>,
    hint: Option<&str>,
) -> std::result::Result<Action, PlanFailure> {
    let query = format!("{} {}", step.intent, step.data.unwrap_or(""));
    let index = ScreenIndex::build(hierarchy, screen_rect, step.platform, &query);
    // The planner only needs an element number: secrets never go to the
    // model (VEL-097). Secret data is described, not sent; the screen and
    // intent are redacted. Grounding still uses the real value.
    let secret = step.data.filter(|_| is_secret_data(step));
    // D-30: with `nl.dataPolicy: describe`, any data is described too.
    let described = step.data.filter(|_| secret.is_none() && step.describe_data);
    let shown_data = match (secret, described) {
        (Some(d), _) => Some(format!("<secret value, {} chars>", d.chars().count())),
        (None, Some(d)) => Some(format!(
            "<value, {} chars> (shown as <value> on the screen)",
            d.chars().count()
        )),
        (None, None) => step.data.map(String::from),
    };
    let mask = |s: &str| -> String {
        let s = velocity_common::redact::redact(s).into_owned();
        let s = match secret {
            Some(d) if !d.is_empty() => s.replace(d, "<secret>"),
            _ => s,
        };
        // A described value of a character or two would mask every such
        // letter on the screen: those are only described.
        match described {
            Some(d) if d.chars().count() >= 3 => s.replace(d, "<value>"),
            _ => s,
        }
    };
    let screen = mask(&index.render());
    let intent = mask(step.intent);
    let wait_until = step.wait_until.map(mask);
    // The re-plan hint and grounding's feedback can quote the failed step,
    // e.g. an assertion's expected secret: mask them too (review M4).
    let mut feedback: Option<String> = hint.map(mask);

    for attempt in 0..=MAX_REASKS {
        let req = PlanRequest {
            intent: &intent,
            kind: step.kind,
            data: shown_data.as_deref(),
            wait_until: wait_until.as_deref(),
            platform: step.platform,
            screen: &screen,
            feedback: feedback.as_deref(),
        };
        let planned = match planner.plan(&req).await {
            Ok(p) => p,
            // A reply that doesn't parse is asked again, like an answer
            // grounding rejects; it failed the step at once (VEL-249).
            Err(e) if attempt < MAX_REASKS && e.to_string().contains(UNPARSEABLE_REPLY) => {
                tracing::debug!(intent = step.intent, %e, attempt, "velocity-nl: re-asking");
                feedback = Some(
                    "the reply was not one valid JSON object with op, element, text, \
                     direction and ms; answer with exactly that object"
                        .into(),
                );
                continue;
            }
            Err(e) if is_model_failure(&e) => return Err(PlanFailure::Rejected(e)),
            Err(e) => return Err(PlanFailure::Service(e)),
        };
        match ground(
            &planned,
            step.intent,
            step.kind,
            step.data,
            &index,
            hierarchy,
            screen_rect,
        ) {
            Ok(action) => {
                tracing::debug!(
                    intent = step.intent,
                    ?action,
                    attempt,
                    "velocity-nl: planned"
                );
                return Ok(action);
            }
            Err(GroundingError::NotFound) => {
                return Err(PlanFailure::NotFound(VelocityError::ElementNotFound {
                    selector: format!("natural step {:?}", step.intent),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                }));
            }
            Err(GroundingError::Rejected(why)) => {
                tracing::debug!(intent = step.intent, %why, attempt, "velocity-nl: re-asking");
                feedback = Some(mask(&why));
            }
        }
    }
    Err(PlanFailure::Rejected(VelocityError::Config(format!(
        "planner gave no usable answer for {:?}: {}",
        step.intent,
        feedback.unwrap_or_default()
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::tests::el;
    use crate::planner::{Op, PlannedStep};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use velocity_common::NaturalKind;
    use velocity_common::Selector;

    /// Replays scripted answers and records the feedback it was given.
    struct ScriptedPlanner {
        answers: StdMutex<Vec<PlannedStep>>,
        calls: Arc<AtomicUsize>,
        feedback: Arc<StdMutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl NaturalPlanner for ScriptedPlanner {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.feedback
                .lock()
                .unwrap()
                .push(req.feedback.map(String::from));
            let mut answers = self.answers.lock().unwrap();
            Ok(if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            })
        }
    }

    fn answer(op: Op, element: Option<usize>) -> PlannedStep {
        PlannedStep {
            op,
            element,
            text: String::new(),
            direction: "none".into(),
            ms: 0,
            key: Default::default(),
        }
    }

    struct Harness {
        resolver: NaturalResolver,
        calls: Arc<AtomicUsize>,
        feedback: Arc<StdMutex<Vec<Option<String>>>>,
    }

    fn harness(name: &str, answers: Vec<PlannedStep>) -> Harness {
        let dir = std::env::temp_dir().join(format!("velnl-res-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let feedback = Arc::new(StdMutex::new(Vec::new()));
        let planner = ScriptedPlanner {
            answers: StdMutex::new(answers),
            calls: calls.clone(),
            feedback: feedback.clone(),
        };
        Harness {
            resolver: NaturalResolver::new(Box::new(planner), dir.join("nl-cache.json")),
            calls,
            feedback,
        }
    }

    fn screen() -> Element {
        el("login", "button", Some("Login"), None)
    }

    fn step(intent: &str) -> NaturalStep<'_> {
        NaturalStep {
            intent,
            kind: NaturalKind::Act,
            data: None,
            data_name: None,
            wait_until: None,
            intent_template: None,
            platform: Platform::Android,
            scope: "",
            fresh: false,
            describe_data: false,
        }
    }

    // nl-cache.json is committed and so is untrusted: an edited entry must
    // pass the checks a fresh plan does before it is replayed.
    #[tokio::test]
    async fn a_hand_edited_cache_entry_is_checked_like_a_fresh_plan() {
        let dir = std::env::temp_dir().join(format!("velnl-res-{}-edited", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nl-cache.json");
        let sel = || Selector::Id("email".into());
        let mut cache = NlCache::default();
        let mut put = |s: &NaturalStep<'_>, action: Action| {
            cache.put(s.cache_key(), s.intent, Platform::Android, action);
        };
        let type_step = step("type \"alice@x.io\" into Email");
        let typed = |t: &str| Action::InputText {
            selector: sel(),
            text: t.into(),
        };
        put(&type_step, typed("attacker text"));
        let tap_step = step("tap login");
        put(&tap_step, typed("anything"));
        let goto_step = step("go to \"https://shop.example/\"");
        put(
            &goto_step,
            Action::Goto {
                url: "https://evil.example/\u{1}velocity:data\u{1}".into(),
            },
        );
        let wait_step = step("tap login again");
        put(&wait_step, Action::Wait { ms: 5000 });
        let gone_step = step("verify \"Welcome\" is gone");
        let gone = |t: &str| Action::AssertNotVisible {
            selector: Selector::TextContains(t.into()),
        };
        put(&gone_step, gone("something unrelated"));
        cache.save(&path).unwrap();
        let r = NaturalResolver::new(
            Box::new(Garbled {
                bad: AtomicUsize::new(0),
                feedback: StdMutex::new(Vec::new()),
            }),
            &path,
        );
        for s in [&type_step, &tap_step, &goto_step, &wait_step] {
            assert!(r.cached_hit(s).await.is_none(), "{}", s.intent);
        }
        let mut gone_step = gone_step;
        gone_step.kind = NaturalKind::Assert;
        assert!(r.cached_hit(&gone_step).await.is_none());

        // Entries a real plan would have stored still replay.
        let mut good = NlCache::default();
        let mut ok = |s: &NaturalStep<'_>, a: Action| {
            good.put(s.cache_key(), s.intent, Platform::Android, a)
        };
        ok(&type_step, typed(&literal_placeholder(0)));
        ok(
            &goto_step,
            Action::Goto {
                url: "https://shop.example/".into(),
            },
        );
        ok(&gone_step, gone("Welcome"));
        let path = dir.join("good.json");
        good.save(&path).unwrap();
        let r = NaturalResolver::new(
            Box::new(Garbled {
                bad: AtomicUsize::new(0),
                feedback: StdMutex::new(Vec::new()),
            }),
            &path,
        );
        let hit = r.cached_hit(&type_step).await.unwrap();
        assert!(matches!(hit.action, Action::InputText { ref text, .. } if text == "alice@x.io"));
        assert!(r.cached_hit(&goto_step).await.is_some());
        assert!(r.cached_hit(&gone_step).await.is_some());
    }

    #[tokio::test]
    async fn plans_once_then_serves_from_cache() {
        let h = harness("hit", vec![answer(Op::Tap, Some(0))]);
        let s = step("tap login");
        assert!(h.resolver.cached(&s).await.is_none());
        let a = h.resolver.resolve(&s, &screen()).await.unwrap();
        // resolve() never caches a plan the run hasn't verified.
        assert!(h.resolver.cached(&s).await.is_none());
        h.resolver.commit(&s, a.clone(), None, None).await;
        let b = h.resolver.resolve(&s, &screen()).await.unwrap();
        assert_eq!(
            h.calls.load(Ordering::SeqCst),
            1,
            "second resolve must hit cache"
        );
        assert!(matches!(a, Action::Tap { selector: Selector::Id(ref id) } if id == "login"));
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn invalidate_forces_replan() {
        let h = harness("heal", vec![answer(Op::Tap, Some(0))]);
        let s = step("tap x");
        let a = h.resolver.resolve(&s, &screen()).await.unwrap();
        h.resolver.commit(&s, a, None, None).await;
        h.resolver.invalidate(&s).await;
        h.resolver.resolve(&s, &screen()).await.unwrap();
        assert_eq!(h.calls.load(Ordering::SeqCst), 2);
    }

    // The cache contract: nothing is written until the caller commits the
    // plan it verified, and an unverified plan that is dropped leaves no
    // trace on disk.
    #[tokio::test]
    async fn a_prepared_plan_is_written_only_when_committed() {
        let h = harness("pending", vec![answer(Op::Tap, Some(0))]);
        let s = step("tap login");
        let a = h.resolver.plan(&s, &screen()).await.unwrap();
        let pending = h.resolver.prepare(&s, a, None, Some(true)).unwrap();
        assert_eq!(pending.template(), "tap login");
        assert!(cache_file("pending").is_empty(), "nothing on disk yet");
        assert!(h.resolver.cached(&s).await.is_none());
        // Dropped (the next step failed): never cached.
        drop(pending);
        assert!(h.resolver.cached(&s).await.is_none());
        let a = h.resolver.plan(&s, &screen()).await.unwrap();
        let pending = h.resolver.prepare(&s, a, None, Some(true)).unwrap();
        h.resolver.commit_pending([pending]).await;
        assert!(cache_file("pending").contains("tap login"));
        assert_eq!(h.resolver.cached_hit(&s).await.unwrap().effect, Some(true));
        // Read-only runs prepare nothing.
        let ro = harness("pending-ro", vec![answer(Op::Tap, Some(0))]);
        let ro = ro.resolver.with_mode(CacheMode::ReadOnly);
        let tap = Action::Tap {
            selector: Selector::Id("login".into()),
        };
        assert!(ro.prepare(&s, tap, None, None).is_none());
    }

    /// Fails with an unparseable reply `bad` times, then taps element 0.
    struct Garbled {
        bad: AtomicUsize,
        feedback: StdMutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl NaturalPlanner for Garbled {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.feedback
                .lock()
                .unwrap()
                .push(req.feedback.map(String::from));
            if self.bad.load(Ordering::SeqCst) > 0 {
                self.bad.fetch_sub(1, Ordering::SeqCst);
                return PlannedStep::from_reply("not json at all");
            }
            Ok(answer(Op::Tap, Some(0)))
        }
    }

    // VEL-249: a reply that didn't parse failed the step with no re-ask.
    #[tokio::test]
    async fn parse_error_is_reasked() {
        let dir = std::env::temp_dir().join(format!("velnl-garbled-{}", std::process::id()));
        let planner = Garbled {
            bad: AtomicUsize::new(1),
            feedback: StdMutex::new(Vec::new()),
        };
        let r = NaturalResolver::new(Box::new(planner), dir.join("nl-cache.json"));
        let a = r.plan(&step("tap login"), &screen()).await.unwrap();
        assert!(matches!(a, Action::Tap { .. }));
        // Always garbled: still an error in the end, not a loop.
        let planner = Garbled {
            bad: AtomicUsize::new(usize::MAX),
            feedback: StdMutex::new(Vec::new()),
        };
        let r = NaturalResolver::new(Box::new(planner), dir.join("nl-cache.json"));
        let e = r.plan(&step("tap login"), &screen()).await.unwrap_err();
        assert!(e.to_string().contains(UNPARSEABLE_REPLY), "{e}");
    }

    #[tokio::test]
    async fn bad_answer_is_reasked_with_feedback() {
        let h = harness(
            "reask",
            vec![answer(Op::Tap, Some(9)), answer(Op::Tap, Some(0))],
        );
        let a = h
            .resolver
            .plan(&step("tap login"), &screen())
            .await
            .unwrap();
        assert!(matches!(a, Action::Tap { .. }));
        let fb = h.feedback.lock().unwrap().clone();
        assert_eq!(fb.len(), 2);
        assert!(fb[0].is_none());
        assert!(fb[1].as_deref().unwrap().contains("not in the list"));
    }

    #[tokio::test]
    async fn a_cached_action_the_grounder_never_makes_is_not_replayed() {
        // Audit 2026-11 NL-1: nl-cache.json is committed; an entry a pull
        // request adds must not replay an http request, a launchApp that
        // wipes another app, or an hour-long wait.
        let h = harness("forged", vec![]);
        let s = step("tap login");
        for forged in [
            Action::LaunchApp {
                app_id: "com.other.bank".into(),
                clear_state: true,
                arguments: Default::default(),
            },
            Action::Wait { ms: 3_600_000 },
            Action::GoBack,
        ] {
            h.resolver
                .commit(&s, forged.clone(), None, Some(true))
                .await;
            let hit = h.resolver.cached(&s).await;
            if forged == Action::GoBack {
                assert_eq!(hit, Some(forged));
            } else {
                assert!(hit.is_none(), "{forged:?} replayed");
            }
        }
        let assert_step = NaturalStep {
            kind: NaturalKind::Assert,
            ..step("the cart is empty")
        };
        h.resolver
            .commit(
                &assert_step,
                Action::Tap {
                    selector: Selector::Id("buy".into()),
                },
                None,
                Some(true),
            )
            .await;
        assert!(h.resolver.cached(&assert_step).await.is_none());
    }

    #[tokio::test]
    async fn a_plan_is_cached_only_when_committed() {
        let h = harness("commit", vec![answer(Op::Tap, Some(0))]);
        let s = step("tap login");
        let a = h.resolver.plan(&s, &screen()).await.unwrap();
        assert!(
            h.resolver.cached(&s).await.is_none(),
            "not before it worked"
        );
        h.resolver.commit(&s, a, None, Some(true)).await;
        let hit = h.resolver.cached_hit(&s).await.unwrap();
        assert_eq!(hit.effect, Some(true));
        let retry = NaturalStep { fresh: true, ..s };
        assert!(
            h.resolver.cached(&retry).await.is_none(),
            "a retry re-plans"
        );
    }

    #[tokio::test]
    async fn data_is_cached_by_name_and_filled_in_on_replay() {
        let h = harness("data", vec![answer(Op::Tap, Some(0))]);
        let first = NaturalStep {
            data: Some("ann@example.com"),
            data_name: Some("${EMAIL}"),
            ..step("type the email")
        };
        let typed = Action::InputText {
            selector: Selector::Id("email".into()),
            text: "ann@example.com".into(),
        };
        h.resolver.commit(&first, typed, None, None).await;
        let second = NaturalStep {
            data: Some("bob@example.com"),
            ..first
        };
        match h.resolver.cached(&second).await {
            Some(Action::InputText { text, .. }) => assert_eq!(text, "bob@example.com"),
            other => panic!("{other:?}"),
        }

        // A selector that is the value is filled in for the next value
        // (Stagehand's %var% arguments); replay still checks it resolves.
        let pick = NaturalStep {
            data: Some("Large"),
            data_name: Some("${SIZE}"),
            ..step("pick the size")
        };
        let tap_large = Action::Tap {
            selector: Selector::Text("Large".into()),
        };
        h.resolver.commit(&pick, tap_large, None, None).await;
        let other_size = NaturalStep {
            data: Some("Small"),
            ..pick
        };
        assert_eq!(
            h.resolver.cached(&other_size).await,
            Some(Action::Tap {
                selector: Selector::Text("Small".into())
            })
        );
        assert!(!cache_file("data").contains("Large"));
    }

    // Deliverable: two runs of the same step with different data hit the
    // same entry and type the right value each time, `data:` written as a
    // literal or as a ${VAR} alike.
    #[tokio::test]
    async fn data_values_never_enter_the_key() {
        let h = harness("datafree", vec![answer(Op::Tap, Some(0))]);
        let typed = |v: &str| Action::InputText {
            selector: Selector::Id("email".into()),
            text: v.into(),
        };
        let run = |v: &'static str| NaturalStep {
            data: Some(v),
            ..step("type the email")
        };
        assert_eq!(run("a@x.io").cache_key(), run("b@x.io").cache_key());
        h.resolver
            .commit(&run("a@x.io"), typed("a@x.io"), None, None)
            .await;
        assert_eq!(
            h.resolver.cached(&run("b@x.io")).await,
            Some(typed("b@x.io"))
        );
        assert_eq!(
            h.resolver.cached(&run("a@x.io")).await,
            Some(typed("a@x.io"))
        );
        // A step without data is a different step.
        assert_ne!(
            run("a@x.io").cache_key(),
            step("type the email").cache_key()
        );
        assert!(!cache_file("datafree").contains("a@x.io"));
    }

    // A ${VAR} in the intent: keyed on the template, the value filled in.
    #[tokio::test]
    async fn an_interpolated_intent_is_keyed_on_its_template() {
        let h = harness("tmpl", vec![answer(Op::Tap, Some(0))]);
        let mk = |intent: &'static str| NaturalStep {
            intent_template: Some("type \"${USER}\" into the name field"),
            ..step(intent)
        };
        let ann = mk("type \"ann\" into the name field");
        let bob = mk("type \"bob\" into the name field");
        assert_eq!(ann.template(), bob.template());
        let typed = |v: &str| Action::InputText {
            selector: Selector::Id("name".into()),
            text: v.into(),
        };
        h.resolver.commit(&ann, typed("ann"), None, None).await;
        assert_eq!(h.resolver.cached(&bob).await, Some(typed("bob")));
        let file = cache_file("tmpl");
        assert!(
            file.contains("into the name field") && !file.contains("ann"),
            "{file}"
        );
    }

    // Quoted values that were typed are data: a slot in the key. Quoted
    // values that name the target stay in the key.
    #[tokio::test]
    async fn quoted_values_typed_as_data_become_slots() {
        let h = harness("slots", vec![answer(Op::Tap, Some(0))]);
        let typed = |v: &str| Action::InputText {
            selector: Selector::Id("email".into()),
            text: v.into(),
        };
        let a = step("type \"ann@x.io\" into the email field");
        let b = step("type \"bob@x.io\" into the email field");
        h.resolver.commit(&a, typed("ann@x.io"), None, None).await;
        assert_eq!(h.resolver.cached(&b).await, Some(typed("bob@x.io")));
        assert!(!cache_file("slots").contains("ann@x.io"));
        // A target named by a quoted value: "Sign up" never replays "Sign in".
        let tap = |t: &str| Action::Tap {
            selector: Selector::Text(t.into()),
        };
        h.resolver
            .commit(&step("tap \"Sign in\""), tap("Sign in"), None, None)
            .await;
        assert!(h.resolver.cached(&step("tap \"Sign up\"")).await.is_none());
        assert_eq!(
            h.resolver.cached(&step("tap \"Sign in\"")).await,
            Some(tap("Sign in"))
        );
        // Typing the field's own quoted name is not a slot either.
        let named = step("type \"Email\" into Email");
        let into_email = Action::InputText {
            selector: Selector::Text("Email".into()),
            text: "Email".into(),
        };
        h.resolver
            .commit(&named, into_email.clone(), None, None)
            .await;
        assert!(h
            .resolver
            .cached(&step("type \"Other\" into Email"))
            .await
            .is_none());
        assert_eq!(h.resolver.cached(&named).await, Some(into_email));
    }

    #[test]
    fn step_shape_blanks_every_quoted_value() {
        assert_eq!(
            step_shape("type \"a\" into \"Email\""),
            step_shape("type \"b\" into \"Name\"")
        );
        assert_ne!(step_shape("tap ok"), step_shape("tap cancel"));
    }

    // A selector holding only part of the value can't be filled in for
    // another value: bound to (a fingerprint of) the one it was planned
    // with, and a miss for any other.
    #[tokio::test]
    async fn an_entry_bound_to_its_value_misses_for_another() {
        let h = harness("bound", vec![answer(Op::Tap, Some(0))]);
        let first = NaturalStep {
            data: Some("Large"),
            ..step("pick the size")
        };
        let tap = Action::Tap {
            selector: Selector::Text("Size Large".into()),
        };
        h.resolver.commit(&first, tap.clone(), None, None).await;
        assert_eq!(h.resolver.cached(&first).await, Some(tap));
        let other = NaturalStep {
            data: Some("Small"),
            ..first
        };
        assert!(h.resolver.cached(&other).await.is_none());
    }

    // Patterns are never rewritten: a value inside a regex could change
    // what it matches.
    #[test]
    fn pattern_selectors_are_left_alone() {
        let s = NaturalStep {
            data: Some("a.b"),
            ..step("find it")
        };
        let action = Action::Tap {
            selector: Selector::TextMatches("a.b".into()),
        };
        let p = generalize(&s, action.clone(), None, None).unwrap();
        assert_eq!(p.action(), &action);
        // It holds the value, so it fits only that value.
        assert!(p.bound.is_some());
    }

    #[tokio::test]
    async fn read_only_mode_never_plans_and_never_writes() {
        let h = harness("ro", vec![answer(Op::Tap, Some(0))]);
        let resolver = h.resolver.with_mode(CacheMode::ReadOnly);
        let s = step("tap login");
        let err = resolver.plan(&s, &screen()).await.unwrap_err();
        assert!(err.to_string().contains("planning is off"), "{err}");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn prune_drops_entries_no_step_used() {
        let h = harness("prune", vec![answer(Op::Tap, Some(0))]);
        let resolver = h.resolver.with_mode(CacheMode::Prune);
        let tap = |t: &str| Action::Tap {
            selector: Selector::Text(t.into()),
        };
        resolver
            .commit(&step("old step"), tap("a"), None, None)
            .await;
        // A new run: only "kept step" is used.
        let fresh = NaturalResolver::new(
            Box::new(ScriptedPlanner {
                answers: StdMutex::new(vec![answer(Op::Tap, Some(0))]),
                calls: Arc::new(AtomicUsize::new(0)),
                feedback: Arc::new(StdMutex::new(Vec::new())),
            }),
            resolver.cache_path.clone(),
        )
        .with_mode(CacheMode::Prune);
        fresh.commit(&step("kept step"), tap("b"), None, None).await;
        assert_eq!(fresh.finish().await, 1);
        assert!(fresh.cached(&step("old step")).await.is_none());
        assert!(fresh.cached(&step("kept step")).await.is_some());
    }

    #[tokio::test]
    async fn persistent_bad_answers_fail_and_are_not_cached() {
        let h = harness("bad", vec![answer(Op::Tap, None)]);
        let s = step("tap ghost");
        assert!(h.resolver.plan(&s, &screen()).await.is_err());
        assert_eq!(h.calls.load(Ordering::SeqCst), 1 + MAX_REASKS);
        assert!(h.resolver.cached(&s).await.is_none());
    }

    #[tokio::test]
    async fn not_found_fails_without_reask() {
        let h = harness("nf", vec![answer(Op::NotFound, None)]);
        let r = h.resolver.plan(&step("tap ghost"), &screen()).await;
        assert!(matches!(r, Err(VelocityError::ElementNotFound { .. })));
        assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_persists_across_resolvers_per_platform() {
        let dir = std::env::temp_dir().join(format!("velnl-res-{}-persist", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nl-cache.json");
        let mk = |calls: Arc<AtomicUsize>| ScriptedPlanner {
            answers: StdMutex::new(vec![answer(Op::Tap, Some(0))]),
            calls,
            feedback: Arc::new(StdMutex::new(Vec::new())),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let first = NaturalResolver::new(Box::new(mk(calls.clone())), &path);
        let a = first.resolve(&step("go"), &screen()).await.unwrap();
        first.commit(&step("go"), a, None, None).await;
        let r2 = NaturalResolver::new(Box::new(mk(calls.clone())), &path);
        r2.resolve(&step("go"), &screen()).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "persisted cache should be reused"
        );

        let web = NaturalStep {
            platform: Platform::Web,
            ..step("go")
        };
        assert!(r2.cached(&web).await.is_none(), "cache is per platform");
    }

    fn cache_file(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("velnl-res-{}-{name}", std::process::id()));
        std::fs::read_to_string(dir.join("nl-cache.json")).unwrap_or_default()
    }

    /// Records what the model would be sent.
    struct Recording(Arc<StdMutex<Vec<String>>>, PlannedStep);

    #[async_trait]
    impl NaturalPlanner for Recording {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.0.lock().unwrap().push(format!(
                "intent={} data={:?} screen={}",
                req.intent, req.data, req.screen
            ));
            Ok(self.1.clone())
        }
    }

    // VEL-097: secret data and the screen showing it never reach the model;
    // the action still types the real value.
    #[tokio::test]
    async fn secret_data_is_described_not_sent() {
        let canary = "S3CR3T-CANARY-097a";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let mut step_answer = answer(Op::InputText, Some(0));
        step_answer.text = canary.into();
        let planner = Recording(seen.clone(), step_answer);
        let root = Element {
            children: vec![el("pw", "EditText", Some(canary), None)],
            ..el("hierarchy", "", None, None)
        };
        let s = NaturalStep {
            data: Some(canary),
            data_name: Some("${LOGIN_PASSWORD}"),
            ..step("type the password")
        };
        let a = plan_step(&planner, &s, &root).await.unwrap();
        assert!(matches!(a, Action::InputText { text, .. } if text == canary));
        let sent = seen.lock().unwrap().join("\n");
        assert!(!sent.contains(canary), "{sent}");
        assert!(sent.contains("<secret value, 18 chars>"), "{sent}");
    }

    // D-30: `nl.dataPolicy: describe` keeps every data value from the
    // model, not only secrets; the action still uses the real value.
    #[tokio::test]
    async fn described_data_never_reaches_the_model() {
        let value = "Ada Lovelace";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let mut step_answer = answer(Op::InputText, Some(0));
        step_answer.text = "<value>".into();
        let planner = Recording(seen.clone(), step_answer);
        let root = Element {
            children: vec![el("name", "EditText", Some(value), None)],
            ..el("hierarchy", "", None, None)
        };
        let s = NaturalStep {
            data: Some(value),
            data_name: Some("${CUSTOMER}"),
            describe_data: true,
            ..step("type the customer name")
        };
        let a = plan_step(&planner, &s, &root).await.unwrap();
        assert!(matches!(a, Action::InputText { text, .. } if text == value));
        let sent = seen.lock().unwrap().join("\n");
        assert!(!sent.contains(value), "{sent}");
        assert!(sent.contains("<value, 12 chars>"), "{sent}");
        // The default (verbatim) still sends it.
        seen.lock().unwrap().clear();
        let s = NaturalStep {
            describe_data: false,
            ..s
        };
        plan_step(&planner, &s, &root).await.unwrap();
        assert!(seen.lock().unwrap().join("\n").contains(value));
    }

    // Review M4: a cached secret assertion that fails is re-planned with a
    // hint quoting the error; neither the hint nor later feedback may carry
    // the secret (or any registered one) to the model.
    #[tokio::test]
    async fn replan_hint_and_feedback_are_masked() {
        let canary = "S3CR3T-CANARY-M4a";
        velocity_common::redact::register_secret("S3CR3T-CANARY-M4b");
        let h = harness(
            "m4hint",
            vec![
                answer(Op::AssertText, Some(9)),
                answer(Op::AssertText, Some(0)),
            ],
        );
        let s = NaturalStep {
            data: Some(canary),
            data_name: Some("${API_TOKEN}"),
            kind: NaturalKind::Assert,
            ..step("the token is shown")
        };
        let err = VelocityError::AssertionFailed {
            expected: canary.into(),
            actual: "S3CR3T-CANARY-M4b".into(),
            selector: "id=token".into(),
            screenshot: None,
        };
        let hint = format!("previously resolved to X and that no longer works: {err}");
        assert!(hint.contains(canary), "the hint quotes the secret: {hint}");
        let _ = h.resolver.plan_with_hint(&s, &screen(), Some(&hint)).await;
        let fb = h.feedback.lock().unwrap().clone();
        assert!(fb.len() >= 2, "{fb:?}");
        assert!(
            fb[0].as_deref().unwrap().contains("no longer works"),
            "{fb:?}"
        );
        for f in fb.iter().flatten() {
            assert!(!f.contains("S3CR3T-CANARY-M4"), "{f}");
        }
    }

    // Review MINOR 10: an unnamed step with secret data is cached (its tap
    // doesn't contain it) under a key that doesn't hash the secret.
    #[tokio::test]
    async fn a_secret_is_never_hashed_into_a_cache_key() {
        let secret = "pw-MINOR10";
        velocity_common::redact::register_secret(secret);
        let h = harness("secretkey", vec![answer(Op::Tap, Some(0))]);
        let s = NaturalStep {
            data: Some(secret),
            ..step("tap login")
        };
        let tap = Action::Tap {
            selector: Selector::Id("login".into()),
        };
        h.resolver.commit(&s, tap, None, None).await;
        let file = cache_file("secretkey");
        assert!(file.contains("login"), "still cached: {file}");
        assert!(!file.contains(secret), "{file}");
        // The key doesn't depend on the value at all.
        let other = NaturalStep {
            data: Some("another-value"),
            ..s
        };
        assert_eq!(s.cache_key(), other.cache_key());
        assert!(h.resolver.cached(&s).await.is_some());
    }

    // VEL-096: an assertion on secret data, a selector containing it, or an
    // intent that interpolated a secret never land in nl-cache.json.
    #[tokio::test]
    async fn secrets_never_reach_the_cache_file() {
        let canary = "S3CR3T-CANARY-096b";
        let h = harness("secretcache", vec![answer(Op::Tap, Some(0))]);
        let s = NaturalStep {
            data: Some(canary),
            data_name: Some("${API_TOKEN}"),
            ..step("the token is shown")
        };
        let check = Action::AssertText {
            selector: Selector::Id("token".into()),
            expected: canary.into(),
        };
        h.resolver.commit(&s, check, None, None).await;
        // Replayed with the run's value.
        match h.resolver.cached(&s).await {
            Some(Action::AssertText { expected, .. }) => assert_eq!(expected, canary),
            other => panic!("{other:?}"),
        }
        // A selector that is the secret is stored as a placeholder too; one
        // that holds part of it is not stored at all.
        let tap = Action::Tap {
            selector: Selector::Text(canary.into()),
        };
        h.resolver.commit(&s, tap.clone(), None, None).await;
        assert_eq!(h.resolver.cached(&s).await, Some(tap));
        let partial = NaturalStep {
            intent: "tap the token row",
            ..s
        };
        let row = Action::Tap {
            selector: Selector::Text(format!("Token: {canary}")),
        };
        h.resolver.commit(&partial, row, None, None).await;
        assert!(h.resolver.cached(&partial).await.is_none());
        velocity_common::redact::register_secret("S3CR3T-CANARY-096c");
        let in_intent = step("type S3CR3T-CANARY-096c into the box");
        let typed = Action::Tap {
            selector: Selector::Id("box".into()),
        };
        h.resolver.commit(&in_intent, typed, None, None).await;
        let file = cache_file("secretcache");
        assert!(!file.contains("S3CR3T-CANARY-096"), "{file}");
        assert!(
            file.contains("velocity:data"),
            "kept with a placeholder: {file}"
        );
    }

    // VEL-S17: typing "Password" into a field labelled Password stores a
    // placeholder, not the value.
    #[tokio::test]
    async fn typed_value_inside_the_selector_is_still_a_placeholder() {
        let h = harness("s17", vec![answer(Op::Tap, Some(0))]);
        let s = NaturalStep {
            data: Some("Hunter-Value-17"),
            data_name: Some("${FIELD_TEXT}"),
            ..step("type it")
        };
        let typed = Action::InputText {
            selector: Selector::Text("Hunter-Value-17 field".into()),
            text: "Hunter-Value-17".into(),
        };
        h.resolver.commit(&s, typed, None, None).await;
        let file = cache_file("s17");
        assert!(file.contains("velocity:data"), "{file}");
        match h.resolver.cached(&s).await {
            Some(Action::InputText { text, .. }) => assert_eq!(text, "Hunter-Value-17"),
            other => panic!("{other:?}"),
        }
    }
}
