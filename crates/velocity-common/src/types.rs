use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A selector used to find UI elements on the screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Selector {
    Id(String),
    Text(String),
    TextContains(String),
    /// A regular expression the whole text, label or hint must match,
    /// ignoring case, `.` matching newlines (Maestro's `text:`, D-11).
    TextMatches(String),
    /// A regular expression the whole id (or its short form after `:id/`)
    /// must match, ignoring case (Maestro's `id:`, D-11).
    IdMatches(String),
    AccessibilityId(String),
    ClassName(String),
    Index {
        selector: Box<Selector>,
        index: usize,
    },
    Compound(Vec<Selector>),
    /// CSS selector (web only). E.g. `".btn-primary"` or `"#submit"`.
    Css(String),
    /// XPath expression (web only). E.g. `"//button[text()='Submit']"`.
    XPath(String),
    /// `target` positioned relative to `anchor` (`below`, `rightOf`,
    /// `containsChild`, …). Candidates are ranked by distance to the anchor.
    Relative {
        target: Box<Selector>,
        relation: Relation,
        anchor: Box<Selector>,
    },
    /// The element's role, by Playwright's (ARIA) role names: `button`,
    /// `checkbox`, `textbox`, `link`, `heading`, … The web computes it
    /// (explicit `role=` or the implicit role of the tag); the devices map
    /// their class or element type to it conservatively ([`crate::role`]).
    Role(String),
    /// The accessible name, as a screen reader announces it: the label
    /// (content description, accessibility label, `aria-label`, a field's
    /// `<label>`), else the text, else for a content-named role (a button,
    /// a link, a tab …) the text of what is inside. Compared like `text:`.
    Name(String),
    /// State filter: a checkbox / switch / radio is (not) checked. Only a
    /// checkable element has a checked state, so `checked: false` never
    /// matches a plain button.
    Checked(bool),
    /// State filter: the element has (no) input focus.
    Focused(bool),
    /// State filter: the element is (not) selected (a tab, a list row, an
    /// `<option>`, Android `isSelected`, iOS `isSelected`).
    Selected(bool),
    /// State filter: the element is (not) enabled.
    Enabled(bool),
}

impl Selector {
    /// A state filter: narrows another selector, never finds on its own.
    pub fn is_state_filter(&self) -> bool {
        matches!(
            self,
            Self::Checked(_) | Self::Focused(_) | Self::Selected(_) | Self::Enabled(_)
        )
    }
}

/// How a relative selector's target relates to its anchor.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Relation {
    Below,
    Above,
    LeftOf,
    RightOf,
    /// The target has a descendant matching the anchor.
    ContainsChild,
    /// The target is inside an element matching the anchor.
    ChildOf,
}

impl std::fmt::Display for Relation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Below => "below",
            Self::Above => "above",
            Self::LeftOf => "leftOf",
            Self::RightOf => "rightOf",
            Self::ContainsChild => "containsChild",
            Self::ChildOf => "childOf",
        })
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(id) => write!(f, "id={id:?}"),
            Self::Text(text) => write!(f, "text={text:?}"),
            Self::TextContains(sub) => write!(f, "textContains={sub:?}"),
            Self::TextMatches(re) => write!(f, "textMatches={re:?}"),
            Self::IdMatches(re) => write!(f, "idMatches={re:?}"),
            Self::AccessibilityId(aid) => write!(f, "accessibilityId={aid:?}"),
            Self::ClassName(cls) => write!(f, "className={cls:?}"),
            Self::Index { selector, index } => write!(f, "{selector}[{index}]"),
            Self::Compound(selectors) => {
                let parts: Vec<String> = selectors.iter().map(|s| s.to_string()).collect();
                write!(f, "compound({})", parts.join(" AND "))
            }
            Self::Css(q) => write!(f, "css={q:?}"),
            Self::XPath(q) => write!(f, "xpath={q:?}"),
            Self::Relative {
                target,
                relation,
                anchor,
            } => write!(f, "{target} {relation}({anchor})"),
            Self::Role(r) => write!(f, "role={r:?}"),
            Self::Name(n) => write!(f, "name={n:?}"),
            Self::Checked(b) => write!(f, "checked={b}"),
            Self::Focused(b) => write!(f, "focused={b}"),
            Self::Selected(b) => write!(f, "selected={b}"),
            Self::Enabled(b) => write!(f, "enabled={b}"),
        }
    }
}

