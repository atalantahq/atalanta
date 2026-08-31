//! What the agent sees of a screen, and the code-side judgements about it:
//! a structural hash (stuck detection, "seen before"), a title (history
//! lines), and which texts are too volatile to assert on.

use velocity_common::selector_build::{robust_selector, stable_id};
use velocity_common::{Element, Platform, Rect, Selector};
use velocity_nl::ScreenIndex;

/// One look at the screen.
pub struct Observation {
    pub root: Element,
    pub rect: Option<Rect>,
    pub index: ScreenIndex,
    /// [`ScreenIndex::render`] of `index`, as the planner sees it.
    pub rendered: String,
    pub hash: u64,
    pub title: String,
}

impl Observation {
    pub fn new(root: Element, rect: Option<Rect>, platform: Platform, goal: &str) -> Self {
        let index = ScreenIndex::build(&root, rect.as_ref(), platform, goal);
        let rendered = index.render();
        let hash = screen_hash(&index, platform);
        let title = screen_title(&index, rect.as_ref());
        Self {
            root,
            rect,
            index,
            rendered,
            hash,
            title,
        }
    }

    pub fn platform(&self) -> Platform {
        self.index.platform()
    }

    /// Whether `other` shows the same: every listed element with the same
    /// key, states and place. Finer than [`hash`](Self::hash), which a
    /// flipped switch or a list scrolled by a row leaves alone; digits are
    /// folded as there, so a ticking clock is no change.
    pub fn looks_same(&self, other: &Observation) -> bool {
        let platform = self.platform();
        let look = |o: &Observation| {
            o.elements()
                .map(|el| {
                    (
                        element_key(el, platform),
                        (el.checked, el.selected, el.focused, el.enabled),
                        el.bounds,
                    )
                })
                .collect::<Vec<_>>()
        };
        look(self) == look(other)
    }

    /// The listed elements, in list order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        (0..self.index.len()).filter_map(|i| self.index.get(i))
    }

    /// The selector code builds for listed element `n`.
    pub fn selector_for(&self, n: usize) -> Option<Selector> {
        let el = self.index.get(n)?;
        Some(robust_selector(
            el,
            &self.root,
            self.rect.as_ref(),
            self.platform(),
        ))
    }

    /// The shown elements `sel` matches on this screen, as a driver would
    /// see them.
    pub fn matches(&self, sel: &Selector) -> Vec<Element> {
        let mut found = Vec::new();
        velocity_common::selector_match::find_all_in_tree(
            &self.root,
            sel,
            self.rect.as_ref(),
            &mut found,
        );
        found.retain(|m| m.visible && !m.bounds.is_empty());
        found
    }

    /// The identity keys of the listed elements (no positions).
    pub fn keys(&self) -> std::collections::HashSet<String> {
        self.elements()
            .map(|e| element_key(e, self.platform()))
            .collect()
    }
}

/// A short human name for an element: its label, else text, else id.
pub fn element_name(el: &Element, platform: Platform) -> String {
    el.label
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(el.text.as_deref().filter(|s| !s.trim().is_empty()))
        .or(stable_id(el, platform))
        .map(|s| clip(s.trim(), 60))
        .unwrap_or_else(|| format!("<{}>", el.element_type))
}

/// An element's identity on a screen, without its position: type, stable
/// id, label and text, with digits folded (a clock must not make every
/// screen new).
pub fn element_key(el: &Element, platform: Platform) -> String {
    fold_digits(&format!(
        "{}|{}|{}|{}",
        el.element_type,
        stable_id(el, platform).unwrap_or(""),
        el.label.as_deref().unwrap_or(""),
        el.text.as_deref().unwrap_or("")
    ))
}

