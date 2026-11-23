//! Change-based test selection that stays safe by default.
//!
//! Most UI test runs find nothing — at Google 1.23% of test targets ever
//! caught a real breakage (Memon et al. 2017) — so running only what a change
//! can affect pays. But a mapping is never complete, so every selection also
//! keeps the tests history says are risky. A rule like this caught 70–80% of
//! failures while running a third of the suites (Elbaum et al. 2014):
//! - tests mapped to a changed file (directly, or through a flow they use);
//! - tests that failed or flaked recently;
//! - new tests (no history) and tests not run for a week.
//!
//! Some changes affect everything (manifests, plists, resources, build files,
//! the suite itself): then everything runs. With no mapping, everything runs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atalanta_common::{Action, Flow, Step, TestCase};
use atalanta_core::impact::{file_matches_pattern, ImpactMapping, Unmapped};

use crate::history::{HistoryOutcome, TestHistory};

/// Changes to these mean every test is affected.
const RUN_ALL_PATTERNS: &[&str] = &[
    "AndroidManifest.xml",
    "**/AndroidManifest.xml",
    "**/res/**",
    "*.plist",
    "**/*.plist",
    "build.gradle",
    "build.gradle.kts",
    "**/build.gradle",
    "**/build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "Podfile",
    "Podfile.lock",
    "package.json",
    "pubspec.yaml",
    "**/*.xcconfig",
    "**/project.pbxproj",
];

/// Tests that haven't run for this long always run.
const STALE_AFTER_SECS: u64 = 7 * 24 * 3600;
/// How many recent runs count as "recently failed or flaked".
const RECENT_RUNS: usize = 5;

#[derive(Debug, Default)]
pub struct Selection {
    /// Names of the tests to run, with why each was picked.
    pub reasons: BTreeMap<String, String>,
    /// Set when the change affects everything.
    pub run_all: Option<String>,
    /// The tests in `reasons` picked only for what the local history says
    /// (failed recently, new, stale). Each CI shard has its own history,
    /// so these are applied after sharding (R-1); the rest follow from
    /// git and the impact map alone and are the same on every shard.
    pub from_history: BTreeSet<String>,
    /// How many tests were picked only because nothing maps them.
    pub unmapped: usize,
}

/// The reason given for a test picked because nothing maps it.
pub const UNMAPPED: &str = "unmapped";

impl Selection {
    pub fn selects(&self, test: &str) -> bool {
        self.run_all.is_some() || self.reasons.contains_key(test)
    }
}

/// Whether the changed file `f` (repo-relative) is the suite file. Whole
/// path components only: a plain `ends_with` made a change to
/// `my_suite.yaml` or `old/suite.yaml.bak`… rerun all of `suite.yaml`
/// (VEL-184). The suite path may be relative to a subdirectory, or
/// absolute.
fn is_suite_file(f: &str, suite_file: &str) -> bool {
    let s = suite_file.trim_start_matches("./");
    let f = f.trim_start_matches("./");
    f == s || f.ends_with(&format!("/{s}")) || s.ends_with(&format!("/{f}"))
}

