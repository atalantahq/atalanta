//! Accessibility audit: deterministic rules over a screen's element tree.
//!
//! Rules are the cheap, deterministic base, not the whole answer:
//! ScreenAudit (Zhong et al., CHI 2025, arXiv 2504.02110) found 69.2% of
//! screen-reader errors where a widely used rule checker found 31.3%, so a
//! clean audit is no proof of an accessible screen. A run reads the element
//! tree at nearly every step anyway, so checking it costs no device round
//! trip (`config.accessibility.audit`, executor).
//!
//! Each rule flags only what the tree can show: Velocity's trees carry no
//! `clickable`, so an element counts as interactive only when its role says
//! it acts ([`crate::role::role_of`]), and a rule leaves out what it can't
//! judge (a system control's fixed size, an inline link) rather than guess.
//!
//! | Rule | Severity | Source |
//! |---|---|---|
//! | `name-missing` | error | WCAG 2.2 SC 4.1.2 Name, Role, Value |
//! | `image-unlabeled` | error | WCAG 2.2 SC 1.1.1 Non-text Content, 4.1.2 |
//! | `target-size` | warning | WCAG 2.2 SC 2.5.8 (24×24 CSS px), Android 48×48 dp, Apple HIG 44×44 pt |
//! | `name-duplicate` | warning | WCAG 2.2 SC 2.4.6 Headings and Labels, 2.4.4 Link Purpose |
//! | `field-unlabeled` | error | WCAG 2.2 SC 1.3.1, 3.3.2 Labels or Instructions, 4.1.2 |
//! | `field-placeholder-only` | warning | WCAG 2.2 SC 3.3.2 (a placeholder goes once text is typed) |

use serde::Serialize;

use crate::role::{accessible_name, role_of};
use crate::selector_match::normalize;
use crate::{Element, Platform, Rect};

/// How bad a finding is. `fail: true` fails a test on errors only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// Rule ids, as findings and the docs name them.
pub mod rule {
    /// An interactive element with no accessible name.
    pub const NAME_MISSING: &str = "name-missing";
    /// An image-only control (icon button) with no label.
    pub const IMAGE_UNLABELED: &str = "image-unlabeled";
    /// A touch target below the platform's minimum.
    pub const TARGET_SIZE: &str = "target-size";
    /// Interactive siblings in one container with the same name.
    pub const NAME_DUPLICATE: &str = "name-duplicate";
    /// An editable field with no label at all.
    pub const FIELD_UNLABELED: &str = "field-unlabeled";
    /// An editable field named only by its placeholder.
    pub const FIELD_PLACEHOLDER_ONLY: &str = "field-placeholder-only";
}

/// Every rule id with its severity, for listings.
pub const RULES: &[(&str, Severity)] = &[
    (rule::NAME_MISSING, Severity::Error),
    (rule::IMAGE_UNLABELED, Severity::Error),
    (rule::TARGET_SIZE, Severity::Warning),
    (rule::NAME_DUPLICATE, Severity::Warning),
    (rule::FIELD_UNLABELED, Severity::Error),
    (rule::FIELD_PLACEHOLDER_ONLY, Severity::Warning),
];

/// One problem on one element.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// The element, without its children.
    pub element: Element,
    /// Address of the tree node this finding is about, for telling twin
    /// elements apart (compare with `std::ptr::from_ref(node) as usize`).
    /// Only meaningful while the audited tree is alive.
    #[serde(skip)]
    pub node: usize,
    /// What is wrong, one line.
    pub message: String,
    /// How to fix it, one line.
    pub fix: String,
}

impl Finding {
    fn new(
        rule: &'static str,
        severity: Severity,
        el: &Element,
        message: String,
        fix: &str,
    ) -> Self {
        Finding {
            rule,
            severity,
            element: Element {
                children: Vec::new(),
                ..el.clone()
            },
            node: std::ptr::from_ref(el) as usize,
            message,
            fix: fix.to_string(),
        }
    }

    /// One line for a report: `error name-missing: ImageButton #menu at
    /// 10,20 24x24: … Fix: …`.
    pub fn line(&self) -> String {
        format!(
            "{} {}: {}: {}. Fix: {}",
            self.severity,
            self.rule,
            describe(&self.element),
            self.message,
            self.fix
        )
    }

