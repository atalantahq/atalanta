//! Why a test flakes: where its passing and failing hunt rounds diverge,
//! the candidate causes ranked, and (`--suggest`) the YAML change for the
//! top one.
//!
//! No model is asked. The step the failing rounds fail at, their failure
//! kinds and the timings just before it localize most flaky root causes on
//! their own (DoeFL, Pei et al., ICST 2026: 47.5% Top-1, 82% Top-5), and a
//! concrete change with its reason is what developers act on (FlakyGuard,
//! Li et al., ASE 2025). Every rule compares the failing rounds with the
//! passing ones and says which numbers it read.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use atalanta_common::{
    Action, Selector, SourcePos, Step, StepSettle, StepStatus, TestResult, TestStatus, TestSuite,
};
use serde::Serialize;

use crate::history::FailureKind;

/// A failing step that took this share of its timeout ran it out.
const RAN_OUT: f64 = 0.9;
/// A previous step's settle this short (ms) in passing rounds is noise,
/// not a contrast.
const SETTLE_FLOOR_MS: u64 = 100;
/// Fewer failing rounds a cause explains, or passing rounds it compares
/// against, and it is only a lead: no fix is suggested from it.
const MIN_ROUNDS: usize = 2;

/// One run of a test in a hunt.
#[derive(Debug, Clone)]
pub struct Observation {
    pub round: u32,
    pub seed: u64,
    /// The round ran under `--stress`.
    pub stressed: bool,
    /// The tests that ran before this one in its round, on its device;
    /// `None` when not known (parallel hunts).
    pub ran_before: Option<Vec<String>>,
    pub result: TestResult,
}

impl Observation {
    fn passed(&self) -> bool {
        matches!(self.result.status, TestStatus::Passed | TestStatus::Retried)
    }

    /// A failure of the test: a lost device says nothing about it.
    fn failed(&self) -> bool {
        self.result.status == TestStatus::Failed && !self.result.infrastructure_failure
    }

    /// The step the test failed at: its last failed step (an optional
    /// step's failure comes before it).
    fn failed_step(&self) -> Option<usize> {
        self.result
            .steps
            .iter()
            .rposition(|s| s.status == StepStatus::Failed)
            .map(|i| self.result.steps[i].step_index)
    }

    fn step(&self, index: usize) -> Option<&atalanta_common::StepResult> {
        self.result.steps.iter().find(|s| s.step_index == index)
    }
}

/// Median, smallest and largest of some milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spread {
    pub n: usize,
    pub median_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
}

impl Spread {
    fn of(mut values: Vec<u64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let n = values.len();
        let median_ms = if n % 2 == 1 {
            values[n / 2]
        } else {
            (values[n / 2 - 1] + values[n / 2]) / 2
        };
        Some(Self {
            n,
            median_ms,
            min_ms: values[0],
            max_ms: values[n - 1],
        })
    }
}

impl std::fmt::Display for Spread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "median {} / max {}", ms(self.median_ms), ms(self.max_ms))
    }
}

/// A step's timings in the failing rounds and in the passing ones.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepTiming {
    pub step: usize,
    pub action: String,
    pub failing: Option<Spread>,
    pub passing: Option<Spread>,
}

/// Where the failing rounds fail.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailingStep {
    /// 0-based, as in the step results.
    pub step: usize,
    pub action: String,
    /// `file:line` of the step in the suite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Failing rounds that failed at this step (of all failing rounds).
    pub rounds: usize,
    /// The step's element timeout, when it waits for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Failing rounds in which the step ran (nearly) to that timeout.
    pub ran_out: usize,
}

/// How many failing rounds failed on one kind of failure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindCount {
    pub kind: &'static str,
    pub rounds: usize,
}

/// A candidate cause, with the rule that found it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    /// `timeout-too-tight`, `settle-too-short`, `order-dependent`,
    /// `needs-earlier-test`, `stress-only`.
    pub rule: &'static str,
    /// Share of the failing rounds the cause explains, weighted by the
    /// rule: the ranking key.
    pub score: f64,
    /// Too few rounds to act on: shown, but no fix is suggested from it.
    pub weak: bool,
    pub summary: String,
    #[serde(skip)]
    fix: Option<Fix>,
}

/// A change to the suite that addresses a cause.
#[derive(Debug, Clone, PartialEq)]
pub enum Fix {
    /// Set step `step`'s `timeoutMs`.
    StepTimeout { step: usize, ms: u64 },
    /// Wait for `selector` before step `before`.
    AssertVisibleBefore { before: usize, selector: Selector },
    /// Give step `step` a longer `settle.quietMs`.
    SettleQuiet { step: usize, quiet_ms: u64 },
    /// Launch with cleared app state in the first step.
    ClearState { step: usize },
    /// `isolated: true` on the test (its launch is shared under `batch`).
    Isolated,
    /// Run `on` first.
    DependsOn { on: String },
}

