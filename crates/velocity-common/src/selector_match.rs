//! Matching selectors against an accessibility tree. One implementation for
//! every platform (Android, iOS, headless, React Native, Flutter, web tree
//! fallback), so a selector means the same thing everywhere.
//!
//! Resolution rules (aligned with Maestro's, which users migrate from):
//! - `id:` matches the whole id, or its short form after `:id/` or `/`
//!   (`login` matches `com.app:id/login`). It used to match any substring, so
//!   `btn` also hit `btn_delete`. The `/` form doesn't apply to an id that is
//!   just the element's label (iOS derives one when there is no identifier):
//!   `id: Settings` hit a label `General/Settings` (VEL-237).
//! - An empty `id`, `text`, `textContains`, `accessibilityId` or `className`
//!   matches nothing (VEL-236: selectors built without the validator, as
//!   MCP's are, matched every element).
//! - Text is compared after [`normalize`] on both sides (D-10): zero-width
//!   characters dropped, whitespace runs (newlines included) one space,
//!   trimmed; the web driver does the same in `injected.js`.
//! - `text:` compares exactly; only when the whole selector finds nothing is
//!   it tried again with every `text:` part ignoring case, inside compounds
//!   and relations too (VEL-234), and then once more against one part of a
//!   merged label (React Native on iOS joins a touchable's children with
//!   ", ", Flutter merges semantics with newlines). An earlier tier's match
//!   always wins. A compound is
//!   tiered as a whole; an `index:` takes its tier from its inner selector,
//!   and a relation tiers its target and its anchor each on its own. The
//!   executor caps lookups at the case-folded tier while it polls and for
//!   negative checks, and widens to merged-label parts only for one last
//!   look at a lookup's deadline ([`with_ceiling`]).
//! - When a match contains another match (a clickable wrapper and the label
//!   inside it carry the same text), the innermost one wins.
//! - `index:` counts matches in on-screen order (top to bottom, then left to
//!   right), not tree order. A bare selector that matches several elements
//!   takes the first in tree order.

use crate::types::{Element, Rect, Relation, Selector};

/// Check if an element matches a selector.
///
/// When `screen` is `Some`, elements that are not visible, have zero-area bounds,
/// or fall outside the screen rect are rejected before matching. When `screen` is
/// `None` these visibility/bounds checks are skipped (useful for bridge drivers
/// that do not track screen geometry).
pub fn matches_selector(element: &Element, selector: &Selector, screen: Option<&Rect>) -> bool {
    matches_in(element, selector, &Want::Nothing, screen, Tier::Exact)
}

/// How `text:` parts compare, tier by tier: exactly; ignoring case; then
/// ignoring case against one part of a merged label. Ordered from strict to
/// loose, so a ceiling can be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Exact,
    Fold,
    Loose,
}

tokio::task_local! {
    static CEILING: Tier;
}

/// Run `f` with lookups inside it (every driver's tree matching) going no
/// looser than `max`. The executor polls under [`Tier::Fold`] and widens to
/// [`Tier::Loose`] only for one last look at the deadline, so a merged
/// label that is on screen before the real target can't win the race, and
/// a negative check ("is it gone?") never sees a part of a label as the
/// element (review M2/M3). Outside any scope lookups use every tier.
pub async fn with_ceiling<F: std::future::Future>(max: Tier, f: F) -> F::Output {
    CEILING.scope(max, f).await
}

/// The loosest tier lookups may use here (see [`with_ceiling`]).
pub fn ceiling() -> Tier {
    CEILING.try_with(|t| *t).unwrap_or(Tier::Loose)
}

/// The tiers tried in turn, up to `max`: a selector without a `text:` part
/// has one.
fn tiers_upto(selector: &Selector, max: Tier) -> &'static [Tier] {
    const ALL: [Tier; 3] = [Tier::Exact, Tier::Fold, Tier::Loose];
    if has_text(selector) {
        &ALL[..=max as usize]
    } else {
        &ALL[..1]
    }
}

/// Characters dropped before any text comparison: zero-width space, joiner
/// and non-joiner, word joiner and the byte-order mark. The same set as
/// `injected.js`'s `ZERO_WIDTH` (D-10).
fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
}

/// Text as every driver compares it (D-10): zero-width characters dropped,
/// every whitespace run (newlines, tabs, NBSP and U+202F included) one
/// space, trimmed. Applied to both the selector's value and the element's,
/// so `Sign\nin` on screen is `text: Sign in`, and a stray NBSP or
/// zero-width space in an app string no longer hides it. Web does the same
/// in `injected.js` (plus NFC, which devices already deliver).
pub fn normalize(s: &str) -> std::borrow::Cow<'_, str> {
    // Already normal (the common case): no copy.
    let mut after_space = true;
    let clean = s.chars().all(|c| {
        let ok = !is_zero_width(c) && (!c.is_whitespace() || (c == ' ' && !after_space));
        after_space = c.is_whitespace();
        ok
    }) && !after_space;
    if clean || s.is_empty() {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars().filter(|c| !is_zero_width(*c)) {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// A selector's own values, prepared once per tree walk rather than once
/// per element compared: its text normalised (and lowercased when a tier
/// ignores case), its role lowercased, its pattern looked up. Built from
/// the selector it is passed with ([`Want::of`]).
enum Want<'s> {
    /// `text:`, `name:` or `accessibilityId:`.
    Text(Wanted<'s>),
    /// `textContains:`, normalised and lowercased.
    Contains(String),
    /// `role:`, trimmed and lowercased.
    Role(String),
    /// `textMatches:` / `idMatches:`.
    Pattern(std::sync::Arc<CompiledPattern>),
    Compound(Vec<Want<'s>>),
    /// Nothing prepared: [`matches_in`] prepares what it needs as it goes
    /// (one element at a time, as [`matches_selector`] tests).
    Nothing,
}

impl<'s> Want<'s> {
    fn of(selector: &'s Selector) -> Self {
        match selector {
            Selector::Text(t) | Selector::Name(t) | Selector::AccessibilityId(t) => {
                Want::Text(Wanted {
                    norm: normalize(t),
                    lower: std::cell::OnceCell::new(),
                })
            }
            Selector::TextContains(t) => Want::Contains(normalize(t).to_lowercase()),
            Selector::Role(r) => Want::Role(r.trim().to_ascii_lowercase()),
            Selector::TextMatches(p) | Selector::IdMatches(p) => Want::Pattern(compiled_pattern(p)),
            Selector::Compound(parts) => Want::Compound(parts.iter().map(Want::of).collect()),
            _ => Want::Nothing,
        }
    }
}

/// A selector's text normalised; lowercased the first time a tier ignores
/// case (the exact tier never needs it).
struct Wanted<'s> {
    norm: std::borrow::Cow<'s, str>,
    lower: std::cell::OnceCell<String>,
}

impl Wanted<'_> {
    fn lower(&self) -> &str {
        self.lower.get_or_init(|| self.norm.to_lowercase())
    }
}

/// `have.to_lowercase() == lower`, without building the lowercase: ASCII
/// lowercases byte for byte, other text char by char, except a capital
/// sigma, whose lowercase depends on where it stands in a word.
fn lower_eq(have: &str, lower: &str) -> bool {
    if have.is_ascii() {
        return have.len() == lower.len()
            && have
                .bytes()
                .zip(lower.bytes())
                .all(|(h, l)| h.to_ascii_lowercase() == l);
    }
    if have.contains('Σ') {
        return have.to_lowercase() == lower;
    }
    have.chars().flat_map(char::to_lowercase).eq(lower.chars())
}

/// `have.to_lowercase().contains(lower)`, without building the lowercase
/// when `have` is ASCII (most device text).
fn lower_contains(have: &str, lower: &str) -> bool {
    if !have.is_ascii() {
        return have.to_lowercase().contains(lower);
    }
    let (have, lower) = (have.as_bytes(), lower.as_bytes());
    lower.is_empty()
        || have.windows(lower.len()).any(|w| {
            w.iter()
                .zip(lower)
                .all(|(h, l)| h.to_ascii_lowercase() == *l)
        })
}

/// Whether `have` (an element's value) equals `want` (a selector's) once
/// both are normalised, or ignoring case too under `fold`.
fn text_eq(have: &Option<String>, want: &Wanted, fold: bool) -> bool {
    let Some(have) = have else {
        return false;
    };
    let have = normalize(have);
    if fold {
        lower_eq(&have, want.lower())
    } else {
        *have == *want.norm
    }
}

/// `want` against one part of `s` split on newlines and ", " (a merged
/// label) — normalised, ignoring case. The whole value with its newlines
/// as spaces is already an exact match after [`normalize`].
fn loose_eq(s: &Option<String>, want: &Wanted) -> bool {
    let Some(s) = s else {
        return false;
    };
    let want = want.lower();
    lower_eq(&normalize(s), want)
        || s.split('\n')
            .flat_map(|line| line.split(", "))
            .any(|part| lower_eq(&normalize(part), want))
}

/// Whether `have` contains `lower` (a selector's value normalised and
/// lowercased), `have` normalised (D-10), ignoring case (`textContains` is
/// a search, as Playwright's default substring match is).
fn text_contains(have: &Option<String>, lower: &str) -> bool {
    have.as_deref()
        .is_some_and(|h| lower_contains(&normalize(h), lower))
}

fn matches_in(
    element: &Element,
    selector: &Selector,
    want: &Want,
    screen: Option<&Rect>,
    case: Tier,
) -> bool {
    if let Some(screen) = screen {
        if !element.visible || element.bounds.width <= 0 || element.bounds.height <= 0 {
            return false;
        }
        if !element.bounds.intersects(screen) {
            return false;
        }
    }

    match (selector, want) {
        (Selector::Id(id), _) => id_matches(element, id) || element.label.as_deref() == Some(id),
        (Selector::Text(_), Want::Text(text)) if text.norm.is_empty() => false,
        (Selector::Text(_), Want::Text(text)) if case == Tier::Loose => {
            loose_eq(&element.text, text) || loose_eq(&element.label, text)
        }
        (Selector::Text(_), Want::Text(text)) => {
            let fold = case == Tier::Fold;
            text_eq(&element.text, text, fold) || text_eq(&element.label, text, fold)
        }
        (Selector::TextContains(_), Want::Contains(lower)) => {
            !lower.is_empty()
                && (text_contains(&element.text, lower) || text_contains(&element.label, lower))
        }
        (Selector::AccessibilityId(aid), Want::Text(text)) => {
            !aid.is_empty() && (text_eq(&element.label, text, false) || element.platform_id == *aid)
        }
        (Selector::TextMatches(pattern), Want::Pattern(compiled)) => {
            [&element.text, &element.label, &element.hint]
                .into_iter()
                .flatten()
                .any(|v| pattern_matches_with(pattern, compiled, v))
        }
        (Selector::IdMatches(pattern), Want::Pattern(compiled)) => {
            let id = element.platform_id.as_str();
            !id.is_empty()
                && (pattern_matches_with(pattern, compiled, id)
                    || id
                        .rsplit_once(":id/")
                        .is_some_and(|(_, short)| pattern_matches_with(pattern, compiled, short)))
        }
        (Selector::ClassName(cls), _) => class_matches(&element.element_type, cls),
        (Selector::Index { .. }, _) => false, // Handled at find level
        (Selector::Compound(selectors), Want::Compound(wants)) => selectors
            .iter()
            .zip(wants)
            .all(|(s, w)| matches_in(element, s, w, screen, case)),
        // One element: each part prepared as it is tested.
        (Selector::Compound(selectors), _) => selectors
            .iter()
            .all(|s| matches_in(element, s, &Want::Nothing, screen, case)),
        // CSS and XPath selectors are resolved by the web driver via CDP,
        // not by tree-walking. They never match in the mobile tree.
        (Selector::Css(_) | Selector::XPath(_), _) => false,
        // Needs the whole tree; resolved at find level.
        (Selector::Relative { .. }, _) => false,
        // S1: Playwright's role names; a device type maps conservatively.
        (Selector::Role(_), Want::Role(want)) => {
            !want.is_empty() && crate::role::role_of(element).is_some_and(|r| r == want)
        }
        // The accessible name, compared like `text:` (tier by tier).
        (Selector::Name(_), Want::Text(name)) if name.norm.is_empty() => false,
        (Selector::Name(_), Want::Text(name)) => {
            let have = crate::role::accessible_name(element);
            match case {
                Tier::Loose => loose_eq(&have, name),
                _ => text_eq(&have, name, case == Tier::Fold),
            }
        }
        // S2 state filters (D-39 fields). Only a checkable element has a
        // checked state, so `checked: false` never matches a plain label.
        (Selector::Checked(want), _) => element.checked == Some(*want),
        // A state the driver can't report (`None`, headless) matches
        // neither `true` nor `false`, as `checked` on a plain label.
        (Selector::Focused(want), _) => element.focused == Some(*want),
        (Selector::Selected(want), _) => element.selected == Some(*want),
        (Selector::Enabled(want), _) => element.enabled == *want,
        // Not prepared yet ([`Want::Nothing`]): `Want::of` gives each of
        // the selectors above the form its arm takes. One it has nothing
        // for has an arm that ignores `want`, so this can't loop.
        (_, _) => {
            let want = Want::of(selector);
            !matches!(want, Want::Nothing) && matches_in(element, selector, &want, screen, case)
        }
    }
}

/// The regex `textMatches` / `idMatches` use for `pattern` (D-11): the
/// whole value must match, ignoring case, `.` matching newlines and `^`/`$`
/// matching at line ends (Maestro's `IGNORE_CASE | DOTALL | MULTILINE` with
/// Kotlin's whole-string `matches`). Rust's `regex` runs in linear time, so
/// a suite's pattern can't hang a lookup. An invalid pattern is an error
/// naming the problem (the validator reports it).
pub fn compile_pattern(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.trim().is_empty() {
        return Err("the pattern is empty".into());
    }
    regex::RegexBuilder::new(&format!(r"\A(?:{pattern})\z"))
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .multi_line(true)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| match e {
            regex::Error::Syntax(msg) => msg
                .lines()
                .last()
                .unwrap_or("invalid regular expression")
                .trim_start_matches("error: ")
                .to_string(),
            other => other.to_string(),
        })
}

