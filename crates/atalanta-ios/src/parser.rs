use atalanta_common::{AtalantaError, Element, Rect, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Deepest tree accepted: deeper ones overflow the stack in the recursive
/// matchers (VEL-079). WDA itself stops at 50 by default (`snapshotMaxDepth`).
const MAX_DEPTH: usize = 512;

/// Parse WDA's /source XML hierarchy into a atalanta Element tree.
///
/// The iOS XML hierarchy uses XCUIElement types (e.g. XCUIElementTypeButton) and
/// attributes like `name` (accessibility identifier), `label`, `value`, `visible`,
/// `enabled`, and coordinate attributes `x`, `y`, `width`, `height`.
///
/// A source that can't be the whole screen is an error, never a short or
/// empty tree (an empty tree passes every `assertNotVisible`): no element,
/// unbalanced tags (a truncated body, VEL-286), a tree deeper than
/// `MAX_DEPTH`, or an empty zero-size application (what a failed
/// snapshot looks like, IOS-N1).
pub fn parse_ios_hierarchy(xml: &str) -> Result<Element> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Element> = Vec::new();

    let root = Element {
        hint: None,
        platform_id: "ios_hierarchy".to_string(),
        label: None,
        text: None,
        element_type: "Application".to_string(),
        bounds: Rect {
            x: 0,
            y: 0,
            width: 390,
            height: 844,
        },
        enabled: true,
        visible: true,
        children: Vec::new(),
        ..Default::default()
    };
    stack.push(root);

    let mut read_any = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if stack.len() > MAX_DEPTH {
                    return Err(unreadable(format!(
                        "the tree is deeper than {MAX_DEPTH} levels"
                    )));
                }
                let element = parse_ios_node(e)?;
                stack.push(element);
                read_any = true;
            }
            Ok(Event::Empty(ref e)) => {
                let element = parse_ios_node(e)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(element);
                }
                read_any = true;
            }
            Ok(Event::End(_)) => {
                if stack.len() <= 1 {
                    return Err(unreadable("a closing tag without its opening tag".into()));
                }
                let child = stack.pop().unwrap();
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(child);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(unreadable(format!("malformed XML: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    if !read_any {
        return Err(unreadable("no elements".into()));
    }
    if stack.len() != 1 {
        return Err(unreadable(format!(
            "{} element(s) never closed (truncated?)",
            stack.len() - 1
        )));
    }
    let mut result = stack
        .into_iter()
        .next()
        .ok_or_else(|| unreadable("no elements".into()))?;

    // If the root has exactly one child (the actual app element), update root bounds
    if result.children.len() == 1 {
        let app = &result.children[0];
        if app.bounds.is_empty() && app.children.is_empty() {
            return Err(unreadable(
                "an empty application with no size (the snapshot failed?)".into(),
            ));
        }
        let child_bounds = app.bounds;
        if !child_bounds.is_empty() {
            result.bounds = child_bounds;
        }
    }

    Ok(result)
}

/// The source could not be read as a screen. Not a configuration error:
/// the next read may well work.
fn unreadable(detail: String) -> AtalantaError {
    AtalantaError::CommandFailed {
        device_id: String::new(),
        command: "parse iOS source".into(),
        detail,
    }
}

/// A frame coordinate: XCTest reports fractions (`19.5`) and sometimes
/// non-finite values; round, and read anything unusable as 0 (VEL-285:
/// every fractional value used to become 0).
fn coord(v: &str) -> i32 {
    match v.trim().parse::<f64>() {
        Ok(f) if f.is_finite() => f.round().clamp(-1e7, 1e7) as i32,
        _ => 0,
    }
}

fn parse_ios_node(e: &quick_xml::events::BytesStart) -> Result<Element> {
    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();

    let mut name: Option<String> = None;
    let mut label: Option<String> = None;
    let mut value: Option<String> = None;
    let mut placeholder: Option<String> = None;
    let mut element_type = simplify_ios_type(&tag_name);
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    let mut width: i32 = 0;
    let mut height: i32 = 0;
    let mut is_visible = true;
    let mut enabled = true;
    // AtalantaRunner writes `focused` / `selected` only when true, and WDA
    // writes them as true/false: a missing attribute is a known `false`.
    let mut focused = false;
    let mut selected = false;
    let mut visible_bounds = None;

    for attr in e.attributes().flatten() {
        // Keys are matched as bytes and only a kept attribute's value is
        // decoded: WDA writes ~10 attributes a node, and allocating every
        // key and value was the parser's main cost (review 04 #4).
        let key = attr.key.as_ref();
        if !matches!(
            key,
            b"type"
                | b"name"
                | b"label"
                | b"value"
                | b"placeholderValue"
                | b"x"
                | b"y"
                | b"width"
                | b"height"
                | b"visible"
                | b"isVisible"
                | b"enabled"
                | b"focused"
                | b"hasFocus"
                | b"selected"
                | b"isSelected"
                | b"visibleFrame"
                | b"visible-frame"
        ) {
            continue;
        }
        // Decode entities: label="Network &amp; internet" must match the text.
        let val = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).into_owned());

        match key {
            b"type" => element_type = simplify_ios_type(&val),
            b"name" => {
                if !val.is_empty() {
                    name = Some(val);
                }
            }
            b"label" => {
                if !val.is_empty() {
                    label = Some(val);
                }
            }
            b"value" => {
                if !val.is_empty() {
                    value = Some(val);
                }
            }
            b"placeholderValue" => placeholder = Some(val),
            b"x" => x = coord(&val),
            b"y" => y = coord(&val),
            b"width" => width = coord(&val),
            b"height" => height = coord(&val),
            b"visible" | b"isVisible" => is_visible = val == "true" || val == "1",
            b"enabled" => enabled = val == "true" || val == "1",
            // AtalantaRunner (hasKeyboardFocus) and WDA (`focused`) only
            // say it when true.
            b"focused" | b"hasFocus" => focused = val == "true" || val == "1",
            b"selected" | b"isSelected" => selected = val == "true" || val == "1",
            b"visibleFrame" | b"visible-frame" => visible_bounds = frame(&val),
            _ => {}
        }
    }

    let is_field = matches!(
        element_type.as_str(),
        "TextField" | "SecureTextField" | "SearchField" | "TextView"
    );
    // An empty field reports its placeholder as its value: it holds no text
    // (clearText has nothing to delete; inputText may paste into it).
    if is_field && value.is_some() && value == placeholder {
        value = None;
    }
    // A field's placeholder is its hint, as Android's `hint` and Flutter's
    // are: `role::accessible_name` names a field without a label by it, and
    // `textMatches` sees it (review m2).
    let hint = if is_field {
        placeholder.filter(|p| !p.trim().is_empty())
    } else {
        None
    };

    let bounds = Rect {
        x,
        y,
        width,
        height,
    };

    // Map iOS attributes to atalanta Element fields, keeping both:
    // - name (the accessibility identifier, or the label when there's none)
    //   -> platform_id, matched by `id:` / `accessibilityId:`
    // - label (what's shown) -> label, matched by `text:`; falls back to name
    // - value (field contents) -> text
    // Using `name` as the label dropped the visible text whenever an element
    // had an identifier, so `text:` lookups missed and fell back to slow WDA
    // queries.
    let platform_id = name.clone().or_else(|| label.clone()).unwrap_or_default();
    // A field named by its placeholder doesn't take its identifier as its
    // label: the identifier would then be its accessible name, ahead of the
    // placeholder, where Android names the same field by its hint (the
    // identifier still matches `id:` / `accessibilityId:` as platform_id).
    let element_label = if hint.is_some() {
        label
    } else {
        label.or(name)
    };
    // A switch / checkbox reports its state as value "1" / "0".
    let checked = match element_type.as_str() {
        "Switch" | "Toggle" | "CheckBox" => match value.as_deref() {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        },
        _ => None,
    };
    let obscured = element_type == "SecureTextField";
    // The type is kept short (`Button`); its role comes from the iOS table,
    // where `TextView` is a field (S1).
    let role = atalanta_common::role::role_for_type(&format!("XCUIElementType{element_type}"))
        .map(str::to_string);

    Ok(Element {
        selected: Some(selected),
        role,
        hint,
        platform_id,
        label: element_label,
        text: value,
        element_type,
        bounds,
        enabled,
        visible: is_visible,
        children: Vec::new(),
        focused: Some(focused),
        obscured,
        checked,
        visible_bounds,
        dom_id: None,
        css_class: None,
        ..Default::default()
    })
}