impl Fix {
    /// Apply to `test` in `suite`, in memory. False when the test or step
    /// isn't there.
    pub fn apply(&self, suite: &mut TestSuite, test: &str) -> bool {
        let Some(t) = suite.tests.iter_mut().find(|t| t.name == test) else {
            return false;
        };
        match self {
            Fix::StepTimeout { step, ms } => match t.steps.get_mut(*step) {
                Some(s) => s.timeout_ms = Some(*ms),
                None => return false,
            },
            Fix::AssertVisibleBefore { before, selector } => {
                if *before > t.steps.len() {
                    return false;
                }
                let source = t.steps.get(*before).and_then(|s| s.source.clone());
                t.steps.insert(
                    *before,
                    Step {
                        action: Action::AssertVisible {
                            selector: selector.clone(),
                        },
                        timeout_ms: None,
                        settle: None,
                        source,
                        modifiers: Default::default(),
                    },
                );
            }
            Fix::SettleQuiet { step, quiet_ms } => match t.steps.get_mut(*step) {
                Some(s) => {
                    let settle = s.settle.get_or_insert_with(StepSettle::default);
                    settle.quiet_ms = Some(*quiet_ms);
                }
                None => return false,
            },
            Fix::ClearState { step } => match t.steps.get_mut(*step).map(|s| &mut s.action) {
                Some(Action::LaunchApp { clear_state, .. }) => *clear_state = true,
                _ => return false,
            },
            Fix::Isolated => t.isolated = true,
            Fix::DependsOn { on } => {
                if !t.depends_on.contains(on) {
                    t.depends_on.push(on.clone());
                }
            }
        }
        true
    }
}

/// The YAML change for a test's top actionable cause.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub rule: &'static str,
    /// `file:line` the change goes at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// A patch-like snippet: context lines, `+` added, `-` removed.
    pub patch: String,
    pub why: String,
    /// What a re-run with the change applied in memory showed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
    #[serde(skip)]
    pub fix: Option<Fix>,
}

/// Where a flaky test's passing and failing rounds diverge, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnosis {
    pub test: String,
    pub passing_rounds: usize,
    pub failing_rounds: usize,
    /// Seeds of the failing rounds, to replay them.
    pub failing_seeds: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<FailingStep>,
    pub kinds: Vec<KindCount>,
    /// The step before the failing one: durations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_duration: Option<StepTiming>,
    /// The step before the failing one: settle times.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_settle: Option<StepTiming>,
    /// The failing step's own durations (passing: where it passed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_duration: Option<StepTiming>,
    /// Most likely first.
    pub causes: Vec<Cause>,
    /// `--suggest` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Suggestion>,
}

/// What [`diagnose`] needs besides the rounds.
#[derive(Debug, Default, Clone)]
pub struct Context<'a> {
    /// The suite the rounds ran (steps, timeouts, `batch`).
    pub suite: Option<&'a TestSuite>,
    /// Each test's `- name:` line.
    pub test_sources: Option<&'a HashMap<String, SourcePos>>,
    /// Work out a [`Suggestion`].
    pub suggest: bool,
}

