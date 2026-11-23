// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Roles and accessible names for `role:` / `name:` selectors (S1).
//!
//! Role names are Playwright's (the ARIA roles `getByRole` accepts). The web
//! computes an element's role in the page (`injected.js`: an explicit
//! `role=`, else the tag's implicit role) and sends it as
//! [`Element::role`]. The devices have no ARIA, so their class or element
//! type is mapped to a role, conservatively: only a type whose meaning is
//! unambiguous gets one, and anything else has no role (a `role:` selector
//! never matches it). The table is [`DEVICE_ROLES`]; the README prints it.

use crate::selector_match::normalize;
use crate::Element;

/// Every role name a `role:` selector accepts: Playwright's list (ARIA 1.2
/// roles, as `page.getByRole` takes them).
pub const ROLES: &[&str] = &[
    "alert",
    "alertdialog",
    "application",
    "article",
    "banner",
    "blockquote",
    "button",
    "caption",
    "cell",
    "checkbox",
    "code",
    "columnheader",
    "combobox",
    "complementary",
    "contentinfo",
    "definition",
    "deletion",
    "dialog",
    "directory",
    "document",
    "emphasis",
    "feed",
    "figure",
    "form",
    "generic",
    "grid",
    "gridcell",
    "group",
    "heading",
    "img",
    "insertion",
    "link",
    "list",
    "listbox",
    "listitem",
    "log",
    "main",
    "marquee",
    "math",
    "meter",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "navigation",
    "none",
    "note",
    "option",
    "paragraph",
    "presentation",
    "progressbar",
    "radio",
    "radiogroup",
    "region",
    "row",
    "rowgroup",
    "rowheader",
    "scrollbar",
    "search",
    "searchbox",
    "separator",
    "slider",
    "spinbutton",
    "status",
    "strong",
    "subscript",
    "superscript",
    "switch",
    "tab",
    "table",
    "tablist",
    "tabpanel",
    "term",
    "textbox",
    "time",
    "timer",
    "toolbar",
    "tooltip",
    "tree",
    "treegrid",
    "treeitem",
];

/// Whether `role` is one of [`ROLES`].
pub fn is_role(role: &str) -> bool {
    ROLES.contains(&role)
}

/// Device element types (short form: after the last `.` or `$`, without
/// `XCUIElementType`) and the role each maps to, for Android (framework,
/// AppCompat, Material and Compose's class names), headless Android and
/// UIKit layouts, and the Flutter bridge's semantics types. iOS's
/// `XCUIElementType*` names are in [`IOS_ROLES`], since a few short names
/// mean something else there (`TextView` is an editable field on iOS, a
/// static label on Android).
pub const DEVICE_ROLES: &[(&str, &str)] = &[
    // Android framework / AppCompat / Material; Compose reports these
    // class names for its `Role`s.
    ("Button", "button"),
    ("ImageButton", "button"),
    ("AppCompatButton", "button"),
    ("AppCompatImageButton", "button"),
    ("MaterialButton", "button"),
    ("FloatingActionButton", "button"),
    ("ExtendedFloatingActionButton", "button"),
    ("ToggleButton", "button"),
    ("CheckBox", "checkbox"),
    ("AppCompatCheckBox", "checkbox"),
    ("MaterialCheckBox", "checkbox"),
    ("Switch", "switch"),
    ("SwitchCompat", "switch"),
    ("SwitchMaterial", "switch"),
    ("MaterialSwitch", "switch"),
    ("RadioButton", "radio"),
    ("AppCompatRadioButton", "radio"),
    ("MaterialRadioButton", "radio"),
    ("RadioGroup", "radiogroup"),
    ("EditText", "textbox"),
    ("AppCompatEditText", "textbox"),
    ("TextInputEditText", "textbox"),
    ("AutoCompleteTextView", "textbox"),
    ("MultiAutoCompleteTextView", "textbox"),
    ("ImageView", "img"),
    ("AppCompatImageView", "img"),
    ("ProgressBar", "progressbar"),
    ("SeekBar", "slider"),
    ("AppCompatSeekBar", "slider"),
    ("Spinner", "combobox"),
    ("AppCompatSpinner", "combobox"),
    ("ListView", "list"),
    ("RecyclerView", "list"),
    ("GridView", "grid"),
    ("TabLayout", "tablist"),
    ("TabWidget", "tablist"),
    ("TabView", "tab"),
    // Headless UIKit layouts (storyboards, xibs).
    ("UIButton", "button"),
    ("UISwitch", "switch"),
    ("UITextField", "textbox"),
    ("UITextView", "textbox"),
    ("UISearchBar", "searchbox"),
    ("UIImageView", "img"),
    ("UISlider", "slider"),
    ("UIProgressView", "progressbar"),
    ("UIActivityIndicatorView", "progressbar"),
    ("UITableView", "list"),
    ("UICollectionView", "list"),
    ("UITableViewCell", "listitem"),
    ("UICollectionViewCell", "listitem"),
    ("UITabBar", "tablist"),
    ("UISegmentedControl", "tablist"),
    // Flutter bridge semantics types (`_role` in hierarchy_extractor.dart).
    ("TextField", "textbox"),
    ("Checkbox", "checkbox"),
    ("Radio", "radio"),
    ("Slider", "slider"),
    ("Link", "link"),
    ("Image", "img"),
    ("Header", "heading"),
];

