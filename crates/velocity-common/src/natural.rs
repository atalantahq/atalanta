//! The interface between the executor and the natural-language resolver.
//!
//! `velocity-core` runs `do:`/`assert:` and bare-string steps through a
//! [`NaturalStepResolver`]; `velocity-nl` implements it (`NaturalResolver`:
//! the planner, grounding and the disk cache). Keeping the trait and its
//! plain data here lets the deterministic executor build without the model
//! clients and their HTTP stack (review 06, finding 5).

use async_trait::async_trait;

use crate::{Action, Element, NaturalKind, Platform, Rect, Result, VerifyCheck};

/// Whether `action` is one grounding can produce for a step of `kind`:
/// what a cached resolution (`.velocity/nl-cache.json`, a committed file)
/// may replay. The cache stores a whole `Action`, so an entry a pull
/// request adds could otherwise be an `http` request posting the step's
/// `data` (a password) anywhere, a `launchApp` that wipes another app, or a
/// `setConfig`, with no model call: model picks, code decides holds only
/// if a cached answer is held to what the grounder emits (audit 2026-11
/// NL-1).
pub fn cacheable(kind: NaturalKind, action: &Action) -> bool {
    /// Grounding's own limits (`velocity-nl/src/grounding.rs`).
    const MAX_WAIT_MS: u64 = 30_000;
    const MAX_LONG_PRESS_MS: u64 = 10_000;
    match kind {
        NaturalKind::Assert => matches!(
            action,
            Action::AssertVisible { .. }
                | Action::AssertNotVisible { .. }
                | Action::AssertText { .. }
        ),
        NaturalKind::Act => match action {
            Action::Wait { ms } => *ms <= MAX_WAIT_MS,
            Action::LongPress { duration_ms, .. } => {
                duration_ms.is_none_or(|ms| ms <= MAX_LONG_PRESS_MS)
            }
            Action::Tap { .. }
            | Action::DoubleTap { .. }
            | Action::InputText { .. }
            | Action::ClearText { .. }
            | Action::Swipe { .. }
            | Action::ScrollUntilVisible { .. }
            | Action::PressKey { .. }
            | Action::Goto { .. }
            | Action::GoBack
            | Action::Refresh
            | Action::AssertVisible { .. }
            | Action::AssertNotVisible { .. }
            | Action::AssertText { .. } => true,
            _ => false,
        },
    }
}

/// One natural-language step, as written in the test.
#[derive(Debug, Clone, Copy)]
pub struct NaturalStep<'a> {
    pub intent: &'a str,
    pub kind: NaturalKind,
    pub data: Option<&'a str>,
    /// The `data` as written, before `${VAR}` substitution. A data value
    /// is never part of the cache key; a name marks the value as the
    /// test's, so it is masked like a secret when the name looks like one.
    pub data_name: Option<&'a str>,
    pub wait_until: Option<&'a str>,
    pub platform: Platform,
    /// Separates identical step text that appears more than once (e.g. "tap
    /// Next" on two screens) so each keeps its own cached action.
    pub scope: &'a str,
    /// Skip the cache (a retry attempt must re-plan, not replay what just
    /// failed).
    pub fresh: bool,
    /// `nl.dataPolicy: describe` (D-30): the model gets a description of
    /// the data (`<value, 5 chars>`), never the value, and the value is
    /// masked on the screen it reads. Grounding still uses the real value.
    pub describe_data: bool,
    /// `intent` as written, before `${VAR}` substitution (when it had
    /// any). Steps are cached under it, so a new value replays the same
    /// plan instead of re-planning (Stagehand, Arbigent).
    pub intent_template: Option<&'a str>,
}

