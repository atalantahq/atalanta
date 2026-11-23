// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::{AtalantaError, Element, Rect, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

/// Parse a uiautomator XML dump into an Element tree.
pub fn parse_hierarchy(xml: &str) -> Result<Element> {
    // Delegate to v2 which handles both Start and Empty events correctly
    parse_hierarchy_v2(xml)
}

/// Deepest node nesting accepted. Real screens stay far below this (the
/// tree server's own cap is lower); deeper input is an error, not a stack
/// of half a million elements.
pub const MAX_DEPTH: usize = 512;

/// Parse a uiautomator XML dump handling both self-closing and nested nodes.
///
/// Strict: the input must be one whole `<hierarchy>` document. Empty input,
/// text that isn't XML, a dump cut off mid-way and nesting deeper than
/// [`MAX_DEPTH`] are errors (`CommandFailed`, filled in with the device by
/// the driver), never an empty screen — an empty screen makes
/// `assertNotVisible` pass (VEL-335).
pub fn parse_hierarchy_v2(xml: &str) -> Result<Element> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Element> = Vec::new();

    let root = Element {
        hint: None,
        platform_id: "hierarchy".to_string(),
        label: None,
        text: None,
        element_type: "hierarchy".to_string(),
        bounds: Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        enabled: true,
        visible: true,
        children: Vec::new(),
        ..Default::default()
    };
    stack.push(root);

    // Seen `<hierarchy>`, and its end.
    let mut opened = false;
    let mut closed = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().as_ref() {
                b"hierarchy" if !opened => opened = true,
                b"node" if opened && !closed => {
                    if stack.len() > MAX_DEPTH {
                        return Err(bad_dump(format!("nodes nested deeper than {MAX_DEPTH}")));
                    }
                    let element = parse_node_attributes_from_bytes(e)?;
                    stack.push(element);
                }
                other => {
                    return Err(bad_dump(format!(
                        "unexpected <{}>",
                        String::from_utf8_lossy(other)
                    )))
                }
            },
            Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"hierarchy" if !opened => {
                    opened = true;
                    closed = true;
                }
                b"node" if opened && !closed => {
                    let element = parse_node_attributes_from_bytes(e)?;
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(element);
                    }
                }
                other => {
                    return Err(bad_dump(format!(
                        "unexpected <{}/>",
                        String::from_utf8_lossy(other)
                    )))
                }
            },
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"node" if stack.len() > 1 => {
                    let child = stack.pop().unwrap();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(child);
                    }
                }
                b"hierarchy" if opened && !closed && stack.len() == 1 => closed = true,
                other => {
                    return Err(bad_dump(format!(
                        "unexpected </{}>",
                        String::from_utf8_lossy(other)
                    )))
                }
            },
            Ok(Event::Eof) => break,
            Ok(Event::Text(ref t)) if !t.iter().all(u8::is_ascii_whitespace) => {
                return Err(bad_dump(format!(
                    "text outside the hierarchy: {}",
                    String::from_utf8_lossy(&t[..t.len().min(80)])
                )));
            }
            Err(e) => return Err(bad_dump(e.to_string())),
            _ => {}
        }
        buf.clear();
    }

    if !opened {
        return Err(bad_dump("no <hierarchy> element".into()));
    }
    if !closed || stack.len() != 1 {
        return Err(bad_dump("the dump is cut off (no </hierarchy>)".into()));
    }
    let mut root = stack.pop().unwrap();
    // The screen the windows cover: their union (no longer a fixed
    // 1080x2400, whatever the device).
    root.bounds = root
        .children
        .iter()
        .map(|c| c.bounds)
        .reduce(union)
        .unwrap_or(root.bounds);
    Ok(root)
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = a.x.saturating_add(a.width).max(b.x.saturating_add(b.width));
    let bottom =
        a.y.saturating_add(a.height)
            .max(b.y.saturating_add(b.height));
    Rect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// A dump that can't be the screen. The device id is filled in by the
/// driver; this is not a config error (it is not the user's file).
fn bad_dump(detail: String) -> AtalantaError {
    AtalantaError::CommandFailed {
        device_id: String::new(),
        command: "parse uiautomator XML".into(),
        detail,
    }
}