/// A `visibleFrame="x,y,w,h"` (AtalantaRunner) or `{{x, y}, {w, h}}`
/// (NSStringFromCGRect): `None` unless it is four finite numbers.
fn frame(v: &str) -> Option<Rect> {
    let nums: Vec<i32> = v
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e'))
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(|_| coord(p))
        })
        .collect::<Option<_>>()?;
    match nums[..] {
        [x, y, width, height] => Some(Rect {
            x,
            y,
            width,
            height,
        }),
        _ => None,
    }
}

/// Simplify iOS element type names.
/// "XCUIElementTypeButton" -> "Button", "XCUIElementTypeStaticText" -> "StaticText"
fn simplify_ios_type(raw: &str) -> String {
    raw.strip_prefix("XCUIElementType")
        .unwrap_or(raw)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review 07 M8: device bytes are untrusted. Mutate a valid input a few
    /// thousand ways (flips, cuts, markup and huge numbers spliced in) and
    /// require an answer, never a panic. Deterministic (a fixed seed).
    fn mutations(seed_input: &str, rounds: usize) -> Vec<String> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        const SPLICE: [&str; 9] = [
            "<",
            ">",
            "\"",
            "=",
            "&amp;",
            "-99999999999",
            "2147483648",
            "\n",
            "][",
        ];
        let base = seed_input.as_bytes();
        (0..rounds)
            .map(|_| {
                let mut b = base.to_vec();
                for _ in 0..1 + next() % 4 {
                    if b.is_empty() {
                        break;
                    }
                    let at = (next() as usize) % b.len();
                    match next() % 4 {
                        0 => b[at] = next() as u8,
                        1 => b.truncate(at),
                        2 => {
                            let s = SPLICE[(next() as usize) % SPLICE.len()].as_bytes();
                            b.splice(at..at, s.iter().copied());
                        }
                        _ => {
                            b.remove(at);
                        }
                    }
                }
                String::from_utf8_lossy(&b).into_owned()
            })
            .collect()
    }

    #[test]
    fn mangled_sources_never_panic() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="Demo" label="Demo" enabled="true" visible="true" x="0" y="0" width="402" height="874"><XCUIElementTypeTextField type="XCUIElementTypeTextField" name="email" value="Email" placeholderValue="Email" enabled="true" focused="true" visibleFrame="{{20, 100}, {362, 30}}" x="20" y="100" width="362" height="44"/><XCUIElementTypeSwitch type="XCUIElementTypeSwitch" label="A &amp; B" value="1" x="0" y="200" width="402" height="44"/></XCUIElementTypeApplication>"#;
        assert!(parse_ios_hierarchy(xml).is_ok());
        for m in mutations(xml, 3000) {
            let _ = parse_ios_hierarchy(&m);
        }
    }

    #[test]
    fn test_simplify_ios_type() {
        assert_eq!(simplify_ios_type("XCUIElementTypeButton"), "Button");
        assert_eq!(simplify_ios_type("XCUIElementTypeStaticText"), "StaticText");
        assert_eq!(simplify_ios_type("XCUIElementTypeCell"), "Cell");
        assert_eq!(simplify_ios_type("Other"), "Other");
        assert_eq!(
            simplify_ios_type("XCUIElementTypeApplication"),
            "Application"
        );
    }

    #[test]
    fn an_empty_field_has_no_text_but_its_placeholder() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844">
  <XCUIElementTypeTextField type="XCUIElementTypeTextField" name="email" value="Email" placeholderValue="Email" x="0" y="0" width="300" height="34" />
  <XCUIElementTypeTextField type="XCUIElementTypeTextField" name="note" value="hello" placeholderValue="Note" x="0" y="40" width="300" height="34" />
  <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="t" value="Email" placeholderValue="Email" x="0" y="80" width="300" height="34" />
</XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let app = &root.children[0];
        assert_eq!(app.children[0].text, None);
        assert_eq!(app.children[1].text.as_deref(), Some("hello"));
        assert_eq!(app.children[2].text.as_deref(), Some("Email"));
    }

    /// Review m2: a field's placeholder is its hint, so `role::accessible_name`
    /// names a field without a label by it, as on Android, also when the
    /// field has an identifier. A real label still wins.
    #[test]
    fn a_field_is_named_by_its_placeholder_like_android() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844">
  <XCUIElementTypeTextField type="XCUIElementTypeTextField" value="Email" placeholderValue="Email" x="0" y="0" width="300" height="34" />
  <XCUIElementTypeSecureTextField type="XCUIElementTypeSecureTextField" name="pwd-field" value="hunter2" placeholderValue="Password" x="0" y="40" width="300" height="34" />
  <XCUIElementTypeTextField type="XCUIElementTypeTextField" name="city" label="City" placeholderValue="e.g. Paris" x="0" y="80" width="300" height="34" />
  <XCUIElementTypeTextField type="XCUIElementTypeTextField" name="zip" x="0" y="120" width="300" height="34" />
  <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="t" value="Hi" placeholderValue="Hint" x="0" y="160" width="300" height="34" />
</XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let kids = &root.children[0].children;
        let name = |i: usize| atalanta_common::role::accessible_name(&kids[i]);
        assert_eq!(kids[0].hint.as_deref(), Some("Email"));
        assert_eq!(name(0).as_deref(), Some("Email"));
        // The identifier stays the id, not the name.
        assert_eq!(name(1).as_deref(), Some("Password"));
        assert_eq!(kids[1].platform_id, "pwd-field");
        assert_eq!(kids[1].text.as_deref(), Some("hunter2"));
        // A label wins over the placeholder.
        assert_eq!(name(2).as_deref(), Some("City"));
        assert_eq!(kids[2].hint.as_deref(), Some("e.g. Paris"));
        // No placeholder: as before.
        assert_eq!(kids[3].hint, None);
        assert_eq!(kids[3].label.as_deref(), Some("zip"));
        // Only fields have a placeholder.
        assert_eq!(kids[4].hint, None);
        // The selector sees it: `{role: textbox, name: Email}`.
        let sel = atalanta_common::Selector::Compound(vec![
            atalanta_common::Selector::Role("textbox".into()),
            atalanta_common::Selector::Name("Email".into()),
        ]);
        assert!(atalanta_common::selector_match::find_in_tree(&root, &sel, None).is_some());
    }

    /// S1/S2: the element type maps to a role through the iOS table (a
    /// `TextView` is a field there, unlike Android's), and `selected` is
    /// read, so `{ role: tab, selected: true }` finds the current tab.
    #[test]
    fn roles_and_selected_come_from_the_ios_tree() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844">
  <XCUIElementTypeTextView type="XCUIElementTypeTextView" name="notes" label="Notes" x="0" y="0" width="300" height="34" />
  <XCUIElementTypeButton type="XCUIElementTypeButton" name="home" label="Home" selected="true" x="0" y="40" width="100" height="34" />
  <XCUIElementTypeButton type="XCUIElementTypeButton" name="away" label="Away" selected="false" x="100" y="40" width="100" height="34" />
  <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" label="Home" x="0" y="80" width="300" height="34" />
</XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let app = &root.children[0];
        assert_eq!(app.children[0].role.as_deref(), Some("textbox"));
        assert_eq!(app.children[1].role.as_deref(), Some("button"));
        assert_eq!(app.children[1].selected, Some(true));
        assert_eq!(app.children[2].selected, Some(false));
        assert_eq!(app.children[3].role, None);
        use atalanta_common::Selector;
        let found = atalanta_common::selector_match::find_in_tree(
            &root,
            &Selector::Compound(vec![
                Selector::Role("button".into()),
                Selector::Selected(true),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(found.label.as_deref(), Some("Home"));
        let notes = atalanta_common::selector_match::find_in_tree(
            &root,
            &Selector::Compound(vec![
                Selector::Role("textbox".into()),
                Selector::Name("Notes".into()),
            ]),
            None,
        );
        assert!(notes.is_some());
    }

    #[test]
    fn atalanta_runner_output_reads_like_wda() {
        // The XML AtalantaRunner writes since it sends placeholderValue and
        // visible (ui-frameworks N1/N2): an empty field reads empty, not as
        // its placeholder (VEL-354), an off-screen row is hidden, and the
        // extra attributes (focused, selected, visibleFrame, a truncated
        // root after a too-deep snapshot) parse without changing anything.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="Demo" label="Demo" enabled="true" visible="true" truncated="true" x="0" y="0" width="402" height="874"><XCUIElementTypeTextField type="XCUIElementTypeTextField" name="email" value="Email" placeholderValue="Email" enabled="true" visible="true" focused="true" visibleFrame="20,100,362,30" x="20" y="100" width="362" height="44"/><XCUIElementTypeCell type="XCUIElementTypeCell" name="Row 40" label="Row 40" enabled="true" visible="false" selected="true" x="0" y="2000" width="402" height="44"/></XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let app = &root.children[0];
        assert_eq!(app.label.as_deref(), Some("Demo"));
        let field = &app.children[0];
        assert_eq!(field.text, None, "placeholder is not text");
        assert!(field.visible);
        assert_eq!((field.bounds.y, field.bounds.height), (100, 44));
        assert!(!app.children[1].visible);
        // D-39: focus and the visible part of the frame are mapped.
        assert_eq!(field.focused, Some(true));
        assert_eq!(
            field.visible_bounds,
            Some(Rect {
                x: 20,
                y: 100,
                width: 362,
                height: 30
            })
        );
        assert_eq!(app.children[1].focused, Some(false));
        assert_eq!(app.children[1].visible_bounds, None);
    }

    /// D-39: a secure field is obscured; a switch's value is its state.
    #[test]
    fn secure_fields_and_switch_state_are_mapped() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844"><XCUIElementTypeSecureTextField type="XCUIElementTypeSecureTextField" name="pw" value="••••" x="0" y="0" width="300" height="34"/><XCUIElementTypeSwitch type="XCUIElementTypeSwitch" name="wifi" value="1" x="0" y="40" width="50" height="30"/><XCUIElementTypeSwitch type="XCUIElementTypeSwitch" name="bt" value="0" x="0" y="80" width="50" height="30"/><XCUIElementTypeButton type="XCUIElementTypeButton" name="b" value="1" visibleFrame="{{0, 120}, {50, 30}}" x="0" y="120" width="50" height="30"/></XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let k = &root.children[0].children;
        assert!(k[0].obscured && !k[1].obscured);
        assert_eq!(k[0].checked, None);
        assert_eq!(k[1].checked, Some(true));
        assert_eq!(k[2].checked, Some(false));
        assert_eq!(k[3].checked, None, "a button's value is not a state");
        assert_eq!(k[3].visible_bounds.map(|r| (r.y, r.width)), Some((120, 50)));
        assert_eq!(frame("nan,1,2,3"), None);
        assert_eq!(frame("1,2,3"), None);
    }

    #[test]
    fn test_parse_simple_hierarchy() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="MyApp" label="MyApp" enabled="true" visible="true" x="0" y="0" width="390" height="844">
  <XCUIElementTypeWindow type="XCUIElementTypeWindow" enabled="true" visible="true" x="0" y="0" width="390" height="844">
    <XCUIElementTypeButton type="XCUIElementTypeButton" name="loginButton" label="Log In" value="" enabled="true" visible="true" x="50" y="400" width="290" height="44" />
    <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="" label="Welcome" value="Welcome" enabled="true" visible="true" x="50" y="200" width="290" height="30" />
  </XCUIElementTypeWindow>
</XCUIElementTypeApplication>"#;

        let root = parse_ios_hierarchy(xml).unwrap();
        assert_eq!(root.element_type, "Application");
        assert_eq!(root.bounds.width, 390);
        assert_eq!(root.bounds.height, 844);

        // Root wraps the Application element
        assert_eq!(root.children.len(), 1);
        let app = &root.children[0];
        assert_eq!(app.element_type, "Application");
        assert_eq!(app.label.as_deref(), Some("MyApp"));

        // App contains a Window
        assert_eq!(app.children.len(), 1);
        let window = &app.children[0];
        assert_eq!(window.element_type, "Window");
        assert_eq!(window.children.len(), 2);

        // Button
        let button = &window.children[0];
        assert_eq!(button.element_type, "Button");
        assert_eq!(button.label.as_deref(), Some("Log In"), "visible text kept");
        assert_eq!(button.platform_id, "loginButton", "identifier kept");
        assert_eq!(button.bounds.x, 50);
        assert_eq!(button.bounds.y, 400);
        assert_eq!(button.bounds.width, 290);
        assert_eq!(button.bounds.height, 44);
        assert!(button.enabled);
        assert!(button.visible);

        // StaticText
        let text = &window.children[1];
        assert_eq!(text.element_type, "StaticText");
        assert_eq!(text.text.as_deref(), Some("Welcome"));
        assert_eq!(text.bounds.x, 50);
        assert_eq!(text.bounds.y, 200);
    }

    #[test]
    fn test_parse_nested_hierarchy() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="App" label="App" enabled="true" visible="true" x="0" y="0" width="390" height="844">
  <XCUIElementTypeOther type="XCUIElementTypeOther" enabled="true" visible="true" x="0" y="0" width="390" height="844">
    <XCUIElementTypeCell type="XCUIElementTypeCell" name="orderCell_0" label="Order 1" enabled="true" visible="true" x="0" y="100" width="390" height="80">
      <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="orderTitle" label="Coffee" value="Coffee" enabled="true" visible="true" x="16" y="110" width="200" height="20" />
      <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="orderPrice" label="$4.50" value="$4.50" enabled="true" visible="true" x="300" y="110" width="74" height="20" />
    </XCUIElementTypeCell>
  </XCUIElementTypeOther>
