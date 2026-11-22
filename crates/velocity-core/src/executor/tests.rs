use super::actions::{finger_for_scroll, line_across, set_sync_field, system_state_mismatch};
use super::lookup::{aim_at, shown_part};
use super::screenshot::{ci_flag, has_status_bar, legacy_baseline_file, ScreenshotStep};
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn a_model_api_outage_is_not_a_device_failure() {
    // Review M5: an Anthropic 429/529 storm marked every NL failure as
    // infrastructure, and the farm quarantined healthy devices.
    let down = VelocityError::ServiceUnavailable {
        service: "Anthropic API".into(),
        detail: "HTTP 529".into(),
    };
    assert!(down.is_transient());
    assert!(!is_infrastructure(&down));
    let act = Action::Tap {
        selector: Selector::Text("Pay".into()),
    };
    assert!(!is_infrastructure(&annotate(&act, down, None)));
}

#[test]
fn a_natural_step_keeps_infrastructure_and_crash_errors() {
    // VEL-073: they became Config errors, losing infrastructure_failure
    // and the crash flag for natural steps.
    let act = Action::Tap {
        selector: Selector::Text("Pay".into()),
    };
    let lost = VelocityError::AdbConnectionLost {
        device_id: "d".into(),
        reason: "gone".into(),
    };
    let e = annotate(&act, lost, None);
    assert!(is_infrastructure(&e), "{e:?}");
    let crashed = VelocityError::AppCrashed {
        app_id: "app".into(),
        detail: "boom".into(),
    };
    match annotate(&act, crashed, None) {
        VelocityError::AppCrashed { detail, .. } => {
            assert!(
                detail.contains("boom") && detail.contains("Pay"),
                "{detail}"
            )
        }
        other => panic!("{other:?}"),
    }
    // Anything else still says what the step resolved to.
    let e = annotate(&act, VelocityError::Config("x".into()), None);
    assert!(e.to_string().contains("resolved to"), "{e}");
}

#[test]
fn strokes_stay_inside_the_element() {
    let b = Rect {
        x: 40,
        y: 300,
        width: 1000,
        height: 600,
    };
    assert_eq!(line_across(&b, Direction::Left), ((840, 600), (240, 600)));
    assert_eq!(line_across(&b, Direction::Up), ((540, 780), (540, 420)));
    for d in [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ] {
        let ((x1, y1), (x2, y2)) = line_across(&b, d);
        for (x, y) in [(x1, y1), (x2, y2)] {
            assert!(
                x > b.x && x < b.x + b.width && y > b.y && y < b.y + b.height,
                "{d:?}"
            );
        }
    }
}

#[test]
fn set_config_accepts_settings_and_sync_timings_only() {
    assert!(check_set_config("android.scroll", "semantic").is_ok());
    assert!(check_set_config("sync.quietMs", "50").is_ok());
    assert!(check_set_config("sync.quietMs", "fast")
        .unwrap_err()
        .contains("milliseconds"));
    assert!(check_set_config("sync.mode", "none").is_err());
    assert!(check_set_config("andriod.scroll", "x").is_err());
    let mut cfg = velocity_common::SyncConfig::default();
    set_sync_field(&mut cfg, "sync.settleCapMs", "250").unwrap();
    assert_eq!(cfg.settle_cap_ms, Some(250));
}

#[test]
fn system_state_expectations_report_each_mismatch() {
    let playing = velocity_common::SystemState {
        media_playing: true,
        media_titles: vec!["Velocity Lab Song".into()],
        in_call: false,
    };
    assert!(system_state_mismatch(&playing, Some(true), Some("Lab Song"), Some(false)).is_empty());
    let m = system_state_mismatch(&playing, Some(false), Some("Other"), Some(true));
    assert_eq!(m.len(), 3, "{m:?}");
}

#[test]
fn an_off_screen_target_gets_one_aimed_drag() {
    let screen = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 1000,
    };
    let row = |y| Rect {
        x: 0,
        y,
        width: 400,
        height: 50,
    };
    // Below the screen: the finger moves up by the gap, capped at 60%.
    assert_eq!(
        aim_at(&screen, &row(1175), Direction::Down, false),
        Some(((200, 800), (200, 200)))
    );
    assert_eq!(
        aim_at(&screen, &row(1020), Direction::Down, false),
        Some(((200, 772), (200, 228)))
    );
    // Above (overshot): the finger moves down.
    assert_eq!(
        aim_at(&screen, &row(-200), Direction::Down, false),
        Some(((200, 200), (200, 800)))
    );
    // On screen, or no frame: the normal scroll.
    assert_eq!(aim_at(&screen, &row(400), Direction::Down, true), None);
    // Visible but under the top bar: one small drag down to the middle.
    assert_eq!(
        aim_at(&screen, &row(40), Direction::Down, true),
        Some(((200, 283), (200, 717)))
    );
    assert_eq!(
        aim_at(
            &screen,
            &Rect {
                x: 0,
                y: 2000,
                width: 0,
                height: 0
            },
            Direction::Down,
            false
        ),
        None
    );
}

#[test]
fn scrolling_down_swipes_the_finger_up() {
    assert_eq!(finger_for_scroll(Direction::Down), Direction::Up);
    assert_eq!(finger_for_scroll(Direction::Up), Direction::Down);
    assert_eq!(finger_for_scroll(Direction::Right), Direction::Left);
    assert_eq!(finger_for_scroll(Direction::Left), Direction::Right);
}
use std::sync::Mutex;
use velocity_common::selector_match::find_in_tree;
use velocity_common::{DeviceInfo, Key, Rect, SyncMode};

fn node(id: &str, text: &str, y: i32) -> Element {
    Element {
        hint: None,
        platform_id: id.into(),
        label: None,
        text: Some(text.into()),
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

/// A screen that changes on a schedule: after `n` element lookups,
/// `change` runs once. Counts lookups, dumps and taps.
type ScreenChange = Box<dyn FnOnce(&mut Element) + Send>;

/// Take one from a countdown; false once it is at zero. A compare-exchange
/// loop: `fetch_update` is deprecated in newer Rust.
fn take_one(a: &AtomicUsize) -> bool {
    let mut cur = a.load(Ordering::SeqCst);
    while cur > 0 {
        match a.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => cur = seen,
        }
    }
    false
}

/// A `drag`'s (from, to).
type DragLine = ((i32, i32), (i32, i32));

struct ScriptDriver {
    /// Capture video and logs: the notes its logs report.
    evidence: Option<Vec<String>>,
    /// (keep_video, keep_logs) of every finished capture.
    evidence_finished: Arc<Mutex<Vec<(bool, bool)>>>,
    screen: Mutex<Element>,
    script: Mutex<Vec<(usize, ScreenChange)>>,
    finds: AtomicUsize,
    dumps: AtomicUsize,
    taps: Mutex<Vec<String>>,
    /// Where each tap landed (the tapped element's top edge).
    tap_ys: Mutex<Vec<i32>>,
    /// The actionability rule each tap ran under (D-3).
    tap_modes: Mutex<Vec<velocity_common::actionability::ActMode>>,
    /// `navigate` (openLink/goto) succeeds.
    navigates: std::sync::atomic::AtomicBool,
    /// What `current_url` says.
    page_url: Mutex<Option<String>>,
    cached: bool,
    /// Lookups left that fail as an unreadable screen (CommandFailed).
    unreadable: AtomicUsize,
    /// Every lookup fails as a lost device.
    lost: std::sync::atomic::AtomicBool,
    /// Every lookup and tree read fails as the headless driver's "add a
    /// launchApp step" (a config error, VEL-255).
    unlaunched: std::sync::atomic::AtomicBool,
    /// What `app_liveness` says (None: the trait's default), and how
    /// often it was asked.
    liveness: Mutex<Option<velocity_common::Liveness>>,
    liveness_reads: AtomicUsize,
    /// What `system_bars` says.
    bars: Option<Vec<Rect>>,
    /// Lookups left before every lookup fails as a lost device.
    lose_after: AtomicUsize,
    /// `element_text_matches` ignores case (a driver that normalises
    /// the text it compares, as web does).
    fold_text: bool,
    /// Every drag (`swipe_coords` / `scroll_coords`).
    strokes: Mutex<Vec<Stroke>>,
    /// A drag moves the whole screen's content with the finger.
    pans: bool,
    /// `tap` on a disabled element fails as NotActionable (web).
    refuse_disabled: bool,
    /// `reveal` scrolls an off-screen element to y = 200, moving the
    /// whole screen (web's `scrollIntoViewIfNeeded`), and counts calls.
    reveals: bool,
    revealed: AtomicUsize,
    /// What `screenshot` returns, in turn (the last one repeats, or all
    /// of them cycle with `cycle_frames`), and how often it was asked.
    frames: Mutex<Vec<Vec<u8>>>,
    cycle_frames: bool,
    shots: AtomicUsize,
    /// How often `freeze_status_bar` was asked (it always takes).
    freezes: AtomicUsize,
    /// Added to the screen right after the deadline's widest-tier look
    /// misses (a target that renders just as the wait runs out).
    late: Mutex<Option<Element>>,
    /// `find_elements` calls (the ambiguity check's extra lookups).
    find_alls: AtomicUsize,
    /// Count on-screen matches in `find_element`, as the real drivers do.
    counts: bool,
    /// Taps left that the driver refuses as covered by a bottom toolbar.
    barred_taps: AtomicUsize,
    /// Every `drag` (from, to).
    drags: Mutex<Vec<DragLine>>,
    match_counts: velocity_common::selector_match::MatchCounts,
    /// Model Android's full-tree lookup cache: `find_element` and
    /// `get_hierarchy_cached` resolve against the cached tree (filled
    /// from the screen when empty, before the script advances); the
    /// settle's compare reads see the live screen and never fill it;
    /// `invalidate_hierarchy_cache` drops it.
    lookup_cache: Option<Mutex<Option<Element>>>,
    /// I3: the crash `pending_crash` reports once a tap landed and at
    /// least `pending_after` lookups ran; a launch clears it.
    pending: Mutex<Option<String>>,
    pending_after: AtomicUsize,
    /// Recorded launches: `(device_id, app_id, clear_state, arguments)`.
    launches: Mutex<Vec<(String, String, bool, velocity_common::LaunchArgs)>>,
}

type Stroke = ((i32, i32), (i32, i32));

fn shift(el: &mut Element, dx: i32, dy: i32) {
    for c in &mut el.children {
        c.bounds.x += dx;
        c.bounds.y += dy;
        shift(c, dx, dy);
    }
}

impl ScriptDriver {
    fn new(children: Vec<Element>) -> Self {
        Self {
            screen: Mutex::new(Element {
                children,
                text: None,
                element_type: "hierarchy".into(),
                ..node("hierarchy", "", 0)
            }),
            script: Mutex::new(Vec::new()),
            finds: AtomicUsize::new(0),
            dumps: AtomicUsize::new(0),
            taps: Mutex::new(Vec::new()),
            barred_taps: AtomicUsize::new(0),
            drags: Mutex::new(Vec::new()),
            tap_ys: Mutex::new(Vec::new()),
            tap_modes: Mutex::new(Vec::new()),
            navigates: Default::default(),
            page_url: Mutex::new(None),
            cached: false,
            unreadable: AtomicUsize::new(0),
            lost: Default::default(),
            unlaunched: Default::default(),
            liveness: Mutex::new(None),
            liveness_reads: AtomicUsize::new(0),
            bars: None,
            lose_after: AtomicUsize::new(usize::MAX),
            fold_text: false,
            strokes: Mutex::new(Vec::new()),
            pans: false,
            refuse_disabled: false,
            reveals: false,
            revealed: AtomicUsize::new(0),
            frames: Mutex::new(Vec::new()),
            cycle_frames: false,
            shots: AtomicUsize::new(0),
            freezes: AtomicUsize::new(0),
            late: Mutex::new(None),
            find_alls: AtomicUsize::new(0),
            counts: true,
            match_counts: Default::default(),
            lookup_cache: None,
            pending: Mutex::new(None),
            pending_after: AtomicUsize::new(0),
            launches: Mutex::new(Vec::new()),
            evidence: None,
            evidence_finished: Default::default(),
        }
    }

    fn after(self, n: usize, change: impl FnOnce(&mut Element) + Send + 'static) -> Self {
        self.script.lock().unwrap().push((n, Box::new(change)));
        self
    }

    fn advance(&self) {
        let n = self.finds.fetch_add(1, Ordering::SeqCst) + 1;
        let mut script = self.script.lock().unwrap();
        let mut screen = self.screen.lock().unwrap();
        let mut i = 0;
        while i < script.len() {
            if script[i].0 <= n {
                (script.remove(i).1)(&mut screen);
            } else {
                i += 1;
            }
        }
    }
}
/// A capture that writes a fake video and log when kept.
struct FakeEvidence {
    notes: Vec<String>,
    finished: Arc<Mutex<Vec<(bool, bool)>>>,
}

#[async_trait::async_trait]
impl velocity_common::EvidenceSession for FakeEvidence {
    async fn finish(
        self: Box<Self>,
        out: velocity_common::EvidenceOut<'_>,
    ) -> velocity_common::Evidence {
        use velocity_common::{Attachment, AttachmentKind};
        self.finished
            .lock()
            .unwrap()
            .push((out.keep_video, out.keep_logs));
        let mut ev = velocity_common::Evidence {
            notes: self.notes.clone(),
            ..Default::default()
        };
        std::fs::create_dir_all(out.dir).unwrap();
        if out.keep_video {
            let p = out.file("video", "mp4");
            std::fs::write(&p, b"mp4").unwrap();
            ev.video_offset_ms = Some(-40);
            ev.attachments
                .push(Attachment::new(AttachmentKind::Video, p));
        }
        if out.keep_logs {
            let p = out.file("app", "log");
            std::fs::write(&p, b"log").unwrap();
            ev.attachments
                .push(Attachment::new(AttachmentKind::AppLog, p));
            ev.log_lines.push(velocity_common::evidence::LogLine {
                t: 5,
                level: 'E',
                text: "boom".into(),
            });
        }
        ev
    }
}

#[async_trait::async_trait]
impl PlatformDriver for ScriptDriver {
    async fn start_evidence(
        &self,
        _: &str,
        _: &velocity_common::EvidenceRequest,
    ) -> Option<Box<dyn velocity_common::EvidenceSession>> {
        self.evidence.clone().map(|notes| {
            Box::new(FakeEvidence {
                notes,
                finished: self.evidence_finished.clone(),
            }) as Box<dyn velocity_common::EvidenceSession>
        })
    }
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
    async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
        *self.pending.lock().unwrap() = None;
        Ok(())
    }
    async fn launch_app_with(
        &self,
        d: &str,
        a: &str,
        c: bool,
        args: &velocity_common::LaunchArgs,
    ) -> Result<()> {
        *self.pending.lock().unwrap() = None;
        self.launches
            .lock()
            .unwrap()
            .push((d.to_string(), a.to_string(), c, args.clone()));
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn pending_crash(&self, _: &str, _: &str) -> Option<String> {
        if self.taps.lock().unwrap().is_empty()
            || self.finds.load(Ordering::SeqCst) < self.pending_after.load(Ordering::SeqCst)
        {
            return None;
        }
        self.pending.lock().unwrap().clone()
    }
    async fn find_element(&self, device_id: &str, selector: &Selector) -> Result<Element> {
        if self.unlaunched.load(Ordering::SeqCst) {
            self.finds.fetch_add(1, Ordering::SeqCst);
            return Err(VelocityError::Config("add a launchApp step".into()));
        }
        let cached_tree = self.lookup_cache.as_ref().map(|c| {
            c.lock()
                .unwrap()
                .get_or_insert_with(|| self.screen.lock().unwrap().clone())
                .clone()
        });
        self.advance();
        if !take_one(&self.lose_after) {
            self.lost.store(true, Ordering::SeqCst);
        }
        if self.lost.load(Ordering::SeqCst) {
            return Err(VelocityError::AdbConnectionLost {
                device_id: "d".into(),
                reason: "device offline".into(),
            });
        }
        if take_one(&self.unreadable) {
            return Err(VelocityError::CommandFailed {
                device_id: "d".into(),
                command: "uiautomator dump".into(),
                detail: "null root node".into(),
            });
        }
        let found = {
            let live = self.screen.lock().unwrap();
            let screen = cached_tree.as_ref().unwrap_or(&live);
            let found = find_in_tree(screen, selector, None);
            if found.is_some() && self.counts {
                // Shown matches, as the web's page call counts them.
                let n = velocity_common::selector_match::find_all_ref(screen, selector, None)
                    .into_iter()
                    .filter(|e| e.visible && !e.bounds.is_empty())
                    .count();
                self.match_counts.record(device_id, selector, n);
            } else if found.is_none() {
                self.match_counts.clear(device_id);
            }
            found
        };
        if velocity_common::selector_match::ceiling()
            == velocity_common::selector_match::Tier::Loose
        {
            if let Some(el) = self.late.lock().unwrap().take() {
                self.screen.lock().unwrap().children.push(el);
            }
        }
        found.ok_or_else(|| VelocityError::ElementNotFound {
            selector: selector.to_string(),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        })
    }
    fn last_match_count(&self, device_id: &str, selector: &Selector) -> Option<usize> {
        self.match_counts.get(device_id, selector)
    }
    async fn find_elements(&self, _: &str, selector: &Selector) -> Result<Vec<Element>> {
        self.find_alls.fetch_add(1, Ordering::SeqCst);
        let screen = self.screen.lock().unwrap();
        Ok(
            velocity_common::selector_match::find_all_ref(&screen, selector, None)
                .into_iter()
                .cloned()
                .collect(),
        )
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        self.dumps.fetch_add(1, Ordering::SeqCst);
        if self.unlaunched.load(Ordering::SeqCst) {
            return Err(VelocityError::Config("add a launchApp step".into()));
        }
        Ok(self.screen.lock().unwrap().clone())
    }
    fn has_hierarchy_cache(&self) -> bool {
        self.cached || self.lookup_cache.is_some()
    }
    async fn get_hierarchy_cached(&self, d: &str) -> Result<Element> {
        match &self.lookup_cache {
            Some(c) => Ok(c
                .lock()
                .unwrap()
                .get_or_insert_with(|| self.screen.lock().unwrap().clone())
                .clone()),
            None => self.get_hierarchy(d).await,
        }
    }
    async fn invalidate_hierarchy_cache(&self) {
        if let Some(c) = &self.lookup_cache {
            *c.lock().unwrap() = None;
        }
    }
    async fn current_url(&self, _: &str) -> Option<String> {
        self.page_url.lock().unwrap().clone()
    }
    async fn navigate(&self, _: &str, _: &str) -> Result<()> {
        if self.navigates.load(Ordering::SeqCst) {
            return Ok(());
        }
        Err(VelocityError::Config(
            "openLink/goto isn't supported on this platform".into(),
        ))
    }
    async fn tap(&self, _: &str, el: &Element) -> Result<()> {
        if self
            .barred_taps
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(VelocityError::AssertionFailed {
                expected: "reachable".into(),
                actual: "covered by the toolbar; scroll it into view first".into(),
                selector: el.platform_id.clone(),
                screenshot: None,
            });
        }
        if self.refuse_disabled && !el.enabled {
            return Err(VelocityError::NotActionable {
                selector: el.platform_id.clone(),
                reason: "disabled".into(),
            });
        }
        self.taps
            .lock()
            .unwrap()
            .push(el.text.clone().unwrap_or_default());
        self.tap_ys.lock().unwrap().push(el.bounds.y);
        self.tap_modes
            .lock()
            .unwrap()
            .push(velocity_common::actionability::mode());
        Ok(())
    }
    async fn drag(&self, _: &str, from: (i32, i32), to: (i32, i32), _: u64, _: u64) -> Result<()> {
        self.drags.lock().unwrap().push((from, to));
        Ok(())
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        Ok(())
    }
    async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
        Ok(())
    }
    async fn swipe_coords(&self, _: &str, a: (i32, i32), b: (i32, i32)) -> Result<()> {
        self.strokes.lock().unwrap().push((a, b));
        if self.pans {
            shift(&mut self.screen.lock().unwrap(), b.0 - a.0, b.1 - a.1);
        }
        Ok(())
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        let n = self.shots.fetch_add(1, Ordering::SeqCst);
        let frames = self.frames.lock().unwrap();
        if frames.is_empty() {
            return Ok(vec![]);
        }
        let i = if self.cycle_frames {
            n % frames.len()
        } else {
            n.min(frames.len() - 1)
        };
        Ok(frames[i].clone())
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((1080, 1920))
    }
    async fn app_liveness(&self, d: &str, a: &str) -> velocity_common::Liveness {
        self.liveness_reads.fetch_add(1, Ordering::SeqCst);
        let set = self.liveness.lock().unwrap().clone();
        match set {
            Some(l) => l,
            None => match self.crash_report(d, a).await {
                Some(detail) => velocity_common::Liveness::Dead(detail),
                None => velocity_common::Liveness::Unknown,
            },
        }
    }
    async fn system_bars(&self, _: &str) -> Option<Vec<Rect>> {
        self.bars.clone()
    }
    async fn freeze_status_bar(&self, _: &str) -> bool {
        self.freezes.fetch_add(1, Ordering::SeqCst);
        true
    }
    async fn get_element_text(&self, _: &str, el: &Element) -> Result<String> {
        Ok(el.text.clone().unwrap_or_default())
    }
    async fn element_text_matches(
        &self,
        d: &str,
        el: &Element,
        expected: &str,
    ) -> Result<(bool, String)> {
        let actual = self.get_element_text(d, el).await?;
        let hit = if self.fold_text {
            actual.to_lowercase() == expected.to_lowercase()
        } else {
            actual == expected
        };
        Ok((hit, actual))
    }
    async fn is_element_visible(&self, _: &str, el: &Element) -> Result<bool> {
        Ok(find_in_tree(
            &self.screen.lock().unwrap(),
            &Selector::Id(el.platform_id.clone()),
            None,
        )
        .is_some())
    }
    async fn reveal(&self, _: &str, el: &Element) -> Result<Element> {
        let mut el = el.clone();
        if !self.reveals {
            return Ok(el);
        }
        self.revealed.fetch_add(1, Ordering::SeqCst);
        if el.bounds.y < 0 || el.bounds.y + el.bounds.height > 1920 {
            let dy = 200 - el.bounds.y;
            shift(&mut self.screen.lock().unwrap(), 0, dy);
            el.bounds.y += dy;
        }
        Ok(el)
    }
}

