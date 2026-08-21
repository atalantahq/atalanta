use velocity_common::{Element, Rect};

/// The element tree from the in-page `walk` (see `injected.js`): a flat
/// pre-order list where each node names its parent's index (`p`, -1 for the
/// root). Flat, so the JSON nesting limit can't cut a deep page short;
/// the recursive walker stopped at depth 40 (VEL-126).
///
/// Each node's Velocity id (kept outside the DOM, stable for the page's
/// life) becomes the `platform_id` of `velocity_common::Element`.
pub fn tree_from_list(list: &[serde_json::Value]) -> Option<Element> {
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); list.len()];
    for (i, node) in list.iter().enumerate() {
        match node["p"].as_i64() {
            Some(p) if p >= 0 && (p as usize) < i => kids[p as usize].push(i),
            _ if i == 0 => {}
            _ => return None,
        }
    }
    // Children come after their parent, so building from the end has every
    // child ready when its parent is built.
    let mut built: Vec<Option<Element>> = vec![None; list.len()];
    for i in (0..list.len()).rev() {
        let mut el = json_to_element(&list[i])?;
        el.children = kids[i].iter().filter_map(|&k| built[k].take()).collect();
        built[i] = Some(el);
    }
    built.into_iter().next().flatten()
}

/// Convert one JSON node (from the DOM walker script) into an `Element`.
pub fn json_to_element(node: &serde_json::Value) -> Option<Element> {
    let vel_id = node["velId"].as_str()?.to_string();
    let tag = node["tag"].as_str().unwrap_or("div").to_string();
    let text = node["text"].as_str().map(str::to_string);

    // Prefer ariaLabel for the `label` field, then fall back to the DOM `id`.
    let label = node["ariaLabel"]
        .as_str()
        .or_else(|| node["domId"].as_str())
        .map(str::to_string);

    let x = node["x"].as_i64().unwrap_or(0) as i32;
    let y = node["y"].as_i64().unwrap_or(0) as i32;
    let w = node["w"].as_i64().unwrap_or(0) as i32;
    let h = node["h"].as_i64().unwrap_or(0) as i32;
    let visible = node["visible"].as_bool().unwrap_or(true);
    let enabled = node["enabled"].as_bool().unwrap_or(true);
    // D-39 state, from `cap` in injected.js, which sends only what isn't
    // the default: no `vb` means the whole box is in the viewport.
    let bounds = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let clipped = node["vb"].as_array().and_then(|v| match v.as_slice() {
        [x, y, w, h] => Some(Rect {
            x: x.as_i64()? as i32,
            y: y.as_i64()? as i32,
            width: w.as_i64()? as i32,
            height: h.as_i64()? as i32,
        }),
        _ => None,
    });
    let visible_bounds = Some(clipped.unwrap_or(bounds));

    let children = node["children"]
        .as_array()
        .map(|arr| arr.iter().filter_map(json_to_element).collect())
        .unwrap_or_default();

    Some(Element {
        hint: node["hint"].as_str().map(str::to_string),
        platform_id: vel_id,
        label,
        text,
        element_type: tag,
        bounds,
        enabled,
        visible,
        children,
        // The page always knows both states (absent: false).
        focused: Some(node["focused"].as_bool().unwrap_or(false)),
        obscured: node["obscured"].as_bool().unwrap_or(false),
        checked: node["checked"].as_bool(),
        visible_bounds,
        // S1/S2: the computed role and `aria-selected` / an option's state.
        selected: Some(node["selected"].as_bool().unwrap_or(false)),
        role: node["role"].as_str().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_basic_node() {
        let node = json!({
            "velId": "abc123",
            "tag": "button",
            "domId": "submit",
            "text": "Click me",
            "ariaLabel": null,
            "x": 10, "y": 20, "w": 100, "h": 40,
            "visible": true,
            "children": []
        });
        let el = json_to_element(&node).unwrap();
        assert_eq!(el.platform_id, "abc123");
        assert_eq!(el.element_type, "button");
        assert_eq!(el.text.as_deref(), Some("Click me"));
        assert_eq!(el.label.as_deref(), Some("submit")); // falls back to domId
        assert_eq!(el.bounds.x, 10);
        assert_eq!(el.bounds.y, 20);
        assert_eq!(el.bounds.width, 100);
        assert_eq!(el.bounds.height, 40);
        assert!(el.visible);
        assert!(el.children.is_empty());
    }

    /// D-39: focus, password, checkbox state and the visible rect are read.
    #[test]
    fn state_fields_are_read() {
        let node = json!({
            "velId": "p", "tag": "input", "x": 0, "y": 700, "w": 200, "h": 40,
            "focused": true, "obscured": true, "checked": null, "vb": [0, 700, 200, 20]
        });
        let el = json_to_element(&node).unwrap();
        assert!(el.focused == Some(true) && el.obscured);
        assert_eq!(el.checked, None);
        assert_eq!(el.visible_bounds.map(|r| r.height), Some(20));
        // Defaults are left out; no `vb`: all of it is on screen.
        let plain =
            json!({"velId": "q", "tag": "input", "x": 1, "y": 2, "w": 3, "h": 4, "checked": false});
        let el = json_to_element(&plain).unwrap();
        assert!(el.focused == Some(false) && !el.obscured);
        assert_eq!(el.selected, Some(false), "the page always knows");
        assert_eq!(el.checked, Some(false));
        assert_eq!(el.visible_bounds, Some(el.bounds));
    }

    #[test]
    fn test_aria_label_preferred_over_dom_id() {
        let node = json!({
            "velId": "x1",
            "tag": "input",
            "domId": "email",
            "text": null,
            "ariaLabel": "Email address",
            "x": 0, "y": 0, "w": 200, "h": 40,
            "visible": true,
            "children": []
        });
        let el = json_to_element(&node).unwrap();
        assert_eq!(el.label.as_deref(), Some("Email address"));
    }

    #[test]
    fn a_flat_list_becomes_the_tree_however_deep() {
        let mut list = vec![json!({"velId": "0", "tag": "body", "p": -1})];
        for i in 1..300 {
            list.push(json!({"velId": i.to_string(), "tag": "div", "p": i - 1}));
        }
        list.push(json!({"velId": "sib", "tag": "p", "p": 0}));
        let tree = tree_from_list(&list).unwrap();
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[1].platform_id, "sib", "order kept");
        let mut depth = 0;
        let mut n = &tree;
        while let Some(c) = n.children.first() {
            depth += 1;
            n = c;
        }
        assert_eq!(depth, 299);
        assert!(tree_from_list(&[json!({"velId": "x", "tag": "a", "p": 5})]).is_some());
        assert!(tree_from_list(&[
            json!({"velId": "x", "tag": "a", "p": -1}),
            json!({"velId": "y", "tag": "a", "p": 7})
        ])
        .is_none());
    }

    #[test]
    fn test_nested_children() {
        let node = json!({
            "velId": "root",
            "tag": "div",
            "domId": null,
            "text": "Parent text",
            "ariaLabel": null,
            "x": 0, "y": 0, "w": 300, "h": 200,
            "visible": true,
            "children": [
                {
                    "velId": "child1",
                    "tag": "span",
                    "domId": null,
                    "text": "Child",
                    "ariaLabel": null,
                    "x": 5, "y": 5, "w": 50, "h": 20,
                    "visible": true,
                    "children": []
                }
            ]
        });
        let el = json_to_element(&node).unwrap();
        assert_eq!(el.children.len(), 1);
        assert_eq!(el.children[0].platform_id, "child1");
    }
}
