//! The executor running natural-language steps through the real
//! `velocity_nl::NaturalResolver`: the cache contract (write only what the
//! run confirmed), re-planning, the D-17 checks and `verify:`.
//!
//! These lived in `velocity-core`'s executor tests; core no longer depends
//! on `velocity-nl` (it sees the resolver through
//! `velocity_common::natural::NaturalStepResolver`), so they run here,
//! where both crates are already dependencies.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use velocity_common::selector_match::find_in_tree;
use velocity_common::{
    Action, DeviceInfo, Direction, Element, Key, NaturalKind, Platform, PlatformDriver, Rect,
    Result, Selector, StepStatus, SuiteConfig, SyncMode, TestStatus, VelocityError,
};
use velocity_common::{Step, TestCase};
use velocity_core::TestExecutor;
use velocity_nl::{NaturalPlanner, NaturalResolver, NaturalStep, Op, PlanRequest, PlannedStep};

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

/// A fixed screen that counts dumps and records taps (by element text).
struct ScriptDriver {
    screen: Mutex<Element>,
    dumps: AtomicUsize,
    taps: Mutex<Vec<String>>,
    /// `navigate` (openLink/goto) succeeds.
    navigates: AtomicBool,
    /// What `current_url` says.
    page_url: Mutex<Option<String>>,
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
            dumps: AtomicUsize::new(0),
            taps: Mutex::new(Vec::new()),
            navigates: AtomicBool::new(false),
            page_url: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl PlatformDriver for ScriptDriver {
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
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn find_element(&self, _: &str, selector: &Selector) -> Result<Element> {
        find_in_tree(&self.screen.lock().unwrap(), selector, None).ok_or_else(|| {
            VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms: 0,
                screenshot: None,
                hierarchy_snapshot: None,
            }
        })
    }
    async fn find_elements(&self, _: &str, selector: &Selector) -> Result<Vec<Element>> {
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
        Ok(self.screen.lock().unwrap().clone())
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
        self.taps
            .lock()
            .unwrap()
            .push(el.text.clone().unwrap_or_default());
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
    async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        Ok(())
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        Ok(vec![])
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((1080, 1920))
    }
    async fn get_element_text(&self, _: &str, el: &Element) -> Result<String> {
        Ok(el.text.clone().unwrap_or_default())
    }
    async fn is_element_visible(&self, _: &str, el: &Element) -> Result<bool> {
        Ok(find_in_tree(
            &self.screen.lock().unwrap(),
            &Selector::Id(el.platform_id.clone()),
            None,
        )
        .is_some())
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

/// Always picks element 0 and counts calls.
struct FirstElement(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl NaturalPlanner for FirstElement {
    async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(PlannedStep {
            op: Op::Tap,
            element: Some(0),
            text: String::new(),
            direction: "none".into(),
            ms: 0,
            key: Default::default(),
        })
    }
}

fn resolver(name: &str, calls: Arc<AtomicUsize>) -> Arc<NaturalResolver> {
    let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(NaturalResolver::new(
        Box::new(FirstElement(calls)),
        dir.join("nl-cache.json"),
    ))
}

fn natural(intent: &str) -> Step {
    step(Action::Natural {
        intent: intent.into(),
        kind: NaturalKind::Act,
        data: None,
        data_name: None,
        wait_until: None,
        intent_template: None,
    })
}

#[tokio::test]
async fn cached_natural_step_needs_no_dump_and_no_model_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("hit", calls.clone());
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let test = test_case(vec![natural("tap buy")]);

    let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl.clone());
    assert_eq!(
        ex.execute_test(&test, "d", "app").await.unwrap().status,
        TestStatus::Passed
    );
    let dumps_after_plan = driver.dumps.load(Ordering::SeqCst);

    let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl);
    assert_eq!(
        ex.execute_test(&test, "d", "app").await.unwrap().status,
        TestStatus::Passed
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "rerun must not call the model"
    );
    assert_eq!(
        driver.dumps.load(Ordering::SeqCst),
        dumps_after_plan,
        "a cache hit must not capture the screen"
    );
}