/// Diagnose `test` from its rounds; `None` unless it both passed and
/// failed (a lost device's round counts as neither).
pub fn diagnose(test: &str, rounds: &[Observation], cx: &Context) -> Option<Diagnosis> {
    let passing: Vec<&Observation> = rounds.iter().filter(|o| o.passed()).collect();
    let failing: Vec<&Observation> = rounds.iter().filter(|o| o.failed()).collect();
    if passing.is_empty() || failing.is_empty() {
        return None;
    }
    let case = cx
        .suite
        .and_then(|s| s.tests.iter().find(|t| t.name == test));

    let mut kinds: BTreeMap<FailureKind, usize> = BTreeMap::new();
    for o in &failing {
        *kinds.entry(FailureKind::of_result(&o.result)).or_default() += 1;
    }
    let mut kinds: Vec<KindCount> = kinds
        .into_iter()
        .map(|(k, rounds)| KindCount {
            kind: k.label(),
            rounds,
        })
        .collect();
    kinds.sort_by_key(|k| std::cmp::Reverse(k.rounds));

    // The step most failing rounds fail at (the first such on a tie).
    let mut at: BTreeMap<usize, usize> = BTreeMap::new();
    for o in &failing {
        if let Some(i) = o.failed_step() {
            *at.entry(i).or_default() += 1;
        }
    }
    let step = at
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(&i, _)| i);
    let at_step: Vec<&Observation> = match step {
        Some(k) => failing
            .iter()
            .copied()
            .filter(|o| o.failed_step() == Some(k))
            .collect(),
        None => vec![],
    };

    let mut d = Diagnosis {
        test: test.to_string(),
        passing_rounds: passing.len(),
        failing_rounds: failing.len(),
        failing_seeds: failing.iter().map(|o| o.seed).collect(),
        failing_step: None,
        kinds,
        previous_duration: None,
        previous_settle: None,
        step_duration: None,
        causes: vec![],
        suggestion: None,
    };
    let mut causes = Vec::new();

    if let Some(k) = step {
        let def = case.and_then(|c| c.steps.get(k));
        let failed = at_step[0].step(k);
        let action = failed.map(|s| s.action_name.clone()).unwrap_or_default();
        let timeout_ms = def.and_then(|s| {
            action_selector(&s.action).map(|_| {
                s.timeout_ms
                    .unwrap_or(cx.suite.map_or(10_000, |s| s.config.sync.timeout_ms))
            })
        });
        let fail_ms = |o: &&Observation| o.step(k).map(|s| millis(s.duration));
        let ran_out = timeout_ms.map_or(0, |t| {
            at_step
                .iter()
                .filter_map(fail_ms)
                .filter(|&d| d as f64 >= t as f64 * RAN_OUT)
                .count()
        });
        let source = failed
            .and_then(|s| s.source.clone())
            .or_else(|| def.and_then(|s| s.source.clone()));
        d.failing_step = Some(FailingStep {
            step: k,
            action: action.clone(),
            source: source.as_ref().map(ToString::to_string),
            rounds: at_step.len(),
            timeout_ms,
            ran_out,
        });
        let timing = |i: usize, read: &dyn Fn(&atalanta_common::StepResult) -> Option<u64>| {
            let name = at_step[0].step(i).map(|s| s.action_name.clone());
            let failing = Spread::of(
                at_step
                    .iter()
                    .filter_map(|o| o.step(i).and_then(read))
                    .collect(),
            );
            let passing = Spread::of(
                passing
                    .iter()
                    .filter_map(|o| {
                        o.step(i)
                            .filter(|s| s.status == StepStatus::Passed)
                            .and_then(read)
                    })
                    .collect(),
            );
            (failing.is_some() || passing.is_some()).then(|| StepTiming {
                step: i,
                action: name.unwrap_or_default(),
                failing,
                passing,
            })
        };
        let duration = |s: &atalanta_common::StepResult| Some(millis(s.duration));
        let settle = |s: &atalanta_common::StepResult| s.settle_ms;
        d.step_duration = timing(k, &duration);
        if k > 0 {
            d.previous_duration = timing(k - 1, &duration);
            d.previous_settle = timing(k - 1, &settle);
        }
        let place = source
            .as_ref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let kind = d.kinds[0].kind;

        // The failing step runs out its timeout, and the passing rounds
        // needed more than 2/3 of it: 1.5× their worst covers them.
        if let (Some(t), Some(pass)) =
            (timeout_ms, d.step_duration.as_ref().and_then(|s| s.passing))
        {
            let wanted = round_up(pass.max_ms.saturating_mul(3) / 2, 500);
            if ran_out > 0 && wanted > t {
                causes.push(Cause {
                    rule: "timeout-too-tight",
                    score: ran_out as f64 / failing.len() as f64,
                    weak: ran_out < MIN_ROUNDS || pass.n < MIN_ROUNDS,
                    summary: format!(
                        "step {} {action}{place} ran to its {} timeout in {ran_out} of {} \
                         failing rounds, while passing rounds needed up to {} of it \
                         → the timeout is too tight",
                        k + 1,
                        ms(t),
                        failing.len(),
                        ms(pass.max_ms),
                    ),
                    fix: Some(Fix::StepTimeout {
                        step: k,
                        ms: wanted,
                    }),
                });
            }
        }

        // The step before settled far sooner in the failing rounds: the
        // screen was still changing when the failing step looked at it.
        if let Some(StepTiming {
            failing: Some(f),
            passing: Some(p),
            action: prev,
            ..
        }) = &d.previous_settle
        {
            if p.min_ms >= SETTLE_FLOOR_MS && f.max_ms.saturating_mul(2) <= p.min_ms {
                let fix = match case
                    .and_then(|c| c.steps.get(k))
                    .filter(|s| acts_on_element(&s.action))
                    .and_then(|s| action_selector(&s.action))
                {
                    Some(sel) => Fix::AssertVisibleBefore {
                        before: k,
                        selector: sel.clone(),
                    },
                    None => Fix::SettleQuiet {
                        step: k - 1,
                        quiet_ms: round_up(p.min_ms, 50),
                    },
                };
                causes.push(Cause {
                    rule: "settle-too-short",
                    score: 0.9 * f.n as f64 / failing.len() as f64,
                    weak: f.n < MIN_ROUNDS || p.n < MIN_ROUNDS,
                    summary: format!(
                        "fails {kind} at step {} {action}{place} after step {k} {prev} \
                         settled in {} in failing rounds vs {}+ in passing ones \
                         → the screen hadn't finished changing",
                        k + 1,
                        ms(f.max_ms),
                        ms(p.min_ms),
                    ),
                    fix: Some(fix),
                });
            }
        }
    }

    causes.extend(order_causes(&passing, &failing, case, cx));

    // Every failing round was stressed, and the unstressed ones all passed.
    let unstressed: Vec<&Observation> = rounds
        .iter()
        .filter(|o| !o.stressed && (o.passed() || o.failed()))
        .collect();
    if failing.iter().all(|o| o.stressed) && !unstressed.is_empty() {
        causes.push(Cause {
            rule: "stress-only",
            score: 0.8,
            weak: failing.len() < MIN_ROUNDS || unstressed.len() < MIN_ROUNDS,
            summary: format!(
                "fails only under --stress ({} of {} stressed rounds failed, none of {} \
                 unstressed) → timing-sensitive under CPU load; see the timings above",
                failing.len(),
                rounds
                    .iter()
                    .filter(|o| o.stressed && (o.passed() || o.failed()))
                    .count(),
                unstressed.len(),
            ),
            fix: None,
        });
    }

    // Highest score first; the order rules were pushed in breaks ties.
    causes.sort_by(|a, b| b.score.total_cmp(&a.score));
    if cx.suggest {
        d.suggestion = causes
            .iter()
            .find(|c| !c.weak && c.fix.is_some())
            .and_then(|c| suggestion(c, test, case, cx));
    }
    d.causes = causes;
    Some(d)
}