/// The first invalid `textMatches` / `idMatches` pattern anywhere in
/// `selector`, as an error message. For selectors built without the
/// validator (MCP), where a bad pattern would otherwise match nothing.
pub fn check_patterns(selector: &Selector) -> Result<(), String> {
    match selector {
        Selector::TextMatches(p) | Selector::IdMatches(p) => compile_pattern(p)
            .map(|_| ())
            .map_err(|e| format!("{selector} is not a valid regular expression: {e}")),
        Selector::Index { selector, .. } => check_patterns(selector),
        Selector::Compound(parts) => parts.iter().try_for_each(check_patterns),
        Selector::Relative { target, anchor, .. } => {
            check_patterns(target).and_then(|_| check_patterns(anchor))
        }
        _ => Ok(()),
    }
}

/// Whether `value` matches `pattern` as `textMatches` means it: the regex
/// (see [`compile_pattern`]) against the value, the value with its newlines
/// read as spaces, or the value normalised; or the pattern taken literally
/// (Maestro: `Price ($)` finds the text it names). An invalid pattern
/// matches only literally. Compiled patterns are cached.
pub fn pattern_matches(pattern: &str, value: &str) -> bool {
    pattern_matches_with(pattern, &compiled_pattern(pattern), value)
}

/// [`pattern_matches`] with `pattern` already compiled (once per lookup,
/// not once per element).
fn pattern_matches_with(pattern: &str, compiled: &CompiledPattern, value: &str) -> bool {
    let literal = |v: &str| v == pattern || lower_eq(v, &compiled.lower);
    let spaced = if value.contains('\n') {
        std::borrow::Cow::Owned(value.replace('\n', " "))
    } else {
        std::borrow::Cow::Borrowed(value)
    };
    let normal = normalize(value);
    let candidates = [value, spaced.as_ref(), normal.as_ref()];
    if candidates.iter().any(|v| literal(v)) {
        return true;
    }
    // The same string twice (a value already one line, already normal)
    // runs the regex once.
    compiled.re.as_ref().is_some_and(|re| {
        candidates
            .iter()
            .enumerate()
            .any(|(i, v)| !candidates[..i].contains(v) && re.is_match(v))
    })
}

/// A `textMatches` / `idMatches` pattern compiled once: the regex (`None`
/// when invalid) and the pattern lowercased.
#[derive(Debug)]
pub struct CompiledPattern {
    pub re: Option<regex::Regex>,
    pub lower: String,
}

/// `pattern` compiled, from a process-wide cache. Shared, not cloned: a
/// cloned `Regex` starts a new match-cache pool, so every element of a
/// lookup rebuilt its lazy DFA (review MINOR 9). Looked up before
/// allocating a key.
pub fn compiled_pattern(pattern: &str) -> std::sync::Arc<CompiledPattern> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<CompiledPattern>>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(c) = cache.get(pattern) {
        return c.clone();
    }
    if cache.len() > 256 {
        cache.clear();
    }
    let c = Arc::new(CompiledPattern {
        re: compile_pattern(pattern).ok(),
        lower: pattern.to_lowercase(),
    });
    cache.insert(pattern.to_string(), c.clone());
    c
}

/// `visibility.minArea` (D-1): the share of an element's box that must be
/// on screen for an assertion to count it as visible. 0 (the default) is
/// any pixel; Maestro's rule is 0.1.
pub fn min_visible_area() -> f64 {
    crate::env::var("VELOCITY_VISIBILITY_MIN_AREA")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite())
        .map_or(0.0, |f| f.clamp(0.0, 1.0))
}

/// The share (0..1) of `bounds` inside `screen`.
pub fn visible_fraction(bounds: &Rect, screen: &Rect) -> f64 {
    let area = |w: i64, h: i64| (w.max(0) * h.max(0)) as f64;
    let (b, s) = (bounds, screen);
    let full = area(i64::from(b.width), i64::from(b.height));
    if full <= 0.0 {
        return 0.0;
    }
    let end = |o: i32, len: i32| i64::from(o) + i64::from(len);
    let w = end(b.x, b.width).min(end(s.x, s.width)) - i64::from(b.x.max(s.x));
    let h = end(b.y, b.height).min(end(s.y, s.height)) - i64::from(b.y.max(s.y));
    (area(w, h) / full).min(1.0)
}

/// Visible for an assertion, the one rule every tree driver applies (D-1):
/// rendered, a non-empty box, intersecting `screen` by any pixel — or by
/// `visibility.minArea` of its box when that is set. A covered element
/// still counts (it is present; actions check the hit target). The
/// platforms' trees already clip bounds to scrolling containers; the web
/// driver checks overflow ancestors in the page.
pub fn shown_on(element: &Element, screen: &Rect) -> bool {
    shown_with(element, screen, min_visible_area())
}

/// [`shown_on`] with an explicit `min_area`.
pub fn shown_with(element: &Element, screen: &Rect, min_area: f64) -> bool {
    if !element.visible || element.bounds.is_empty() || !element.bounds.intersects(screen) {
        return false;
    }
    min_area <= 0.0 || visible_fraction(&element.bounds, screen) >= min_area
}

/// Same class by full or short name: the Android parser keeps `EditText`
/// from `android.widget.EditText` and the iOS one `Button` from
/// `XCUIElementTypeButton`, while users write either form.
fn class_matches(element_type: &str, cls: &str) -> bool {
    fn short(c: &str) -> &str {
        let c = c.rsplit('.').next().unwrap_or(c);
        c.strip_prefix("XCUIElementType").unwrap_or(c)
    }
    !cls.is_empty() && (element_type == cls || short(element_type) == short(cls))
}

/// Whole id, or the short form after the package's `:id/` (Android) or a
/// `/` — the latter not when the id is just the element's label (iOS uses
/// the label when there is no identifier, and `General/Settings` isn't a
/// resource id: VEL-237).
fn id_matches(element: &Element, id: &str) -> bool {
    let platform_id = element.platform_id.as_str();
    if id.is_empty() || platform_id.is_empty() {
        return false;
    }
    let label_derived = element.label.as_deref() == Some(platform_id);
    platform_id == id
        || platform_id.strip_suffix(id).is_some_and(|prefix| {
            prefix.ends_with(":id/") || (prefix.ends_with('/') && !label_derived)
        })
}

