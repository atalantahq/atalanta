//! Headless scrolling.
//!
//! Layout places content at its unscrolled position. A drag moves the scroll offset of
//! the innermost scroll container under its start point, as on a device: the finger
//! moving up shows what is below. Whatever a container can't take (the other axis, or
//! past its end) goes to the container around it, and last to the window, which scrolls
//! when the whole layout is taller or wider than the screen. Offsets are clamped to the
//! content, and children of a scroll container are clipped to it.

use std::collections::HashMap;

use crate::render_tree::{ComputedLayout, RenderNode};

/// The axes a scroll container moves along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxes {
    Vertical,
    Horizontal,
    Both,
}

impl ScrollAxes {
    /// (horizontal, vertical)
    fn mask(self) -> (bool, bool) {
        match self {
            Self::Vertical => (false, true),
            Self::Horizontal => (true, false),
            Self::Both => (true, true),
        }
    }
}

/// The axes `node_type` scrolls along, or `None` when it doesn't scroll.
pub fn scroll_axes(node_type: &str) -> Option<ScrollAxes> {
    match node_type {
        "ScrollView" | "NestedScrollView" | "ListView" | "GridView" | "RecyclerView"
        | "UITableView" => Some(ScrollAxes::Vertical),
        "HorizontalScrollView" => Some(ScrollAxes::Horizontal),
        "UIScrollView" | "UICollectionView" => Some(ScrollAxes::Both),
        _ => None,
    }
}

/// An axis-aligned clip rectangle, `x0..x1` by `y0..y1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Clip {
    pub fn screen(width: f32, height: f32) -> Self {
        Self {
            x0: 0.0,
            y0: 0.0,
            x1: width,
            y1: height,
        }
    }

    pub fn of(l: &ComputedLayout) -> Self {
        Self {
            x0: l.x,
            y0: l.y,
            x1: l.x + l.width,
            y1: l.y + l.height,
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// Whether `l` overlaps this clip by a positive area.
    pub fn overlaps(&self, l: &ComputedLayout) -> bool {
        l.x < self.x1 && l.y < self.y1 && l.x + l.width > self.x0 && l.y + l.height > self.y0
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
}

/// The scroll position of every scrolled container (by its child-index path in the
/// render tree) and of the window.
#[derive(Debug, Clone, Default)]
pub struct ScrollState {
    offsets: HashMap<Vec<usize>, (f32, f32)>,
    window: (f32, f32),
}

impl ScrollState {
    /// Back to the top-left everywhere (a launch).
    pub fn reset(&mut self) {
        self.offsets.clear();
        self.window = (0.0, 0.0);
    }

    /// The offset of the container at `path`.
    pub fn offset(&self, path: &[usize]) -> (f32, f32) {
        self.offsets.get(path).copied().unwrap_or_default()
    }

    /// The window's offset.
    pub fn window_offset(&self) -> (f32, f32) {
        self.window
    }

    /// A copy of the laid-out `tree` with every node moved by the offsets above it.
    pub fn apply(&self, tree: &RenderNode) -> RenderNode {
        let mut out = tree.clone();
        self.shift(&mut out, &mut Vec::new(), -self.window.0, -self.window.1);
        out
    }

    fn shift(&self, node: &mut RenderNode, path: &mut Vec<usize>, dx: f32, dy: f32) {
        if let Some(l) = node.layout.as_mut() {
            l.x += dx;
            l.y += dy;
        }
        let (ox, oy) = self.offset(path);
        for (i, child) in node.children.iter_mut().enumerate() {
            path.push(i);
            self.shift(child, path, dx - ox, dy - oy);
            path.pop();
        }
    }

    /// Clamp every offset to its content after a relayout (the content may have shrunk),
    /// and forget containers that are gone.
    pub fn clamp(&mut self, tree: &RenderNode, screen: (f32, f32)) {
        self.offsets.retain(|path, off| match node_at(tree, path) {
            Some(node) if scroll_axes(&node.node_type).is_some() => {
                let max = container_max(node);
                *off = (off.0.clamp(0.0, max.0), off.1.clamp(0.0, max.1));
                *off != (0.0, 0.0)
            }
            _ => false,
        });
        let max = window_max(tree, screen);
        self.window = (
            self.window.0.clamp(0.0, max.0),
            self.window.1.clamp(0.0, max.1),
        );
    }

    /// A finger dragged from `from` to `to` over the laid-out (unscrolled) `tree`.
    /// The content moves with the finger, one pixel per pixel, as far as it can.
    /// Returns whether anything moved.
    pub fn drag(
        &mut self,
        tree: &RenderNode,
        screen: (f32, f32),
        from: (f32, f32),
        to: (f32, f32),
    ) -> bool {
        self.clamp(tree, screen);
        let shown = self.apply(tree);
        let mut chain = Vec::new();
        hit_chain(
            &shown,
            &mut Vec::new(),
            Clip::screen(screen.0, screen.1),
            from,
            &mut chain,
        );

        // Finger up: the content moves up, the offset grows.
        let mut rest = (from.0 - to.0, from.1 - to.1);
        let mut moved = false;
        for path in chain.iter().rev() {
            let Some(node) = node_at(tree, path) else {
                continue;
            };
            let Some(axes) = scroll_axes(&node.node_type) else {
                continue;
            };
            let cur = self.offset(path);
            let new = consume(cur, &mut rest, container_max(node), axes.mask());
            if new != cur {
                moved = true;
                if new == (0.0, 0.0) {
                    self.offsets.remove(path);
                } else {
                    self.offsets.insert(path.clone(), new);
                }
            }
        }
        let cur = self.window;
        self.window = consume(cur, &mut rest, window_max(tree, screen), (true, true));
        moved || self.window != cur
    }
}

/// Move `cur` by `rest` on the masked axes within `0..=max`, taking what was used
/// out of `rest`.
fn consume(
    cur: (f32, f32),
    rest: &mut (f32, f32),
    max: (f32, f32),
    (h, v): (bool, bool),
) -> (f32, f32) {
    let mut new = cur;
    if h {
        new.0 = (cur.0 + rest.0).clamp(0.0, max.0.max(0.0));
        rest.0 -= new.0 - cur.0;
    }
    if v {
        new.1 = (cur.1 + rest.1).clamp(0.0, max.1.max(0.0));
        rest.1 -= new.1 - cur.1;
    }
    new
}

/// The node at child-index `path` below `root`.
fn node_at<'a>(root: &'a RenderNode, path: &[usize]) -> Option<&'a RenderNode> {
    path.iter().try_fold(root, |node, &i| node.children.get(i))
}

