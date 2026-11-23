// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::{Element, Rect};

use crate::render_tree::{ComputedLayout, RenderNode};
use crate::scroll::{scroll_axes, Clip};

/// Convert a render tree to a atalanta-common Element tree.
///
/// Maps RenderNode properties to Element fields:
/// - RenderNode.id → Element.platform_id as `headless:id/<id>`: the same
///   `:id/` form Android uses, so `id: login` matches by its short form
/// - RenderNode.label → Element.label
/// - RenderNode.text → Element.text
/// - RenderNode.node_type → Element.element_type
/// - RenderNode.layout → Element.bounds
/// - RenderNode.style.visible → Element.visible
/// - RenderNode.enabled → Element.enabled
pub fn render_tree_to_element(node: &RenderNode) -> Element {
    render_node_to_element(node, &mut Vec::new(), None, true)
}

/// Like [`render_tree_to_element`], but an element is only visible when it intersects
/// the `width` x `height` screen, as on a device: content laid out below the fold is
/// in the hierarchy but not visible (VEL-109).
///
/// The children of a scroll container are clipped to it, so content scrolled out of a
/// list (under a header above it, say) is not visible either.
pub fn render_tree_to_element_on_screen(node: &RenderNode, width: f32, height: f32) -> Element {
    render_node_to_element(
        node,
        &mut Vec::new(),
        Some(Clip::screen(width, height)),
        true,
    )
}

/// The platform id of render node `id`.
fn headless_id(id: &str) -> String {
    format!("headless:id/{id}")
}

/// Prefix of the platform id of a node the layout gave no id. It is not an
/// `:id/` form, so no `id:` selector matches it (VEL-102: `node_1` used to
/// collide with an app's own `node_1`).
pub const SYNTHETIC_ID_PREFIX: &str = "headless:node#";

/// The platform id of the id-less node at child-index `path` from the root:
/// `headless:node#0.2.1`, unique in the tree.
fn synthetic_id(path: &[usize]) -> String {
    let mut out = String::from(SYNTHETIC_ID_PREFIX);
    out.push('0');
    for i in path {
        out.push('.');
        out.push_str(&i.to_string());
    }
    out
}

