//! A fake app for tests: screens of named elements and the taps that move
//! between them, behind a real [`PlatformDriver`]. The executor runs
//! against it unchanged (selectors, actionability, settle), so an explore
//! session and its replay can be tested end to end without a device.
//!
//! Every screen shows a status-bar clock whose time changes on every read
//! (volatile text that must never be asserted), and a typed value shows in
//! its field (user data that must never be asserted either).
//!
//! Opt-in device behaviours (review 07 H2): a field's placeholder
//! ([`Node::hint`]), a bar drawn over a screen's bottom that swallows taps
//! under it until it is tapped away ([`FakeApp::overlay`]), and a soft
//! keyboard that opens on a text field, covers the bottom of the screen and
//! closes on Back before Back navigates ([`FakeApp::keyboard`]).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use velocity_common::selector_match::{find_all_ref, find_in_tree};
use velocity_common::{
    DeviceInfo, Direction, Element, Key, PlatformDriver, Rect, Result, Selector, SuiteConfig,
    SyncMode, VelocityError,
};

const W: i32 = 1080;
const H: i32 = 1920;
const ROW: i32 = 160;

/// One element of a fake screen.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub ty: String,
    pub text: Option<String>,
    pub label: Option<String>,
    pub enabled: bool,
    /// A switch or checkbox's state at start; a tap flips it.
    pub checked: Option<bool>,
    /// A text field's placeholder, reported as `hint` (never as text).
    pub hint: Option<String>,
    /// The solid colour the element is drawn in on screenshots (white
    /// otherwise).
    pub color: Option<[u8; 3]>,
}

impl Node {
    pub fn text(ty: &str, text: &str) -> Self {
        Self {
            id: String::new(),
            ty: ty.into(),
            text: Some(text.into()),
            label: None,
            enabled: true,
            checked: None,
            hint: None,
            color: None,
        }
    }

    /// An empty text field showing `hint` as its placeholder.
    pub fn field(id: &str, hint: &str) -> Self {
        Self {
            id: id.into(),
            ty: "android.widget.EditText".into(),
            text: None,
            label: None,
            enabled: true,
            checked: None,
            hint: Some(hint.into()),
            color: None,
        }
    }

    /// Draw the element in one solid colour on screenshots.
    pub fn color(mut self, rgb: [u8; 3]) -> Self {
        self.color = Some(rgb);
        self
    }

    pub fn hint(mut self, h: &str) -> Self {
        self.hint = Some(h.into());
        self
    }

    fn editable(&self) -> bool {
        self.ty.contains("EditText") || self.ty.contains("TextField")
    }

    fn key_matches(&self, el: &Element) -> bool {
        (!self.id.is_empty() && self.id == el.platform_id)
            || (self.text.is_some() && self.text == el.text)
            || (self.label.is_some() && self.label == el.label)
    }

    pub fn id(mut self, id: &str) -> Self {
        self.id = id.into();
        self
    }

    pub fn label(mut self, l: &str) -> Self {
        self.label = Some(l.into());
        self
    }

