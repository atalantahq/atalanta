//! Where to tap an element (VEL-341).
//!
//! XCUITest frames are not clipped by the screen, and nothing in `/source`
//! says what is drawn on top, so the centre of the frame can be a keyboard
//! key, the status bar or a point off the screen, and the tap "succeeds".
//! WDA's element click uses XCTest's own hit point and Maestro checks the
//! topmost node at the point; Velocity's Android driver has
//! `uncovered_point`. This is the frame-only version (EarlGrey's "quick"
//! visibility check): clip to the screen, keep off the bars that are always
//! in front (keyboard, navigation bar, tab bar, toolbar, in-app alert, status
//! bar), and pick the centre of what is left, else a point of a 5×5 grid.
//! A modal layer (an action sheet, a popover, an in-app alert, or the
//! dimming view XCTest reports as `PopoverDismissRegion`) covers everything
//! presented before it: a tap there lands on its dimming view and dismisses
//! it, so the target is refused rather than tapped (review 09 #2).

use velocity_common::{Element, Rect};

/// Why no point of the target can be tapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// No part of it is on the screen.
    OffScreen,
    /// Everything on screen is under this ("keyboard", "status bar", …).
    Covered(&'static str),
}

impl std::fmt::Display for Blocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OffScreen => f.write_str("off screen"),
            Self::Covered(what) => write!(f, "covered by the {what}"),
        }
    }
}

/// A switch laid out as a whole row (a SwiftUI Toggle with its label): far
/// wider than tall. A bare UISwitch is about 51x31 and keeps its centre.
pub fn is_row_switch(el: &Element) -> bool {
    let t = el
        .element_type
        .strip_prefix("XCUIElementType")
        .unwrap_or(&el.element_type);
    t == "Switch" && el.bounds.width > 3 * el.bounds.height.max(1)
}

/// Where a row switch's knob sits when the row is mirrored (RTL: ar, he,
/// fa): its leading end. [`tap_point`] aims at the trailing end, which is
/// the label there; the driver taps here when that tap left the switch's
/// value as it was (review 09 #11). `None` when it is not a row switch,
/// no part of it is on screen, or the point is covered.
pub fn row_switch_leading_point(tree: &Element, target: &Element) -> Option<(i32, i32)> {
    if !is_row_switch(target) {
        return None;
    }
    let visible = intersect(&target.bounds, &tree.bounds)?;
    let p = (
        visible.x + (visible.height / 2).clamp(12, 40),
        visible.y + visible.height / 2,
    );
    let mut covers = Vec::new();
    collect_occluders(tree, target, &mut covers);
    if covers.iter().any(|(r, _)| contains(r, p.0, p.1)) || behind_modal(tree, target).is_some() {
        return None;
    }
    Some(p)
}

/// The node of `tree` that is `target` (same type and frame, and its
/// identifier or label): how a read after a tap finds the switch again.
pub fn find_again<'a>(tree: &'a Element, target: &Element) -> Option<&'a Element> {
    if same(tree, target) {
        return Some(tree);
    }
    tree.children.iter().find_map(|c| find_again(c, target))
}

/// What a node of this type covers when it isn't the target's own (types
/// drawn above the content in every UIKit and SwiftUI app). The system
/// alert Velocity adds to the tree (`XCUIElementTypeAlert`, a full-screen
/// frame) is left out: it is handled by the dialog handler.
fn occluder_kind(element_type: &str) -> Option<&'static str> {
    Some(match element_type {
        "Keyboard" => "keyboard",
        "NavigationBar" => "navigation bar",
        "TabBar" => "tab bar",
        "Toolbar" => "toolbar",
        _ => return None,
    })
}

/// The identifier XCTest gives the dimming view behind a popover, a form or
/// page sheet and an action sheet ("dismiss popup"): a tap on it dismisses
/// what is presented.
const DISMISS_REGION: &str = "PopoverDismissRegion";

/// A modal layer: what is presented over the screen, blocking every touch
/// outside it. Its dimming view is not in `/source` (or, as the
/// `PopoverDismissRegion`, is a leaf before the content), so its own frame
/// says nothing about what it blocks: everything listed before it is
/// covered. `Alert` is an in-app `UIAlertController` (the system alert
/// Velocity adds keeps its `XCUIElementTypeAlert` type and is the dialog
/// handler's).
fn modal_kind(node: &Element) -> Option<&'static str> {
    if node.platform_id == DISMISS_REGION {
        return Some("sheet's dimming view");
    }
    Some(match node.element_type.as_str() {
        "Sheet" => "sheet",
        "Popover" => "popover",
        "Alert" => "alert",
        _ => return None,
    })
}

/// Whether a modal layer may be up anywhere in `tree`: one of the modal
/// node types, or two navigation bars on screen. A SwiftUI page sheet on
/// iOS 26 has no Sheet, Popover or `PopoverDismissRegion` node at all; it
/// shows only as a second `NavigationBar` later in the tree, so the
/// visibility read never ran and taps behind the sheet went through
/// (2026-11-11, Finding B). The visibility read settles which content is
/// really hidden; two bars side by side (an iPad split view) only cost
/// that read.
pub fn has_modal_layer(tree: &Element) -> bool {
    fn any_modal(n: &Element) -> bool {
        modal_kind(n).is_some() || n.children.iter().any(any_modal)
    }
    fn bars(n: &Element, screen: &Rect, count: &mut usize) {
        if n.element_type == "NavigationBar" && n.visible && intersect(&n.bounds, screen).is_some()
        {
            *count += 1;
        }
        for c in &n.children {
            bars(c, screen, count);
        }
    }
    if any_modal(tree) {
        return true;
    }
    let mut count = 0;
    bars(tree, &tree.bounds, &mut count);
    count >= 2
}