/// A rectangular region on the screen.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// The middle point. Computed wide and clamped, so hostile bounds from a
    /// device or page can't overflow (VEL-231).
    pub fn center(&self) -> (i32, i32) {
        let mid = |o: i32, len: i32| {
            (i64::from(o) + i64::from(len) / 2).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
                as i32
        };
        (mid(self.x, self.width), mid(self.y, self.height))
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        let end = |o: i32, len: i32| i64::from(o) + i64::from(len);
        i64::from(self.x) < end(other.x, other.width)
            && end(self.x, self.width) > i64::from(other.x)
            && i64::from(self.y) < end(other.y, other.height)
            && end(self.y, self.height) > i64::from(other.y)
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

/// A UI element from the accessibility tree.
///
/// Build new literals with `..Default::default()`, so a field added later
/// doesn't break every call site (D-39).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Element {
    pub platform_id: String,
    pub label: Option<String>,
    pub text: Option<String>,
    pub element_type: String,
    pub bounds: Rect,
    pub enabled: bool,
    pub visible: bool,
    pub children: Vec<Element>,
    /// A text field's placeholder (Android `hint`): how an empty Flutter or
    /// Compose field is labelled. Matching uses it only as a fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether the element has input focus (Android `focused`, iOS
    /// `hasKeyboardFocus`, Flutter `isFocused`, the web's
    /// `document.activeElement`). `None` when the driver can't tell
    /// (headless): then `focused: true` and `focused: false` both fail to
    /// match it, as `checked` does for an element that isn't checkable.
    /// Only a `Some(true)` somewhere in a tree is a focus signal (D-5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    /// A password / secure text field: its text is masked, so a check that
    /// reads it back can't see what was typed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub obscured: bool,
    /// Checkbox / switch / radio state: `Some` only for a checkable element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// The on-screen part of `bounds` when the driver reports it (iOS
    /// `visibleFrame`, Android `getBoundsInScreen` vs. its clipped rect,
    /// the web's viewport-clipped rect). `None`: not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_bounds: Option<Rect>,
    /// Whether the element is selected (a tab, a list row, an option):
    /// Android `selected`, iOS `selected`, Flutter `isSelected`, the web's
    /// `aria-selected` or an `<option>`'s selectedness. `None` when the
    /// driver can't tell (headless): then `selected: true` and
    /// `selected: false` both fail to match it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    /// The role the driver read, by Playwright's role names: the web's
    /// computed ARIA role, an Android role description that names one.
    /// `None`: derive it from `element_type` ([`crate::role::role_of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The web element's DOM `id` attribute (`platform_id` is a per-load
    /// node id there). Set by the web driver only: `id:` selectors match
    /// it, and it is the web's stable developer id. Until 2026-11-07 it
    /// was stored as `label`, so recorded tests typed `text: <id>`, which
    /// the page never matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom_id: Option<String>,
    /// The web element's `class` attribute (web driver only): `className:`
    /// matches its words, and an icon with nothing else to go by is
    /// selected by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub css_class: Option<String>,
    /// The web element's test id (web driver only): its `data-testid`
    /// (RN-web `testID`) or Flutter web's `flt-semantics-identifier`.
    /// `id:` selectors match it, as the page's own `id:` query does, so a
    /// recorded `id:` is checked for uniqueness against the same set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_id: Option<String>,
    /// An indeterminate progress indicator (Android: a `ProgressBar` with
    /// no range, reported by the tree server as `indeterminate="true"`):
    /// a spinner that turns until the work behind it ends. Settle waits
    /// on it, within its cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub indeterminate: bool,
}

/// Where the input focus is, relative to an `inputText` target (D-5).
#[derive(Debug, Clone, Copy)]
pub enum FocusCheck<'a> {
    /// No element in the tree reports focus: the driver can't tell (or
    /// nothing has focus), so the check doesn't apply.
    Unknown,
    /// A focused element overlaps the target (the field itself, its
    /// wrapper, or a label drawn inside it).
    OnTarget,
    /// Focus is on something that doesn't overlap the target: typed keys
    /// would go there (VEL-S46).
    Elsewhere(&'a Element),
}

impl Element {
    /// This element without its subtree: a shallow copy, for a flat list
    /// (`el.clone()` then dropping `children` copied the whole subtree
    /// first, O(n · depth) over a tree; review 04 #1).
    pub fn without_children(&self) -> Element {
        Element {
            platform_id: self.platform_id.clone(),
            label: self.label.clone(),
            text: self.text.clone(),
            element_type: self.element_type.clone(),
            bounds: self.bounds,
            enabled: self.enabled,
            visible: self.visible,
            children: Vec::new(),
            hint: self.hint.clone(),
            focused: self.focused,
            obscured: self.obscured,
            checked: self.checked,
            visible_bounds: self.visible_bounds,
            selected: self.selected,
            role: self.role.clone(),
            dom_id: self.dom_id.clone(),
            css_class: self.css_class.clone(),
            test_id: self.test_id.clone(),
            indeterminate: self.indeterminate,
        }
    }

    /// Every element in this tree that reports input focus, pre-order.
    pub fn focused_elements(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        let mut stack = vec![self];
        while let Some(e) = stack.pop() {
            if e.focused == Some(true) {
                out.push(e);
            }
            stack.extend(e.children.iter().rev());
        }
        out
    }