fn config(timeout_ms: u64) -> SuiteConfig {
    let mut c = SuiteConfig {
        platform: Some(Platform::Android),
        ..Default::default()
    };
    c.sync.mode = SyncMode::None;
    c.sync.timeout_ms = timeout_ms;
    c.dialog.enabled = false;
    c.healing.enabled = false;
    c.artifacts.on_failure = false;
    c
}

fn step(action: Action) -> Step {
    Step {
        action,
        timeout_ms: None,
        settle: None,
        source: None,
        modifiers: Default::default(),
    }
}

fn test_case(steps: Vec<Step>) -> TestCase {
    TestCase {
        name: "t".into(),
        tags: vec![],
        isolated: false,
        depends_on: vec![],
        steps,
        description: None,
        verify: None,
    }
}

fn tap(text: &str) -> Action {
    Action::Tap {
        selector: Selector::Text(text.into()),
    }
}

/// iPhone 12, 2026-11-21: a Settings row under iOS 26's floating search
/// bar was refused ("covered by the toolbar; scroll it into view first")
/// and the step failed. Now its list is dragged up once and the tap runs
/// again; outside a scrolling list nothing is dragged.
#[tokio::test]
async fn a_tap_under_a_bottom_bar_scrolls_its_list_once_and_taps_again() {
    let row = node("general", "General", 786);
    let list = Element {
        element_type: "Table".into(),
        text: None,
        bounds: Rect {
            x: 0,
            y: 0,
            width: 390,
            height: 844,
        },
        children: vec![row.clone()],
        ..node("list", "", 0)
    };
    let driver = ScriptDriver::new(vec![list]);
    driver.barred_taps.store(1, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(1000), "app");
    let r = ex
        .execute_step(&step(tap("General")), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let drags = driver.drags.lock().unwrap().clone();
    assert_eq!(drags.len(), 1, "{drags:?}");
    let ((_, from_y), (_, to_y)) = drags[0];
    assert!(
        from_y < 786 && to_y < from_y,
        "drags up from above the bar: {drags:?}"
    );
    assert_eq!(*driver.taps.lock().unwrap(), ["General"]);

    // Not in a scrolling list: refused as before, nothing dragged.
    let driver = ScriptDriver::new(vec![node("general", "General", 786)]);
    driver.barred_taps.store(1, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(1000), "app");
    let r = ex
        .execute_step(&step(tap("General")), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(driver.drags.lock().unwrap().is_empty());
}

/// AUT-024: a launchApp's arguments reach the driver unchanged.
#[tokio::test]
async fn launch_app_passes_its_arguments_to_the_driver() {
    let driver = ScriptDriver::new(vec![]);
    let mut args = velocity_common::LaunchArgs::default();
    args.inner
        .insert("isE2E".into(), velocity_common::LaunchArg::Bool(true));
    args.inner.insert(
        "server".into(),
        velocity_common::LaunchArg::Str("https://staging.example.com".into()),
    );
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let launch = step(Action::LaunchApp {
        app_id: "app".into(),
        clear_state: false,
        arguments: args.clone(),
    });
    let r = ex.execute_step(&launch, "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let launches = driver.launches.lock().unwrap();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].3, args);
}

/// VEL-218: the default refusals named internal methods ("navigate()
/// not supported") and said "Android only" on Android.
#[tokio::test]
async fn default_refusals_name_the_step_not_the_method() {
    let d = ScriptDriver::new(vec![]);
    let msgs = [
        d.navigate("d", "x").await.unwrap_err().to_string(),
        d.go_back("d").await.unwrap_err().to_string(),
        d.refresh_page("d").await.unwrap_err().to_string(),
        d.system_state("d").await.unwrap_err().to_string(),
    ];
    for m in &msgs {
        assert!(!m.contains("()") && !m.contains("Android only"), "{m}");
    }
    assert!(msgs[0].contains("openLink"), "{}", msgs[0]);
    assert!(msgs[1].contains("goBack"), "{}", msgs[1]);
}

/// P4: the suite's `framework` reaches the sync probe's `config`.
#[test]
fn the_suite_framework_reaches_native_sync() {
    let driver = ScriptDriver::new(vec![]);
    let mut cfg = config(300);
    cfg.framework = Some(velocity_common::Framework::ReactNative);
    let ex = TestExecutor::new(&driver, cfg, "app");
    assert_eq!(
        ex.sync_manager.framework(),
        velocity_common::Framework::ReactNative
    );
    let ex = TestExecutor::new(&driver, config(300), "app");
    assert_eq!(
        ex.sync_manager.framework(),
        velocity_common::Framework::Native
    );
}

#[tokio::test]
async fn waits_for_an_element_that_renders_late_in_every_sync_mode() {
    for mode in [SyncMode::None, SyncMode::Polling] {
        let driver = ScriptDriver::new(vec![]).after(3, |s| s.children.push(node("b", "Buy", 0)));
        let mut cfg = config(3000);
        cfg.sync.mode = mode;
        cfg.sync.interval_ms = 10;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_step(&step(tap("Buy")), "d", "app", 0)
            .await
            .unwrap();
        assert_eq!(
            r.status,
            StepStatus::Passed,
            "{mode:?}: {:?}",
            r.error_message
        );
        assert_eq!(*driver.taps.lock().unwrap(), ["Buy"]);
    }
}

/// A screen no wait can read (headless with no `launchApp`, VEL-255)
/// failed each step only at its element timeout: 10.2 s a test. Now at
/// once, from the pre-action settle or the lookup; a not-found still
/// waits (the test above).
#[tokio::test]
async fn a_config_error_fails_the_step_at_once() {
    for mode in [SyncMode::None, SyncMode::Polling] {
        let gone = Action::AssertNotVisible {
            selector: Selector::Text("Buy".into()),
        };
        for action in [tap("Buy"), gone] {
            let driver = ScriptDriver::new(vec![]);
            driver.unlaunched.store(true, Ordering::SeqCst);
            let mut cfg = config(5000);
            cfg.sync.mode = mode;
            let mut ex = TestExecutor::new(&driver, cfg, "app");
            let started = std::time::Instant::now();
            let r = ex.execute_step(&step(action), "d", "app", 0).await.unwrap();
            assert_eq!(r.status, StepStatus::Failed, "{mode:?}");
            let msg = r.error_message.unwrap_or_default();
            assert!(msg.contains("launchApp"), "{msg}");
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{mode:?}: {:?}",
                started.elapsed()
            );
            // Polling's settle reads the screen first and stops there.
            let finds = driver.finds.load(Ordering::SeqCst);
            assert_eq!(finds, usize::from(mode == SyncMode::None), "{mode:?}");
        }
    }
}

#[tokio::test]
async fn a_failed_step_names_its_line_and_its_trace_keeps_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let mut driver = ScriptDriver::new(vec![node("cart", "Cart", 0), node("pay", "Pay now", 200)]);
    driver.cached = true;
    let mut cfg = config(300);
    cfg.artifacts.on_failure = true;
    cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let at = |line| Some(velocity_common::SourcePos::new("tests/app.yaml", line, 9));
    let steps = vec![
        Step {
            source: at(41),
            ..step(tap("Cart"))
        },
        Step {
            source: at(42),
            ..step(tap("Pay"))
        },
    ];
    let r = ex
        .execute_test(&test_case(steps), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.clone().unwrap();
    assert!(
        msg.starts_with("tests/app.yaml:42: tap { text: \"Pay\" }: Element not found"),
        "{msg}"
    );
    assert_eq!(r.steps[1].source.as_ref().unwrap().line, 42);

    let path = r.trace.expect("a failed test keeps its trace");
    let (trace, trees) = crate::trace::load_trace(&path).unwrap();
    assert_eq!(trace.steps.len(), 2);
    let cart = trace.steps[0].matched.as_ref().expect("matched element");
    assert_eq!(cart.text.as_deref(), Some("Cart"));
    assert!(!trace.steps[0].trees.is_empty(), "lookup tree kept");
    assert!(!trace.steps[1].trees.is_empty(), "failure tree kept");
    assert_eq!(trace.steps[1].nearest[0].text.as_deref(), Some("Pay now"));
    assert_eq!(trees.len(), 1, "one screen, one tree file");
    let d = crate::trace::describe_failure(&trace, &trees, 3).unwrap();
    assert_eq!(d.source.unwrap().line, 42);
    assert_eq!(d.closest[0].text.as_deref(), Some("Pay now"));
}

#[tokio::test]
async fn a_passing_test_keeps_no_trace_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let driver = ScriptDriver::new(vec![node("cart", "Cart", 0)]);
    let mut cfg = config(300);
    cfg.artifacts.on_failure = true;
    cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
    let mut ex = TestExecutor::new(&driver, cfg.clone(), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Cart"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert!(r.trace.is_none());
    assert!(!dir.path().join("traces").exists());

    let mut ex =
        TestExecutor::new(&driver, cfg, "app").with_trace(velocity_common::TraceMode::Always);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Cart"))]), "d", "app")
        .await
        .unwrap();
    assert!(r.trace.unwrap().exists());
}

#[tokio::test]
async fn step_timeout_overrides_sync_timeout() {
    let driver = ScriptDriver::new(vec![]);
    let mut ex = TestExecutor::new(&driver, config(60_000), "app");
    let start = Instant::now();
    let r = ex
        .execute_step(
            &Step {
                action: tap("Missing"),
                timeout_ms: Some(300),
                settle: None,
                source: None,
                modifiers: Default::default(),
            },
            "d",
            "app",
            0,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(r.error_message.unwrap().contains("after 300ms"));
}

#[tokio::test]
async fn assert_not_visible_waits_for_the_element_to_go() {
    let driver =
        ScriptDriver::new(vec![node("spin", "Loading", 0)]).after(3, |s| s.children.clear());
    let mut ex = TestExecutor::new(&driver, config(3000), "app");
    let r = ex
        .execute_step(
            &step(Action::AssertNotVisible {
                selector: Selector::Text("Loading".into()),
            }),
            "d",
            "app",
            0,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn assert_text_waits_for_the_text_to_settle() {
    let driver = ScriptDriver::new(vec![node("total", "$0.00", 0)])
        .after(3, |s| s.children[0].text = Some("$42.00".into()));
    let mut ex = TestExecutor::new(&driver, config(3000), "app");
    let r = ex
        .execute_step(
            &step(Action::AssertText {
                selector: Selector::Id("total".into()),
                expected: "$42.00".into(),
            }),
            "d",
            "app",
            0,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

/// B-1/M-1: assertText leaves the comparison to the driver, so a
/// driver that normalises its text (web) normalises `expected` alike.
#[tokio::test]
async fn assert_text_compares_through_the_driver() {
    let mut driver = ScriptDriver::new(vec![node("go", "SUBMIT", 0)]);
    driver.fold_text = true;
    let mut ex = TestExecutor::new(&driver, config(500), "app");
    let r = ex
        .execute_step(
            &step(Action::AssertText {
                selector: Selector::Id("go".into()),
                expected: "Submit".into(),
            }),
            "d",
            "app",
            0,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

/// Taps and screenshots never return (a wedged device).
struct HangingTap(ScriptDriver);

#[async_trait::async_trait]
impl PlatformDriver for HangingTap {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.0.list_devices().await
    }
    async fn boot_device(&self, d: &str) -> Result<()> {
        self.0.boot_device(d).await
    }
    async fn shutdown_device(&self, d: &str) -> Result<()> {
        self.0.shutdown_device(d).await
    }
    async fn install_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.install_app(d, a).await
    }
    async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
        self.0.launch_app(d, a, c).await
    }
    async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.stop_app(d, a).await
    }
    async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
        self.0.find_element(d, s).await
    }
    async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
        self.0.find_elements(d, s).await
    }
    async fn get_hierarchy(&self, d: &str) -> Result<Element> {
        self.0.get_hierarchy(d).await
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        std::future::pending().await
    }
    async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
        self.0.double_tap(d, e).await
    }
    async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
        self.0.long_press(d, e, ms).await
    }
    async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
        self.0.input_text(d, e, t).await
    }
    async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
        self.0.clear_text(d, e).await
    }
    async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
        self.0.swipe(d, dir).await
    }
    async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
        self.0.swipe_coords(d, f, t).await
    }
    async fn press_key(&self, d: &str, k: Key) -> Result<()> {
        self.0.press_key(d, k).await
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        std::future::pending().await
    }
    async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
        self.0.screen_size(d).await
    }
    async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
        self.0.get_element_text(d, e).await
    }
    async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
        self.0.is_element_visible(d, e).await
    }
}

/// The app dies on the tap: the device's settle never goes quiet (the
/// launcher's live wallpaper) and the crash log names the exception.
/// The second field is how long the device's settle takes to give up.
struct CrashingTap(ScriptDriver, Duration);

#[async_trait::async_trait]
impl PlatformDriver for CrashingTap {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.0.list_devices().await
    }
    async fn boot_device(&self, d: &str) -> Result<()> {
        self.0.boot_device(d).await
    }
    async fn shutdown_device(&self, d: &str) -> Result<()> {
        self.0.shutdown_device(d).await
    }
    async fn install_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.install_app(d, a).await
    }
    async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
        self.0.launch_app(d, a, c).await
    }
    async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.stop_app(d, a).await
    }
    async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
        self.0.find_element(d, s).await
    }
    async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
        self.0.find_elements(d, s).await
    }
    async fn get_hierarchy(&self, d: &str) -> Result<Element> {
        self.0.get_hierarchy(d).await
    }
    async fn tap(&self, d: &str, e: &Element) -> Result<()> {
        self.0.tap(d, e).await
    }
    async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
        self.0.double_tap(d, e).await
    }
    async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
        self.0.long_press(d, e, ms).await
    }
    async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
        self.0.input_text(d, e, t).await
    }
    async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
        self.0.clear_text(d, e).await
    }
    async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
        self.0.swipe(d, dir).await
    }
    async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
        self.0.swipe_coords(d, f, t).await
    }
    async fn press_key(&self, d: &str, k: Key) -> Result<()> {
        self.0.press_key(d, k).await
    }
    async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
        self.0.screenshot(d).await
    }
    async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
        self.0.screen_size(d).await
    }
    async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
        self.0.get_element_text(d, e).await
    }
    async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
        self.0.is_element_visible(d, e).await
    }
    async fn settle_on_device(
        &self,
        _: &str,
        _: u64,
        _: u64,
        _: u64,
    ) -> Option<velocity_common::DeviceSettle> {
        tokio::time::sleep(self.1).await;
        Some(velocity_common::DeviceSettle {
            settled: false,
            reacted: true,
            elapsed_ms: self.1.as_millis() as u64,
        })
    }
    async fn crash_report(&self, _: &str, _: &str) -> Option<String> {
        (!self.0.taps.lock().unwrap().is_empty())
            .then(|| "java.lang.IllegalStateException: boom".to_string())
    }
    async fn copy_crash_logs(
        &self,
        _: &str,
        app: &str,
        dest: &std::path::Path,
    ) -> Vec<std::path::PathBuf> {
        std::fs::create_dir_all(dest).unwrap();
        let f = dest.join(format!("{app}.ips"));
        std::fs::write(&f, "{}").unwrap();
        vec![f]
    }
}