/// Decide which `tests` to run for `changed` files.
pub fn select(
    tests: &[TestCase],
    flows_used: &HashMap<String, HashSet<String>>,
    mapping: &ImpactMapping,
    changed: &[String],
    suite_file: &str,
    history: &TestHistory,
    now: u64,
) -> Selection {
    let mut sel = Selection::default();
    if mapping.flows.is_empty() && mapping.tests.is_empty() {
        sel.run_all = Some("no impact mapping".into());
        return sel;
    }
    if let Some(f) = changed.iter().find(|f| {
        is_suite_file(f, suite_file)
            || RUN_ALL_PATTERNS
                .iter()
                .map(|p| p.to_string())
                .chain(mapping.run_all.iter().cloned())
                .any(|p| file_matches_pattern(f, &p))
    }) {
        sel.run_all = Some(format!("{f} affects every test"));
        return sel;
    }

    let hits = |patterns: &[String]| -> Option<String> {
        changed
            .iter()
            .find(|f| patterns.iter().any(|p| file_matches_pattern(f, p)))
            .cloned()
    };
    for test in tests {
        let name = &test.name;
        let git_reason = if let Some(f) = mapping.tests.get(name).and_then(|p| hits(p)) {
            Some(format!("covers {f}"))
        } else if let Some((flow, f)) = flows_used.get(name).and_then(|flows| {
            flows
                .iter()
                .find_map(|fl| mapping.flows.get(fl).and_then(|p| hits(p)).map(|f| (fl, f)))
        }) {
            Some(format!("flow {flow} covers {f}"))
        } else {
            None
        };
        if let Some(r) = git_reason {
            sel.reasons.insert(name.clone(), r);
            continue;
        }
        // Nothing maps it, so nothing says a change can't affect it: it
        // runs (D-24; Ekstazi selects what it has no dependency data for).
        let mapped = mapping.tests.contains_key(name)
            || flows_used
                .get(name)
                .is_some_and(|flows| flows.iter().any(|f| mapping.flows.contains_key(f)));
        if !mapped && mapping.impact.unmapped == Unmapped::Run {
            sel.reasons.insert(name.clone(), UNMAPPED.to_string());
            sel.unmapped += 1;
            continue;
        }
        let reason = if let Some(recent) = history.outcomes.get(name).map(|o| {
            o.iter()
                .rev()
                .take(RECENT_RUNS)
                .any(|x| matches!(x, HistoryOutcome::Fail | HistoryOutcome::Flaky))
        }) {
            recent.then(|| "failed or flaked recently".to_string())
        } else {
            Some("new (no history)".to_string())
        };
        let reason = reason.or_else(|| {
            let last = history.last_run.get(name).copied().unwrap_or(0);
            (now.saturating_sub(last) > STALE_AFTER_SECS)
                .then(|| "not run in the last 7 days".to_string())
        });
        if let Some(r) = reason {
            sel.reasons.insert(name.clone(), r);
            sel.from_history.insert(name.clone());
        }
    }
    sel
}

/// Which flows each test runs directly (before flows are inlined). Flows
/// can run flows: use [`flows_used_in`] with the suite's flows to see them.
#[deprecated(note = "use flows_used_in: flows_used cannot see a flow run by another flow")]
pub fn flows_used(tests: &[TestCase]) -> HashMap<String, HashSet<String>> {
    flows_used_in(tests, &[])
}

/// Which flows each test uses, directly or through the flows those flows
/// run, however deep (before flows are inlined). A change mapped to an
/// inner flow then selects every test that reaches it; counting only the
/// flows a test names left those tests out of the run.
pub fn flows_used_in(tests: &[TestCase], flows: &[Flow]) -> HashMap<String, HashSet<String>> {
    let by_id: HashMap<&str, &Flow> = flows.iter().map(|f| (f.id.as_str(), f)).collect();
    fn walk(steps: &[Step], by_id: &HashMap<&str, &Flow>, out: &mut HashSet<String>) {
        for s in steps {
            if let Action::RunFlow { flow_id, .. } = &s.action {
                // `insert` is the cycle guard: a flow is expanded once.
                if out.insert(flow_id.clone()) {
                    if let Some(flow) = by_id.get(flow_id.as_str()) {
                        walk(&flow.steps, by_id, out);
                    }
                }
            }
        }
    }
    tests
        .iter()
        .map(|t| {
            let mut used = HashSet::new();
            walk(&t.steps, &by_id, &mut used);
            (t.name.clone(), used)
        })
        .collect()
}

