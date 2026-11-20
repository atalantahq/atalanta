//! The gates a written test passes before it is trusted (row 1.3:
//! REL-108, AUT-106). A test that replays is a replay cache, and a cache
//! must not lie: it has to fail when the app goes the wrong way, and it has
//! to check what the goal asked for.
//!
//! - **Burn-in:** every replay passes (five by default, the later ones
//!   with timing jitter so a race shows). Five is "burned in", not "proven
//!   stable": 95% confidence needs about 170 reruns (ICST 2021).
//! - **Wrong path:** on the device, the test runs up to its last
//!   navigating tap, taps a sibling of that element instead, and runs on.
//!   It must fail. If it passes, its assertions don't tell the screen it
//!   was meant to reach from a neighbouring one.
//! - **Goal text:** at least one assertion names text from the goal, so the
//!   test checks the goal and not only that some screen showed.
//!
//! (Each assertion explore writes already had to fail on the screen before
//! its step: the writer's own rule.)

use std::collections::HashMap;

use serde::Serialize;
use velocity_common::{
    Action, Element, Platform, PlatformDriver, Result, Selector, Step, TestCase, TestStatus,
    VelocityError,
};
use velocity_core::TestExecutor;

/// How the wrong-path replay went.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "result")]
pub enum WrongPath {
    /// The test failed after the sibling tap: good.
    Rejected { step: usize, tapped: String },
    /// The test still passed after tapping a sibling: it can't be trusted.
    Passed { step: usize, tapped: String },
    /// No check was possible (no tap, or the tap has no sibling).
    Skipped { why: String },
}

/// What [`check`] needs besides the driver, the YAML and the goal.
pub struct CheckOptions<'a> {
    pub device_id: &'a str,
    pub platform: velocity_common::Platform,
    /// `${NAME}` values (data and secrets) for the replays.
    pub overrides: &'a std::collections::HashMap<String, String>,
    /// The settings to replay with (a suite file's own `config:`).
    pub config: &'a velocity_common::SuiteConfig,
    /// Burn-in replays; must be >= 1 (0 is a Config error).
    pub replays: usize,
    /// Upper bound of the random pause before each step, on replays after the first.
    pub jitter_ms: u64,
}

/// The gates' verdict and the replays they ran.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub gates: GateReport,
    pub replay: crate::replay::ReplayReport,
}

/// What the gates found.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateReport {
    pub replays: usize,
    pub replays_passed: usize,
    pub jitter_ms: u64,
    pub wrong_path: WrongPath,
    /// An assertion that names goal text, when one does.
    pub goal_text: Option<String>,
    pub trusted: bool,
    /// Why not, one line each.
    pub reasons: Vec<String>,
}

impl GateReport {
    pub fn decide(
        replays: usize,
        replays_passed: usize,
        jitter_ms: u64,
        wrong_path: WrongPath,
        goal_text: Option<String>,
    ) -> Self {
        let mut reasons = Vec::new();
        if replays == 0 || replays_passed < replays {
            reasons.push(format!(
                "burn-in: {replays_passed} of {replays} replays passed"
            ));
        }
        if let WrongPath::Passed { step, tapped } = &wrong_path {
            reasons.push(format!(
                "wrong path: with step {} tapping {tapped} instead, the test still passed; its \
                 assertions don't tell the screen it should reach from a neighbouring one",
                step + 1
            ));
        }
        if goal_text.is_none() {
            reasons.push(
                "goal text: no assertion names anything from the goal; add one that checks \
                 what the goal asked for"
                    .into(),
            );
        }
        GateReport {
            replays,
            replays_passed,
            jitter_ms,
            wrong_path,
            goal_text,
            trusted: reasons.is_empty(),
            reasons,
        }
    }
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "into", "from", "that", "this", "then", "open", "tap", "go", "to",
    "a", "an", "of", "on", "in", "my", "it", "is", "be", "page", "screen", "app", "see", "check",
    "make", "sure", "show", "shows", "find", "set", "use",
];