    /// Whether this tree's focus is on `target` (D-5), with no record of
    /// the focus before the tap: [`Self::focus_check_after_tap`].
    pub fn focus_check(&self, target: &Element) -> FocusCheck<'_> {
        self.focus_check_after_tap(target, None)
    }

    /// Whether this tree (read after `inputText`'s tap) has its focus on
    /// `target`, the element located before the tap (D-5). `before` is the
    /// tree that lookup read, when the driver still has it.
    ///
    /// A field often moves when it takes focus (an iOS search bar slides
    /// into the navigation bar, a form scrolls the field above the
    /// keyboard, a fake search bar opens a screen with the real field), so
    /// the target's old rectangle alone can't judge it (review BLOCKER 2).
    /// Passes when a focused element overlaps the target, or a focused text
    /// field is the target re-resolved by type and id/label, or one took
    /// focus with the tap (it wasn't focused before) and is not clearly
    /// another field. Fails only when the focus is not on a text field at
    /// all, when the focused field already had the focus before the tap
    /// (the tap didn't move it: VEL-S46), or when the target is itself a
    /// text field and the focused one is a different, identified field
    /// that was already on screen. A target without a size can't be
    /// judged, so it counts as focused.
    pub fn focus_check_after_tap(
        &self,
        target: &Element,
        before: Option<&Element>,
    ) -> FocusCheck<'_> {
        let focused = self.focused_elements();
        let Some(&first) = focused.first() else {
            return FocusCheck::Unknown;
        };
        if target.bounds.is_empty() || focused.iter().any(|f| f.bounds.intersects(&target.bounds)) {
            return FocusCheck::OnTarget;
        }
        let editable: Vec<&Element> = focused.iter().copied().filter(|f| is_editable(f)).collect();
        let Some(&field) = editable.first() else {
            // Focus on something that takes no text.
            return FocusCheck::Elsewhere(first);
        };
        // The target itself, moved by the tap.
        if editable.iter().any(|f| same_field(f, target)) {
            return FocusCheck::OnTarget;
        }
        let target_editable = holds_editable(target);
        // Another field with its own id/label than the target's.
        let other_field = |f: &Element| {
            target_editable
                && identity(f).is_some()
                && identity(target).is_some()
                && identity(f) != identity(target)
        };
        match before {
            Some(before) => {
                let before_focused = before.focused_elements();
                // The same field had the focus before: same box, or, when
                // it has an id or label, the same field moved by the tap
                // (a header collapsing under it). It counted as "took the
                // focus with the tap" once it moved (review MINOR 11).
                let was_focused = |f: &Element| {
                    before_focused.iter().any(|b| {
                        b.element_type == f.element_type
                            && b.platform_id == f.platform_id
                            && (b.bounds == f.bounds || same_field(b, f))
                    })
                };
                if editable.iter().all(|f| was_focused(f)) {
                    // The tap didn't move the focus (VEL-S46).
                    return FocusCheck::Elsewhere(field);
                }
                let was_on_screen = |f: &Element| before.find_first(|b| same_field(b, f));
                match editable
                    .iter()
                    .find(|f| !was_focused(f) && !(other_field(f) && was_on_screen(f)))
                {
                    Some(_) => FocusCheck::OnTarget,
                    None => FocusCheck::Elsewhere(field),
                }
            }
            // Nothing to tell a field that took focus from one that kept it.
            None if !target_editable => FocusCheck::Elsewhere(field),
            None if editable.iter().all(|f| other_field(f)) => FocusCheck::Elsewhere(field),
            None => FocusCheck::OnTarget,
        }
    }

    /// Whether any element of this tree (this one included) satisfies `f`.
    fn find_first(&self, f: impl Fn(&Element) -> bool) -> bool {
        let mut stack = vec![self];
        while let Some(e) = stack.pop() {
            if f(e) {
                return true;
            }
            stack.extend(e.children.iter());
        }
        false
    }
}

/// A type that takes typed text: iOS `TextField` / `SecureTextField` /
/// `SearchField` / `TextView`, Android `EditText` and its subclasses
/// (`AutoCompleteTextView`, `SearchView$SearchAutoComplete`; Compose text
/// fields report `EditText`), the web's `input` / `textarea`.
pub fn is_editable_type(element_type: &str) -> bool {
    let short = element_type.rsplit('.').next().unwrap_or(element_type);
    matches!(
        short,
        "TextField" | "SecureTextField" | "SearchField" | "TextView" | "input" | "textarea"
    ) || short.ends_with("EditText")
        || short.contains("AutoComplete")
}

/// Whether `e` takes typed text: [`is_editable_type`], except that a
/// `TextView` is a field only on iOS (the iOS parser gives it the role
/// `textbox`); Android's is a static label (review MINOR 11).
pub fn is_editable(e: &Element) -> bool {
    let short = e.element_type.rsplit('.').next().unwrap_or(&e.element_type);
    if short == "TextView" {
        return e.role.as_deref() == Some("textbox");
    }
    is_editable_type(&e.element_type)
}

fn holds_editable(e: &Element) -> bool {
    is_editable(e) || e.children.iter().any(holds_editable)
}

/// What names a field across two reads: its id, else its label.
fn identity(e: &Element) -> Option<&str> {
    Some(e.platform_id.as_str())
        .filter(|s| !s.is_empty())
        .or(e.label.as_deref().filter(|s| !s.is_empty()))
}

/// The same field in two reads: same type and the same id/label.
fn same_field(a: &Element, b: &Element) -> bool {
    a.element_type == b.element_type && identity(a).is_some() && identity(a) == identity(b)
}

/// Swipe direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Hardware/soft keys that can be pressed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Key {
    /// Android's back button; iOS's navigation-bar back; history back on
    /// the web (D-9). Never "delete a character": that is `backspace`.
    Back,
    Home,
    Enter,
    VolumeUp,
    VolumeDown,
    /// Delete the character before the caret.
    Backspace,
    Tab,
    Escape,
    /// Forward delete: the character after the caret.
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

impl Key {
    /// Every key, in the order the docs list them.
    pub const ALL: [Key; 13] = [
        Key::Back,
        Key::Home,
        Key::Enter,
        Key::VolumeUp,
        Key::VolumeDown,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
        Key::Delete,
        Key::ArrowUp,
        Key::ArrowDown,
        Key::ArrowLeft,
        Key::ArrowRight,
    ];