    /// The same problem on the same element, across screens: a toolbar
    /// button seen on every screen is one finding. Elements with no id are
    /// told apart by where they are.
    pub fn key(&self) -> String {
        let e = &self.element;
        let place = if e.platform_id.is_empty() {
            format!(
                "{},{},{},{}",
                e.bounds.x, e.bounds.y, e.bounds.width, e.bounds.height
            )
        } else {
            String::new()
        };
        format!(
            "{}|{}|{}|{}|{place}",
            self.rule,
            short_type(&e.element_type),
            e.platform_id,
            accessible_name(e).unwrap_or_default()
        )
    }
}

/// What an audit needs besides the tree.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub platform: Platform,
    /// The screen, in the tree's units. `None`: the root's bounds, else the
    /// union of its children's.
    pub screen: Option<Rect>,
    /// Android pixels per dp (`density`, 2.625 at 420 dpi). `None`: the
    /// screen's short side is taken as 411 dp, a phone's (the Pixel and
    /// Android Studio's default emulator). That errs lenient on phones,
    /// narrower ones (360 dp) included, and strict on tablets and unfolded
    /// foldables, which should set it.
    pub density: Option<f64>,
}

/// The assumed short side of an Android screen in dp when no density is
/// given (see [`Options::density`]).
pub const ASSUMED_SHORT_SIDE_DP: f64 = 411.0;

/// Audit one screen: every rule, findings in tree order.
pub fn audit(root: &Element, platform: Platform, screen: Option<Rect>) -> Vec<Finding> {
    audit_with(
        root,
        &Options {
            platform,
            screen,
            density: None,
        },
    )
}

/// [`audit`] with an Android density.
pub fn audit_with(root: &Element, opts: &Options) -> Vec<Finding> {
    let screen = opts.screen.or_else(|| extent(root));
    let density = match opts.platform {
        Platform::Android => opts
            .density
            .filter(|d| d.is_finite() && *d > 0.0)
            .or_else(|| {
                screen.map(|s| {
                    (f64::from(s.width.min(s.height)) / ASSUMED_SHORT_SIDE_DP).clamp(0.75, 4.0)
                })
            })
            .unwrap_or(2.625),
        _ => 1.0,
    };
    let mut a = Auditor {
        platform: opts.platform,
        screen,
        density,
        targets: Vec::new(),
        out: Vec::new(),
    };
    a.collect_targets(root, 0);
    a.visit(root, None, false, 0);
    a.out
}

/// Deep enough for any real screen; a hostile tree stops here.
const MAX_DEPTH: usize = 256;

/// Roles that act when activated: what a screen reader announces as a
/// control.
const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "checkbox",
    "switch",
    "radio",
    "textbox",
    "searchbox",
    "combobox",
    "slider",
    "spinbutton",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
];

/// Editable fields: named by a label, not by their value.
const FIELDS: &[&str] = &["textbox", "searchbox"];

struct Auditor<'a> {
    platform: Platform,
    screen: Option<Rect>,
    /// Pixels per target-size unit (Android: per dp; 1 elsewhere).
    density: f64,
    /// Every shown target, for the web's spacing exception.
    targets: Vec<&'a Element>,
    out: Vec<Finding>,
}

fn short_type(t: &str) -> &str {
    let t = t.strip_prefix("XCUIElementType").unwrap_or(t);
    let t = t.rsplit('.').next().unwrap_or(t);
    t.rsplit('$').next().unwrap_or(t)
}

/// System UI the app can't fix: the iOS keyboard and status bar.
fn is_system(el: &Element) -> bool {
    matches!(short_type(&el.element_type), "Keyboard" | "StatusBar")
}

fn interactive(el: &Element) -> Option<&str> {
    role_of(el).filter(|r| INTERACTIVE.contains(r))
}

fn non_empty(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|v| !normalize(v).is_empty())
}