// The cache contract: a plan is written only once the run confirmed it.
// Here the step after it fails, so the plan is never cached and the next
// run plans again; once a test passes, its plans are kept.
#[tokio::test]
async fn a_plan_is_cached_only_after_the_run_confirms_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("confirm", calls.clone());
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let unconfirmed = test_case(vec![natural("tap buy"), step(tap("Nowhere"))]);
    let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl.clone());
    let r = ex.execute_test(&unconfirmed, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        nl.cached(&first_step("tap buy", NaturalKind::Act))
            .await
            .is_none(),
        "the step after it failed: not cached"
    );

    let confirmed = test_case(vec![natural("tap buy"), step(tap("Buy"))]);
    let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl.clone());
    let r = ex.execute_test(&confirmed, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "planned again");
    assert!(nl
        .cached(&first_step("tap buy", NaturalKind::Act))
        .await
        .is_some());

    // A plan in a test's last step is kept when the test passes.
    let last = test_case(vec![natural("tap buy")]);
    let nl2 = resolver("confirm-last", Arc::new(AtomicUsize::new(0)));
    let mut ex = TestExecutor::new(&driver, config(200), "app").with_planner(nl2.clone());
    ex.execute_test(&last, "d", "app").await.unwrap();
    assert!(nl2
        .cached(&first_step("tap buy", NaturalKind::Act))
        .await
        .is_some());
}

// Two runs of one data-driven step with different values share the
// cache entry and type each run's own value, with no second model call.
#[tokio::test]
async fn a_data_driven_step_replays_with_each_runs_value() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut typed = answer(Op::InputText, Some(0));
    typed.text = String::new();
    let nl = answering("datadriven", vec![typed], calls.clone());
    let field = |id: &str| Element {
        element_type: "EditText".into(),
        text: None,
        label: Some("Email".into()),
        ..node(id, "", 0)
    };
    let typed_into = |value: &str| {
        test_case(vec![step(Action::Natural {
            intent: "type the email".into(),
            kind: NaturalKind::Act,
            data: Some(value.into()),
            data_name: Some("${EMAIL}".into()),
            wait_until: None,
            intent_template: None,
        })])
    };
    let driver = ScriptDriver::new(vec![field("email")]);
    for value in ["ann@example.com", "bob@example.com"] {
        let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
        let r = ex
            .execute_test(&typed_into(value), "d", "app")
            .await
            .unwrap();
        assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the second value replays");
    let file = std::fs::read_to_string(nl.cache_path()).unwrap();
    assert!(
        !file.contains("ann@example.com") && !file.contains("bob@"),
        "{file}"
    );
}

/// A checker with a fixed verdict that counts its calls.
struct Verdict(
    velocity_nl::CheckAnswer,
    Arc<AtomicUsize>,
    Mutex<Vec<String>>,
);

#[async_trait::async_trait]
impl velocity_nl::ScreenChecker for Verdict {
    async fn check(&self, req: &velocity_nl::CheckRequest<'_>) -> Result<velocity_nl::CheckAnswer> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.2.lock().unwrap().push(req.claim.to_string());
        Ok(self.0.clone())
    }
}

fn checked(
    name: &str,
    holds: bool,
    ids: &[i64],
    checks: Arc<AtomicUsize>,
) -> (Arc<NaturalResolver>, Arc<AtomicUsize>) {
    let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let r = NaturalResolver::new(
        Box::new(FirstElement(calls.clone())),
        dir.join("nl-cache.json"),
    )
    .with_checker(Box::new(Verdict(
        velocity_nl::CheckAnswer {
            holds,
            evidence_element_ids: ids.to_vec(),
            reason: "fake".into(),
            ..Default::default()
        },
        checks,
        Mutex::new(vec![]),
    )));
    (Arc::new(r), calls)
}