    /// The key's name in YAML (`pressKey: { key: arrowUp }`).
    pub fn name(&self) -> &'static str {
        match self {
            Key::Back => "back",
            Key::Home => "home",
            Key::Enter => "enter",
            Key::VolumeUp => "volumeUp",
            Key::VolumeDown => "volumeDown",
            Key::Backspace => "backspace",
            Key::Tab => "tab",
            Key::Escape => "escape",
            Key::Delete => "delete",
            Key::ArrowUp => "arrowUp",
            Key::ArrowDown => "arrowDown",
            Key::ArrowLeft => "arrowLeft",
            Key::ArrowRight => "arrowRight",
        }
    }

    /// Whether `platform`'s device driver can send the key. iOS has no
    /// hardware Escape, forward Delete or arrow keys to press through
    /// WebDriverAgent's keyboard, and a browser has no volume buttons.
    pub fn supported_on(&self, platform: crate::Platform) -> bool {
        match platform {
            crate::Platform::Android => true,
            crate::Platform::Ios => !matches!(
                self,
                Key::Escape
                    | Key::Delete
                    | Key::ArrowUp
                    | Key::ArrowDown
                    | Key::ArrowLeft
                    | Key::ArrowRight
            ),
            crate::Platform::Web => !matches!(self, Key::VolumeUp | Key::VolumeDown),
        }
    }
}

/// How `assertScreenshot` decides that one pixel changed (D-V2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Comparator {
    /// Any channel (alpha included) differs by more than `tolerance`.
    #[default]
    Strict,
    /// The YIQ colour distance is above 0.1 of its maximum (pixelmatch's
    /// and Playwright's rule): forgiving of shading drift, strict on hue.
    Perceptual,
}

/// The part of the screen an `assertScreenshot` compares (VT-12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScreenshotCrop {
    /// The bounds of the element this selector finds.
    Element(Selector),
    /// A rectangle in screen coordinates (as `tapXY` and element bounds:
    /// points on iOS, pixels on Android, CSS px on web).
    Rect(Rect),
}

/// `launchApp` arguments (AUT-024), by name: Android intent extras, iOS
/// launch arguments (`-name value`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaunchArgs {
    pub inner: BTreeMap<String, LaunchArg>,
}

impl LaunchArgs {
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl std::fmt::Display for LaunchArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (name, value) in &self.inner {
            parts.push(format!("{name}={value}"));
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// One launch argument: name -> value. Bool is decoded before numbers
/// before string so YAML `true`/`3` keep their type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LaunchArg {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl std::fmt::Display for LaunchArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Str(s) => write!(f, "{s:?}"),
        }
    }
}

/// Validates a launch argument name: non-empty, `[A-Za-z0-9_.-]` only,
/// never starting with `-`. Returns an error message (not a `Result`) for a
/// parser/validator to wrap in a clear message.
pub fn validate_launch_arg_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("launch argument name must not be empty".into());
    }
    if name.starts_with('-') {
        return Err(format!(
            "launch argument name {name:?} must not start with '-'"
        ));
    }
    if !name
        .chars()
        .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '.' | '-'))
    {
        return Err(format!(
            "launch argument name {name:?} may only contain [A-Za-z0-9_.-]"
        ));
    }
    Ok(())
}