/// An icon control: an image button type, or a control whose only content
/// is an image.
fn image_like(el: &Element) -> bool {
    let t = short_type(&el.element_type);
    if t.contains("ImageButton") || t.contains("FloatingActionButton") {
        return true;
    }
    let has_text = |e: &Element| non_empty(&e.text) || non_empty(&e.label);
    fn any_image(e: &Element, depth: usize) -> bool {
        depth < 8
            && e.children.iter().any(|c| {
                role_of(c) == Some("img")
                    || matches!(
                        short_type(&c.element_type),
                        "Image" | "ImageView" | "img" | "svg"
                    )
                    || any_image(c, depth + 1)
            })
    }
    fn any_text(e: &Element, depth: usize) -> bool {
        depth < 8
            && e.children
                .iter()
                .any(|c| non_empty(&c.text) || non_empty(&c.label) || any_text(c, depth + 1))
    }
    !has_text(el) && any_image(el, 0) && !any_text(el, 0)
}

/// The tree's own extent: the root's bounds, else its children's union.
fn extent(root: &Element) -> Option<Rect> {
    if !root.bounds.is_empty() {
        return Some(root.bounds);
    }
    let mut it = root
        .children
        .iter()
        .map(|c| c.bounds)
        .filter(|b| !b.is_empty());
    let first = it.next()?;
    Some(it.fold(first, |a, b| {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        let r = (i64::from(a.x) + i64::from(a.width)).max(i64::from(b.x) + i64::from(b.width));
        let bot = (i64::from(a.y) + i64::from(a.height)).max(i64::from(b.y) + i64::from(b.height));
        Rect {
            x,
            y,
            width: (r - i64::from(x)).clamp(0, i64::from(i32::MAX)) as i32,
            height: (bot - i64::from(y)).clamp(0, i64::from(i32::MAX)) as i32,
        }
    }))
}

/// `Button "Pay" #pay at 10,20 48x48`.
fn describe(el: &Element) -> String {
    let mut s = short_type(&el.element_type).to_string();
    if let Some(n) = accessible_name(el) {
        s.push_str(&format!(" {n:?}"));
    }
    if !el.platform_id.is_empty() {
        s.push_str(&format!(" #{}", el.platform_id));
    }
    let b = el.bounds;
    s.push_str(&format!(" at {},{} {}x{}", b.x, b.y, b.width, b.height));
    s
}

impl<'a> Auditor<'a> {
    /// Whether any part of `el` is on the screen.
    fn on_screen(&self, el: &Element) -> bool {
        if el.visible_bounds.is_some_and(|v| v.is_empty()) {
            return false;
        }
        self.screen.is_none_or(|s| el.bounds.intersects(&s))
    }

    fn shown_target(&self, el: &Element) -> bool {
        el.visible && el.enabled && !el.bounds.is_empty() && self.on_screen(el)
    }

    fn collect_targets(&mut self, el: &'a Element, depth: usize) {
        if depth > MAX_DEPTH || is_system(el) {
            return;
        }
        if interactive(el).is_some() && self.shown_target(el) {
            self.targets.push(el);
        }
        for c in &el.children {
            self.collect_targets(c, depth + 1);
        }
    }

    /// The name a screen reader reads, with what the role helper can't see:
    /// a web control inside a `<label>` is named by it, and a combobox by
    /// its value when nothing else names it (lenient: Android's Spinner and
    /// the web's `<select>` announce their selection).
    fn named(&self, el: &Element, role: &str, in_label: bool) -> bool {
        if in_label && self.platform == Platform::Web {
            return true;
        }
        accessible_name(el).is_some() || (role == "combobox" && non_empty(&el.text))
    }

    fn visit(
        &mut self,
        el: &'a Element,
        parent: Option<&'a Element>,
        in_label: bool,
        depth: usize,
    ) {
        if depth > MAX_DEPTH || is_system(el) {
            return;
        }
        let in_label = in_label || (self.platform == Platform::Web && el.element_type == "label");
        if el.visible {
            if let Some(role) = interactive(el) {
                self.names(el, role, in_label);
                self.target_size(el, role, parent);
            }
            self.duplicates(el);
        }
        for c in &el.children {
            self.visit(c, Some(el), in_label, depth + 1);
        }
    }