fn words(s: &str) -> Vec<String> {
    crate::values::fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(w))
        .map(String::from)
        .collect()
}

/// The first assertion in `yaml`'s test whose text names a word of `goal`.
pub fn goal_text_assertion(yaml: &str, goal: &str) -> Option<String> {
    let suite = velocity_core::parse_suite_from_str(yaml).ok()?;
    let goal_words = words(goal);
    let test = suite.tests.first()?;
    test.steps.iter().find_map(|s| {
        let Action::AssertVisible { selector } = &s.action else {
            return None;
        };
        let text = selector_text(selector)?;
        let w = words(&text);
        w.iter()
            .any(|x| goal_words.contains(x))
            .then(|| format!("assertVisible {text:?}"))
    })
}

fn selector_text(sel: &Selector) -> Option<String> {
    match sel {
        Selector::Text(t)
        | Selector::TextContains(t)
        | Selector::AccessibilityId(t)
        | Selector::Id(t) => Some(t.clone()),
        _ => None,
    }
}

/// The wrong-path replay of `yaml`'s test: up to its last tap, a sibling
/// instead, then the rest. `overrides` fill its `${NAME}`s.
pub async fn wrong_path(
    driver: &dyn PlatformDriver,
    yaml: &str,
    device_id: &str,
    platform: Platform,
    overrides: &HashMap<String, String>,
    base: &velocity_common::SuiteConfig,
) -> Result<WrongPath> {
    let mut suite = velocity_core::parse_suite_from_str(yaml)?;
    velocity_core::env::interpolate_suite(&mut suite, overrides)?;
    let Some(test) = suite.tests.first().cloned() else {
        return Ok(WrongPath::Skipped {
            why: "no test".into(),
        });
    };
    // The last tap with a step after it: the one that should reach the
    // screen the test ends on.
    let Some(k) = test
        .steps
        .iter()
        .enumerate()
        .rev()
        .find(|(i, s)| matches!(s.action, Action::Tap { .. }) && *i + 1 < test.steps.len())
        .map(|(i, _)| i)
    else {
        return Ok(WrongPath::Skipped {
            why: "the test taps nothing before its end".into(),
        });
    };
    let Action::Tap { selector } = &test.steps[k].action else {
        unreachable!()
    };
    let mut config = base.clone();
    config.platform = Some(platform);
    config.artifacts.on_failure = false;
    let part = |name: &str, steps: Vec<Step>| TestCase {
        name: name.into(),
        steps,
        ..test.clone()
    };

    // Up to two neighbours, nearest first: the test must fail for each.
    let mut tried: Option<(usize, String)> = None;
    for attempt in 0..SIBLINGS_TRIED {
        let mut executor = TestExecutor::new(driver, config.clone(), &suite.app_id);
        let before = executor
            .execute_test(
                &part("wrong path: before", test.steps[..k].to_vec()),
                device_id,
                &suite.app_id,
            )
            .await?;
        if before.status != TestStatus::Passed {
            return Ok(WrongPath::Skipped {
                why: format!(
                    "the steps before the tap didn't replay: {}",
                    before.error_message.unwrap_or_default()
                ),
            });
        }
        let root = driver.get_hierarchy(device_id).await?;
        // A lookup that fails here is the gate's problem, not the test's:
        // report it as not checked instead of losing the whole report (a web
        // `aria-label` target the driver's direct lookup missed ended the
        // explore with no JSON at all; pilot, 2026-11-07).
        let target = match driver.find_element(device_id, selector).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(WrongPath::Skipped {
                    why: format!(
                        "step {} ({selector}) wasn't found to look for a neighbour: {e}",
                        k + 1
                    ),
                })
            }
        };
        let Some(sibling) = siblings_of(&root, &target).into_iter().nth(attempt) else {
            break;
        };
        let screen = driver
            .screen_size(device_id)
            .await
            .ok()
            .map(|(w, h)| velocity_common::Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            });
        let wrong = velocity_common::selector_build::robust_selector(
            &sibling,
            &root,
            screen.as_ref(),
            platform,
        );
        let tapped = wrong.to_string();
        let mut rest = vec![Step {
            action: Action::Tap { selector: wrong },
            ..test.steps[k].clone()
        }];
        rest.extend(test.steps[k + 1..].iter().cloned());
        let mut executor = TestExecutor::new(driver, config.clone(), &suite.app_id);
        let after = executor
            .execute_test(&part("wrong path: after", rest), device_id, &suite.app_id)
            .await?;
        if after.status == TestStatus::Passed {
            return Ok(WrongPath::Passed { step: k, tapped });
        }
        tried = Some((k, tapped));
    }
    Ok(match tried {
        Some((step, tapped)) => WrongPath::Rejected { step, tapped },
        None => WrongPath::Skipped {
            why: format!("step {} ({selector}) has no sibling to tap instead", k + 1),
        },
    })
}

