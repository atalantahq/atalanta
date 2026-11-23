use taffy::FlexDirection;

use crate::render_tree::{Color, NodeStyle, RenderNode, SafeAreaInsets};

use super::constraints::SimpleConstraintSolver;
use super::resources::IosResourceLoader;
use super::xib::{XibConstraint, XibDocument, XibObject};

/// Inflates iOS XIB/Storyboard views into RenderNode trees.
pub struct IosInflater {
    // Reserved for named-color/asset-catalog resolution; every call site currently
    // passes IosResourceLoader::empty().
    #[allow(dead_code)]
    resources: IosResourceLoader,
    safe_area_insets: SafeAreaInsets,
    /// Properties parsed but not drawn (D-47).
    unrendered: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl IosInflater {
    pub fn new(resources: IosResourceLoader) -> Self {
        Self {
            resources,
            safe_area_insets: SafeAreaInsets::default(),
            unrendered: Default::default(),
        }
    }

    /// The properties the inflates so far parsed but headless can't draw
    /// (D-47), each once, emptied.
    pub fn take_unrendered(&self) -> Vec<String> {
        std::mem::take(&mut *self.unrendered.borrow_mut())
            .into_iter()
            .collect()
    }

    pub fn with_safe_area_insets(mut self, safe_area_insets: SafeAreaInsets) -> Self {
        self.safe_area_insets = safe_area_insets;
        self
    }

    /// Inflate a XIB document into a RenderNode tree.
    pub fn inflate(&self, doc: &XibDocument) -> Result<RenderNode, IosInflateError> {
        let root_obj = doc.objects.first().ok_or(IosInflateError::NoViews)?;
        self.inflate_object(root_obj, None, &[], FlexDirection::Column, 0)
    }

    /// Inflate a single XIB object recursively.
    fn inflate_object(
        &self,
        obj: &XibObject,
        parent_id: Option<&str>,
        parent_constraints: &[XibConstraint],
        parent_flex_direction: FlexDirection,
        depth: usize,
    ) -> Result<RenderNode, IosInflateError> {
        // Layout, paint and the element tree all recurse per level (VEL-108).
        if depth >= crate::android::inflate::MAX_VIEW_DEPTH {
            return Err(IosInflateError::TooDeep(
                crate::android::inflate::MAX_VIEW_DEPTH,
            ));
        }
        let view_name = &obj.class;
        self.unrendered
            .borrow_mut()
            .extend(crate::unrendered::ios(&obj.properties));
        let mut style = self.map_uikit_style(obj);

        // Apply constraints
        let constraints = own_constraints(obj, parent_id, parent_constraints);
        SimpleConstraintSolver::apply_constraints(
            &constraints,
            &obj.id,
            parent_id,
            parent_flex_direction,
            self.safe_area_insets,
            &mut style,
        );

        // Apply frame-based layout if present and no explicit size
        if let Some(frame) = &obj.frame {
            if style.width.is_auto() {
                style.width = crate::render_tree::Dimension::length(frame.width);
            }
            if style.height.is_auto() {
                style.height = crate::render_tree::Dimension::length(frame.height);
            }
        }

        // Apply UIStackView properties
        if view_name == "UIStackView" {
            SimpleConstraintSolver::apply_stack_view_properties(&obj.properties, &mut style);
        }

        // Extract text content
        let own_text = obj
            .properties
            .get("text")
            .or_else(|| obj.properties.get("title"))
            .cloned();
        let placeholder = own_text.is_none() && obj.properties.contains_key("placeholder");
        let text = own_text.or_else(|| obj.properties.get("placeholder").cloned());

        let mut node = if text.is_some() {
            RenderNode::text_node(text.clone().unwrap(), view_name)
        } else {
            RenderNode::container(view_name)
        };

        // Set properties
        node.id = obj
            .properties
            .get("accessibilityIdentifier")
            .cloned()
            .or_else(|| {
                if !obj.id.is_empty() {
                    Some(obj.id.clone())
                } else {
                    None
                }
            });
        node.label = obj
            .properties
            .get("accessibilityLabel")
            .cloned()
            .or(text.clone());
        node.style = style;
        node.style.placeholder = placeholder;
        node.enabled = obj
            .properties
            .get("userInteractionEnabled")
            .map(|v| v != "NO")
            .unwrap_or(true);

        // Inflate subviews
        let is_fill_equally = obj
            .properties
            .get("distribution")
            .map(|d| d == "fillEqually")
            .unwrap_or(false);

        for subview in &obj.subviews {
            let mut child = self.inflate_object(
                subview,
                Some(&obj.id),
                &obj.constraints,
                node.style.flex_direction,
                depth + 1,
            )?;

            // For fillEqually stack views, set flex_grow on children
            if is_fill_equally {
                child.style.flex_grow = 1.0;
            }
            // A scroll view's content keeps its size and overflows, to be scrolled
            // to; it isn't squashed to fit.
            if crate::scroll::scroll_axes(view_name).is_some() {
                child.style.flex_shrink = 0.0;
            }

            node.children.push(child);
        }

        Ok(node)
    }

    /// Map UIKit view properties to NodeStyle.
    fn map_uikit_style(&self, obj: &XibObject) -> NodeStyle {
        let mut style = NodeStyle::default();

        // Hidden property
        // Treated as removed from layout, as UIStackView does with hidden arranged views.
        if let Some(hidden) = obj.properties.get("hidden") {
            style.visible = hidden != "YES";
            style.gone = hidden == "YES";
        }

        // Alpha
        if let Some(alpha) = obj.properties.get("alpha") {
            if let Ok(a) = alpha.parse::<f32>() {
                style.opacity = a;
                // Fully transparent views keep their frame.
                if a == 0.0 {
                    style.visible = false;
                }
            }
        }

        // Background color
        if let Some(hex) = obj.properties.get("color_backgroundColor") {
            if let Some(argb) = crate::android::resources::ResourceTable::parse_color(hex) {
                style.background_color = Color::from_argb(argb);
            }
        }

        // Text color
        if let Some(hex) = obj.properties.get("color_textColor") {
            if let Some(argb) = crate::android::resources::ResourceTable::parse_color(hex) {
                style.text_color = Color::from_argb(argb);
            }
        }

        // Font size
        if let Some(size) = obj.properties.get("fontSize") {
            if let Ok(s) = size.parse::<f32>() {
                style.font_size = s;
            }
        }

        // Bold: `<fontDescription type="boldSystem">` or `weight="bold"` (VT-9 B7).
        let bold_type = obj
            .properties
            .get("font_type")
            .is_some_and(|t| t.to_ascii_lowercase().contains("bold"));
        let weight = obj.properties.get("font_weight").map(|w| w.as_str());
        if bold_type || matches!(weight, Some("bold" | "heavy" | "black" | "semibold")) {
            style.font_weight = 700;
        }
        if obj.properties.get("secureTextEntry").map(|v| v.as_str()) == Some("YES") {
            style.password = true;
        }

        // Number of lines (affects text measurement constraints)
        if let Some(lines) = obj.properties.get("numberOfLines") {
            if lines == "1" {
                // Single line: no wrapping
            }
        }

        // Content mode (for images)
        if let Some(mode) = obj.properties.get("contentMode") {
            match mode.as_str() {
                "scaleAspectFit" | "scaleAspectFill" => {
                    // Image scaling modes — keep aspect ratio
                }
                "scaleToFill" => {
                    // Stretch to fill
                }
                _ => {}
            }
        }

        style
    }
}

/// The constraints that size and place `obj`. Interface Builder stores a
/// constraint on the nearest common ancestor, so a subview's pins to its
/// superview (and often its width or height) live in the superview's list;
/// they were ignored, and a superview applied its subviews' width constraints
/// to itself (VEL-S23).
///
/// - `obj`'s own list: those about `obj` itself (no `firstItem`, or `obj`),
///   minus the ones relating it to one of its subviews (about the subview).
/// - The superview's list: those whose `firstItem` is `obj`, and those
///   written superview-first (`superview.trailing = obj.trailing + 16`, IB's
///   usual form for trailing and bottom pins), turned around to name `obj`
///   first. Their constant stays the inset, as the solver reads it.
fn own_constraints(
    obj: &XibObject,
    parent_id: Option<&str>,
    parent_constraints: &[XibConstraint],
) -> Vec<XibConstraint> {
    let is_child = |id: &str| obj.subviews.iter().any(|s| s.id == id);
    let mut out: Vec<XibConstraint> = obj
        .constraints
        .iter()
        .filter(|c| c.first_item.as_deref().is_none_or(|f| f == obj.id))
        .filter(|c| !c.second_item.as_deref().is_some_and(is_child))
        .cloned()
        .collect();
    for c in parent_constraints {
        if c.first_item.as_deref() == Some(obj.id.as_str()) {
            out.push(c.clone());
        } else if c.second_item.as_deref() == Some(obj.id.as_str())
            && c.first_item.as_deref().is_none_or(|f| Some(f) == parent_id)
        {
            let mut turned = c.clone();
            turned.first_item = Some(obj.id.clone());
            turned.second_item = parent_id.map(str::to_string);
            if let Some(attr) = c.second_attribute.clone() {
                turned.first_attribute = attr;
            }
            turned.second_attribute = Some(c.first_attribute.clone());
            out.push(turned);
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum IosInflateError {
    #[error("No views found in XIB document")]
    NoViews,
    #[error("Layout nests views deeper than {0} levels")]
    TooDeep(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ios::xib::{XibDocument, XibFrame, XibObject};
    use std::collections::HashMap;

    fn make_label(id: &str, text: &str) -> XibObject {
        let mut props = HashMap::new();
        props.insert("text".to_string(), text.to_string());

        XibObject {
            class: "UILabel".to_string(),
            id: id.to_string(),
            properties: props,
            subviews: Vec::new(),
            constraints: Vec::new(),
            frame: Some(XibFrame {
                x: 20.0,
                y: 100.0,
                width: 335.0,
                height: 21.0,
            }),
        }
    }

    #[test]
    fn test_inflate_simple_hierarchy() {
        let doc = XibDocument {
            objects: vec![XibObject {
                class: "UIView".to_string(),
                id: "root".to_string(),
                properties: HashMap::new(),
                subviews: vec![
                    make_label("title", "Hello"),
                    make_label("subtitle", "World"),
                ],
                constraints: Vec::new(),
                frame: Some(XibFrame {
                    x: 0.0,
                    y: 0.0,
                    width: 375.0,
                    height: 812.0,
                }),
            }],
            connections: Vec::new(),
        };

        let inflater = IosInflater::new(IosResourceLoader::empty());
        let root = inflater.inflate(&doc).unwrap();

        assert_eq!(root.node_type, "UIView");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].text.as_deref(), Some("Hello"));
        assert_eq!(root.children[1].text.as_deref(), Some("World"));
    }

    #[test]
    fn test_inflate_stack_view() {
        let mut stack_props = HashMap::new();
        stack_props.insert("axis".to_string(), "horizontal".to_string());
        stack_props.insert("spacing".to_string(), "8".to_string());
        stack_props.insert("distribution".to_string(), "fillEqually".to_string());

        let doc = XibDocument {
            objects: vec![XibObject {
                class: "UIStackView".to_string(),
                id: "stack".to_string(),
                properties: stack_props,
                subviews: vec![make_label("l1", "Left"), make_label("l2", "Right")],
                constraints: Vec::new(),
                frame: None,
            }],
            connections: Vec::new(),
        };

        let inflater = IosInflater::new(IosResourceLoader::empty());
        let root = inflater.inflate(&doc).unwrap();

        assert_eq!(root.node_type, "UIStackView");
        assert_eq!(root.style.flex_direction, taffy::FlexDirection::Row);
        assert_eq!(root.style.gap, 8.0);
        // Children should have flex_grow from fillEqually
        assert_eq!(root.children[0].style.flex_grow, 1.0);
        assert_eq!(root.children[1].style.flex_grow, 1.0);
    }
}