/// An id the build or the driver made up rather than one the app's authors
/// chose: 4+ digits in a row, 8+ hex characters with a digit among them, a
/// UUID, or a driver's pre-order `node_N`. A selector naming one breaks on
/// the next build or the next view added (R6/R7).
pub fn is_volatile_id(id: &str) -> bool {
    let short = id.rsplit_once(":id/").map_or(id, |(_, s)| s);
    let tail = short.rsplit('/').next().unwrap_or(short);
    if let Some(n) = tail.strip_prefix("node_") {
        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    let has_digit = short.chars().any(|c| c.is_ascii_digit());
    let (mut digits, mut hex) = (0, 0);
    for c in short.chars() {
        digits = if c.is_ascii_digit() { digits + 1 } else { 0 };
        hex = if c.is_ascii_hexdigit() { hex + 1 } else { 0 };
        if digits >= 4 || (hex >= 8 && has_digit) {
            return true;
        }
    }
    false
}

/// Which of several matches a bare selector acts on, for messages. Every
/// driver that resolves over a tree should pick with [`resolve`], so this
/// stays true everywhere.
pub const BARE_PICK_RULE: &str = "the first in tree order";

/// The element an action on `selector` acts on: the one entry point every
/// driver's tree lookup should use, so the same selector picks the same
/// element on every platform (a bare selector: [`BARE_PICK_RULE`]; `index:`
/// in reading order; a relation nearest first). Same as [`find_ref`].
pub fn resolve<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
) -> Option<&'a Element> {
    find_ref(root, selector, screen)
}

/// The element `selector` resolves to, by reference.
///
/// A bare selector that matches several elements resolves to the first in
/// tree order, as it always has (and as Maestro does): every existing suite
/// that taps an ambiguous `text:` keeps tapping the same element. Only
/// `index:` counts in reading order, and a relation ranks nearest first
/// (see [`find_all_ref`]).
pub fn find_ref<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
) -> Option<&'a Element> {
    find_ref_upto(root, selector, screen, ceiling())
}

/// [`find_ref`] going no looser than `max`.
pub fn find_ref_upto<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Option<&'a Element> {
    match selector {
        Selector::Relative { .. } | Selector::Index { .. } => {
            find_all_tiered(root, selector, screen, max)
                .into_iter()
                .next()
        }
        _ => tiers_upto(selector, max).iter().find_map(|case| {
            innermost_matches(root, selector, screen, *case)
                .into_iter()
                .next()
        }),
    }
}

/// Whether `selector` has a `text:` part anywhere (the case-insensitive
/// pass can only find something new then).
fn has_text(selector: &Selector) -> bool {
    match selector {
        Selector::Text(_) | Selector::Name(_) => true,
        Selector::Compound(parts) => parts.iter().any(has_text),
        Selector::Index { selector, .. } => has_text(selector),
        Selector::Relative { target, anchor, .. } => has_text(target) || has_text(anchor),
        _ => false,
    }
}

/// Every element `selector` matches, innermost per nested chain, in on-screen
/// order (top to bottom, then left to right). An `index:` selector yields its
/// one element (or none), so it also works as a relation anchor.
pub fn find_all_ref<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
) -> Vec<&'a Element> {
    find_all_tiered(root, selector, screen, ceiling())
}

/// Every element `selector` (a plain one: no index or relation) matches,
/// innermost per nested chain, in tree order: the order a bare selector
/// picks from ([`BARE_PICK_RULE`]).
pub fn find_all_tree_order<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
) -> Vec<&'a Element> {
    let case = tier_for(root, selector, screen, ceiling());
    innermost_matches(root, selector, screen, case)
}

/// [`find_all_ref`] going no looser than `max`.
pub fn find_all_ref_upto<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Vec<&'a Element> {
    find_all_tiered(root, selector, screen, max)
}

/// The first tier (up to `max`) in which the plain selector `selector`
/// finds anything.
fn tier_for(root: &Element, selector: &Selector, screen: Option<&Rect>, max: Tier) -> Tier {
    tiers_upto(selector, max)
        .iter()
        .copied()
        .find(|case| !innermost_matches(root, selector, screen, *case).is_empty())
        .unwrap_or(Tier::Exact)
}

/// Matches with each part tiered on its own (review M1): an `index:` picks
/// its tier from the inner selector alone, so `{text: Add, index: 2}` with
/// two exact "Add" rows finds nothing rather than counting a folded "ADD"
/// as the third; a relation tiers its anchor and its target separately. A
/// compound is one element test, so it is tiered as a whole.
fn find_all_tiered<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Vec<&'a Element> {
    match selector {
        Selector::Relative {
            target,
            relation,
            anchor,
        } => relative_matches(root, target, *relation, anchor, screen, max),
        Selector::Index {
            selector: inner,
            index,
        } => find_all_tiered(root, inner, screen, max)
            .into_iter()
            .nth(*index)
            .into_iter()
            .collect(),
        _ => {
            let case = tier_for(root, selector, screen, max);
            find_all_in(root, selector, screen, case)
        }
    }
}

/// Every match of the plain selector `selector` in one tier, in reading
/// order.
fn find_all_in<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    case: Tier,
) -> Vec<&'a Element> {
    reading_order(innermost_matches(root, selector, screen, case))
}

/// `all` in reading order (see [`find_all_ref`]).
pub fn in_reading_order(all: Vec<&Element>) -> Vec<&Element> {
    reading_order(all)
}

/// Sort into reading order: rows top to bottom, each row left to right.
/// Elements share a row when their vertical centre lies within the row's
/// first element (or its centre within theirs), so a row of vertically
/// centred buttons of different heights, or grid cards whose tops differ by
/// a pixel or two, still count left to right. Sorting by the exact top edge
/// made `index: 0` the right-hand one of such a row.
fn reading_order(mut all: Vec<&Element>) -> Vec<&Element> {
    // Stable: equal positions keep tree order.
    all.sort_by_key(|e| (e.bounds.y, e.bounds.x));
    let same_row = |a: &Rect, b: &Rect| {
        let within = |r: &Rect, cy: i32| {
            cy >= r.y && i64::from(cy) < i64::from(r.y) + i64::from(r.height.max(1))
        };
        within(a, b.center().1) || within(b, a.center().1)
    };
    let mut rows: Vec<Vec<&Element>> = Vec::new();
    for e in all {
        match rows.last_mut() {
            Some(row) if same_row(&row[0].bounds, &e.bounds) => row.push(e),
            _ => rows.push(vec![e]),
        }
    }
    rows.into_iter()
        .flat_map(|mut row| {
            row.sort_by_key(|e| e.bounds.x);
            row
        })
        .collect()
}

/// A selector [`matches_selector`] can test element by element.
fn is_plain(selector: &Selector) -> bool {
    match selector {
        Selector::Relative { .. } | Selector::Index { .. } => false,
        Selector::Compound(parts) => parts.iter().all(is_plain),
        _ => true,
    }
}

/// Pixels of overlap still counted as "below"/"right of" (borders touch).
const EDGE_SLACK: i32 = 2;

/// Targets that stand in `relation` to some anchor, nearest first: the
/// gap along the relation's axis, then the offset across it (Playwright's
/// layout selectors score the same way). The anchor and the target are
/// each tiered on their own (up to `max`).
fn relative_matches<'a>(
    root: &'a Element,
    target: &Selector,
    relation: Relation,
    anchor: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Vec<&'a Element> {
    match relation {
        Relation::ContainsChild | Relation::ChildOf => {
            containment_matches(root, target, relation, anchor, screen, max)
        }
        _ => {
            let mut scored = layout_scored(root, target, relation, anchor, screen, max);
            scored.sort_by_key(|(s, _)| *s);
            scored.into_iter().map(|(_, c)| c).collect()
        }
    }
}

/// `childOf` / `containsChild` matches.
fn containment_matches<'a>(
    root: &'a Element,
    target: &Selector,
    relation: Relation,
    anchor: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Vec<&'a Element> {
    let anchors = find_all_tiered(root, anchor, screen, max);
    if anchors.is_empty() {
        return Vec::new();
    }
    let plain = is_plain(target);
    if relation == Relation::ContainsChild {
        let case = if plain {
            tier_for(root, target, screen, max)
        } else {
            Tier::Exact
        };
        // A container: allow outer matches too (innermost would drop the
        // wrapper whose child is the anchor), then prefer the tightest.
        // A target that is itself a relation or an index (`{className:
        // Cell, below: …, containsChild: …}` nests that way) has no
        // per-element test: take its matches (VEL-078).
        let mut found: Vec<&Element> = Vec::new();
        if plain {
            collect_all(root, target, screen, case, &mut found);
        } else {
            found = find_all_tiered(root, target, screen, max);
        }
        found.retain(|c| {
            anchors
                .iter()
                .any(|a| !std::ptr::eq(*c, *a) && contains(c, a))
        });
        found.sort_by_key(|c| area(&c.bounds));
        return found;
    }
    if !plain {
        // Resolve the target on the whole screen (its own anchor may lie
        // outside the container), then keep what is inside one.
        return find_all_tiered(root, target, screen, max)
            .into_iter()
            .filter(|c| anchors.iter().any(|a| contains(a, c)))
            .collect();
    }
    // The target's tier is decided inside the anchors, as it was before
    // tiering per part: an exact "Delete" row behind a dialog must not keep
    // `{text: Delete, childOf: buttonPanel}` from the panel's "DELETE"
    // (review-s8 MAJOR-1). One tier over all the anchors: per anchor, the
    // first cell's merged/folded match came before the second's exact one
    // (review-f3-f4-s10 MINOR 6).
    for &case in tiers_upto(target, max) {
        let mut found: Vec<&Element> = Vec::new();
        for a in &anchors {
            for c in find_all_in(a, target, screen, case) {
                if !std::ptr::eq(c, *a) && !found.iter().any(|f| std::ptr::eq(*f, c)) {
                    found.push(c);
                }
            }
        }
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// Each target on the relation's side of some anchor, with its score.
fn layout_scored<'a>(
    root: &'a Element,
    target: &Selector,
    relation: Relation,
    anchor: &Selector,
    screen: Option<&Rect>,
    max: Tier,
) -> Vec<((i64, i64), &'a Element)> {
    let anchors = find_all_tiered(root, anchor, screen, max);
    if anchors.is_empty() {
        return Vec::new();
    }
    find_all_tiered(root, target, screen, max)
        .into_iter()
        .filter(|c| !anchors.iter().any(|a| std::ptr::eq(*c, *a)))
        .filter_map(|c| {
            anchors
                .iter()
                .filter_map(|a| layout_score(relation, &c.bounds, &a.bounds))
                .min()
                .map(|s| (s, c))
        })
        .collect()
}

/// Whether `el` (a copy of a node of `root`, found by [`locate`] at the same
/// bounds) stands in `relation` to some match of `anchor`. Self-healing uses
/// it to keep a relative selector's constraint when it heals the target.
pub fn stands_in(
    root: &Element,
    el: &Element,
    relation: Relation,
    anchor: &Selector,
    screen: Option<&Rect>,
) -> bool {
    let Some(node) = locate(root, el).filter(|n| n.bounds == el.bounds) else {
        return false;
    };
    let anchors = find_all_ref(root, anchor, screen);
    anchors.iter().any(|a| {
        !std::ptr::eq(*a, node)
            && match relation {
                Relation::ChildOf => contains(a, node),
                Relation::ContainsChild => contains(node, a),
                _ => layout_score(relation, &node.bounds, &a.bounds).is_some(),
            }
    })
}