/// Copy `src`'s `visible` flags onto `dst`, node by node, when the two
/// trees have the same shape (type and frame of every node `src` holds;
/// `dst` may hold more trailing children: the system alert Velocity adds).
/// False, with `dst` partly updated, when they differ: the caller drops it.
pub fn copy_visibility(dst: &mut Element, src: &Element) -> bool {
    if dst.element_type != src.element_type
        || dst.bounds != src.bounds
        || dst.children.len() < src.children.len()
    {
        return false;
    }
    dst.visible = src.visible;
    dst.children
        .iter_mut()
        .zip(&src.children)
        .all(|(d, s)| copy_visibility(d, s))
}

/// The modal layer `target` is behind, if any: one listed after the target
/// (presented later, so drawn over it) that neither holds it nor is inside
/// it. A target the tree doesn't hold (WDA found it beyond the snapshot
/// depth) can't be ordered: it is behind any modal layer whose frame doesn't
/// hold all of it. A layer the runner reports as not visible is gone.
pub fn behind_modal(tree: &Element, target: &Element) -> Option<&'static str> {
    fn walk(
        n: &Element,
        target: &Element,
        i: &mut usize,
        target_at: &mut Option<usize>,
        out: &mut Vec<(usize, &'static str, Rect, bool)>,
    ) {
        let me = *i;
        *i += 1;
        if target_at.is_none() && same(n, target) {
            *target_at = Some(me);
        }
        if let Some(kind) = modal_kind(n) {
            if n.visible && !contains_node(target, n) {
                out.push((me, kind, n.bounds, contains_node(n, target)));
            }
        }
        for c in &n.children {
            walk(c, target, i, target_at, out);
        }
    }
    let (mut i, mut target_at, mut layers) = (0, None, Vec::new());
    walk(tree, target, &mut i, &mut target_at, &mut layers);
    layers
        .into_iter()
        .filter(|&(_, _, _, holds)| !holds)
        .find(|&(order, _, bounds, _)| match target_at {
            Some(t) => order > t,
            None => !covers_all(&bounds, &target.bounds),
        })
        .map(|(_, kind, _, _)| kind)
}

/// A point inside `target` that is on the screen and under none of the
/// occluders in `tree`, preferring the centre of its on-screen part.
/// `status_bar` is the status bar's height in points (0 when unknown or
/// hidden). A point below the status bar is preferred (review M2). When the
/// only free part of the target is under the status bar, the tap fails:
/// on iOS a tap there scrolls to the top instead of reaching the target
/// (VEL-341). An app that hides its bar while WDA still reports a height
/// gets a loud failure naming `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off` (which
/// passes 0 here), never a silent wrong tap.
pub fn tap_point(tree: &Element, target: &Element, status_bar: i32) -> Result<(i32, i32), Blocker> {
    let screen = tree.bounds;
    let visible = intersect(&target.bounds, &screen).ok_or(Blocker::OffScreen)?;

    let mut covers: Vec<(Rect, &'static str)> = Vec::new();
    // A target WDA found but the tree doesn't hold (beyond the snapshot
    // depth) can't be told apart from its own bar or keyboard by identity:
    // a bar around all of it is taken to be its container (review M3).
    let in_tree = contains_node(tree, target);
    collect_occluders(tree, target, &mut covers);
    if in_tree {
        covers.retain(|(r, kind)| {
            let bar_type = match *kind {
                "tab bar" => "TabBar",
                "navigation bar" => "NavigationBar",
                _ => return true,
            };
            !presented_over(tree, target, r, bar_type)
        });
    }
    if !in_tree {
        covers.retain(|(r, _)| !covers_all(r, &target.bounds));
    }
    if let Some(kind) = behind_modal(tree, target) {
        return Err(Blocker::Covered(kind));
    }
    let bar = (status_bar > 0).then_some(Rect {
        x: screen.x,
        y: screen.y,
        width: screen.width,
        height: status_bar,
    });

    let covered = |(x, y): (i32, i32)| covers.iter().find(|(r, _)| contains(r, x, y));
    let under_bar = |(x, y): (i32, i32)| bar.as_ref().is_some_and(|r| contains(r, x, y));
    let centre = visible.center();
    let mut grid = Vec::new();
    // A SwiftUI Toggle's switch element spans its whole row (label and
    // knob): the centre is the label, and a tap there leaves the switch as
    // it was ("Outdoor Seating" never turned on: iOSWorld, 2026-11-08).
    // Aim at the knob, at the trailing end, first.
    if is_row_switch(target) {
        let knob_x = visible.x + visible.width - (visible.height / 2).clamp(12, 40);
        grid.push((knob_x, visible.y + visible.height / 2));
    }
    // Then the centre and the grid every platform's tap guard tries.
    grid.extend(velocity_common::tap_guard::candidate_points(&visible));
    let mut free = grid.iter().copied().filter(|&p| covered(p).is_none());
    let Some(first_free) = free.next() else {
        let (_, what) = covered(centre).copied().unwrap_or((screen, "screen edge"));
        return Err(Blocker::Covered(what));
    };
    std::iter::once(first_free)
        .chain(free)
        .find(|&p| !under_bar(p))
        .ok_or(Blocker::Covered("status bar"))
}

/// D-38 (`ios.verifyTap`): whether a tap that the runner's hit test says
/// lands on `hit` (the deepest element drawn there, `hit_type` without the
/// `XCUIElementType` prefix) reaches `target`. Yes when `hit` is the
/// target, inside it, or one of its ancestors (the target is beyond the
/// snapshot depth, or not hittable itself), going by the nodes of `tree`
/// with that frame and type; a node the tree doesn't hold counts when its
/// frame lies inside the target's. Anything else is drawn over the target.
pub fn hit_lands_on(tree: &Element, target: &Element, hit: &Rect, hit_type: &str) -> bool {
    let mut matches = Vec::new();
    find_by_frame(tree, hit, hit_type, &mut matches);
    if matches.is_empty() {
        return covers_all(&target.bounds, hit);
    }
    matches.iter().any(|n| {
        contains_node(n, target)
            || contains_node(target, n)
            || find_in_tree(tree, target).is_some_and(|t| contains_node(t, n))
    })
}

/// A node the runner's hit test reports (D-38): its type (no
/// `XCUIElementType` prefix), identifier, label and frame; where it sits in
/// the app's snapshot (`path`: child indices from the root, and `chain`:
/// the nodes from the root down to it), and what the runner could read of
/// whether it takes touches. `None` flags: the runner couldn't say.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HitNode {
    pub element_type: String,
    pub label: String,
    pub identifier: String,
    pub bounds: Rect,
    pub path: Option<Vec<usize>>,
    /// Root first, this node last (each without its own chain).
    pub chain: Vec<HitNode>,
    /// `isAccessibilityElement`. Reported, never a reason to pass a touch
    /// through: it is `NO` for every plain `UIView`, the dimming view that
    /// swallows touches included.
    pub accessible: Option<bool>,
    /// `userInteractionEnabled`: the only flag that lets touches through.
    pub interactive: Option<bool>,
    /// XCTest's `hittable`. Reported only: it comes from an accessibility
    /// hit test, so a non-accessibility overlay reads "not hittable" while
    /// it swallows the touch.
    pub hittable: Option<bool>,
}