/// Order rules: another test ran before this one in every failing round
/// and in no passing one (it leaves state behind), or in every passing
/// round and no failing one (this test relies on its state).
fn order_causes(
    passing: &[&Observation],
    failing: &[&Observation],
    case: Option<&atalanta_common::TestCase>,
    cx: &Context,
) -> Vec<Cause> {
    let known = |os: &[&Observation]| -> Option<Vec<BTreeSet<String>>> {
        os.iter()
            .map(|o| {
                o.ran_before
                    .as_ref()
                    .map(|b| b.iter().cloned().collect::<BTreeSet<_>>())
            })
            .collect()
    };
    let (Some(pass), Some(fail)) = (known(passing), known(failing)) else {
        return vec![];
    };
    let in_all = |sets: &[BTreeSet<String>]| -> BTreeSet<String> {
        let mut it = sets.iter();
        let first = it.next().cloned().unwrap_or_default();
        it.fold(first, |acc, s| acc.intersection(s).cloned().collect())
    };
    let in_none = |sets: &[BTreeSet<String>], name: &String| sets.iter().all(|s| !s.contains(name));
    let seeds = |os: &[&Observation]| {
        os.iter()
            .map(|o| o.seed.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let weak = failing.len() < MIN_ROUNDS || passing.len() < MIN_ROUNDS;
    let mut causes = Vec::new();

    let polluters: Vec<String> = in_all(&fail)
        .into_iter()
        .filter(|n| in_none(&pass, n))
        .collect();
    if !polluters.is_empty() {
        let fix = case.map(|c| match c.steps.first().map(|s| &s.action) {
            _ if cx.suite.is_some_and(|s| s.config.batch) => Fix::Isolated,
            Some(Action::LaunchApp {
                clear_state: false, ..
            }) => Fix::ClearState { step: 0 },
            _ => Fix::Isolated,
        });
        causes.push(Cause {
            rule: "order-dependent",
            // Several tests that separate the rounds equally well: each
            // is less likely the one.
            score: 0.9 / polluters.len() as f64,
            weak,
            summary: format!(
                "fails only after {} ran earlier in the round (failing seeds {}; it ran \
                 after {} in none of {} passing rounds) → order-dependent: that test \
                 leaves state behind",
                quoted_list(&polluters),
                seeds(failing),
                if polluters.len() == 1 { "it" } else { "them" },
                passing.len(),
            ),
            fix,
        });
    }

    let providers: Vec<String> = in_all(&pass)
        .into_iter()
        .filter(|n| in_none(&fail, n))
        .filter(|n| case.is_none_or(|c| !c.depends_on.contains(n)))
        .collect();
    if !providers.is_empty() {
        causes.push(Cause {
            rule: "needs-earlier-test",
            score: 0.9 / providers.len() as f64,
            weak,
            summary: format!(
                "passes only after {} ran earlier in the round (passing seeds {}; failing \
                 seeds {} ran it later or not at all) → it relies on state that test leaves",
                quoted_list(&providers),
                seeds(passing),
                seeds(failing),
            ),
            fix: (providers.len() == 1).then(|| Fix::DependsOn {
                on: providers[0].clone(),
            }),
        });
    }
    causes
}

/// The patch for `cause`'s fix, read against the suite file when it can
/// be (else the step written back from the parsed suite).
fn suggestion(
    cause: &Cause,
    test: &str,
    case: Option<&atalanta_common::TestCase>,
    cx: &Context,
) -> Option<Suggestion> {
    let fix = cause.fix.clone()?;
    let case = case?;
    let step_line = |i: usize| -> (Option<String>, String) {
        let s = case.steps.get(i);
        let at = s.and_then(|s| s.source.as_ref());
        let text = at
            .and_then(source_line)
            .unwrap_or_else(|| s.map_or_else(String::new, |s| format!("      {}", step_yaml(s))));
        (at.map(ToString::to_string), text)
    };
    let test_line = || -> (Option<String>, String) {
        let at = cx.test_sources.and_then(|m| m.get(test));
        let text = at
            .and_then(source_line)
            .unwrap_or_else(|| format!("  - name: {test}"));
        (at.map(ToString::to_string), text)
    };
    let (at, patch, why) = match &fix {
        Fix::StepTimeout { step, ms: t } => {
            let (at, line) = step_line(*step);
            let pad = key_indent(&line);
            let mut patch = format!("   {line}\n");
            if let Some(old) = case.steps[*step].timeout_ms {
                patch.push_str(&format!("-  {pad}timeoutMs: {old}\n"));
            }
            patch.push_str(&format!("+  {pad}timeoutMs: {t}"));
            (
                at,
                patch,
                "1.5× the slowest passing round, rounded up to 0.5 s: the step waits as \
                 long as it took when it passed, with room to spare"
                    .to_string(),
            )
        }
        Fix::AssertVisibleBefore { before, selector } => {
            let (at, line) = step_line(*before);
            let indent: String = line.chars().take_while(|c| *c == ' ').collect();
            let fields = atalanta_core::executor::selector_fields(selector);
            (
                at,
                format!("+  {indent}- assertVisible: {{ {fields} }}\n   {line}"),
                format!(
                    "wait for {{ {fields} }} to be on screen before step {} acts on it, \
                     instead of trusting the step before to have settled",
                    before + 1
                ),
            )
        }
        Fix::SettleQuiet { step, quiet_ms } => {
            let (at, line) = step_line(*step);
            let pad = key_indent(&line);
            (
                at,
                format!("   {line}\n+  {pad}settle: {{ quietMs: {quiet_ms} }}"),
                format!(
                    "step {} counts the screen as settled only after {quiet_ms} quiet ms, \
                     the least it took in passing rounds",
                    step + 1
                ),
            )
        }
        Fix::ClearState { step } => {
            let (at, line) = step_line(*step);
            let indent: String = line.chars().take_while(|c| *c == ' ').collect();
            let launch = match &case.steps[*step].action {
                Action::LaunchApp { app_id, .. } if !app_id.is_empty() => {
                    format!("launchApp: {{ appId: {app_id}, clearState: true }}")
                }
                _ => "launchApp: { clearState: true }".to_string(),
            };
            (
                at,
                format!("-  {line}\n+  {indent}- {launch}"),
                "start from a cleared app, so what an earlier test left behind can't \
                 reach this one"
                    .to_string(),
            )
        }
        Fix::Isolated => {
            let (at, line) = test_line();
            let pad = key_indent(&line);
            (
                at,
                format!("   {line}\n+  {pad}isolated: true"),
                "never share a launch with the test before it, so its state can't leak in"
                    .to_string(),
            )
        }
        Fix::DependsOn { on } => {
            let (at, line) = test_line();
            let pad = key_indent(&line);
            let mut all = case.depends_on.clone();
            all.push(on.clone());
            (
                at,
                format!("   {line}\n+  {pad}dependsOn: [{}]", quoted_list(&all)),
                format!(
                    "always run {on:?} first, the state this test needs; or better, set \
                     that state up in this test"
                ),
            )
        }
    };
    Some(Suggestion {
        rule: cause.rule,
        at,
        patch,
        why: format!("{} ({})", why, cause.summary),
        verified: None,
        fix: Some(fix),
    })
}

/// The verdict of a re-run with the fix applied: `before` = (passed,
/// failed) in the hunt, `after` the same with the change.
pub fn verdict(before: (usize, usize), after: (usize, usize)) -> String {
    let (bp, bf) = before;
    let (ap, af) = after;
    let n = ap + af;
    if n == 0 {
        return "not re-run (the test didn't run)".into();
    }
    if af > 0 {
        return format!("still failed {af} of {n} rounds with the change: not the whole cause");
    }
    let p_fail = bf as f64 / (bp + bf).max(1) as f64;
    let chance = (1.0 - p_fail).powi(n as i32) * 100.0;
    format!(
        "no failure in {n} rounds with the change (it failed {bf} of {} before); \
         {n} clean rounds would happen by chance {chance:.0}% of the time without it, \
         so this suggests the fix works but doesn't prove it",
        bp + bf
    )
}

/// The terminal lines for one diagnosis (under its FLAKY line).
pub fn render(d: &Diagnosis, suggest: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(f) = &d.failing_step {
        let kinds: Vec<String> = d
            .kinds
            .iter()
            .map(|k| format!("{} ×{}", k.kind, k.rounds))
            .collect();
        out.push(format!(
            "diverges at step {} {}{}: {} of {} failing rounds fail there ({})",
            f.step + 1,
            f.action,
            f.source
                .as_ref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default(),
            f.rounds,
            d.failing_rounds,
            kinds.join(", ")
        ));
    }
    for (what, t) in [
        ("settle", &d.previous_settle),
        ("took", &d.previous_duration),
    ] {
        if let Some(t) = t {
            out.push(format!(
                "step {} {} {what}: {}",
                t.step + 1,
                t.action,
                compare(t)
            ));
        }
    }
    if let (Some(f), Some(t)) = (&d.failing_step, &d.step_duration) {
        let mut line = format!("step {} {} took: {}", t.step + 1, t.action, compare(t));
        if let Some(timeout) = f.timeout_ms {
            line.push_str(&format!(
                "; ran out its {} timeout in {} of {} failing rounds",
                ms(timeout),
                f.ran_out,
                f.rounds
            ));
        }
        out.push(line);
    }
    if d.causes.is_empty() {
        out.push("no rule matched: compare the traces of a passing and a failing round".into());
    } else {
        out.push("likely causes:".into());
        for (i, c) in d.causes.iter().enumerate() {
            out.push(format!(
                "  {}. [{}] {}{}",
                i + 1,
                c.rule,
                c.summary,
                if c.weak {
                    " (few rounds: a lead, not a verdict)"
                } else {
                    ""
                }
            ));
        }
    }
    if suggest {
        match &d.suggestion {
            Some(s) => {
                out.push(format!(
                    "suggested change ({}){}:",
                    s.rule,
                    s.at.as_ref()
                        .map(|a| format!(" at {a}"))
                        .unwrap_or_default()
                ));
                out.extend(s.patch.lines().map(|l| format!("  {l}")));
                out.push(format!("  why: {}", s.why));
                if let Some(v) = &s.verified {
                    out.push(format!("  re-run: {v}"));
                }
            }
            None => out.push(
                "no suggested change: the evidence is too weak (a cause needs at least 2 \
                 failing and 2 passing rounds); hunt with more rounds"
                    .into(),
            ),
        }
    }
    out
}

fn compare(t: &StepTiming) -> String {
    let side = |s: &Option<Spread>| s.map_or_else(|| "n/a".to_string(), |s| s.to_string());
    format!("failing {}, passing {}", side(&t.failing), side(&t.passing))
}

/// `1.2 s`, `40 ms`.
fn ms(v: u64) -> String {
    if v >= 1000 {
        format!("{:.1} s", v as f64 / 1000.0)
    } else {
        format!("{v} ms")
    }
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn round_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to).saturating_mul(to)
}

fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The line `pos` points at, when the suite file can be read.
fn source_line(pos: &SourcePos) -> Option<String> {
    if pos.file.is_empty() {
        return None;
    }
    let text = std::fs::read_to_string(&pos.file).ok()?;
    text.lines()
        .nth(pos.line.checked_sub(1)?)
        .map(|l| l.trim_end().to_string())
}

/// Indent of a key beside the item a `- ` line starts.
fn key_indent(line: &str) -> String {
    let lead = line.chars().take_while(|c| *c == ' ').count();
    " ".repeat(lead + 2)
}

/// A step written back as YAML, for when the suite file can't be read.
fn step_yaml(s: &Step) -> String {
    let name = action_label(&s.action);
    match action_selector(&s.action) {
        Some(sel) => format!(
            "- {name}: {{ {} }}",
            atalanta_core::executor::selector_fields(sel)
        ),
        None => format!("- {name}"),
    }
}

fn action_label(a: &Action) -> &'static str {
    match a {
        Action::Tap { .. } => "tap",
        Action::DoubleTap { .. } => "doubleTap",
        Action::LongPress { .. } => "longPress",
        Action::InputText { .. } => "inputText",
        Action::ClearText { .. } => "clearText",
        Action::AssertVisible { .. } => "assertVisible",
        Action::AssertNotVisible { .. } => "assertNotVisible",
        Action::AssertText { .. } => "assertText",
        Action::ScrollUntilVisible { .. } => "scrollUntilVisible",
        Action::LaunchApp { .. } => "launchApp",
        _ => "step",
    }
}