fn with_verify(level: velocity_common::VerifyLevel) -> SuiteConfig {
    SuiteConfig {
        verify: Some(level),
        ..config(300)
    }
}

fn checkpoint(action: Action, claim: &str) -> Step {
    let mut s = step(action);
    s.modifiers.checkpoint = Some(claim.into());
    s
}

// verify: off (the default) never asks the model, even with a
// checker configured and checkpoints written.
#[tokio::test]
async fn verify_off_makes_no_model_call() {
    use velocity_common::VerifyLevel;
    let checks = Arc::new(AtomicUsize::new(0));
    let (nl, _) = checked("verify-off", true, &[0], checks.clone());
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let test = test_case(vec![checkpoint(tap("Buy"), "bought")]);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert!(r.verification.is_empty());
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    // --verify off wins over the suite's level too.
    let mut ex = TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app")
        .with_planner(nl)
        .with_verify(Some(VerifyLevel::Off));
    ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(checks.load(Ordering::SeqCst), 0);
}

// verify: final checks the description on the last screen. A verdict
// that cites no real element is unknown, and fails the test.
#[tokio::test]
async fn verify_final_needs_real_evidence() {
    use velocity_common::{VerifyLevel, VerifyOutcome};
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let mut test = test_case(vec![step(tap("Buy"))]);
    test.description = Some("the purchase went through".into());

    let checks = Arc::new(AtomicUsize::new(0));
    let (nl, _) = checked("verify-final-ok", true, &[0], checks.clone());
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app").with_planner(nl);
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(r.verification.len(), 1);
    assert_eq!(r.verification[0].at, "final");
    assert_eq!(r.verification[0].claim, "the purchase went through");
    assert_eq!(r.verification[0].outcome, VerifyOutcome::Holds);
    assert!(r.verification[0].evidence[0].contains("Buy"));

    // "Holds", citing element 42 of a one-element screen: not a pass.
    let (nl, _) = checked("verify-final-fake", true, &[42], checks.clone());
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(r.verification[0].outcome, VerifyOutcome::Unknown);
    let err = r.error_message.unwrap();
    assert!(
        err.contains("AI verification failed") && err.contains("unknown"),
        "{err}"
    );

    // The name stands in for a missing description; a test that fails
    // its checks keeps none of its plans.
    let mut plain = test_case(vec![natural("tap buy")]);
    plain.name = "buying works".into();
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Final), "app").with_planner(nl.clone());
    let r = ex.execute_test(&plain, "d", "app").await.unwrap();
    assert_eq!(r.verification[0].claim, "buying works");
    assert_eq!(r.status, TestStatus::Failed);
    let key = NaturalStep {
        scope: "buying works#0",
        ..first_step("tap buy", NaturalKind::Act)
    };
    assert!(
        nl.cached(&key).await.is_none(),
        "unverified plans aren't cached"
    );
}

// A test's own `verify:` survives parsing and flow resolution, the path a
// real run takes. Until 2026-11-20 the resolver dropped it, so the check
// never ran and a test that should have failed passed. The tests above
// build TestCase by hand and never went through the resolver.
#[tokio::test]
async fn a_tests_own_verify_runs_after_parsing_and_resolution() {
    use velocity_common::VerifyOutcome;
    let yaml = r#"
appId: app
tests:
  - name: buying works
    description: the purchase went through
    verify: final
    steps:
      - tap: { text: Buy }
"#;
    let suite = velocity_core::parse_suite_from_str(yaml).unwrap();
    let tests = velocity_core::resolve_flows(&suite).unwrap();
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let checks = Arc::new(AtomicUsize::new(0));
    let (nl, _) = checked("verify-own-level", false, &[], checks.clone());
    // The suite sets no level: the test's own is the only one.
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl);
    let r = ex.execute_test(&tests[0], "d", "app").await.unwrap();
    assert_eq!(checks.load(Ordering::SeqCst), 1, "the check ran");
    assert_eq!(
        r.status,
        TestStatus::Failed,
        "a failed check fails the test"
    );
    assert_eq!(r.verification[0].claim, "the purchase went through");
    assert_eq!(r.verification[0].outcome, VerifyOutcome::Fails);
}