/// A failed test keeps its video and logs next to its trace, says what
/// the logs saw, and links everything; a pass keeps nothing.
#[tokio::test]
async fn a_failure_keeps_video_logs_and_trace_html() {
    let mut driver = ScriptDriver::new(vec![node("b", "Pay", 0)]);
    driver.evidence = Some(vec![
        "crash (pid 7): java.lang.IllegalStateException: boom".into()
    ]);
    let out = tempfile::tempdir().unwrap();
    let mut c = config(300);
    c.artifacts.on_failure = true;
    c.artifacts.output_dir = out.path().to_string_lossy().into_owned();
    let mut ex =
        TestExecutor::new(&driver, c, "app").with_trace(velocity_common::TraceMode::OnFailure);
    let pass = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(pass.status, TestStatus::Passed);
    assert!(pass.attachments.is_empty(), "{:?}", pass.attachments);
    let fail = ex
        .execute_test(
            &test_case(vec![step(tap("Pay")), step(tap("Nowhere"))]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(fail.status, TestStatus::Failed);
    assert_eq!(
        *driver.evidence_finished.lock().unwrap(),
        vec![(false, false), (true, true)]
    );
    let kinds: Vec<_> = fail.attachments.iter().map(|a| a.kind).collect();
    use velocity_common::AttachmentKind as K;
    assert_eq!(
        kinds,
        vec![K::Video, K::AppLog, K::Trace],
        "{:?}",
        fail.attachments
    );
    for a in &fail.attachments {
        assert!(a.path.exists(), "{}", a.path.display());
    }
    let msg = fail.error_message.clone().unwrap();
    assert!(msg.contains("log: crash (pid 7)"), "{msg}");
    // The trace records step times and points at the evidence.
    let (trace, _) = crate::trace::load_trace(fail.trace.as_ref().unwrap()).unwrap();
    let ev = trace.evidence.unwrap();
    assert_eq!(ev.video.as_deref(), Some("video.mp4"));
    assert_eq!(ev.video_offset_ms, Some(-40));
    assert_eq!(ev.logs, vec!["app.log".to_string()]);
    assert_eq!(ev.log_lines.as_deref(), Some("logs.json"));
    assert!(trace.steps[1].start_ms >= trace.steps[0].start_ms);
    let html = std::fs::read_to_string(&fail.attachments[2].path).unwrap();
    assert!(html.contains("video.mp4") && html.contains("boom"));
}

#[test]
fn pause_answers_and_held_step_names() {
    assert_eq!(parse_pause_answer("\n"), PauseChoice::Continue);
    assert_eq!(parse_pause_answer("r\n"), PauseChoice::Retry);
    assert_eq!(parse_pause_answer(" Q "), PauseChoice::Abort);
    assert_eq!(parse_pause_answer("what"), PauseChoice::Continue);
    assert_eq!(
        held_step_index(std::path::Path::new("/x/step07_tap.png")),
        Some(7)
    );
    assert_eq!(held_step_index(std::path::Path::new("/x/shot.png")), None);
}

/// D-50: a crash's reports land in the test's artifacts.
#[tokio::test]
async fn a_crash_saves_the_crash_reports_with_the_artifacts() {
    let driver = CrashingTap(
        ScriptDriver::new(vec![node("b", "Crash", 0)]),
        Duration::ZERO,
    );
    let out = tempfile::tempdir().unwrap();
    let mut c = config(10_000);
    c.sync.mode = SyncMode::default();
    c.artifacts.on_failure = true;
    c.artifacts.output_dir = out.path().to_string_lossy().into_owned();
    let mut ex = TestExecutor::new(&driver, c, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Crash"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let saved: Vec<_> = walk(out.path());
    assert!(
        saved
            .iter()
            .any(|p| p.contains("crashlogs") && p.ends_with("app.ips")),
        "{saved:?}"
    );
    fn walk(d: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
        out
    }
}

#[tokio::test]
async fn a_crash_is_reported_at_the_step_that_caused_it() {
    let driver = CrashingTap(
        ScriptDriver::new(vec![node("b", "Crash", 0)]),
        Duration::ZERO,
    );
    let mut c = config(10_000);
    c.sync.mode = SyncMode::default();
    let mut ex = TestExecutor::new(&driver, c, "app");
    let start = Instant::now();
    let r = ex
        .execute_test(
            &test_case(vec![step(tap("Crash")), step(tap("Crash"))]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap();
    assert!(msg.contains("crashed") && msg.contains("boom"), "{msg}");
    assert_eq!(
        driver.0.taps.lock().unwrap().len(),
        1,
        "stops at the crashing step"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_crash_is_seen_before_the_settle_cap() {
    // The launcher's wallpaper keeps the settle busy for its full 5 s.
    let driver = CrashingTap(
        ScriptDriver::new(vec![node("b", "Crash", 0)]),
        Duration::from_secs(5),
    );
    let mut c = config(10_000);
    c.sync.mode = SyncMode::default();
    let mut ex = TestExecutor::new(&driver, c, "app");
    let start = Instant::now();
    let r = ex
        .execute_test(&test_case(vec![step(tap("Crash"))]), "d", "app")
        .await
        .unwrap();
    assert!(r.error_message.unwrap().contains("crashed"));
    let took = start.elapsed();
    assert!(took < Duration::from_millis(2500), "{took:?}");
}

#[tokio::test]
async fn a_hung_step_is_cut_off_by_the_watchdog() {
    let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
    let mut ex = TestExecutor::new(&driver, config(50), "app");
    let start = Instant::now();
    let r = ex
        .execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert!(r.error_message.unwrap().contains("Step timeout"));
    assert!(
        r.infrastructure_failure,
        "a hang on the first step is the device's fault"
    );
}

/// An API on 127.0.0.1 that answers every request with `body` (JSON).
fn json_api(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
        }
    });
    format!("http://{addr}")
}

fn http(yaml: &str) -> Action {
    Action::Http(Box::new(serde_yaml::from_str(yaml).unwrap()))
}

#[tokio::test]
async fn a_value_an_api_returns_reaches_a_ui_step() {
    // Seed through the API, then find the seeded thing on screen.
    let api = json_api(r#"{"id": "o-7"}"#);
    let driver = ScriptDriver::new(vec![node("row1", "o-6", 0), node("row2", "o-7", 100)]);
    let mut config = config(2000);
    config.captured_vars = ["ORDER_ID".to_string()].into();
    let mut ex = TestExecutor::new(&driver, config, "app");
    let seed = step(http(&format!(
        "{{ method: POST, url: \"{api}/orders\", capture: {{ ORDER_ID: id }} }}"
    )));
    let open = step(tap("${ORDER_ID}"));
    let r = ex
        .execute_test(&test_case(vec![seed, open.clone()]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(r.steps[0].action_name, format!("http(POST {api}/orders)"));
    assert_eq!(driver.taps.lock().unwrap().as_slice(), ["o-7"]);
    // The next test starts without it: no stale value from another test.
    let r = ex
        .execute_test(&test_case(vec![open]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let e = r.error_message.unwrap_or_default();
    assert!(e.contains("`${ORDER_ID}` has no value yet"), "{e}");
    assert_eq!(driver.taps.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_api_check_is_a_test_failure_an_optional_step_forgives() {
    let api = json_api(r#"{"ready": false}"#);
    let driver = ScriptDriver::new(vec![node("ok", "OK", 0)]);
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let check = format!("{{ url: \"{api}/status\", expect: {{ json: {{ ready: true }} }} }}");
    let r = ex
        .execute_test(&test_case(vec![step(http(&check))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let e = r.error_message.unwrap_or_default();
    assert!(e.contains("ready: expected true, got false"), "{e}");
    // Never put down to an app crash or the device.
    assert!(!e.contains("crashed"), "{e}");
    let mut optional = step(http(&check));
    optional.modifiers.optional = true;
    let r = ex
        .execute_test(&test_case(vec![optional, step(tap("OK"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_disabled_target_is_tapped_once_it_becomes_enabled() {
    let mut pay = node("pay", "Pay", 0);
    pay.enabled = false;
    // Enabled from the third lookup on (e.g. after a form validates).
    let driver = ScriptDriver::new(vec![pay]).after(3, |screen| {
        screen.children[0].enabled = true;
    });
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert!(driver.finds.load(Ordering::SeqCst) >= 3, "it waited");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[tokio::test]
async fn clear_text_waits_for_its_target_to_be_actionable() {
    let mut field = node("name", "Name", 0);
    field.enabled = false;
    let driver = ScriptDriver::new(vec![field]).after(3, |screen| {
        screen.children[0].enabled = true;
    });
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let clear = Action::ClearText {
        selector: Selector::Text("Name".into()),
    };
    let r = ex
        .execute_test(&test_case(vec![step(clear)]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert!(driver.finds.load(Ordering::SeqCst) >= 3, "it waited");
}

/// A node with no text whose label merges its parts (RN on iOS).
fn merged(id: &str, label: &str, y: i32) -> Element {
    Element {
        text: None,
        label: Some(label.into()),
        ..node(id, "", y)
    }
}

#[tokio::test]
async fn a_merged_label_part_is_not_the_element_for_a_negative_check() {
    // Review M2: "Invoice 3, Delete" read as a Delete button that never
    // went away.
    let driver = ScriptDriver::new(vec![merged("row", "Invoice 3, Delete", 100)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_step(&step(not_visible("Delete")), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn the_real_target_beats_a_merged_label_that_came_first() {
    // Review M3: the tab bar's "Home, tab, 1 of 4" is up before the new
    // screen's "Home" title renders; the first poll took the tab.
    let driver = ScriptDriver::new(vec![merged("tab", "Home, tab, 1 of 4", 1800)])
        .after(3, |screen| screen.children.push(node("title", "Home", 100)));
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let r = ex
        .execute_step(&step(tap("Home")), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*driver.tap_ys.lock().unwrap(), [100]);
}

#[tokio::test]
async fn a_merged_label_part_is_used_only_at_the_deadline_and_never_asserted() {
    // Review M3: the widest tier still finds a merged label, once the
    // stricter ones had the whole timeout.
    let driver = ScriptDriver::new(vec![merged("tab", "Home, tab, 1 of 4", 1800)]);
    let mut ex = TestExecutor::new(&driver, config(400), "app");
    let started = Instant::now();
    let visible = Action::AssertVisible {
        selector: Selector::Text("Home".into()),
    };
    let r = ex
        .execute_step(&step(visible), "d", "app", 0)
        .await
        .unwrap();
    // D-10: an assertion on a merged-label part fails, naming the label.
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("Home, tab, 1 of 4"), "{msg}");
    assert!(started.elapsed() >= Duration::from_millis(350));
    let r = ex
        .execute_step(&step(tap("Home")), "d", "app", 1)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*driver.tap_ys.lock().unwrap(), [1800]);
}

#[tokio::test]
async fn a_target_that_stays_disabled_fails_as_not_actionable() {
    // D-3: strict by default. The step fails, names the way out, and
    // nothing is tapped.
    let mut pay = node("pay", "Pay", 0);
    pay.enabled = false;
    let driver = ScriptDriver::new(vec![pay]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(driver.taps.lock().unwrap().is_empty());
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("not actionable"), "{err}");
    assert!(err.contains("was disabled"), "{err}");
    assert!(err.contains("force: true"), "{err}");
}

#[tokio::test]
async fn a_lenient_suite_taps_a_disabled_target_with_a_warning() {
    let mut pay = node("pay", "Pay", 0);
    pay.enabled = false;
    let driver = ScriptDriver::new(vec![pay]);
    let mut cfg = config(300);
    cfg.actionability = velocity_common::Actionability::Lenient;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert_eq!(driver.taps.lock().unwrap().len(), 1);
    assert!(
        r.warnings.iter().any(|w| w.contains("was disabled")),
        "{:?}",
        r.warnings
    );
}

/// Plain touches are refused as covered (Android's cover check); a forced
/// touch is counted and lands.
struct CoveredUnlessForced(ScriptDriver, std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl PlatformDriver for CoveredUnlessForced {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.0.list_devices().await
    }
    async fn boot_device(&self, d: &str) -> Result<()> {
        self.0.boot_device(d).await
    }
    async fn shutdown_device(&self, d: &str) -> Result<()> {
        self.0.shutdown_device(d).await
    }
    async fn install_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.install_app(d, a).await
    }
    async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
        self.0.launch_app(d, a, c).await
    }
    async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.stop_app(d, a).await
    }
    async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
        self.0.find_element(d, s).await
    }
    async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
        self.0.find_elements(d, s).await
    }
    async fn get_hierarchy(&self, d: &str) -> Result<Element> {
        self.0.get_hierarchy(d).await
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        Err(covered())
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        Err(covered())
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        Err(covered())
    }
    async fn touch_forced(&self, _: &str, _: &Element, _: velocity_common::Touch) -> Result<()> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
        self.0.input_text(d, e, t).await
    }
    async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
        self.0.clear_text(d, e).await
    }
    async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
        self.0.swipe(d, dir).await
    }
    async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
        self.0.swipe_coords(d, f, t).await
    }
    async fn press_key(&self, d: &str, k: Key) -> Result<()> {
        self.0.press_key(d, k).await
    }
    async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
        self.0.screenshot(d).await
    }
    async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
        self.0.screen_size(d).await
    }
    async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
        self.0.get_element_text(d, e).await
    }
    async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
        self.0.is_element_visible(d, e).await
    }
}

fn covered() -> VelocityError {
    VelocityError::AssertionFailed {
        expected: "target reachable".into(),
        actual: "covered by the system window (com.android.systemui)".into(),
        selector: String::new(),
        screenshot: None,
    }
}

/// `force: true` reached only the executor's actionability checks; the
/// Android driver's cover check still refused the touch.
#[tokio::test]
async fn force_reaches_the_driver_for_every_touch() {
    let driver = CoveredUnlessForced(
        ScriptDriver::new(vec![node("pay", "Pay", 0)]),
        std::sync::atomic::AtomicUsize::new(0),
    );
    let sel = || Selector::Text("Pay".into());
    let forced = |a: Action| {
        let mut s = step(a);
        s.modifiers.force = true;
        s
    };
    let steps = vec![
        forced(tap("Pay")),
        forced(Action::DoubleTap { selector: sel() }),
        forced(Action::LongPress {
            selector: sel(),
            duration_ms: Some(10),
        }),
    ];
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(steps), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.1.load(Ordering::SeqCst), 3);
    // Without force the same tap is refused.
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(driver.1.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn force_taps_a_disabled_target_at_once() {
    // A React Native view that reports enabled=false but works.
    let mut pay = node("pay", "Pay", 0);
    pay.enabled = false;
    let driver = ScriptDriver::new(vec![pay]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let mut forced = step(tap("Pay"));
    forced.modifiers.force = true;
    let r = ex
        .execute_test(&test_case(vec![forced]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().len(), 1);
    assert_eq!(
        driver.finds.load(Ordering::SeqCst),
        1,
        "no actionability reads"
    );
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
}

#[tokio::test]
async fn a_target_that_moves_between_reads_is_tapped_where_it_stops() {
    // VEL-072 / P13: the stable check runs on every action, not only
    // when the settle hit its cap. Found at y=0, then at y=100 for good.
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]).after(2, |screen| {
        shift(screen, 0, 100);
    });
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
}

/// A polling settle with short timings, for the read-count tests.
fn polling(timeout_ms: u64) -> SuiteConfig {
    let mut c = config(timeout_ms);
    c.sync.mode = SyncMode::Polling;
    c.sync.quiet_ms = Some(30);
    c.sync.reaction_ms = Some(30);
    c.sync.settle_cap_ms = Some(1000);
    c
}

#[tokio::test]
async fn a_settled_screen_needs_no_second_read_before_a_strict_tap() {
    // Review M4: the settle just saw two identical trees, so the strict
    // stable check reuses them instead of reading the tree again (a full
    // `/source`, ~540 ms, on iOS). Before: 2 lookups per tap; now 1.
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    let mut ex = TestExecutor::new(&driver, polling(2000), "app");
    let r = ex
        .execute_test(
            &test_case(vec![step(tap("Pay")), step(tap("Pay"))]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().len(), 2);
    assert_eq!(
        driver.finds.load(Ordering::SeqCst),
        2,
        "one lookup per tap, no re-read"
    );
}

/// Audit #11: the scroll loop settles after each swipe, and with no
/// swipe nothing moved, yet the step settled again afterwards: the
/// whole reaction floor's reads on an unchanged screen. Before, at the
/// default 300 ms floor: 15 dumps and 355 ms of settle with no swipe, 5
/// with one; now 2 (32 ms) and 3, and the next step's wait is one read.
#[tokio::test]
async fn scroll_until_visible_settles_once() {
    for (swipes, driver) in [
        (0, ScriptDriver::new(vec![node("row", "Row 1", 800)])),
        (
            1,
            ScriptDriver::new(vec![]).after(2, |s| s.children.push(node("row", "Row 1", 800))),
        ),
    ] {
        let mut cfg = polling(2000);
        cfg.sync.reaction_ms = None;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let scroll = Action::ScrollUntilVisible {
            selector: Selector::Text("Row 1".into()),
            direction: Direction::Down,
            max_scrolls: 5,
            within: None,
        };
        let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.finds.load(Ordering::SeqCst), swipes + 1);
        // The pre-action settle's two reads, plus one in-loop settle
        // read per swipe; nothing after the loop.
        let dumps = driver.dumps.load(Ordering::SeqCst);
        assert_eq!(dumps, 2 + swipes, "swipes={swipes}");
        assert!(r.settle_ms.unwrap_or(0) < 200, "{:?}", r.settle_ms);
        if swipes > 0 {
            // (This fake's row lands after the loop's settle, so the
            // next wait rightly reads it again.)
            continue;
        }
        let next = step(Action::AssertVisible {
            selector: Selector::Text("Row 1".into()),
        });
        let r = ex.execute_step(&next, "d", "app", 1).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.dumps.load(Ordering::SeqCst) - dumps, 1);
    }
}

#[tokio::test]
async fn a_settled_strict_tap_never_resolves_on_a_tree_cached_before_the_settle() {
    // review-fixes #1: `assertVisible: Pay` caches the full tree with
    // Pay mid-slide (y=0); the sheet then comes to rest at y=100. The
    // tap's settle compares live reads (never the lookup cache) and is
    // stable, so the lookup must not hit on the pre-settle cached tree
    // and skip the re-read. Before: tapped at y=0.
    let mut driver =
        ScriptDriver::new(vec![node("pay", "Pay", 0)]).after(1, |screen| shift(screen, 0, 100));
    driver.lookup_cache = Some(Mutex::new(None));
    let mut ex = TestExecutor::new(&driver, polling(2000), "app");
    let seen = step(Action::AssertVisible {
        selector: Selector::Text("Pay".into()),
    });
    let r = ex
        .execute_test(&test_case(vec![seen, step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
}

#[tokio::test]
async fn without_a_settle_a_strict_tap_still_reads_twice() {
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    let mut ex = TestExecutor::new(&driver, config(2000), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.finds.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_lookup_that_had_to_wait_is_read_again_after_a_settle() {
    // The target arrived after the settle (the first lookup missed):
    // the settle's verdict says nothing about it, so it is re-read, and
    // tapped where it stops.
    let driver = ScriptDriver::new(vec![node("other", "Other", 500)])
        .after(2, |screen| screen.children.push(node("pay", "Pay", 0)))
        .after(3, |screen| shift(screen, 0, 100));
    let mut ex = TestExecutor::new(&driver, polling(2000), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*driver.tap_ys.lock().unwrap(), vec![100]);
}

#[tokio::test]
async fn web_leaves_the_stable_check_to_the_page() {
    // Review M4: `injected.js` `act` waits two frames for a still box;
    // the executor doesn't read the tree a second time on top.
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    let mut cfg = config(2000);
    cfg.platform = Some(Platform::Web);
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.finds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_one_pixel_wobble_is_still() {
    // Review m1: a spring's fractional frames round either way.
    let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    for n in 2..400 {
        let dy = if n % 2 == 0 { 1 } else { -1 };
        driver = driver.after(n, move |screen| shift(screen, 0, dy));
    }
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_target_that_never_stops_moving_fails_naming_the_animation() {
    // A marquee: its box changes between every two reads. Strict fails,
    // says it was moving (not "reports enabled=false"), and names
    // `force: true`.
    let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    for n in 2..400 {
        driver = driver.after(n, |screen| shift(screen, 10, 0));
    }
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(driver.taps.lock().unwrap().is_empty());
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("still moving"), "{err}");
    assert!(err.contains("continuously animating"), "{err}");
    assert!(err.contains("force: true"), "{err}");
    assert!(!err.contains("enabled=false"), "{err}");
}

#[tokio::test]
async fn the_actionability_mode_reaches_the_driver() {
    use velocity_common::actionability::ActMode;
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    let mut forced = step(tap("Pay"));
    forced.modifiers.force = true;
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay")), forced]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    let mut cfg = config(300);
    cfg.actionability = velocity_common::Actionability::Lenient;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    ex.execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(
        *driver.tap_modes.lock().unwrap(),
        [ActMode::Strict, ActMode::Force, ActMode::Lenient]
    );
}

#[test]
fn lookup_retries_start_fast_then_back_off() {
    let delays: Vec<u64> = (0..7).map(|n| retry_delay(n).as_millis() as u64).collect();
    assert_eq!(delays, vec![20, 50, 100, 100, 250, 500, 500]);
}

#[test]
fn selector_fields_round_trip_through_the_parser() {
    // VEL-242: a combining accent or a control character made invalid YAML.
    for text in [
        "Cafe\u{301}",
        "a\u{1}b",
        "say \"hi\"\n\tthere",
        "日本 \\ x",
        "del\u{7f}",
        "c1\u{85}\u{9f}",
        "ls\u{2028}ps\u{2029}",
        "nc\u{fffe}",
        "bom\u{feff}",
        "\u{1b}[31mred",
    ] {
        let sel = Selector::Compound(vec![
            Selector::Text(text.into()),
            Selector::ClassName("Button".into()),
        ]);
        let yaml = format!(
            "appId: a\ntests:\n  - name: t\n    steps:\n      - tap: {{ {} }}\n",
            selector_fields(&sel)
        );
        let suite = crate::parser::parse_suite_from_str(&yaml)
            .unwrap_or_else(|e| panic!("{text:?}: {e}\n{yaml}"));
        match &suite.tests[0].steps[0].action {
            Action::Tap { selector } => {
                assert_eq!(selector.to_string(), sel.to_string(), "{yaml}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn retries_use_the_whole_timeout() {
    // P7: a 300 ms timeout stopped once the next step (250 ms) would
    // overshoot, and never slept the remainder.
    let deadline = Instant::now() + Duration::from_millis(30);
    let t = Duration::from_millis(1000);
    assert!(next_pause(6, deadline, t).unwrap() <= Duration::from_millis(30));
    assert!(next_pause(6, Instant::now() + Duration::from_secs(9), t).unwrap() <= t / 5);
    assert!(next_pause(0, Instant::now() - Duration::from_millis(1), t).is_none());
}

#[tokio::test]
async fn an_element_that_appears_just_before_the_deadline_is_found() {
    // P7: with a 1 s timeout the old loop looked at 0, 20, 70, 170, 270
    // and 520 ms, then stopped (the next 500 ms step overshot). The
    // element shows at 700 ms; now there are checks at 670, 870, 1000.
    let driver = ScriptDriver::new(vec![]);
    let mut ex = TestExecutor::new(&driver, config(1000), "app");
    let screen = &driver.screen;
    let appear = async {
        tokio::time::sleep(Duration::from_millis(700)).await;
        screen
            .lock()
            .unwrap()
            .children
            .push(node("late", "Late", 0));
    };
    let s = step(tap("Late"));
    let (r, _) = tokio::join!(ex.execute_step(&s, "d", "app", 0), appear);
    let r = r.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_healed_target_is_waited_for_until_enabled() {
    // N-3: the actionability wait re-queried the selector that had just
    // failed, so a healed, briefly disabled button was "gone".
    let mut btn = node("submit", "Submit orders", 0);
    btn.enabled = false;
    let driver = ScriptDriver::new(vec![btn]).after(12, |s| s.children[0].enabled = true);
    let mut cfg = config(1500);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Auto);
    cfg.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Submit orders"]);
    assert_eq!(r.healed_count(), 1, "{:?}", r.warnings);
}

#[tokio::test]
async fn report_mode_fails_a_healed_step_with_the_patch_and_the_command() {
    // D-7: in CI (the default there) a heal is proposed, not used.
    let driver = ScriptDriver::new(vec![node("submit", "Submit orders", 0)]);
    let mut cfg = config(300);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Report);
    cfg.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let mut t = step(tap("Submit order"));
    t.source = Some(velocity_common::SourcePos::new("tests/shop.yaml", 7, 9));
    let r = ex
        .execute_test(&test_case(vec![t]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        driver.taps.lock().unwrap().is_empty(),
        "the heal was not used"
    );
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("healing.mode is report"), "{err}");
    assert!(
        err.contains(r#"text: "Submit order" → text: "Submit orders""#),
        "{err}"
    );
    assert!(
        err.contains("velocity heal --accept --config tests/shop.yaml"),
        "{err}"
    );
    // Kept for `velocity heal --accept` although the test failed.
    let kept = ex.selector_engine.healer().healed_mappings();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].test.as_deref(), Some("t"));
    // `off`: no heal at all.
    let mut cfg = config(300);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Off);
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
        .await
        .unwrap();
    let err = r.error_message.unwrap_or_default();
    assert!(!err.contains("heal"), "{err}");
    assert!(ex.selector_engine.healer().healed_mappings().is_empty());
}

#[tokio::test]
async fn report_mode_uses_a_target_that_arrives_during_the_last_look() {
    // Review M1: the healing lookup looks the selector up first; a
    // target that rendered just then is the real one, not a heal. It
    // failed as "the heal was not used" with another test's patch.
    let driver = ScriptDriver::new(vec![node("other", "Something else", 500)]);
    *driver.late.lock().unwrap() = Some(node("submit", "Submit order", 0));
    let mut cfg = config(300);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Report);
    cfg.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Submit order"]);
}

#[tokio::test]
async fn report_mode_proposes_this_steps_heal_not_the_last_one() {
    // Review M1: the patch came from `healed_mappings().pop()`, the last
    // heal of any step or test.
    let driver = ScriptDriver::new(vec![
        node("submit", "Submit orders", 0),
        node("cancel", "Cancel orders", 300),
    ]);
    let mut cfg = config(300);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Report);
    cfg.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    // A heal from an earlier step, with an unrelated mapping last.
    let r = ex
        .execute_test(&test_case(vec![step(tap("Cancel order"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        r.healed_count() == 0,
        "a heal that wasn't used is not counted: {:?}",
        r.warnings
    );
    let r = ex
        .execute_test(&test_case(vec![step(tap("Submit order"))]), "d", "app")
        .await
        .unwrap();
    let err = r.error_message.unwrap_or_default();
    assert!(
        err.contains(r#"text: "Submit order" → text: "Submit orders""#),
        "{err}"
    );
    assert!(!err.contains("Cancel"), "{err}");
    // m6: the reject command names this heal alone.
    assert!(
        err.contains(r#"velocity heal --reject 'text="Submit order"'"#),
        "{err}"
    );
}

/// Review NIT 12: a selector holding a masked value can't be rejected
/// by its (masked) text; the hint names the number instead, and never
/// the value.
#[test]
fn a_reject_hint_never_names_a_masked_selector() {
    let plain = reject_hint(r#"text="Submit order""#, "s.yaml");
    assert!(
        plain.contains(r#"velocity heal --reject 'text="Submit order"' --config s.yaml"#),
        "{plain}"
    );
    let quoted = reject_hint("text=\"it's\"", "s.yaml");
    assert!(quoted.contains(r#"'text="it'\''s"'"#), "{quoted}");
    velocity_common::redact::register_secret("nit12-Zq9xV7pL3kW");
    let masked = reject_hint(r#"text="Card nit12-Zq9xV7pL3kW""#, "s.yaml");
    assert!(!masked.contains("nit12-Zq9xV7pL3kW"), "{masked}");
    assert!(masked.contains("--reject <N>"), "{masked}");
}

fn shot(name: &str) -> Step {
    Step {
        timeout_ms: Some(600),
        ..step(Action::AssertScreenshot {
            name: name.into(),
            threshold: None,
            cluster_px: None,
            mask: vec![],
            tolerance: None,
            comparator: None,
            crop: None,
        })
    }
}

/// An executor whose suite lives in `dir` (baselines under it).
fn in_suite(dir: &std::path::Path) -> SuiteConfig {
    let mut c = config(100);
    c.suite_dir = Some(dir.to_path_buf());
    c.artifacts.output_dir = dir.join("out").to_string_lossy().into_owned();
    c
}

fn put(path: &std::path::Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

#[tokio::test]
async fn screenshot_names_that_share_a_baseline_file_are_an_error() {
    // VEL-201: "a.b" and "a b" both read velocity-baselines/<p>/a_b.png.
    // New names get their own files; two names that fall back to one
    // legacy file are still an error.
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[]);
    put(
        &dir.path().join("velocity-baselines/android/vel201_a_b.png"),
        &screen,
    );
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex
        .execute_step(&shot("vel201 a.b"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let r = ex
        .execute_step(&shot("vel201 a b"), "d", "app", 1)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("both map to baseline"), "{msg}");
}

#[test]
fn clean_names_keep_their_file_and_others_get_a_hash() {
    // VT-2 / VEL-201.
    assert_eq!(baseline_file("login"), "login.png");
    assert_eq!(baseline_file("home-2_dark"), "home-2_dark.png");
    let (a, b) = (baseline_file("cart total"), baseline_file("cart/total"));
    assert_ne!(a, b);
    assert!(a.starts_with("cart_total-") && a.ends_with(".png"), "{a}");
    // Non-ASCII letters no longer reach the file system as they are.
    assert!(baseline_file("café").is_ascii());
    assert!(baseline_file(&"x".repeat(300)).len() < 120);
}

#[tokio::test]
async fn names_that_differ_only_in_case_are_an_error() {
    // Review m7: "Home" and "home" share one file on a case-insensitive
    // file system (default macOS, Windows).
    // "Home" has a baseline, so the test doesn't depend on whether
    // `CI` is set (a missing one fails there: review-s8 MAJOR-2).
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[]);
    put(
        &dir.path().join("velocity-baselines/android/Home.png"),
        &screen,
    );
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("Home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let r = ex.execute_step(&shot("home"), "d", "app", 1).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(r.error_message.unwrap_or_default().contains("both map"));
}

#[test]
fn strokes_survive_hostile_bounds() {
    // Review m6: line_across and aim_at still added in i32.
    let huge = Rect {
        x: i32::MAX - 10,
        y: i32::MAX - 10,
        width: i32::MAX,
        height: i32::MAX,
    };
    for d in [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ] {
        let _ = line_across(&huge, d);
        let _ = aim_at(&huge, &huge, d, true);
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 1920,
        };
        let _ = aim_at(&screen, &huge, d, false);
    }
}

#[tokio::test]
async fn a_first_frame_that_matches_passes_with_one_capture() {
    // VT-5 / B5: two captures and a 250 ms pause at least, every time.
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    put(
        &dir.path().join("velocity-baselines/android/home.png"),
        &screen,
    );
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.shots.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_blinking_caret_is_still_under_the_steps_tolerance() {
    // VT-5 / B5: a 1 px speck flipping between frames used to keep the
    // zero-diff gate from ever seeing a still screen.
    let dir = tempfile::tempdir().unwrap();
    let driver = ScriptDriver {
        cycle_frames: true,
        ..ScriptDriver::new(vec![])
    };
    *driver.frames.lock().unwrap() = vec![
        crate::visual::painted(40, 40, &[]),
        crate::visual::painted(40, 40, &[(20, 20, 1, 1)]),
    ];
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
    let r = ex
        .execute_step(&shot("caret"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.shots.load(Ordering::SeqCst), 2);
    assert!(ex.warnings.iter().all(|w| !w.contains("held still")));
    assert!(dir
        .path()
        .join("velocity-baselines/android/40x40/caret.png")
        .is_file());
}

#[tokio::test]
async fn an_unstable_screen_fails_and_never_becomes_the_baseline() {
    // VT-5 / B6: the frame after the timeout was written as the
    // baseline with only a warning.
    let dir = tempfile::tempdir().unwrap();
    let driver = ScriptDriver {
        cycle_frames: true,
        ..ScriptDriver::new(vec![])
    };
    let (a, b) = (
        crate::visual::painted(40, 40, &[]),
        crate::visual::painted(40, 40, &[(10, 0, 2, 40)]),
    );
    *driver.frames.lock().unwrap() = vec![a.clone(), b.clone()];
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
    let r = ex.execute_step(&shot("spin"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(
        msg.contains("never held still") && msg.contains("not saved"),
        "{msg}"
    );
    assert!(!dir
        .path()
        .join("velocity-baselines/android/40x40/spin.png")
        .exists());
    for kind in ["previous", "actual", "diff"] {
        assert!(
            dir.path()
                .join(format!("out/diffs/spin-{kind}.png"))
                .is_file(),
            "{kind}"
        );
    }
    // Without --update-baselines and no baseline: no baseline either.
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("spin"), "d", "app", 1).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(!dir
        .path()
        .join("velocity-baselines/android/40x40/spin.png")
        .exists());
    // With a baseline equal to one of the frames: passes.
    put(&dir.path().join("velocity-baselines/android/spin.png"), &b);
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("spin"), "d", "app", 2).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_corrupt_capture_is_taken_again_and_never_a_config_error() {
    // VT-4 / VEL-203: "invalid PNG" was a Config error (exit 2).
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[]);
    put(
        &dir.path().join("velocity-baselines/android/c.png"),
        &screen,
    );
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![vec![0x89, b'P', b'N', b'G'], screen];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("c"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    // Garbage every time: a device failure, not configuration.
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![vec![1, 2, 3]];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let plain = ScreenshotStep {
        threshold: None,
        cluster_px: None,
        mask: &[],
        rule: crate::visual::PixelRule::default(),
        crop: None,
    };
    let e = ex.assert_screenshot("d", "c", plain).await.unwrap_err();
    assert_ne!(e.kind(), velocity_common::ErrorKind::ConfigError, "{e}");
    assert_eq!(e.exit_code(), 3, "{e}");
    assert_eq!(driver.shots.load(Ordering::SeqCst), 3);
    // The run's outcome too: an infrastructure failure, so the suite
    // exits 3, not 1 (review-s8 m-c).
    assert!(is_infrastructure(&e), "{e}");
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![vec![1, 2, 3]];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex
        .execute_test(&test_case(vec![shot("c")]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(r.infrastructure_failure, "{:?}", r.error_message);
}

#[tokio::test]
async fn huge_masks_and_unreadable_baselines_say_so_at_once() {
    // Review-s8 m-f: masks over most of the screen failed every
    // frame-to-frame comparison, so the step waited for a still screen
    // and advised masking more; a corrupt baseline read "invalid PNG".
    let dir = tempfile::tempdir().unwrap();
    let mut big = node("big", "list", 0);
    big.bounds.width = 5_000;
    big.bounds.height = 9_000;
    let mut driver = ScriptDriver::new(vec![big]);
    driver.bars = Some(vec![]);
    *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 40, &[])];
    let masked = |name: &str| Step {
        timeout_ms: Some(3_000),
        ..step(Action::AssertScreenshot {
            name: name.into(),
            threshold: None,
            cluster_px: None,
            mask: vec![Selector::Id("big".into())],
            tolerance: None,
            comparator: None,
            crop: None,
        })
    };
    for update in [false, true] {
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(update);
        let start = Instant::now();
        let r = ex.execute_step(&masked("m"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed);
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.contains("masks cover") && !msg.contains("held still"),
            "{msg}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    put(
        &dir.path().join("velocity-baselines/android/bad.png"),
        b"not a png",
    );
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("bad"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(
        msg.contains("bad.png") && msg.contains("re-record"),
        "{msg}"
    );
}

#[tokio::test]
async fn assert_text_reads_a_merged_label_it_found_loosely() {
    // Review-s8 m-b: the lookup found "Points" as part of a merged
    // label at its deadline, then the read loop (Fold) never found it,
    // waited a second timeout and failed with actual "".
    let mut row = node("row", "Points\n120", 0);
    row.element_type = "View".into();
    let driver = ScriptDriver::new(vec![row]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let start = Instant::now();
    let r = ex
        .execute_step(
            &step(Action::AssertText {
                selector: Selector::Text("Points".into()),
                expected: "Points\n120".into(),
            }),
            "d",
            "app",
            0,
        )
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(start.elapsed() < Duration::from_millis(1_500));
}

#[tokio::test]
async fn every_match_of_a_mask_is_masked() {
    // VT-3 / VEL-223, B4: only the first row's timestamp was masked.
    let mut driver = ScriptDriver::new(vec![node("ts", "10:01", 300), node("ts", "10:02", 900)]);
    driver.bars = Some(vec![]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let masks = ex
        .screenshot_masks("d", &[], &[Selector::Id("ts".into())])
        .await
        .unwrap();
    assert_eq!(regions(&masks), [(0, 300, 100, 40), (0, 900, 100, 40)]);
}

#[test]
fn headless_runs_keep_their_own_baselines_next_to_the_suite() {
    // B1: headless and device Android runs shared android/<name>.png.
    // B2: the folder came from the process's working directory.
    // D-V3: and each capture size has its own folder.
    let dir = tempfile::tempdir().unwrap();
    let driver = ScriptDriver::new(vec![]);
    let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let size = (1080, 1920);
    let device = ex.baseline_place("home", size, None);
    assert_eq!(
        device.write,
        dir.path()
            .join("velocity-baselines/android/1080x1920/home.png")
    );
    let mut c = in_suite(dir.path());
    c.driver = Some(velocity_common::DriverMode::Headless);
    let ex = TestExecutor::new(&driver, c, "app");
    let headless = ex.baseline_place("home", size, None);
    assert_eq!(
        headless.write,
        dir.path()
            .join("velocity-baselines/android-headless/1080x1920/home.png")
    );
    assert!(headless.read.is_none());
    // An existing shared (pre-D-V3) baseline is still read, with a hint.
    let legacy = dir.path().join("velocity-baselines/android/home.png");
    put(&legacy, b"x");
    let p = ex.baseline_place("home", size, None);
    assert_eq!(p.read.as_deref(), Some(legacy.as_path()));
    assert!(p.legacy);
    // A baseline in the folder the run started from is found too.
    let name = format!("vel-b2-{}", std::process::id());
    let cwd = PathBuf::from("velocity-baselines/android").join(format!("{name}.png"));
    put(&cwd, b"x");
    let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let p = ex.baseline_place(&name, size, None);
    let _ = std::fs::remove_file(&cwd);
    let _ = std::fs::remove_dir("velocity-baselines/android");
    let _ = std::fs::remove_dir("velocity-baselines");
    assert_eq!(p.read, Some(cwd));
    assert!(p.legacy);
    assert_eq!(
        p.write,
        dir.path()
            .join(format!("velocity-baselines/android/1080x1920/{name}.png"))
    );
}

/// review-d5 m-3 / review-fixes #3: a capture at a size that has no
/// baseline yet while another size has one (a rotation, another
/// device) fails a plain run without writing, naming
/// `--update-baselines=missing`; `=missing` records the new size and
/// leaves the other size alone.
#[tokio::test]
async fn a_baseline_only_at_another_size_is_missing_for_this_size() {
    use velocity_common::BaselineUpdate;
    let dir = tempfile::tempdir().unwrap();
    let portrait = crate::visual::painted(40, 60, &[(5, 5, 10, 10)]);
    let landscape = crate::visual::painted(60, 40, &[(5, 5, 10, 10)]);
    let upright = dir.path().join("velocity-baselines/android/40x60/home.png");
    put(&upright, &portrait);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![landscape.clone()];
    let rotated = dir.path().join("velocity-baselines/android/60x40/home.png");
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .with_update_baselines(BaselineUpdate::Off);
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(
        msg.contains("size mismatch: baseline is 40x60, capture is 60x40")
            && msg.contains("--update-baselines=missing"),
        "{msg}"
    );
    assert!(!rotated.exists(), "a plain run wrote the rotated screen");
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .with_update_baselines(BaselineUpdate::Missing);
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(std::fs::read(&rotated).unwrap(), landscape);
    assert_eq!(std::fs::read(&upright).unwrap(), portrait, "untouched");
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .with_update_baselines(BaselineUpdate::Changed);
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

/// review-fixes #3: a file in the capture's own size folder with another
/// pixel size is a real size mismatch, and `=missing` doesn't hide it.
#[tokio::test]
async fn a_wrong_sized_file_in_the_same_size_folder_is_a_size_mismatch() {
    use velocity_common::BaselineUpdate;
    let dir = tempfile::tempdir().unwrap();
    let small = crate::visual::painted(20, 20, &[(5, 5, 4, 4)]);
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    let file = dir.path().join("velocity-baselines/android/40x40/home.png");
    put(&file, &small);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    for mode in [BaselineUpdate::Off, BaselineUpdate::Missing] {
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(mode);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Failed, "{mode:?}");
        let msg = r.error_message.unwrap_or_default();
        assert!(
            msg.contains("size mismatch") && msg.contains("is 20x20, capture is 40x40"),
            "{mode:?}: {msg}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), small, "{mode:?}");
    }
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .with_update_baselines(BaselineUpdate::Changed);
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(std::fs::read(&file).unwrap(), screen);
}

/// review-fixes #3: a first recording on two screen sizes in one
/// parallel run with `=missing`: both devices record their own size and
/// pass (before, the second one saw the first's fresh file and failed
/// with "size mismatch" on every run).
#[tokio::test]
async fn two_sizes_record_their_first_baselines_in_one_parallel_run() {
    use velocity_common::BaselineUpdate;
    let dir = tempfile::tempdir().unwrap();
    let phone = crate::visual::painted(40, 60, &[(5, 5, 10, 10)]);
    let tablet = crate::visual::painted(60, 80, &[(5, 5, 10, 10)]);
    let (da, db) = (ScriptDriver::new(vec![]), ScriptDriver::new(vec![]));
    *da.frames.lock().unwrap() = vec![phone.clone()];
    *db.frames.lock().unwrap() = vec![tablet.clone()];
    let run = velocity_common::CreatedBaselines::default();
    let device = |drv| {
        TestExecutor::new(drv, in_suite(dir.path()), "app")
            .with_created_baselines(run.clone())
            .with_update_baselines(BaselineUpdate::Missing)
    };
    let (mut a, mut b) = (device(&da), device(&db));
    let shot_a = shot("home");
    let shot_b = shot("home");
    let (ra, rb) = tokio::join!(
        a.execute_step(&shot_a, "a", "app", 0),
        b.execute_step(&shot_b, "b", "app", 0)
    );
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_eq!(ra.status, StepStatus::Passed, "{:?}", ra.error_message);
    assert_eq!(rb.status, StepStatus::Passed, "{:?}", rb.error_message);
    // Sequentially too (device a strictly first), which is the order
    // that failed before.
    let base = dir.path().join("velocity-baselines/android");
    assert_eq!(std::fs::read(base.join("40x60/home.png")).unwrap(), phone);
    assert_eq!(std::fs::read(base.join("60x80/home.png")).unwrap(), tablet);
    std::fs::remove_dir_all(&base).unwrap();
    let (mut a, mut b) = (device(&da), device(&db));
    let ra = a.execute_step(&shot_a, "a", "app", 0).await.unwrap();
    let rb = b.execute_step(&shot_b, "b", "app", 0).await.unwrap();
    assert_eq!(ra.status, StepStatus::Passed, "{:?}", ra.error_message);
    assert_eq!(rb.status, StepStatus::Passed, "{:?}", rb.error_message);
    // The next plain run passes on both.
    for (drv, id) in [(&da, "a"), (&db, "b")] {
        let mut ex = TestExecutor::new(drv, in_suite(dir.path()), "app");
        let r = ex.execute_step(&shot("home"), id, "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }
}

#[tokio::test]
async fn baselines_live_under_the_capture_size_and_legacy_ones_warn() {
    // D-V3: <platform>/<W>x<H>/<name>.png; the old flat file is read
    // (with a warning) only when it is the captured size.
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
    let legacy = dir.path().join("velocity-baselines/android/size.png");
    put(&legacy, &screen);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("size"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(
        ex.warnings
            .iter()
            .any(|w| w.contains("older layout") && w.contains("40x30")),
        "{:?}",
        ex.warnings
    );
    // Re-recording writes only the new layout.
    let other = crate::visual::painted(40, 30, &[(20, 5, 10, 10)]);
    *driver.frames.lock().unwrap() = vec![other.clone()];
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
    let r = ex.execute_step(&shot("size"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let sized = dir.path().join("velocity-baselines/android/40x30/size.png");
    assert_eq!(std::fs::read(&sized).unwrap(), other);
    assert_eq!(std::fs::read(&legacy).unwrap(), screen, "legacy untouched");
    // Another device size never reads that legacy file.
    let small = crate::visual::painted(20, 20, &[]);
    let ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let p = ex.baseline_place("size", (20, 20), crate::visual::png_size(&small));
    assert!(p.read.is_none(), "{:?}", p.read);
}

/// U1: a mismatch leaves a review record naming the baseline at this
/// capture size, so `velocity baselines accept` (and the inspector's
/// review page) can replace it; accepting makes the next run pass.
#[tokio::test]
async fn a_mismatch_is_recorded_for_review_and_accepting_it_passes() {
    let dir = tempfile::tempdir().unwrap();
    let old = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
    let baseline = dir.path().join("velocity-baselines/android/40x30/home.png");
    put(&baseline, &old);
    let new = crate::visual::painted(40, 30, &[(20, 5, 15, 15)]);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![new.clone()];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);

    let out = dir.path().join("out");
    let list = crate::baselines::pending(&out);
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].id, "diffs/home");
    assert_eq!(list[0].name, "home");
    assert_eq!(
        list[0].baseline.canonicalize().unwrap(),
        baseline.canonicalize().unwrap()
    );
    assert!(list[0].detail.contains("similar"), "{}", list[0].detail);
    crate::baselines::accept(&list[0]).unwrap();
    assert_eq!(std::fs::read(&baseline).unwrap(), new);
    assert!(crate::baselines::pending(&out).is_empty());

    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

/// fi56 MAJOR 1: a record from a failed run outlived a later pass (or
/// `--update-baselines`), and `baselines accept all` then wrote that
/// run's old frame over the good baseline.
#[tokio::test]
async fn a_later_pass_or_update_clears_the_review_record() {
    let dir = tempfile::tempdir().unwrap();
    let good = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
    let broken = crate::visual::painted(40, 30, &[(20, 5, 15, 15)]);
    let baseline = dir.path().join("velocity-baselines/android/40x30/home.png");
    put(&baseline, &good);
    let out = dir.path().join("out");
    let driver = ScriptDriver::new(vec![]);
    let run = |frame: &Vec<u8>, update: bool| {
        *driver.frames.lock().unwrap() = vec![frame.clone()];
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(update)
    };

    // Monday: it fails and is recorded.
    let r = run(&broken, false)
        .execute_step(&shot("home"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert_eq!(crate::baselines::pending(&out).len(), 1);
    // Tuesday: the app is fixed and the run passes.
    let r = run(&good, false)
        .execute_step(&shot("home"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let list = crate::baselines::pending(&out);
    assert!(
        crate::baselines::select(&list, "all").is_empty(),
        "{list:?}"
    );
    assert_eq!(std::fs::read(&baseline).unwrap(), good);

    // The same with --update-baselines instead of a pass.
    let r = run(&broken, false)
        .execute_step(&shot("home"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert_eq!(crate::baselines::pending(&out).len(), 1);
    let reviewed = crate::visual::painted(40, 30, &[(1, 1, 4, 4)]);
    let r = run(&reviewed, true)
        .execute_step(&shot("home"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(crate::baselines::pending(&out).is_empty());
    assert_eq!(std::fs::read(&baseline).unwrap(), reviewed);
}

/// review-fi56 m1: records are named by file only, so a pass of
/// `home` on another screen size (same output dir) deleted this size's
/// fresh failure before anyone reviewed it.
#[tokio::test]
async fn a_pass_on_another_size_keeps_this_sizes_review_record() {
    let dir = tempfile::tempdir().unwrap();
    let phone = crate::visual::painted(40, 30, &[(5, 5, 10, 10)]);
    let tablet = crate::visual::painted(60, 80, &[(5, 5, 10, 10)]);
    put(
        &dir.path().join("velocity-baselines/android/40x30/home.png"),
        &phone,
    );
    put(
        &dir.path().join("velocity-baselines/android/60x80/home.png"),
        &tablet,
    );
    let out = dir.path().join("out");
    let driver = ScriptDriver::new(vec![]);
    // The phone fails...
    *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 30, &[(20, 5, 15, 15)])];
    let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .execute_step(&shot("home"), "phone", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    // ...then the tablet passes the same assertScreenshot.
    *driver.frames.lock().unwrap() = vec![tablet.clone()];
    let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .execute_step(&shot("home"), "tablet", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let list = crate::baselines::pending(&out);
    assert_eq!(list.len(), 1, "the phone's failure is still up for review");
    assert!(
        list[0].baseline.ends_with("40x30/home.png"),
        "{:?}",
        list[0].baseline
    );
    // The phone passing clears it.
    *driver.frames.lock().unwrap() = vec![phone.clone()];
    let r = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .execute_step(&shot("home"), "phone", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(crate::baselines::pending(&out).is_empty());
}

/// fi56 (review MINOR): an unstable screen cleared the current-layout
/// record while the failure had been recorded under the legacy file
/// name it read, so the old frame stayed acceptable.
#[tokio::test]
async fn an_unstable_screen_clears_the_legacy_named_record() {
    let dir = tempfile::tempdir().unwrap();
    let name = "cart total";
    let legacy = legacy_baseline_file(name);
    assert_ne!(legacy, baseline_file(name));
    let old = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    put(
        &dir.path()
            .join("velocity-baselines/android/40x40")
            .join(&legacy),
        &old,
    );
    let out = dir.path().join("out");
    let driver = ScriptDriver {
        cycle_frames: true,
        ..ScriptDriver::new(vec![])
    };
    *driver.frames.lock().unwrap() = vec![crate::visual::painted(40, 40, &[(20, 5, 15, 15)])];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot(name), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert_eq!(crate::baselines::pending(&out).len(), 1);

    *driver.frames.lock().unwrap() = vec![
        crate::visual::painted(40, 40, &[]),
        crate::visual::painted(40, 40, &[(10, 0, 2, 40)]),
    ];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot(name), "d", "app", 0).await.unwrap();
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("never held still"), "{msg}");
    let list = crate::baselines::pending(&out);
    assert!(list.is_empty(), "{list:?}");
}

#[tokio::test]
async fn a_missing_baseline_is_created_and_fails_everywhere() {
    // D-V1: it was created and the step passed unless a CI variable was
    // set; an unknown CI passed silently (VEL-214).
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("created") && msg.contains("commit"), "{msg}");
    let created = dir.path().join("velocity-baselines/android/40x40/new.png");
    assert_eq!(std::fs::read(&created).unwrap(), screen);
    // A retry in the same run doesn't pass on the unreviewed file.
    let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed, "retry flipped to a pass");
    // The next run compares with it.
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("new"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

/// review-d5 M-1: two runs in one process (`velocity daemon`, MCP):
/// the second passes once the baseline exists, in every mode that
/// compares; the registry no longer outlives the run.
#[tokio::test]
async fn a_created_baseline_passes_on_the_next_run_in_the_same_process() {
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    // Run 1 creates it and fails.
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    drop(ex);
    // Run 2 (plain) and run 3 (=missing) pass on it.
    for mode in [
        velocity_common::BaselineUpdate::Off,
        velocity_common::BaselineUpdate::Missing,
    ] {
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(mode);
        let r = ex.execute_step(&shot("home"), "d", "app", 0).await.unwrap();
        assert_eq!(
            r.status,
            StepStatus::Passed,
            "{mode:?}: {:?}",
            r.error_message
        );
    }
}

/// review-d5 M-1: one run's parallel devices share the registry: a
/// device that finds the baseline another device of this run just
/// created fails too; the next run's devices pass.
#[tokio::test]
async fn parallel_devices_share_one_runs_created_baselines() {
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    let run = velocity_common::CreatedBaselines::default();
    let device = |run: &velocity_common::CreatedBaselines| {
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_created_baselines(run.clone())
    };
    let (mut a, mut b) = (device(&run), device(&run));
    let r = a.execute_step(&shot("home"), "a", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let r = b.execute_step(&shot("home"), "b", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed, "device b passed on it");
    assert_eq!(run.len(), 1);
    let next = velocity_common::CreatedBaselines::default();
    let (mut a, mut b) = (device(&next), device(&next));
    for (ex, id) in [(&mut a, "a"), (&mut b, "b")] {
        let r = ex.execute_step(&shot("home"), id, "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    }
}

#[tokio::test]
async fn update_baselines_missing_writes_only_missing_ones_and_passes() {
    // D-V1: `--update-baselines=missing`.
    let dir = tempfile::tempdir().unwrap();
    let screen = crate::visual::painted(40, 40, &[(5, 5, 10, 10)]);
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![screen.clone()];
    let missing = velocity_common::BaselineUpdate::Missing;
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(missing);
    let r = ex
        .execute_step(&shot("fresh"), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    let fresh = dir
        .path()
        .join("velocity-baselines/android/40x40/fresh.png");
    assert_eq!(std::fs::read(&fresh).unwrap(), screen);
    assert!(ex.warnings.iter().any(|w| w.contains("created baseline")));
    // An existing baseline that differs is still compared, and kept.
    let old = crate::visual::painted(40, 40, &[(25, 25, 10, 10)]);
    let kept = dir.path().join("velocity-baselines/android/40x40/kept.png");
    put(&kept, &old);
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(missing);
    let r = ex.execute_step(&shot("kept"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert_eq!(std::fs::read(&kept).unwrap(), old);
}

fn shot_with(name: &str, f: impl FnOnce(&mut Action)) -> Step {
    let mut s = shot(name);
    f(&mut s.action);
    s
}

#[tokio::test]
async fn tolerance_and_the_perceptual_comparator_are_per_step() {
    // D-V2: a uniform grey +20 drift (another host's shading) fails
    // the default rule; `tolerance: 24` or `comparator: perceptual`
    // accept it.
    let dir = tempfile::tempdir().unwrap();
    let grey = |v: u8| {
        let mut png = Vec::new();
        let mut e = png::Encoder::new(&mut png, 20, 20);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().unwrap();
        w.write_image_data(&[v, v, v, 255].repeat(400)).unwrap();
        w.finish().unwrap();
        png
    };
    put(
        &dir.path()
            .join("velocity-baselines/android/20x20/drift.png"),
        &grey(100),
    );
    let driver = ScriptDriver::new(vec![]);
    *driver.frames.lock().unwrap() = vec![grey(120)];
    let run = |step: Step| {
        let driver = &driver;
        let dir = dir.path().to_path_buf();
        async move {
            let mut ex = TestExecutor::new(driver, in_suite(&dir), "app");
            ex.execute_step(&step, "d", "app", 0).await.unwrap().status
        }
    };
    assert_eq!(run(shot("drift")).await, StepStatus::Failed);
    let tol = shot_with("drift", |a| {
        if let Action::AssertScreenshot { tolerance, .. } = a {
            *tolerance = Some(24);
        }
    });
    assert_eq!(run(tol).await, StepStatus::Passed);
    let perceptual = shot_with("drift", |a| {
        if let Action::AssertScreenshot { comparator, .. } = a {
            *comparator = Some(velocity_common::Comparator::Perceptual);
        }
    });
    assert_eq!(run(perceptual).await, StepStatus::Passed);
}

#[tokio::test]
async fn crop_compares_only_the_cropped_part() {
    // VT-12 (maskLimit decision): two screens that differ outside the
    // card pass; inside, they fail. The 50% mask limit still holds
    // inside the crop.
    let dir = tempfile::tempdir().unwrap();
    let mut card = node("card", "Card", 0);
    card.bounds = Rect {
        x: 0,
        y: 0,
        width: 540,
        height: 960,
    };
    let mut driver = ScriptDriver::new(vec![card]);
    driver.bars = Some(vec![]);
    // 108x192 frames: a tenth of the 1080x1920 screen.
    let base = crate::visual::painted(108, 192, &[(10, 10, 10, 10)]);
    let outside = crate::visual::painted(108, 192, &[(10, 10, 10, 10), (70, 120, 20, 20)]);
    let inside = crate::visual::painted(108, 192, &[(10, 10, 10, 10), (20, 40, 20, 20)]);
    for (crop, label) in [
        (
            velocity_common::ScreenshotCrop::Element(Selector::Id("card".into())),
            "element",
        ),
        (
            velocity_common::ScreenshotCrop::Rect(Rect {
                x: 0,
                y: 0,
                width: 540,
                height: 960,
            }),
            "rect",
        ),
    ] {
        let name = format!("card-{label}");
        let step = shot_with(&name, |a| {
            if let Action::AssertScreenshot { crop: c, .. } = a {
                *c = Some(crop.clone());
            }
        });
        *driver.frames.lock().unwrap() = vec![base.clone()];
        let mut ex =
            TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
        let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
        let stored = std::fs::read(
            dir.path()
                .join(format!("velocity-baselines/android/108x192/{name}.png")),
        )
        .unwrap();
        assert_eq!(crate::visual::png_size(&stored), Some((54, 96)), "{label}");
        for (frame, want) in [
            (&outside, StepStatus::Passed),
            (&inside, StepStatus::Failed),
        ] {
            *driver.frames.lock().unwrap() = vec![frame.clone()];
            let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
            let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
            assert_eq!(r.status, want, "{label}: {:?}", r.error_message);
        }
    }
    // A mask over most of the crop is still refused.
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let step = shot_with("card-element", |a| {
        if let Action::AssertScreenshot { crop, mask, .. } = a {
            *crop = Some(velocity_common::ScreenshotCrop::Element(Selector::Id(
                "card".into(),
            )));
            *mask = vec![Selector::Id("card".into())];
        }
    });
    let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(r.error_message.unwrap_or_default().contains("masks cover"));
}

/// review-d5 m-2: a `crop:` selector that matches several elements
/// warns, as an action does, and fails under `selectors.strict`.
#[tokio::test]
async fn an_ambiguous_crop_selector_warns_or_fails_when_strict() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = node("price", "Price", 0);
    a.bounds = Rect {
        x: 0,
        y: 0,
        width: 540,
        height: 400,
    };
    let mut b = a.clone();
    b.bounds.y = 900;
    let mut driver = ScriptDriver::new(vec![a, b]);
    driver.bars = Some(vec![]);
    *driver.frames.lock().unwrap() = vec![crate::visual::painted(108, 192, &[])];
    let step = shot_with("price", |act| {
        if let Action::AssertScreenshot { crop, .. } = act {
            *crop = Some(velocity_common::ScreenshotCrop::Element(Selector::Text(
                "Price".into(),
            )));
        }
    });
    let mut ex =
        TestExecutor::new(&driver, in_suite(dir.path()), "app").with_update_baselines(true);
    let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(
        ex.warnings.iter().any(|w| w.contains("matched 2")),
        "{:?}",
        ex.warnings
    );
    // Review NIT 14: an ambiguous crop uses the match an action would
    // (the first in tree order), and the warning says so.
    assert!(
        ex.warnings.iter().any(|w| w.contains(&format!(
            "used {}",
            velocity_common::selector_match::BARE_PICK_RULE
        ))),
        "{:?}",
        ex.warnings
    );
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app")
        .with_update_baselines(true)
        .with_strict_selectors(true);
    let r = ex.execute_step(&step, "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("2 match"), "{msg}");
}

#[tokio::test]
async fn a_frozen_status_bar_is_compared_instead_of_masked() {
    // D-V4: `screenshots.freezeStatusBar` asks the driver once per
    // device; where it took, the bar is no longer masked.
    let dir = tempfile::tempdir().unwrap();
    let mut driver = ScriptDriver::new(vec![]);
    driver.bars = Some(vec![Rect {
        x: 0,
        y: 0,
        width: 1080,
        height: 200,
    }]);
    let base = crate::visual::painted(108, 192, &[]);
    // A change inside the status bar (y < 20 px).
    let clock = crate::visual::painted(108, 192, &[(40, 5, 20, 10)]);
    put(
        &dir.path()
            .join("velocity-baselines/android/108x192/bar.png"),
        &base,
    );
    *driver.frames.lock().unwrap() = vec![clock];
    let mut ex = TestExecutor::new(&driver, in_suite(dir.path()), "app");
    let r = ex.execute_step(&shot("bar"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "masked by default");
    assert_eq!(driver.freezes.load(Ordering::SeqCst), 0);
    let mut c = in_suite(dir.path());
    c.screenshots.freeze_status_bar = true;
    let mut ex = TestExecutor::new(&driver, c, "app");
    let r = ex.execute_step(&shot("bar"), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed, "the bar is compared");
    let _ = ex.execute_step(&shot("bar"), "d", "app", 1).await.unwrap();
    assert_eq!(driver.freezes.load(Ordering::SeqCst), 1, "asked once");
}

#[tokio::test]
async fn a_refused_tap_is_not_reported_as_acted_on() {
    // VEL-274: "acted on it anyway" and then "Element not found".
    let mut off = node("pay", "Pay", 0);
    off.enabled = false;
    let mut driver = ScriptDriver::new(vec![off]);
    driver.refuse_disabled = true;
    let mut ex = TestExecutor::new(&driver, config(200), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("not actionable"), "{msg}");
    assert!(
        !r.warnings.iter().any(|w| w.contains("acted on it anyway")),
        "{:?}",
        r.warnings
    );
}

#[tokio::test]
async fn tapping_one_of_several_matches_is_reported() {
    let mut driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
    driver.cached = true;
    let mut ex = TestExecutor::new(&driver, config(500), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert!(
        r.warnings.iter().any(|w| w.contains("matched 2 elements")),
        "{:?}",
        r.warnings
    );
}

/// A stand-in resolver for the executor's error mapping: the model API is
/// down (`outage`), or it taps the element called "Buy" and finds nothing
/// for a check. The real resolver runs through the executor in
/// `velocity-runner/tests/executor_nl.rs`.
struct FakeResolver {
    outage: bool,
}

#[async_trait::async_trait]
impl NaturalStepResolver for FakeResolver {
    async fn cached_hit(&self, _: &NaturalStep<'_>) -> Option<velocity_common::natural::CachedHit> {
        None
    }
    async fn plan_on_screen(
        &self,
        step: &NaturalStep<'_>,
        _: &Element,
        _: Option<&Rect>,
        _: Option<&str>,
    ) -> Result<Action> {
        if self.outage {
            return Err(VelocityError::ServiceUnavailable {
                service: "Anthropic API".into(),
                detail: "HTTP 529".into(),
            });
        }
        match step.kind {
            NaturalKind::Act => Ok(tap("Buy")),
            NaturalKind::Assert => Err(VelocityError::ElementNotFound {
                selector: step.intent.into(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            }),
        }
    }
    fn prepare(
        &self,
        _: &NaturalStep<'_>,
        _: Action,
        _: Option<usize>,
        _: Option<bool>,
    ) -> Option<PendingPlan> {
        None
    }
    async fn commit_pending(&self, _: Vec<PendingPlan>) {}
    async fn invalidate(&self, _: &NaturalStep<'_>) {}
    async fn check(
        &self,
        at: &str,
        claim: &str,
        _: &Element,
        _: Option<&Rect>,
        _: Platform,
        _: Option<&[u8]>,
    ) -> velocity_common::VerifyCheck {
        velocity_common::VerifyCheck {
            at: at.into(),
            claim: claim.into(),
            outcome: velocity_common::VerifyOutcome::Error,
            reason: "fake".into(),
            evidence: vec![],
        }
    }
}

#[tokio::test]
async fn natural_step_errors_keep_their_type() {
    // NB-1: an API outage became a Config error (permanent, exit 2),
    // and an unmet waitUntil a configuration problem (VEL-222).
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let nl = Arc::new(FakeResolver { outage: true });
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl);
    let (r, _) = ex
        .execute_natural(
            ("tap buy", None),
            NaturalKind::Act,
            (None, None),
            None,
            "d",
            "app",
        )
        .await;
    let e = r.unwrap_err();
    assert!(
        matches!(e, VelocityError::ServiceUnavailable { .. }) && e.is_transient(),
        "{e:?}"
    );

    let mut ex = TestExecutor::new(&driver, config(300), "app")
        .with_planner(Arc::new(FakeResolver { outage: false }));
    let (r, _) = ex
        .execute_natural(
            ("tap buy", None),
            NaturalKind::Act,
            (None, None),
            Some("the receipt is shown"),
            "d",
            "app",
        )
        .await;
    let e = r.unwrap_err();
    assert!(matches!(e, VelocityError::AssertionFailed { .. }), "{e:?}");
    assert!(e.to_string().contains("receipt"), "{e}");
}

#[test]
fn a_natural_step_that_finds_nothing_stays_not_found() {
    let act = Action::Tap {
        selector: Selector::Text("Pay".into()),
    };
    let e = annotate(
        &act,
        VelocityError::ElementNotFound {
            selector: "text=\"Pay\"".into(),
            timeout_ms: 300,
            screenshot: None,
            hierarchy_snapshot: None,
        },
        Some("re-plan failed: x"),
    );
    assert!(is_not_found(&e), "{e:?}");
    assert!(e.to_string().contains("natural step resolved to"), "{e}");
    assert!(e.to_string().contains("re-plan failed: x"), "{e}");
}

#[test]
fn a_model_outage_during_a_re_plan_stays_an_outage() {
    // Review M5: a cached step's element vanished, the re-plan hit a
    // 529, and the step ended as ElementNotFound without the 529.
    let act = Action::Tap {
        selector: Selector::Text("Pay".into()),
    };
    let gone = || VelocityError::ElementNotFound {
        selector: "text=\"Pay\"".into(),
        timeout_ms: 300,
        screenshot: None,
        hierarchy_snapshot: None,
    };
    let outage = VelocityError::ServiceUnavailable {
        service: "model API".into(),
        detail: "HTTP 529 overloaded".into(),
    };
    let e = replan_failed(&act, gone(), outage);
    assert!(
        matches!(e, VelocityError::ServiceUnavailable { .. }),
        "{e:?}"
    );
    let msg = e.to_string();
    assert!(
        msg.contains("529") && msg.contains("re-plan failed"),
        "{msg}"
    );
    assert_ne!(e.exit_code(), 1);
    // A plan that simply didn't work: the first failure, with the note.
    let e = replan_failed(&act, gone(), VelocityError::Config("no plan".into()));
    assert!(is_not_found(&e), "{e:?}");
    assert!(e.to_string().contains("re-plan failed: "), "{e}");
}

#[tokio::test]
async fn batching_skips_a_plain_relaunch_after_a_passing_test() {
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let mut cfg = config(500);
    cfg.batch = true;
    let launch = || {
        step(Action::LaunchApp {
            app_id: String::new(),
            clear_state: false,
            arguments: Default::default(),
        })
    };
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let first = ex
        .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
        .await
        .unwrap();
    assert!(!first.steps[0].action_name.contains("batched"));
    let second = ex
        .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
        .await
        .unwrap();
    assert!(
        second.steps[0].action_name.contains("batched"),
        "{:?}",
        second.steps[0]
    );
    // A clearState launch always runs.
    let third = ex
        .execute_test(
            &test_case(vec![step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: true,
                arguments: Default::default(),
            })]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert!(!third.steps[0].action_name.contains("batched"));
}

fn not_visible(text: &str) -> Action {
    Action::AssertNotVisible {
        selector: Selector::Text(text.into()),
    }
}

#[tokio::test]
async fn assert_not_visible_fails_when_the_screen_cannot_be_read() {
    // VEL-012: every read fails while the spinner is on screen.
    let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
    driver.unreadable.store(usize::MAX, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(not_visible("Loading"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap();
    assert!(msg.contains("null root node"), "{msg}");
}

#[tokio::test]
async fn assert_not_visible_reads_again_after_a_failed_read() {
    // One failed read is not "gone"; the next good read decides.
    let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
    driver.unreadable.store(1, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(400), "app");
    let r = ex
        .execute_test(&test_case(vec![step(not_visible("Loading"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed, "still on screen");
    assert!(r.error_message.unwrap().contains("not visible"));

    let driver = ScriptDriver::new(vec![node("s", "Loading", 0)]);
    driver.unreadable.store(1, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(400), "app");
    let r = ex
        .execute_test(&test_case(vec![step(not_visible("Gone"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_crash_does_not_pass_a_negative_assertion_or_the_last_step() {
    // VEL-014: with no settle the post-action crash watch never runs.
    for steps in [
        vec![step(tap("Pay")), step(not_visible("Payment failed"))],
        vec![step(tap("Pay"))],
    ] {
        let driver = CrashingTap(ScriptDriver::new(vec![node("p", "Pay", 0)]), Duration::ZERO);
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(steps), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap();
        assert!(msg.contains("crashed") && msg.contains("boom"), "{msg}");
        assert_eq!(r.steps.last().unwrap().status, StepStatus::Failed);
    }
    // A test that stops the app on purpose is not a crash.
    let driver = CrashingTap(ScriptDriver::new(vec![node("p", "Pay", 0)]), Duration::ZERO);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(Action::StopApp {
                    app_id: String::new(),
                }),
                step(not_visible("Payment failed")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

fn visible(text: &str) -> Action {
    Action::AssertVisible {
        selector: Selector::Text(text.into()),
    }
}

/// I3 (D-50 sticky crash): a crash the driver saw on its own (a
/// physical iPhone's crash watcher) fails every later step at once with
/// the crash: no tap lands on the home screen, no element wait runs to
/// its timeout.
#[tokio::test]
async fn a_crash_the_driver_saw_fails_every_later_step_at_once() {
    let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
    *driver.pending.lock().unwrap() = Some("SIGABRT in PaymentSheet".into());
    let mut ex = TestExecutor::new(&driver, config(10_000), "app");
    let start = Instant::now();
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(tap("Pay")),
                step(visible("Receipt")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.clone().unwrap();
    assert!(msg.contains("crashed") && msg.contains("SIGABRT"), "{msg}");
    assert_eq!(r.steps[1].status, StepStatus::Failed, "{:?}", r.steps);
    assert_eq!(r.steps[2].status, StepStatus::Skipped);
    assert_eq!(
        driver.taps.lock().unwrap().len(),
        1,
        "nothing tapped after the crash"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );

    // The crash shows up while a wait runs: the wait ends at once.
    let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
    *driver.pending.lock().unwrap() = Some("EXC_BAD_ACCESS".into());
    driver.pending_after.store(3, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(10_000), "app");
    let start = Instant::now();
    let r = ex
        .execute_test(
            &test_case(vec![step(tap("Pay")), step(visible("Receipt"))]),
            "d",
            "app",
        )
        .await
        .unwrap();
    let msg = r.error_message.clone().unwrap();
    assert!(
        msg.contains("crashed") && msg.contains("EXC_BAD_ACCESS"),
        "{msg}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

/// I3: a relaunch clears the sticky crash, and so does stopping the
/// app on purpose; launchApp itself is never refused.
#[tokio::test]
async fn a_relaunch_or_a_stop_ends_the_sticky_crash() {
    let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
    *driver.pending.lock().unwrap() = Some("boom".into());
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(Action::LaunchApp {
                    app_id: String::new(),
                    clear_state: false,
                    arguments: Default::default(),
                }),
                step(tap("Pay")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().len(), 2);

    let driver = ScriptDriver::new(vec![node("p", "Pay", 0)]);
    *driver.pending.lock().unwrap() = Some("boom".into());
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(Action::StopApp {
                    app_id: String::new(),
                }),
                step(not_visible("Pay sheet")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

/// iOS-like: save/restore terminate the app without clearing the
/// driver's launch record, so `crash_report` sees a launched app with
/// no process and calls it a crash (unless the executor knows better).
struct StateKeeping(ScriptDriver, std::sync::atomic::AtomicBool);

#[async_trait::async_trait]
impl PlatformDriver for StateKeeping {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.0.list_devices().await
    }
    async fn boot_device(&self, d: &str) -> Result<()> {
        self.0.boot_device(d).await
    }
    async fn shutdown_device(&self, d: &str) -> Result<()> {
        self.0.shutdown_device(d).await
    }
    async fn install_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.install_app(d, a).await
    }
    async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
        self.1.store(true, Ordering::SeqCst);
        self.0.launch_app(d, a, c).await
    }
    async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
        self.0.stop_app(d, a).await
    }
    async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
        self.0.find_element(d, s).await
    }
    async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
        self.0.find_elements(d, s).await
    }
    async fn get_hierarchy(&self, d: &str) -> Result<Element> {
        self.0.get_hierarchy(d).await
    }
    async fn tap(&self, d: &str, e: &Element) -> Result<()> {
        self.0.tap(d, e).await
    }
    async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
        self.0.double_tap(d, e).await
    }
    async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
        self.0.long_press(d, e, ms).await
    }
    async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
        self.0.input_text(d, e, t).await
    }
    async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
        self.0.clear_text(d, e).await
    }
    async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
        self.0.swipe(d, dir).await
    }
    async fn swipe_coords(&self, d: &str, f: (i32, i32), t: (i32, i32)) -> Result<()> {
        self.0.swipe_coords(d, f, t).await
    }
    async fn press_key(&self, d: &str, k: Key) -> Result<()> {
        self.0.press_key(d, k).await
    }
    async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
        self.0.screenshot(d).await
    }
    async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
        self.0.screen_size(d).await
    }
    async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
        self.0.get_element_text(d, e).await
    }
    async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
        self.0.is_element_visible(d, e).await
    }
    async fn save_app_state(&self, _: &str, _: &str, _: &str) -> Result<()> {
        self.1.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn restore_app_state(&self, _: &str, _: &str, _: &str) -> Result<()> {
        self.1.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn crash_report(&self, _: &str, _: &str) -> Option<String> {
        (!self.1.load(Ordering::SeqCst))
            .then(|| "the app's process exited unexpectedly".to_string())
    }
    async fn pending_crash(&self, d: &str, a: &str) -> Option<String> {
        self.0.pending_crash(d, a).await
    }
}

/// SUNMI tablet, 2026-11-21: a configuration error (launch arguments a
/// driver doesn't take) was reported as "App ... crashed", because the
/// crash lookup found an older crash. A configuration error stays one.
#[tokio::test]
async fn a_configuration_error_is_never_reported_as_a_crash() {
    let driver = StateKeeping(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        std::sync::atomic::AtomicBool::new(false),
    );
    let mut args = velocity_common::LaunchArgs::default();
    args.inner
        .insert("isE2E".into(), velocity_common::LaunchArg::Bool(true));
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![step(Action::LaunchApp {
                app_id: String::new(),
                clear_state: false,
                arguments: args,
            })]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(msg.contains("not supported"), "{msg}");
    assert!(!msg.contains("crashed"), "{msg}");
}

/// fi56 (review MINOR): after a crash, `restoreState` (which touches
/// no UI and stops the app on purpose) failed at once with the crash,
/// so a restore-and-relaunch recovery couldn't run.
#[tokio::test]
async fn restore_state_runs_after_a_crash() {
    let driver = StateKeeping(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        std::sync::atomic::AtomicBool::new(true),
    );
    *driver.0.pending.lock().unwrap() = Some("boom".into());
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(Action::RestoreState {
                    name: "loggedIn".into(),
                }),
                step(Action::LaunchApp {
                    app_id: String::new(),
                    clear_state: false,
                    arguments: Default::default(),
                }),
                step(tap("Pay")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.0.taps.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn save_and_restore_state_are_not_a_crash() {
    // B-1: saveState/restoreState leave the app stopped on purpose.
    let restore = Action::RestoreState {
        name: "loggedIn".into(),
    };
    for steps in [
        vec![
            step(tap("Pay")),
            step(Action::SaveState {
                name: "loggedIn".into(),
            }),
        ],
        vec![step(restore.clone()), step(not_visible("Payment failed"))],
        vec![step(restore)],
    ] {
        let driver = StateKeeping(
            ScriptDriver::new(vec![node("p", "Pay", 0)]),
            std::sync::atomic::AtomicBool::new(true),
        );
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(steps), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }
}

#[tokio::test]
async fn a_tap_after_an_assertion_uses_the_moved_bounds() {
    // VEL-340: assertVisible cached Coffee; a promo row then pushed it down.
    let driver = ScriptDriver::new(vec![node("coffee", "Coffee", 0)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let seen = Action::AssertVisible {
        selector: Selector::Text("Coffee".into()),
    };
    let r = ex.execute_step(&step(seen), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed);
    driver.screen.lock().unwrap().children =
        vec![node("promo", "Promo", 0), node("coffee", "Coffee", 50)];
    let r = ex
        .execute_step(&step(tap("Coffee")), "d", "app", 1)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Passed);
    assert_eq!(
        *driver.tap_ys.lock().unwrap(),
        [50],
        "tapped where Coffee is now"
    );
}

#[tokio::test]
async fn a_system_dialog_the_step_answers_is_left_to_the_step() {
    // VEL-011: `tap: Don't Allow` on a permission prompt; the handler
    // used to tap "Allow" first.
    let mut alert = node("", "", 300);
    alert.text = None;
    alert.label = Some("\u{201c}Shop\u{201d} Would Like to Use Your Location".into());
    alert.element_type = "XCUIElementTypeAlert".into();
    alert.bounds.height = 200;
    alert.children = vec![
        node("deny", "Don't Allow", 320),
        node("allow", "Allow", 380),
    ];
    let driver = ScriptDriver::new(vec![alert]);
    let mut cfg = config(300);
    cfg.dialog = Default::default();
    cfg.dialog.max_dismissals = 1;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Don't Allow"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    // (The fake keeps the alert on screen, so the post-action pass may
    // answer it again; what matters is the step's own tap came first.)
    assert_eq!(driver.taps.lock().unwrap()[0], "Don't Allow");
}

#[test]
fn only_device_screens_get_a_status_bar_mask() {
    // VEL-013: the top 4% of a web page or headless render was never compared.
    let mut c = config(100);
    assert!(has_status_bar(&c));
    c.platform = Some(Platform::Ios);
    assert!(has_status_bar(&c));
    c.driver = Some(velocity_common::DriverMode::Headless);
    assert!(!has_status_bar(&c));
    c.driver = None;
    c.platform = Some(Platform::Web);
    assert!(!has_status_bar(&c));
}

#[tokio::test]
async fn scroll_until_visible_looks_after_its_last_scroll() {
    // VEL-056: the row the only allowed scroll revealed was never seen.
    let driver = ScriptDriver::new(vec![]).after(2, |s| s.children.push(node("row", "Row 9", 0)));
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let scroll = Action::ScrollUntilVisible {
        selector: Selector::Text("Row 9".into()),
        direction: Direction::Down,
        max_scrolls: 1,
        within: None,
    };
    let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn scroll_until_visible_stops_aiming_when_nothing_moves() {
    // VEL-068: a visible row in the top 12% band of a screen that
    // can't scroll was dragged at on every one of the scrolls.
    let driver = ScriptDriver::new(vec![node("top", "Header", 20)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let scroll = Action::ScrollUntilVisible {
        selector: Selector::Text("Header".into()),
        direction: Direction::Down,
        max_scrolls: 5,
        within: None,
    };
    let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.strokes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn scroll_until_visible_brings_a_container_below_the_fold_in() {
    // VEL-069: a `within:` list at y=3000 was dragged at off-screen
    // coordinates and never moved.
    let mut list = node("inner2", "", 3000);
    list.text = None;
    list.bounds.width = 1000;
    list.bounds.height = 600;
    list.children = vec![node("low", "Low 45", 3900)];
    let mut driver = ScriptDriver::new(vec![list]);
    driver.pans = true;
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let scroll = Action::ScrollUntilVisible {
        selector: Selector::Text("Low 45".into()),
        direction: Direction::Down,
        max_scrolls: 10,
        within: Some(Selector::Id("inner2".into())),
    };
    let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    for (a, b) in driver.strokes.lock().unwrap().iter() {
        for (_, y) in [a, b] {
            assert!((0..=1920).contains(y), "stroke off screen: {a:?} -> {b:?}");
        }
    }
}

/// VEL-S53: a direction-only swipe is the executor's stroke on every
/// device driver (here 1080x1920: centred, 30%-70% of the height), and
/// the driver's own page scroll on the web.
#[tokio::test]
async fn a_direction_swipe_is_the_same_stroke_on_every_device_driver() {
    let driver = ScriptDriver::new(vec![]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    for direction in [Some(Direction::Up), None] {
        let swipe = Action::Swipe {
            direction,
            from: None,
            to: None,
            within: None,
        };
        let r = ex.execute_step(&step(swipe), "d", "app", 0).await.unwrap();
        assert_eq!(r.status, StepStatus::Passed, "{r:?}");
    }
    assert_eq!(
        *driver.strokes.lock().unwrap(),
        vec![((540, 1344), (540, 576)), ((540, 576), (540, 1344))]
    );
    let web = ScriptDriver::new(vec![]);
    let mut cfg = config(300);
    cfg.platform = Some(Platform::Web);
    let mut ex = TestExecutor::new(&web, cfg, "app");
    let swipe = Action::Swipe {
        direction: Some(Direction::Up),
        from: None,
        to: None,
        within: None,
    };
    ex.execute_step(&step(swipe), "d", "app", 0).await.unwrap();
    assert!(web.strokes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_container_is_revealed_before_its_target_is_aimed_at() {
    // Review T-1: removing `reveal` from `scroll_area` broke no test.
    // M-1: the target was read before the reveal scrolled the page, so
    // the first stroke aimed at its stale, pre-scroll bounds.
    let mut list = node("inner4", "", 3000);
    list.text = None;
    list.bounds.width = 1000;
    list.bounds.height = 600;
    list.children = vec![node("row", "Row 3", 3100)];
    let mut driver = ScriptDriver::new(vec![list]);
    driver.reveals = true;
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let scroll = Action::ScrollUntilVisible {
        selector: Selector::Text("Row 3".into()),
        direction: Direction::Down,
        max_scrolls: 3,
        within: Some(Selector::Id("inner4".into())),
    };
    let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Passed, "{:?}", r.error_message);
    assert!(driver.revealed.load(Ordering::SeqCst) > 0, "never revealed");
    let strokes = driver.strokes.lock().unwrap();
    assert!(strokes.is_empty(), "aimed at stale bounds: {strokes:?}");
}

#[tokio::test]
async fn a_container_that_wont_come_in_gets_one_drag_and_no_blind_strokes() {
    // Review m5: up to 5 drags on every call (two per scroll), then a
    // stroke at the raw off-screen bounds.
    let mut list = node("inner3", "", 3000);
    list.text = None;
    list.bounds.width = 1000;
    list.bounds.height = 600;
    list.children = vec![node("low", "Low 46", 3100)];
    let driver = ScriptDriver::new(vec![list]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let scroll = Action::ScrollUntilVisible {
        selector: Selector::Text("Low 46".into()),
        direction: Direction::Down,
        max_scrolls: 3,
        within: Some(Selector::Id("inner3".into())),
    };
    let r = ex.execute_step(&step(scroll), "d", "app", 0).await.unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    let strokes = driver.strokes.lock().unwrap();
    assert_eq!(strokes.len(), 1, "{strokes:?}");
    for (a, b) in strokes.iter() {
        for (_, y) in [a, b] {
            assert!((0..=1920).contains(y), "stroke off screen: {a:?} -> {b:?}");
        }
    }
}

#[test]
fn only_the_shown_part_of_a_container_is_stroked() {
    let screen = Rect {
        x: 0,
        y: 0,
        width: 1000,
        height: 2000,
    };
    let r = |y, h| Rect {
        x: 0,
        y,
        width: 1000,
        height: h,
    };
    assert_eq!(shown_part(&screen, &r(1800, 1000)), None);
    assert_eq!(shown_part(&screen, &r(1000, 2000)), Some(r(1000, 1000)));
    assert_eq!(shown_part(&screen, &r(1980, 10)), Some(r(1980, 10)));
    assert_eq!(shown_part(&screen, &r(3000, 600)), None);
}

#[tokio::test]
async fn a_lost_device_is_infrastructure_in_assert_text_and_scrolling() {
    // VEL-073: reported as a wrong text / not found, infra=false. The
    // device goes away after a set number of lookups, not on a timer
    // (C-m6: a 30 ms flip raced the 300 ms deadline on a busy runner).
    for (action, good_reads) in [
        (
            Action::AssertText {
                selector: Selector::Text("Total".into()),
                expected: "$10".into(),
            },
            2,
        ),
        (
            Action::ScrollUntilVisible {
                selector: Selector::Text("Row 9".into()),
                direction: Direction::Down,
                max_scrolls: 3,
                within: None,
            },
            0,
        ),
    ] {
        let driver = ScriptDriver::new(vec![node("t", "Total", 0)]);
        driver.lose_after.store(good_reads, Ordering::SeqCst);
        let mut ex = TestExecutor::new(&driver, config(5_000), "app");
        let r = ex
            .execute_test(&test_case(vec![step(action)]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        assert!(r.infrastructure_failure, "{:?}", r.error_message);
    }
}

#[tokio::test]
async fn batching_relaunches_after_a_mid_test_stop() {
    // VEL-071: only a stop as the very last step counted.
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let mut cfg = config(500);
    cfg.batch = true;
    let launch = || {
        step(Action::LaunchApp {
            app_id: String::new(),
            clear_state: false,
            arguments: Default::default(),
        })
    };
    let stop = step(Action::StopApp {
        app_id: String::new(),
    });
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let first = ex
        .execute_test(
            &test_case(vec![launch(), stop, step(Action::Wait { ms: 1 })]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(first.status, TestStatus::Passed);
    let second = ex
        .execute_test(&test_case(vec![launch(), step(tap("Buy"))]), "d", "app")
        .await
        .unwrap();
    assert!(!second.steps[0].action_name.contains("batched"));
}

#[tokio::test]
async fn a_disabled_target_that_vanishes_is_not_tapped() {
    // VEL-072: the stale element was tapped at its old bounds.
    let mut submit = node("submit", "Submit", 0);
    submit.enabled = false;
    let driver = ScriptDriver::new(vec![submit]).after(2, |s| s.children.clear());
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_step(&step(tap("Submit")), "d", "app", 0)
        .await
        .unwrap();
    assert_eq!(r.status, StepStatus::Failed);
    assert!(driver.taps.lock().unwrap().is_empty());
}

fn with_liveness(driver: ScriptDriver, l: velocity_common::Liveness) -> ScriptDriver {
    *driver.liveness.lock().unwrap() = Some(l);
    driver
}

#[tokio::test]
async fn the_drivers_liveness_decides_a_negative_check_and_the_test() {
    // P2: a driver that knows the app died (its front-app check) though
    // the crash log is silent (ANR, OOM kill, force-stop).
    let dead = || velocity_common::Liveness::Dead("killed by the low-memory killer".into());
    for steps in [
        vec![step(tap("Pay")), step(not_visible("Payment failed"))],
        vec![step(tap("Pay"))],
    ] {
        let driver = with_liveness(ScriptDriver::new(vec![node("p", "Pay", 0)]), dead());
        let mut ex = TestExecutor::new(&driver, config(300), "app");
        let r = ex
            .execute_test(&test_case(steps), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Failed);
        let msg = r.error_message.unwrap();
        assert!(msg.contains("low-memory killer"), "{msg}");
    }
}

#[tokio::test]
async fn negative_checks_read_liveness_once_between_actions() {
    // C-m4: one liveness read per assertNotVisible pass (a `simctl`
    // call on iOS) cost tens of seconds over a suite.
    let driver = with_liveness(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        velocity_common::Liveness::Foreground,
    );
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(
            &test_case(vec![
                step(tap("Pay")),
                step(not_visible("Error")),
                step(not_visible("Retry")),
                step(not_visible("Cancel")),
            ]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(
        driver.liveness_reads.load(Ordering::SeqCst),
        2,
        "one for the three checks after the tap, one at the end"
    );
}

#[tokio::test]
async fn another_app_in_front_is_noted_and_never_batched() {
    let driver = with_liveness(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        velocity_common::Liveness::Background(Some("com.android.launcher3".into())),
    );
    let mut cfg = config(300);
    cfg.batch = true;
    let launch = || {
        step(Action::LaunchApp {
            app_id: String::new(),
            clear_state: false,
            arguments: Default::default(),
        })
    };
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let mut off_app = step(not_visible("Payment failed"));
    off_app.modifiers.app_in_front = Some(false);
    let first = ex
        .execute_test(&test_case(vec![launch(), off_app]), "d", "app")
        .await
        .unwrap();
    // `appInFront: false` (D-2): passes, and it is said.
    assert_eq!(first.status, TestStatus::Passed);
    assert!(
        first
            .warnings
            .iter()
            .any(|w| w.contains("launcher3") && w.contains("wasn't in front")),
        "{:?}",
        first.warnings
    );
    let second = ex
        .execute_test(&test_case(vec![launch()]), "d", "app")
        .await
        .unwrap();
    assert!(
        !second.steps[0].action_name.contains("batched"),
        "the launcher was in front: launch the app"
    );
}

#[tokio::test]
async fn a_negative_check_off_app_fails_unless_the_test_left_the_app() {
    // D-2 (VEL-014): the launcher lacks every element of the app, so
    // `assertNotVisible` there proved nothing and passed.
    let driver = with_liveness(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        velocity_common::Liveness::Background(Some("com.android.launcher3".into())),
    );
    driver.navigates.store(true, Ordering::SeqCst);
    let launch = |app: &str| {
        step(Action::LaunchApp {
            app_id: app.into(),
            clear_state: false,
            arguments: Default::default(),
        })
    };
    let run = |steps: Vec<Step>| {
        let driver = &driver;
        async move {
            let mut ex = TestExecutor::new(driver, config(300), "app");
            ex.execute_test(&test_case(steps), "d", "app")
                .await
                .unwrap()
        }
    };
    let r = run(vec![launch(""), step(not_visible("Error"))]).await;
    assert_eq!(r.status, TestStatus::Failed);
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("launcher3"), "{err}");
    assert!(err.contains("appInFront: false"), "{err}");
    // Left on purpose: the check runs on that screen, with a note.
    let home = step(Action::PressKey {
        key: velocity_common::Key::Home,
    });
    let link = step(Action::Goto {
        url: "app://promo".into(),
    });
    for (left, noted) in [(home, true), (link, true), (launch("com.other"), false)] {
        let what = action_name(&left.action);
        let r = run(vec![launch("app"), left, step(not_visible("Error"))]).await;
        assert_eq!(
            r.status,
            TestStatus::Passed,
            "{what}: {:?}",
            r.error_message
        );
        // Another app launched by the test is not "the app" at all:
        // its liveness isn't read, so there is nothing to note.
        assert_eq!(
            r.warnings.iter().any(|w| w.contains("wasn't in front")),
            noted,
            "{what}: {:?}",
            r.warnings
        );
    }
    // Until the suite's app is launched again.
    let r = run(vec![
        launch("com.other"),
        launch("app"),
        step(not_visible("Error")),
    ])
    .await;
    assert_eq!(r.status, TestStatus::Failed);
}

#[tokio::test]
async fn an_optional_step_that_fails_is_a_warning_and_the_test_goes_on() {
    // D-12: Maestro's `optional: true` (and `assertWithAI`).
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    let mut ex = TestExecutor::new(&driver, config(200), "app");
    let mut banner = step(Action::AssertVisible {
        selector: Selector::Text("Promo banner".into()),
    });
    banner.modifiers.optional = true;
    let r = ex
        .execute_test(&test_case(vec![banner, step(tap("Pay"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(r.steps[0].status, StepStatus::Passed);
    assert!(r.steps[0].action_name.contains("optional, failed"));
    assert_eq!(driver.taps.lock().unwrap().len(), 1, "the test went on");
    assert!(
        r.warnings
            .iter()
            .any(|w| w.starts_with("optional step") && w.contains("Promo banner")),
        "{:?}",
        r.warnings
    );
    // Without it, the same step fails the test.
    let r = ex
        .execute_test(
            &test_case(vec![step(Action::AssertVisible {
                selector: Selector::Text("Promo banner".into()),
            })]),
            "d",
            "app",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
}

#[tokio::test]
async fn an_optional_flow_stops_at_its_first_failure_and_the_test_goes_on() {
    // Review M2: Maestro's `runFlow: { file: onboarding.yaml, optional:
    // true }`. With no onboarding on screen, "Skip" fails; the flow's
    // "Continue" must not then run against the real screen.
    let mut run = step(Action::RunFlow {
        flow_id: "onboarding".into(),
        env: Default::default(),
    });
    run.modifiers.optional = true;
    let suite = velocity_common::TestSuite {
        app_id: "app".into(),
        config: SuiteConfig::default(),
        flows: vec![velocity_common::Flow {
            id: "onboarding".into(),
            steps: vec![step(tap("Skip")), step(tap("Continue"))],
        }],
        tests: vec![test_case(vec![run, step(tap("Pay"))])],
    };
    let test = crate::resolve_flows(&suite).unwrap().remove(0);
    let driver = ScriptDriver::new(vec![node("c", "Continue", 0), node("p", "Pay", 300)]);
    let mut ex = TestExecutor::new(&driver, config(200), "app");
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.steps);
    assert_eq!(driver.taps.lock().unwrap().as_slice(), ["Pay"]);
    assert_eq!(r.steps[1].status, StepStatus::Skipped);
    let notes: Vec<_> = r
        .warnings
        .iter()
        .filter(|w| w.starts_with("optional"))
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", r.warnings);
    assert!(notes[0].contains("onboarding"), "{notes:?}");
}

#[tokio::test]
async fn an_optional_step_does_not_forgive_the_watchdog() {
    // Review m3: a step cut off after hanging means a wedged device;
    // carrying on paid every later step's watchdog too.
    let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
    let mut ex = TestExecutor::new(&driver, config(50), "app");
    let mut hang = step(tap("Buy"));
    hang.modifiers.optional = true;
    let seen = step(Action::AssertVisible {
        selector: Selector::Text("Buy".into()),
    });
    let r = ex
        .execute_test(&test_case(vec![seen, hang]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        r.error_message.unwrap_or_default().contains("Step timeout"),
        "{:?}",
        r.warnings
    );
}

#[tokio::test]
async fn an_optional_step_does_not_forgive_a_lost_device() {
    let driver = ScriptDriver::new(vec![node("pay", "Pay", 0)]);
    driver.lost.store(true, Ordering::SeqCst);
    let mut ex = TestExecutor::new(&driver, config(200), "app");
    let mut t = step(tap("Pay"));
    t.modifiers.optional = true;
    let r = ex
        .execute_test(&test_case(vec![t]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(r.infrastructure_failure);
}

#[tokio::test]
async fn an_isolated_test_is_never_batched() {
    // VEL-212: `isolated: true` was parsed and ignored.
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let mut cfg = config(500);
    cfg.batch = true;
    let launch = || {
        step(Action::LaunchApp {
            app_id: String::new(),
            clear_state: false,
            arguments: Default::default(),
        })
    };
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    ex.execute_test(&test_case(vec![launch()]), "d", "app")
        .await
        .unwrap();
    let mut isolated = test_case(vec![launch()]);
    isolated.isolated = true;
    let r = ex.execute_test(&isolated, "d", "app").await.unwrap();
    assert!(
        !r.steps[0].action_name.contains("batched"),
        "{:?}",
        r.steps[0]
    );
}

fn regions(masks: &[crate::visual::MaskPreset]) -> Vec<(u32, u32, u32, u32)> {
    masks
        .iter()
        .map(|m| {
            let r = m.to_region(1080, 1920);
            (r.x, r.y, r.width, r.height)
        })
        .collect()
}

#[tokio::test]
async fn screenshots_mask_the_bars_the_driver_reports() {
    // P8: Android's status and navigation bars, not 4% of the height.
    let mut driver = ScriptDriver::new(vec![node("clock", "12:00", 1800)]);
    let bar = |y, height| Rect {
        x: 0,
        y,
        width: 1080,
        height,
    };
    driver.bars = Some(vec![bar(0, 63), bar(1794, 126)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let masks = ex.screenshot_masks("d", &[], &[]).await.unwrap();
    assert_eq!(regions(&masks), [(0, 0, 1080, 63), (0, 1794, 1080, 126)]);
    // A driver that can't tell keeps the old guess.
    driver.bars = None;
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let masks = ex.screenshot_masks("d", &[], &[]).await.unwrap();
    assert_eq!(regions(&masks), [(0, 0, 1080, 76)]);
    // VEL-063: an element partly below the screen is masked only where
    // it is on screen.
    driver.bars = Some(vec![]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let masks = ex
        .screenshot_masks("d", &[], &[Selector::Text("12:00".into())])
        .await
        .unwrap();
    // 1800 + 40 is on screen (1920): all of it; move it lower.
    assert_eq!(regions(&masks), [(0, 1800, 100, 40)]);
    driver.screen.lock().unwrap().children[0].bounds.y = 1900;
    let masks = ex
        .screenshot_masks("d", &[], &[Selector::Text("12:00".into())])
        .await
        .unwrap();
    assert_eq!(regions(&masks), [(0, 1900, 100, 20)]);
}

/// S3: while `selectors.strict` is unset, an ambiguous action's warning
/// says it becomes an error in 1.0 and how to keep today's behaviour;
/// with it set to false it doesn't, and nothing fails either way.
#[tokio::test]
async fn an_ambiguous_action_warns_of_the_1_0_default_while_strict_is_unset() {
    let driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    let w = r
        .warnings
        .iter()
        .find(|w| w.contains("matched 2 elements"))
        .unwrap_or_else(|| panic!("{:?}", r.warnings));
    if velocity_common::env::var("VELOCITY_SELECTORS_STRICT").is_err() {
        assert!(w.contains(STRICT_STAGING), "{w}");
        assert!(w.contains("selectors.strict: false"), "{w}");
    }
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(false);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    let w = r
        .warnings
        .iter()
        .find(|w| w.contains("matched 2 elements"))
        .unwrap_or_else(|| panic!("{:?}", r.warnings));
    assert!(!w.contains("1.0"), "{w}");
}

#[tokio::test]
async fn ambiguity_is_counted_on_every_driver_with_ready_made_picks() {
    // D-6 / R5: a driver without a tree cache warned nothing; now every
    // driver counts what its own lookup finds, and the warning names a
    // selector picking each match.
    let row = |id: &str, y: i32| Element {
        children: vec![node("", "Add", y + 10)],
        ..node(id, "", y)
    };
    let rows = || vec![row("com.app:id/row_a", 0), row("com.app:id/row_b", 100)];
    let driver = ScriptDriver::new(rows());
    assert!(!driver.has_hierarchy_cache());
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    let w = r
        .warnings
        .iter()
        .find(|w| w.contains("matched 2 elements"))
        .unwrap_or_else(|| panic!("{:?}", r.warnings));
    assert!(
        w.contains(r#"{ text: "Add", childOf: { id: "row_a" } }"#),
        "{w}"
    );
    assert!(
        w.contains(r#"{ text: "Add", childOf: { id: "row_b" } }"#),
        "{w}"
    );
    // Strict mode fails and names the same picks.
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        r.error_message.as_deref().unwrap_or("").contains("childOf"),
        "{:?}",
        r.error_message
    );
    // On the web the page's own matches count, hidden ones not (one
    // inside a closed <details>): no warning.
    let mut hidden = node("b", "Save", 100);
    hidden.visible = false;
    let driver = ScriptDriver::new(vec![node("a", "Save", 0), hidden]);
    let mut web = config(300);
    web.platform = Some(Platform::Web);
    let mut ex = TestExecutor::new(&driver, web.clone(), "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Save"))]), "d", "app")
        .await
        .unwrap();
    assert!(
        !r.warnings.iter().any(|w| w.contains("matched")),
        "{:?}",
        r.warnings
    );
    let driver = ScriptDriver::new(vec![node("a", "Save", 0), node("b", "Save", 100)]);
    let mut ex = TestExecutor::new(&driver, web, "app");
    let r = ex
        .execute_test(&test_case(vec![step(tap("Save"))]), "d", "app")
        .await
        .unwrap();
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("matched 2") && w.contains(r#"{ text: "Save", index: 1 }"#)),
        "{:?}",
        r.warnings
    );
}

#[tokio::test]
async fn the_ambiguity_check_adds_no_lookup_to_a_clear_tap() {
    // Review MAJOR 8: every action on web, Flutter, RN and headless
    // paid a second full lookup (`find_elements`) just to count. The
    // count now comes from the lookup that resolved.
    async fn tap_once(
        driver: &ScriptDriver,
        platform: Option<Platform>,
        strict: bool,
    ) -> TestResult {
        let mut cfg = config(300);
        cfg.platform = platform;
        let mut ex = TestExecutor::new(driver, cfg, "app").with_strict_selectors(strict);
        ex.execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
            .await
            .unwrap()
    }
    for platform in [None, Some(Platform::Web)] {
        let driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Other", 100)]);
        let r = tap_once(&driver, platform, false).await;
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(
            driver.find_alls.load(Ordering::SeqCst),
            0,
            "{platform:?}: lookups per tap beyond the resolve"
        );
        // Ambiguous: the listing (and only then) looks again.
        let driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Buy", 100)]);
        let r = tap_once(&driver, platform, false).await;
        assert!(
            r.warnings.iter().any(|w| w.contains("matched 2")),
            "{:?}",
            r.warnings
        );
    }
    // A driver that doesn't count: no extra lookup unless strict.
    let mut driver = ScriptDriver::new(vec![node("a", "Buy", 0), node("b", "Buy", 100)]);
    driver.counts = false;
    tap_once(&driver, None, false).await;
    assert_eq!(driver.find_alls.load(Ordering::SeqCst), 0);
    let r = tap_once(&driver, None, true).await;
    assert_eq!(r.status, TestStatus::Failed, "strict still counts");
    assert_eq!(driver.find_alls.load(Ordering::SeqCst), 1);
}

#[test]
fn ci_is_detected_beyond_the_ci_variable() {
    // VEL-214: Jenkins, Azure Pipelines and CodeBuild don't set CI.
    for var in ["JENKINS_URL", "TF_BUILD", "CODEBUILD_BUILD_ID", "CI"] {
        assert!(in_ci(|k| k == var), "{var}");
    }
    assert!(!in_ci(|_| false));
}

#[test]
fn ci_false_or_zero_is_not_ci() {
    use std::ffi::OsStr;
    for v in ["false", "FALSE", "0", "", " "] {
        assert!(!ci_flag(OsStr::new(v)), "{v:?}");
    }
    for v in ["true", "1", "yes", "github"] {
        assert!(ci_flag(OsStr::new(v)), "{v:?}");
    }
}

/// review-fixes #6: a count left from an earlier screen (more matches
/// than the page holds now) never fails a strict tap as ambiguous.
#[tokio::test]
async fn a_stale_match_count_never_makes_a_lookup_ambiguous() {
    let mut driver = ScriptDriver::new(vec![node("a", "Add", 0)]);
    driver.counts = false;
    driver
        .match_counts
        .record("d", &Selector::Text("Add".into()), 2);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(driver.taps.lock().unwrap().len(), 1);
}

/// review-fixes #13: the fake records counts under the device it was
/// asked about (it used `"d"` whatever the device).
#[tokio::test]
async fn the_script_driver_counts_per_device() {
    let driver = ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
    let add = Selector::Text("Add".into());
    driver.find_element("phone-2", &add).await.unwrap();
    assert_eq!(driver.last_match_count("phone-2", &add), Some(2));
    assert_eq!(driver.last_match_count("d", &add), None);
    assert!(driver
        .find_element("phone-2", &Selector::Text("Gone".into()))
        .await
        .is_err());
    assert_eq!(
        driver.last_match_count("phone-2", &add),
        None,
        "a miss clears"
    );
}

#[tokio::test]
async fn strict_selectors_fail_an_ambiguous_tap_on_any_driver() {
    // P11, opt-in: two "Add" buttons; the driver keeps no tree cache
    // (web, headless), where not even the warning was given (VEL-220).
    let two = || ScriptDriver::new(vec![node("a", "Add", 0), node("b", "Add", 100)]);
    let driver = two();
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap();
    assert!(msg.contains("2 match") && msg.contains("(0, 100)"), "{msg}");
    assert!(driver.taps.lock().unwrap().is_empty(), "nothing tapped");
    // Off (the default): the first is tapped, as before.
    let driver = two();
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(false);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    // One match passes in strict mode too.
    let driver = ScriptDriver::new(vec![node("a", "Add", 0)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_strict_selectors(true);
    let r = ex
        .execute_test(&test_case(vec![step(tap("Add"))]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn web_suites_get_the_tree_settle_they_ask_for() {
    // VEL-067: `sync.mode: polling` was dropped on the web.
    for (mode, settles) in [(SyncMode::Polling, true), (SyncMode::Auto, false)] {
        let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
        let mut cfg = config(2000);
        cfg.platform = Some(Platform::Web);
        cfg.sync.mode = mode;
        cfg.sync.interval_ms = 10;
        let mut ex = TestExecutor::new(&driver, cfg, "app");
        let r = ex
            .execute_test(&test_case(vec![step(tap("Buy"))]), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
        assert_eq!(driver.dumps.load(Ordering::SeqCst) > 0, settles, "{mode:?}");
    }
}

#[tokio::test]
async fn a_hung_device_cannot_hold_the_run_in_failure_artifacts() {
    // VEL-351: the failure screenshot after a watchdog timeout ran
    // outside the watchdog, on the device that had just hung.
    let dir = tempfile::tempdir().unwrap();
    let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
    let mut cfg = config(50);
    cfg.artifacts.on_failure = true;
    cfg.artifacts.output_dir = dir.path().to_string_lossy().into_owned();
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let t = test_case(vec![step(tap("Buy"))]);
    let run = ex.execute_test(&t, "d", "app");
    let r = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the run must end")
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
}

/// SUM-4: on the web `web.settle` wins over `sync.*` (a cross-platform
/// suite tunes each side), what it leaves unset falls back to `sync.*`,
/// and off the web it does nothing.
#[test]
fn web_settle_wins_over_sync_on_the_web_only() {
    let driver = ScriptDriver::new(vec![]);
    let suite = |platform: Platform| {
        let mut c = config(50);
        c.platform = Some(platform);
        c.sync.reaction_ms = Some(500);
        c.sync.quiet_ms = Some(90);
        c.web = Some(velocity_common::test_types::WebConfig {
            settle: Some(velocity_common::test_types::WebSettle {
                reaction_ms: Some(77),
                cap_ms: Some(600),
                ..Default::default()
            }),
            ..Default::default()
        });
        c
    };
    let mut ex = TestExecutor::new(&driver, suite(Platform::Web), "app");
    let cfg = ex.sync_manager.sync_config_mut().clone();
    assert_eq!(
        (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms),
        (Some(77), Some(90), Some(600))
    );
    let mut ex = TestExecutor::new(&driver, suite(Platform::Android), "app");
    let cfg = ex.sync_manager.sync_config_mut().clone();
    assert_eq!(
        (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms),
        (Some(500), Some(90), None)
    );
}

#[tokio::test]
async fn web_settle_survives_set_config_and_the_next_test() {
    // review-d6 M1: the overlay lived only in the SyncManager's clone,
    // so `setConfig` and `restore_test_config` reset it to `sync.*`.
    let driver = ScriptDriver::new(vec![]);
    let mut c = config(50);
    c.platform = Some(Platform::Web);
    c.sync.reaction_ms = Some(300);
    c.web = Some(velocity_common::test_types::WebConfig {
        settle: Some(velocity_common::test_types::WebSettle {
            reaction_ms: Some(40),
            cap_ms: Some(800),
            ..Default::default()
        }),
        ..Default::default()
    });
    let mut ex = TestExecutor::new(&driver, c, "app");
    let timings = |ex: &mut TestExecutor| {
        let cfg = ex.sync_manager.sync_config_mut().clone();
        (cfg.reaction_ms, cfg.quiet_ms, cfg.settle_cap_ms)
    };
    let set = step(Action::SetConfig {
        values: [("sync.quietMs".to_string(), "200".to_string())]
            .into_iter()
            .collect(),
    });
    let r = ex
        .execute_test(&test_case(vec![set]), "web", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    // Test 2 runs with web.settle again, not sync.* or the defaults.
    assert_eq!(timings(&mut ex), (Some(40), None, Some(800)));
    // And inside a test, setConfig changes only the key it sets.
    ex.execute_action(
        &Action::SetConfig {
            values: [("sync.quietMs".to_string(), "200".to_string())]
                .into_iter()
                .collect(),
        },
        "web",
        "app",
    )
    .await
    .unwrap();
    assert_eq!(timings(&mut ex), (Some(40), Some(200), Some(800)));
}

#[tokio::test]
async fn a_cancelled_step_does_not_leak_its_settle_override() {
    // VEL-058: restored only when the step body returned.
    let driver = HangingTap(ScriptDriver::new(vec![node("b", "Buy", 0)]));
    let mut ex = TestExecutor::new(&driver, config(50), "app");
    let cap = ex.sync_manager.sync_config_mut().settle_cap_ms;
    let hung = Step {
        settle: Some(velocity_common::StepSettle {
            cap_ms: Some(cap.unwrap_or_default() + 1234),
            ..Default::default()
        }),
        ..step(tap("Buy"))
    };
    let r = ex
        .execute_test(&test_case(vec![hung]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(ex.sync_manager.sync_config_mut().settle_cap_ms, cap);
    assert!(ex.step_settle.is_none());
}

#[tokio::test]
async fn huge_timeouts_do_not_overflow() {
    // VEL-191: `base * 3` and `Instant + timeoutMs` overflowed.
    let driver = ScriptDriver::new(vec![node("b", "Buy", 0)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app");
    let huge = Step {
        timeout_ms: Some(u64::MAX),
        ..step(tap("Buy"))
    };
    assert_eq!(ex.step_deadline(&huge), Duration::from_millis(u64::MAX));
    let wait = step(Action::Wait { ms: u64::MAX });
    assert_eq!(ex.step_deadline(&wait), Duration::from_millis(u64::MAX));
    let r = ex
        .execute_test(&test_case(vec![huge]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

#[tokio::test]
async fn a_screenshot_name_cannot_leave_the_artifacts_dir() {
    // VEL-197: `join` with `..` or an absolute path wrote outside it.
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    let driver = ScriptDriver::new(vec![]);
    let mut cfg = config(300);
    cfg.artifacts.output_dir = out.to_string_lossy().into_owned();
    cfg.artifacts.screenshots = velocity_common::ScreenshotPolicy::Always;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let shot = step(Action::Screenshot {
        filename: Some("../escaped.png".into()),
    });
    let r = ex
        .execute_test(&test_case(vec![shot]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert!(!root.path().join("escaped.png").exists());
    assert!(out.join("escaped.png").exists());
}

/// A screen with an unlabeled icon button, and the driver's lookup
/// cache on (Android's), so the audit reads only cached trees.
fn a11y_driver() -> ScriptDriver {
    let icon = Element {
        text: None,
        element_type: "android.widget.ImageButton".into(),
        ..node("menu", "", 100)
    };
    let mut driver = ScriptDriver::new(vec![node("pay", "Pay", 0), icon]);
    driver.lookup_cache = Some(Mutex::new(None));
    driver
}

fn a11y_steps() -> TestCase {
    let seen = || {
        step(Action::AssertVisible {
            selector: Selector::Text("Pay".into()),
        })
    };
    test_case(vec![seen(), step(tap("Pay")), seen()])
}

#[tokio::test]
async fn the_audit_warns_once_per_problem_and_reads_no_extra_trees() {
    let run = |cfg: velocity_common::AccessibilityConfig| async move {
        let driver = a11y_driver();
        let mut c = config(2000);
        c.accessibility = cfg;
        let mut ex = TestExecutor::new(&driver, c, "app");
        let r = ex.execute_test(&a11y_steps(), "d", "app").await.unwrap();
        let audits = ex.a11y.as_ref().map_or(0, |a| a.audits());
        (r, driver.dumps.load(Ordering::SeqCst), audits)
    };
    // Off by default: no findings.
    let (off, dumps_off, _) = run(Default::default()).await;
    assert_eq!(off.status, TestStatus::Passed, "{:?}", off.error_message);
    assert!(off.warnings.iter().all(|w| !w.starts_with("accessibility")));
    // On: the same screen read by three lookups is audited once, its
    // one problem reported once, and no tree is read for it.
    let on = velocity_common::AccessibilityConfig {
        audit: true,
        ..Default::default()
    };
    let (r, dumps_on, audits) = run(on.clone()).await;
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    let found: Vec<&String> = r
        .warnings
        .iter()
        .filter(|w| w.starts_with("accessibility: "))
        .collect();
    assert_eq!(found.len(), 1, "{:?}", r.warnings);
    assert!(
        found[0].contains("image-unlabeled") && found[0].contains("#menu"),
        "{found:?}"
    );
    assert_eq!(audits, 1);
    assert_eq!(dumps_on, dumps_off, "the audit read the screen itself");
    // `fail: true`: the error finding fails the test on its last step.
    let (r, _, _) = run(velocity_common::AccessibilityConfig { fail: true, ..on }).await;
    assert_eq!(r.status, TestStatus::Failed);
    let msg = r.error_message.unwrap_or_default();
    assert!(
        msg.starts_with("accessibility audit found 1 error(s)"),
        "{msg}"
    );
    assert_eq!(r.steps.last().unwrap().status, StepStatus::Failed);
}

#[tokio::test]
async fn the_audit_caps_its_warnings_per_test() {
    let icons: Vec<Element> = (0..25)
        .map(|i| Element {
            text: None,
            element_type: "android.widget.ImageButton".into(),
            ..node(&format!("icon{i}"), "", 100 + i * 50)
        })
        .collect();
    let mut children = vec![node("pay", "Pay", 0)];
    children.extend(icons);
    let mut driver = ScriptDriver::new(children);
    driver.lookup_cache = Some(Mutex::new(None));
    let mut c = config(2000);
    c.accessibility.audit = true;
    let mut ex = TestExecutor::new(&driver, c, "app");
    let r = ex.execute_test(&a11y_steps(), "d", "app").await.unwrap();
    let found: Vec<&String> = r
        .warnings
        .iter()
        .filter(|w| w.starts_with("accessibility: "))
        .collect();
    assert_eq!(
        found.len(),
        crate::a11y_run::MAX_PER_TEST + 1,
        "{:?}",
        r.warnings
    );
    assert!(
        found.last().unwrap().contains("5 more finding(s) left out"),
        "{found:?}"
    );
}

/// Review 01 #3: `exec` ran without a process group, so a timed-out
/// script's own children (a server, `sleep`) were left running.
#[cfg(unix)]
#[tokio::test]
async fn a_timed_out_exec_takes_its_children_down_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let driver = ScriptDriver::new(vec![node("x", "X", 0)]);
    let mut cfg = config(300);
    cfg.exec = Some(velocity_common::exec::ExecConfig {
        allow: vec!["sh".into()],
    });
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
    let s = Step {
        timeout_ms: Some(400),
        ..step(Action::Exec(Box::new(velocity_common::exec::ExecStep {
            run: velocity_common::exec::ExecCommand::Parts(vec!["sh".into(), "-c".into(), script]),
            expect: Default::default(),
            capture: None,
            cwd: None,
        })))
    };
    let r = ex
        .execute_test(&test_case(vec![s]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed, "{:?}", r.error_message);
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    let pid = pid.trim();
    // The group was SIGKILLed; give the kernel a moment to reap it.
    let mut alive = true;
    for _ in 0..50 {
        let st = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        if !st.success() {
            alive = false;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!alive, "the script's child {pid} outlived the step");
}

/// Seeded failures: Calendar in front of a Settings suite read only as "not
/// found". The failure names the app that was in front.
#[tokio::test]
async fn a_missing_element_names_the_app_in_front() {
    let driver = with_liveness(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        velocity_common::Liveness::Background(Some("com.google.android.calendar".into())),
    );
    let mut ex = TestExecutor::new(&driver, config(300), "com.android.settings");
    let r = ex
        .execute_test(
            &test_case(vec![step(Action::AssertVisible {
                selector: Selector::Text("Network".into()),
            })]),
            "d",
            "com.android.settings",
        )
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let err = r.steps[0].error_message.clone().unwrap_or_default();
    assert!(
        err.contains("com.google.android.calendar was in front, not com.android.settings"),
        "{err}"
    );
    // With the app itself in front, nothing is added.
    let driver = with_liveness(
        ScriptDriver::new(vec![node("p", "Pay", 0)]),
        velocity_common::Liveness::Foreground,
    );
    let mut ex = TestExecutor::new(&driver, config(300), "com.android.settings");
    let r = ex
        .execute_test(
            &test_case(vec![step(Action::AssertVisible {
                selector: Selector::Text("Network".into()),
            })]),
            "d",
            "com.android.settings",
        )
        .await
        .unwrap();
    let err = r.steps[0].error_message.clone().unwrap_or_default();
    assert!(!err.contains("was in front"), "{err}");
}

/// Real apps: an optional step that misses is expected to miss; healing it
/// onto a look-alike only cost time (and could tap the wrong element).
#[tokio::test]
async fn an_optional_step_does_not_heal() {
    let driver = ScriptDriver::new(vec![node("submit", "Submit orders", 0)]);
    let mut cfg = config(300);
    cfg.healing.enabled = true;
    cfg.healing.mode = Some(velocity_common::HealingMode::Auto);
    cfg.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, cfg, "app");
    let mut t = step(tap("Submit order"));
    t.modifiers.optional = true;
    let r = ex
        .execute_test(&test_case(vec![t]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.healed_count(), 0, "{:?}", r.warnings);
    assert!(driver.taps.lock().unwrap().is_empty(), "nothing tapped");
}

#[test]
fn hostile_bounds_do_not_overflow_the_stillness_check() {
    // i32::MIN - i32::MAX overflowed: a panic in a debug build, and in a
    // release build a wrapped 1, so the two reads counted as the same box.
    let at = |x: i32| velocity_common::Rect {
        x,
        y: 0,
        width: 10,
        height: 10,
    };
    assert!(!bounds_close(&at(i32::MIN), &at(i32::MAX)));
    assert!(bounds_close(&at(5), &at(6)));
    assert!(!bounds_close(&at(5), &at(7)));
}

#[test]
fn a_scroll_container_with_hostile_bounds_does_not_overflow() {
    let list = Element {
        element_type: "ScrollView".into(),
        bounds: velocity_common::Rect {
            x: i32::MAX - 1,
            y: 0,
            width: 100,
            height: 100,
        },
        ..Default::default()
    };
    let target = Element {
        bounds: velocity_common::Rect {
            x: i32::MAX - 1,
            y: 10,
            width: 1,
            height: 10,
        },
        ..Default::default()
    };
    let tree = Element {
        children: vec![list],
        ..Default::default()
    };
    assert!(super::lookup::scroll_container_of(&tree, &target).is_some());
}

#[tokio::test]
async fn a_crash_after_an_abandoned_optional_flow_still_fails_the_test() {
    // The end-of-test liveness check ran only when every step had passed,
    // and an abandoned optional flow's skipped steps are not passed ones.
    let mut run = step(Action::RunFlow {
        flow_id: "onboarding".into(),
        env: Default::default(),
    });
    run.modifiers.optional = true;
    let suite = velocity_common::TestSuite {
        app_id: "app".into(),
        config: SuiteConfig::default(),
        flows: vec![velocity_common::Flow {
            id: "onboarding".into(),
            steps: vec![step(tap("Skip")), step(tap("Continue"))],
        }],
        tests: vec![test_case(vec![run, step(tap("Pay"))])],
    };
    let test = crate::resolve_flows(&suite).unwrap().remove(0);
    let driver = with_liveness(
        ScriptDriver::new(vec![node("c", "Continue", 0), node("p", "Pay", 300)]),
        velocity_common::Liveness::Dead("killed by the low-memory killer".into()),
    );
    let mut ex = TestExecutor::new(&driver, config(200), "app");
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.steps[1].status, StepStatus::Skipped, "{:?}", r.steps);
    assert_eq!(r.status, TestStatus::Failed, "{:?}", r.steps);
    assert!(r.error_message.unwrap().contains("low-memory killer"));
}