    /// A switch or checkbox, starting in `on`.
    pub fn checkable(mut self, on: bool) -> Self {
        self.checked = Some(on);
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

#[derive(Default)]
struct State {
    stack: Vec<String>,
    /// Scroll offset per screen.
    offset: HashMap<String, i32>,
    /// What was typed into each field (by id), shown as its text.
    fields: HashMap<String, String>,
    /// Flipped switches: (screen, node index) -> state.
    checks: HashMap<(String, usize), bool>,
    taps: Vec<String>,
    typed: Vec<(String, String)>,
    launches: Vec<bool>,
    reads: usize,
    /// A tapped link whose screen shows only after this many more reads.
    pending: Option<(String, usize)>,
    /// The field (by id) that has input focus.
    focused: Option<String>,
    /// The soft keyboard is showing.
    keyboard_up: bool,
    /// Screens whose overlay was tapped away.
    dismissed: HashSet<String>,
}

/// A fake app. Build with [`screen`](Self::screen) and
/// [`link`](Self::link); start screen is the first one added.
pub struct FakeApp {
    screens: Mutex<HashMap<String, Vec<Node>>>,
    /// (launch count, screen, new nodes): the app "ships an update".
    changes: Mutex<Vec<(usize, String, Vec<Node>)>>,
    order: Vec<String>,
    links: HashMap<(String, String), String>,
    state: Mutex<State>,
    /// Reads a tapped link's screen lags behind (web content's late tree).
    lag_reads: usize,
    /// (from, key) -> the screen shown until the link's own one arrives,
    /// and after how many reads it does.
    interims: HashMap<(String, String), (String, usize)>,
    /// screen -> a bar of the given height over its bottom.
    overlays: HashMap<String, (Node, i32)>,
    /// The soft keyboard's height, when the app has one.
    keyboard_h: Option<i32>,
}

impl Default for FakeApp {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeApp {
    pub fn new() -> Self {
        Self {
            screens: Mutex::new(HashMap::new()),
            changes: Mutex::new(vec![]),
            order: vec![],
            links: HashMap::new(),
            state: Mutex::new(State::default()),
            lag_reads: 0,
            interims: HashMap::new(),
            overlays: HashMap::new(),
            keyboard_h: None,
        }
    }

    /// `screen` shows `bar` over its bottom `height` px (a cookie banner, a
    /// bottom sheet): a tap on what it covers fails as covered, as the
    /// device drivers' tap guard reports it; tapping the bar takes it away
    /// until the next launch.
    pub fn overlay(mut self, screen: &str, bar: Node, height: i32) -> Self {
        self.overlays.insert(screen.into(), (bar, height));
        self
    }

    /// A soft keyboard `height` px tall opens when a text field is tapped
    /// or typed into; it covers the bottom of the screen, and Back closes
    /// it before it navigates.
    pub fn keyboard(mut self, height: i32) -> Self {
        self.keyboard_h = Some(height);
        self
    }

    /// Whether the soft keyboard is showing.
    pub fn keyboard_up(&self) -> bool {
        self.state.lock().unwrap().keyboard_up
    }

    /// What covers the screen now, with what each is called: the overlay
    /// (unless dismissed) and the keyboard.
    fn occluders(&self, st: &State, name: &str) -> Vec<(Rect, String)> {
        let mut out = Vec::new();
        if let Some((bar, h)) = self.overlays.get(name) {
            if !st.dismissed.contains(name) {
                let what = bar
                    .text
                    .clone()
                    .or(bar.label.clone())
                    .unwrap_or(bar.id.clone());
                out.push((bottom(*h), what));
            }
        }
        if let (true, Some(h)) = (st.keyboard_up, self.keyboard_h) {
            out.push((bottom(h), "the keyboard".to_string()));
        }
        out
    }

    pub fn screen(mut self, name: &str, nodes: Vec<Node>) -> Self {
        self.screens.lock().unwrap().insert(name.into(), nodes);
        self.order.push(name.into());
        self
    }

    /// Tapping the element whose id, text or label is `key` on `from`
    /// opens `to`.
    pub fn link(mut self, from: &str, key: &str, to: &str) -> Self {
        self.links.insert((from.into(), key.into()), to.into());
        self
    }

    /// A tapped link's screen reaches the tree only `reads` reads later,
    /// as web content's accessibility tree does.
    pub fn late_by_reads(mut self, reads: usize) -> Self {
        self.lag_reads = reads;
        self
    }

    /// Tapping `key` on `from` shows `interim` at once, and the link's own
    /// screen `reads` reads later in its place (a message sent: the field
    /// empties, the bubble comes later).
    pub fn interim(mut self, from: &str, key: &str, interim: &str, reads: usize) -> Self {
        self.interims
            .insert((from.into(), key.into()), (interim.into(), reads));
        self
    }

    /// From the `launches`-th launch on, `screen` shows `nodes` instead.
    pub fn change_after_launches(self, launches: usize, screen: &str, nodes: Vec<Node>) -> Self {
        self.changes
            .lock()
            .unwrap()
            .push((launches, screen.into(), nodes));
        self
    }

    /// The screen on top.
    pub fn current(&self) -> String {
        let st = self.state.lock().unwrap();
        st.stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.order[0].clone())
    }

    pub fn taps(&self) -> Vec<String> {
        self.state.lock().unwrap().taps.clone()
    }

    pub fn typed(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().typed.clone()
    }

    pub fn launches(&self) -> Vec<bool> {
        self.state.lock().unwrap().launches.clone()
    }

    /// Settings for a fast executor against the fake: no settle polling,
    /// no dialog handling, short waits.
    pub fn config() -> SuiteConfig {
        let mut c = SuiteConfig {
            platform: Some(velocity_common::Platform::Android),
            ..Default::default()
        };
        c.sync.mode = SyncMode::None;
        c.sync.timeout_ms = 300;
        c.dialog.enabled = false;
        c.healing.enabled = false;
        c.artifacts.on_failure = false;
        c
    }

    fn tree(&self) -> Element {
        let mut st = self.state.lock().unwrap();
        st.reads += 1;
        if let Some((to, left)) = st.pending.take() {
            if left == 0 {
                if st.stack.last().is_some_and(|s| s.ends_with("#interim")) {
                    st.stack.pop();
                }
                st.stack.push(to);
            } else {
                st.pending = Some((to, left - 1));
            }
        }
        let name = st
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.order[0].clone())
            .trim_end_matches("#interim")
            .to_string();
        let offset = st.offset.get(&name).copied().unwrap_or(0);
        let mut children = vec![Element {
            platform_id: "clock".into(),
            element_type: "android.widget.TextView".into(),
            text: Some(format!("12:{:02}", st.reads % 60)),
            bounds: Rect {
                x: 900,
                y: 10,
                width: 150,
                height: 50,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        }];
        let screens = self.screens.lock().unwrap();
        for (i, n) in screens[&name].iter().enumerate() {
            let text = st.fields.get(&n.id).cloned().or_else(|| n.text.clone());
            let checked = st.checks.get(&(name.clone(), i)).copied().or(n.checked);
            children.push(Element {
                platform_id: n.id.clone(),
                element_type: n.ty.clone(),
                text,
                label: n.label.clone(),
                bounds: Rect {
                    x: 0,
                    y: 120 + i as i32 * ROW - offset,
                    width: W,
                    height: ROW - 20,
                },
                enabled: n.enabled,
                visible: true,
                checked,
                hint: n.hint.clone(),
                focused: Some(!n.id.is_empty() && st.focused.as_deref() == Some(n.id.as_str())),
                ..Default::default()
            });
        }
        if let Some((bar, h)) = self.overlays.get(&name) {
            if !st.dismissed.contains(&name) {
                children.push(Element {
                    platform_id: bar.id.clone(),
                    element_type: bar.ty.clone(),
                    text: bar.text.clone(),
                    label: bar.label.clone(),
                    bounds: bottom(*h),
                    enabled: true,
                    visible: true,
                    ..Default::default()
                });
            }
        }
        if let (true, Some(h)) = (st.keyboard_up, self.keyboard_h) {
            children.push(Element {
                platform_id: "keyboard".into(),
                element_type: "android.inputmethodservice.KeyboardView".into(),
                bounds: bottom(h),
                enabled: true,
                visible: true,
                ..Default::default()
            });
        }
        Element {
            platform_id: "hierarchy".into(),
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: W,
                height: H,
            },
            visible: true,
            enabled: true,
            children,
            ..Default::default()
        }
    }

    fn screen_rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: W,
            height: H,
        }
    }
}

