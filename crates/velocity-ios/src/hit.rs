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
        "Alert" => "alert",
        _ => return None,
    })
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
    if !in_tree {
        covers.retain(|(r, _)| !covers_all(r, &target.bounds));
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
    let mut grid = vec![centre];
    for fy in [50, 25, 75, 10, 90] {
        for fx in [50, 25, 75, 10, 90] {
            grid.push((
                visible.x + visible.width * fx / 100,
                visible.y + visible.height * fy / 100,
            ));
        }
    }
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
fn collect_occluders(node: &Element, target: &Element, out: &mut Vec<(Rect, &'static str)>) {
    if same(node, target) {
        return; // the target and everything inside it
    }
    if let Some(kind) = occluder_kind(&node.element_type) {
        if !node.bounds.is_empty() && !contains_node(node, target) {
            out.push((node.bounds, kind));
            return; // its own children are under it anyway
        }
    }
    for c in &node.children {
        collect_occluders(c, target, out);
    }
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