/// The scroll containers under `p`, outermost first, along the topmost branch that has
/// one. Content clipped away by a container doesn't take the touch.
fn hit_chain(
    node: &RenderNode,
    path: &mut Vec<usize>,
    clip: Clip,
    p: (f32, f32),
    chain: &mut Vec<Vec<usize>>,
) -> bool {
    if node.style.gone || !node.style.visible {
        return false;
    }
    let scrolls = scroll_axes(&node.node_type).is_some();
    let mut inner = clip;
    if scrolls {
        inner = clip.intersect(Clip::of(&node.layout.unwrap_or_default()));
        if !inner.contains(p) {
            return false;
        }
    }
    // The last child is drawn on top, so it gets the touch first.
    for i in (0..node.children.len()).rev() {
        path.push(i);
        let mut sub = Vec::new();
        let hit = hit_chain(&node.children[i], path, inner, p, &mut sub);
        path.pop();
        if hit {
            if scrolls {
                chain.push(path.clone());
            }
            chain.append(&mut sub);
            return true;
        }
    }
    if scrolls {
        chain.push(path.clone());
    }
    scrolls
}

/// The right and bottom edges of what `node` contains, stopping at nested scroll
/// containers (their content is clipped to them).
fn content_extent(node: &RenderNode, ext: &mut (f32, f32)) {
    for child in &node.children {
        if child.style.gone {
            continue;
        }
        let l = child.layout.unwrap_or_default();
        ext.0 = ext.0.max(l.x + l.width + child.style.margin.right);
        ext.1 = ext.1.max(l.y + l.height + child.style.margin.bottom);
        if scroll_axes(&child.node_type).is_none() {
            content_extent(child, ext);
        }
    }
}

/// How far the scroll container `node` can scroll: its content (plus its end padding)
/// past its own edges.
fn container_max(node: &RenderNode) -> (f32, f32) {
    let l = node.layout.unwrap_or_default();
    let mut ext = (l.x + l.width, l.y + l.height);
    content_extent(node, &mut ext);
    (
        (ext.0 + node.style.padding.right - (l.x + l.width)).max(0.0),
        (ext.1 + node.style.padding.bottom - (l.y + l.height)).max(0.0),
    )
}

/// How far the window can scroll: the layout past the screen's edges.
fn window_max(root: &RenderNode, (width, height): (f32, f32)) -> (f32, f32) {
    let l = root.layout.unwrap_or_default();
    let mut ext = (l.x + l.width, l.y + l.height);
    if scroll_axes(&root.node_type).is_none() {
        content_extent(root, &mut ext);
    }
    ((ext.0 - width).max(0.0), (ext.1 - height).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mut node: RenderNode, x: f32, y: f32, w: f32, h: f32) -> RenderNode {
        node.layout = Some(ComputedLayout {
            x,
            y,
            width: w,
            height: h,
        });
        node
    }

    #[test]
    fn drags_move_the_innermost_container_and_spill_over() {
        // A 100x100 ScrollView holding 300px of content, inside a 200px-tall window.
        let tree = at(
            RenderNode::container("LinearLayout"),
            0.0,
            0.0,
            100.0,
            200.0,
        )
        .with_child(
            at(RenderNode::container("ScrollView"), 0.0, 0.0, 100.0, 100.0).with_child(at(
                RenderNode::container("View"),
                0.0,
                0.0,
                100.0,
                300.0,
            )),
        );
        let mut s = ScrollState::default();
        assert!(s.drag(&tree, (100.0, 200.0), (50.0, 80.0), (50.0, 30.0)));
        assert_eq!(s.offset(&[0]), (0.0, 50.0));
        // Past the end: clamped at 200, and the window has nothing to scroll.
        s.drag(&tree, (100.0, 200.0), (50.0, 90.0), (50.0, -900.0));
        assert_eq!(s.offset(&[0]), (0.0, 200.0));
        assert_eq!(s.window_offset(), (0.0, 0.0));
        let shown = s.apply(&tree);
        assert_eq!(shown.children[0].children[0].layout.unwrap().y, -200.0);
        // Outside the ScrollView: nothing scrolls.
        assert!(!s.drag(&tree, (100.0, 200.0), (50.0, 150.0), (50.0, 50.0)));
    }
}