/// (gap along the relation, offset across it), or None if `c` isn't on
/// that side of `a`.
fn layout_score(relation: Relation, c: &Rect, a: &Rect) -> Option<(i64, i64)> {
    // In i64: hostile bounds overflowed i32 (VEL-231).
    let w = i64::from;
    let (ccx, ccy) = c.center();
    let (acx, acy) = a.center();
    let (gap, across) = match relation {
        Relation::Below => (w(c.y) - (w(a.y) + w(a.height)), w(ccx) - w(acx)),
        Relation::Above => (w(a.y) - (w(c.y) + w(c.height)), w(ccx) - w(acx)),
        Relation::RightOf => (w(c.x) - (w(a.x) + w(a.width)), w(ccy) - w(acy)),
        Relation::LeftOf => (w(a.x) - (w(c.x) + w(c.width)), w(ccy) - w(acy)),
        Relation::ContainsChild | Relation::ChildOf => return None,
    };
    (gap >= -i64::from(EDGE_SLACK)).then_some((gap.max(0), across.abs()))
}

fn area(r: &Rect) -> i64 {
    i64::from(r.width) * i64::from(r.height)
}

/// Every match of `selector`, outer ones included, in tree order.
fn collect_all<'a>(
    el: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    case: Tier,
    out: &mut Vec<&'a Element>,
) {
    fn walk<'a>(
        el: &'a Element,
        selector: &Selector,
        want: &Want,
        screen: Option<&Rect>,
        case: Tier,
        out: &mut Vec<&'a Element>,
    ) {
        if matches_in(el, selector, want, screen, case) {
            out.push(el);
        }
        for c in &el.children {
            walk(c, selector, want, screen, case, out);
        }
    }
    walk(el, selector, &Want::of(selector), screen, case, out);
}

/// Whether `node` is a strict descendant of `container`.
fn contains(container: &Element, node: &Element) -> bool {
    container
        .children
        .iter()
        .any(|c| std::ptr::eq(c, node) || contains(c, node))
}

/// How many distinct elements `selector` matches — more than one means a
/// bare selector picked one of several (worth a warning).
pub fn match_count(root: &Element, selector: &Selector, screen: Option<&Rect>) -> usize {
    find_all_ref(root, selector, screen).len()
}

/// The on-screen match count of each device's last successful lookup, for
/// a driver that counts as it resolves
/// ([`crate::PlatformDriver::last_match_count`]).
#[derive(Debug, Default)]
pub struct MatchCounts(std::sync::Mutex<std::collections::HashMap<String, (String, usize)>>);

impl MatchCounts {
    /// `selector` just resolved on `device_id`, with `n` on-screen matches.
    pub fn record(&self, device_id: &str, selector: &Selector, n: usize) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device_id.to_string(), (format!("{selector:?}"), n));
    }

    /// `device_id`'s lookup missed (or counted nothing): forget the last
    /// count, so a later check of the same selector never reads a count
    /// from an earlier screen (review-fixes #6).
    pub fn clear(&self, device_id: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device_id);
    }

    /// The count of `device_id`'s last lookup, if it was of `selector`.
    pub fn get(&self, device_id: &str, selector: &Selector) -> Option<usize> {
        let key = format!("{selector:?}");
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .filter(|(s, _)| *s == key)
            .map(|(_, n)| *n)
    }
}

/// How many elements an action on `selector` could equally mean. Like
/// [`match_count`], except a layout relation (below/above/leftOf/rightOf)
/// ranks its matches nearest first, so only those tied with the nearest
/// count: `tap {text: Buy, rightOf: {text: "Product C"}}` warned "matched 3
/// elements; used the first" though it rightly took the nearest.
pub fn ambiguity(root: &Element, selector: &Selector, screen: Option<&Rect>) -> usize {
    let Selector::Relative {
        target,
        relation,
        anchor,
    } = selector
    else {
        return match_count(root, selector, screen);
    };
    if matches!(relation, Relation::ContainsChild | Relation::ChildOf) {
        return match_count(root, selector, screen);
    }
    let scores: Vec<(i64, i64)> = layout_scored(root, target, *relation, anchor, screen, ceiling())
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    match scores.iter().min() {
        Some(best) => scores.iter().filter(|s| *s == best).count(),
        None => 0,
    }
}

/// For each of the first `max` matches of `selector` (reading order), a
/// selector that picks just that one (D-6 / R6, Playwright lists every
/// match as a ready-made locator): `childOf` the nearest ancestor a unique,
/// stable selector names (an id the app set, a label, a text), else an
/// explicit `index:`.
pub fn suggest_picks(
    root: &Element,
    selector: &Selector,
    screen: Option<&Rect>,
    max: usize,
) -> Vec<Selector> {
    find_all_ref(root, selector, screen)
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, m)| {
            by_container(root, selector, m, screen).unwrap_or_else(|| Selector::Index {
                selector: Box::new(selector.clone()),
                index: i,
            })
        })
        .collect()
}

/// `{selector, childOf: <anchor>}` resolving to `target` alone, with the
/// nearest ancestor of `target` that one stable selector names uniquely.
fn by_container(
    root: &Element,
    selector: &Selector,
    target: &Element,
    screen: Option<&Rect>,
) -> Option<Selector> {
    fn path<'a>(el: &'a Element, target: &Element, out: &mut Vec<&'a Element>) -> bool {
        if std::ptr::eq(el, target) {
            return true;
        }
        out.push(el);
        if el.children.iter().any(|c| path(c, target, out)) {
            return true;
        }
        out.pop();
        false
    }
    let mut ancestors = Vec::new();
    if !path(root, target, &mut ancestors) {
        return None;
    }
    let short = |v: &str| v.rsplit_once(":id/").map_or(v, |(_, s)| s).to_string();
    let some = |v: &Option<String>| {
        v.as_deref()
            .filter(|v| !v.trim().is_empty() && v.chars().count() <= 60)
            .map(str::to_string)
    };
    for anc in ancestors.iter().skip(1).rev() {
        let id = anc.platform_id.as_str();
        let mut names = Vec::new();
        if !id.is_empty()
            && anc.label.as_deref() != Some(id)
            && !is_volatile_id(id)
            && !crate::selector_build::synthetic_id(id)
        {
            names.push(Selector::Id(short(id)));
        }
        if let Some(label) = some(&anc.label) {
            names.push(Selector::AccessibilityId(label));
        }
        if let Some(text) = some(&anc.text) {
            names.push(Selector::Text(text));
        }
        for anchor in names {
            if match_count(root, &anchor, screen) != 1 {
                continue;
            }
            let pick = Selector::Relative {
                target: Box::new(selector.clone()),
                relation: Relation::ChildOf,
                anchor: Box::new(anchor),
            };
            let found = find_all_ref(root, &pick, screen);
            if found.len() == 1 && std::ptr::eq(found[0], target) {
                return Some(pick);
            }
        }
    }
    None
}

/// Matches in tree order, dropping any match that has a matching descendant.
///
/// Under [`Tier::Fold`], `text:` ignores case: Android draws Material buttons
/// and dialog actions in capitals (`textAllCaps`) and reports them so
/// ("DELETE" for a button labelled "Delete"), and an exact-only match waited
/// out the whole element timeout before self-healing tapped it (VelocityLab,
/// 10 s per dialog button). [`find_ref`] and [`find_all_ref`] try that pass
/// only when the exact one finds nothing, so an exact match always wins.
fn innermost_matches<'a>(
    root: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
    case: Tier,
) -> Vec<&'a Element> {
    let want = Want::of(selector);
    innermost_matches_by(root, &|el| matches_in(el, selector, &want, screen, case))
}

fn innermost_matches_by<'a>(
    root: &'a Element,
    pred: &dyn Fn(&Element) -> bool,
) -> Vec<&'a Element> {
    fn walk<'a>(
        el: &'a Element,
        pred: &dyn Fn(&Element) -> bool,
        out: &mut Vec<&'a Element>,
    ) -> bool {
        let before = out.len();
        let mut child_matched = false;
        for c in &el.children {
            child_matched |= walk(c, pred, out);
        }
        let me = pred(el);
        if me && !child_matched {
            // Keep tree (pre-)order: an element precedes its descendants.
            out.insert(before, el);
        }
        me || child_matched
    }
    let mut out = Vec::new();
    walk(root, pred, &mut out);
    out
}

/// Find the element `selector` resolves to in the tree rooted at `root`.
///
/// When `screen` is `Some`, visibility/bounds filtering is applied.
pub fn find_in_tree(root: &Element, selector: &Selector, screen: Option<&Rect>) -> Option<Element> {
    find_ref(root, selector, screen).cloned()
}

/// Collect all elements `selector` matches (see [`find_all_ref`]).
///
/// When `screen` is `Some`, visibility/bounds filtering is applied.
pub fn find_all_in_tree(
    root: &Element,
    selector: &Selector,
    screen: Option<&Rect>,
    results: &mut Vec<Element>,
) {
    results.extend(find_all_ref(root, selector, screen).into_iter().cloned());
}

/// Find `target` again in a fresh tree. Re-finding by resource-id alone was
/// wrong twice over: an element without one matched the root (`contains("")`),
/// and list rows sharing one always resolved to the first row. Identity is
/// (resource-id, class, bounds); if the element moved, fall back to the
/// nearest node with the same resource-id and class, or — when there is no
/// resource-id — the same class and label.
pub fn locate<'a>(tree: &'a Element, target: &Element) -> Option<&'a Element> {
    fn walk<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
        out.push(el);
        for c in &el.children {
            walk(c, out);
        }
    }
    let mut all = Vec::new();
    walk(tree, &mut all);
    let same_kind = |e: &&Element| {
        e.element_type == target.element_type
            && e.platform_id == target.platform_id
            && (!target.platform_id.is_empty() || e.label == target.label)
    };
    if let Some(exact) = all
        .iter()
        .find(|e| same_kind(e) && e.bounds == target.bounds)
    {
        return Some(exact);
    }
    let (tx, ty) = target.bounds.center();
    all.into_iter().filter(same_kind).min_by_key(|e| {
        let (x, y) = e.bounds.center();
        (i64::from(x) - i64::from(tx)).abs() + (i64::from(y) - i64::from(ty)).abs()
    })
}

#[cfg(test)]
mod tests {