/// iOS `XCUIElementType*` names (without the prefix) and their roles.
pub const IOS_ROLES: &[(&str, &str)] = &[
    ("Button", "button"),
    ("Link", "link"),
    ("CheckBox", "checkbox"),
    ("Switch", "switch"),
    ("Toggle", "switch"),
    ("RadioButton", "radio"),
    ("RadioGroup", "radiogroup"),
    ("TextField", "textbox"),
    ("SecureTextField", "textbox"),
    ("TextView", "textbox"),
    ("SearchField", "searchbox"),
    ("Image", "img"),
    ("Slider", "slider"),
    ("ProgressIndicator", "progressbar"),
    ("ActivityIndicator", "progressbar"),
    ("ComboBox", "combobox"),
    ("Table", "list"),
    ("CollectionView", "list"),
    ("Cell", "listitem"),
    ("TabBar", "tablist"),
    ("Tab", "tab"),
    ("Menu", "menu"),
    ("MenuBar", "menubar"),
    ("MenuItem", "menuitem"),
    ("Toolbar", "toolbar"),
    ("Alert", "alertdialog"),
    ("Dialog", "dialog"),
    ("Grid", "grid"),
];

/// Roles whose accessible name comes from their content when nothing else
/// names them (accname: "name from content"), as `injected.js`'s
/// `NAMED_BY_CONTENT`.
const NAMED_BY_CONTENT: &[&str] = &[
    "button",
    "link",
    "heading",
    "cell",
    "gridcell",
    "columnheader",
    "rowheader",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "checkbox",
    "switch",
    "treeitem",
    "tooltip",
    "row",
    "listitem",
];

/// The element's role: the one the driver read ([`Element::role`]), else
/// the one its element type maps to. `None`: no role this matcher knows.
pub fn role_of(el: &Element) -> Option<&str> {
    if let Some(r) = el.role.as_deref().filter(|r| !r.is_empty()) {
        return Some(r);
    }
    role_for_type(&el.element_type)
}

/// The role a device element type maps to (see [`DEVICE_ROLES`]).
pub fn role_for_type(element_type: &str) -> Option<&'static str> {
    let table = |t: &[(&str, &'static str)], k: &str| {
        t.iter().find(|(name, _)| *name == k).map(|(_, role)| *role)
    };
    if let Some(ios) = element_type.strip_prefix("XCUIElementType") {
        return table(IOS_ROLES, ios);
    }
    let short = element_type.rsplit('.').next().unwrap_or(element_type);
    let short = short.rsplit('$').next().unwrap_or(short);
    table(DEVICE_ROLES, short)
}

/// A role an Android role description names, when it is one of [`ROLES`]
/// (Compose's `Role.Switch` and `Role.Tab` report a plain `View` with the
/// role description "Switch" / "Tab"). A localised or free-form
/// description names none.
pub fn role_from_description(desc: &str) -> Option<&'static str> {
    let d = normalize(desc).to_lowercase();
    let d = match d.as_str() {
        "toggle" | "toggle button" => "switch",
        "image" => "img",
        "check box" => "checkbox",
        "radio button" => "radio",
        "text field" | "edit box" => "textbox",
        "drop down list" | "dropdown list" => "combobox",
        other => other,
    };
    ROLES.iter().find(|r| **r == d).copied()
}