fn parse_node_attributes_from_bytes(e: &quick_xml::events::BytesStart) -> Result<Element> {
    let mut text = None;
    let mut resource_id = String::new();
    let mut content_desc = None;
    let mut class = String::new();
    let mut bounds_str = String::new();
    let mut enabled = true;

    let mut hint = None;
    let mut test_tag = None;
    // Both dump sources (uiautomator, the tree server, lite or full) write
    // `focused` and `selected` on every node; a dump without one leaves the
    // state unknown, which no state filter matches.
    let (mut focused, mut password) = (None, false);
    let (mut checkable, mut checked) = (None, false);
    let (mut selected, mut role_desc) = (None, None);
    let mut indeterminate = false;

    for attr in e.attributes().flatten() {
        // Match the key as bytes and decode only the values kept: a big
        // dump has a dozen attributes per node that are never read (VEL-309).
        let key = attr.key.as_ref();
        if !matches!(
            key,
            b"text"
                | b"resource-id"
                | b"content-desc"
                | b"class"
                | b"bounds"
                | b"enabled"
                | b"hint"
                | b"test-tag"
                | b"focused"
                | b"password"
                | b"checkable"
                | b"checked"
                | b"selected"
                | b"role-desc"
                | b"indeterminate"
        ) {
            continue;
        }
        // Decode entities: "Network &amp; internet" must match text="Network & internet".
        let val = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).into_owned());
        let non_empty = |v: String| (!v.is_empty()).then_some(v);

        match key {
            b"text" => text = non_empty(val),
            b"resource-id" => resource_id = val,
            b"content-desc" => content_desc = non_empty(val),
            b"class" => class = val,
            b"bounds" => bounds_str = val,
            b"enabled" => enabled = val == "true",
            // Protocol 9 tree server: an empty field's placeholder.
            b"hint" => hint = non_empty(val),
            // Protocol 10: a Compose testTag the app didn't expose as a
            // resource-id (D-53).
            b"test-tag" => test_tag = non_empty(val),
            b"focused" => focused = Some(val == "true"),
            b"password" => password = val == "true",
            b"checkable" => checkable = Some(val == "true"),
            b"checked" => checked = val == "true",
            b"selected" => selected = Some(val == "true"),
            // Protocol 11: the node's role description (Compose's
            // `Role.Switch` / `Role.Tab` are a plain View with one).
            b"role-desc" => role_desc = non_empty(val),
            // Protocol 18: a ProgressBar with no range, a spinner.
            b"indeterminate" => indeterminate = val == "true",
            _ => {}
        }
    }

    let bounds = parse_bounds(&bounds_str);
    let visible = !bounds.is_empty();

    // Simplify class name: "android.widget.Button" -> "Button"
    let element_type = class.rsplit('.').next().unwrap_or(&class).to_string();

    // A lite dump has `checked` but not `checkable`: a checked node is
    // checkable, an unchecked one unknown.
    let checked = match checkable {
        Some(true) => Some(checked),
        Some(false) => None,
        None => checked.then_some(true),
    };
    // The tag stands in for the resource-id it would have been with
    // testTagsAsResourceId (bare, no `pkg:id/`), so `id:` matches either way.
    let platform_id = match test_tag {
        Some(tag) if resource_id.is_empty() && compose_test_tags() => tag,
        _ => resource_id,
    };

    // A role description counts only when it names a Playwright role.
    let role = role_desc
        .as_deref()
        .and_then(atalanta_common::role::role_from_description)
        .map(str::to_string);

    Ok(Element {
        hint,
        platform_id,
        selected,
        role,
        focused,
        obscured: password,
        checked,
        label: content_desc,
        text,
        element_type,
        bounds,
        enabled,
        visible,
        children: Vec::new(),
        indeterminate,
        ..Default::default()
    })
}