    /// Review MINOR 9: a pattern is compiled once and shared, never cloned
    /// per element.
    #[test]
    fn a_pattern_is_compiled_once_and_shared() {
        let a = compiled_pattern("fd12 row [0-9]+");
        let b = compiled_pattern("fd12 row [0-9]+");
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        assert!(pattern_matches("fd12 row [0-9]+", "FD12 Row 42"));
        assert!(!pattern_matches("fd12 row [0-9]+", "row x"));
        assert!(compiled_pattern("(").re.is_none());
        assert!(
            pattern_matches("(", "("),
            "an invalid pattern matches literally"
        );
    }
    use super::*;

    fn el(id: &str, text: Option<&str>, x: i32, y: i32, children: Vec<Element>) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: text.map(String::from),
            element_type: "View".into(),
            bounds: Rect {
                x,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children,
            ..Default::default()
        }
    }

    #[test]
    fn id_matches_whole_or_short_form_only() {
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                el("com.app:id/btn_delete", None, 0, 0, vec![]),
                el("com.app:id/btn", None, 0, 50, vec![]),
            ],
        );
        let found = find_in_tree(&root, &Selector::Id("btn".into()), None).unwrap();
        assert_eq!(
            found.platform_id, "com.app:id/btn",
            "no substring match on btn_delete"
        );
        assert!(find_in_tree(&root, &Selector::Id("com.app:id/btn".into()), None).is_some());
        assert!(find_in_tree(&root, &Selector::Id("delete".into()), None).is_none());
        assert!(find_in_tree(&root, &Selector::Id("".into()), None).is_none());
    }

    #[test]
    fn text_falls_back_to_ignoring_case_only_without_an_exact_match() {
        // Android reports a Material dialog's "Delete" button as "DELETE".
        let dialog = el(
            "",
            None,
            0,
            0,
            vec![
                el("android:id/button2", Some("CANCEL"), 0, 0, vec![]),
                el("android:id/button1", Some("DELETE"), 0, 50, vec![]),
            ],
        );
        let found = find_in_tree(&dialog, &Selector::Text("Delete".into()), None).unwrap();
        assert_eq!(found.platform_id, "android:id/button1");
        assert_eq!(
            match_count(&dialog, &Selector::Text("cancel".into()), None),
            1
        );
        // An exact match wins over a case-only one.
        let both = el(
            "",
            None,
            0,
            0,
            vec![
                el("upper", Some("SAVE"), 0, 0, vec![]),
                el("exact", Some("Save"), 0, 50, vec![]),
            ],
        );
        assert_eq!(
            find_in_tree(&both, &Selector::Text("Save".into()), None)
                .unwrap()
                .platform_id,
            "exact"
        );
        assert_eq!(match_count(&both, &Selector::Text("Save".into()), None), 1);
        // Nothing matches a different word; textContains ignores case
        // anyway (tc-case).
        assert!(find_in_tree(&dialog, &Selector::Text("Remove".into()), None).is_none());
        assert!(find_in_tree(&dialog, &Selector::TextContains("delete".into()), None).is_some());
    }

    #[test]
    fn resolve_takes_the_tree_first_of_twins_not_the_top_one() {
        // R1 / N-1: iOS took the first in reading order (the top "Go"),
        // Android the first in tree order (the bottom one).
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                el("bottom", Some("Go"), 0, 500, vec![]),
                el("top", Some("Go"), 0, 100, vec![]),
            ],
        );
        let go = Selector::Text("Go".into());
        assert_eq!(resolve(&root, &go, None).unwrap().platform_id, "bottom");
        assert_eq!(find_all_ref(&root, &go, None)[0].platform_id, "top");
        assert_eq!(
            resolve(&root, &idx(go, 0), None).unwrap().platform_id,
            "top",
            "index: counts in reading order"
        );
    }

    #[test]
    fn hostile_bounds_never_overflow() {
        // VEL-231: bounds near i32::MAX panicked (debug) or wrapped.
        let mut far = el("far", Some("Go"), i32::MAX - 5, i32::MAX - 5, vec![]);
        far.bounds.width = i32::MAX;
        far.bounds.height = i32::MAX;
        let mut neg = el("neg", Some("Go"), i32::MIN, i32::MIN, vec![]);
        neg.bounds.width = 10;
        let root = el(
            "",
            None,
            0,
            0,
            vec![el("a", Some("Anchor"), 0, 0, vec![]), far, neg],
        );
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        assert!(!root.children[1].bounds.intersects(&screen));
        let _ = root.children[1].bounds.center();
        for relation in [
            Relation::Below,
            Relation::Above,
            Relation::LeftOf,
            Relation::RightOf,
        ] {
            let sel = rel(
                Selector::Text("Go".into()),
                relation,
                Selector::Text("Anchor".into()),
            );
            let _ = find_all_ref(&root, &sel, None);
            let _ = ambiguity(&root, &sel, None);
        }
        let _ = find_all_ref(&root, &Selector::Text("Go".into()), None);
        let _ = locate(&root, &root.children[1]);
    }

    #[test]
    fn case_insensitive_fallback_reaches_compounds_and_relations() {
        // VEL-234: only a bare `text:` fell back.
        let mut delete = el("d", Some("DELETE"), 0, 100, vec![]);
        delete.element_type = "Button".into();
        let root = el(
            "",
            None,
            0,
            0,
            vec![el("t", Some("Title"), 0, 0, vec![]), delete],
        );
        let compound = Selector::Compound(vec![
            Selector::Text("Delete".into()),
            Selector::ClassName("Button".into()),
        ]);
        assert_eq!(
            find_in_tree(&root, &compound, None).unwrap().platform_id,
            "d"
        );
        let below = rel(
            Selector::Text("Delete".into()),
            Relation::Below,
            Selector::Text("title".into()),
        );
        assert_eq!(find_in_tree(&root, &below, None).unwrap().platform_id, "d");
        assert_eq!(match_count(&root, &below, None), 1);
        assert_eq!(ambiguity(&root, &below, None), 1);
        // An exact match anywhere still wins over a folded one.
        let both = el(
            "",
            None,
            0,
            0,
            vec![
                el("upper", Some("OK"), 0, 0, vec![]),
                el("exact", Some("Ok"), 0, 50, vec![]),
            ],
        );
        let sel = Selector::Compound(vec![Selector::Text("Ok".into())]);
        assert_eq!(find_all_ref(&both, &sel, None).len(), 1);
        assert_eq!(
            find_in_tree(&both, &sel, None).unwrap().platform_id,
            "exact"
        );
    }

    #[test]
    fn empty_values_match_nothing() {
        // VEL-236: MCP builds selectors without the validator; an empty
        // textContains matched every element (the root first).
        let mut b = el("com.app:id/b", Some("Buy"), 0, 0, vec![]);
        b.label = Some("Buy".into());
        let root = el("", None, 0, 0, vec![b]);
        for sel in [
            Selector::Text(String::new()),
            Selector::TextContains(String::new()),
            Selector::AccessibilityId(String::new()),
            Selector::Id(String::new()),
            Selector::ClassName(String::new()),
            Selector::Compound(vec![Selector::TextContains(String::new())]),
        ] {
            assert!(find_in_tree(&root, &sel, None).is_none(), "{sel}");
            assert_eq!(match_count(&root, &sel, None), 0, "{sel}");
        }
    }

    #[test]
    fn id_short_form_skips_label_derived_ids() {
        // VEL-237: iOS uses the label as the id when there's no identifier.
        let mut label_id = el("General/Settings", None, 0, 0, vec![]);
        label_id.label = Some("General/Settings".into());
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                label_id,
                el("headless:id/save", None, 0, 50, vec![]),
                el("screen/login", None, 0, 100, vec![]),
            ],
        );
        assert!(find_in_tree(&root, &Selector::Id("Settings".into()), None).is_none());
        assert!(find_in_tree(&root, &Selector::Id("General/Settings".into()), None).is_some());
        assert!(find_in_tree(&root, &Selector::Id("save".into()), None).is_some());
        assert!(find_in_tree(&root, &Selector::Id("login".into()), None).is_some());
    }

    #[test]
    fn text_matches_one_part_of_a_merged_label_last() {
        // React Native on iOS: a touchable's label joins its children's.
        let mut rn = el("", None, 0, 0, vec![]);
        rn.label = Some("Points, 120, View rewards".into());
        // Flutter merges semantics with newlines.
        let fl = el("", Some("Total\n$12.00"), 0, 50, vec![]);
        let root = el("", None, 0, 0, vec![rn, fl]);
        let find = |t: &str| find_in_tree(&root, &Selector::Text(t.into()), None);
        assert_eq!(find("View rewards").unwrap().bounds.y, 0);
        assert_eq!(find("total $12.00").unwrap().bounds.y, 50);
        assert_eq!(find("$12.00").unwrap().bounds.y, 50);
        assert!(find("Points, 1").is_none());
        // An exact (or case-folded) match elsewhere wins over a part.
        let mut root2 = root.clone();
        root2
            .children
            .push(el("", Some("view rewards"), 0, 100, vec![]));
        assert_eq!(
            find_in_tree(&root2, &Selector::Text("View rewards".into()), None)
                .unwrap()
                .bounds
                .y,
            100
        );
        assert_eq!(
            match_count(&root2, &Selector::Text("View rewards".into()), None),
            1
        );
    }

    #[test]
    fn innermost_match_wins() {
        let label = el("label", Some("Buy"), 10, 10, vec![]);
        let wrapper = el("row", Some("Buy"), 0, 0, vec![label]);
        let root = el("", None, 0, 0, vec![wrapper]);
        let found = find_in_tree(&root, &Selector::Text("Buy".into()), None).unwrap();
        assert_eq!(found.platform_id, "label");
        assert_eq!(match_count(&root, &Selector::Text("Buy".into()), None), 1);
    }

    #[test]
    fn index_counts_by_screen_position() {
        // Tree order is bottom row first; index 0 must be the top one.
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                el("b", Some("Add"), 0, 300, vec![]),
                el("a", Some("Add"), 0, 100, vec![]),
            ],
        );
        let sel = |i| Selector::Index {
            selector: Box::new(Selector::Text("Add".into())),
            index: i,
        };
        assert_eq!(find_in_tree(&root, &sel(0), None).unwrap().platform_id, "a");
        assert_eq!(find_in_tree(&root, &sel(1), None).unwrap().platform_id, "b");
        assert_eq!(match_count(&root, &Selector::Text("Add".into()), None), 2);
    }

    fn node(id: &str, text: &str, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: Some(text.into()),
            element_type: "TextView".into(),
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
        }
    }

    #[test]
    fn rows_sharing_an_id_resolve_to_the_right_row() {
        let tree = Element {
            children: vec![
                node("app:id/price", "$1", 0),
                node("app:id/price", "$2", 50),
            ],
            ..node("", "", 0)
        };
        let second = node("app:id/price", "$2", 50);
        assert_eq!(locate(&tree, &second).unwrap().text.as_deref(), Some("$2"));
    }

    #[test]
    fn elements_without_an_id_never_resolve_to_the_root() {
        let mut root = node("", "", 0);
        root.element_type = "hierarchy".into();
        root.children = vec![node("", "Hello", 100)];
        let target = node("", "Hello", 100);
        let found = locate(&root, &target).unwrap();
        assert_eq!(found.element_type, "TextView");
        let gone = Element {
            element_type: "Button".into(),
            ..target
        };
        assert!(locate(&root, &gone).is_none());
    }

    #[test]
    fn class_names_match_in_full_or_short_form() {
        assert!(class_matches("EditText", "android.widget.EditText"));
        assert!(class_matches("EditText", "EditText"));
        assert!(class_matches("Button", "XCUIElementTypeButton"));
        assert!(!class_matches("EditText", "android.widget.TextView"));
        assert!(!class_matches("EditText", ""));
    }

    fn at(id: &str, text: &str, x: i32, y: i32) -> Element {
        let mut e = el(id, Some(text), x, y, vec![]);
        e.bounds.width = 80;
        e.bounds.height = 30;
        e
    }

    fn rel(target: Selector, relation: Relation, anchor: Selector) -> Selector {
        Selector::Relative {
            target: Box::new(target),
            relation,
            anchor: Box::new(anchor),
        }
    }

    #[test]
    fn a_layout_relation_is_ambiguous_only_on_a_tie_for_nearest() {
        let el = |id: &str, text: &str, x: i32, y: i32| Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: Some(text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let root = Element {
            children: vec![
                el("pa", "Product A", 0, 0),
                el("ba", "Buy", 200, 0),
                el("pc", "Product C", 0, 200),
                el("bc", "Buy", 200, 200),
                el("bb", "Buy", 200, 100),
            ],
            ..el("root", "", 0, 0)
        };
        let right_of = |anchor: &str| Selector::Relative {
            target: Box::new(Selector::Text("Buy".into())),
            relation: Relation::RightOf,
            anchor: Box::new(Selector::Text(anchor.into())),
        };
        assert_eq!(match_count(&root, &right_of("Product C"), None), 3);
        assert_eq!(ambiguity(&root, &right_of("Product C"), None), 1);
        assert_eq!(ambiguity(&root, &Selector::Text("Buy".into()), None), 3);
    }

    #[test]
    fn below_picks_the_nearest_match_under_the_anchor() {
        // Two "Edit" buttons; we want the one under "Shipping".
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                at("h1", "Billing", 0, 0),
                at("e1", "Edit", 0, 40),
                at("h2", "Shipping", 0, 200),
                at("e2", "Edit", 0, 240),
            ],
        );
        let sel = rel(
            Selector::Text("Edit".into()),
            Relation::Below,
            Selector::Text("Shipping".into()),
        );
        assert_eq!(find_in_tree(&root, &sel, None).unwrap().platform_id, "e2");
        let above = rel(
            Selector::Text("Edit".into()),
            Relation::Above,
            Selector::Text("Shipping".into()),
        );
        assert_eq!(find_in_tree(&root, &above, None).unwrap().platform_id, "e1");
    }

    #[test]
    fn right_of_ranks_by_gap_then_alignment() {
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                at("label", "Qty", 0, 100),
                at("far", "+", 400, 100),
                at("near", "+", 120, 100),
                at("left", "+", 0, 300),
            ],
        );
        let sel = rel(
            Selector::Text("+".into()),
            Relation::RightOf,
            Selector::Text("Qty".into()),
        );
        assert_eq!(find_in_tree(&root, &sel, None).unwrap().platform_id, "near");
        assert_eq!(
            match_count(&root, &sel, None),
            2,
            "the one below isn't right of it"
        );
    }

    #[test]
    fn contains_child_and_child_of_scope_to_a_row() {
        let row = |id: &str, title: &str, y: i32| {
            let mut r = el(
                id,
                None,
                0,
                y,
                vec![
                    at(&format!("{id}-t"), title, 0, y),
                    at(&format!("{id}-b"), "Add", 200, y),
                ],
            );
            r.element_type = "Cell".into();
            r.bounds.width = 400;
            r
        };
        let root = el(
            "",
            None,
            0,
            0,
            vec![row("r1", "Tea", 0), row("r2", "Coffee", 100)],
        );
        let cell = rel(
            Selector::ClassName("Cell".into()),
            Relation::ContainsChild,
            Selector::Text("Coffee".into()),
        );
        assert_eq!(find_in_tree(&root, &cell, None).unwrap().platform_id, "r2");
        let add = rel(Selector::Text("Add".into()), Relation::ChildOf, cell);
        assert_eq!(find_in_tree(&root, &add, None).unwrap().platform_id, "r2-b");
    }

    fn idx(inner: Selector, index: usize) -> Selector {
        Selector::Index {
            selector: Box::new(inner),
            index,
        }
    }

    #[test]
    fn an_indexed_anchor_picks_the_nth_section() {
        // VEL-016: `below: {text: Address, index: 1}` matched no anchor at all.
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                at("h1", "Address", 0, 100),
                at("e1", "Edit", 0, 160),
                at("h2", "Address", 0, 600),
                at("e2", "Edit", 0, 660),
            ],
        );
        let second = idx(Selector::Text("Address".into()), 1);
        let all = find_all_ref(&root, &second, None);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].platform_id, "h2");
        assert_eq!(match_count(&root, &second, None), 1);
        assert!(find_all_ref(&root, &idx(Selector::Text("Address".into()), 2), None).is_empty());
        let edit = rel(Selector::Text("Edit".into()), Relation::Below, second);
        assert_eq!(find_in_tree(&root, &edit, None).unwrap().platform_id, "e2");
        let mut found = Vec::new();
        find_all_in_tree(&root, &edit, None, &mut found);
        assert_eq!(found[0].platform_id, "e2");
    }

    #[test]
    fn a_bare_ambiguous_selector_keeps_the_first_in_tree_order() {
        // M-4: reading order is for `index:` only. A bare `text: Go` keeps
        // resolving to the tree's first match, as before the VEL-343 change,
        // even when another match sits higher on screen.
        let lower = at("lower", "Go", 0, 500);
        let upper = at("upper", "Go", 0, 100);
        let root = el("", None, 0, 0, vec![lower, upper]);
        let go = Selector::Text("Go".into());
        assert_eq!(find_in_tree(&root, &go, None).unwrap().platform_id, "lower");
        // `index:` still counts top to bottom.
        assert_eq!(
            find_in_tree(&root, &idx(go.clone(), 0), None)
                .unwrap()
                .platform_id,
            "upper"
        );
        assert_eq!(match_count(&root, &go, None), 2);
    }

    #[test]
    fn index_reads_a_centred_row_left_to_right() {
        // VEL-343: a tall button on the right starts higher than the short
        // one on its left; both sit on one row.
        let mut left = at("left", "Go", 0, 110);
        left.bounds.height = 40;
        let mut right = at("right", "Go", 200, 100);
        right.bounds.height = 60;
        // Grid cards whose tops differ by a pixel.
        let card_a = at("a", "Card", 0, 301);
        let card_b = at("b", "Card", 200, 300);
        let below = at("c", "Card", 0, 400);
        let root = el("", None, 0, 0, vec![right, left, card_b, card_a, below]);
        let go = |i| find_in_tree(&root, &idx(Selector::Text("Go".into()), i), None);
        assert_eq!(go(0).unwrap().platform_id, "left");
        assert_eq!(go(1).unwrap().platform_id, "right");
        let ids: Vec<_> = find_all_ref(&root, &Selector::Text("Card".into()), None)
            .iter()
            .map(|e| e.platform_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn contains_child_combines_with_a_layout_relation() {
        // VEL-078: `{className: Cell, below: {text: Drinks}, containsChild:
        // {text: Coffee}}` never matched: the containsChild target is the
        // `below` relation, which no single element "matches".
        let cell = |id: &str, title: &str, y: i32| {
            let mut c = el(id, None, 0, y, vec![at(&format!("{id}-t"), title, 0, y)]);
            c.element_type = "Cell".into();
            c.bounds.width = 400;
            c
        };
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                cell("food-coffee", "Coffee", 0),
                at("h", "Drinks", 0, 100),
                cell("tea", "Tea", 200),
                cell("drink-coffee", "Coffee", 300),
            ],
        );
        let below_drinks = rel(
            Selector::ClassName("Cell".into()),
            Relation::Below,
            Selector::Text("Drinks".into()),
        );
        let sel = rel(
            below_drinks.clone(),
            Relation::ContainsChild,
            Selector::Text("Coffee".into()),
        );
        assert_eq!(
            find_in_tree(&root, &sel, None).unwrap().platform_id,
            "drink-coffee"
        );
        assert_eq!(match_count(&root, &sel, None), 1);
        // And a nested relation as a childOf target.
        let title = rel(
            rel(
                Selector::Text("Coffee".into()),
                Relation::Below,
                Selector::Text("Drinks".into()),
            ),
            Relation::ChildOf,
            Selector::ClassName("Cell".into()),
        );
        assert_eq!(
            find_in_tree(&root, &title, None).unwrap().platform_id,
            "drink-coffee-t"
        );
    }

    #[test]
    fn an_index_counts_only_its_own_tier() {
        // Review M1: two exact "Add" rows and a folded "ADD" header: index 2
        // took the header (the composite was empty in Exact, so the Fold
        // tier counted all three).
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                el("a0", Some("Add"), 0, 100, vec![]),
                el("a1", Some("Add"), 0, 200, vec![]),
                el("hdr", Some("ADD"), 0, 300, vec![]),
            ],
        );
        let nth = |i| Selector::Index {
            selector: Box::new(Selector::Text("Add".into())),
            index: i,
        };
        assert_eq!(
            find_in_tree(&root, &nth(1), None).unwrap().platform_id,
            "a1"
        );
        assert!(find_in_tree(&root, &nth(2), None).is_none());
        // With no exact match at all, the folded tier still counts.
        let upper = el("", None, 0, 0, vec![el("u", Some("ADD"), 0, 0, vec![])]);
        assert_eq!(
            find_in_tree(&upper, &nth(0), None).unwrap().platform_id,
            "u"
        );
    }

    #[test]
    fn a_relation_tiers_its_target_and_anchor_apart() {
        // Review M1: exact "Buy" and exact "Product C" exist but the Buy
        // below it isn't laid out yet; a folded "BUY" and a merged row
        // label satisfied the relation in the looser tiers.
        let mut row = at("row", "x", 0, 0);
        row.text = None;
        row.label = Some("Product C, $5.00".into());
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                row,
                at("buy-merged", "Buy, $5", 0, 50),
                at("buy", "Buy", 0, 100),
                at("pc", "Product C", 0, 500),
                at("buy-up", "BUY", 0, 600),
            ],
        );
        let sel = rel(
            Selector::Text("Buy".into()),
            Relation::Below,
            Selector::Text("Product C".into()),
        );
        assert!(
            find_in_tree(&root, &sel, None).is_none(),
            "{:?}",
            find_in_tree(&root, &sel, None)
        );
        assert_eq!(ambiguity(&root, &sel, None), 0);
        assert_eq!(match_count(&root, &sel, None), 0);
    }

    #[tokio::test]
    async fn a_child_of_target_is_tiered_inside_its_anchor() {
        // Review-s8 MAJOR-1: an exact "Delete" row stays in the tree behind
        // a confirmation dialog whose textAllCaps button reports "DELETE".
        // Scoping the tap to the dialog must find the button at once.
        let mut panel = at("android:id/buttonPanel", "", 0, 500);
        panel.text = None;
        panel.bounds.height = 100;
        panel.bounds.width = 300;
        panel.children = vec![
            at("android:id/button2", "CANCEL", 0, 510),
            at("android:id/button1", "DELETE", 100, 510),
        ];
        let root = el(
            "",
            None,
            0,
            0,
            vec![at("row-delete", "Delete", 0, 100), panel],
        );
        let sel = rel(
            Selector::Text("Delete".into()),
            Relation::ChildOf,
            Selector::Id("android:id/buttonPanel".into()),
        );
        let polled = with_ceiling(Tier::Fold, async { find_in_tree(&root, &sel, None) }).await;
        assert_eq!(polled.unwrap().platform_id, "android:id/button1");
        assert_eq!(match_count(&root, &sel, None), 1);
        // An exact match inside the anchor still wins over a folded one.
        let mut both = root.clone();
        both.children[1]
            .children
            .push(at("exact", "Delete", 200, 510));
        assert_eq!(
            find_in_tree(&both, &sel, None).unwrap().platform_id,
            "exact"
        );
        // Under an Exact ceiling the folded button stays out.
        assert!(find_ref_upto(&root, &sel, None, Tier::Exact).is_none());
    }

    #[tokio::test]
    async fn a_child_of_tier_is_chosen_across_all_anchors() {
        // Review MINOR 6: cell 1 has only a folded "DELETE", cell 2 an
        // exact "Delete": the exact one wins whatever the anchor order.
        let cell = |y: i32, child: Element| {
            let mut c = at("cell", "", 0, y);
            c.text = None;
            c.bounds.height = 100;
            c.bounds.width = 300;
            c.children = vec![child];
            c
        };
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                cell(100, at("folded", "DELETE", 10, 110)),
                cell(300, at("exact", "Delete", 10, 310)),
            ],
        );
        let sel = rel(
            Selector::Text("Delete".into()),
            Relation::ChildOf,
            Selector::Id("cell".into()),
        );
        let found = with_ceiling(Tier::Fold, async { find_in_tree(&root, &sel, None) }).await;
        assert_eq!(found.unwrap().platform_id, "exact");
    }

    #[tokio::test]
    async fn a_ceiling_keeps_merged_label_parts_out() {
        // Review M2: "Invoice 3, Delete" is not a "Delete" element for a
        // negative check, and a tab "Home, tab, 1 of 4" is the target only
        // in a lookup's last, widest pass.
        let mut row = el("row", None, 0, 0, vec![]);
        row.label = Some("Invoice 3, Delete".into());
        let root = el("", None, 0, 0, vec![row]);
        let delete = Selector::Text("Delete".into());
        assert!(find_in_tree(&root, &delete, None).is_some());
        let folded = with_ceiling(Tier::Fold, async { find_in_tree(&root, &delete, None) }).await;
        assert!(folded.is_none());
        assert!(find_ref_upto(&root, &delete, None, Tier::Fold).is_none());

        let mut tab = el("tab", None, 0, 800, vec![]);
        tab.label = Some("Home, tab, 1 of 4".into());
        let home = Selector::Text("Home".into());
        let only_tab = el("", None, 0, 0, vec![tab.clone()]);
        assert!(find_ref_upto(&only_tab, &home, None, Tier::Fold).is_none());
        assert_eq!(
            find_ref_upto(&only_tab, &home, None, Tier::Loose)
                .unwrap()
                .platform_id,
            "tab"
        );
        // A real "Home" elsewhere wins in every tier.
        let both = el(
            "",
            None,
            0,
            0,
            vec![tab, el("h", Some("Home"), 0, 100, vec![])],
        );
        assert_eq!(
            find_ref_upto(&both, &home, None, Tier::Loose)
                .unwrap()
                .platform_id,
            "h"
        );
        assert_eq!(ceiling(), Tier::Loose);
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    fn leaf(text: &str, y: i32) -> Element {
        Element {
            visible_bounds: Default::default(),
            checked: Default::default(),
            focused: Default::default(),
            obscured: Default::default(),
            hint: None,
            platform_id: String::new(),
            label: None,
            text: Some(text.into()),
            element_type: "View".into(),
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
        }
    }

    fn root(kids: Vec<Element>) -> Element {
        let mut r = leaf("", 0);
        r.text = None;
        r.bounds.height = 2000;
        r.children = kids;
        r
    }

    #[test]
    fn normalize_drops_zero_width_and_collapses_whitespace() {
        assert_eq!(normalize("  Sign\n   in\t"), "Sign in");
        assert_eq!(normalize("Pay\u{200B}now"), "Paynow");
        assert_eq!(normalize("12\u{00A0}000\u{202F}€"), "12 000 €");
        assert_eq!(normalize("\u{FEFF}Done\u{2060}"), "Done");
        assert_eq!(normalize("   "), "");
        assert!(matches!(
            normalize("Plain text"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn text_matches_has_maestro_semantics() {
        // D-11: whole match, ignoring case, `.` across newlines, the
        // pattern taken literally too, text ∪ label ∪ hint.
        let mut field = leaf("", 300);
        field.text = None;
        field.hint = Some("Email address".into());
        let mut labelled = leaf("", 400);
        labelled.text = None;
        labelled.label = Some("Price ($)".into());
        let tree = root(vec![
            leaf("Order #1234", 0),
            leaf("Total\n$12.00", 100),
            leaf("Item 7 of 9", 200),
            field,
            labelled,
        ]);
        let y = |re: &str| {
            find_ref_upto(&tree, &Selector::TextMatches(re.into()), None, Tier::Exact)
                .map(|e| e.bounds.y)
        };
        assert_eq!(y(r"order #\d+"), Some(0));
        assert_eq!(y("Order"), None, "a full match, not a search");
        assert_eq!(y(r"Total.\$\d+\.\d\d"), Some(100), "DOTALL");
        assert_eq!(y(r"total \$12\.00"), Some(100), "newline read as a space");
        assert_eq!(y(r"Item \d+ of \d+"), Some(200));
        assert_eq!(y("email.*"), Some(300), "the hint");
        assert_eq!(y("Price ($)"), Some(400), "literally");
        assert_eq!(
            y("[unclosed"),
            None,
            "an invalid pattern matches only literally"
        );
        assert!(compile_pattern("[unclosed").is_err());
        assert!(compile_pattern("").is_err());
        assert!(compile_pattern(r"row_\d+").is_ok());
    }

    #[test]
    fn id_matches_the_whole_id_or_its_short_form() {
        let mut a = leaf("", 0);
        a.platform_id = "com.app:id/row_12".into();
        let mut b = leaf("", 100);
        b.platform_id = "com.app:id/header".into();
        let tree = root(vec![a, b]);
        let n = |re: &str| match_count(&tree, &Selector::IdMatches(re.into()), None);
        assert_eq!(n(r"row_\d+"), 1);
        assert_eq!(n(r"ROW_.*"), 1, "ignoring case");
        assert_eq!(n(r".*:id/.*"), 2);
        assert_eq!(n("row"), 0, "a full match");
    }

    #[test]
    fn visible_means_rendered_with_a_box_on_screen() {
        // D-1: one rule; any pixel by default, `visibility.minArea` more.
        let screen = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 800,
        };
        let at = |y: i32| leaf("x", y);
        assert!(shown_with(&at(100), &screen, 0.0));
        assert!(!shown_with(&at(900), &screen, 0.0), "below the screen");
        let half = at(780); // 20 of its 40 px on screen
        assert!(shown_with(&half, &screen, 0.0));
        assert!((visible_fraction(&half.bounds, &screen) - 0.5).abs() < 1e-9);
        assert!(shown_with(&half, &screen, 0.5));
        assert!(!shown_with(&half, &screen, 0.6));
        let mut hidden = at(100);
        hidden.visible = false;
        assert!(!shown_with(&hidden, &screen, 0.0));
        let mut empty = at(100);
        empty.bounds.height = 0;
        assert!(!shown_with(&empty, &screen, 0.0));
        assert_eq!(min_visible_area(), 0.0, "the default");
    }

    #[test]
    fn ambiguity_suggests_child_of_a_named_row_or_an_index() {
        // D-6 / R6: two "Add" buttons, each in a row with an id; a third
        // in a row that has nothing stable to name it by.
        let row = |id: &str, y: i32| {
            let mut r = leaf("", y);
            r.text = None;
            r.platform_id = id.into();
            r.bounds.height = 90;
            r.children = vec![leaf("Add", y + 10)];
            r
        };
        let tree = root(vec![
            row("com.app:id/row_coffee", 0),
            row("com.app:id/row_tea", 100),
            row("", 200),
        ]);
        let add = Selector::Text("Add".into());
        assert_eq!(ambiguity(&tree, &add, None), 3);
        let picks = suggest_picks(&tree, &add, None, 5);
        assert_eq!(picks.len(), 3);
        assert_eq!(
            picks[0],
            Selector::Relative {
                target: Box::new(add.clone()),
                relation: Relation::ChildOf,
                anchor: Box::new(Selector::Id("row_coffee".into())),
            }
        );
        assert_eq!(
            picks[2],
            Selector::Index {
                selector: Box::new(add.clone()),
                index: 2
            }
        );
        // Every suggestion picks exactly its own match.
        for (i, p) in picks.iter().enumerate() {
            let found = find_all_ref(&tree, p, None);
            assert_eq!(found.len(), 1, "{p}");
            assert_eq!(found[0].bounds.y, i as i32 * 100 + 10, "{p}");
        }
    }

    /// The comparisons that skip building a lowercase copy agree with
    /// lowercasing the whole string, Unicode's special cases included: a
    /// final sigma, the Kelvin sign, a dotted capital I (two chars), ß.
    #[test]
    fn caseless_comparisons_agree_with_to_lowercase() {
        let samples = [
            "",
            "a",
            "Sign in",
            "SIGN IN",
            "ΟΔΟΣ",
            "οδος",
            "οδοσ",
            "ΣΑΣ ΣΑΣ",
            "σας σας",
            "\u{212A}elvin",
            "kelvin",
            "İstanbul",
            "i\u{307}stanbul",
            "STRASSE",
            "Straße",
            "Größe Ärger 7",
            "größe ärger",
            "ärger",
        ];
        for have in samples {
            for want in samples {
                for want in [want.to_string(), want.to_lowercase()] {
                    let lower = have.to_lowercase();
                    assert_eq!(lower_eq(have, &want), lower == want, "{have:?} = {want:?}");
                    assert_eq!(
                        lower_contains(have, &want),
                        lower.contains(&want),
                        "{have:?} contains {want:?}"
                    );
                }
            }
        }
        // Through a lookup: a folded `text:` on Greek capitals.
        let tree = root(vec![leaf("ΟΔΟΣ", 0)]);
        let sel = Selector::Text("οδος".into());
        assert!(find_ref_upto(&tree, &sel, None, Tier::Exact).is_none());
        assert!(find_ref_upto(&tree, &sel, None, Tier::Fold).is_some());
        assert!(matches_in(
            &tree.children[0],
            &sel,
            &Want::Nothing,
            None,
            Tier::Fold
        ));
    }

    #[test]
    fn text_matches_after_normalising_both_sides() {
        // D-10: a label split over lines, an NBSP, a zero-width space.
        let tree = root(vec![
            leaf("Sign\nin", 0),
            leaf("Total\u{00A0}due", 100),
            leaf("Pay\u{200B}now", 200),
        ]);
        let y = |sel: Selector| find_ref_upto(&tree, &sel, None, Tier::Exact).map(|e| e.bounds.y);
        assert_eq!(y(Selector::Text("Sign in".into())), Some(0));
        assert_eq!(y(Selector::Text(" Sign  in ".into())), Some(0));
        assert_eq!(y(Selector::Text("Total due".into())), Some(100));
        assert_eq!(y(Selector::Text("Paynow".into())), Some(200));
        assert_eq!(y(Selector::TextContains("n i".into())), Some(0));
        assert_eq!(y(Selector::TextContains("l d".into())), Some(100));
        // tc-case: textContains ignores case.
        assert_eq!(y(Selector::TextContains("SIGN".into())), Some(0));
        assert_eq!(y(Selector::TextContains("total DUE".into())), Some(100));
        // Whitespace alone is still an empty selector.
        assert_eq!(y(Selector::Text(" \n ".into())), None);
    }
}

#[cfg(test)]
mod role_state_tests {
    use super::*;

    fn el(id: &str, text: Option<&str>, x: i32, y: i32, children: Vec<Element>) -> Element {
        Element {
            platform_id: id.into(),
            text: text.map(String::from),
            element_type: "View".into(),
            bounds: Rect {
                x,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children,
            ..Default::default()
        }
    }

    fn typed(ty: &str, text: &str, y: i32) -> Element {
        let mut e = el("", Some(text), 0, y, vec![]);
        e.element_type = ty.into();
        e
    }

    #[test]
    fn role_and_name_select_by_what_the_element_is() {
        // S1: a label and a button carry the same text; `role:` picks the
        // button, and `name:` narrows among buttons.
        let root = el(
            "",
            None,
            0,
            0,
            vec![
                typed("android.widget.TextView", "Pay", 0),
                typed("android.widget.Button", "Cancel", 50),
                typed("android.widget.Button", "Pay", 100),
                typed("XCUIElementTypeTextField", "", 150),
            ],
        );
        let find = |s: Selector| find_in_tree(&root, &s, None).map(|e| e.bounds.y);
        assert_eq!(find(Selector::Text("Pay".into())), Some(0));
        assert_eq!(
            find(Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("Pay".into()),
            ])),
            Some(100)
        );
        assert_eq!(
            match_count(&root, &Selector::Role("button".into()), None),
            2
        );
        assert_eq!(find(Selector::Role("textbox".into())), Some(150));
        assert_eq!(find(Selector::Role("link".into())), None);
        // `name:` folds case only when nothing matches exactly, as `text:`.
        assert_eq!(
            find(Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Name("pay".into()),
            ])),
            Some(100)
        );
        assert_eq!(find(Selector::Name(" ".into())), None);
        assert_eq!(find(Selector::Role(String::new())), None);
    }

    #[test]
    fn state_filters_narrow_a_selector() {
        // S2: two "Remember" checkboxes, one checked; a disabled and an
        // enabled "Next"; a selected tab; a focused field.
        let mut on = typed("CheckBox", "Remember", 0);
        on.checked = Some(true);
        let mut off = typed("CheckBox", "Remember", 50);
        off.checked = Some(false);
        let mut next_off = typed("Button", "Next", 100);
        next_off.enabled = false;
        let next_on = typed("Button", "Next", 150);
        let mut tab = typed("XCUIElementTypeButton", "Home", 200);
        tab.selected = Some(true);
        let mut tab2 = typed("XCUIElementTypeButton", "Away", 250);
        tab2.selected = Some(false);
        let mut field = typed("EditText", "", 300);
        field.focused = Some(true);
        let label = typed("TextView", "Remember", 350);
        let root = el(
            "",
            None,
            0,
            0,
            vec![on, off, next_off, next_on, tab, tab2, field, label],
        );
        let y = |parts: Vec<Selector>| {
            find_in_tree(&root, &Selector::Compound(parts), None).map(|e| e.bounds.y)
        };
        let text = |t: &str| Selector::Text(t.into());
        assert_eq!(y(vec![text("Remember"), Selector::Checked(true)]), Some(0));
        assert_eq!(
            y(vec![text("Remember"), Selector::Checked(false)]),
            Some(50)
        );
        assert_eq!(
            match_count(
                &root,
                &Selector::Compound(vec![text("Remember"), Selector::Checked(false)]),
                None
            ),
            1,
            "a label has no checked state"
        );
        assert_eq!(y(vec![text("Next"), Selector::Enabled(true)]), Some(150));
        assert_eq!(y(vec![text("Next"), Selector::Enabled(false)]), Some(100));
        assert_eq!(
            y(vec![
                Selector::Role("button".into()),
                Selector::Selected(true)
            ]),
            Some(200)
        );
        assert_eq!(
            y(vec![
                Selector::Role("textbox".into()),
                Selector::Focused(true)
            ]),
            Some(300)
        );
        assert_eq!(
            y(vec![
                Selector::Role("textbox".into()),
                Selector::Focused(false)
            ]),
            None
        );
    }

    /// Review m1: a driver that can't report focus or selection (headless)
    /// leaves them unknown, and an unknown state matches neither value. It
    /// counted as `false`: `selected: false` matched every tab and
    /// `selected: true` none.
    #[test]
    fn an_unknown_state_matches_neither_true_nor_false() {
        let unknown = typed("XCUIElementTypeButton", "Home", 0);
        assert_eq!((unknown.selected, unknown.focused), (None, None));
        let mut known = typed("XCUIElementTypeButton", "Away", 50);
        known.selected = Some(false);
        known.focused = Some(false);
        let root = el("", None, 0, 0, vec![unknown, known]);
        let count = |state: Selector| {
            match_count(
                &root,
                &Selector::Compound(vec![Selector::Role("button".into()), state]),
                None,
            )
        };
        assert_eq!(count(Selector::Selected(false)), 1, "only the known one");
        assert_eq!(count(Selector::Focused(false)), 1, "only the known one");
        assert_eq!(count(Selector::Selected(true)), 0);
        assert_eq!(count(Selector::Focused(true)), 0);
        let headless_tab: Element =
            serde_json::from_str(r#"{"platform_id":"t","label":"Home","text":null,"element_type":"XCUIElementTypeButton","bounds":{"x":0,"y":0,"width":10,"height":10},"enabled":true,"visible":true,"children":[]}"#)
                .unwrap();
        assert!(!matches_selector(
            &headless_tab,
            &Selector::Selected(false),
            None
        ));
        assert!(!matches_selector(
            &headless_tab,
            &Selector::Focused(false),
            None
        ));
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    /// A rough timing of lookups over a large synthetic tree (5,000
    /// elements, a tenth with non-ASCII text and a tenth over two lines),
    /// for each tier and selector kind that compares text. Not run by
    /// default:
    /// `cargo test --release -p velocity-common lookup_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing only"]
    fn lookup_timing_on_a_large_tree() {
        let leaf = |i: i32| Element {
            platform_id: format!("com.app:id/row_{i}"),
            text: Some(match i % 10 {
                0 => format!("Größe Ärger {i}"),
                1 => format!("Row\nnumber {i}"),
                _ => format!("Row number {i}"),
            }),
            label: Some(format!("Row label {i}")),
            element_type: "android.widget.TextView".into(),
            bounds: Rect {
                x: 0,
                y: i * 10,
                width: 100,
                height: 10,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        };
        let groups = (0..100)
            .map(|g| Element {
                children: (0..50).map(|i| leaf(g * 50 + i)).collect(),
                bounds: Rect {
                    x: 0,
                    y: g * 500,
                    width: 100,
                    height: 500,
                },
                visible: true,
                ..Default::default()
            })
            .collect();
        let root = Element {
            children: groups,
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 50_000,
            },
            visible: true,
            ..Default::default()
        };
        let cases = [
            ("text, exact", Selector::Text("Row number 4999".into())),
            ("text, folded", Selector::Text("ROW NUMBER 4999".into())),
            ("text, missing", Selector::Text("Nowhere".into())),
            ("textContains", Selector::TextContains("NUMBER 4998".into())),
            (
                "textMatches",
                Selector::TextMatches("row number 49[0-9]9".into()),
            ),
            (
                "compound",
                Selector::Compound(vec![
                    Selector::ClassName("TextView".into()),
                    Selector::Text("größe ärger 4990".into()),
                ]),
            ),
        ];
        const RUNS: u32 = 50;
        for (name, sel) in &cases {
            let start = std::time::Instant::now();
            for _ in 0..RUNS {
                std::hint::black_box(find_all_ref_upto(&root, sel, None, Tier::Loose));
            }
            eprintln!("{name:>14}: {:?} per lookup", start.elapsed() / RUNS);
        }
    }
}