/// A structural hash of the screen: the listed elements' keys in order.
/// Positions are left out (a scroll by a few pixels of the same list is the
/// same screen) and digits are folded (clocks, counters).
pub fn screen_hash(index: &ScreenIndex, platform: Platform) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for i in 0..index.len() {
        if let Some(el) = index.get(i) {
            for b in element_key(el, platform).bytes().chain(std::iter::once(0)) {
                h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h
}

fn fold_digits(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_digit() { '#' } else { c })
        .collect()
}

/// The screen's title, for history lines: a navigation bar's or heading's
/// name, else the top-most non-volatile text below the status bar.
pub fn screen_title(index: &ScreenIndex, rect: Option<&Rect>) -> String {
    let platform = index.platform();
    let els: Vec<&Element> = (0..index.len()).filter_map(|i| index.get(i)).collect();
    let named = |el: &&Element| {
        let n = element_name(el, platform);
        (!n.starts_with('<') && !is_volatile(&n)).then_some(n)
    };
    for el in &els {
        let ty = el.element_type.to_ascii_lowercase();
        if ty.contains("navigationbar") || ty.contains("heading") || ty == "h1" {
            if let Some(n) = named(el) {
                return n;
            }
        }
    }
    for el in &els {
        if is_title_id(&el.platform_id) {
            if let Some(n) = named(el) {
                return n;
            }
        }
    }
    let height = rect.map(|r| r.height).unwrap_or(0);
    let status_bar = height / 20;
    els.iter()
        .filter(|el| el.bounds.y >= status_bar)
        .filter(|el| {
            let ty = el.element_type.to_ascii_lowercase();
            ty.contains("text") || ty.contains("label")
        })
        .filter(|el| el.text.is_some() || el.label.is_some())
        .min_by_key(|el| (el.bounds.y, el.bounds.x))
        .and_then(named)
        .unwrap_or_else(|| "untitled screen".into())
}

/// An id that names a screen's title bar, not a list row's title:
/// `collapsing_toolbar`, `action_bar`, `homepage_title`, `toolbar_title`
/// (but not Android's per-row `android:id/title`).
fn is_title_id(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let short = id.rsplit_once(":id/").map(|(_, s)| s).unwrap_or(&id);
    short.contains("toolbar")
        || short.contains("action_bar")
        || short.contains("header")
        || (short.ends_with("_title") && short != "title")
}

/// Selectors by the element's content (label, then text) that match
/// only it on this screen: what a screen assertion prefers, since an id
/// like `collapsing_toolbar` is on every page and proves nothing.
pub fn content_selectors(el: &Element, obs: &Observation) -> Vec<Selector> {
    let platform = obs.platform();
    let mut candidates = Vec::new();
    if let Some(l) = el.label.as_deref().filter(|s| !s.trim().is_empty()) {
        candidates.push(if platform == Platform::Web {
            Selector::Text(l.to_string())
        } else {
            Selector::AccessibilityId(l.to_string())
        });
    }
    if let Some(t) = el.text.as_deref().filter(|s| !s.trim().is_empty()) {
        candidates.push(Selector::Text(t.to_string()));
    }
    candidates
        .into_iter()
        .filter(|sel| {
            // One element, or one nested in another (an iOS cell and its
            // label carry the same name): either way one place on screen.
            let found = obs.matches(sel);
            !found.is_empty()
                && found.iter().any(|m| m.bounds == el.bounds)
                && found
                    .iter()
                    .all(|m| nested(&m.bounds, &el.bounds) || nested(&el.bounds, &m.bounds))
        })
        .collect()
}

/// Whether `inner` lies within `outer`.
fn nested(inner: &Rect, outer: &Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// Whether a text is too volatile to assert on: times, dates, prices,
/// percentages, counters and long numbers (order ids, phone numbers).
/// A word or two with a small number ("Android 15", "Wi-Fi 6") is not.
pub fn is_volatile(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let chars: Vec<char> = t.chars().collect();
    // Number groups: maximal runs of digits.
    let mut groups: Vec<usize> = Vec::new();
    let mut run = 0usize;
    for c in &chars {
        if c.is_ascii_digit() {
            run += 1;
        } else if run > 0 {
            groups.push(run);
            run = 0;
        }
    }
    if run > 0 {
        groups.push(run);
    }
    if groups.is_empty() {
        return false;
    }
    let has = |pat: &[char]| chars.iter().any(|c| pat.contains(c));
    // A time (12:30), a price or a percentage.
    let time = chars
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == ':' && w[2].is_ascii_digit());
    if time || has(&['$', '€', '£', '¥', '₹', '%']) {
        return true;
    }
    // Only digits and separators: a counter, a date, an amount.
    if chars
        .iter()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || ",./-+:".contains(*c))
    {
        return true;
    }
    // A long number, or several numbers (dates in words, "3 of 12").
    groups.iter().any(|&g| g >= 3) || groups.len() >= 2
}

/// A selector worth asserting on: unique (no `index:`), and no text in it
/// that is volatile or holds one of `user_values` (typed data, secrets).
pub fn assertable(sel: &Selector, user_values: &[String]) -> bool {
    fn texts(sel: &Selector, out: &mut Vec<String>) -> bool {
        match sel {
            Selector::Index { .. } => false,
            Selector::Id(_) | Selector::Role(_) => true,
            Selector::Text(t)
            | Selector::TextContains(t)
            | Selector::AccessibilityId(t)
            | Selector::Name(t) => {
                out.push(t.clone());
                true
            }
            Selector::Compound(parts) => parts.iter().all(|p| texts(p, out)),
            Selector::Relative { target, anchor, .. } => texts(target, out) && texts(anchor, out),
            _ => false,
        }
    }
    let mut out = Vec::new();
    if !texts(sel, &mut out) {
        return false;
    }
    out.iter().all(|t| {
        !is_volatile(t)
            && !user_values
                .iter()
                .any(|v| !v.is_empty() && (t.contains(v.as_str()) || v.contains(t.as_str())))
    })
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volatile_texts_are_recognised() {
        for v in [
            "12:30",
            "$12.40",
            "Total €3",
            "45%",
            "1,024",
            "2026-10-25",
            "Order 834221",
            "3 of 12",
            "+1 555 0100",
        ] {
            assert!(is_volatile(v), "{v}");
        }
        for s in ["Network & internet", "Android 15", "Wi-Fi 6", "Sign in", ""] {
            assert!(!is_volatile(s), "{s}");
        }
    }

    #[test]
    fn assertable_selectors_are_unique_and_stable() {
        let t = |s: &str| Selector::Text(s.into());
        assert!(assertable(&t("Internet"), &[]));
        assert!(assertable(&Selector::Id("title".into()), &[]));
        assert!(!assertable(&t("12:30"), &[]));
        assert!(!assertable(
            &Selector::Index {
                selector: Box::new(t("Row")),
                index: 1
            },
            &[]
        ));
        // User data never becomes an assertion.
        assert!(!assertable(
            &t("Hello alice@example.com"),
            &["alice@example.com".into()]
        ));
    }
}