/// Whether a dump's Compose `test-tag` is used (D-53): on unless
/// `ATALANTA_ANDROID_COMPOSE_TESTTAG=0` (or `off`/`false`).
pub(crate) fn compose_test_tags() -> bool {
    test_tags_on(
        atalanta_common::env::var("ATALANTA_ANDROID_COMPOSE_TESTTAG")
            .ok()
            .as_deref(),
    )
}

fn test_tags_on(setting: Option<&str>) -> bool {
    !matches!(
        setting.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "off" | "false")
    )
}

/// Parse bounds string like "[120,640][960,736]" into a Rect.
fn parse_bounds(bounds: &str) -> Rect {
    let nums: Vec<i32> = bounds
        .replace('[', "")
        .replace(']', ",")
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();

    if nums.len() == 4 {
        Rect {
            x: nums[0],
            y: nums[1],
            width: nums[2].saturating_sub(nums[0]),
            height: nums[3].saturating_sub(nums[1]),
        }
    } else {
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }
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
    fn mangled_dumps_never_panic() {
        let xml = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="Network &amp; internet" resource-id="com.app:id/row" class="android.widget.TextView" package="com.app" content-desc="" checkable="true" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,63][1080,240]" hint="Name" test-tag="row"><node index="0" text="" class="android.widget.ProgressBar" indeterminate="true" bounds="[10,70][50,110]" /></node></hierarchy>"#;
        assert!(parse_hierarchy_v2(xml).is_ok());
        for m in mutations(xml, 3000) {
            let _ = parse_hierarchy_v2(&m);
            let _ = parse_hierarchy(&m);
        }
    }

    #[test]
    fn test_parse_bounds() {
        let rect = parse_bounds("[120,640][960,736]");
        assert_eq!(rect.x, 120);
        assert_eq!(rect.y, 640);
        assert_eq!(rect.width, 840);
        assert_eq!(rect.height, 96);
    }

    #[test]
    fn test_attribute_entities_are_decoded() {
        let xml = r#"<hierarchy><node text="Network &amp; internet" content-desc="&quot;Hi&quot; &lt;3"
            resource-id="" class="android.widget.TextView" bounds="[0,0][10,10]" /></hierarchy>"#;
        let root = parse_hierarchy_v2(xml).unwrap();
        let node = &root.children[0];
        assert_eq!(node.text.as_deref(), Some("Network & internet"));
        assert_eq!(node.label.as_deref(), Some("\"Hi\" <3"));
    }

    #[test]
    fn an_indeterminate_progress_bar_is_read_as_a_spinner() {
        // Protocol 18: the tree server marks a ProgressBar with no range.
        let xml = r#"<hierarchy><node class="android.widget.ProgressBar" indeterminate="true"
            bounds="[0,0][10,10]" /><node class="android.widget.ProgressBar"
            bounds="[0,20][10,30]" /></hierarchy>"#;
        let root = parse_hierarchy_v2(xml).unwrap();
        assert!(root.children[0].indeterminate);
        assert!(!root.children[1].indeterminate, "a meter with a range");
    }

    #[test]
    fn empty_and_text_only_dumps_are_errors() {
        for bad in [
            "",
            "   \n",
            "ERROR: could not get idle state.",
            "<?xml version='1.0' ?>",
            "<node bounds=\"[0,0][1,1]\"/>",
            "<hierarchy><node bounds=\"[0,0][1,1]\"/></hierarchy>trailing",
        ] {
            let e = parse_hierarchy_v2(bad).unwrap_err();
            assert!(
                matches!(e, AtalantaError::CommandFailed { .. }),
                "{bad:?}: {e}"
            );
        }
        // An empty screen is still a whole document.
        assert!(parse_hierarchy_v2("<hierarchy rotation=\"0\"/>").is_ok());
    }

    /// VEL-335: a dump cut off mid-way parsed as a smaller screen.
    #[test]
    fn truncated_dump_is_an_error() {
        let xml = r#"<hierarchy rotation="0"><node class="a.B" bounds="[0,0][10,10]"><node text="Pay" bounds="[0,0][5,5]"/>"#;
        assert!(parse_hierarchy_v2(xml).is_err());
        assert!(parse_hierarchy_v2(&format!("{xml}</node>")).is_err());
        let whole = parse_hierarchy_v2(&format!("{xml}</node></hierarchy>")).unwrap();
        assert_eq!(whole.children[0].children[0].text.as_deref(), Some("Pay"));
    }

    #[test]
    fn depth_cap_errors_not_aborts() {
        let nest = |n: usize| {
            format!(
                "<hierarchy>{}{}</hierarchy>",
                "<node bounds=\"[0,0][1,1]\">".repeat(n),
                "</node>".repeat(n)
            )
        };
        assert!(parse_hierarchy_v2(&nest(100)).is_ok());
        let e = parse_hierarchy_v2(&nest(MAX_DEPTH + 10)).unwrap_err();
        assert!(e.to_string().contains("deeper"), "{e}");
    }

    /// VEL-157: extreme bounds overflowed the subtraction.
    #[test]
    fn extreme_bounds_saturate() {
        let r = parse_bounds("[-2147483648,0][2147483647,10]");
        assert_eq!(r.width, i32::MAX);
        let r = parse_bounds("[2147483647,0][-2147483648,10]");
        assert_eq!(r.width, i32::MIN);
    }

    #[test]
    fn root_bounds_come_from_the_screen() {
        let root = parse_hierarchy_v2(
            r#"<hierarchy><node bounds="[0,0][720,1600]"/><node bounds="[0,1500][720,1600]"/></hierarchy>"#,
        )
        .unwrap();
        assert_eq!((root.bounds.width, root.bounds.height), (720, 1600));
    }

    #[test]
    fn test_parse_bounds_empty() {
        let rect = parse_bounds("");
        assert!(rect.is_empty());
    }

    #[test]
    fn test_parse_hierarchy() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hierarchy rotation="0">
  <node index="0" text="" resource-id="" class="android.widget.FrameLayout"
        bounds="[0,0][1080,2400]" enabled="true" clickable="false" content-desc="">
    <node index="0" text="Log In" resource-id="com.app:id/login_button"
          class="android.widget.Button" content-desc="Login"
          bounds="[120,640][960,736]" enabled="true" clickable="true">
    </node>
  </node>
