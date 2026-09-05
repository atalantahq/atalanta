//! Element refs (`e12`) handed out by `list_elements`, so an agent can act
//! on what it saw without inventing a selector (Playwright MCP's model). Each
//! ref stands for a selector built here, from the tree, and an exported flow
//! records that selector, never the ref. Refs from an older listing are
//! unknown, and a ref whose selector now finds a different element is stale:
//! both ask the agent to list the elements again.
//!
//! Like Playwright MCP's, refs belong to one snapshot: any tool that changes
//! the screen (tap, type_text, swipe, press_key, run_flow, run_test) retires
//! them all. And "the same element" includes where it is and its id: twins
//! ("Add to cart" on every row) differ only there, so a scroll that slides
//! another twin under an indexed selector makes the ref stale (review
//! MAJOR-2).

use std::collections::HashMap;

use serde_json::Value;
use velocity_common::selector_build::robust_selector;
use velocity_common::selector_match::find_all_ref;
use velocity_common::{Element, Platform, Rect, Result, Selector, VelocityError};

/// What a ref's element looked like when it was listed.
#[derive(Debug, Clone, PartialEq)]
struct Fingerprint {
    element_type: String,
    platform_id: String,
    label: Option<String>,
    text: Option<String>,
    bounds: Rect,
}

impl Fingerprint {
    /// On the web, `platform_id` is a per-page-load node id, so it is left
    /// out: a reload that keeps the page the same keeps its refs (NB-2).
    fn of(e: &Element, platform: Platform) -> Self {
        Self {
            element_type: e.element_type.clone(),
            platform_id: match platform {
                Platform::Web => String::new(),
                _ => e.platform_id.clone(),
            },
            label: e.label.clone(),
            text: e.text.clone(),
            bounds: e.bounds,
        }
    }
}

/// Tools that change the screen, and so retire every ref.
pub const SCREEN_CHANGING_TOOLS: &[&str] = &[
    "tap",
    "type_text",
    "swipe",
    "press_key",
    "scroll",
    "launch_app",
    "stop_app",
    "run_flow",
    "run_test",
];

#[derive(Debug)]
pub struct Refs {
    /// The device's platform: it decides which ids are stable and how labels
    /// are selected (the NL builder's rules).
    pub platform: Platform,
    next: u64,
    current: HashMap<String, (Selector, Fingerprint)>,
    /// The tool that last retired the refs, for the error message.
    expired_by: Option<String>,
}

impl Default for Refs {
    fn default() -> Self {
        Self {
            platform: Platform::Android,
            next: 0,
            current: HashMap::new(),
            expired_by: None,
        }
    }
}

impl Refs {
    /// Forget the previous listing's refs; numbering keeps counting, so an
    /// old ref is never mistaken for a new one.
    pub fn start_listing(&mut self) {
        self.current.clear();
        self.expired_by = None;
    }

    /// `tool` changed the screen: every ref is retired until the next
    /// listing.
    pub fn expire(&mut self, tool: &str) {
        if !self.current.is_empty() {
            self.current.clear();
            self.expired_by = Some(tool.to_string());
        }
    }

    /// A ref for `node` (inside `tree`), or `None` when it can't be acted
    /// on (hidden, empty or off screen, as Playwright's interactable refs)
    /// or no selector the NL builder makes finds it.
    pub fn mint(
        &mut self,
        tree: &Element,
        node: &Element,
        screen: Option<&Rect>,
    ) -> Option<String> {
        if !actionable(node, screen) {
            return None;
        }
        let selector = selector_for(tree, node, screen, self.platform)?;
        self.next += 1;
        let r = format!("e{}", self.next);
        self.current
            .insert(r.clone(), (selector, Fingerprint::of(node, self.platform)));
        Some(r)
    }

    /// Replace `{"ref": "eN"}` in `args.selector` with the selector it
    /// stands for, after checking it still finds the same element.
    pub fn resolve_args(
        &self,
        args: &mut Value,
        tree: &Element,
        screen: Option<&Rect>,
    ) -> Result<()> {
        let Some(r) = args
            .get("selector")
            .and_then(|s| s.get("ref"))
            .and_then(Value::as_str)
            .map(String::from)
        else {
            return Ok(());
        };
        let selector = self.resolve(&r, tree, screen)?;
        args["selector"] = serde_json::to_value(selector)
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        Ok(())
    }

    fn resolve(&self, r: &str, tree: &Element, screen: Option<&Rect>) -> Result<Selector> {
        let Some((selector, seen)) = self.current.get(r) else {
            if let Some(tool) = &self.expired_by {
                return Err(VelocityError::Config(format!(
                    "ref {r} is stale: `{tool}` changed the screen since list_elements; \
                     call list_elements again"
                )));
            }
            return Err(VelocityError::Config(format!(
                "ref {r} is not in the latest list_elements; call list_elements again"
            )));
        };
        let now = pick(tree, selector, screen);
        if now.map(|e| Fingerprint::of(e, self.platform)).as_ref() != Some(seen) {
            return Err(VelocityError::Config(format!(
                "ref {r} is stale: the screen changed since list_elements; call it again"
            )));
        }
        Ok(selector.clone())
    }
}