impl<'a> NaturalStep<'a> {
    /// The step as written: `${VAR}`s unresolved.
    pub fn written(&self) -> &'a str {
        self.intent_template.unwrap_or(self.intent)
    }

    /// The quoted values of the step as it runs, when they line up one to
    /// one with the quoted spans of the step as written (a `${VAR}` that
    /// expands to a quote breaks that, and then no value is a slot).
    pub fn literals(&self) -> Option<Vec<&'a str>> {
        let written = quoted_spans(self.written());
        let resolved = quoted_spans(self.intent);
        (written.len() == resolved.len() && !resolved.is_empty()).then_some(resolved)
    }

    /// The step as written with the quoted values at `slots` replaced by
    /// a slot marker: `type "ann@x.io" into Email` and `type "bob@x.io"
    /// into Email` share `type "␁0" into Email` when the quoted value was
    /// the text typed.
    pub fn template_with(&self, slots: &[usize]) -> String {
        let written = self.written();
        if slots.is_empty() {
            return written.to_string();
        }
        let mut out = String::with_capacity(written.len());
        let mut at = 0;
        for (i, span) in quoted_spans(written).into_iter().enumerate() {
            let start = span.as_ptr() as usize - written.as_ptr() as usize;
            out.push_str(&written[at..start]);
            if slots.contains(&i) {
                out.push_str(&format!("\u{1}{i}"));
            } else {
                out.push_str(span);
            }
            at = start + span.len();
        }
        out.push_str(&written[at..]);
        out
    }

    /// The step's template: as written, no slots.
    pub fn template(&self) -> String {
        self.template_with(&[])
    }
}

/// The written step with every quoted value blanked: what the executor
/// counts occurrences by, so `type "a"` then `type "b"` in one test are the
/// first and second of one step and never share an entry.
pub fn step_shape(written: &str) -> String {
    let mut out = String::with_capacity(written.len());
    let mut at = 0;
    for span in quoted_spans(written) {
        let start = span.as_ptr() as usize - written.as_ptr() as usize;
        out.push_str(&written[at..start]);
        out.push('\u{1}');
        at = start + span.len();
    }
    out.push_str(&written[at..]);
    out
}

/// The quoted literals in the step: `"…"`, `“…”`, `` `…` ``, and `'…'` when
/// the opening quote starts a word (so "don't" isn't one).
pub fn quoted_spans(intent: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut prev: Option<char> = None;
    let mut iter = intent.char_indices();
    while let Some((i, c)) = iter.next() {
        let close = match c {
            '"' => Some('"'),
            '“' => Some('”'),
            '`' => Some('`'),
            '\'' | '‘' if prev.is_none_or(|p| !p.is_alphanumeric()) => {
                Some(if c == '‘' { '’' } else { '\'' })
            }
            _ => None,
        };
        prev = Some(c);
        let Some(close) = close else { continue };
        let start = i + c.len_utf8();
        if let Some(end) = intent[start..].find(close) {
            let span = intent[start..start + end].trim();
            if !span.is_empty() {
                spans.push(span);
            }
            // Skip past the closing quote.
            for (j, ch) in iter.by_ref() {
                if j >= start + end {
                    prev = Some(ch);
                    break;
                }
            }
        }
    }
    spans
}

/// A cached resolution with what was observed when it was planned.
#[derive(Debug, Clone)]
pub struct CachedHit {
    pub action: Action,
    /// How many elements an indexed selector chose among.
    pub twins: Option<usize>,
    /// Whether the action changed the screen when it was planned.
    pub effect: Option<bool>,
    /// The fingerprint of the only value this entry fits, if it fits only
    /// one.
    pub bound: Option<String>,
}

/// A working resolution waiting to be cached, with the test's data
/// already taken out.
///
/// The cache contract: a plan is written only after the run verified it.
/// The executor [`prepare`](NaturalStepResolver::prepare)s one when the
/// action and its `waitUntil` passed, holds it, and commits it when the
/// next step passes or the test passes (with `verify:` on, only when the
/// test passes, checks included). A plan the run didn't verify is dropped.
/// The executor only holds it; the fields are the resolver's.
#[derive(Debug, Clone)]
pub struct PendingPlan {
    pub key: String,
    pub template: String,
    pub platform: Platform,
    pub action: Action,
    pub twins: Option<usize>,
    pub effect: Option<bool>,
    pub bound: Option<String>,
}

impl PendingPlan {
    /// The step as written (never its values).
    pub fn template(&self) -> &str {
        &self.template
    }