/// The accessible name, as the devices know it: the label (content
/// description / accessibility label), else for a field its placeholder
/// (its text is the value, not the name), else the text, else for a role
/// named by its content the text of what is inside, else the placeholder.
pub fn accessible_name(el: &Element) -> Option<String> {
    let non_empty = |s: &Option<String>| {
        s.as_deref()
            .map(|v| normalize(v).into_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(l) = non_empty(&el.label) {
        return Some(l);
    }
    let role = role_of(el);
    let field = matches!(role, Some("textbox" | "searchbox" | "combobox"));
    if field {
        return non_empty(&el.hint);
    }
    if let Some(t) = non_empty(&el.text) {
        return Some(t);
    }
    if role.is_some_and(|r| NAMED_BY_CONTENT.contains(&r)) {
        let mut parts = Vec::new();
        content_text(el, &mut parts, 0);
        let joined = normalize(&parts.join(" ")).into_owned();
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    non_empty(&el.hint)
}

/// The labels and texts inside `el`, in tree order (a few levels, as a
/// screen reader reads a button's content).
fn content_text(el: &Element, out: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    for c in &el.children {
        match c.label.as_deref().or(c.text.as_deref()) {
            Some(v) if !v.trim().is_empty() => out.push(v.to_string()),
            _ => content_text(c, out, depth + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(ty: &str) -> Element {
        Element {
            element_type: ty.into(),
            ..Default::default()
        }
    }

    #[test]
    fn device_types_map_to_playwright_roles() {
        assert_eq!(role_of(&el("Button")), Some("button"));
        assert_eq!(
            role_of(&el("com.google.android.material.button.MaterialButton")),
            Some("button")
        );
        assert_eq!(role_of(&el("XCUIElementTypeButton")), Some("button"));
        assert_eq!(role_of(&el("XCUIElementTypeTextView")), Some("textbox"));
        // A static Android label has no role; an iOS TextView is a field.
        assert_eq!(role_of(&el("TextView")), None);
        assert_eq!(role_of(&el("XCUIElementTypeStaticText")), None);
        assert_eq!(role_of(&el("FrameLayout")), None);
        assert_eq!(
            role_of(&el("com.google.android.material.tabs.TabLayout$TabView")),
            Some("tab")
        );
        assert_eq!(role_of(&el("Header")), Some("heading"));
        // Every mapped role is a Playwright role.
        for (_, r) in DEVICE_ROLES.iter().chain(IOS_ROLES) {
            assert!(is_role(r), "{r}");
        }
    }

    #[test]
    fn a_read_role_wins_over_the_type() {
        let mut e = el("View");
        assert_eq!(role_of(&e), None);
        e.role = Some("switch".into());
        assert_eq!(role_of(&e), Some("switch"));
    }

    #[test]
    fn role_descriptions_map_only_when_they_name_a_role() {
        assert_eq!(role_from_description("Switch"), Some("switch"));
        assert_eq!(role_from_description(" Tab "), Some("tab"));
        assert_eq!(role_from_description("Toggle"), Some("switch"));
        assert_eq!(role_from_description("Schalter"), None);
        assert_eq!(role_from_description("double tap to open"), None);
    }

    #[test]
    fn accessible_name_follows_label_then_text_then_content() {
        let mut b = el("Button");
        b.text = Some("Pay".into());
        assert_eq!(accessible_name(&b).as_deref(), Some("Pay"));
        b.label = Some("Pay now".into());
        assert_eq!(accessible_name(&b).as_deref(), Some("Pay now"));
        // A field's text is its value; its name is the label or hint.
        let mut f = el("EditText");
        f.text = Some("ada@example.com".into());
        f.hint = Some("Email".into());
        assert_eq!(accessible_name(&f).as_deref(), Some("Email"));
        // A button named by the text inside it (Compose, RN).
        let mut wrap = el("Button");
        let mut inner = el("TextView");
        inner.text = Some("Check\nout".into());
        wrap.children.push(inner);
        assert_eq!(accessible_name(&wrap).as_deref(), Some("Check out"));
        // A plain container isn't named by its content.
        let mut div = el("FrameLayout");
        div.children = wrap.children.clone();
        assert_eq!(accessible_name(&div), None);
    }
}