/// Patterns in `mapping` that match none of the repository's `files`
/// (`git ls-files`), as (owner, pattern): `test login`, `flow checkout`,
/// `runAll`. Such a pattern never selects anything — a typo or a folder
/// that moved — so every change to what it meant to cover is missed.
pub fn dead_patterns(mapping: &ImpactMapping, files: &[String]) -> Vec<(String, String)> {
    let mut owners: Vec<(String, &Vec<String>)> = Vec::new();
    owners.extend(mapping.tests.iter().map(|(t, p)| (format!("test {t}"), p)));
    owners.extend(mapping.flows.iter().map(|(f, p)| (format!("flow {f}"), p)));
    owners.push(("runAll".to_string(), &mapping.run_all));
    let mut dead: Vec<(String, String)> = owners
        .into_iter()
        .flat_map(|(owner, patterns)| {
            patterns
                .iter()
                .filter(|p| !files.iter().any(|f| file_matches_pattern(f, p)))
                .map(move |p| (owner.clone(), p.clone()))
        })
        .collect();
    dead.sort();
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::update;
    use atalanta_common::{TestResult, TestStatus};

    fn test(name: &str, flow: Option<&str>) -> TestCase {
        TestCase {
            name: name.into(),
            tags: vec![],
            isolated: false,
            depends_on: vec![],
            steps: flow
                .map(|f| {
                    vec![Step {
                        action: Action::RunFlow {
                            flow_id: f.into(),
                            env: Default::default(),
                        },
                        timeout_ms: None,
                        settle: None,
                        source: None,
                        modifiers: Default::default(),
                    }]
                })
                .unwrap_or_default(),
            description: None,
            verify: None,
        }
    }

    /// `profile` and `search` are unmapped; these tests leave them to
    /// the history rules (`unmapped: skip`, the behaviour before D-24).
    fn mapping() -> ImpactMapping {
        let mut m = ImpactMapping::empty();
        m.tests.insert("login".into(), vec!["src/auth/".into()]);
        m.flows
            .insert("checkout_flow".into(), vec!["src/cart/**".into()]);
        m.impact.unmapped = Unmapped::Skip;
        m
    }

    // D-24: by default a test nothing maps runs, whatever its history; a
    // mapped test with clean history and no matching change doesn't.
    #[test]
    fn unmapped_tests_run_by_default() {
        let tests = vec![
            test("login", None),
            test("checkout", Some("checkout_flow")),
            test("profile", None),
        ];
        let mut m = mapping();
        m.impact = Default::default();
        let sel = select(
            &tests,
            &flows_used_in(&tests, &[]),
            &m,
            &["README.md".to_string()],
            "suite.yaml",
            &all_green(),
            crate::history::now_secs(),
        );
        assert_eq!(
            sel.reasons.get("profile").map(String::as_str),
            Some(UNMAPPED)
        );
        assert_eq!(sel.unmapped, 1);
        assert!(!sel.selects("login"));
        assert!(!sel.selects("checkout"));
        // Not history: every shard picks it the same way.
        assert!(!sel.from_history.contains("profile"));
        let parsed: ImpactMapping =
            serde_yaml::from_str("tests:\n  login: [src/]\nimpact:\n  unmapped: skip\n").unwrap();
        assert_eq!(parsed.impact.unmapped, Unmapped::Skip);
        let parsed: ImpactMapping = serde_yaml::from_str("tests:\n  login: [src/]\n").unwrap();
        assert_eq!(parsed.impact.unmapped, Unmapped::Run);
    }

    fn history_with(names: &[(&str, TestStatus)]) -> TestHistory {
        let mut h = TestHistory::default();
        for (n, s) in names {
            let r = TestResult {
                status: *s,
                ..TestResult::failed(n, String::new(), std::time::Duration::ZERO)
            };
            update(&mut h, &[r]);
        }
        h
    }

    fn pick(changed: &[&str], history: &TestHistory) -> Selection {
        let tests = vec![
            test("login", None),
            test("checkout", Some("checkout_flow")),
            test("profile", None),
            test("search", None),
        ];
        let changed: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
        select(
            &tests,
            &flows_used_in(&tests, &[]),
            &mapping(),
            &changed,
            "suite.yaml",
            history,
            crate::history::now_secs(),
        )
    }

    fn all_green() -> TestHistory {
        history_with(&[
            ("login", TestStatus::Passed),
            ("checkout", TestStatus::Passed),
            ("profile", TestStatus::Passed),
            ("search", TestStatus::Passed),
        ])
    }

    #[test]
    fn maps_changes_to_tests_directly_and_through_flows() {
        let s = pick(&["src/auth/Login.kt", "src/cart/Cart.kt"], &all_green());
        assert!(s.run_all.is_none());
        let names: Vec<_> = s.reasons.keys().map(String::as_str).collect();
        assert_eq!(names, ["checkout", "login"]);
    }

    #[test]
    fn no_changes_still_runs_risky_tests_not_nothing() {
        let mut h = all_green();
        update(
            &mut h,
            &[TestResult::failed(
                "profile",
                "x".into(),
                std::time::Duration::ZERO,
            )],
        );
        let s = pick(&[], &h);
        assert_eq!(s.reasons.keys().collect::<Vec<_>>(), ["profile"]);
        assert!(s.reasons["profile"].contains("failed"));
    }

    #[test]
    fn new_and_stale_tests_always_run() {
        let mut h = history_with(&[
            ("login", TestStatus::Passed),
            ("checkout", TestStatus::Passed),
        ]);
        h.last_run.insert("checkout".into(), 0); // long ago
        let s = pick(&["README.md"], &h);
        assert!(s.reasons["profile"].contains("new"));
        assert!(s.reasons["checkout"].contains("7 days"));
        assert!(!s.selects("login"));
    }

    // VEL-005: a change mapped to a flow that another flow runs selects
    // the tests that reach it through that flow (and cycles terminate).
    #[test]
    fn a_change_in_a_nested_flow_selects_tests_that_reach_it() {
        let flow = |id: &str, calls: &[&str]| Flow {
            id: id.into(),
            steps: calls
                .iter()
                .map(|c| test("_", Some(c)).steps.remove(0))
                .collect(),
        };
        let flows = vec![
            flow("login_flow", &["checkout_flow"]),
            flow("checkout_flow", &["login_flow"]),
            flow("pay_flow", &["checkout_flow"]),
        ];
        let tests = vec![test("checkout", Some("pay_flow")), test("other", None)];
        let used = flows_used_in(&tests, &flows);
        assert_eq!(used["checkout"].len(), 3, "{used:?}");
        let mut m = ImpactMapping::empty();
        m.flows
            .insert("login_flow".into(), vec!["src/auth/".into()]);
        m.tests.insert("other".into(), vec!["src/other/".into()]);
        let h = history_with(&[
            ("checkout", TestStatus::Passed),
            ("other", TestStatus::Passed),
        ]);
        let sel = select(
            &tests,
            &used,
            &m,
            &["src/auth/Login.kt".to_string()],
            "suite.yaml",
            &h,
            crate::history::now_secs(),
        );
        assert!(sel.selects("checkout"), "{sel:?}");
        assert!(sel.reasons["checkout"].contains("login_flow"));
        assert!(!sel.selects("other"));
    }

    // VEL-184: another file whose name merely ends like the suite's.
    #[test]
    fn a_file_named_like_the_suite_is_not_the_suite() {
        for f in ["tests/my_suite.yaml", "other-suite.yaml"] {
            assert!(pick(&[f], &all_green()).run_all.is_none(), "{f}");
        }
        assert!(is_suite_file("mobile/tests/app.yaml", "./tests/app.yaml"));
        assert!(is_suite_file("tests/app.yaml", "/repo/tests/app.yaml"));
        assert!(!is_suite_file("tests/myapp.yaml", "tests/app.yaml"));
    }

    #[test]
    fn global_files_and_the_suite_itself_run_everything() {
        for f in [
            "app/src/main/AndroidManifest.xml",
            "app/src/main/res/values/strings.xml",
            "ios/App/Info.plist",
            "tests/suite.yaml",
        ] {
            let s = pick(&[f], &all_green());
            assert!(s.run_all.is_some(), "{f} should run all");
        }
    }

    #[test]
    fn mapping_patterns_that_match_no_file_are_reported() {
        let mut m = mapping();
        m.tests.insert("pay".into(), vec!["src/paymnet/**".into()]);
        m.run_all.push("config/flags.json".into());
        let files: Vec<String> = ["src/auth/login.ts", "src/cart/cart.ts", "README.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            dead_patterns(&m, &files),
            vec![
                ("runAll".to_string(), "config/flags.json".to_string()),
                ("test pay".to_string(), "src/paymnet/**".to_string()),
            ]
        );
    }
}
