use velocity_common::{Element, Rect};

/// JavaScript DOM walker that serialises the live page into a JSON tree.
///
/// A single `Runtime.evaluate` call executes this script and returns the
/// entire element hierarchy in one CDP round-trip regardless of DOM size.
///
/// Each node receives a stable `data-vel-id` attribute (assigned once per
/// page-load) that becomes the `platform_id` field of `velocity_common::Element`.
pub const DOM_WALKER_SCRIPT: &str = r#"
(function walk(node, depth) {
    if (!node || depth > 40) return null;
    var style = window.getComputedStyle(node);
    var visible = style.display !== 'none' &&
                  style.visibility !== 'hidden' &&
                  parseFloat(style.opacity) > 0;
    var rect = node.getBoundingClientRect();

    // Assign a stable ID if not already present.
    if (!node.dataset.velId) {
        node.dataset.velId = Math.random().toString(36).slice(2, 10);
    }

    var children = [];
    for (var i = 0; i < node.children.length; i++) {
        var c = walk(node.children[i], depth + 1);
        if (c) children.push(c);
    }

    // Trim text to 200 chars to avoid huge payloads.
    var rawText = (node.textContent || '').trim();
    var ownText = rawText.length > 200 ? rawText.slice(0, 200) : rawText;

    return {
        velId:      node.dataset.velId,
        tag:        node.tagName.toLowerCase(),
        domId:      node.id || null,
        text:       ownText || null,
        ariaLabel:  node.getAttribute('aria-label') || null,
        role:       node.getAttribute('role') || null,
        className:  node.className || null,
        x:          Math.round(rect.x),
        y:          Math.round(rect.y),
        w:          Math.round(rect.width),
        h:          Math.round(rect.height),
        visible:    visible,
        children:   children
    };
})(document.body, 0)
"#;

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

    let children = node["children"]
        .as_array()
        .map(|arr| arr.iter().filter_map(json_to_element).collect())
        .unwrap_or_default();

    Some(Element {
        platform_id: vel_id,
        label,
        text,
        element_type: tag,
        bounds: Rect {
            x,
            y,
            width: w,
            height: h,
        },
        enabled: true,
        visible,
        children,
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