impl HitNode {
    /// Touches pass through it: the runner read `userInteractionEnabled ==
    /// NO`. UIKit hit testing skips a view for that (or hidden / alpha),
    /// never for not being an accessibility element or not "hittable" to
    /// XCTest, so those flags don't count (review-fi56 MAJOR-1).
    pub fn pass_through(&self) -> bool {
        self.interactive == Some(false)
    }

    /// How it is named in a failure: `Other "Dimming"`.
    pub fn describe(&self) -> String {
        let name = if self.label.is_empty() {
            &self.identifier
        } else {
            &self.label
        };
        if name.is_empty() {
            self.element_type.clone()
        } else {
            format!("{} \"{name}\"", self.element_type)
        }
    }

    /// Whether this reported node is plausibly `e` of the host's tree (type
    /// and frame, and its identifier or label when it has one).
    fn looks_like(&self, e: &Element) -> bool {
        self.element_type == e.element_type
            && self.bounds == e.bounds
            && (self.identifier.is_empty() && self.label.is_empty()
                || (!self.identifier.is_empty() && self.identifier == e.platform_id)
                || (!self.label.is_empty() && e.label.as_deref() == Some(self.label.as_str())))
    }
}

/// Where a reported hit node stands relative to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// The target, or inside it.
    Inside,
    /// Holds the target (a container with nothing more specific there).
    Container,
    /// Neither: something else at the point.
    Elsewhere,
    /// Can't be placed in the host's tree.
    Unknown,
}

/// The host's nodes from the snapshot's root down to `node`, by its
/// snapshot path, when every one of them has the type and frame the runner
/// reported. The host's tree may wrap the app's snapshot (a source root
/// around it), so the snapshot root is looked for in the top levels.
fn resolve<'a>(tree: &'a Element, node: &HitNode) -> Option<Vec<&'a Element>> {
    let path = node.path.as_ref()?;
    let matches =
        |h: &HitNode, e: &Element| h.element_type == e.element_type && h.bounds == e.bounds;
    let walk = |root: &'a Element| -> Option<Vec<&'a Element>> {
        let mut chain = vec![root];
        let mut cur = root;
        for &i in path {
            cur = cur.children.get(i)?;
            chain.push(cur);
        }
        let ok = if node.chain.is_empty() {
            matches(node, cur)
        } else {
            node.chain.len() == chain.len()
                && node.chain.iter().zip(&chain).all(|(h, e)| matches(h, e))
        };
        ok.then_some(chain)
    };
    let mut roots = vec![tree];
    roots.extend(tree.children.iter());
    roots.extend(tree.children.iter().flat_map(|c| c.children.iter()));
    roots.into_iter().find_map(walk)
}

fn relation(tree: &Element, target: &Element, node: &HitNode) -> Relation {
    if let Some(chain) = resolve(tree, node) {
        let last = chain[chain.len() - 1];
        return if chain.iter().any(|e| same(e, target)) {
            Relation::Inside
        } else if contains_node(last, target) {
            Relation::Container
        } else {
            Relation::Elsewhere
        };
    }
    // Not placed by path (a truncated tree): only a chain that names the
    // target itself says anything.
    let named = node
        .chain
        .iter()
        .chain(std::iter::once(node))
        .any(|h| h.looks_like(target));
    if named {
        Relation::Inside
    } else {
        Relation::Unknown
    }
}

