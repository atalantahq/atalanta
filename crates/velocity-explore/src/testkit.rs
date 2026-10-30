//! A fake app for tests: screens of named elements and the taps that move
//! between them, behind a real [`PlatformDriver`]. The executor runs
//! against it unchanged (selectors, actionability, settle), so an explore
//! session and its replay can be tested end to end without a device.
//!
//! Every screen shows a status-bar clock whose time changes on every read
//! (volatile text that must never be asserted), and a typed value shows in
//! its field (user data that must never be asserted either).

use std::collections::HashMap;
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
        }
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
        }
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
        let name = st
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| self.order[0].clone());
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
            .unwrap_or_else(|| self.order[0].clone());
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
            st.stack.push(to.clone());
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
            .unwrap_or_else(|| self.order[0].clone());
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
            if st.stack.len() > 1 {
                st.stack.pop();
            }
        }
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        Ok(b"\x89PNG fake".to_vec())
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