/// The largest random pause before a step on burn-in replays after the
/// first, in ms: what `velocity explore` and `velocity gate` use by default.
pub const DEFAULT_JITTER_MS: u64 = 150;

/// Gate `yaml`'s one test against `goal`: replay it `replays` times (the
/// later ones with jitter, stopping at the first that fails), then the
/// wrong-path replay, then the goal-text rule. Explore's job decides with
/// the same [`decide_gates`] after its own replays.
pub async fn check(
    driver: &dyn PlatformDriver,
    yaml: &str,
    goal: &str,
    opts: &CheckOptions<'_>,
) -> Result<CheckResult> {
    if goal.trim().is_empty() {
        return Err(VelocityError::Config(
            "the gates need a goal: what the test is meant to show".into(),
        ));
    }
    let suite = velocity_core::parse_suite_from_str(yaml)?;
    if suite.tests.len() != 1 {
        return Err(VelocityError::Config(format!(
            "the gates check a file with one test; this one has {}",
            suite.tests.len()
        )));
    }
    if opts.replays == 0 {
        return Err(VelocityError::Config(
            "replays must be at least 1 for the gates".into(),
        ));
    }
    let mut replay = crate::replay::ReplayReport::default();
    for run in 1..=opts.replays {
        let r = crate::replay::replay_once(
            driver,
            yaml,
            opts.device_id,
            opts.platform,
            opts.overrides,
            opts.config,
            (run > 1).then_some((opts.jitter_ms, run as u64)),
        )
        .await?;
        let passed = r.passed;
        replay.runs.push(r);
        if !passed {
            break;
        }
    }
    let gates = decide_gates(driver, yaml, goal, replay.runs.len(), replay.passed(), opts).await?;
    Ok(CheckResult { gates, replay })
}

/// After the replays: the wrong-path replay (only for a test whose replays
/// all passed), the goal-text rule, and the verdict. Shared by [`check`]
/// and explore's job so the two can't drift. `opts.replays` is not read:
/// `runs` is how many replays actually ran.
pub(crate) async fn decide_gates(
    driver: &dyn PlatformDriver,
    yaml: &str,
    goal: &str,
    runs: usize,
    passed: usize,
    opts: &CheckOptions<'_>,
) -> Result<GateReport> {
    // The wrong path only means something for a test that replays.
    let wrong = if passed == runs && runs > 0 {
        wrong_path(
            driver,
            yaml,
            opts.device_id,
            opts.platform,
            opts.overrides,
            opts.config,
        )
        .await?
    } else {
        WrongPath::Skipped {
            why: "the replays didn't all pass".into(),
        }
    };
    let goal_text = goal_text_assertion(yaml, goal);
    Ok(GateReport::decide(
        runs,
        passed,
        opts.jitter_ms,
        wrong,
        goal_text,
    ))
}

/// Neighbours of the last tap the wrong-path replay tries.
const SIBLINGS_TRIED: usize = 2;