// checkpoints: a failed check is recorded and the test goes on, failing
// at the end; strict stops at the first failed check.
#[tokio::test]
async fn checkpoints_report_at_the_end_and_strict_stops_at_once() {
    use velocity_common::{VerifyLevel, VerifyOutcome};
    let test = test_case(vec![
        checkpoint(tap("Buy"), "the cart is open"),
        step(tap("Pay")),
    ]);

    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
    let checks = Arc::new(AtomicUsize::new(0));
    let (nl, _) = checked("verify-cp", false, &[], checks.clone());
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Checkpoints), "app").with_planner(nl);
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(*driver.taps.lock().unwrap(), ["Buy", "Pay"], "went on");
    assert_eq!(r.status, TestStatus::Failed);
    let at: Vec<(&str, VerifyOutcome)> = r
        .verification
        .iter()
        .map(|c| (c.at.as_str(), c.outcome))
        .collect();
    assert_eq!(
        at,
        [
            ("step 0", VerifyOutcome::Fails),
            ("final", VerifyOutcome::Fails)
        ]
    );
    assert!(r.error_message.unwrap().contains("the cart is open"));

    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
    let (nl, _) = checked("verify-strict", false, &[], checks.clone());
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app").with_planner(nl);
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(
        *driver.taps.lock().unwrap(),
        ["Buy"],
        "stopped at the check"
    );
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(r.steps[1].status, StepStatus::Skipped);
    assert_eq!(r.verification.len(), 1);

    // A per-test level wins over the suite's.
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("pay", "Pay", 50)]);
    let (nl, _) = checked("verify-test", false, &[], checks.clone());
    let mut off = test.clone();
    off.verify = Some(VerifyLevel::Off);
    let mut ex =
        TestExecutor::new(&driver, with_verify(VerifyLevel::Strict), "app").with_planner(nl);
    let r = ex.execute_test(&off, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed);
    assert!(r.verification.is_empty());
}

#[tokio::test]
async fn stale_cached_step_is_replanned_in_the_same_run() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("heal", calls.clone());
    let test = test_case(vec![natural("tap checkout")]);

    let v1 = ScriptDriver::new(vec![node("old_btn", "Checkout", 0)]);
    let mut ex = TestExecutor::new(&v1, config(300), "app").with_planner(nl.clone());
    ex.execute_test(&test, "d", "app").await.unwrap();

    // The app changed: the cached id no longer exists.
    let v2 = ScriptDriver::new(vec![node("new_btn", "Check out", 0)]);
    let mut ex = TestExecutor::new(&v2, config(300), "app").with_planner(nl);
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    assert_eq!(*v2.taps.lock().unwrap(), ["Check out"]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        r.warnings.iter().any(|w| w.contains("re-planned")),
        "{:?}",
        r.warnings
    );
}

/// A planner that gives these answers in turn (the last one repeats).
struct Answers(Mutex<Vec<PlannedStep>>, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl NaturalPlanner for Answers {
    async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
        self.1.fetch_add(1, Ordering::SeqCst);
        let mut a = self.0.lock().unwrap();
        Ok(if a.len() > 1 {
            a.remove(0)
        } else {
            a[0].clone()
        })
    }
}

fn answer(op: Op, element: Option<usize>) -> PlannedStep {
    PlannedStep {
        op,
        element,
        text: String::new(),
        direction: "none".into(),
        ms: 0,
        key: Default::default(),
    }
}

fn answering(
    name: &str,
    answers: Vec<PlannedStep>,
    calls: Arc<AtomicUsize>,
) -> Arc<NaturalResolver> {
    let dir = std::env::temp_dir().join(format!("vel-exec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(NaturalResolver::new(
        Box::new(Answers(Mutex::new(answers), calls)),
        dir.join("nl-cache.json"),
    ))
}