/// An action to perform on the device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    LaunchApp {
        app_id: String,
        clear_state: bool,
        #[serde(default, skip_serializing_if = "LaunchArgs::is_empty")]
        arguments: LaunchArgs,
    },
    StopApp {
        app_id: String,
    },
    /// Answer matching web requests from now until the test ends (web only).
    MockRequest {
        rule: MockRule,
    },
    /// Drop the mocks this test added (the suite's `web.mocks` stay).
    ClearMocks,
    /// Change settings for the rest of the test (`sync.*` timings and named
    /// settings such as `android.scroll`); restored when the test ends.
    SetConfig {
        values: std::collections::BTreeMap<String, String>,
    },
    /// Device state beyond the screen (OpenDroid's outcome checks): a media
    /// session playing (with a title), a phone call. Android only.
    AssertSystemState {
        media_playing: Option<bool>,
        media_title: Option<String>,
        in_call: Option<bool>,
    },
    Tap {
        selector: Selector,
    },
    DoubleTap {
        selector: Selector,
    },
    LongPress {
        selector: Selector,
        duration_ms: Option<u64>,
    },
    /// Press `from`, hold, move onto `to`, rest, lift: reordering a list
    /// (row 3.1, Maestro #1203).
    Drag {
        from: Selector,
        to: Selector,
        hold_ms: Option<u64>,
        move_ms: Option<u64>,
    },
    InputText {
        selector: Selector,
        text: String,
    },
    ClearText {
        selector: Selector,
    },
    AssertVisible {
        selector: Selector,
    },
    AssertNotVisible {
        selector: Selector,
    },
    AssertText {
        selector: Selector,
        expected: String,
    },
    ScrollUntilVisible {
        selector: Selector,
        direction: Direction,
        max_scrolls: u32,
        /// The scrolling container to drag (a list beside another pane, a
        /// carousel); the middle of the screen when absent.
        #[serde(default)]
        within: Option<Selector>,
    },
    Swipe {
        direction: Option<Direction>,
        from: Option<(i32, i32)>,
        to: Option<(i32, i32)>,
        /// Swipe across this element instead of the middle of the screen.
        #[serde(default)]
        within: Option<Selector>,
    },
    Screenshot {
        filename: Option<String>,
    },
    /// Save the app's data (login, caches, preferences) as `name`, so later
    /// tests can start from it instead of repeating the steps that made it.
    SaveState {
        name: String,
    },
    /// Put back the app data saved as `name` (the app is stopped; launch it
    /// next).
    RestoreState {
        name: String,
    },
    /// Compare the screen with the stored baseline `name`.
    AssertScreenshot {
        name: String,
        /// Minimum share of compared pixels that must match, 0..1 (default
        /// 0.98). Not a colour tolerance (Playwright's `threshold` is one):
        /// that is `tolerance`, or `comparator: perceptual`.
        threshold: Option<f64>,
        /// Smallest cluster of changed pixels that fails (default 20).
        cluster_px: Option<usize>,
        /// Elements to leave out of the comparison (a clock, an avatar).
        mask: Vec<Selector>,
        /// `strict` (each channel within `tolerance`): how a pixel counts as
        /// changed, default 16 of 255.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tolerance: Option<u8>,
        /// How a pixel counts as changed: `strict` (default) or
        /// `perceptual` (YIQ distance, threshold 0.1, as pixelmatch and
        /// Playwright).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comparator: Option<Comparator>,
        /// Compare only this part of the screen: an element or a rect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crop: Option<ScreenshotCrop>,
    },
    PressKey {
        key: Key,
    },
    Wait {
        ms: u64,
    },
    /// `flow_id` names a flow from `flows:`; `env` are parameters bound for
    /// that call only (`runFlow: { flow: login, env: { USER: "qa" } }`).
    /// The shorthand `runFlow: login` has an empty env.
    RunFlow {
        flow_id: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Tap at absolute screen coordinates (for WebView / non-accessible UI).
    TapXY {
        x: i32,
        y: i32,
    },
    /// Type text via ADB input (for WebView fields not in accessibility tree).
    InputRaw {
        text: String,
    },
    /// Navigate the browser to an absolute URL (web only).
    Goto {
        url: String,
    },
    /// Navigate the browser back (web only).
    GoBack,
    /// Reload the current browser page (web only).
    Refresh,
    /// Send an HTTP request, check its response and capture values from it
    /// for `${NAME}` in later steps (`velocity-api` runs it). Needs no
    /// device: a suite of only these runs without one.
    Http(Box<crate::http::HttpRequest>),
    /// Run an allowed program on the host (`exec`, row 3.2).
    Exec(Box<crate::exec::ExecStep>),
    /// A natural-language step (e.g. "tap the login button") resolved to a
    /// concrete action at run time by an LLM planner (see the `velocity-nl`
    /// crate). The first resolution is cached against the screen fingerprint so
    /// reruns replay instantly without another model call.
    Natural {
        /// Plain-English instruction or assertion.
        intent: String,
        /// Whether this drives the UI (`Act`) or checks it (`Assert`).
        kind: NaturalKind,
        /// Optional parameter value to substitute (passmark-style `data`).
        data: Option<String>,
        /// `data` as written before `${VAR}` substitution; the cache keys on
        /// it so a new value doesn't re-plan. Set by env interpolation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_name: Option<String>,
        /// Optional plain-English condition to wait for after acting.
        wait_until: Option<String>,
        /// `intent` as written before `${VAR}` substitution, when it had
        /// any. The cache keys on it, so a new value replays the same plan.
        /// Set by env interpolation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intent_template: Option<String>,
    },
}

/// Whether a natural-language step performs an action or an assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NaturalKind {
    /// Drive the UI (tap, type, scroll, navigate, …).
    Act,
    /// Verify a condition about the current screen.
    Assert,
}

/// The stand-in "device" of a run whose steps are all `http`: `http-0`,
/// `http-1`, … (one per `--parallel` slot).
pub fn no_device_id(slot: usize) -> String {
    format!("http-{slot}")
}

/// Whether `id` is one of those stand-ins: nothing on the machine to share,
/// so never leased, prepared or installed on.
pub fn is_no_device(id: &str) -> bool {
    id.strip_prefix("http-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The target platform.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Ios,
    Android,
    Web,
}

impl Platform {
    /// The platform a device id belongs to, from its shape: a simulator
    /// UUID (8-4-4-4-12 hex) or an iPhone UDID (8-16 hex, or 40 hex) is iOS,
    /// `web` is web, anything else (`emulator-5554`, `host:port`, a USB
    /// serial) is Android.
    pub fn of_device_id(id: &str) -> Platform {
        let hex = |p: &str, n: usize| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit());
        let parts: Vec<&str> = id.split('-').collect();
        let apple = match parts.as_slice() {
            [a, b, c, d, e] => hex(a, 8) && hex(b, 4) && hex(c, 4) && hex(d, 4) && hex(e, 12),
            [a, b] => hex(a, 8) && hex(b, 16),
            [a] => hex(a, 40),
            _ => false,
        };
        match id {
            _ if apple => Platform::Ios,
            "web" => Platform::Web,
            _ => Platform::Android,
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ios => write!(f, "ios"),
            Self::Android => write!(f, "android"),
            Self::Web => write!(f, "web"),
        }
    }
}