    /// The stored action, placeholders in place of the data.
    pub fn action(&self) -> &Action {
        &self.action
    }
}

/// Resolves natural-language steps to concrete [`Action`]s for the
/// executor. Implemented by `velocity_nl::NaturalResolver`.
#[async_trait]
pub trait NaturalStepResolver: Send + Sync {
    /// The cached resolution for `step`, if any: the fast path, no model
    /// call and no screen capture.
    async fn cached_hit(&self, step: &NaturalStep<'_>) -> Option<CachedHit>;

    /// Plan `step` against `hierarchy` (grounded in `screen`, the rect the
    /// driver matches selectors in). Not cached: see
    /// [`prepare`](Self::prepare). `hint` describes a previous resolution
    /// that stopped working.
    async fn plan_on_screen(
        &self,
        step: &NaturalStep<'_>,
        hierarchy: &Element,
        screen: Option<&Rect>,
        hint: Option<&str>,
    ) -> Result<Action>;

    /// What caching `action` for `step` would write, held until the run
    /// verified it; `None` when nothing may be written.
    fn prepare(
        &self,
        step: &NaturalStep<'_>,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
    ) -> Option<PendingPlan>;

    /// Write verified plans to the cache.
    async fn commit_pending(&self, plans: Vec<PendingPlan>);

    /// Forget a step's cached action so it re-plans next time.
    async fn invalidate(&self, step: &NaturalStep<'_>);

    /// Ask the AI checker whether `claim` holds on `hierarchy`. `at` names
    /// the point in the test. Without a checker the outcome is `error`.
    async fn check(
        &self,
        at: &str,
        claim: &str,
        hierarchy: &Element,
        screen: Option<&Rect>,
        platform: Platform,
        screenshot: Option<&[u8]>,
    ) -> VerifyCheck;
}

/// Every AI provider preset name (`ai.provider`), for the schema and the
/// validator. `velocity_nl::provider::PRESETS` holds the presets; a test
/// there keeps the two in step.
pub const AI_PROVIDER_PRESETS: &[&str] = &[
    "anthropic",
    "openai",
    "azure-openai",
    "gemini",
    "openrouter",
    "groq",
    "mistral",
    "deepseek",
    "xai",
    "together",
    "fireworks",
    "dashscope",
    "perplexity",
    "bedrock",
    "vertex",
    "ollama",
    "lmstudio",
    "vllm",
    "openai-compatible",
];

/// Whether a natural-language step may go to `url` (D-17): an `http` or
/// `https` URL (or one relative to the page), on the same origin as
/// `current` (the page now shown) unless `allow_cross_origin`
/// (`nl.allowCrossOrigin: true`). `Err` says why not.
pub fn check_goto(
    url: &str,
    current: Option<&str>,
    allow_cross_origin: bool,
) -> std::result::Result<(), String> {
    use url::Url;
    let base = current.and_then(|c| Url::parse(c).ok());
    let target = match (Url::parse(url.trim()), &base) {
        (Ok(u), _) => u,
        // Relative (`/pricing`, `//host/x`): resolved as the page would.
        (Err(_), Some(b)) => b
            .join(url.trim())
            .map_err(|e| format!("goto {url:?}: not a URL ({e})"))?,
        // No page to resolve it against: the driver says so.
        (Err(_), None) => return Ok(()),
    };
    if !matches!(target.scheme(), "http" | "https") {
        return Err(format!(
            "goto {url:?}: a natural-language step may only open http or https URLs \
             (use an explicit `openLink:` step for {}: links)",
            target.scheme()
        ));
    }
    if allow_cross_origin {
        return Ok(());
    }
    match base.filter(|b| matches!(b.scheme(), "http" | "https")) {
        Some(b) if b.origin() != target.origin() => Err(format!(
            "goto {url:?} leaves the current page's origin ({}); a natural-language step \
             stays on it unless the suite sets `nl.allowCrossOrigin: true`",
            b.origin().ascii_serialization()
        )),
        _ => Ok(()),
    }
}