/// D-38 (`ios.verifyTap`), from what the runner reports at the tap point:
/// `ax`, the element XCTest's own hit test returned (found in the app's
/// snapshot), and `deepest`, the deepest node of the snapshot holding the
/// point (nodes with user interaction off skipped). `Err` names what the tap would land
/// on instead of the target.
///
/// XCTest's element decides: the target or a node inside it reaches the
/// target; anything else is drawn over it. When it is a container of the
/// target (or can't be placed), the deepest opaque node decides the same
/// way, and a container or pass-through node there lets the tap through.
/// A runner that sends no path (an older one) gets the frame rule
/// ([`hit_lands_on`]).
pub fn hit_reaches<'a>(
    tree: &Element,
    target: &Element,
    ax: Option<&'a HitNode>,
    deepest: Option<&'a HitNode>,
) -> Result<(), &'a HitNode> {
    // XCTest's hit test follows UIKit's, which skips views that don't take
    // touches; one reported as such anyway is left to the deepest node.
    if let Some(ax) = ax.filter(|a| !a.pass_through()) {
        match relation(tree, target, ax) {
            Relation::Inside => return Ok(()),
            Relation::Elsewhere => return Err(ax),
            Relation::Container | Relation::Unknown => {}
        }
    }
    let Some(d) = deepest.filter(|d| !d.pass_through()) else {
        return Ok(());
    };
    match relation(tree, target, d) {
        Relation::Inside | Relation::Container => Ok(()),
        Relation::Elsewhere if empty_window(tree, target, d) => Ok(()),
        Relation::Elsewhere => Err(d),
        Relation::Unknown if hit_lands_on(tree, target, &d.bounds, &d.element_type) => Ok(()),
        Relation::Unknown => Err(d),
    }
}

/// Whether `d` is the bare root of another window than the target's: the
/// window and every node down to `d` fill the window and carry no label or
/// identifier. SwiftUI apps on iOS 26 keep such a window after their
/// content window; it lets touches through, but as the last window it is
/// the deepest node at every point, so a tap XCTest's own element couldn't
/// be placed for (an alert's button) was refused (DV-4, iPhone 17 Pro
/// simulator). An overlay in the target's own window still counts.
fn empty_window(tree: &Element, target: &Element, d: &HitNode) -> bool {
    let Some(chain) = resolve(tree, d) else {
        return false;
    };
    let Some(window) = chain.get(1).filter(|w| w.element_type == "Window") else {
        return false;
    };
    let bare = |e: &&Element| {
        e.bounds == window.bounds
            && e.platform_id.is_empty()
            && e.label.as_deref().unwrap_or("").is_empty()
    };
    !contains_node(window, target) && chain[1..].iter().all(bare)
}

/// Whether `target` is inside a web view of `tree`.
pub fn in_web_view(tree: &Element, target: &Element) -> bool {
    if tree.element_type == "WebView" && contains_node(tree, target) {
        return true;
    }
    tree.children.iter().any(|c| in_web_view(c, target))
}

/// The tree's own copy of `target` (lookups return clones with children).
fn find_in_tree<'a>(root: &'a Element, target: &Element) -> Option<&'a Element> {
    if same(root, target) {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_in_tree(c, target))
}

fn find_by_frame<'a>(node: &'a Element, frame: &Rect, ty: &str, out: &mut Vec<&'a Element>) {
    if node.bounds == *frame && node.element_type == ty {
        out.push(node);
    }
    for c in &node.children {
        find_by_frame(c, frame, ty, out);
    }
}