/// Type of device (physical hardware vs virtual).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Physical,
    Simulator,
    Emulator,
    #[default]
    Unknown,
}

impl std::fmt::Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Physical => write!(f, "physical"),
            Self::Simulator => write!(f, "simulator"),
            Self::Emulator => write!(f, "emulator"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Information about a connected device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: Platform,
    pub state: DeviceState,
    pub os_version: Option<String>,
    #[serde(default)]
    pub device_type: DeviceType,
}

/// A settle measured on the device (see `PlatformDriver::settle_on_device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceSettle {
    /// Quiet for the whole quiet window before the cap.
    pub settled: bool,
    /// The screen changed after the action.
    pub reacted: bool,
    pub elapsed_ms: u64,
}

/// State of a device.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceState {
    Booted,
    Shutdown,
    Unknown,
}

impl std::fmt::Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Booted => write!(f, "booted"),
            Self::Shutdown => write!(f, "shutdown"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        glob_match, validate_launch_arg_name, Element, FocusCheck, Key, LaunchArg, LaunchArgs,
        MockRule, Platform, Rect,
    };

    /// `without_children` copies every field but the subtree: a field added
    /// to `Element` and forgotten there would show up here.
    #[test]
    fn without_children_keeps_every_field_but_the_subtree() {
        let r = Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let el = Element {
            platform_id: "p".into(),
            label: Some("l".into()),
            text: Some("t".into()),
            element_type: "Button".into(),
            bounds: r,
            enabled: true,
            visible: true,
            children: vec![Element::default()],
            hint: Some("h".into()),
            focused: Some(true),
            obscured: true,
            checked: Some(false),
            visible_bounds: Some(r),
            selected: Some(true),
            role: Some("button".into()),
            dom_id: Some("d".into()),
            css_class: Some("c".into()),
            test_id: Some("ti".into()),
            indeterminate: true,
        };
        let flat = el.without_children();
        assert!(flat.children.is_empty());
        let mut want = serde_json::to_value(&el).unwrap();
        want["children"] = serde_json::json!([]);
        assert_eq!(serde_json::to_value(&flat).unwrap(), want);
    }

    /// D-5: focus on the target or overlapping it passes; focus elsewhere
    /// is reported; a tree with no focus says nothing.
    #[test]
    fn focus_check_compares_the_focused_element_with_the_target() {
        let r = |y| Rect {
            x: 0,
            y,
            width: 100,
            height: 40,
        };
        let field = |y, focused| Element {
            bounds: r(y),
            focused: Some(focused),
            ..Default::default()
        };
        let tree = |a, b| Element {
            bounds: r(0),
            children: vec![field(100, a), field(200, b)],
            ..Default::default()
        };
        let target = field(200, false);
        assert!(matches!(
            tree(false, false).focus_check(&target),
            FocusCheck::Unknown
        ));
        assert!(matches!(
            tree(false, true).focus_check(&target),
            FocusCheck::OnTarget
        ));
        let t = tree(true, false);
        match t.focus_check(&target) {
            FocusCheck::Elsewhere(e) => assert_eq!(e.bounds.y, 100),
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(t.focus_check(&Element::default()), FocusCheck::OnTarget),
            "no bounds: not judged"
        );
    }

    fn el(t: &str, id: &str, label: &str, y: i32, focused: bool) -> Element {
        Element {
            element_type: t.into(),
            platform_id: id.into(),
            label: (!label.is_empty()).then(|| label.to_string()),
            bounds: Rect {
                x: 0,
                y,
                width: 300,
                height: 40,
            },
            focused: Some(focused),
            ..Default::default()
        }
    }

    fn screen(children: Vec<Element>) -> Element {
        Element {
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 900,
            },
            children,
            ..Default::default()
        }
    }

    /// Review BLOCKER 2: a field that moves when it takes focus passes.
    #[test]
    fn focus_on_a_field_that_moved_with_the_tap_passes() {
        // iOS UISearchController: the search field slides up into the
        // navigation bar (y 140 → 50) when it takes focus.
        let target = el("SearchField", "Search", "Search", 140, false);
        let before = screen(vec![target.clone(), el("Cell", "Row", "Row", 200, false)]);
        let after = screen(vec![el("SearchField", "Search", "Search", 50, true)]);
        assert!(matches!(
            after.focus_check_after_tap(&target, Some(&before)),
            FocusCheck::OnTarget
        ));
        assert!(matches!(after.focus_check(&target), FocusCheck::OnTarget));

        // Android adjustResize: the form scrolls the field above the
        // keyboard (y 700 → 300); with an id, and without one (Compose).
        for id in ["com.app:id/email", ""] {
            let target = el("EditText", id, "", 700, false);
            let before = screen(vec![el("EditText", "", "", 600, false), target.clone()]);
            let after = screen(vec![
                el("EditText", "", "", 200, false),
                el("EditText", id, "", 300, true),
                // The next field scrolled into the target's old place.
                el("EditText", "", "", 700, false),
            ]);
            assert!(
                matches!(
                    after.focus_check_after_tap(&target, Some(&before)),
                    FocusCheck::OnTarget
                ),
                "id {id:?}"
            );
        }

        // Play Store style: tapping a fake search bar (not a text field)
        // opens a screen whose real field takes the focus.
        let target = el("TextView", "", "Search apps", 120, false);
        let target = Element {
            element_type: "Button".into(),
            ..target
        };
        let before = screen(vec![target.clone()]);
        let after = screen(vec![el("EditText", "search_src_text", "", 40, true)]);
        assert!(matches!(
            after.focus_check_after_tap(&target, Some(&before)),
            FocusCheck::OnTarget
        ));
    }

    /// Review MINOR 11: the field focused before the tap keeps the focus
    /// but moves (a header collapses): that is focus that stayed, not focus
    /// the tap gave; and an Android `TextView` takes no text, while an iOS
    /// one (role `textbox`) does.
    #[test]
    fn a_moved_field_that_kept_the_focus_and_an_android_text_view_fail() {
        let a = el("EditText", "com.app:id/a", "", 300, true);
        let target = el("Button", "b", "B", 600, false);
        let before = screen(vec![a.clone(), target.clone()]);
        let after = screen(vec![
            el("EditText", "com.app:id/a", "", 180, true),
            el("Button", "b", "B", 480, false),
        ]);
        match after.focus_check_after_tap(&target, Some(&before)) {
            FocusCheck::Elsewhere(e) => assert_eq!(e.platform_id, "com.app:id/a"),
            other => panic!("{other:?}"),
        }
        // Android: the tap on a custom field (a Button-like view) left the
        // focus newly on a selectable TextView elsewhere: no text field.
        let custom = el("Button", "amount", "Amount", 300, false);
        let before = screen(vec![custom.clone(), el("TextView", "note", "", 600, false)]);
        let after = screen(vec![custom.clone(), el("TextView", "note", "", 600, true)]);
        assert!(matches!(
            after.focus_check_after_tap(&custom, Some(&before)),
            FocusCheck::Elsewhere(_)
        ));
        // iOS: the same TextView is an editable field.
        let ios_view = |focused| Element {
            role: Some("textbox".into()),
            ..el("TextView", "note", "", 600, focused)
        };
        let target = el("Button", "compose", "Compose", 100, false);
        let before = screen(vec![target.clone(), ios_view(false)]);
        let after = screen(vec![target.clone(), ios_view(true)]);
        assert!(matches!(
            after.focus_check_after_tap(&target, Some(&before)),
            FocusCheck::OnTarget
        ));
    }

    /// The checks D-5 is for still fail: focus that stayed on the field
    /// focused before the tap (VEL-S46), focus on something that takes no
    /// text, and focus on a different identified field already on screen.
    #[test]
    fn focus_that_stayed_or_went_to_another_field_still_fails() {
        // VEL-S46: the tap on a label didn't move the focus off "email".
        let email = el("EditText", "email", "", 100, true);
        let label = el("TextView", "", "Password", 300, false);
        let before = screen(vec![email.clone(), label.clone()]);
        let after = screen(vec![email.clone(), label.clone()]);
        match after.focus_check_after_tap(&label, Some(&before)) {
            FocusCheck::Elsewhere(e) => assert_eq!(e.platform_id, "email"),
            other => panic!("{other:?}"),
        }
        // Focus on a button: no text goes anywhere useful.
        let target = el("EditText", "pw", "", 300, false);
        let after = screen(vec![
            el("Button", "next", "Next", 600, true),
            target.clone(),
        ]);
        assert!(matches!(
            after.focus_check_after_tap(&target, Some(&before)),
            FocusCheck::Elsewhere(_)
        ));
        // Tapping "pw" focused "email", which was already on screen.
        let before = screen(vec![
            el("EditText", "email", "", 100, false),
            target.clone(),
        ]);
        let after = screen(vec![el("EditText", "email", "", 100, true), target.clone()]);
        match after.focus_check_after_tap(&target, Some(&before)) {
            FocusCheck::Elsewhere(e) => assert_eq!(e.platform_id, "email"),
            other => panic!("{other:?}"),
        }
    }

    /// D-39: the state fields default, stay out of JSON when unset (old
    /// traces and MCP output don't change), and round-trip when set.
    #[test]
    fn element_state_fields_default_and_round_trip() {
        let old = r#"{"platform_id":"a","label":null,"text":null,"element_type":"Button","bounds":{"x":0,"y":0,"width":1,"height":1},"enabled":true,"visible":true,"children":[]}"#;
        let e: Element = serde_json::from_str(old).unwrap();
        assert!(e.focused.is_none() && e.selected.is_none() && !e.obscured);
        assert_eq!((e.checked, e.visible_bounds), (None, None));
        let json = serde_json::to_string(&e).unwrap();
        for k in [
            "focused",
            "obscured",
            "checked",
            "visible_bounds",
            "selected",
        ] {
            assert!(!json.contains(k), "{k} in {json}");
        }
        let set = Element {
            focused: Some(true),
            selected: Some(false),
            obscured: true,
            checked: Some(false),
            visible_bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 5,
            }),
            ..Default::default()
        };
        let back: Element = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
        assert!(back.focused == Some(true) && back.obscured);
        assert_eq!(back.selected, Some(false), "a known false survives");
        assert_eq!(back.checked, Some(false));
        assert_eq!(back.visible_bounds.map(|r| r.width), Some(5));
    }

    /// D-9: the editing and arrow keys exist in YAML under the names the
    /// docs give, and `name()` is what serde reads.
    #[test]
    fn every_key_parses_by_its_yaml_name() {
        for key in Key::ALL {
            let parsed: Key = serde_json::from_value(serde_json::json!(key.name())).unwrap();
            assert_eq!(parsed, key);
        }
        for name in [
            "backspace",
            "tab",
            "escape",
            "delete",
            "arrowUp",
            "arrowLeft",
        ] {
            assert!(
                serde_json::from_value::<Key>(serde_json::json!(name)).is_ok(),
                "{name}"
            );
        }
        assert!(Key::Escape.supported_on(Platform::Android));
        assert!(!Key::ArrowUp.supported_on(Platform::Ios));
        assert!(Key::Backspace.supported_on(Platform::Ios));
        assert!(!Key::VolumeUp.supported_on(Platform::Web));
    }

    #[test]
    fn mock_rules_match_by_glob_or_substring_and_method() {
        assert!(glob_match("*/api/users*", "http://x.dev/api/users?page=2"));
        assert!(!glob_match("*/api/users", "http://x.dev/api/users/7"));
        assert!(
            !glob_match("https://?.dev/*", "https://a.dev/b"),
            "? is literal"
        );
        let r = MockRule {
            url: "/api/save".into(),
            method: Some("post".into()),
            ..Default::default()
        };
        assert!(r.matches("POST", "http://127.0.0.1/api/save"));
        assert!(!r.matches("GET", "http://127.0.0.1/api/save"));
    }

    /// VEL-076: `?` in a mock URL is the query string, not a wildcard. Any
    /// `?` switched to whole-URL glob matching with `?` as "one character",
    /// so `/api/data.json?v=1` never matched and the real backend answered.
    #[test]
    fn a_query_string_in_a_mock_url_matches() {
        let r = MockRule {
            url: "/api/data.json?v=1".into(),
            ..Default::default()
        };
        assert!(r.matches("GET", "http://127.0.0.1:8000/api/data.json?v=1"));
        assert!(!r.matches("GET", "http://127.0.0.1:8000/api/data.json?v=2"));
        let r = MockRule {
            url: "*/search?q=*".into(),
            ..Default::default()
        };
        assert!(r.matches("GET", "http://x.dev/search?q=shoes"));
        assert!(!r.matches("GET", "http://x.dev/searchXq=shoes"));
    }

    #[test]
    fn device_ids_name_their_platform() {
        for (id, p) in [
            ("290403AA-C40F-404B-9CB0-DAFB6A943142", Platform::Ios),
            ("00008110-001A2C3E0E12801E", Platform::Ios),
            ("0123456789abcdef0123456789abcdef01234567", Platform::Ios),
            ("emulator-5554", Platform::Android),
            ("192.168.1.5:5555", Platform::Android),
            ("farm-1:5037/emulator-5554", Platform::Android),
            ("R5CT10ABC1X", Platform::Android),
            ("web", Platform::Web),
        ] {
            assert_eq!(Platform::of_device_id(id), p, "{id}");
        }
    }

    #[test]
    fn launch_arg_names_reject_bad_and_accept_good() {
        for good in ["x", "a_b", "A.B", "isE2E", "server1", "a-b", "x.y-z"] {
            assert!(validate_launch_arg_name(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", "-a.b", "a b", "a!b", "a/b", "a@b", "a.b c"] {
            assert!(
                validate_launch_arg_name(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn launch_args_round_trip_preserve_types() {
        let args = LaunchArgs {
            inner: [
                ("isE2E".into(), LaunchArg::Bool(true)),
                ("retries".into(), LaunchArg::Int(3)),
                ("ratio".into(), LaunchArg::Float(1.5)),
                (
                    "server".into(),
                    LaunchArg::Str("https://staging.example.com".into()),
                ),
            ]
            .into(),
        };
        let json = serde_json::to_string(&args).unwrap();
        let back: LaunchArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(args, back);
        // Bool before number before string: YAML `true`/`3` keep their type.
        let y = serde_yaml::to_string(&args).unwrap();
        let back_y: LaunchArgs = serde_yaml::from_str(&y).unwrap();
        assert_eq!(args, back_y);
    }
}

/// A web request answered without the network (`web.mocks`, `mockRequest`)
/// or refused (`web.block`). `url` with `*`/`?` is a glob over the whole
/// URL; without, a substring of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockRule {
    pub url: String,
    /// Only this method (`GET`, `POST`, …); any when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Response status (default 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// A JSON body (sets `content-type: application/json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    /// A text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The body from a file (relative to the working directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Answer after this long (a slow backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Fail the request instead (as an ad blocker would).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub block: bool,
    /// Answer only the first N matching requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

impl MockRule {
    /// Whether this rule is for `method` `url`.
    pub fn matches(&self, method: &str, url: &str) -> bool {
        if let Some(m) = &self.method {
            if !m.eq_ignore_ascii_case(method) {
                return false;
            }
        }
        // Only `*` makes a glob: a `?` is a URL's query string (VEL-076).
        if self.url.contains('*') {
            glob_match(&self.url, url)
        } else {
            url.contains(&self.url)
        }
    }
}

/// `*` (any run) wildcards over the whole text; every other character,
/// `?` included, is literal (URLs are matched, and `?` starts a query).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// What `assertSystemState` reads from the device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemState {
    /// Some active media session is playing.
    pub media_playing: bool,
    /// Titles of the playing sessions.
    pub media_titles: Vec<String>,
    /// A call is ringing or in progress.
    pub in_call: bool,
}