/// The rect drivers match selectors in, as the executor computes it.
pub async fn screen_rect(
    driver: &std::sync::Arc<dyn velocity_common::PlatformDriver>,
    device_id: &str,
) -> Option<Rect> {
    let (width, height) = driver.screen_size(device_id).await.ok()?;
    (width > 0 && height > 0).then_some(Rect {
        x: 0,
        y: 0,
        width,
        height,
    })
}

/// The element `selector` acts on.
pub(crate) fn pick<'a>(
    tree: &'a Element,
    selector: &Selector,
    screen: Option<&Rect>,
) -> Option<&'a Element> {
    match selector {
        Selector::Index { selector, index } => {
            find_all_ref(tree, selector, screen).get(*index).copied()
        }
        s => find_all_ref(tree, s, screen).first().copied(),
    }
}

/// Whether an agent can act on `node`: visible, with an area, and on screen.
fn actionable(node: &Element, screen: Option<&Rect>) -> bool {
    let b = &node.bounds;
    node.visible
        && b.width > 0
        && b.height > 0
        && screen.is_none_or(|s| {
            b.x < s.x + s.width
                && b.x + b.width > s.x
                && b.y < s.y + s.height
                && b.y + b.height > s.y
        })
}

/// The selector NL steps would record for `node` (one builder for both,
/// P-7), when it finds `node` here.
fn selector_for(
    tree: &Element,
    node: &Element,
    screen: Option<&Rect>,
    platform: Platform,
) -> Option<Selector> {
    let selector = robust_selector(node, tree, screen, platform);
    pick(tree, &selector, screen)
        .is_some_and(|hit| std::ptr::eq(hit, node))
        .then_some(selector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::Rect;

    fn el(id: &str, text: &str, y: i32) -> Element {
        Element {
            hint: None,
            platform_id: id.into(),
            label: None,
            text: (!text.is_empty()).then(|| text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn tree(kids: Vec<Element>) -> Element {
        Element {
            children: kids,
            ..el("", "", 0)
        }
    }

    #[test]
    fn refs_resolve_to_built_selectors_and_go_stale() {
        let t = tree(vec![
            el("ok", "OK", 0),
            el("", "Buy", 50),
            el("", "Buy", 100),
        ]);
        let mut refs = Refs::default();
        refs.start_listing();
        let ok = refs.mint(&t, &t.children[0], None).unwrap();
        let second_buy = refs.mint(&t, &t.children[2], None).unwrap();

        let mut args = serde_json::json!({ "selector": { "ref": ok } });
        refs.resolve_args(&mut args, &t, None).unwrap();
        assert_eq!(args["selector"], serde_json::json!({ "id": "ok" }));

        let mut args = serde_json::json!({ "selector": { "ref": second_buy } });
        refs.resolve_args(&mut args, &t, None).unwrap();
        let s: Selector = serde_json::from_value(args["selector"].clone()).unwrap();
        assert_eq!(
            s,
            Selector::Index {
                selector: Box::new(Selector::Text("Buy".into())),
                index: 1
            }
        );

        // The second "Buy" row became "Sold out": the index now hits nothing.
        let changed = tree(vec![
            el("ok", "OK", 0),
            el("", "Buy", 50),
            el("", "Sold out", 100),
        ]);
        let mut args = serde_json::json!({ "selector": { "ref": second_buy } });
        let e = refs.resolve_args(&mut args, &changed, None).unwrap_err();
        assert!(e.to_string().contains("stale"), "{e}");

        // A new listing forgets the old refs.
        refs.start_listing();
        let mut args = serde_json::json!({ "selector": { "ref": ok } });
        let e = refs.resolve_args(&mut args, &t, None).unwrap_err();
        assert!(e.to_string().contains("list_elements again"), "{e}");
    }

    // Review MAJOR-2: twins differ only in position. A scroll slid another
    // "Add to cart" under `Index{Text, 1}`; the type/label/text fingerprint
    // still matched, so the tap went to the wrong product.
    #[test]
    fn a_twin_slid_under_an_indexed_ref_is_stale() {
        let t = tree(vec![el("", "Add to cart", 100), el("", "Add to cart", 200)]);
        let mut refs = Refs::default();
        refs.start_listing();
        let second = refs.mint(&t, &t.children[1], None).unwrap();
        // Scrolled by 150: the old second row is now first, a new one below.
        let scrolled = tree(vec![el("", "Add to cart", 50), el("", "Add to cart", 150)]);
        let mut args = serde_json::json!({ "selector": { "ref": second } });
        let e = refs.resolve_args(&mut args, &scrolled, None).unwrap_err();
        assert!(e.to_string().contains("stale"), "{e}");
    }

    #[test]
    fn a_screen_changing_tool_retires_every_ref() {
        let t = tree(vec![el("ok", "OK", 0)]);
        let mut refs = Refs::default();
        refs.start_listing();
        let ok = refs.mint(&t, &t.children[0], None).unwrap();
        refs.expire("swipe");
        let mut args = serde_json::json!({ "selector": { "ref": ok } });
        let e = refs.resolve_args(&mut args, &t, None).unwrap_err();
        assert!(
            e.to_string().contains("stale") && e.to_string().contains("swipe"),
            "{e}"
        );
    }

    // Review m2: a synthetic headless id is positional; record the text.
    #[test]
    fn synthetic_ids_are_not_used_as_selectors() {
        let t = tree(vec![
            el("headless:id/node_3", "Pay", 0),
            el("", "Other", 50),
        ]);
        let mut refs = Refs::default();
        refs.start_listing();
        let pay = refs.mint(&t, &t.children[0], None).unwrap();
        let mut args = serde_json::json!({ "selector": { "ref": pay } });
        refs.resolve_args(&mut args, &t, None).unwrap();
        assert_eq!(args["selector"], serde_json::json!({ "text": "Pay" }));
        use velocity_common::selector_build::synthetic_id;
        assert!(synthetic_id("8F3A6C1E-0B2D-4C55-9E1F-1234567890AB"));
        assert!(!synthetic_id("headless:id/node_title"));
        assert!(!synthetic_id("com.app:id/pay"));
    }

    #[test]
    fn a_plain_selector_is_left_alone() {
        let refs = Refs::default();
        let mut args = serde_json::json!({ "selector": { "text": "OK" } });
        refs.resolve_args(&mut args, &tree(vec![]), None).unwrap();
        assert_eq!(args["selector"], serde_json::json!({ "text": "OK" }));
    }

    fn resolved(refs: &Refs, r: &str, t: &Element) -> Selector {
        let mut args = serde_json::json!({ "selector": { "ref": r } });
        refs.resolve_args(&mut args, t, None).unwrap();
        serde_json::from_value(args["selector"].clone()).unwrap()
    }

    // NB-2: a web node id is per page load; a ref must never record it.
    #[test]
    fn web_refs_never_use_vel_ids() {
        let t = tree(vec![el("3.k9x2ab", "Checkout", 0)]);
        let mut refs = Refs {
            platform: Platform::Web,
            ..Default::default()
        };
        refs.start_listing();
        let r = refs.mint(&t, &t.children[0], None).unwrap();
        assert_eq!(resolved(&refs, &r, &t), Selector::Text("Checkout".into()));
    }

    // NB-2: after a reload the same page has new node ids; the ref still holds.
    #[test]
    fn web_refs_survive_a_reload() {
        let t = tree(vec![el("3.k9x2ab", "Checkout", 0)]);
        let mut refs = Refs {
            platform: Platform::Web,
            ..Default::default()
        };
        refs.start_listing();
        let r = refs.mint(&t, &t.children[0], None).unwrap();
        let reloaded = tree(vec![el("7.zz01qq", "Checkout", 0)]);
        let mut args = serde_json::json!({ "selector": { "ref": r } });
        refs.resolve_args(&mut args, &reloaded, None).unwrap();
        assert_eq!(args["selector"], serde_json::json!({ "text": "Checkout" }));
    }

    // P-7: hidden, empty and off-screen nodes get no ref.
    #[test]
    fn hidden_nodes_get_no_ref() {
        let mut hidden = el("hidden", "Hidden", 0);
        hidden.visible = false;
        let mut empty = el("empty", "Empty", 50);
        empty.bounds.height = 0;
        let below = el("below", "Below", 5000);
        let shown = el("shown", "Shown", 100);
        let t = tree(vec![hidden, empty, below, shown]);
        let screen = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 800,
        };
        let mut refs = Refs::default();
        refs.start_listing();
        let minted: Vec<_> = t
            .children
            .iter()
            .map(|n| refs.mint(&t, n, Some(&screen)).is_some())
            .collect();
        assert_eq!(minted, vec![false, false, false, true]);
    }

    // P-7: one builder, so NL and MCP record the same selector; a label is an
    // accessibilityId on mobile, as NL records it.
    #[test]
    fn nl_and_mcp_build_the_same_selector() {
        let mut labelled = el("", "", 0);
        labelled.label = Some("Close".into());
        let t = tree(vec![
            el("ok", "OK", 50),
            labelled,
            el("", "Buy", 100),
            el("", "Buy", 150),
            el("headless:node#0.4", "Pay", 200),
        ]);
        for platform in [Platform::Android, Platform::Ios, Platform::Web] {
            let mut refs = Refs {
                platform,
                ..Default::default()
            };
            refs.start_listing();
            for node in &t.children {
                let r = refs.mint(&t, node, None).unwrap();
                assert_eq!(
                    resolved(&refs, &r, &t),
                    robust_selector(node, &t, None, platform),
                    "{platform:?} {node:?}"
                );
            }
        }
        let mut refs = Refs::default();
        refs.start_listing();
        let r = refs.mint(&t, &t.children[1], None).unwrap();
        assert_eq!(
            resolved(&refs, &r, &t),
            Selector::AccessibilityId("Close".into())
        );
        let r = refs.mint(&t, &t.children[4], None).unwrap();
        assert_eq!(resolved(&refs, &r, &t), Selector::Text("Pay".into()));
    }
}