/// Tappable elements next to `target` (under its parent, else its
/// grandparent), of its type, with text of their own, nearest first.
fn siblings_of(root: &Element, target: &Element) -> Vec<Element> {
    fn same(a: &Element, b: &Element) -> bool {
        a.bounds == b.bounds && a.platform_id == b.platform_id && a.text == b.text
    }
    fn path<'a>(e: &'a Element, target: &Element, out: &mut Vec<&'a Element>) -> bool {
        if same(e, target) {
            return true;
        }
        for c in &e.children {
            if path(c, target, out) {
                out.push(e);
                return true;
            }
        }
        false
    }
    fn wordy(t: Option<&str>) -> bool {
        // Real text: a clock or a counter is not a place to go.
        t.is_some_and(|t| t.chars().filter(|c| c.is_alphabetic()).count() >= 2)
    }
    fn candidate(e: &Element, target: &Element) -> bool {
        e.visible
            && e.enabled
            && !same(e, target)
            && e.element_type == target.element_type
            && e.bounds.width > 0
            && e.bounds.height > 0
            && (wordy(e.text.as_deref()) || wordy(e.label.as_deref()))
            // Text and label together: an iOS cell has a label and no text,
            // so comparing text alone (None == None) ruled out every
            // neighbour and the gate was skipped on iOS (pilot, 2026-11-07).
            && (&e.text, &e.label) != (&target.text, &target.label)
    }
    fn collect<'a>(e: &'a Element, target: &Element, out: &mut Vec<&'a Element>) {
        if candidate(e, target) {
            out.push(e);
        }
        for c in &e.children {
            collect(c, target, out);
        }
    }
    let centre = |e: &Element| {
        (
            e.bounds.x as i64 + e.bounds.width as i64 / 2,
            e.bounds.y as i64 + e.bounds.height as i64 / 2,
        )
    };
    let (tx, ty) = centre(target);
    let mut ancestors = Vec::new();
    path(root, target, &mut ancestors);
    // The title and summary of one list row ("All apps" over "34 apps
    // installed") are one tap target: tapping either opens the same screen,
    // so the summary is no wrong path. Such a row is a parent at most about
    // three lines tall in which the other text sits above or below the
    // target; side by side (a nav bar's buttons) they are separate targets.
    // Picked, the summary made the gate call good Android tests weak
    // (pilot, 2026-11-07).
    let same_row = |parent: &Element, e: &Element| {
        let (p, t, b) = (&parent.bounds, &target.bounds, &e.bounds);
        let small = t.height > 0 && p.height > 0 && p.height <= 3 * t.height;
        let stacked = b.y >= t.y + t.height || t.y >= b.y + b.height;
        let overlap_x = b.x < t.x + t.width && t.x < b.x + b.width;
        small && stacked && overlap_x
    };
    // Up to five levels: an Android Settings title sits two levels inside
    // its row (row > text container > title), so the next row is only
    // found from the list three levels up.
    for parent in ancestors.iter().take(5) {
        let mut found = Vec::new();
        for c in parent
            .children
            .iter()
            .filter(|c| !path(c, target, &mut Vec::new()))
        {
            collect(c, target, &mut found);
        }
        found.retain(|e| !same_row(parent, e));
        if !found.is_empty() {
            found.sort_by_key(|e| {
                let (x, y) = centre(e);
                (x - tx).abs() + (y - ty).abs()
            });
            return found.into_iter().cloned().collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rows_summary_is_not_a_neighbour_but_the_next_row_is() {
        let text = |t: &str, y: i32, h: i32| Element {
            platform_id: "android:id/title".into(),
            text: Some(t.into()),
            element_type: "android.widget.TextView".into(),
            bounds: velocity_common::Rect {
                x: 40,
                y,
                width: 600,
                height: h,
            },
            visible: true,
            enabled: true,
            ..Default::default()
        };
        let row = |kids: Vec<Element>, y: i32| Element {
            element_type: "android.widget.LinearLayout".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y,
                width: 1080,
                height: 140,
            },
            children: kids,
            visible: true,
            enabled: true,
            ..Default::default()
        };
        let target = text("All apps", 20, 60);
        let list = Element {
            element_type: "androidx.recyclerview.widget.RecyclerView".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 1400,
            },
            children: vec![
                row(vec![target.clone(), text("34 apps installed", 80, 50)], 0),
                row(vec![text("Default apps", 160, 60)], 140),
            ],
            visible: true,
            ..Default::default()
        };
        let found = siblings_of(&list, &target);
        assert_eq!(
            found.first().and_then(|e| e.text.as_deref()),
            Some("Default apps"),
            "{found:?}"
        );
        // As Android Settings nests it: row > text container > title, so
        // the next row is three levels up.
        let wrap = |kids: Vec<Element>, y: i32| row(vec![row(kids, y)], y);
        let list = Element {
            element_type: "androidx.recyclerview.widget.RecyclerView".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 1400,
            },
            children: vec![
                wrap(vec![target.clone(), text("34 apps installed", 80, 50)], 0),
                wrap(vec![text("Default apps", 160, 60)], 140),
            ],
            visible: true,
            ..Default::default()
        };
        let found = siblings_of(&list, &target);
        assert_eq!(
            found.first().and_then(|e| e.text.as_deref()),
            Some("Default apps"),
            "{found:?}"
        );
    }

    #[test]
    fn label_only_cells_have_neighbours() {
        let cell = |label: &str, y: i32| Element {
            platform_id: "CameraVideoSettingsList".into(),
            label: Some(label.into()),
            element_type: "XCUIElementTypeCell".into(),
            bounds: velocity_common::Rect {
                x: 0,
                y,
                width: 400,
                height: 44,
            },
            visible: true,
            enabled: true,
            ..Default::default()
        };
        let target = cell("Record Video, 1080p at 30 FPS", 100);
        let root = Element {
            element_type: "XCUIElementTypeTable".into(),
            children: vec![target.clone(), cell("Record Slo-mo, 1080p at 240 FPS", 144)],
            visible: true,
            ..Default::default()
        };
        let found = siblings_of(&root, &target);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].label.as_deref(),
            Some("Record Slo-mo, 1080p at 240 FPS")
        );
    }

    #[test]
    fn an_assertion_must_name_goal_text() {
        let yaml = "appId: a\ntests:\n  - name: t\n    steps:\n      - tap: { text: Network }\n      - assertVisible: { text: \"Airplane mode\" }\n      - assertVisible: { text: \"Internet\" }\n";
        assert_eq!(
            goal_text_assertion(yaml, "open Network & internet").as_deref(),
            Some("assertVisible \"Internet\"")
        );
        assert!(goal_text_assertion(yaml, "turn on dark theme").is_none());
        // Stop words don't count.
        let only_page = "appId: a\ntests:\n  - name: t\n    steps:\n      - assertVisible: { text: \"Open page\" }\n";
        assert!(goal_text_assertion(only_page, "open the page").is_none());
    }

    #[test]
    fn the_report_names_each_failed_gate() {
        let ok = GateReport::decide(
            5,
            5,
            150,
            WrongPath::Rejected {
                step: 1,
                tapped: "x".into(),
            },
            Some("a".into()),
        );
        assert!(ok.trusted && ok.reasons.is_empty());
        let bad = GateReport::decide(
            5,
            4,
            150,
            WrongPath::Passed {
                step: 1,
                tapped: "text=Display".into(),
            },
            None,
        );
        assert!(!bad.trusted);
        assert_eq!(bad.reasons.len(), 3, "{:?}", bad.reasons);
        // A skipped wrong-path check is reported, not a failure.
        let skip = GateReport::decide(
            5,
            5,
            0,
            WrongPath::Skipped {
                why: "no tap".into(),
            },
            Some("a".into()),
        );
        assert!(skip.trusted);
    }
}