/// The child-index path of `target` in `root`, an element tree derived from a
/// render tree (so the same path addresses the render node). Elements are told
/// apart by id, type, bounds, text and label, so two fields sharing an id are
/// two different targets (VEL-339).
///
/// An element whose text changed since it was read (typed into) is found again
/// by everything but its text, so the same element can be typed into twice.
pub fn element_path(root: &Element, target: &Element) -> Option<Vec<usize>> {
    fn same(a: &Element, b: &Element, with_text: bool) -> bool {
        a.platform_id == b.platform_id
            && a.element_type == b.element_type
            && a.bounds == b.bounds
            && (!with_text || a.text == b.text)
            && a.label == b.label
    }
    fn walk(node: &Element, target: &Element, with_text: bool, path: &mut Vec<usize>) -> bool {
        if same(node, target, with_text) {
            return true;
        }
        for (i, child) in node.children.iter().enumerate() {
            path.push(i);
            if walk(child, target, with_text, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    [true, false].into_iter().find_map(|with_text| {
        let mut path = Vec::new();
        walk(root, target, with_text, &mut path).then_some(path)
    })
}

fn render_node_to_element(
    node: &RenderNode,
    path: &mut Vec<usize>,
    clip: Option<Clip>,
    parent_visible: bool,
) -> Element {
    let layout = node.layout.unwrap_or_default();

    // The node's own id, or a tree-path key no `id:` selector can match.
    let platform_id = match &node.id {
        Some(id) => headless_id(id),
        None => synthetic_id(path),
    };

    let bounds = layout_to_rect(&layout);
    let on_screen = clip.is_none_or(|clip| clip.overlaps(&layout));
    let child_clip = match clip {
        Some(clip) if scroll_axes(&node.node_type).is_some() => {
            Some(clip.intersect(Clip::of(&layout)))
        }
        clip => clip,
    };
    // An invisible view keeps its space, and so do its children, but none of them shows.
    let painted = parent_visible && node.style.visible;
    let visible = painted && on_screen && bounds.width > 0 && bounds.height > 0;
    // D-39: the part inside the screen and every scrolling ancestor, when
    // the tree is built against a screen.
    let visible_bounds = clip.map(|clip| {
        let c = clip.intersect(Clip::of(&layout));
        if c.is_empty() {
            Rect {
                x: bounds.x,
                y: bounds.y,
                width: 0,
                height: 0,
            }
        } else {
            layout_to_rect(&ComputedLayout {
                x: c.x0,
                y: c.y0,
                width: c.x1 - c.x0,
                height: c.y1 - c.y0,
            })
        }
    });

    let children: Vec<Element> = node
        .children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            path.push(i);
            let el = render_node_to_element(child, path, child_clip, painted);
            path.pop();
            el
        })
        .collect();

    Element {
        hint: None,
        platform_id,
        label: node.label.clone(),
        text: node.text.clone(),
        element_type: node.node_type.clone(),
        bounds,
        enabled: node.enabled,
        visible,
        children,
        obscured: node.style.password,
        visible_bounds,
        ..Default::default()
    }
}

/// Convert a ComputedLayout to a atalanta-common Rect (f32 → i32).
fn layout_to_rect(layout: &ComputedLayout) -> Rect {
    Rect {
        x: layout.x.round() as i32,
        y: layout.y.round() as i32,
        width: layout.width.round() as i32,
        height: layout.height.round() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_tree::{ComputedLayout, NodeStyle, RenderNode};

    #[test]
    fn test_simple_conversion() {
        let root = RenderNode {
            id: Some("root".to_string()),
            label: Some("Root View".to_string()),
            text: None,
            node_type: "FrameLayout".to_string(),
            style: NodeStyle::default(),
            layout: Some(ComputedLayout {
                x: 0.0,
                y: 0.0,
                width: 1080.0,
                height: 1920.0,
            }),
            taffy_id: None,
            enabled: true,
            children: vec![RenderNode {
                id: Some("greeting".to_string()),
                label: Some("Greeting Label".to_string()),
                text: Some("Hello World".to_string()),
                node_type: "TextView".to_string(),
                style: NodeStyle::default(),
                layout: Some(ComputedLayout {
                    x: 100.0,
                    y: 200.0,
                    width: 400.0,
                    height: 50.0,
                }),
                taffy_id: None,
                enabled: true,
                children: Vec::new(),
            }],
        };

        let element = render_tree_to_element(&root);

        assert_eq!(element.platform_id, "headless:id/root");
        assert_eq!(element.label.as_deref(), Some("Root View"));
        assert_eq!(element.element_type, "FrameLayout");
        assert_eq!(element.bounds.width, 1080);
        assert_eq!(element.bounds.height, 1920);
        assert!(element.visible);
        assert_eq!(element.children.len(), 1);

        let child = &element.children[0];
        assert_eq!(child.platform_id, "headless:id/greeting");
        assert_eq!(child.text.as_deref(), Some("Hello World"));
        assert_eq!(child.bounds.x, 100);
        assert_eq!(child.bounds.y, 200);
    }

    #[test]
    fn test_invisible_node() {
        let node = RenderNode {
            id: Some("hidden".to_string()),
            label: None,
            text: None,
            node_type: "View".to_string(),
            style: NodeStyle {
                visible: false,
                ..Default::default()
            },
            layout: Some(ComputedLayout {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            }),
            taffy_id: None,
            enabled: true,
            children: Vec::new(),
        };

        let element = render_tree_to_element(&node);
        assert!(!element.visible);
    }

    /// D-39: a password field is obscured, and the on-screen variant
    /// reports the part of a node inside the screen.
    #[test]
    fn password_and_visible_bounds_are_mapped() {
        let mut root = RenderNode::container("View").with_child(RenderNode::container("EditText"));
        root.layout = Some(ComputedLayout {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 300.0,
        });
        root.children[0].style.password = true;
        root.children[0].layout = Some(ComputedLayout {
            x: 0.0,
            y: 80.0,
            width: 100.0,
            height: 40.0,
        });
        let tree = render_tree_to_element_on_screen(&root, 100.0, 100.0);
        let field = &tree.children[0];
        assert!(field.obscured && !tree.obscured);
        assert_eq!(
            field.visible_bounds.map(|r| (r.y, r.height)),
            Some((80, 20))
        );
        assert_eq!(tree.visible_bounds.map(|r| r.height), Some(100));
        assert_eq!(
            render_tree_to_element(&root).children[0].visible_bounds,
            None
        );
    }

    #[test]
    fn test_auto_generated_ids() {
        let root = RenderNode::container("View")
            .with_child(RenderNode::container("View"))
            .with_child(RenderNode::container("View"));

        // Set layout for all nodes
        let mut root = root;
        root.layout = Some(ComputedLayout {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        });
        for child in &mut root.children {
            child.layout = Some(ComputedLayout::default());
        }

        let element = render_tree_to_element(&root);

        // Root has no id, so gets a tree-path key
        assert_eq!(element.platform_id, "headless:node#0");
        assert_eq!(element.children[1].platform_id, "headless:node#0.1");
        // Children get unique auto-generated ids
        assert_ne!(
            element.children[0].platform_id,
            element.children[1].platform_id
        );
    }

    #[test]
    fn synthetic_ids_never_match_an_id_selector() {
        // VEL-102: an id-less view used to be `headless:id/node_1`, so
        // `id: node_1` (or `id: 1`) found it instead of the app's own view.
        let mut root = RenderNode::container("View")
            .with_child(RenderNode::container("EditText"))
            .with_child(RenderNode::container("EditText").with_id("node_1"));
        root.layout = Some(ComputedLayout {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        });
        for (i, child) in root.children.iter_mut().enumerate() {
            child.layout = Some(ComputedLayout {
                x: 0.0,
                y: 10.0 * i as f32,
                width: 100.0,
                height: 10.0,
            });
        }
        let tree = render_tree_to_element(&root);
        for id in ["node_1", "1", "0.0", "0"] {
            let found = atalanta_common::selector_match::find_in_tree(
                &tree,
                &atalanta_common::Selector::Id(id.into()),
                None,
            );
            match id {
                "node_1" => assert_eq!(found.unwrap().platform_id, "headless:id/node_1"),
                _ => assert!(found.is_none(), "id: {id} matched {:?}", found),
            }
        }
        let second = &tree.children[1];
        assert_eq!(element_path(&tree, second), Some(vec![1]));
    }

    #[test]
    fn id_selectors_match_headless_elements_by_their_short_id() {
        let el = Element {
            hint: None,
            platform_id: headless_id("login"),
            label: None,
            text: None,
            element_type: "Button".into(),
            bounds: atalanta_common::Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let found = atalanta_common::selector_match::find_in_tree(
            &el,
            &atalanta_common::Selector::Id("login".into()),
            None,
        );
        assert!(found.is_some(), "{}", el.platform_id);
    }
}
