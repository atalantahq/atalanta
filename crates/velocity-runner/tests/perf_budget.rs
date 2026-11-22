//! C6: a performance budget in driver calls. Each action type runs through
//! the real executor against a counting fake driver, and the calls it
//! makes (tree reads, element lookups, input) are checked against a
//! budget. Time on a device is dominated by these round trips (a tree
//! dump is 50–500 ms on Android), so a change that adds one per action is
//! a slowdown even when every test still passes.
//!
//! The budgets are the counts measured when this test was written
//! (2026-10-16). A change that lowers a count should lower its budget; one
//! that raises it must justify it and raise the budget in the same change.
//! `VELOCITY_PERF_PRINT=1 cargo test -p velocity-runner --test perf_budget
//! -- --nocapture` prints the measured counts.
//!
//! The counts are the steady state. The first time a selector finds an
//! element, self-healing reads the tree once more to snapshot its context,
//! and keeps that snapshot in its state directory (`.velocity/` of a real
//! project) so later runs don't pay it. Each test keeps that state in its
//! own temp dir: it used to land in this crate's directory, where whatever
//! an earlier run (or the other test, running at the same time) had left
//! decided whether `tap` read 6 trees or 7.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use velocity_common::{
    DeviceInfo, Direction, Element, Key, PlatformDriver, Rect, Result, Selector, TestStatus,
    VelocityError,
};

/// A screen that reacts to every input (a counter on it changes, as a
/// real app's screen does), so the settle after an action sees its
/// reaction at once and its count doesn't depend on timing; every driver
/// call is counted by name.
struct Counting {
    screen: Element,
    inputs: std::sync::atomic::AtomicUsize,
    calls: Mutex<BTreeMap<&'static str, usize>>,
    /// Wall-clock time each tree read takes (a loaded machine).
    read_delay: std::time::Duration,
}

impl Counting {
    fn new() -> Self {
        let el = |id: &str, text: &str, ty: &str, y: i32| Element {
            platform_id: id.into(),
            text: Some(text.into()),
            label: Some(text.into()),
            element_type: ty.into(),
            bounds: Rect {
                x: 20,
                y,
                width: 300,
                height: 60,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        };
        let screen = Element {
            platform_id: "root".into(),
            element_type: "Window".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            enabled: true,
            visible: true,
            children: vec![
                el("title", "Welcome", "StaticText", 100),
                el("email", "Email", "TextField", 200),
                el("login", "Log in", "Button", 300),
            ],
            ..Default::default()
        };
        Self {
            screen,
            inputs: Default::default(),
            calls: Mutex::new(BTreeMap::new()),
            read_delay: std::time::Duration::ZERO,
        }
    }

    fn hit(&self, name: &'static str) {
        *self.calls.lock().unwrap().entry(name).or_default() += 1;
    }

    /// An input: the screen reacts.
    fn input(&self, name: &'static str) {
        self.hit(name);
        self.inputs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn current(&self) -> Element {
        let mut screen = self.screen.clone();
        let n = self.inputs.load(std::sync::atomic::Ordering::SeqCst);
        screen.children.push(Element {
            platform_id: "counter".into(),
            text: Some(format!("inputs: {n}")),
            element_type: "StaticText".into(),
            bounds: Rect {
                x: 20,
                y: 500,
                width: 300,
                height: 40,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        });
        screen
    }

    fn take(&self) -> BTreeMap<&'static str, usize> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn matches(&self, selector: &Selector) -> Vec<Element> {
        velocity_common::selector_match::find_all_ref(&self.screen, selector, None)
            .into_iter()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl PlatformDriver for Counting {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.hit("list_devices");
        Ok(vec![])
    }
    async fn boot_device(&self, _: &str) -> Result<()> {
        self.hit("boot_device");
        Ok(())
    }
    async fn shutdown_device(&self, _: &str) -> Result<()> {
        self.hit("shutdown_device");
        Ok(())
    }
    async fn install_app(&self, _: &str, _: &str) -> Result<()> {
        self.hit("install_app");
        Ok(())
    }
    async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
        self.input("launch_app");
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        self.hit("stop_app");
        Ok(())
    }
    async fn find_element(&self, _: &str, selector: &Selector) -> Result<Element> {
        self.hit("find_element");
        self.matches(selector)
            .into_iter()
            .next()
            .ok_or_else(|| VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            })
    }
    async fn find_elements(&self, _: &str, selector: &Selector) -> Result<Vec<Element>> {
        self.hit("find_elements");
        Ok(self.matches(selector))
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        self.hit("get_hierarchy");
        // Blocking on purpose: what a busy CPU does to a read, without
        // touching the (paused) test clock.
        std::thread::sleep(self.read_delay);
        Ok(self.current())
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        self.input("tap");
        Ok(())
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        self.input("double_tap");
        Ok(())
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        self.input("long_press");
        Ok(())
    }
    async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
        self.input("input_text");
        Ok(())
    }
    async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
        self.input("clear_text");
        Ok(())
    }
    async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
        self.input("swipe");
        Ok(())
    }
    async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        self.input("swipe_coords");
        Ok(())
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        self.input("press_key");
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        self.hit("screenshot");
        Ok(vec![])
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        self.hit("screen_size");
        Ok((400, 800))
    }
    async fn get_element_text(&self, _: &str, el: &Element) -> Result<String> {
        self.hit("get_element_text");
        Ok(el.text.clone().unwrap_or_default())
    }
    async fn is_element_visible(&self, _: &str, el: &Element) -> Result<bool> {
        self.hit("is_element_visible");
        Ok(el.visible)
    }
}