    /// `name-missing`, `image-unlabeled`, `field-unlabeled`,
    /// `field-placeholder-only`: one finding per element at most.
    fn names(&mut self, el: &Element, role: &str, in_label: bool) {
        let p = self.platform;
        if FIELDS.contains(&role) {
            if (in_label && p == Platform::Web) || non_empty(&el.label) {
                return;
            }
            if non_empty(&el.hint) {
                // Android names an EditText by its hint and TalkBack reads it
                // after the text too (developer.android.com, "Make apps more
                // accessible": label editable fields with android:hint);
                // elsewhere the placeholder goes once text is typed.
                if p != Platform::Android {
                    let fix = match p {
                        Platform::Ios => "set an accessibilityLabel too: the placeholder is gone once text is typed",
                        _ => "add a <label> (or aria-label): a placeholder disappears as the user types",
                    };
                    self.out.push(Finding::new(
                        rule::FIELD_PLACEHOLDER_ONLY,
                        Severity::Warning,
                        el,
                        "the field is labelled only by its placeholder".into(),
                        fix,
                    ));
                }
                return;
            }
            let fix = match p {
                Platform::Android => {
                    "label it: android:hint, a TextInputLayout, or a TextView with android:labelFor"
                }
                Platform::Ios => "set its accessibilityLabel",
                Platform::Web => "add a <label for> or aria-label / aria-labelledby",
            };
            self.out.push(Finding::new(
                rule::FIELD_UNLABELED,
                Severity::Error,
                el,
                "an editable field has no label".into(),
                fix,
            ));
            return;
        }
        if self.named(el, role, in_label) {
            return;
        }
        if image_like(el) {
            let fix = match p {
                Platform::Android => "set a contentDescription saying what it does (Compose: on the Icon or the clickable)",
                Platform::Ios => "set its accessibilityLabel (an image's file name is not a label)",
                Platform::Web => "give the image alt text saying what the control does, or the control an aria-label",
            };
            self.out.push(Finding::new(
                rule::IMAGE_UNLABELED,
                Severity::Error,
                el,
                format!("an image-only {role} has no label"),
                fix,
            ));
            return;
        }
        let fix = match p {
            Platform::Android => "give it text or a contentDescription",
            Platform::Ios => "set its accessibilityLabel",
            Platform::Web => "give it text content, aria-label or aria-labelledby",
        };
        self.out.push(Finding::new(
            rule::NAME_MISSING,
            Severity::Error,
            el,
            format!("a {role} has no accessible name"),
            fix,
        ));
    }

    /// The platform minimum in the tree's units, and how it's written.
    fn minimum(&self) -> (f64, &'static str) {
        match self.platform {
            // developer.android.com "Make apps more accessible": touch
            // targets at least 48x48 dp; Material's minimum too.
            Platform::Android => (48.0, "dp"),
            // Apple HIG, Accessibility: a hit target at least 44x44 pt.
            Platform::Ios => (44.0, "pt"),
            // WCAG 2.2 SC 2.5.8 Target Size (Minimum), level AA.
            Platform::Web => (24.0, "px"),
        }
    }

    fn target_size(&mut self, el: &Element, role: &str, parent: Option<&Element>) {
        if !self.shown_target(el) {
            return;
        }
        // System-drawn controls whose size the app doesn't set: a UISwitch
        // is 51x31 pt, a native checkbox or radio 13x13 px (2.5.8's "user
        // agent control" exception).
        match (self.platform, role) {
            (Platform::Ios, "switch") => return,
            (Platform::Web, "checkbox" | "radio") if el.element_type == "input" => return,
            _ => {}
        }
        // 2.5.8's inline exception: a link in a sentence is sized by the
        // line, so a link whose container says more than it does is left out.
        if role == "link" {
            let own = el.text.as_deref().map_or(0, |t| normalize(t).len());
            let around = parent
                .and_then(|p| p.text.as_deref())
                .map_or(0, |t| normalize(t).len());
            if around > own {
                return;
            }
        }
        let (min, unit) = self.minimum();
        let w = (f64::from(el.bounds.width) / self.density).round();
        let h = (f64::from(el.bounds.height) / self.density).round();
        if w >= min && h >= min {
            return;
        }
        if self.platform == Platform::Web && self.spaced(el, min) {
            return;
        }
        let fix = match self.platform {
            Platform::Android => "make it at least 48x48 dp (minWidth/minHeight, padding, or a TouchDelegate)",
            Platform::Ios => "make its hit area at least 44x44 pt",
            Platform::Web => "make it at least 24x24 CSS px, or keep a 24 px circle around it clear of other targets",
        };
        self.out.push(Finding::new(
            rule::TARGET_SIZE,
            Severity::Warning,
            el,
            format!("the touch target is {w}x{h} {unit}, under {min}x{min} {unit}"),
            fix,
        ));
    }

