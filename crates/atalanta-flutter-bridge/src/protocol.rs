use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use velocity_common::{Element, Rect};

/// Replies from the Flutter test process (`ok` with data, or `error` with a
/// message and a code such as `not_hittable`).
pub use velocity_common::jsonline::Reply as FlutterResponse;

/// What an element action acts on: the semantics/widget node from the last
/// hierarchy, the element's id (`ValueKey` or semantics identifier), and its
/// centre. The Dart side tries them in that order.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Target {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The hierarchy `node` comes from. The Dart side ignores a node from
    /// any other snapshot, whose number may name a different element.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub x: i32,
    pub y: i32,
}

/// Commands sent to the Flutter test process.
#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum FlutterCommand {
    /// Mount the app widget afresh; `clear` also runs the app's reset hook.
    Init {
        target: String,
        width: u32,
        height: u32,
        clear: bool,
        /// Only when configured: size the surface at this ratio.
        #[serde(skip_serializing_if = "Option::is_none")]
        dpr: Option<f64>,
    },
    /// Whether a tap at the target would reach it.
    HitTest { target: Target },
    /// Get the current hierarchy (`mode`: `widgets` or `semantics`).
    GetHierarchy {
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// Capture a screenshot as base64 PNG.
    Screenshot,
    /// Tap the target (`x`/`y` repeat its centre for older servers).
    Tap {
        x: i32,
        y: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    /// Double-tap the target.
    DoubleTap {
        x: i32,
        y: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    /// Long-press the target.
    LongPress {
        x: i32,
        y: i32,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    /// Type into the text field under the target.
    InputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    /// Clear the text field under the target.
    ClearText {
        #[serde(skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
    },
    /// Drag from one point to another.
    Swipe {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    /// Simulate a key press.
    PressKey { key: String },
    /// Pump frames (Flutter-specific: advance animations).
    PumpFrames { count: u32 },
    /// Unmount the app (stopApp); the server keeps running.
    Reset,
    /// Shut the server down.
    Shutdown,
}

/// Legacy nested element from an older Dart server (`children` inline).
#[derive(Debug, Deserialize)]
pub struct FlutterElement {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(rename = "type")]
    pub element_type: String,
    pub bounds: FlutterRect,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub children: Vec<FlutterElement>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub obscured: bool,
    #[serde(default)]
    pub selected: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct FlutterRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl From<FlutterRect> for Rect {
    fn from(b: FlutterRect) -> Self {
        Rect {
            x: b.x,
            y: b.y,
            width: b.width,
            height: b.height,
        }
    }
}

impl FlutterElement {
    pub fn to_element(&self) -> Element {
        Element {
            hint: None,
            platform_id: self.id.clone().unwrap_or_default(),
            label: self.label.clone(),
            text: self.text.clone(),
            element_type: self.element_type.clone(),
            bounds: self.bounds.into(),
            enabled: self.enabled,
            visible: self.visible,
            children: self.children.iter().map(|c| c.to_element()).collect(),
            // The Dart server sends these flags when set: absent is a
            // known `false`.
            focused: Some(self.focused),
            checked: self.checked,
            obscured: self.obscured,
            selected: Some(self.selected),
            ..Default::default()
        }
    }
}

/// One node of a flat hierarchy: `{"format":"flat","nodes":[…]}`, parents
/// listed before their children.
#[derive(Debug, Deserialize)]
pub struct FlutterNode {
    pub node: i64,
    #[serde(default)]
    pub parent: Option<i64>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(rename = "type")]
    pub element_type: String,
    pub bounds: FlutterRect,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// D-39 state from the semantics flags (`isFocused`, `hasCheckedState`
    /// + `isChecked`, `isObscured`); the Dart server sends them when set.
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub obscured: bool,
    /// `isSelected` (S2), sent when set.
    #[serde(default)]
    pub selected: bool,
}

#[derive(Debug, Deserialize)]
struct FlatHierarchy {
    nodes: Vec<FlutterNode>,
    #[serde(default)]
    generation: Option<u64>,
}

/// Identifies an element across hierarchy fetches well enough to find its
/// node again: what selectors match on, plus its bounds.
pub type NodeKey = (String, Option<String>, Option<String>, i32, i32, i32, i32);

pub fn node_key(e: &Element) -> NodeKey {
    (
        e.platform_id.clone(),
        e.label.clone(),
        e.text.clone(),
        e.bounds.x,
        e.bounds.y,
        e.bounds.width,
        e.bounds.height,
    )
}

/// A parsed hierarchy: the element tree, and each element's node id.
pub struct Hierarchy {
    pub root: Element,
    pub nodes: HashMap<NodeKey, i64>,
    /// The snapshot's generation, from servers that number them.
    pub generation: Option<u64>,
}

impl Hierarchy {
    /// Parse a `get_hierarchy` reply: the flat format, or the nested one
    /// older servers send.
    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        if value.get("nodes").is_some() {
            let flat: FlatHierarchy = serde_json::from_value(value)?;
            let mut h = build_tree(flat.nodes)?;
            h.generation = flat.generation;
            return Ok(h);
        }
        // Flatten it without recursion (a derived `Deserialize` recurses
        // once per level, with a large frame) and rebuild it like a flat one.
        // Its synthetic node numbers mean nothing to the server: drop them.
        let mut h = build_tree(flatten_nested(value)?)?;
        h.nodes.clear();
        Ok(h)
    }
}

/// A nested tree as flat nodes, parents first, siblings in order.
fn flatten_nested(root: serde_json::Value) -> anyhow::Result<Vec<FlutterNode>> {
    let mut out = Vec::new();
    let mut stack = vec![(root, None)];
    while let Some((value, parent)) = stack.pop() {
        let serde_json::Value::Object(mut fields) = value else {
            anyhow::bail!("hierarchy node is not an object");
        };
        let children = match fields.remove("children") {
            Some(serde_json::Value::Array(c)) => c,
            _ => Vec::new(),
        };
        let node = out.len() as i64;
        fields.insert("node".into(), node.into());
        fields.insert("parent".into(), serde_json::to_value(parent)?);
        out.push(serde_json::from_value::<FlutterNode>(fields.into())?);
        stack.extend(children.into_iter().rev().map(|c| (c, Some(node))));
    }
    Ok(out)
}

/// Deepest element tree [`build_tree`] builds (the root is depth 0). The
/// matchers, `Element`'s derived `Clone` and `Drop`, and serde all recurse
/// once per level, and a widgets-mode tree of a real app can be far deeper
/// than a tokio worker's stack allows; the iOS parser caps at the same depth.
pub const MAX_DEPTH: usize = 512;

/// Rebuild the tree from a flat list without recursion. A node whose parent
/// is unknown (or listed after it) hangs off the root. A node deeper than
/// [`MAX_DEPTH`] hangs off its ancestor at that depth instead, so it can
/// still be found and tapped, but the tree stays shallow enough to walk.
pub fn build_tree(nodes: Vec<FlutterNode>) -> anyhow::Result<Hierarchy> {
    if nodes.is_empty() {
        anyhow::bail!("hierarchy has no nodes");
    }
    let mut index_of: HashMap<i64, usize> = HashMap::with_capacity(nodes.len());
    let mut parent_of: Vec<usize> = Vec::with_capacity(nodes.len());
    let mut depth_of: Vec<usize> = Vec::with_capacity(nodes.len());
    let mut slots: Vec<Option<Element>> = Vec::with_capacity(nodes.len());
    let mut keys = HashMap::with_capacity(nodes.len());
    let mut flattened = 0usize;
    for (i, n) in nodes.into_iter().enumerate() {
        let mut parent = n
            .parent
            .and_then(|p| index_of.get(&p).copied())
            .unwrap_or(0);
        if i > 0 && depth_of[parent] >= MAX_DEPTH {
            flattened += 1;
            while depth_of[parent] >= MAX_DEPTH {
                parent = parent_of[parent];
            }
        }
        depth_of.push(if i == 0 { 0 } else { depth_of[parent] + 1 });
        index_of.insert(n.node, i);
        parent_of.push(parent);
        let el = Element {
            hint: None,
            platform_id: n.id.unwrap_or_default(),
            label: n.label,
            text: n.text,
            element_type: n.element_type,
            bounds: n.bounds.into(),
            enabled: n.enabled,
            visible: n.visible,
            children: Vec::new(),
            focused: Some(n.focused),
            checked: n.checked,
            obscured: n.obscured,
            selected: Some(n.selected),
            ..Default::default()
        };
        // Twins (a wrapper and its child with the same id, text and bounds)
        // keep the outermost node: its box holds the child's, so the Dart
        // hit check judges the whole widget.
        keys.entry(node_key(&el)).or_insert(n.node);
        slots.push(Some(el));
    }

    if flattened > 0 {
        tracing::debug!(
            "Flutter hierarchy deeper than {MAX_DEPTH} levels: {flattened} nodes moved up"
        );
    }

    // Children have larger indices than their parents: fold from the back,
    // so every element is complete before it moves into its parent.
    for i in (0..slots.len()).rev() {
        let mut el = slots[i].take().expect("each slot is taken once");
        el.children.reverse();
        if i == 0 {
            return Ok(Hierarchy {
                root: el,
                nodes: keys,
                generation: None,
            });
        }
        slots[parent_of[i]]
            .as_mut()
            .expect("parents come before children")
            .children
            .push(el);
    }
    unreachable!("the loop returns at the root")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(n: i64, parent: Option<i64>, text: &str) -> serde_json::Value {
        json!({"node": n, "parent": parent, "text": text, "type": "View",
               "bounds": {"x": 0, "y": n, "width": 10, "height": 10}})
    }

    fn depth(root: &Element) -> usize {
        let mut depth = 0;
        let mut cur = root;
        while let Some(c) = cur.children.first() {
            depth += 1;
            cur = c;
        }
        depth
    }

    fn chain(len: i64) -> serde_json::Value {
        let nodes: Vec<_> = (0..len)
            .map(|i| node(i, if i == 0 { None } else { Some(i - 1) }, &format!("n{i}")))
            .collect();
        json!({"format": "flat", "nodes": nodes})
    }

    #[test]
    fn a_2000_level_flat_chain_is_capped_and_keeps_every_node() {
        let h = Hierarchy::from_value(chain(2000)).unwrap();
        assert_eq!(depth(&h.root), MAX_DEPTH);
        assert_eq!(h.nodes.len(), 2000);
        // The nodes past the cap hang off the deepest kept ancestor, in order.
        let mut cur = &h.root;
        for _ in 0..MAX_DEPTH - 1 {
            cur = &cur.children[0];
        }
        assert_eq!(cur.children.len(), 2000 - MAX_DEPTH);
        assert_eq!(cur.children[0].text.as_deref(), Some("n512"));
        assert_eq!(cur.children.last().unwrap().text.as_deref(), Some("n1999"));
        let sel = velocity_common::Selector::Text("n1999".into());
        assert!(velocity_common::selector_match::find_in_tree(&h.root, &sel, None).is_some());
    }

    /// M6: what the driver does with a hierarchy (cache clone, match,
    /// serialize into a report, drop) fits a 2 MiB tokio worker stack at the
    /// deepest tree `build_tree` makes.
    #[test]
    fn the_deepest_tree_is_safe_on_a_tokio_worker_stack() {
        let h = Hierarchy::from_value(chain(5000)).unwrap();
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let copy = h.root.clone();
                let screen = Rect {
                    x: 0,
                    y: 0,
                    width: 100_000,
                    height: 100_000,
                };
                let mut out = Vec::new();
                velocity_common::selector_match::find_all_in_tree(
                    &copy,
                    &velocity_common::Selector::Text("n4999".into()),
                    Some(&screen),
                    &mut out,
                );
                assert_eq!(out.len(), 1);
                let json = serde_json::to_string(&copy).unwrap();
                assert!(json.len() > 5000);
                drop(copy);
                drop(h);
            })
            .unwrap()
            .join()
            .expect("a deep tree must not overflow a 2 MiB stack");
    }

    #[test]
    fn children_keep_their_order_and_orphans_go_to_the_root() {
        let h = Hierarchy::from_value(json!({"nodes": [
            node(10, None, "root"),
            node(11, Some(10), "a"),
            node(12, Some(11), "a1"),
            node(13, Some(10), "b"),
            node(-1, Some(99), "orphan"),
        ]}))
        .unwrap();
        let texts: Vec<_> = h
            .root
            .children
            .iter()
            .map(|c| c.text.clone().unwrap())
            .collect();
        assert_eq!(texts, ["a", "b", "orphan"]);
        assert_eq!(h.root.children[0].children[0].text.as_deref(), Some("a1"));
        assert_eq!(h.nodes[&node_key(&h.root.children[1])], 13);
    }

    /// D-39: the semantics flags the Dart server sends are mapped, in both
    /// the flat and the nested format.
    #[test]
    fn semantics_state_flags_are_mapped() {
        let b = json!({"x": 0, "y": 0, "width": 10, "height": 10});
        let h = Hierarchy::from_value(json!({"nodes": [
            {"node": 1, "type": "Root", "bounds": b},
            {"node": 2, "parent": 1, "type": "TextField", "bounds": b, "focused": true, "obscured": true},
            {"node": 3, "parent": 1, "type": "Checkbox", "bounds": b, "checked": false},
            {"node": 4, "parent": 1, "type": "Text", "bounds": b},
        ]}))
        .unwrap();
        let k = &h.root.children;
        assert!(k[0].focused == Some(true) && k[0].obscured);
        assert_eq!(k[1].checked, Some(false));
        assert!(k[2].focused == Some(false) && !k[2].obscured);
        assert_eq!(k[2].checked, None);
        let nested = Hierarchy::from_value(json!({"type": "Root", "bounds": b,
            "children": [{"type": "Switch", "bounds": b, "checked": true, "focused": true}]}))
        .unwrap();
        assert_eq!(nested.root.children[0].checked, Some(true));
        assert_eq!(nested.root.children[0].focused, Some(true));
    }

    /// S1/S2: `selected` is mapped, and the semantics types map to roles,
    /// so `{ role: switch, checked: true }` and `{ role: tab… }` work.
    #[test]
    fn selected_and_roles_are_mapped() {
        let b = json!({"x": 0, "y": 0, "width": 10, "height": 10});
        let h = Hierarchy::from_value(json!({"nodes": [
            {"node": 1, "type": "Root", "bounds": b},
            {"node": 2, "parent": 1, "type": "Button", "label": "Home", "bounds": b, "selected": true},
            {"node": 3, "parent": 1, "type": "Switch", "label": "Wi-Fi", "bounds": b, "checked": true},
            {"node": 4, "parent": 1, "type": "Header", "label": "Settings", "bounds": b},
        ]}))
        .unwrap();
        let k = &h.root.children;
        assert_eq!((k[0].selected, k[1].selected), (Some(true), Some(false)));
        use velocity_common::role::role_of;
        assert_eq!(role_of(&k[0]), Some("button"));
        assert_eq!(role_of(&k[1]), Some("switch"));
        assert_eq!(role_of(&k[2]), Some("heading"));
    }

    #[test]
    fn twins_map_to_the_outermost_node() {
        let twin = |n: i64, parent: Option<i64>| {
            json!({"node": n, "parent": parent, "text": "Save", "type": "View",
                   "bounds": {"x": 0, "y": 0, "width": 10, "height": 10}})
        };
        let h = Hierarchy::from_value(json!({"nodes": [
            node(1, None, "root"),
            twin(2, Some(1)),
            twin(3, Some(2)),
        ]}))
        .unwrap();
        assert_eq!(h.nodes[&node_key(&h.root.children[0])], 2);
    }

    /// Review MINOR 13: a nested reply from an older server as deep as the
    /// JSON-lines cap allows parses, converts and drops on a 2 MiB stack.
    #[test]
    fn the_deepest_nested_reply_fits_a_tokio_worker_stack() {
        let mut v = json!({"text": "leaf", "type": "Text",
            "bounds": {"x": 0, "y": 0, "width": 1, "height": 1}});
        // Each level is two JSON levels (`{` and `children: [`).
        let levels = velocity_common::jsonline::MAX_DEPTH / 2 - 2;
        for _ in 0..levels {
            v = json!({"type": "View", "bounds": {"x": 0, "y": 0, "width": 1, "height": 1},
                       "children": [v]});
        }
        let line = format!("{v}\n");
        drop(v);
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let value = velocity_common::jsonline::parse_line(line.as_bytes()).unwrap();
                let h = Hierarchy::from_value(value).unwrap();
                let copy = h.root.clone();
                assert!(velocity_common::selector_match::find_in_tree(
                    &copy,
                    &velocity_common::Selector::Text("leaf".into()),
                    None
                )
                .is_some());
                drop(copy);
                drop(h);
            })
            .unwrap()
            .join()
            .expect("a nested reply at the cap must not overflow a 2 MiB stack");
    }

    #[test]
    fn nested_trees_from_older_servers_still_parse() {
        let h = Hierarchy::from_value(json!({"id": "r", "type": "Root",
            "bounds": {"x": 0, "y": 0, "width": 1, "height": 1},
            "children": [{"text": "hi", "type": "Text",
                          "bounds": {"x": 0, "y": 0, "width": 1, "height": 1}}]}))
        .unwrap();
        assert_eq!(h.root.children[0].text.as_deref(), Some("hi"));
    }
}