</XCUIElementTypeApplication>"#;

        let root = parse_ios_hierarchy(xml).unwrap();
        let app = &root.children[0];
        let other = &app.children[0];
        let cell = &other.children[0];

        assert_eq!(cell.element_type, "Cell");
        assert_eq!(cell.platform_id, "orderCell_0");
        assert_eq!(cell.label.as_deref(), Some("Order 1"));
        assert_eq!(cell.children.len(), 2);

        let title = &cell.children[0];
        assert_eq!(title.text.as_deref(), Some("Coffee"));
        assert_eq!(title.platform_id, "orderTitle");
        assert_eq!(title.label.as_deref(), Some("Coffee"));

        let price = &cell.children[1];
        assert_eq!(price.text.as_deref(), Some("$4.50"));
    }

    #[test]
    fn trimmed_source_without_visible_attributes_still_matches() {
        // What `/source?excluded_attributes=visible,accessible` returns.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="Settings" label="Settings" enabled="true" x="0" y="0" width="402" height="874">
  <XCUIElementTypeCell type="XCUIElementTypeCell" name="General" label="General" enabled="true" x="0" y="300" width="402" height="52"/>
  <XCUIElementTypeCell type="XCUIElementTypeCell" name="Offscreen" label="Offscreen" enabled="true" x="0" y="2000" width="402" height="52"/>
</XCUIElementTypeApplication>"#;
        let tree = parse_ios_hierarchy(xml).unwrap();
        let screen = tree.bounds;
        let sel = |t: &str| atalanta_common::Selector::Text(t.into());
        assert!(atalanta_common::selector_match::find_in_tree(
            &tree,
            &sel("General"),
            Some(&screen)
        )
        .is_some());
        // Visibility now comes from the frame: off screen means not visible.
        assert!(atalanta_common::selector_match::find_in_tree(
            &tree,
            &sel("Offscreen"),
            Some(&screen)
        )
        .is_none());
    }

    #[test]
    fn test_parse_invisible_element() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeOther type="XCUIElementTypeOther" enabled="true" visible="true" x="0" y="0" width="390" height="844">
  <XCUIElementTypeButton type="XCUIElementTypeButton" name="hidden" label="Hidden" enabled="true" visible="false" x="0" y="0" width="0" height="0" />