/// The bottom `h` px of the screen.
fn bottom(h: i32) -> Rect {
    Rect {
        x: 0,
        y: H - h,
        width: W,
        height: h,
    }
}

fn not_found(selector: &Selector) -> VelocityError {
    VelocityError::ElementNotFound {
        selector: selector.to_string(),
        timeout_ms: 0,
        screenshot: None,
        hierarchy_snapshot: None,
    }
}

#[async_trait]
impl PlatformDriver for FakeApp {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }
    async fn boot_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn shutdown_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn install_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn launch_app(&self, _: &str, _: &str, clear_state: bool) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.launches.push(clear_state);
        let n = st.launches.len();
        let mut screens = self.screens.lock().unwrap();
        for (after, screen, nodes) in self.changes.lock().unwrap().iter() {
            if n >= *after {
                screens.insert(screen.clone(), nodes.clone());
            }
        }
        st.stack = vec![self.order[0].clone()];
        st.offset.clear();
        st.focused = None;
        st.keyboard_up = false;
        st.dismissed.clear();
        if clear_state {
            st.fields.clear();
            st.checks.clear();
        }
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn find_element(&self, _: &str, selector: &Selector) -> Result<Element> {
        find_in_tree(&self.tree(), selector, Some(&Self::screen_rect()))
            .ok_or_else(|| not_found(selector))
    }
    async fn find_elements(&self, _: &str, selector: &Selector) -> Result<Vec<Element>> {
        let tree = self.tree();
        Ok(find_all_ref(&tree, selector, Some(&Self::screen_rect()))
            .into_iter()
            .cloned()
            .collect())
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        Ok(self.tree())
    }
    async fn tap(&self, _: &str, el: &Element) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let name = st
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.order[0].clone())
            .trim_end_matches("#interim")
            .to_string();
        // The overlay itself: tapped away.
        if let Some((bar, _)) = self.overlays.get(&name) {
            if !st.dismissed.contains(&name) && bar.key_matches(el) {
                st.taps.push(
                    bar.text
                        .clone()
                        .or(bar.label.clone())
                        .unwrap_or(bar.id.clone()),
                );
                st.dismissed.insert(name);
                return Ok(());
            }
        }
        // A tap lands at the element's centre: whatever covers that point
        // takes it, so the step fails as the drivers' tap guard fails it.
        let (cx, cy) = el.bounds.center();
        let point = Rect {
            x: cx,
            y: cy,
            width: 1,
            height: 1,
        };
        if let Some((_, what)) = self
            .occluders(&st, &name)
            .into_iter()
            .find(|(r, _)| r.intersects(&point))
        {
            return Err(VelocityError::NotActionable {
                selector: el
                    .text
                    .clone()
                    .or(el.label.clone())
                    .unwrap_or(el.platform_id.clone()),
                reason: format!("covered by {what}"),
            });
        }
        let field = self.screens.lock().unwrap()[&name]
            .iter()
            .find(|n| n.editable() && n.key_matches(el))
            .map(|n| n.id.clone());
        match field {
            Some(id) => {
                st.focused = Some(id);
                st.keyboard_up = self.keyboard_h.is_some();
            }
            None => {
                st.focused = None;
                st.keyboard_up = false;
            }
        }
        let keys = [
            Some(el.platform_id.clone()),
            el.text.clone(),
            el.label.clone(),
        ];
        st.taps.push(
            el.text
                .clone()
                .or(el.label.clone())
                .unwrap_or(el.platform_id.clone()),
        );
        let flip = self.screens.lock().unwrap()[&name]
            .iter()
            .enumerate()
            .find(|(_, n)| {
                n.checked.is_some()
                    && ((!n.id.is_empty() && n.id == el.platform_id)
                        || (n.text.is_some() && n.text == el.text)
                        || (n.label.is_some() && n.label == el.label))
            })
            .map(|(i, n)| (i, n.checked.unwrap_or(false)));
        if let Some((i, start)) = flip {
            let now = st.checks.get(&(name.clone(), i)).copied().unwrap_or(start);
            st.checks.insert((name.clone(), i), !now);
        }
        if let Some(to) = keys
            .iter()
            .flatten()
            .find_map(|k| self.links.get(&(name.clone(), k.clone())))
        {
            let interim = keys
                .iter()
                .flatten()
                .find_map(|k| self.interims.get(&(name.clone(), k.clone())));
            if let Some((i, reads)) = interim {
                st.stack.push(format!("{i}#interim"));
                st.pending = Some((to.clone(), *reads));
            } else if self.lag_reads > 0 {
                st.pending = Some((to.clone(), self.lag_reads));
            } else {
                st.stack.push(to.clone());
            }
        }
        Ok(())
    }
    async fn double_tap(&self, d: &str, el: &Element) -> Result<()> {
        self.tap(d, el).await
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        Ok(())
    }
    async fn input_text(&self, _: &str, el: &Element, text: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.focused = Some(el.platform_id.clone());
        st.keyboard_up = self.keyboard_h.is_some();
        st.fields.insert(el.platform_id.clone(), text.to_string());
        st.typed.push((el.platform_id.clone(), text.to_string()));
        Ok(())
    }
    async fn clear_text(&self, _: &str, el: &Element) -> Result<()> {
        self.state.lock().unwrap().fields.remove(&el.platform_id);
        Ok(())
    }
    async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
        let (a, b) = match dir {
            Direction::Up => ((540, 1200), (540, 600)),
            _ => ((540, 600), (540, 1200)),
        };
        self.swipe_coords(d, a, b).await
    }
    async fn swipe_coords(&self, _: &str, from: (i32, i32), to: (i32, i32)) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let name = st
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.order[0].clone())
            .trim_end_matches("#interim")
            .to_string();
        let rows = self.screens.lock().unwrap()[&name].len() as i32;
        let max = (120 + rows * ROW - H).max(0);
        let o = st.offset.entry(name).or_default();
        // Finger up (to.y < from.y) shows what is below.
        *o = (*o + (from.1 - to.1)).clamp(0, max);
        Ok(())
    }
    async fn press_key(&self, _: &str, key: Key) -> Result<()> {
        if key == Key::Back {
            let mut st = self.state.lock().unwrap();
            if st.keyboard_up {
                // Back closes the keyboard first, as on Android.
                st.keyboard_up = false;
            } else if st.stack.len() > 1 {
                st.stack.pop();
                st.focused = None;
            }
        }
        Ok(())
    }
    /// A real PNG at a quarter of the screen's size: white, with each
    /// coloured node drawn as its rectangle, so the pixel paths (swatch
    /// colours, the change check, the model's downscaled copy) run end to
    /// end (review 07 H2). Reading it moves no late change along.
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        const SCALE: i32 = 4;
        let (w, h) = ((W / SCALE) as usize, (H / SCALE) as usize);
        let mut rgb = vec![255u8; w * h * 3];
        let (name, offset) = {
            let st = self.state.lock().unwrap();
            let name = st
                .stack
                .last()
                .cloned()
                .unwrap_or_else(|| self.order[0].clone())
                .trim_end_matches("#interim")
                .to_string();
            let offset = st.offset.get(&name).copied().unwrap_or(0);
            (name, offset)
        };
        let screens = self.screens.lock().unwrap();
        for (i, n) in screens[&name].iter().enumerate() {
            let Some(c) = n.color else { continue };
            let y0 = (120 + i as i32 * ROW - offset) / SCALE;
            let y1 = y0 + (ROW - 20) / SCALE;
            for y in y0.max(0)..y1.min(h as i32) {
                for x in 0..w {
                    let p = (y as usize * w + x) * 3;
                    rgb[p..p + 3].copy_from_slice(&c);
                }
            }
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w as u32, h as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc
                .write_header()
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("fake png: {e}")))?;
            wr.write_image_data(&rgb)
                .map_err(|e| VelocityError::Internal(anyhow::anyhow!("fake png: {e}")))?;
        }
        Ok(bytes)
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((W, H))
    }
    async fn get_element_text(&self, _: &str, el: &Element) -> Result<String> {
        Ok(el.text.clone().unwrap_or_default())
    }
    async fn is_element_visible(&self, _: &str, el: &Element) -> Result<bool> {
        let tree = self.tree();
        Ok(tree.children.iter().any(|e| {
            e.bounds == el.bounds
                && e.platform_id == el.platform_id
                && e.label == el.label
                && e.bounds.intersects(&Self::screen_rect())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap_text(text: &str) -> Selector {
        Selector::Text(text.into())
    }

    #[tokio::test]
    async fn a_bar_over_the_bottom_swallows_taps_until_tapped_away() {
        let mut rows: Vec<Node> = (0..12)
            .map(|i| Node::text("Button", &format!("Row {i}")))
            .collect();
        rows.push(Node::text("Button", "Pay"));
        let app = FakeApp::new().screen("home", rows).overlay(
            "home",
            Node::text("Button", "Accept cookies"),
            400,
        );
        let low = app.find_element("d", &tap_text("Row 9")).await.unwrap();
        let e = app.tap("d", &low).await.unwrap_err();
        assert!(e.to_string().contains("covered by Accept cookies"), "{e}");
        let high = app.find_element("d", &tap_text("Row 0")).await.unwrap();
        app.tap("d", &high).await.unwrap();
        let bar = app
            .find_element("d", &tap_text("Accept cookies"))
            .await
            .unwrap();
        app.tap("d", &bar).await.unwrap();
        let low = app.find_element("d", &tap_text("Row 9")).await.unwrap();
        app.tap("d", &low).await.unwrap();
    }

    #[tokio::test]
    async fn a_text_field_opens_a_keyboard_that_covers_the_bottom_and_back_closes_first() {
        let mut rows = vec![Node::field("name", "Your name")];
        rows.extend((1..12).map(|i| Node::text("Button", &format!("Row {i}"))));
        let app = FakeApp::new()
            .screen("home", vec![Node::text("Button", "Form")])
            .screen("form", rows)
            .link("home", "Form", "form")
            .keyboard(800);
        app.launch_app("d", "app", false).await.unwrap();
        let form = app.find_element("d", &tap_text("Form")).await.unwrap();
        app.tap("d", &form).await.unwrap();
        let tree = app.get_hierarchy("d").await.unwrap();
        let field = tree
            .children
            .iter()
            .find(|e| e.platform_id == "name")
            .unwrap();
        assert_eq!(field.text, None, "a placeholder is not text");
        assert_eq!(field.hint.as_deref(), Some("Your name"));
        assert_eq!(field.focused, Some(false));

        app.tap("d", field).await.unwrap();
        assert!(app.keyboard_up());
        let tree = app.get_hierarchy("d").await.unwrap();
        assert!(tree
            .children
            .iter()
            .any(|e| e.platform_id == "name" && e.focused == Some(true)));
        let low = app.find_element("d", &tap_text("Row 9")).await.unwrap();
        let e = app.tap("d", &low).await.unwrap_err();
        assert!(e.to_string().contains("covered by the keyboard"), "{e}");

        app.press_key("d", Key::Back).await.unwrap();
        assert!(!app.keyboard_up());
        assert_eq!(
            app.current(),
            "form",
            "the first Back only closed the keyboard"
        );
        app.press_key("d", Key::Back).await.unwrap();
        assert_eq!(app.current(), "home");
    }
}