/// The driver calls one step makes, after the same launch, with the
/// self-healing state kept in `state` (see the module docs).
///
/// `sync.mode: polling`: the default (`auto`) first tries the in-app sync
/// probe over TCP, finds none here and polls with the same engine, so the
/// driver calls are the same; but a real socket wait on the paused test
/// clock lets tokio jump the clock past the connect timeout, which would
/// make the measured settle the odd one out.
async fn calls_for(step: &str, state: &std::path::Path) -> BTreeMap<&'static str, usize> {
    calls_with_delay(step, std::time::Duration::ZERO, state).await
}

async fn calls_with_delay(
    step: &str,
    read_delay: std::time::Duration,
    state: &std::path::Path,
) -> BTreeMap<&'static str, usize> {
    let yaml = format!(
        "appId: com.example.app\nconfig:\n  platform: android\n  sync:\n    mode: polling\ntests:\n  - name: t\n    steps:\n      - {step}\n"
    );
    let suite = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
    let driver = Counting {
        read_delay,
        ..Counting::new()
    };
    let mut exec =
        velocity_core::executor::TestExecutor::new(&driver, suite.config.clone(), &suite.app_id)
            .with_state_dir(state);
    let result = exec
        .execute_test(&suite.tests[0], "d", &suite.app_id)
        .await
        .unwrap();
    assert_eq!(
        result.status,
        TestStatus::Passed,
        "{step}: {:?}",
        result.error_message
    );
    driver.take()
}

/// (step, budget per call name). A call name missing from a budget is
/// budgeted at 0.
fn budgets() -> Vec<(&'static str, Vec<(&'static str, usize)>)> {
    vec![
        // Filled in from the measured counts; see the module docs.
        include!("perf_budget.in"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// One test, steps one after another: the executor keeps some state per
/// process (selector match counts, the last stable screen), and steps run
/// concurrently in one process read one more tree each.
///
/// The clock is tokio's paused test clock: the settle's reaction floor and
/// quiet window are timed on it, so a loaded machine (a parallel workspace
/// test run) can't make a sleep overshoot and change how many trees a
/// settle reads.
#[tokio::test(start_paused = true)]
async fn driver_calls_per_action_stay_within_budget() {
    let print = std::env::var("VELOCITY_PERF_PRINT").is_ok();
    let mut over = Vec::new();
    for (step, budget) in budgets() {
        // A fresh state per step: the first run sees its selector for the
        // first time.
        let state = tempfile::tempdir().unwrap();
        let first = calls_for(step, state.path()).await;
        let got = calls_for(step, state.path()).await;
        if print {
            println!("{step:<45} {got:?} (first run: {first:?})");
        }
        // The counts don't depend on timing: a third run agrees, so the
        // budget can be exact.
        assert_eq!(
            calls_for(step, state.path()).await,
            got,
            "{step}: counts differ between runs"
        );
        // The first sight of a selector costs one tree read at most (the
        // healing snapshot), and nothing else.
        let mut first_reads = first.clone();
        let extra = first_reads
            .remove("get_hierarchy")
            .unwrap_or(0)
            .checked_sub(got.get("get_hierarchy").copied().unwrap_or(0));
        let mut steady = got.clone();
        steady.remove("get_hierarchy");
        assert!(
            matches!(extra, Some(0 | 1)) && first_reads == steady,
            "{step}: first run {first:?}, then {got:?}"
        );
        let budget: BTreeMap<&str, usize> = budget.into_iter().collect();
        for (call, n) in &got {
            let max = budget.get(call).copied().unwrap_or(0);
            if *n > max {
                over.push(format!("{step}: {call} called {n}× (budget {max})"));
            }
        }
        // The budget is an upper bound, so a step that stopped doing its
        // work (no `tap`, no `input_text`) would stay inside it and still
        // pass: every budgeted call must happen at least once.
        for (call, max) in &budget {
            if *max > 0 && !got.contains_key(call) {
                over.push(format!("{step}: {call} never called (budget {max})"));
            }
        }
    }
    assert!(
        over.is_empty(),
        "over the driver-call budget:\n  {}",
        over.join("\n  ")
    );
}

/// fi56: under a parallel workspace test run the launch settle read 13
/// trees on one run and 14 on the next: its 25 ms samples overshot on a
/// busy CPU. Each read here takes 20 ms of wall clock, as on a loaded
/// machine; the counts must not move.
#[tokio::test(start_paused = true)]
async fn counts_do_not_depend_on_how_busy_the_machine_is() {
    let state = tempfile::tempdir().unwrap();
    for step in [
        "launchApp: { appId: com.example.app }",
        "tap: { id: login }",
    ] {
        // Past the selector's first sight (see the module docs).
        calls_for(step, state.path()).await;
        let fast = calls_for(step, state.path()).await;
        let slow = calls_with_delay(step, std::time::Duration::from_millis(20), state.path()).await;
        assert_eq!(fast, slow, "{step}: counts depend on wall-clock time");
    }
}

/// The healing state goes where the test says, never into the directory
/// the tests run in (they used to leave `.velocity/healer-snapshots.json`
/// in this crate).
#[tokio::test(start_paused = true)]
async fn the_healing_state_stays_in_the_state_dir() {
    let state = tempfile::tempdir().unwrap();
    calls_for("tap: { id: login }", state.path()).await;
    assert!(state.path().join("healer-snapshots.json").is_file());
}