/// The cache key the executor uses for the first natural step of test
/// `t` on Android.
fn first_step<'a>(intent: &'a str, kind: NaturalKind) -> NaturalStep<'a> {
    NaturalStep {
        intent,
        kind,
        data: None,
        data_name: None,
        wait_until: None,
        intent_template: None,
        platform: Platform::Android,
        scope: "t#0",
        fresh: false,
        describe_data: false,
    }
}

#[tokio::test]
async fn a_cached_assertion_that_fails_is_not_replanned_onto_the_screen() {
    // D-17 / VEL-213: re-planning a failed check looked for whatever
    // was on screen and passed on another element.
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = answering(
        "assert-213",
        vec![answer(Op::AssertVisible, Some(0))],
        calls.clone(),
    );
    let check = |intent: &str| {
        step(Action::Natural {
            intent: intent.into(),
            kind: NaturalKind::Assert,
            data: None,
            data_name: None,
            wait_until: None,
            intent_template: None,
        })
    };
    let test = test_case(vec![check("the order total is shown")]);
    let v1 = ScriptDriver::new(vec![node("total", "Total $12", 0)]);
    let mut ex = TestExecutor::new(&v1, config(300), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    // The total is gone; something else is on screen.
    let v2 = ScriptDriver::new(vec![node("promo", "Free shipping", 0)]);
    let mut ex = TestExecutor::new(&v2, config(300), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "not re-planned");
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("not re-planned"), "{err}");
    // Dropped from the cache: the next attempt plans it afresh.
    assert!(nl
        .cached(&first_step("the order total is shown", NaturalKind::Assert))
        .await
        .is_none());
}

#[tokio::test]
async fn a_cached_action_without_its_effect_fails_without_a_second_action() {
    // D-17 / VEL-349: the re-plan ran a second, different action in the
    // same step after the first one had already acted.
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = answering("effect-349", vec![answer(Op::Tap, Some(1))], calls.clone());
    // Planned earlier, when tapping Buy changed the screen.
    nl.commit(
        &first_step("tap buy", NaturalKind::Act),
        Action::Tap {
            selector: Selector::Id("buy".into()),
        },
        None,
        Some(true),
    )
    .await;
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0), node("other", "Other", 50)]);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
    let r = ex
        .execute_test(&test_case(vec![natural("tap buy")]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert_eq!(*driver.taps.lock().unwrap(), ["Buy"], "one action only");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "not re-planned in the step"
    );
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("no visible effect"), "{err}");
    assert!(nl
        .cached(&first_step("tap buy", NaturalKind::Act))
        .await
        .is_none());
}

#[tokio::test]
async fn a_natural_goto_stays_on_the_current_origin() {
    // D-17: a cached (or planned) goto to another origin is refused
    // unless `nl.allowCrossOrigin: true`. (The step names the URL: a cached
    // goto to a URL the step doesn't contain never replays at all.)
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = answering("goto-origin", vec![answer(Op::NotFound, None)], calls);
    nl.commit(
        &first_step("open \"https://evil.example/deals\"", NaturalKind::Act),
        Action::Goto {
            url: "https://evil.example/deals".into(),
        },
        None,
        None,
    )
    .await;
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    driver.navigates.store(true, Ordering::SeqCst);
    *driver.page_url.lock().unwrap() = Some("https://shop.example/cart".into());
    let test = test_case(vec![natural("open \"https://evil.example/deals\"")]);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    let err = r.error_message.unwrap_or_default();
    assert!(err.contains("nl.allowCrossOrigin"), "{err}");
    // Allowed by the suite.
    nl.commit(
        &first_step("open \"https://evil.example/deals\"", NaturalKind::Act),
        Action::Goto {
            url: "https://evil.example/deals".into(),
        },
        None,
        None,
    )
    .await;
    let mut cfg = config(300);
    cfg.nl.allow_cross_origin = true;
    let mut ex = TestExecutor::new(&driver, cfg, "app").with_planner(nl);
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
}