</hierarchy>"#;

        let root = parse_hierarchy_v2(xml).unwrap();
        assert_eq!(root.element_type, "hierarchy");
        assert_eq!(root.children.len(), 1);

        let frame = &root.children[0];
        assert_eq!(frame.element_type, "FrameLayout");
        assert_eq!(frame.children.len(), 1);

        let button = &frame.children[0];
        assert_eq!(button.text.as_deref(), Some("Log In"));
        assert_eq!(button.platform_id, "com.app:id/login_button");
        assert_eq!(button.label.as_deref(), Some("Login"));
        assert_eq!(button.element_type, "Button");
        assert_eq!(button.bounds.x, 120);
        assert_eq!(button.bounds.width, 840);
    }

    /// S1/S2: `selected` and a role description that names a role come
    /// from the dump, so `role: switch` finds a Compose switch (a plain
    /// View) and `selected: true` the current tab.
    #[test]
    fn selected_and_role_description_are_read_from_the_dump() {
        let xml = r#"<hierarchy rotation="0"><node index="0" text="" class="android.view.View" content-desc="Wi-Fi" role-desc="Switch" checkable="true" checked="true" selected="false" bounds="[0,0][100,50]" /><node index="1" text="Home" class="android.view.View" role-desc="Tab" selected="true" bounds="[0,50][100,100]" /><node index="2" text="Other" class="android.view.View" role-desc="Umschalter" bounds="[0,100][100,150]" /></hierarchy>"#;
        let root = parse_hierarchy(xml).unwrap();
        let k = &root.children;
        assert_eq!(k[0].role.as_deref(), Some("switch"));
        assert_eq!(k[0].selected, Some(false));
        assert_eq!(k[1].role.as_deref(), Some("tab"));
        assert_eq!(k[1].selected, Some(true));
        assert_eq!(k[2].selected, None, "no attribute: unknown");
        assert_eq!(k[2].role, None, "a description that names no role");
        let sel = atalanta_common::Selector::Compound(vec![
            atalanta_common::Selector::Role("switch".into()),
            atalanta_common::Selector::Name("Wi-Fi".into()),
            atalanta_common::Selector::Checked(true),
        ]);
        assert!(atalanta_common::selector_match::find_in_tree(&root, &sel, None).is_some());
    }

    /// D-39: focus, password and checkbox state come from the dump. A lite
    /// dump (no `checkable`) still reports a checked box.
    #[test]
    fn state_flags_are_read_from_the_dump() {
        let xml = r#"<hierarchy rotation="0"><node index="0" text="" resource-id="pw" class="android.widget.EditText" content-desc="" checkable="false" checked="false" focused="true" password="true" enabled="true" bounds="[0,0][100,50]" /><node index="1" text="Remember" resource-id="" class="android.widget.CheckBox" content-desc="" checkable="true" checked="false" focused="false" password="false" enabled="true" bounds="[0,50][100,100]" /><node index="2" text="On" class="android.widget.Switch" checked="true" focused="false" bounds="[0,100][100,150]" /><node index="3" text="t" class="android.widget.TextView" bounds="[0,150][100,200]" /></hierarchy>"#;
        let root = parse_hierarchy(xml).unwrap();
        let k = &root.children;
        assert!(k[0].focused == Some(true) && k[0].obscured);
        assert_eq!(k[0].checked, None, "not checkable");
        assert!(k[1].focused == Some(false) && !k[1].obscured);
        assert_eq!(k[1].checked, Some(false));
        assert_eq!(
            k[2].checked,
            Some(true),
            "lite dump: checked implies checkable"
        );
        assert_eq!(k[3].checked, None);
        assert_eq!(k[3].focused, None, "no attribute: unknown");
    }

    /// D-53: a protocol 10 dump's Compose `test-tag` stands in for an empty
    /// resource-id, so `id: login_button` matches without
    /// testTagsAsResourceId; a real resource-id still wins.
    #[test]
    fn compose_test_tag_becomes_the_id() {
        let xml = r#"<hierarchy rotation="0"><node index="0" text="" resource-id="" test-tag="login_button" class="android.view.View" content-desc="" enabled="true" bounds="[0,0][100,50]" /><node index="1" text="" resource-id="com.app:id/real" test-tag="tag" class="android.view.View" enabled="true" bounds="[0,50][100,100]" /></hierarchy>"#;
        let root = parse_hierarchy(xml).unwrap();
        assert_eq!(root.children[0].platform_id, "login_button");
        assert_eq!(root.children[1].platform_id, "com.app:id/real");
        let sel = atalanta_common::Selector::Id("login_button".into());
        let found = crate::selector::find_element(&root, &sel, &Default::default(), &root.bounds);
        assert!(found.is_some(), "id: matches the testTag");
    }

    #[test]
    fn compose_test_tag_opt_out() {
        assert!(test_tags_on(None));
        assert!(test_tags_on(Some("1")));
        for off in ["0", "off", "OFF", "false", " 0 "] {
            assert!(!test_tags_on(Some(off)), "{off}");
        }
    }

    /// N4: a protocol 9 dump's `hint` is read; an empty one, or a dump
    /// without it (older servers, uiautomator), leaves it unset.
    #[test]
    fn hint_is_read_from_the_dump() {
        let xml = r#"<hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.EditText" content-desc="" hint="Email &amp; phone" state-desc="" error="" enabled="true" bounds="[0,0][100,50]" /><node index="1" text="x" resource-id="" class="android.widget.EditText" content-desc="" hint="" enabled="true" bounds="[0,50][100,100]" /><node index="2" text="y" class="android.widget.TextView" bounds="[0,100][100,150]" /></hierarchy>"#;
        let root = parse_hierarchy_v2(xml).unwrap();
        let kids = &root.children;
        assert_eq!(kids[0].hint.as_deref(), Some("Email & phone"));
        assert_eq!(kids[0].text, None);
        assert_eq!(kids[1].hint, None);
        assert_eq!(kids[2].hint, None);
        assert_eq!(kids[2].text.as_deref(), Some("y"));
    }
}
