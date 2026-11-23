// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use taffy::prelude::*;

use crate::render_tree::{self, ComputedLayout, EdgeSizes, NodeStyle, Position, RenderNode};
use crate::text::TextMeasurer;

/// What taffy needs to measure a leaf during layout.
enum NodeContext {
    Text {
        text: String,
        font_size: f32,
        font_weight: u16,
    },
}

/// Layout engine that uses Taffy for flexbox computation.
///
/// The taffy tree lives only for one `compute_layout` call, so nothing holding taffy's
/// tagged-pointer lengths outlives it and the engine is `Send + Sync` without `unsafe`.
pub struct LayoutEngine {
    text_measurer: Arc<TextMeasurer>,
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutEngine {
    pub fn new() -> Self {
        Self::with_measurer(Arc::new(TextMeasurer::new()))
    }

    /// An engine that measures text with `text_measurer`, the same one used to paint it.
    pub fn with_measurer(text_measurer: Arc<TextMeasurer>) -> Self {
        Self { text_measurer }
    }

    /// Compute layout for the entire render tree at the given viewport size.
    pub fn compute_layout(
        &mut self,
        root: &mut RenderNode,
        viewport_width: f32,
        viewport_height: f32,
    ) -> Result<(), LayoutError> {
        let mut taffy: TaffyTree<NodeContext> = TaffyTree::new();

        // Build Taffy tree recursively
        let root_id = Self::build_taffy_node(&mut taffy, root)?;

        // Text is measured inside layout (VEL-103): taffy asks for the size under the
        // width it is actually offering, so wrapped text gets the height it paints at.
        let measurer = &self.text_measurer;
        taffy.compute_layout_with_measure(
            root_id,
            Size {
                width: AvailableSpace::Definite(viewport_width),
                height: AvailableSpace::Definite(viewport_height),
            },
            |known, available, _node, context, _style| match context {
                Some(NodeContext::Text {
                    text,
                    font_size,
                    font_weight,
                }) => measure_text(measurer, text, *font_size, *font_weight, known, available),
                None => Size::ZERO,
            },
        )?;

        // Write computed positions back to render nodes
        Self::apply_layout(&taffy, root, root_id, 0.0, 0.0)?;

        Ok(())
    }

    /// Recursively build Taffy nodes from the render tree.
    fn build_taffy_node(
        taffy: &mut TaffyTree<NodeContext>,
        node: &mut RenderNode,
    ) -> Result<NodeId, LayoutError> {
        let style = convert_style(&node.style);

        if node.is_leaf() {
            let context = NodeContext::Text {
                text: node.text.clone().unwrap_or_default(),
                font_size: node.style.font_size,
                font_weight: node.style.font_weight,
            };
            let leaf_id = taffy.new_leaf_with_context(style, context)?;
            node.taffy_id = Some(leaf_id);
            Ok(leaf_id)
        } else {
            // Container node: build children first
            let mut child_ids = Vec::with_capacity(node.children.len());
            for child in &mut node.children {
                let child_id = Self::build_taffy_node(taffy, child)?;
                child_ids.push(child_id);
            }

            let node_id = taffy.new_with_children(style, &child_ids)?;
            node.taffy_id = Some(node_id);
            Ok(node_id)
        }
    }

    /// Write computed layout positions back to render nodes (absolute coordinates).
    fn apply_layout(
        taffy: &TaffyTree<NodeContext>,
        node: &mut RenderNode,
        taffy_id: NodeId,
        parent_x: f32,
        parent_y: f32,
    ) -> Result<(), LayoutError> {
        let layout = taffy.layout(taffy_id)?;

        let abs_x = parent_x + layout.location.x;
        let abs_y = parent_y + layout.location.y;

        node.layout = Some(ComputedLayout {
            x: abs_x,
            y: abs_y,
            width: layout.size.width,
            height: layout.size.height,
        });

        // Apply to children
        let child_ids: Vec<NodeId> = taffy.children(taffy_id)?.to_vec();
        for (i, child) in node.children.iter_mut().enumerate() {
            if let Some(&child_taffy_id) = child_ids.get(i) {
                Self::apply_layout(taffy, child, child_taffy_id, abs_x, abs_y)?;
            }
        }

        Ok(())
    }
}

/// The size of a text leaf under taffy's constraints. A definite width wraps the text;
/// min-content wraps at every opportunity (the longest word); max-content is one line.
fn measure_text(
    measurer: &TextMeasurer,
    text: &str,
    font_size: f32,
    font_weight: u16,
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
) -> Size<f32> {
    if let (Some(width), Some(height)) = (known.width, known.height) {
        return Size { width, height };
    }
    let wrap = known.width.or(match available.width {
        AvailableSpace::Definite(w) => Some(w),
        AvailableSpace::MinContent => Some(0.0),
        AvailableSpace::MaxContent => None,
    });
    // `measure` treats a zero wrap width as unbounded; wrap as tightly as it allows.
    let wrap = wrap.map(|w| w.max(1.0));
    let (w, h) = measurer.measure_weighted(text, font_size, font_weight, wrap);
    Size {
        width: known.width.unwrap_or(w),
        height: known.height.unwrap_or(h),
    }
}

/// Convert our NodeStyle to Taffy Style.
fn convert_style(style: &NodeStyle) -> Style {
    Style {
        // Only `gone` leaves the layout; `invisible` keeps its space (VEL-106).
        display: if style.gone {
            Display::None
        } else {
            Display::Flex
        },
        position: match style.position {
            Position::Relative => taffy::Position::Relative,
            Position::Absolute => taffy::Position::Absolute,
        },
        flex_direction: style.flex_direction,
        flex_wrap: style.flex_wrap,
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        flex_basis: convert_dimension(style.flex_basis),
        justify_content: style.justify_content,
        align_items: style.align_items,
        align_self: style.align_self,
        justify_self: style.justify_self,
        size: Size {
            width: convert_dimension(style.width),
            height: convert_dimension(style.height),
        },
        min_size: Size {
            width: convert_dimension(style.min_width),
            height: convert_dimension(style.min_height),
        },
        max_size: Size {
            width: convert_dimension(style.max_width),
            height: convert_dimension(style.max_height),
        },
        margin: convert_edges_auto(&style.margin),
        padding: convert_edges(&style.padding),
        gap: Size {
            width: length(style.gap),
            height: length(style.gap),
        },
        inset: Rect {
            top: convert_inset(style.inset.top),
            right: convert_inset(style.inset.right),
            bottom: convert_inset(style.inset.bottom),
            left: convert_inset(style.inset.left),
        },
        ..Default::default()
    }
}

/// Convert our Dimension to Taffy Dimension.
fn convert_dimension(dim: render_tree::Dimension) -> taffy::Dimension {
    match dim {
        render_tree::Dimension::Auto => taffy::Dimension::auto(),
        render_tree::Dimension::Length(px) => taffy::Dimension::length(px),
        render_tree::Dimension::Percent(fraction) => taffy::Dimension::percent(fraction),
    }
}

/// Convert EdgeSizes to Taffy Rect<LengthPercentage>.
fn convert_edges(edges: &EdgeSizes) -> Rect<LengthPercentage> {
    Rect {
        top: length(edges.top),
        right: length(edges.right),
        bottom: length(edges.bottom),
        left: length(edges.left),
    }
}

/// Convert EdgeSizes to Taffy Rect<LengthPercentageAuto>.
fn convert_edges_auto(edges: &EdgeSizes) -> Rect<LengthPercentageAuto> {
    Rect {
        top: LengthPercentageAuto::length(edges.top),
        right: LengthPercentageAuto::length(edges.right),
        bottom: LengthPercentageAuto::length(edges.bottom),
        left: LengthPercentageAuto::length(edges.left),
    }
}

fn convert_inset(value: Option<f32>) -> LengthPercentageAuto {
    match value {
        Some(value) => LengthPercentageAuto::length(value),
        None => LengthPercentageAuto::auto(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("Taffy layout error: {0}")]
    Taffy(#[from] taffy::TaffyError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_tree::{Dimension, RenderNode};

    #[test]
    fn test_simple_vertical_layout() {
        let mut root = RenderNode::container("LinearLayout")
            .with_style(NodeStyle {
                width: Dimension::length(400.0),
                height: Dimension::length(800.0),
                flex_direction: FlexDirection::Column,
                ..Default::default()
            })
            .with_child(
                RenderNode::text_node("Hello", "TextView")
                    .with_id("text1")
                    .with_style(NodeStyle {
                        width: Dimension::length(400.0),
                        height: Dimension::length(50.0),
                        ..Default::default()
                    }),
            )
            .with_child(
                RenderNode::text_node("World", "TextView")
                    .with_id("text2")
                    .with_style(NodeStyle {
                        width: Dimension::length(400.0),
                        height: Dimension::length(50.0),
                        ..Default::default()
                    }),
            );

        let mut engine = LayoutEngine::new();
        engine.compute_layout(&mut root, 400.0, 800.0).unwrap();

        // Root at (0,0) with full size
        let root_layout = root.layout.unwrap();
        assert_eq!(root_layout.x, 0.0);
        assert_eq!(root_layout.y, 0.0);
        assert_eq!(root_layout.width, 400.0);

        // First child at (0,0)
        let child1 = root.children[0].layout.unwrap();
        assert_eq!(child1.x, 0.0);
        assert_eq!(child1.y, 0.0);
        assert_eq!(child1.height, 50.0);

        // Second child below first at (0,50)
        let child2 = root.children[1].layout.unwrap();
        assert_eq!(child2.x, 0.0);
        assert_eq!(child2.y, 50.0);
        assert_eq!(child2.height, 50.0);
    }

    #[test]
    fn test_horizontal_layout_with_flex_grow() {
        let mut root = RenderNode::container("LinearLayout")
            .with_style(NodeStyle {
                width: Dimension::length(300.0),
                height: Dimension::length(100.0),
                flex_direction: FlexDirection::Row,
                ..Default::default()
            })
            .with_child(RenderNode::container("View").with_style(NodeStyle {
                flex_grow: 1.0,
                height: Dimension::length(100.0),
                ..Default::default()
            }))
            .with_child(RenderNode::container("View").with_style(NodeStyle {
                flex_grow: 2.0,
                height: Dimension::length(100.0),
                ..Default::default()
            }));

        let mut engine = LayoutEngine::new();
        engine.compute_layout(&mut root, 300.0, 100.0).unwrap();

        // flex_grow 1:2 ratio means 100px and 200px
        let child1 = root.children[0].layout.unwrap();
        let child2 = root.children[1].layout.unwrap();
        assert_eq!(child1.width, 100.0);
        assert_eq!(child2.width, 200.0);
        assert_eq!(child2.x, 100.0);
    }
}