/// Whether `outer` holds all of `inner`.
fn covers_all(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// Occluders anywhere in `node`'s subtree that are neither the target's
/// ancestors nor inside it.
/// The bars that cover `target`. A bar the runner reports as not visible
/// covers nothing: SwiftUI keeps a hidden tab bar in the tree with its
/// frame, and a chat screen's message field at the bottom was refused as
/// "covered by the tab bar" (iOSWorld rerun, 2026-11-08).
fn collect_occluders(node: &Element, target: &Element, out: &mut Vec<(Rect, &'static str)>) {
    if same(node, target) {
        return; // the target and everything inside it
    }
    if let Some(kind) = occluder_kind(&node.element_type) {
        if !node.bounds.is_empty() && !contains_node(node, target) {
            if node.visible {
                out.push((node.bounds, kind));
            }
            return; // its own children are under it anyway
        }
    }
    for c in &node.children {
        collect_occluders(c, target, out);
    }
}

/// Whether `target` sits in a container listed after the bar at `bar` that
/// covers the target: a sheet presented over a screen with a tab bar. The
/// tab bar stays in the tree, visible, under the sheet, and a chat sheet's
/// message field over it was refused as "covered by the tab bar" (iOSWorld
/// rerun, 2026-11-08). UIKit lists a screen's content before its own tab
/// bar, so content scrolled under a tab bar is still covered. Navigation
/// bars are listed before the content they float over, so this can't apply
/// to them.
fn presented_over(tree: &Element, target: &Element, bar: &Rect, bar_type: &str) -> bool {
    // Depth-first orders of the bar and of the target's ancestors.
    #[allow(clippy::too_many_arguments)]
    fn walk<'a>(
        n: &'a Element,
        target: &Element,
        bar: &Rect,
        bar_type: &str,
        i: &mut usize,
        path: &mut Vec<(usize, &'a Element)>,
        bar_at: &mut Option<usize>,
        bar_parent: &mut Option<usize>,
        found: &mut Option<Vec<(usize, &'a Element)>>,
    ) {
        let me = *i;
        *i += 1;
        if bar_at.is_none() && n.element_type == bar_type && n.bounds == *bar {
            *bar_at = Some(me);
            *bar_parent = path.last().map(|(o, _)| *o);
        }
        if found.is_none() && same(n, target) {
            *found = Some(path.clone());
        }
        path.push((me, n));
        for c in &n.children {
            walk(c, target, bar, bar_type, i, path, bar_at, bar_parent, found);
        }
        path.pop();
    }
    let (mut i, mut path, mut bar_at, mut bar_parent, mut found) =
        (0, Vec::new(), None, None, None);
    walk(
        tree,
        target,
        bar,
        bar_type,
        &mut i,
        &mut path,
        &mut bar_at,
        &mut bar_parent,
        &mut found,
    );
    let (Some(bar_at), Some(ancestors)) = (bar_at, found) else {
        return false;
    };
    // A navigation bar is listed before its own screen's content, which it
    // floats over: that content shares the bar's parent. Only a target in
    // another branch (a sheet presented over the screen) is drawn over it.
    if bar_type == "NavigationBar"
        && bar_parent.is_some_and(|p| ancestors.iter().any(|(o, _)| *o == p))
    {
        return false;
    }
    ancestors
        .iter()
        .any(|(order, a)| *order > bar_at && covers_all(&a.bounds, &target.bounds))
}

/// The same node of the tree: lookups return clones, so compare fields.
fn same(a: &Element, b: &Element) -> bool {
    a.bounds == b.bounds
        && a.element_type == b.element_type
        && a.platform_id == b.platform_id
        && a.label == b.label
}

fn contains_node(root: &Element, target: &Element) -> bool {
    same(root, target) || root.children.iter().any(|c| contains_node(c, target))
}

fn contains(r: &Rect, x: i32, y: i32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then(|| Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// An iPhone on its side (its short side under 600 pt): no status bar.
pub fn landscape_phone(width: i32, height: i32) -> bool {
    width > height && height < 600
}

/// The status bar's height when the runner can't say, from the screen size
/// in points (portrait iPhones; landscape iPhones hide it).
pub fn estimated_status_bar(width: i32, height: i32) -> i32 {
    if width > height {
        return 0;
    }
    match height {
        // Home-button iPhones.
        568 | 667 | 736 => 20,
        // Notch iPhones (X … 14).
        780 | 812 | 844 | 896 | 926 => 47,
        // Dynamic Island (14 Pro … 16).
        852 | 932 => 54,
        // 16 Pro / Pro Max and later.
        874 | 956 => 62,
        // iPads.
        h if width >= 744 && h >= 1000 => 24,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(ty: &str, label: &str, x: i32, y: i32, w: i32, h: i32) -> Element {
        Element {
            hint: None,
            platform_id: label.into(),
            label: Some(label.into()),
            text: None,
            element_type: ty.into(),
            bounds: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn screen(children: Vec<Element>) -> Element {
        let mut root = el("Application", "app", 0, 0, 402, 874);
        root.children = children;
        root
    }

    /// Finding B (2026-11-11): the iOS 26 Settings "Apple Account" page
    /// sheet is only a second NavigationBar later in the tree.
    #[test]
    fn a_second_navigation_bar_counts_as_a_possible_modal_layer() {
        let list = el("Table", "list", 0, 116, 402, 758);
        let one = screen(vec![
            el("NavigationBar", "Settings", 0, 62, 402, 54),
            list.clone(),
        ]);
        assert!(!has_modal_layer(&one));
        let mut sheet = el("Other", "AUTH_SIGN_IN_CHOICE_VIEW", 0, 70, 402, 804);
        sheet.children = vec![el("NavigationBar", "Apple Account", 0, 70, 402, 56)];
        let two = screen(vec![
            el("NavigationBar", "Settings", 0, 62, 402, 54),
            list.clone(),
            sheet,
        ]);
        assert!(has_modal_layer(&two));
        // A bar that is off screen or reported hidden does not count.
        let mut gone = el("NavigationBar", "Old", 0, 900, 402, 54);
        gone.visible = true;
        let off = screen(vec![
            el("NavigationBar", "Settings", 0, 62, 402, 54),
            list,
            gone,
        ]);
        assert!(!has_modal_layer(&off));
    }

    fn keyboard() -> Element {
        let mut k = el("Keyboard", "", 0, 538, 402, 336);
        k.children = vec![el("Key", "g", 181, 566, 40, 42)];
        k
    }

    #[test]
    fn a_button_under_the_keyboard_is_covered() {
        let submit = el("Button", "Submit", 20, 560, 362, 50);
        let tree = screen(vec![submit.clone(), keyboard()]);
        assert_eq!(
            tap_point(&tree, &submit, 62),
            Err(Blocker::Covered("keyboard"))
        );
        // A key of the keyboard itself is its own target.
        let key = el("Key", "g", 181, 566, 40, 42);
        assert_eq!(tap_point(&tree, &key, 62), Ok((201, 587)));
    }

    #[test]
    fn a_half_covered_button_is_tapped_on_its_free_part() {
        let b = el("Button", "Pay", 0, 500, 402, 80); // 500..580, keyboard from 538
        let tree = screen(vec![b.clone(), keyboard()]);
        let (_, y) = tap_point(&tree, &b, 62).unwrap();
        assert!((500..538).contains(&y), "{y}");
    }

    #[test]
    fn a_row_cut_by_the_screen_edge_is_tapped_on_screen() {
        let row = el("Cell", "Bottom", 0, 850, 402, 100);
        let tree = screen(vec![row.clone()]);
        let (_, y) = tap_point(&tree, &row, 62).unwrap();
        assert!((850..874).contains(&y), "{y}");
        let gone = el("Cell", "Gone", 0, 900, 402, 100);
        assert_eq!(tap_point(&tree, &gone, 62), Err(Blocker::OffScreen));
    }

    #[test]
    fn a_row_under_the_status_bar_is_tapped_below_it_else_refused() {
        // VEL-341: a row whose on-screen part is all under the status bar
        // can't be reached (a tap there scrolls to the top), so it fails
        // loudly. A row reaching below the bar is tapped below it (M2).
        let top = el("StaticText", "Top", 0, -30, 402, 80); // 0..50 on screen
        let tree = screen(vec![top.clone()]);
        assert_eq!(
            tap_point(&tree, &top, 62),
            Err(Blocker::Covered("status bar"))
        );
        let taller = el("Cell", "Taller", 0, 20, 402, 80); // 20..100
        let (_, y) = tap_point(&tree, &taller, 62).unwrap();
        assert!(y >= 62, "{y}");
        // Unknown status bar: as before, the centre.
        assert_eq!(tap_point(&tree, &taller, 0), Ok((201, 60)));
    }

    #[test]
    fn a_navigation_bar_covers_content_but_not_its_own_buttons() {
        let mut nav = el("NavigationBar", "Settings", 0, 62, 402, 54);
        let back = el("Button", "Back", 8, 66, 60, 44);
        nav.children = vec![back.clone()];
        let cell = el("Cell", "Under", 0, 70, 402, 30); // entirely under the bar
        let tree = screen(vec![nav, cell.clone()]);
        assert_eq!(tap_point(&tree, &back, 62), Ok((38, 88)));
        assert_eq!(
            tap_point(&tree, &cell, 62),
            Err(Blocker::Covered("navigation bar"))
        );
    }

    #[test]
    fn a_sheet_over_a_tab_bar_is_not_covered_but_content_under_it_is() {
        let tabs = el("TabBar", "Tab Bar", 0, 791, 402, 83);
        let field = el("TextField", "Message", 63, 802, 271, 19);
        // A screen's own content, listed before its tab bar: covered.
        let mut content = el("Other", "", 0, 0, 402, 874);
        content.children = vec![field.clone()];
        let tree = screen(vec![content, tabs.clone()]);
        assert_eq!(
            tap_point(&tree, &field, 62),
            Err(Blocker::Covered("tab bar"))
        );
        // A sheet presented after it, holding the field: drawn over the bar.
        let mut sheet = el("Other", "", 0, 70, 402, 804);
        sheet.children = vec![field.clone()];
        let tree = screen(vec![el("Other", "", 0, 0, 402, 874), tabs, sheet]);
        assert!(tap_point(&tree, &field, 62).is_ok());
    }

    #[test]
    fn a_sheet_over_a_navigation_bar_is_not_covered_but_the_screen_under_it_is() {
        let nav = el("NavigationBar", "Home", 0, 62, 402, 54);
        let close = el("Button", "Close", 25, 80, 26, 36);
        // The screen's own content (sharing the bar's parent): covered.
        let mut screen_view = el("Other", "", 0, 0, 402, 874);
        screen_view.children = vec![nav.clone(), close.clone()];
        let tree = screen(vec![screen_view]);
        assert_eq!(
            tap_point(&tree, &close, 62),
            Err(Blocker::Covered("navigation bar"))
        );
        // A sheet presented over that screen holds the button: drawn over it.
        let mut presenting = el("Other", "", 0, 0, 402, 874);
        presenting.children = vec![nav];
        let mut sheet = el("Other", "", 0, 70, 402, 804);
        sheet.children = vec![close.clone()];
        let tree = screen(vec![presenting, sheet]);
        assert!(tap_point(&tree, &close, 62).is_ok());
    }

    #[test]
    fn a_row_switch_is_tapped_on_its_knob_and_a_bare_one_in_its_centre() {
        let row = el("Switch", "Outdoor Seating", 16, 612, 370, 52);
        let tree = screen(vec![row.clone()]);
        let (x, y) = tap_point(&tree, &row, 62).unwrap();
        assert!(
            x > 16 + 370 - 45 && x < 16 + 370,
            "near the trailing end: {x}"
        );
        assert_eq!(y, 612 + 26);
        let bare = el("Switch", "Wi-Fi", 300, 200, 51, 31);
        let tree = screen(vec![bare.clone()]);
        assert_eq!(tap_point(&tree, &bare, 62), Ok((325, 215)));
    }

    #[test]
    fn visibility_is_copied_only_onto_a_tree_of_the_same_shape() {
        let mut sheet = el("Sheet", "Options", 0, 400, 402, 474);
        sheet.children = vec![el("Button", "Cancel", 20, 800, 362, 44)];
        let cheap = screen(vec![el("Button", "Delete", 20, 700, 362, 44), sheet]);
        assert!(has_modal_layer(&cheap));
        assert!(!has_modal_layer(&screen(vec![keyboard()])));
        let mut full = cheap.clone();
        full.children[0].visible = false;
        let mut merged = cheap.clone();
        assert!(copy_visibility(&mut merged, &full));
        assert!(!merged.children[0].visible && merged.children[1].children[0].visible);
        // The system alert Velocity appends is not in the full read.
        let mut with_alert = cheap.clone();
        with_alert
            .children
            .push(el("XCUIElementTypeAlert", "alert", 0, 0, 402, 874));
        assert!(copy_visibility(&mut with_alert, &full));
        // The screen moved between the reads: not applied.
        let mut moved = cheap.clone();
        moved.children[0].bounds.y = 650;
        assert!(!copy_visibility(&mut moved, &full));
    }

    #[test]
    fn a_row_switch_has_a_leading_end_unless_it_is_covered() {
        let row = el("Switch", "Outdoor Seating", 0, 300, 402, 44);
        let tree = screen(vec![row.clone()]);
        assert_eq!(row_switch_leading_point(&tree, &row), Some((22, 322)));
        let plain = el("Switch", "wifi", 300, 300, 51, 31);
        assert_eq!(
            row_switch_leading_point(&screen(vec![plain.clone()]), &plain),
            None
        );
        let low = el("Switch", "Low", 0, 600, 402, 44);
        assert_eq!(
            row_switch_leading_point(&screen(vec![low.clone(), keyboard()]), &low),
            None
        );
        assert!(find_again(&tree, &row).is_some());
    }

    /// How a hidden tab bar really shows up. With the default (cheap)
    /// read every node is `visible`, so the case that happens is UIKit's
    /// `hidesBottomBarWhenPushed`: the bar stays in the tree, moved below
    /// the screen. `visible="false"` with an on-screen frame comes only
    /// from a read that asks for `visible` (`VELOCITY_IOS_FULL_SOURCE=1`,
    /// or the read under a sheet), for a bar whose view is hidden in place.
    #[test]
    fn a_hidden_tab_bar_covers_nothing() {
        let tabs = el("TabBar", "Tab Bar", 0, 791, 402, 83);
        let field = el("TextField", "Message", 63, 802, 271, 19);
        let tree_shown = screen(vec![field.clone(), tabs.clone()]);
        assert_eq!(
            tap_point(&tree_shown, &field, 62),
            Err(Blocker::Covered("tab bar"))
        );
        // Pushed with hidesBottomBarWhenPushed: moved off the bottom edge.
        let pushed_away = el("TabBar", "Tab Bar", 0, 874, 402, 83);
        let tree_pushed = screen(vec![field.clone(), pushed_away]);
        assert!(tap_point(&tree_pushed, &field, 62).is_ok());
        // Hidden in place, read with `visible`.
        let mut hidden = tabs;
        hidden.visible = false;
        let tree_hidden = screen(vec![field.clone(), hidden]);
        assert!(tap_point(&tree_hidden, &field, 62).is_ok());
    }

    #[test]
    fn the_status_bar_never_turns_a_hard_cover_into_a_pass() {
        let mut nav = el("NavigationBar", "Bar", 0, 0, 402, 100);
        nav.children = vec![el("Button", "Mine", 8, 50, 60, 44)];
        let under = el("Cell", "Under", 0, 10, 402, 40);
        let tree = screen(vec![nav, under.clone()]);
        assert_eq!(
            tap_point(&tree, &under, 62),
            Err(Blocker::Covered("navigation bar"))
        );
    }

    #[test]
    fn a_wda_found_element_is_not_covered_by_its_own_bar() {
        // Review M3: an element WDA found below the snapshot depth carries
        // WDA's id, so it matches no node of the tree; its own tab bar and
        // the keyboard around a key used to cover it.
        let mut tabs = el("TabBar", "Tabs", 0, 790, 402, 84);
        tabs.children = vec![el("Other", "", 0, 790, 402, 84)];
        let mut item = el("Button", "Settings", 300, 800, 80, 50);
        item.platform_id = "8D000000-0000-0000-WDA".into();
        let tree = screen(vec![tabs, keyboard()]);
        assert_eq!(tap_point(&tree, &item, 62), Ok((340, 825)));
        let mut key = el("Key", "q", 0, 566, 40, 42);
        key.platform_id = "9E000000-0000-0000-WDA".into();
        assert_eq!(tap_point(&tree, &key, 62), Ok((20, 587)));
        // Content WDA found under the keyboard is still covered by it: the
        // keyboard doesn't hold all of it.
        let mut field = el("TextField", "Note", 0, 500, 402, 80);
        field.platform_id = "AF000000-0000-0000-WDA".into();
        let (_, y) = tap_point(&tree, &field, 62).unwrap();
        assert!((500..538).contains(&y), "{y}");
    }

    /// Review 09 #2: the presenting screen stays in the tree under an
    /// action sheet; a tap on it lands on the dimming view.
    #[test]
    fn content_behind_an_action_sheet_is_covered_its_buttons_are_not() {
        let save_behind = el("Button", "Save", 300, 100, 80, 44);
        let mut sheet = el("Sheet", "", 8, 600, 386, 266);
        let save_in_sheet = el("Button", "Save", 8, 650, 386, 56);
        let cancel = el("Button", "Cancel", 8, 800, 386, 56);
        sheet.children = vec![save_in_sheet.clone(), cancel.clone()];
        let mut content = el("Other", "", 0, 0, 402, 874);
        content.children = vec![save_behind.clone()];
        let tree = screen(vec![content, sheet]);
        assert_eq!(
            tap_point(&tree, &save_behind, 62),
            Err(Blocker::Covered("sheet")),
            "far from the sheet's own frame, still behind its dimming view"
        );
        assert_eq!(behind_modal(&tree, &save_behind), Some("sheet"));
        assert_eq!(tap_point(&tree, &save_in_sheet, 62), Ok((201, 678)));
        assert!(tap_point(&tree, &cancel, 62).is_ok());
        assert_eq!(behind_modal(&tree, &cancel), None);
    }

    #[test]
    fn a_popover_and_its_dismiss_region_cover_what_was_presented_before() {
        let row = el("Cell", "Row", 0, 300, 402, 44);
        let mut region = el("Other", "dismiss popup", 0, 0, 402, 874);
        region.platform_id = DISMISS_REGION.into();
        let mut card = el("Other", "", 0, 60, 402, 814);
        let field = el("TextField", "Title", 16, 120, 370, 44);
        card.children = vec![field.clone()];
        let mut content = el("Other", "", 0, 0, 402, 874);
        content.children = vec![row.clone()];
        // A page sheet: the dimming view XCTest reports, then the card.
        let tree = screen(vec![content.clone(), region.clone(), card.clone()]);
        assert_eq!(
            tap_point(&tree, &row, 62),
            Err(Blocker::Covered("sheet's dimming view"))
        );
        assert!(tap_point(&tree, &field, 62).is_ok());
        // A popover, with the keyboard up for its field: keys stay tappable.
        let mut popover = el("Popover", "", 100, 100, 300, 400);
        let pop_field = el("TextField", "Name", 110, 120, 280, 44);
        popover.children = vec![pop_field.clone()];
        let tree = screen(vec![content, popover, keyboard()]);
        assert_eq!(tap_point(&tree, &row, 62), Err(Blocker::Covered("popover")));
        assert!(tap_point(&tree, &pop_field, 62).is_ok());
        let key = el("Key", "g", 181, 566, 40, 42);
        assert!(tap_point(&tree, &key, 62).is_ok());
    }

    #[test]
    fn an_in_app_alert_covers_the_screen_but_not_its_buttons() {
        let behind = el("Button", "Buy", 20, 760, 362, 50);
        let mut alert = el("Alert", "Discard draft?", 60, 360, 282, 150);
        let ok = el("Button", "Discard", 60, 460, 141, 50);
        alert.children = vec![ok.clone()];
        let tree = screen(vec![behind.clone(), alert]);
        assert_eq!(
            tap_point(&tree, &behind, 62),
            Err(Blocker::Covered("alert"))
        );
        assert_eq!(tap_point(&tree, &ok, 62), Ok((130, 485)));
        // Velocity's own system-alert node is the dialog handler's.
        let mut system = el("XCUIElementTypeAlert", "Allow?", 0, 0, 402, 874);
        system.children = vec![];
        let tree = screen(vec![behind.clone(), system]);
        assert!(tap_point(&tree, &behind, 62).is_ok());
    }

    #[test]
    fn a_sheet_reported_hidden_or_a_target_found_beyond_the_tree() {
        let behind = el("Button", "Save", 300, 100, 80, 44);
        let mut sheet = el("Sheet", "", 8, 600, 386, 266);
        sheet.visible = false;
        let tree = screen(vec![behind.clone(), sheet.clone()]);
        assert!(tap_point(&tree, &behind, 62).is_ok(), "a gone sheet");
        // WDA found the target below the snapshot depth: inside the sheet's
        // frame it is the sheet's, elsewhere it is behind it.
        sheet.visible = true;
        let tree = screen(vec![sheet]);
        let mut inside = el("Button", "Delete", 16, 700, 370, 56);
        inside.platform_id = "8D000000-0000-0000-WDA".into();
        assert!(tap_point(&tree, &inside, 62).is_ok());
        let mut outside = behind;
        outside.platform_id = "9E000000-0000-0000-WDA".into();
        assert_eq!(
            tap_point(&tree, &outside, 62),
            Err(Blocker::Covered("sheet"))
        );
    }

    #[test]
    fn landscape_phones_have_no_status_bar() {
        assert!(landscape_phone(874, 402));
        assert!(!landscape_phone(402, 874));
        assert!(!landscape_phone(1366, 1024), "iPads keep theirs");
    }

    #[test]
    fn an_uncovered_element_keeps_its_centre() {
        let b = el("Button", "OK", 20, 300, 100, 40);
        let tree = screen(vec![b.clone(), keyboard()]);
        assert_eq!(tap_point(&tree, &b, 62), Ok((70, 320)));
    }

    #[test]
    fn status_bar_estimates() {
        assert_eq!(estimated_status_bar(402, 874), 62);
        assert_eq!(estimated_status_bar(390, 844), 47);
        assert_eq!(estimated_status_bar(375, 667), 20);
        assert_eq!(estimated_status_bar(874, 402), 0);
        assert_eq!(estimated_status_bar(500, 500), 0);
    }

    /// D-38: what the runner's hit test found at the tap point reaches the
    /// target only when it is the target, inside it or around it.
    #[test]
    fn a_hit_lands_on_the_target_its_children_or_its_container() {
        let mut button = el("Button", "Pay", 20, 400, 200, 50);
        button.children = vec![el("StaticText", "Pay", 30, 410, 60, 30)];
        let mut cell = el("Cell", "row", 0, 390, 402, 70);
        cell.children = vec![button.clone()];
        let overlay = el("Other", "dimming", 0, 0, 402, 874);
        let badge = el("Image", "badge", 180, 400, 20, 20);
        let tree = screen(vec![cell.clone(), badge.clone(), overlay.clone()]);
        let lands = |hit: &Element| hit_lands_on(&tree, &button, &hit.bounds, &hit.element_type);
        assert!(lands(&button), "the target itself");
        assert!(lands(&button.children[0]), "its label");
        assert!(lands(&cell), "its container");
        assert!(!lands(&overlay), "a full-screen view drawn over it");
        assert!(!lands(&badge), "a sibling drawn on top, inside its frame");
        // A node the tree doesn't hold: inside the target's frame or not.
        assert!(hit_lands_on(
            &tree,
            &button,
            &Rect {
                x: 25,
                y: 405,
                width: 10,
                height: 10
            },
            "Other"
        ));
        assert!(!hit_lands_on(
            &tree,
            &button,
            &Rect {
                x: 0,
                y: 0,
                width: 402,
                height: 62
            },
            "StatusBar"
        ));
    }
}