    /// 2.5.8's spacing exception: an undersized target passes when a circle
    /// of the minimum's diameter on its centre meets no other target nor
    /// another undersized target's circle.
    fn spaced(&self, el: &Element, min: f64) -> bool {
        let r = min / 2.0;
        let centre = |b: &Rect| {
            (
                f64::from(b.x) + f64::from(b.width) / 2.0,
                f64::from(b.y) + f64::from(b.height) / 2.0,
            )
        };
        let (cx, cy) = centre(&el.bounds);
        self.targets
            .iter()
            .filter(|t| !std::ptr::eq(**t, el) && t.bounds != el.bounds)
            .all(|t| {
                let b = t.bounds;
                let small = f64::from(b.width) < min || f64::from(b.height) < min;
                if small {
                    let (tx, ty) = centre(&b);
                    (tx - cx).hypot(ty - cy) >= 2.0 * r
                } else {
                    let nx = cx.clamp(f64::from(b.x), f64::from(b.x) + f64::from(b.width));
                    let ny = cy.clamp(f64::from(b.y), f64::from(b.y) + f64::from(b.height));
                    (nx - cx).hypot(ny - cy) >= r
                }
            })
    }

    /// `name-duplicate`: interactive siblings with one name. A screen
    /// reader's list of controls then shows the same entry twice with
    /// nothing to tell them apart.
    fn duplicates(&mut self, el: &Element) {
        let mut groups: Vec<(String, Vec<&Element>)> = Vec::new();
        for c in &el.children {
            if !c.visible || is_system(c) || interactive(c).is_none() {
                continue;
            }
            let Some(name) = accessible_name(c) else {
                continue;
            };
            let key = name.to_lowercase();
            match groups.iter_mut().find(|(k, _)| *k == key) {
                // The same control reported twice (same box) isn't two.
                Some((_, members)) if members.iter().any(|m| m.bounds == c.bounds) => {}
                Some((_, members)) => members.push(c),
                None => groups.push((key, vec![c])),
            }
        }
        for (_, members) in groups.into_iter().filter(|(_, m)| m.len() > 1) {
            let first = members[0];
            let name = accessible_name(first).unwrap_or_default();
            self.out.push(Finding::new(
                rule::NAME_DUPLICATE,
                Severity::Warning,
                first,
                format!(
                    "{} controls in one container are all named {name:?}",
                    members.len()
                ),
                "give each a name that says what it acts on (\"Delete Ada\", not \"Delete\")",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn el(ty: &str, b: Rect) -> Element {
        Element {
            element_type: ty.into(),
            bounds: b,
            enabled: true,
            visible: true,
            ..Default::default()
        }
    }

    fn named(ty: &str, b: Rect, label: &str) -> Element {
        Element {
            label: Some(label.into()),
            ..el(ty, b)
        }
    }

    fn root(ty: &str, screen: Rect, children: Vec<Element>) -> Element {
        Element {
            children,
            ..el(ty, screen)
        }
    }

    fn rules(f: &[Finding]) -> Vec<&'static str> {
        f.iter().map(|f| f.rule).collect()
    }

    const PHONE: Rect = Rect {
        x: 0,
        y: 0,
        width: 1080,
        height: 2400,
    };
    const IPHONE: Rect = Rect {
        x: 0,
        y: 0,
        width: 393,
        height: 852,
    };
    const PAGE: Rect = Rect {
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
    };

    #[test]
    fn an_unnamed_control_is_an_error_and_a_named_one_is_fine() {
        // Android: a Button with no text or description; one with text.
        let mut ok = el("android.widget.Button", rect(0, 0, 300, 150));
        ok.text = Some("Pay".into());
        let tree = root(
            "hierarchy",
            PHONE,
            vec![el("android.widget.Button", rect(0, 200, 300, 150)), ok],
        );
        let f = audit(&tree, Platform::Android, None);
        assert_eq!(rules(&f), [rule::NAME_MISSING], "{f:#?}");
        assert_eq!(f[0].severity, Severity::Error);
        assert!(
            f[0].line().contains("contentDescription"),
            "{}",
            f[0].line()
        );
        // iOS: a Button with no label.
        let tree = root(
            "Application",
            IPHONE,
            vec![
                el("Button", rect(0, 0, 100, 44)),
                named("Button", rect(0, 50, 100, 44), "Done"),
            ],
        );
        assert_eq!(
            rules(&audit(&tree, Platform::Ios, None)),
            [rule::NAME_MISSING]
        );
        // Web: a role=button div with no text; a button named by its content.
        let mut b = Element {
            role: Some("button".into()),
            ..el("div", rect(0, 0, 100, 40))
        };
        let mut content = el("span", rect(0, 0, 100, 40));
        content.text = Some("Save".into());
        b.children.push(content);
        let bare = Element {
            role: Some("button".into()),
            ..el("div", rect(0, 50, 100, 40))
        };
        let tree = root("html", PAGE, vec![b, bare]);
        assert_eq!(
            rules(&audit(&tree, Platform::Web, None)),
            [rule::NAME_MISSING]
        );
    }

    #[test]
    fn an_icon_button_without_a_label_is_image_unlabeled() {
        let tree = root(
            "hierarchy",
            PHONE,
            vec![
                el("android.widget.ImageButton", rect(0, 0, 144, 144)),
                named("android.widget.ImageButton", rect(200, 0, 144, 144), "Menu"),
            ],
        );
        let f = audit(&tree, Platform::Android, None);
        assert_eq!(rules(&f), [rule::IMAGE_UNLABELED], "{f:#?}");
        // iOS: a Button whose only content is an image.
        let mut b = el("Button", rect(0, 0, 44, 44));
        b.children.push(el("Image", rect(10, 10, 24, 24)));
        let tree = root("Application", IPHONE, vec![b]);
        assert_eq!(
            rules(&audit(&tree, Platform::Ios, None)),
            [rule::IMAGE_UNLABELED]
        );
        // Web: a link around an <img> with no alt.
        let mut a = Element {
            role: Some("link".into()),
            ..el("a", rect(0, 0, 48, 48))
        };
        a.children.push(Element {
            role: Some("img".into()),
            ..el("img", rect(0, 0, 48, 48))
        });
        let tree = root("html", PAGE, vec![a]);
        let f = audit(&tree, Platform::Web, None);
        assert_eq!(rules(&f), [rule::IMAGE_UNLABELED]);
        assert!(f[0].fix.contains("alt"), "{}", f[0].fix);
    }

    #[test]
    fn small_targets_follow_each_platforms_minimum() {
        // Android at 2.625 px/dp: 126 px is 48 dp, 105 px is 40 dp.
        let tree = root(
            "hierarchy",
            PHONE,
            vec![
                named("android.widget.ImageButton", rect(0, 0, 126, 126), "Back"),
                named(
                    "android.widget.ImageButton",
                    rect(200, 0, 105, 105),
                    "Close",
                ),
            ],
        );
        let opts = Options {
            platform: Platform::Android,
            screen: None,
            density: Some(2.625),
        };
        let f = audit_with(&tree, &opts);
        assert_eq!(rules(&f), [rule::TARGET_SIZE], "{f:#?}");
        assert!(f[0].message.contains("40x40 dp"), "{}", f[0].message);
        // No density: 1080 px wide reads as 411 dp, so a 48 dp target
        // (126 px) still passes.
        assert_eq!(
            rules(&audit(&tree, Platform::Android, None)),
            [rule::TARGET_SIZE]
        );
        // iOS: 44 pt, and a UISwitch's fixed 51x31 is left alone.
        let tree = root(
            "Application",
            IPHONE,
            vec![
                named("Button", rect(0, 0, 44, 44), "Back"),
                named("Button", rect(100, 0, 30, 30), "Info"),
                named("Switch", rect(200, 0, 51, 31), "Wi-Fi"),
            ],
        );
        let f = audit(&tree, Platform::Ios, None);
        assert_eq!(rules(&f), [rule::TARGET_SIZE], "{f:#?}");
        assert!(f[0].line().contains("\"Info\""), "{}", f[0].line());
        // Off screen, hidden or disabled: not a target here.
        let mut hidden = named("Button", rect(0, 100, 20, 20), "x");
        hidden.visible = false;
        let mut off = named("Button", rect(0, 2000, 20, 20), "y");
        off.visible_bounds = Some(rect(0, 0, 0, 0));
        let tree = root(
            "Application",
            IPHONE,
            vec![hidden, off, named("Button", rect(500, 0, 20, 20), "z")],
        );
        assert!(audit(&tree, Platform::Ios, None).is_empty());
    }

    #[test]
    fn web_targets_get_wcag_exceptions() {
        let button = |x: i32, w: i32, name: &str| Element {
            role: Some("button".into()),
            ..named("button", rect(x, 0, w, 20), name)
        };
        // Two 20 px buttons side by side: their circles overlap.
        let tree = root("html", PAGE, vec![button(0, 20, "a"), button(22, 20, "b")]);
        assert_eq!(
            rules(&audit(&tree, Platform::Web, None)),
            [rule::TARGET_SIZE, rule::TARGET_SIZE]
        );
        // One alone, 24 px clear of anything: spacing exception.
        let tree = root("html", PAGE, vec![button(0, 20, "a"), button(200, 20, "b")]);
        assert!(audit(&tree, Platform::Web, None).is_empty());
        // A native checkbox (user agent control) and an inline link.
        let check = Element {
            role: Some("checkbox".into()),
            ..named("input", rect(0, 100, 13, 13), "Remember")
        };
        let mut p = el("p", rect(0, 200, 600, 18));
        p.text = Some("Read the terms before you sign.".into());
        p.children.push(Element {
            role: Some("link".into()),
            text: Some("terms".into()),
            ..el("a", rect(60, 200, 40, 18))
        });
        let near = button(30, 20, "c");
        let tree = root("html", PAGE, vec![check, p, near]);
        assert!(audit(&tree, Platform::Web, None).is_empty());
    }

    #[test]
    fn duplicate_names_among_siblings_are_one_warning() {
        let row = |y: i32| {
            let mut r = el("android.widget.LinearLayout", rect(0, y, 1080, 200));
            r.children.push(named(
                "android.widget.Button",
                rect(800, y, 200, 150),
                "Delete",
            ));
            r
        };
        // One "Delete" per row: different containers, fine.
        let tree = root("hierarchy", PHONE, vec![row(0), row(300)]);
        assert!(audit(&tree, Platform::Android, None).is_empty());
        // Two in one container: ambiguous.
        let mut bar = el("android.widget.LinearLayout", rect(0, 0, 1080, 200));
        bar.children.push(named(
            "android.widget.Button",
            rect(0, 0, 200, 150),
            "Delete",
        ));
        bar.children.push(named(
            "android.widget.Button",
            rect(300, 0, 200, 150),
            "delete",
        ));
        let tree = root("hierarchy", PHONE, vec![bar]);
        let f = audit(&tree, Platform::Android, None);
        assert_eq!(rules(&f), [rule::NAME_DUPLICATE], "{f:#?}");
        assert!(f[0].message.starts_with("2 controls"), "{}", f[0].message);
        // iOS and web the same way.
        let tree = root(
            "Application",
            IPHONE,
            vec![
                named("Button", rect(0, 0, 60, 44), "Edit"),
                named("Button", rect(100, 0, 60, 44), "Edit"),
            ],
        );
        assert_eq!(
            rules(&audit(&tree, Platform::Ios, None)),
            [rule::NAME_DUPLICATE]
        );
        let link = |x: i32| Element {
            role: Some("link".into()),
            ..named("a", rect(x, 0, 100, 30), "More")
        };
        let tree = root("html", PAGE, vec![link(0), link(200)]);
        assert_eq!(
            rules(&audit(&tree, Platform::Web, None)),
            [rule::NAME_DUPLICATE]
        );
    }

    #[test]
    fn fields_need_a_label_and_a_placeholder_is_weak() {
        let field = |ty: &str, hint: Option<&str>, y: i32| Element {
            hint: hint.map(str::to_string),
            text: Some("typed".into()),
            ..el(ty, rect(0, y, 300, 150))
        };
        // Android: a hint labels an EditText; nothing at all doesn't.
        let tree = root(
            "hierarchy",
            PHONE,
            vec![
                field("android.widget.EditText", Some("Email"), 0),
                field("android.widget.EditText", None, 200),
            ],
        );
        let f = audit(&tree, Platform::Android, None);
        assert_eq!(rules(&f), [rule::FIELD_UNLABELED], "{f:#?}");
        // iOS: a placeholder alone is a warning; a label is fine.
        let tree = root(
            "Application",
            IPHONE,
            vec![
                field("TextField", Some("Email"), 0),
                field("TextField", None, 200),
                Element {
                    label: Some("Name".into()),
                    ..field("TextField", Some("Ada"), 400)
                },
            ],
        );
        let f = audit(&tree, Platform::Ios, None);
        assert_eq!(
            rules(&f),
            [rule::FIELD_PLACEHOLDER_ONLY, rule::FIELD_UNLABELED]
        );
        assert_eq!(f[0].severity, Severity::Warning);
        // Web: a field inside a <label> is labelled by it.
        let input = |hint: Option<&str>, y: i32| Element {
            role: Some("textbox".into()),
            ..field("input", hint, y)
        };
        let mut label = el("label", rect(0, 400, 300, 40));
        label.text = Some("Name".into());
        label.children.push(input(None, 400));
        let tree = root(
            "html",
            PAGE,
            vec![input(Some("Email"), 0), input(None, 200), label],
        );
        assert_eq!(
            rules(&audit(&tree, Platform::Web, None)),
            [rule::FIELD_PLACEHOLDER_ONLY, rule::FIELD_UNLABELED]
        );
    }

    #[test]
    fn system_ui_and_plain_views_are_left_out() {
        // The iOS keyboard's keys, a role-less clickable View (Compose
        // without a Role: nothing says it acts), static text.
        let mut kb = el("Keyboard", rect(0, 500, 393, 300));
        kb.children.push(el("Key", rect(0, 500, 30, 40)));
        kb.children.push(el("Button", rect(40, 500, 30, 40)));
        let tree = root(
            "Application",
            IPHONE,
            vec![
                kb,
                el("Other", rect(0, 0, 10, 10)),
                el("StaticText", rect(0, 20, 10, 10)),
            ],
        );
        assert!(audit(&tree, Platform::Ios, None).is_empty());
        let tree = root(
            "hierarchy",
            PHONE,
            vec![el("android.view.View", rect(0, 0, 10, 10))],
        );
        assert!(audit(&tree, Platform::Android, None).is_empty());
    }

    #[test]
    fn keys_stay_the_same_across_screens_and_lines_read_well() {
        let a = root(
            "hierarchy",
            PHONE,
            vec![Element {
                platform_id: "com.app:id/menu".into(),
                ..el("android.widget.ImageButton", rect(0, 0, 144, 144))
            }],
        );
        let mut b = a.clone();
        b.children.push(named(
            "android.widget.Button",
            rect(0, 300, 300, 150),
            "Next",
        ));
        let (fa, fb) = (
            audit(&a, Platform::Android, None),
            audit(&b, Platform::Android, None),
        );
        assert_eq!(fa[0].key(), fb[0].key());
        assert_eq!(
            fa[0].line(),
            "error image-unlabeled: ImageButton #com.app:id/menu at 0,0 144x144: \
             an image-only button has no label. Fix: set a contentDescription saying \
             what it does (Compose: on the Icon or the clickable)"
        );
        assert!(fa[0].element.children.is_empty());
    }
}