/// The element a step waits for.
fn action_selector(a: &Action) -> Option<&Selector> {
    match a {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => Some(selector),
        _ => None,
    }
}

/// A step that acts on its element (an `assertVisible` before it waits
/// for it); an assertion already waits.
fn acts_on_element(a: &Action) -> bool {
    matches!(
        a,
        Action::Tap { .. }
            | Action::DoubleTap { .. }
            | Action::LongPress { .. }
            | Action::InputText { .. }
            | Action::ClearText { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::StepResult;
    use std::time::Duration;

    const YAML: &str = "appId: com.example
config:
  sync:
    timeoutMs: 5000
tests:
  - name: Seed
    steps:
      - launchApp
  - name: Checkout
    steps:
      - launchApp
      - tap: { text: \"Next\" }
      - tap: { text: \"Pay\" }
        timeoutMs: 5000
";

    fn suite() -> TestSuite {
        atalanta_core::parser::parse_suite_from_str(YAML).unwrap()
    }

    /// A step: (duration ms, settle ms); the last one of a failing round
    /// fails with `error`.
    fn round(
        n: u32,
        steps: &[(u64, Option<u64>)],
        error: Option<&str>,
        before: &[&str],
    ) -> Observation {
        let names = ["launchApp", "tap", "tap"];
        let last = steps.len() - 1;
        let steps = steps
            .iter()
            .enumerate()
            .map(|(i, &(d, settle_ms))| StepResult {
                step_index: i,
                action_name: names[i].into(),
                status: if error.is_some() && i == last {
                    StepStatus::Failed
                } else {
                    StepStatus::Passed
                },
                duration: Duration::from_millis(d),
                screenshot: None,
                error_message: None,
                resource_delta: None,
                settle_ms,
                source: Some(SourcePos::new("", 10 + i, 9)),
            })
            .collect();
        let mut result = TestResult::failed("Checkout", String::new(), Duration::ZERO);
        result.steps = steps;
        match error {
            Some(e) => result.error_message = Some(e.into()),
            None => {
                result.status = TestStatus::Passed;
                result.error_message = None;
            }
        }
        Observation {
            round: n,
            seed: u64::from(n) + 1,
            stressed: false,
            ran_before: Some(before.iter().map(|s| s.to_string()).collect()),
            result,
        }
    }

    const NOT_FOUND: Option<&str> = Some("line 12: tap: Element not found: text=\"Pay\"");

    fn cx(suite: &TestSuite) -> Context<'_> {
        Context {
            suite: Some(suite),
            test_sources: None,
            suggest: true,
        }
    }

    fn rules(d: &Diagnosis) -> Vec<&'static str> {
        d.causes.iter().map(|c| c.rule).collect()
    }

    #[test]
    fn a_step_that_runs_out_a_tight_timeout_gets_one_from_the_passing_rounds() {
        let s = suite();
        let rounds = [
            round(
                0,
                &[(900, Some(300)), (300, Some(300)), (3000, None)],
                None,
                &[],
            ),
            round(
                1,
                &[(900, Some(300)), (300, Some(300)), (5010, None)],
                NOT_FOUND,
                &[],
            ),
            round(
                2,
                &[(900, Some(300)), (300, Some(300)), (4100, None)],
                None,
                &[],
            ),
            round(
                3,
                &[(900, Some(300)), (300, Some(300)), (5004, None)],
                NOT_FOUND,
                &[],
            ),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        let f = d.failing_step.as_ref().unwrap();
        assert_eq!((f.step, f.rounds, f.ran_out), (2, 2, 2));
        assert_eq!(f.timeout_ms, Some(5000));
        assert_eq!(f.source.as_deref(), Some("line 12"));
        assert_eq!(d.kinds[0].kind, "not-found");
        assert_eq!(rules(&d)[0], "timeout-too-tight");
        assert!(d.causes[0].summary.contains("needed up to 4.1 s"), "{d:#?}");
        let s = d.suggestion.as_ref().unwrap();
        assert_eq!(s.fix, Some(Fix::StepTimeout { step: 2, ms: 6500 }));
        assert!(
            s.patch.contains("-          timeoutMs: 5000"),
            "{}",
            s.patch
        );
        assert!(
            s.patch.contains("+          timeoutMs: 6500"),
            "{}",
            s.patch
        );
    }

    #[test]
    fn a_previous_step_that_settled_early_suggests_waiting_for_the_target() {
        let s = suite();
        let rounds = [
            round(
                0,
                &[(900, Some(300)), (400, Some(320)), (500, None)],
                None,
                &[],
            ),
            round(
                1,
                &[(900, Some(300)), (100, Some(40)), (600, None)],
                NOT_FOUND,
                &[],
            ),
            round(
                2,
                &[(900, Some(300)), (500, Some(410)), (500, None)],
                None,
                &[],
            ),
            round(
                3,
                &[(900, Some(300)), (100, Some(45)), (600, None)],
                NOT_FOUND,
                &[],
            ),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert_eq!(rules(&d), ["settle-too-short"]);
        assert!(
            d.causes[0]
                .summary
                .contains("settled in 45 ms in failing rounds vs 320 ms+"),
            "{}",
            d.causes[0].summary
        );
        let settle = d.previous_settle.as_ref().unwrap();
        assert_eq!(settle.failing.unwrap().median_ms, 42);
        assert_eq!(settle.passing.unwrap().max_ms, 410);
        let s = d.suggestion.as_ref().unwrap();
        assert!(
            s.patch
                .starts_with("+        - assertVisible: { text: \"Pay\" }"),
            "{}",
            s.patch
        );
        // Not a tight timeout: the failing step never ran it out.
        assert_eq!(d.failing_step.unwrap().ran_out, 0);
    }

    #[test]
    fn failing_only_after_another_test_is_order_dependent() {
        let s = suite();
        let t = [(900, Some(300)), (300, Some(300)), (500, None)];
        let rounds = [
            round(0, &t, None, &[]),
            round(1, &t, NOT_FOUND, &["Seed"]),
            round(2, &t, None, &[]),
            round(3, &t, NOT_FOUND, &["Seed"]),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert_eq!(rules(&d), ["order-dependent"]);
        assert!(d.causes[0].summary.contains("\"Seed\""));
        assert!(d.causes[0].summary.contains("failing seeds 2, 4"));
        let s = d.suggestion.unwrap();
        assert_eq!(s.fix, Some(Fix::ClearState { step: 0 }));
        assert!(s
            .patch
            .contains("+        - launchApp: { clearState: true }"));
    }

    #[test]
    fn passing_only_after_another_test_suggests_depends_on() {
        let s = suite();
        let t = [(900, Some(300)), (300, Some(300)), (500, None)];
        let rounds = [
            round(0, &t, None, &["Seed"]),
            round(1, &t, NOT_FOUND, &[]),
            round(2, &t, None, &["Seed"]),
            round(3, &t, NOT_FOUND, &[]),
        ];
        let sources = HashMap::from([("Checkout".to_string(), SourcePos::new("", 9, 5))]);
        let cx = Context {
            test_sources: Some(&sources),
            ..cx(&s)
        };
        let d = diagnose("Checkout", &rounds, &cx).unwrap();
        assert_eq!(rules(&d), ["needs-earlier-test"]);
        let s = d.suggestion.unwrap();
        assert_eq!(s.at.as_deref(), Some("line 9"));
        assert!(
            s.patch.contains("+      dependsOn: [\"Seed\"]"),
            "{}",
            s.patch
        );
    }

    #[test]
    fn failing_only_in_stressed_rounds_is_stress_only() {
        let s = suite();
        let t = [(900, Some(300)), (300, Some(300)), (500, None)];
        let mut rounds = [
            round(0, &t, NOT_FOUND, &[]),
            round(1, &t, None, &[]),
            round(2, &t, NOT_FOUND, &[]),
            round(3, &t, None, &[]),
            round(4, &t, None, &[]),
        ];
        for (i, r) in rounds.iter_mut().enumerate() {
            r.stressed = i % 2 == 0;
            r.ran_before = None;
        }
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert_eq!(rules(&d), ["stress-only"]);
        assert!(d.causes[0]
            .summary
            .contains("2 of 3 stressed rounds failed, none of 2 unstressed"));
        assert_eq!(d.suggestion, None, "stress-only has no YAML change");
    }

    #[test]
    fn causes_rank_by_the_share_of_failing_rounds_they_explain() {
        let s = suite();
        // Both rounds ran out the timeout (1.0) and both came after Seed
        // (0.9): the timeout ranks first and is the suggestion.
        let rounds = [
            round(0, &[(900, None), (300, None), (4000, None)], None, &[]),
            round(
                1,
                &[(900, None), (300, None), (5000, None)],
                NOT_FOUND,
                &["Seed"],
            ),
            round(2, &[(900, None), (300, None), (4500, None)], None, &[]),
            round(
                3,
                &[(900, None), (300, None), (5000, None)],
                NOT_FOUND,
                &["Seed"],
            ),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert_eq!(rules(&d), ["timeout-too-tight", "order-dependent"]);
        assert!(d.causes[0].score > d.causes[1].score);
        assert_eq!(d.suggestion.unwrap().rule, "timeout-too-tight");
    }

    #[test]
    fn one_failing_round_is_a_lead_not_a_suggestion() {
        let s = suite();
        let rounds = [
            round(
                0,
                &[(900, Some(300)), (400, Some(320)), (500, None)],
                None,
                &[],
            ),
            round(
                1,
                &[(900, Some(300)), (100, Some(40)), (600, None)],
                NOT_FOUND,
                &[],
            ),
            round(
                2,
                &[(900, Some(300)), (500, Some(410)), (500, None)],
                None,
                &[],
            ),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert_eq!(rules(&d), ["settle-too-short"]);
        assert!(d.causes[0].weak);
        assert_eq!(d.suggestion, None);
        let text = render(&d, true).join("\n");
        assert!(text.contains("a lead, not a verdict"), "{text}");
        assert!(text.contains("no suggested change"), "{text}");
    }

    #[test]
    fn no_rule_without_a_contrast_and_no_diagnosis_without_both_outcomes() {
        let s = suite();
        let t = [(900, Some(300)), (300, Some(300)), (500, None)];
        let rounds = [
            round(0, &t, None, &[]),
            round(1, &t, NOT_FOUND, &[]),
            round(2, &t, None, &[]),
            round(3, &t, NOT_FOUND, &[]),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        assert!(d.causes.is_empty(), "{:?}", d.causes);
        assert!(render(&d, false)
            .iter()
            .any(|l| l.contains("no rule matched")));
        assert!(diagnose("Checkout", &rounds[..1], &cx(&s)).is_none());
        // A lost device says nothing about the test.
        let mut lost = round(1, &t, Some("adb connection lost"), &[]);
        lost.result.infrastructure_failure = true;
        assert!(diagnose("Checkout", &[rounds[0].clone(), lost], &cx(&s)).is_none());
    }

    #[test]
    fn fixes_apply_in_memory() {
        let mut s = suite();
        assert!(Fix::StepTimeout { step: 2, ms: 6500 }.apply(&mut s, "Checkout"));
        assert!(Fix::AssertVisibleBefore {
            before: 2,
            selector: Selector::Text("Pay".into()),
        }
        .apply(&mut s, "Checkout"));
        assert!(Fix::DependsOn { on: "Seed".into() }.apply(&mut s, "Checkout"));
        assert!(Fix::ClearState { step: 0 }.apply(&mut s, "Checkout"));
        assert!(!Fix::Isolated.apply(&mut s, "Missing"));
        let t = &s.tests[1];
        assert!(matches!(t.steps[2].action, Action::AssertVisible { .. }));
        assert_eq!(t.steps[3].timeout_ms, Some(6500));
        assert_eq!(t.depends_on, ["Seed"]);
        assert!(matches!(
            t.steps[0].action,
            Action::LaunchApp {
                clear_state: true,
                ..
            }
        ));
    }

    #[test]
    fn the_rerun_verdict_is_honest_about_what_few_rounds_show() {
        let gone = verdict((3, 2), (5, 0));
        assert!(gone.contains("no failure in 5 rounds"), "{gone}");
        // 0.6^5 ≈ 8%: clean by luck now and then.
        assert!(gone.contains("by chance 8%"), "{gone}");
        assert!(gone.contains("doesn't prove it"), "{gone}");
        assert_eq!(
            verdict((3, 2), (4, 1)),
            "still failed 1 of 5 rounds with the change: not the whole cause"
        );
    }

    #[test]
    fn the_diagnosis_serializes_camel_case_without_the_fix() {
        let s = suite();
        let rounds = [
            round(
                0,
                &[(900, Some(300)), (300, Some(300)), (3000, None)],
                None,
                &[],
            ),
            round(
                1,
                &[(900, Some(300)), (300, Some(300)), (5010, None)],
                NOT_FOUND,
                &[],
            ),
            round(
                2,
                &[(900, Some(300)), (300, Some(300)), (4100, None)],
                None,
                &[],
            ),
            round(
                3,
                &[(900, Some(300)), (300, Some(300)), (5004, None)],
                NOT_FOUND,
                &[],
            ),
        ];
        let d = diagnose("Checkout", &rounds, &cx(&s)).unwrap();
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["failingStep"]["step"], 2);
        assert_eq!(v["failingStep"]["timeoutMs"], 5000);
        assert_eq!(v["failingStep"]["ranOut"], 2);
        assert_eq!(v["kinds"][0]["kind"], "not-found");
        assert_eq!(v["stepDuration"]["passing"]["maxMs"], 4100);
        assert_eq!(v["previousSettle"]["failing"]["medianMs"], 300);
        assert_eq!(v["causes"][0]["rule"], "timeout-too-tight");
        assert_eq!(v["causes"][0]["weak"], false);
        assert_eq!(v["suggestion"]["rule"], "timeout-too-tight");
        assert!(v["suggestion"].get("fix").is_none());
        assert_eq!(v["failingSeeds"], serde_json::json!([2, 4]));
    }
}