/// Records the data each request carried.
struct DataSeen(Arc<Mutex<Vec<Option<String>>>>);

#[async_trait::async_trait]
impl NaturalPlanner for DataSeen {
    async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
        self.0.lock().unwrap().push(req.data.map(String::from));
        Ok(answer(Op::InputText, Some(0)))
    }
}

#[tokio::test]
async fn the_suite_data_policy_reaches_the_planner() {
    // D-30: `nl.dataPolicy: describe`.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dir = std::env::temp_dir().join(format!("vel-exec-{}-d30", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let nl = Arc::new(NaturalResolver::new(
        Box::new(DataSeen(seen.clone())),
        dir.join("nl-cache.json"),
    ));
    let driver = ScriptDriver::new(vec![node("name", "Name", 0)]);
    let typed = step(Action::Natural {
        intent: "type the customer name".into(),
        kind: NaturalKind::Act,
        data: Some("Ada Lovelace".into()),
        data_name: None,
        wait_until: None,
        intent_template: None,
    });
    let mut cfg = config(300);
    cfg.nl.data_policy = velocity_common::NlDataPolicy::Describe;
    let mut ex = TestExecutor::new(&driver, cfg, "app").with_planner(nl);
    let r = ex
        .execute_test(&test_case(vec![typed]), "d", "app")
        .await
        .unwrap();
    assert_eq!(r.status, TestStatus::Passed, "{:?}", r.error_message);
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty());
    for d in &seen {
        let d = d.as_deref().unwrap_or_default();
        assert!(d.starts_with("<value, 12 chars>"), "{d}");
    }
}

#[tokio::test]
async fn repeated_step_text_gets_separate_cache_entries() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("scope", calls.clone());
    let driver = ScriptDriver::new(vec![node("next", "Next", 0)]);
    let test = test_case(vec![natural("tap next"), natural("tap next")]);
    let mut ex = TestExecutor::new(&driver, config(1000), "app").with_planner(nl);
    ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "each occurrence is planned on its own screen"
    );
}

fn main_step<'a>(intent: &'a str, scope: &'a str) -> NaturalStep<'a> {
    NaturalStep {
        intent,
        kind: NaturalKind::Act,
        data: None,
        data_name: None,
        wait_until: None,
        intent_template: None,
        platform: Platform::Android,
        scope,
        fresh: false,
        describe_data: false,
    }
}

#[tokio::test]
async fn a_step_whose_wait_until_fails_is_not_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("until", calls.clone());
    // Tapping "Buy" never shows a receipt: the check can't be planned.
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let test = test_case(vec![step(Action::Natural {
        intent: "tap buy".into(),
        kind: NaturalKind::Act,
        data: None,
        data_name: None,
        wait_until: Some("the receipt is shown".into()),
        intent_template: None,
    })]);
    let mut ex = TestExecutor::new(&driver, config(300), "app").with_planner(nl.clone());
    let r = ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(r.status, TestStatus::Failed);
    assert!(
        nl.cached(&main_step("tap buy", "t#0")).await.is_none(),
        "a plan that didn't reach its waitUntil must not be replayed"
    );
}

#[tokio::test]
async fn a_retry_attempt_re_plans_instead_of_replaying() {
    let calls = Arc::new(AtomicUsize::new(0));
    let nl = resolver("retry", calls.clone());
    let driver = ScriptDriver::new(vec![node("buy", "Buy", 0)]);
    let test = test_case(vec![natural("tap buy")]);
    let mut ex = TestExecutor::new(&driver, config(2000), "app").with_planner(nl.clone());
    ex.execute_test(&test, "d", "app").await.unwrap();
    assert!(nl.cached(&main_step("tap buy", "t#0")).await.is_some());
    let planned = calls.load(Ordering::SeqCst);
    ex.set_retry_attempt(1);
    ex.execute_test(&test, "d", "app").await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        planned + 1,
        "the retry asked the model"
    );
}