</XCUIElementTypeOther>"#;

        let root = parse_ios_hierarchy(xml).unwrap();
        let other = &root.children[0];
        let button = &other.children[0];

        assert!(!button.visible);
        assert!(button.bounds.is_empty());
    }

    #[test]
    fn an_empty_source_is_an_error() {
        // It used to be a synthetic empty screen: every lookup missed.
        assert!(parse_ios_hierarchy("").is_err());
        assert!(parse_ios_hierarchy("<?xml version=\"1.0\"?>").is_err());
    }

    #[test]
    fn an_unclosed_source_is_an_error() {
        // VEL-286: a truncated body parsed as a short screen.
        assert!(parse_ios_hierarchy("<unclosed>").is_err());
        let truncated = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844">
  <XCUIElementTypeButton type="XCUIElementTypeButton" label="A" x="0" y="0" width="10" height="10"/>"#;
        let e = parse_ios_hierarchy(truncated).unwrap_err();
        assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
        assert!(parse_ios_hierarchy("<a></a></b>").is_err());
    }

    #[test]
    fn a_failed_snapshot_is_an_error_not_a_blank_screen() {
        // IOS-N1: AtalantaRunner answered a failed snapshot with this.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="0" height="0"/>"#;
        assert!(parse_ios_hierarchy(xml).is_err());
    }

    #[test]
    fn fractional_coordinates_round() {
        // VEL-285: "19.5" parsed as an integer failed and became 0.
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="390" height="844">
  <XCUIElementTypeButton type="XCUIElementTypeButton" label="A" x="19.5" y="100.4" width="120.6" height="nan"/>
</XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        let b = root.children[0].children[0].bounds;
        assert_eq!((b.x, b.y, b.width, b.height), (20, 100, 121, 0));
    }

    #[test]
    fn a_too_deep_tree_is_an_error_not_a_crash() {
        // VEL-079 (iOS part): 10 000 levels overflowed the stack later on.
        let open = r#"<XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="0" width="10" height="10">"#;
        let xml = format!(
            "{}{}",
            open.repeat(10_000),
            "</XCUIElementTypeOther>".repeat(10_000)
        );
        let e = parse_ios_hierarchy(&xml).unwrap_err();
        assert!(e.to_string().contains("deeper"), "{e}");
        // A normal depth still parses.
        let ok = format!(
            "{}{}",
            open.repeat(100),
            "</XCUIElementTypeOther>".repeat(100)
        );
        assert!(parse_ios_hierarchy(&ok).is_ok());
    }

    #[test]
    fn test_identifier_and_label_both_match() {
        use atalanta_common::selector_match::find_in_tree;
        use atalanta_common::Selector;
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" name="App" x="0" y="0" width="390" height="844">
  <XCUIElementTypeButton type="XCUIElementTypeButton" name="checkout_btn" label="Pay &amp; finish" enabled="true" visible="true" x="0" y="0" width="100" height="40" />
</XCUIElementTypeApplication>"#;
        let root = parse_ios_hierarchy(xml).unwrap();
        for sel in [
            Selector::Id("checkout_btn".into()),
            Selector::AccessibilityId("checkout_btn".into()),
            Selector::Text("Pay & finish".into()),
        ] {
            assert!(
                find_in_tree(&root, &sel, None).is_some(),
                "{sel} should match"
            );
        }
    }
}
