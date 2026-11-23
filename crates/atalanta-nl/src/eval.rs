// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Planner consistency eval (pass^k).
//!
//! Caching makes replays deterministic, which also hides how consistent the
//! planner itself is: a step that resolves correctly once may resolve
//! differently next time it's re-planned (after a UI change, or on a new
//! machine with an empty cache). Agent benchmarks show this gap is large —
//! τ-bench's GPT-4o drops below 25% at pass^8 — so it's measured, not assumed.
//!
//! Every fresh plan saves a trace: the step, the screen it was planned on and
//! the action chosen. [`evaluate_with`] re-plans each trace `n` times from
//! scratch and reports:
//! - **pass@1** — share of single re-plans that reproduce the recorded action,
//!   with a 95% Wilson interval clustered by trace (see [`wilson_clustered`]):
//!   a trace's re-plans share its screen and intent, so they aren't
//!   independent draws;
//! - **pass^k** — the unbiased estimate (τ-bench) of the chance that k
//!   independent re-plans all reproduce it: mean over traces of
//!   C(c, k) / C(n, k), with c of the n re-plans matching. Drawing exactly
//!   k samples (`n == k`) is the estimator's highest-variance form;
//! - **consistent** — share where every answer agrees with the others (even
//!   if they differ from the recording, e.g. after a prompt change);
//! - not-found and error counts, and per-call latency.
//!
//! A re-plan "reproduces" the recording when it does the same thing to the
//! same node: the same op and value, and selectors that pick the same element
//! on the trace's screen (`id:` and `accessibilityId:` on one button match).
//! A planner mistake is a failed answer: grounding rejecting every answer (an
//! assertion for an action, an unasked wait, an over-long `ms`, a disabled or
//! unknown element), or a reply that is unparseable, refused or out of
//! tokens. It counts as a miss in every rate and never as self-consistent
//! (review MJ-1). Only service errors (transport, auth, outage) are not
//! answers: they are counted apart, left out of every rate and checked
//! against `--max-error-rate`, so k identical errors no longer read as
//! "self-consistent" (VEL-039).
//!
//! [`compare`] runs one task set across several models (`atalanta nl-eval
//! --model anthropic:… --model openai:…`) and adds tokens, cost and wall
//! time per model; [`Comparison::recommend`] names the model to make the
//! default. [`builtin_tasks`] is a fixed task set (recorded screens and the
//! expected operation for each) shipped with the crate, so a comparison is
//! reproducible offline except for the model calls themselves.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atalanta_common::selector_match::find_all_ref;
use atalanta_common::{
    Action, AtalantaError, Element, NaturalKind, Platform, Rect, Result, Selector,
};
use serde::{Deserialize, Serialize};

use crate::planner::{NaturalPlanner, Usage};
use crate::resolver::{plan_step_outcome, NaturalStep, PlanFailure, StepKeys};

const TRACE_VERSION: u32 = 1;

/// One planning call's inputs and result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub version: u32,
    pub platform: Platform,
    pub kind: NaturalKind,
    pub intent: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub wait_until: Option<String>,
    #[serde(default)]
    pub scope: String,
    pub screen: Element,
    /// The rect the driver matched selectors in, when known. Re-plans ground
    /// against it so they count twins as the run did (VEL-018).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_rect: Option<Rect>,
    pub action: Action,
}

impl Trace {
    fn step(&self) -> NaturalStep<'_> {
        NaturalStep {
            intent: &self.intent,
            kind: self.kind,
            data: self.data.as_deref(),
            data_name: None,
            wait_until: self.wait_until.as_deref(),
            intent_template: None,
            platform: self.platform,
            scope: &self.scope,
            fresh: true,
            describe_data: false,
        }
    }
}

/// Save a trace for `step` into `dir` (one file per step key; the latest
/// screen wins). Written to a temporary file and renamed into place, so a
/// run killed mid-write leaves no half trace (VEL-328).
pub fn write_trace(
    dir: &Path,
    step: &NaturalStep<'_>,
    screen: &Element,
    screen_rect: Option<&Rect>,
    action: &Action,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let trace = Trace {
        version: TRACE_VERSION,
        platform: step.platform,
        kind: step.kind,
        intent: step.intent.to_string(),
        data: step.data.map(String::from),
        wait_until: step.wait_until.map(String::from),
        scope: step.scope.to_string(),
        screen: screen.clone(),
        screen_rect: screen_rect.copied(),
        action: action.clone(),
    };
    let name = step.cache_key();
    let mut json = serde_json::to_value(&trace).map_err(std::io::Error::other)?;
    atalanta_common::redact::redact_value(&mut json);
    let path = dir.join(format!("{name}.json"));
    let tmp = dir.join(format!(".{name}.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, json.to_string())?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The traces in a directory, and how many files were skipped and why.
#[derive(Debug, Default)]
pub struct LoadedTraces {
    pub traces: Vec<Trace>,
    /// Files that couldn't be read.
    pub unreadable: usize,
    /// Files that aren't a trace (bad JSON, missing fields).
    pub malformed: usize,
    /// Traces written by another format version.
    pub other_version: usize,
}

impl LoadedTraces {
    pub fn skipped(&self) -> usize {
        self.unreadable + self.malformed + self.other_version
    }

    /// "N traces skipped (…)", or `None` when nothing was.
    pub fn skip_summary(&self) -> Option<String> {
        if self.skipped() == 0 {
            return None;
        }
        let parts: Vec<String> = [
            (self.unreadable, "unreadable"),
            (self.malformed, "not a valid trace"),
            (self.other_version, "another format version"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, why)| format!("{n} {why}"))
        .collect();
        Some(format!(
            "{} trace file(s) skipped ({})",
            self.skipped(),
            parts.join(", ")
        ))
    }
}

/// Load every trace in `dir`, oldest format versions skipped.
pub fn load_traces(dir: &Path) -> Result<Vec<Trace>> {
    Ok(load_traces_counted(dir)?.traces)
}

/// [`load_traces`], also counting the files skipped by reason: they were
/// dropped silently, and a directory of old or broken traces read as "no
/// traces, run a suite first" (VEL-328).
pub fn load_traces_counted(dir: &Path) -> Result<LoadedTraces> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AtalantaError::Config(format!("no traces at {}: {e}", dir.display())))?;
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut out = LoadedTraces::default();
    for p in &paths {
        let Ok(text) = std::fs::read_to_string(p) else {
            out.unreadable += 1;
            continue;
        };
        let version = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("version").and_then(|v| v.as_u64()));
        if version.is_some_and(|v| v != u64::from(TRACE_VERSION)) {
            out.other_version += 1;
            continue;
        }
        match serde_json::from_str::<Trace>(&text) {
            Ok(t) => out.traces.push(t),
            Err(_) => out.malformed += 1,
        }
    }
    Ok(out)
}

/// Outcome for one trace across k re-plans.
#[derive(Debug, Clone, Serialize)]
pub struct TraceResult {
    pub intent: String,
    pub scope: String,
    /// Re-plans that reproduced the recorded action.
    pub matched: usize,
    /// Distinct answers seen, errors not counted (1 = fully consistent).
    pub distinct: usize,
    pub not_found: usize,
    /// Re-plans the model answered unusably (a planner failure, a miss).
    pub rejected: usize,
    /// Re-plans that failed on the service (transport, auth, outage).
    pub errors: usize,
    /// One example answer that differed from the recording, for debugging.
    pub divergent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub k: usize,
    /// Re-plans per trace.
    pub samples: usize,
    pub traces: usize,
    pub pass_at_1: f64,
    /// 95% Wilson interval of pass@1, clustered by trace
    /// ([`wilson_clustered`]).
    pub pass_at_1_ci: (f64, f64),
    pub pass_hat_k: f64,
    /// Traces in pass^k: those with at least k answers that weren't errors.
    pub scored_traces: usize,
    pub consistent: f64,
    /// Calls the model answered unusably; scored as misses.
    pub rejected: usize,
    /// Calls that failed on the service (transport, auth, outage).
    pub errors: usize,
    /// `errors` over all calls.
    pub error_rate: f64,
    pub calls: usize,
    pub p50_ms: u128,
    pub p95_ms: u128,
    pub results: Vec<TraceResult>,
}

/// Re-plan every trace `k` times with `planner` (no cache) and score the
/// answers against the recorded actions.
pub async fn evaluate(planner: &dyn NaturalPlanner, traces: &[Trace], k: usize) -> EvalReport {
    evaluate_with(planner, traces, k, k).await
}

/// Re-plan every trace `samples` times and estimate pass^k from them
/// (`samples >= k`; more samples, a less variable estimate).
pub async fn evaluate_with(
    planner: &dyn NaturalPlanner,
    traces: &[Trace],
    samples: usize,
    k: usize,
) -> EvalReport {
    let k = k.max(1);
    let samples = samples.max(k);
    let mut results = Vec::with_capacity(traces.len());
    let mut latencies: Vec<Duration> = Vec::new();

    for trace in traces {
        let mut answers: Vec<String> = Vec::with_capacity(k);
        let (mut matched, mut not_found, mut rejected, mut errors) = (0, 0, 0, 0);
        let mut divergent = None;
        for _ in 0..samples {
            let start = Instant::now();
            let outcome = plan_step_outcome(
                planner,
                &trace.step(),
                &trace.screen,
                trace.screen_rect.as_ref(),
                None,
            )
            .await;
            latencies.push(start.elapsed());
            let mut same = false;
            let answer = match outcome {
                Ok(action) => {
                    same = same_target(&action, &trace.action, trace);
                    Some((answer_key(&action, trace), format!("{action:?}")))
                }
                Err(PlanFailure::NotFound(_)) => {
                    not_found += 1;
                    Some(("notFound".to_string(), "notFound".to_string()))
                }
                // A miss, and never the same answer as another one.
                Err(PlanFailure::Rejected(e)) => {
                    rejected += 1;
                    Some((format!("rejected#{rejected}"), format!("rejected: {e}")))
                }
                Err(PlanFailure::Service(e)) => {
                    errors += 1;
                    if divergent.is_none() {
                        divergent = Some(format!("error: {e}"));
                    }
                    None
                }
            };
            if same {
                matched += 1;
            } else if let Some((_, shown)) = &answer {
                if divergent
                    .as_deref()
                    .is_none_or(|d| d.starts_with("error: "))
                {
                    divergent = Some(shown.clone());
                }
            }
            answers.extend(answer.map(|(key, _)| key));
        }
        answers.sort();
        answers.dedup();
        results.push(TraceResult {
            intent: trace.intent.clone(),
            scope: trace.scope.clone(),
            matched,
            distinct: answers.len(),
            not_found,
            rejected,
            errors,
            divergent,
        });
    }

    let n = results.len().max(1) as f64;
    let errors: usize = results.iter().map(|r| r.errors).sum();
    let rejected: usize = results.iter().map(|r| r.rejected).sum();
    let answered: usize = results.iter().map(|r| samples - r.errors).sum();
    let matched: usize = results.iter().map(|r| r.matched).sum();
    // pass^k over traces with enough answers; errors aren't samples.
    let per_trace: Vec<f64> = results
        .iter()
        .filter(|r| samples - r.errors >= k)
        .map(|r| pass_hat_k(samples - r.errors, r.matched, k))
        .collect();
    let with_answers = results.iter().filter(|r| r.errors < samples).count();
    latencies.sort();
    let pct = |p: f64| -> u128 {
        if latencies.is_empty() {
            return 0;
        }
        let i = ((latencies.len() - 1) as f64 * p).round() as usize;
        latencies[i].as_millis()
    };
    let _ = n;
    EvalReport {
        k,
        samples,
        traces: results.len(),
        pass_at_1: ratio(matched, answered),
        pass_at_1_ci: wilson_clustered(
            &results
                .iter()
                .map(|r| (r.matched, samples - r.errors))
                .collect::<Vec<_>>(),
        ),
        pass_hat_k: if per_trace.is_empty() {
            0.0
        } else {
            per_trace.iter().sum::<f64>() / per_trace.len() as f64
        },
        scored_traces: per_trace.len(),
        consistent: ratio(
            results
                .iter()
                .filter(|r| r.errors < samples && r.distinct == 1)
                .count(),
            with_answers,
        ),
        rejected,
        errors,
        error_rate: ratio(errors, latencies.len()),
        calls: latencies.len(),
        p50_ms: pct(0.5),
        p95_ms: pct(0.95),
        results,
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// C(n, k) as a float (n is small: samples per trace).
fn choose(n: usize, k: usize) -> f64 {
    if k > n {
        return 0.0;
    }
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// The unbiased pass^k estimate for one trace: C(c, k) / C(n, k).
pub fn pass_hat_k(n: usize, c: usize, k: usize) -> f64 {
    let all = choose(n, k);
    if all == 0.0 {
        0.0
    } else {
        choose(c, k) / all
    }
}

/// 95% Wilson score interval for `hits` of `n`.
pub fn wilson(hits: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    wilson_p(hits as f64 / n as f64, n as f64)
}

/// 95% Wilson interval for pooled `(hits, n)` clusters (one per trace),
/// with the effective sample size from the design effect (Kish): the
/// cluster-robust variance of the ratio estimate p = Σc / Σn is
/// T/(T-1) · Σ(cᵢ - p·nᵢ)² / (Σn)², and n_eff = p(1-p) / that variance,
/// kept between T (re-plans of a trace agree completely: one Bernoulli per
/// trace) and Σn (independent re-plans). At p = 0 or 1 the variance is 0
/// and n_eff = T, the conservative end. Pooling all re-plans as
/// independent made 10 traces x 8 samples read as n = 80 (review M-10).
pub fn wilson_clustered(clusters: &[(usize, usize)]) -> (f64, f64) {
    let clusters: Vec<(f64, f64)> = clusters
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|&(c, n)| (c as f64, n as f64))
        .collect();
    let t = clusters.len() as f64;
    let total: f64 = clusters.iter().map(|(_, n)| n).sum();
    if total == 0.0 {
        return (0.0, 0.0);
    }
    let p = clusters.iter().map(|(c, _)| c).sum::<f64>() / total;
    let n_eff = if t < 2.0 || p <= 0.0 || p >= 1.0 {
        t
    } else {
        let ss: f64 = clusters.iter().map(|(c, n)| (c - p * n).powi(2)).sum();
        let var = t / (t - 1.0) * ss / (total * total);
        if var <= 0.0 {
            total
        } else {
            (p * (1.0 - p) / var).clamp(t, total)
        }
    };
    wilson_p(p, n_eff)
}

fn wilson_p(p: f64, n: f64) -> (f64, f64) {
    const Z: f64 = 1.959_964;
    let denom = 1.0 + Z * Z / n;
    let centre = (p + Z * Z / (2.0 * n)) / denom;
    let half = Z * ((p * (1.0 - p) / n + Z * Z / (4.0 * n * n)).sqrt()) / denom;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

/// The element `selector` picks on the trace's screen, if any.
fn target_of<'a>(selector: &Selector, trace: &'a Trace) -> Option<&'a Element> {
    find_all_ref(&trace.screen, selector, trace.screen_rect.as_ref())
        .into_iter()
        .next()
}

/// The action with its selector taken out, and that selector.
fn split_target(action: &Action) -> Option<(Action, Selector)> {
    let mut a = action.clone();
    let blank = || Selector::Id(String::new());
    let sel = match &mut a {
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::LongPress { selector, .. }
        | Action::InputText { selector, .. }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector }
        | Action::AssertText { selector, .. }
        | Action::ScrollUntilVisible { selector, .. } => std::mem::replace(selector, blank()),
        _ => return None,
    };
    Some((a, sel))
}

/// Whether `got` does what `want` did: equal, or the same op and values with
/// selectors that pick the same node on the trace's screen.
fn same_target(got: &Action, want: &Action, trace: &Trace) -> bool {
    if got == want {
        return true;
    }
    let (Some((g, gs)), Some((w, ws))) = (split_target(got), split_target(want)) else {
        return false;
    };
    if g != w {
        return false;
    }
    match (target_of(&gs, trace), target_of(&ws, trace)) {
        (Some(a), Some(b)) => std::ptr::eq(a, b),
        _ => false,
    }
}

/// An answer's identity for "distinct answers": what it does to which node.
fn answer_key(action: &Action, trace: &Trace) -> String {
    match split_target(action) {
        Some((rest, sel)) => match target_of(&sel, trace) {
            Some(node) => format!("{rest:?} on {:p}", node as *const Element),
            None => format!("{action:?}"),
        },
        None => format!("{action:?}"),
    }
}

/// The fixed task set, as shipped in `fixtures/eval/tasks.json`.
const BUILTIN_TASKS: &str = include_str!("../fixtures/eval/tasks.json");

/// The built-in task set: recorded screens (Android, iOS, web) with the
/// expected operation and target for each step. Fixed, so a model
/// comparison measures the models and nothing else.
pub fn builtin_tasks() -> Vec<Trace> {
    serde_json::from_str(BUILTIN_TASKS).expect("fixtures/eval/tasks.json is a list of traces")
}

/// One model's row in a [`Comparison`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRow {
    /// `provider:model` (`@url` for a custom endpoint).
    pub model: String,
    pub report: EvalReport,
    /// Calls and tokens the model spent on this task set.
    pub usage: Usage,
    /// Dollars for `usage` at the given price, when one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Wall time for the model's whole run.
    pub wall_ms: u128,
}

/// A task set run across several models.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub k: usize,
    pub samples: usize,
    pub tasks: usize,
    pub rows: Vec<ModelRow>,
}

/// Dollars per million input and output tokens.
pub type Price = (f64, f64);

/// Run `traces` across `models` (each re-planned `samples` times per
/// trace, pass^k estimated from them). `prices` maps a model's label to
/// its price; a model without one has no cost.
pub async fn compare(
    models: &[(String, &dyn NaturalPlanner)],
    traces: &[Trace],
    samples: usize,
    k: usize,
    prices: &std::collections::HashMap<String, Price>,
) -> Comparison {
    let mut rows = Vec::with_capacity(models.len());
    for (label, planner) in models {
        let before = planner.usage();
        let started = Instant::now();
        let report = evaluate_with(*planner, traces, samples, k).await;
        let usage = planner.usage().since(before);
        // `Usage::cost` bills the cache reads and writes inside
        // `input_tokens` at their own rates, as `atalanta explore` does: at
        // the plain input price a cached prompt read as ten times its cost.
        let cost_usd = prices.get(label).map(|(i, o)| usage.cost(*i, *o));
        rows.push(ModelRow {
            model: label.clone(),
            report,
            usage,
            cost_usd,
            wall_ms: started.elapsed().as_millis(),
        });
    }
    Comparison {
        k: k.max(1),
        samples: samples.max(k.max(1)),
        tasks: traces.len(),
        rows,
    }
}

/// The model [`Comparison::recommend`] picks, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recommendation {
    pub model: String,
    pub why: String,
}

/// Models within this much pass^k of the best count as equally accurate.
pub const RECOMMEND_TOLERANCE: f64 = 0.02;
/// Models with more errors than this are left out of the pick.
const RECOMMEND_MAX_ERRORS: f64 = 0.1;

impl Comparison {
    /// A fixed-width table: one row per model.
    pub fn table(&self) -> String {
        let mut out = format!(
            "{:<44} {:>7} {:>7} {:>7} {:>6} {:>8} {:>8} {:>10} {:>10} {:>9}\n",
            "model",
            "pass@1",
            format!("pass^{}", self.k),
            "consist",
            "errors",
            "p50 ms",
            "p95 ms",
            "in tok",
            "out tok",
            "cost $"
        );
        for r in &self.rows {
            let pct = |v: f64| format!("{:.1}%", v * 100.0);
            out.push_str(&format!(
                "{:<44} {:>7} {:>7} {:>7} {:>6} {:>8} {:>8} {:>10} {:>10} {:>9}\n",
                r.model,
                pct(r.report.pass_at_1),
                pct(r.report.pass_hat_k),
                pct(r.report.consistent),
                pct(r.report.error_rate),
                r.report.p50_ms,
                r.report.p95_ms,
                r.usage.input_tokens,
                r.usage.output_tokens,
                r.cost_usd
                    .map_or_else(|| "-".to_string(), |c| format!("{c:.4}"))
            ));
        }
        out
    }

    /// The model to make the default: the most accurate by pass^k, and
    /// among those within [`RECOMMEND_TOLERANCE`] of it the cheapest (by
    /// cost when every one has a price, else by tokens, else by p50
    /// latency). Models whose calls mostly errored are left out; `None`
    /// when none is left.
    pub fn recommend(&self) -> Option<Recommendation> {
        let usable: Vec<&ModelRow> = self
            .rows
            .iter()
            .filter(|r| r.report.scored_traces > 0 && r.report.error_rate <= RECOMMEND_MAX_ERRORS)
            .collect();
        let best = usable
            .iter()
            .map(|r| r.report.pass_hat_k)
            .fold(f64::NEG_INFINITY, f64::max);
        let close: Vec<&ModelRow> = usable
            .into_iter()
            .filter(|r| r.report.pass_hat_k >= best - RECOMMEND_TOLERANCE)
            .collect();
        let tokens = |r: &ModelRow| r.usage.input_tokens + r.usage.output_tokens;
        let (pick, by) = if close.iter().all(|r| r.cost_usd.is_some()) {
            let r = close.iter().min_by(|a, b| {
                a.cost_usd
                    .partial_cmp(&b.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
            (*r, "cost")
        } else if close.iter().all(|r| tokens(r) > 0) {
            (*close.iter().min_by_key(|r| tokens(r))?, "tokens")
        } else {
            (
                *close.iter().min_by_key(|r| r.report.p50_ms)?,
                "p50 latency",
            )
        };
        let others = close.len() - 1;
        Some(Recommendation {
            model: pick.model.clone(),
            why: format!(
                "pass^{} {:.1}% (best {:.1}%){}",
                self.k,
                pick.report.pass_hat_k * 100.0,
                best * 100.0,
                if others > 0 {
                    format!(
                        "; lowest {by} of the {} model(s) within {:.0} points of the best",
                        others + 1,
                        RECOMMEND_TOLERANCE * 100.0
                    )
                } else {
                    String::new()
                }
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::tests::el;
    use crate::planner::{Op, PlanRequest, PlannedStep};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers element 0, except every `flip`-th call answers element 1.
    struct Flaky {
        calls: AtomicUsize,
        flip: usize,
    }

    #[async_trait]
    impl NaturalPlanner for Flaky {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(PlannedStep {
                op: Op::Tap,
                element: Some(usize::from(self.flip > 0 && n.is_multiple_of(self.flip))),
                text: String::new(),
                direction: "none".into(),
                ms: 0,
                key: Default::default(),
            })
        }
    }

    fn screen() -> Element {
        Element {
            children: vec![
                el("com.app:id/ok", "Button", Some("OK"), None),
                el("com.app:id/cancel", "Button", Some("Cancel"), None),
            ],
            ..el("hierarchy", "", None, None)
        }
    }

    fn trace(intent: &str) -> Trace {
        Trace {
            version: TRACE_VERSION,
            platform: Platform::Android,
            kind: NaturalKind::Act,
            intent: intent.into(),
            data: None,
            wait_until: None,
            scope: "t#0".into(),
            screen: screen(),
            screen_rect: None,
            action: Action::Tap {
                selector: Selector::Id("com.app:id/ok".into()),
            },
        }
    }

    #[tokio::test]
    async fn perfectly_consistent_planner_scores_one() {
        let p = Flaky {
            calls: AtomicUsize::new(0),
            flip: 0,
        };
        let r = evaluate(&p, &[trace("tap ok"), trace("press ok")], 4).await;
        assert_eq!((r.pass_at_1, r.pass_hat_k, r.consistent), (1.0, 1.0, 1.0));
        assert_eq!(r.calls, 8);
    }

    #[tokio::test]
    async fn one_divergent_answer_breaks_pass_hat_k_not_pass_at_1() {
        // Calls 1..=8; every 4th answers "Cancel": one miss per trace.
        let p = Flaky {
            calls: AtomicUsize::new(0),
            flip: 4,
        };
        let r = evaluate(&p, &[trace("tap ok"), trace("press ok")], 4).await;
        assert_eq!(r.pass_at_1, 0.75);
        assert_eq!(r.pass_hat_k, 0.0);
        assert_eq!(r.consistent, 0.0);
        assert!(r.results[0]
            .divergent
            .as_deref()
            .unwrap()
            .contains("cancel"));
    }

    /// Fails every call.
    struct Down;

    #[async_trait]
    impl NaturalPlanner for Down {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            Err(AtalantaError::ServiceUnavailable {
                service: "Anthropic API".into(),
                detail: "529".into(),
            })
        }
    }

    /// Answers `assertVisible` (a check, rejected for an Act step) on every
    /// `bad_every`-th call, and taps element 0 otherwise.
    struct Sloppy {
        calls: AtomicUsize,
        bad_every: usize,
    }

    #[async_trait]
    impl NaturalPlanner for Sloppy {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let op = if n.is_multiple_of(self.bad_every) {
                Op::AssertVisible
            } else {
                Op::Tap
            };
            Ok(PlannedStep {
                op,
                element: Some(0),
                text: String::new(),
                direction: "none".into(),
                ms: 0,
                key: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn grounding_rejections_are_misses_not_errors() {
        // MJ-1: every answer is assertVisible on an Act trace, so grounding
        // rejects each one (and the re-ask). That scored as an "error", left
        // out of every rate, and tripped --max-error-rate as an outage.
        let p = Sloppy {
            calls: AtomicUsize::new(0),
            bad_every: 1,
        };
        let r = evaluate(&p, &[trace("tap ok")], 3).await;
        assert_eq!((r.errors, r.error_rate), (0, 0.0), "{r:?}");
        assert_eq!(r.rejected, 3);
        assert_eq!((r.pass_at_1, r.pass_hat_k), (0.0, 0.0));
        assert_eq!(r.scored_traces, 1);
        assert_eq!(r.consistent, 0.0);
        assert!(r.results[0]
            .divergent
            .as_deref()
            .unwrap()
            .starts_with("rejected: "));
    }

    #[tokio::test]
    async fn some_rejections_lower_pass_at_1_and_pass_hat_k() {
        // A planner that is always rejected must score below one that
        // always reproduces the recording (before MJ-1 both read 0 errors'
        // worth of nothing vs 100%: the rejected one had no scored answers).
        let good = Flaky {
            calls: AtomicUsize::new(0),
            flip: 0,
        };
        let bad = Sloppy {
            calls: AtomicUsize::new(0),
            bad_every: 1,
        };
        let ok = evaluate(&good, &[trace("tap ok")], 4).await;
        let rejected = evaluate(&bad, &[trace("tap ok")], 4).await;
        assert!(rejected.pass_at_1 < ok.pass_at_1);
        assert!(rejected.pass_hat_k < ok.pass_hat_k);
    }

    /// Replies with prose, never a JSON answer.
    struct Prose;

    #[async_trait]
    impl NaturalPlanner for Prose {
        async fn plan(&self, _: &PlanRequest<'_>) -> Result<PlannedStep> {
            PlannedStep::from_reply("I think you should tap OK")
        }
    }

    #[tokio::test]
    async fn an_unparseable_reply_is_a_miss_and_an_outage_an_error() {
        let r = evaluate(&Prose, &[trace("tap ok")], 2).await;
        assert_eq!((r.rejected, r.errors), (2, 0));
        let r = evaluate(&Down, &[trace("tap ok")], 2).await;
        assert_eq!((r.rejected, r.errors), (0, 2));
    }

    #[test]
    fn pass_hat_k_is_unbiased() {
        // τ-bench: c = 6 of n = 8, k = 3 gives C(6,3)/C(8,3) = 20/56.
        assert!((pass_hat_k(8, 6, 3) - 20.0 / 56.0).abs() < 1e-12);
        assert_eq!(pass_hat_k(4, 4, 4), 1.0);
        assert_eq!(pass_hat_k(4, 3, 4), 0.0);
        let (lo, hi) = wilson(9, 10);
        assert!(lo > 0.55 && lo < 0.6 && hi > 0.98, "{lo} {hi}");
    }

    #[test]
    fn the_interval_is_clustered_by_trace() {
        // Review M-10: 10 traces x 8 re-plans, half always right and half
        // always wrong, is 10 observations, not 80.
        let clusters: Vec<(usize, usize)> =
            (0..10).map(|i| (if i < 5 { 8 } else { 0 }, 8)).collect();
        let pooled = wilson(40, 80);
        let clustered = wilson_clustered(&clusters);
        assert_eq!(clustered, wilson(5, 10));
        assert!(clustered.1 - clustered.0 > pooled.1 - pooled.0);
        // Independent-looking re-plans (every trace 4 of 8) keep n = 80.
        let even: Vec<(usize, usize)> = (0..10).map(|_| (4, 8)).collect();
        assert_eq!(wilson_clustered(&even), pooled);
        // All right: n_eff is the trace count.
        assert_eq!(wilson_clustered(&[(8, 8), (8, 8)]), wilson(2, 2));
    }

    #[tokio::test]
    async fn the_report_uses_the_clustered_interval() {
        let p = Flaky {
            calls: AtomicUsize::new(0),
            flip: 0,
        };
        let r = evaluate(&p, &[trace("tap ok"), trace("press ok")], 4).await;
        assert_eq!(r.pass_at_1_ci, wilson(2, 2), "not wilson(8, 8)");
    }

    #[tokio::test]
    async fn more_samples_give_the_unbiased_estimate() {
        // 8 re-plans per trace, every 4th answers Cancel: c = 6 of 8.
        let p = Flaky {
            calls: AtomicUsize::new(0),
            flip: 4,
        };
        let r = evaluate_with(&p, &[trace("tap ok")], 8, 3).await;
        assert_eq!((r.samples, r.calls), (8, 8));
        assert!(
            (r.pass_hat_k - 20.0 / 56.0).abs() < 1e-12,
            "{}",
            r.pass_hat_k
        );
        assert_eq!(r.pass_at_1, 0.75);
    }

    #[tokio::test]
    async fn errors_are_not_consistency() {
        // VEL-039: k identical errors scored as 100% self-consistent.
        let r = evaluate(&Down, &[trace("tap ok")], 3).await;
        assert_eq!(r.consistent, 0.0);
        assert_eq!((r.errors, r.calls), (3, 3));
        assert_eq!(r.error_rate, 1.0);
        assert_eq!(r.scored_traces, 0);
        assert_eq!(r.pass_at_1, 0.0);
    }

    #[tokio::test]
    async fn same_node_different_selector_matches() {
        // The recording used the label; the re-plan picks the id of the
        // same button. Syntactic equality called that a miss.
        let p = Flaky {
            calls: AtomicUsize::new(0),
            flip: 0,
        };
        let mut t = trace("tap ok");
        t.action = Action::Tap {
            selector: Selector::Text("OK".into()),
        };
        let r = evaluate(&p, &[t.clone()], 2).await;
        assert_eq!(r.pass_hat_k, 1.0);
        // A different node is still a miss.
        t.action = Action::Tap {
            selector: Selector::Text("Cancel".into()),
        };
        let r = evaluate(&p, &[t], 2).await;
        assert_eq!(r.pass_hat_k, 0.0);
    }

    #[test]
    fn skipped_traces_are_reported() {
        // VEL-328: unreadable, malformed and other-version files vanished.
        let dir = std::env::temp_dir().join(format!("velnl-skips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = trace("tap ok");
        write_trace(&dir, &t.step(), &t.screen, None, &t.action).unwrap();
        std::fs::write(dir.join("broken.json"), "{not json").unwrap();
        std::fs::write(dir.join("old.json"), r#"{"version":0,"intent":"x"}"#).unwrap();
        let loaded = load_traces_counted(&dir).unwrap();
        assert_eq!(loaded.traces.len(), 1);
        assert_eq!((loaded.malformed, loaded.other_version), (1, 1));
        let msg = loaded.skip_summary().unwrap();
        assert!(msg.starts_with("2 trace file(s) skipped"), "{msg}");
        // No temporary files are left behind.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn node(id: &str, ty: &str, text: Option<&str>, label: Option<&str>, y: i32) -> Element {
        Element {
            platform_id: id.into(),
            element_type: ty.into(),
            text: text.map(String::from),
            label: label.map(String::from),
            bounds: Rect {
                x: 20,
                y,
                width: 300,
                height: 44,
            },
            enabled: true,
            visible: true,
            ..Default::default()
        }
    }

    fn screen_of(children: Vec<Element>) -> Element {
        Element {
            platform_id: "root".into(),
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 900,
            },
            enabled: true,
            visible: true,
            children,
            ..Default::default()
        }
    }

    fn task(
        platform: Platform,
        kind: NaturalKind,
        intent: &str,
        data: Option<&str>,
        screen: Element,
        action: Action,
    ) -> Trace {
        Trace {
            version: TRACE_VERSION,
            platform,
            kind,
            intent: intent.into(),
            data: data.map(String::from),
            wait_until: None,
            scope: String::new(),
            screen,
            screen_rect: Some(Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 900,
            }),
            action,
        }
    }

    /// The source of `fixtures/eval/tasks.json` (regenerate with
    /// `ATALANTA_BLESS=1 cargo test -p atalanta-nl builtin_tasks`).
    fn builtin_task_source() -> Vec<Trace> {
        use NaturalKind::{Act, Assert};
        use Platform::{Android, Ios, Web};
        let id = |s: &str| Selector::Id(s.into());
        let login = || {
            screen_of(vec![
                node(
                    "com.shop:id/title",
                    "TextView",
                    Some("Welcome back"),
                    None,
                    80,
                ),
                node("com.shop:id/email", "EditText", None, Some("Email"), 200),
                node(
                    "com.shop:id/password",
                    "EditText",
                    None,
                    Some("Password"),
                    260,
                ),
                node("com.shop:id/sign_in", "Button", Some("Sign in"), None, 340),
                node("com.shop:id/sign_up", "Button", Some("Sign up"), None, 400),
                node(
                    "com.shop:id/forgot",
                    "TextView",
                    Some("Forgot password?"),
                    None,
                    460,
                ),
            ])
        };
        let cart = || {
            screen_of(vec![
                node("nav.back", "XCUIElementTypeButton", None, Some("Back"), 40),
                node(
                    "cart.title",
                    "XCUIElementTypeStaticText",
                    Some("Your cart"),
                    None,
                    100,
                ),
                node(
                    "row.0.name",
                    "XCUIElementTypeStaticText",
                    Some("Red shoes"),
                    None,
                    180,
                ),
                node(
                    "row.0.remove",
                    "XCUIElementTypeButton",
                    None,
                    Some("Remove Red shoes"),
                    180,
                ),
                node(
                    "row.1.name",
                    "XCUIElementTypeStaticText",
                    Some("Blue hat"),
                    None,
                    240,
                ),
                node(
                    "row.1.remove",
                    "XCUIElementTypeButton",
                    None,
                    Some("Remove Blue hat"),
                    240,
                ),
                node(
                    "cart.total",
                    "XCUIElementTypeStaticText",
                    Some("Total $54.90"),
                    None,
                    320,
                ),
                node(
                    "cart.checkout",
                    "XCUIElementTypeButton",
                    None,
                    Some("Checkout"),
                    400,
                ),
                node(
                    "tab.settings",
                    "XCUIElementTypeButton",
                    None,
                    Some("Settings"),
                    840,
                ),
            ])
        };
        let home = || {
            screen_of(vec![
                node("cookie-accept", "button", Some("Accept all"), None, 700),
                node("cookie-reject", "button", Some("Reject all"), None, 760),
                node("nav-pricing", "a", Some("Pricing"), None, 30),
                node("nav-docs", "a", Some("Docs"), None, 30),
                node("search", "input", None, Some("Search products"), 120),
                node("hero-title", "h1", Some("Shoes for every day"), None, 200),
            ])
        };
        let settings = || {
            screen_of(vec![
                node(
                    "android:id/title",
                    "TextView",
                    Some("Network & internet"),
                    None,
                    100,
                ),
                node(
                    "com.android.settings:id/wifi_label",
                    "TextView",
                    Some("Wi-Fi"),
                    None,
                    180,
                ),
                node(
                    "com.android.settings:id/wifi_switch",
                    "Switch",
                    None,
                    Some("Wi-Fi"),
                    180,
                ),
                node(
                    "com.android.settings:id/bt_label",
                    "TextView",
                    Some("Bluetooth"),
                    None,
                    240,
                ),
                node(
                    "com.android.settings:id/bt_switch",
                    "Switch",
                    None,
                    Some("Bluetooth"),
                    240,
                ),
            ])
        };
        let tap = |s: &str| Action::Tap { selector: id(s) };
        vec![
            task(
                Android,
                Act,
                "tap Sign in",
                None,
                login(),
                tap("com.shop:id/sign_in"),
            ),
            task(
                Android,
                Act,
                "enter the email",
                Some("ann@example.com"),
                login(),
                Action::InputText {
                    selector: id("com.shop:id/email"),
                    text: "ann@example.com".into(),
                },
            ),
            task(
                Android,
                Act,
                "type \"hunter2-demo\" into the password field",
                None,
                login(),
                Action::InputText {
                    selector: id("com.shop:id/password"),
                    text: "hunter2-demo".into(),
                },
            ),
            task(
                Android,
                Act,
                "open the sign up form",
                None,
                login(),
                tap("com.shop:id/sign_up"),
            ),
            task(
                Android,
                Assert,
                "the welcome title is shown",
                None,
                login(),
                Action::AssertVisible {
                    selector: id("com.shop:id/title"),
                },
            ),
            task(
                Ios,
                Act,
                "remove the blue hat",
                None,
                cart(),
                tap("row.1.remove"),
            ),
            task(Ios, Act, "go back", None, cart(), tap("nav.back")),
            task(
                Ios,
                Act,
                "proceed to checkout",
                None,
                cart(),
                tap("cart.checkout"),
            ),
            task(
                Ios,
                Assert,
                "the order total is shown",
                None,
                cart(),
                Action::AssertVisible {
                    selector: id("cart.total"),
                },
            ),
            task(Ios, Act, "open settings", None, cart(), tap("tab.settings")),
            task(
                Web,
                Act,
                "accept the cookie banner",
                None,
                home(),
                tap("cookie-accept"),
            ),
            task(
                Web,
                Act,
                "go to the pricing page",
                None,
                home(),
                tap("nav-pricing"),
            ),
            task(
                Web,
                Act,
                "search for \"red shoes\"",
                None,
                home(),
                Action::InputText {
                    selector: id("search"),
                    text: "red shoes".into(),
                },
            ),
            task(
                Android,
                Act,
                "turn on Bluetooth",
                None,
                settings(),
                tap("com.android.settings:id/bt_switch"),
            ),
        ]
    }

    #[test]
    fn builtin_tasks_match_the_fixture_file() {
        let want = serde_json::to_string_pretty(&builtin_task_source()).unwrap() + "\n";
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval/tasks.json");
        if std::env::var("ATALANTA_BLESS").as_deref() == Ok("1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            want,
            "regenerate with ATALANTA_BLESS=1 cargo test -p atalanta-nl builtin_tasks"
        );
        assert_eq!(builtin_tasks().len(), builtin_task_source().len());
    }

    /// Answers each built-in task with the element its expected action
    /// targets: a perfect model.
    struct Oracle;

    #[async_trait]
    impl NaturalPlanner for Oracle {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            let tasks = builtin_tasks();
            let t = tasks
                .iter()
                .find(|t| t.intent == req.intent)
                .expect("a built-in task");
            let (rest, sel) = split_target(&t.action).unwrap();
            let target = target_of(&sel, t).unwrap();
            let index = crate::planner::ScreenIndex::build(
                &t.screen,
                t.screen_rect.as_ref(),
                t.platform,
                &format!("{} {}", t.intent, t.data.as_deref().unwrap_or("")),
            );
            let element = (0..index.len())
                .find(|i| index.get(*i).unwrap().platform_id == target.platform_id)
                .expect("target listed");
            let (op, text) = match rest {
                Action::InputText { text, .. } => (Op::InputText, text),
                Action::AssertVisible { .. } => (Op::AssertVisible, String::new()),
                _ => (Op::Tap, String::new()),
            };
            Ok(PlannedStep {
                op,
                element: Some(element),
                text,
                direction: "none".into(),
                ms: 0,
                key: Default::default(),
            })
        }
    }

    // Every built-in task can be solved, and solving it scores as a match:
    // the fixtures are sound, so a miss in a comparison is the model's.
    #[tokio::test]
    async fn every_builtin_task_is_solvable() {
        let tasks = builtin_tasks();
        assert!(tasks.len() >= 12);
        for t in &tasks {
            let (_, sel) = split_target(&t.action).unwrap();
            assert_eq!(
                find_all_ref(&t.screen, &sel, t.screen_rect.as_ref()).len(),
                1,
                "{}: expected target must be unique",
                t.intent
            );
        }
        let r = evaluate(&Oracle, &tasks, 2).await;
        assert_eq!(r.pass_hat_k, 1.0, "{:#?}", r.results);
        let platforms: std::collections::HashSet<String> =
            tasks.iter().map(|t| t.platform.to_string()).collect();
        assert_eq!(platforms.len(), 3, "Android, iOS and web");
    }

    /// Counts tokens like a real backend, answering like `inner`.
    struct Metered<P> {
        inner: P,
        meter: crate::planner::UsageMeter,
        per_call: (u64, u64),
        delay_ms: u64,
    }

    #[async_trait]
    impl<P: NaturalPlanner> NaturalPlanner for Metered<P> {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.meter.record(self.per_call.0, self.per_call.1);
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.inner.plan(req).await
        }

        fn usage(&self) -> Usage {
            self.meter.get()
        }
    }

    /// Like [`Oracle`], but every `flip`-th call picks the wrong element.
    struct Sometimes {
        calls: AtomicUsize,
        flip: usize,
    }

    #[async_trait]
    impl NaturalPlanner for Sometimes {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let mut step = Oracle.plan(req).await?;
            if n.is_multiple_of(self.flip) {
                step.element = step.element.map(|e| if e == 0 { 1 } else { 0 });
            }
            Ok(step)
        }
    }

    fn metered<P>(inner: P, per_call: (u64, u64)) -> Metered<P> {
        Metered {
            inner,
            meter: Default::default(),
            per_call,
            delay_ms: 0,
        }
    }

    #[tokio::test]
    async fn compare_tabulates_models_and_recommends_the_cheapest_accurate_one() {
        let tasks = builtin_tasks();
        let big = metered(Oracle, (1000, 30));
        let small = metered(Oracle, (400, 10));
        let shaky = metered(
            Sometimes {
                calls: AtomicUsize::new(0),
                flip: 3,
            },
            (100, 5),
        );
        let models: Vec<(String, &dyn NaturalPlanner)> = vec![
            ("anthropic:big".into(), &big),
            ("openai:small".into(), &small),
            ("openai:shaky".into(), &shaky),
        ];
        let prices = std::collections::HashMap::from([
            ("anthropic:big".to_string(), (5.0, 25.0)),
            ("openai:small".to_string(), (0.25, 2.0)),
            ("openai:shaky".to_string(), (0.05, 0.4)),
        ]);
        let c = compare(&models, &tasks, 3, 3, &prices).await;
        assert_eq!(
            (c.k, c.samples, c.tasks, c.rows.len()),
            (3, 3, tasks.len(), 3)
        );
        let calls = (tasks.len() * 3) as u64;
        assert_eq!(c.rows[0].usage.calls, calls);
        assert_eq!(c.rows[0].usage.input_tokens, 1000 * calls);
        let big_cost = c.rows[0].cost_usd.unwrap();
        assert!((big_cost - (1000.0 * 5.0 + 30.0 * 25.0) * calls as f64 / 1e6).abs() < 1e-9);
        assert_eq!(c.rows[0].report.pass_hat_k, 1.0);
        assert!(c.rows[2].report.pass_hat_k < 1.0);
        let table = c.table();
        assert!(table.lines().next().unwrap().contains("pass^3"), "{table}");
        assert_eq!(table.lines().count(), 4, "{table}");
        assert!(table.contains("openai:shaky"));
        // Small is as accurate as big and cheaper; shaky is cheaper still
        // but not accurate enough.
        let rec = c.recommend().unwrap();
        assert_eq!(rec.model, "openai:small", "{rec:?}");
        assert!(rec.why.contains("cost"), "{rec:?}");
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["rows"][1]["model"], "openai:small");
        assert!(
            json["rows"][1]["report"]["passHatK"].is_null()
                || json["rows"][1]["report"]["pass_hat_k"].is_number()
        );
        assert!(json["rows"][1]["usage"]["inputTokens"].is_number());
    }

    /// Like [`Metered`], but half of each call's prompt is read from the
    /// provider's prompt cache.
    struct Cached(Metered<Oracle>);

    #[async_trait]
    impl NaturalPlanner for Cached {
        async fn plan(&self, req: &PlanRequest<'_>) -> Result<PlannedStep> {
            self.0.meter.record_cached(1000, 10, 500, 0);
            self.0.inner.plan(req).await
        }

        fn usage(&self) -> Usage {
            self.0.meter.get()
        }
    }

    // The cost column bills cache reads at the cache-read rate (a tenth of
    // the input price by default), like `Usage::cost`.
    #[tokio::test]
    async fn compare_prices_cache_reads_at_their_own_rate() {
        let tasks = builtin_tasks();
        let m = Cached(metered(Oracle, (0, 0)));
        let models: Vec<(String, &dyn NaturalPlanner)> = vec![("anthropic:m".into(), &m)];
        let prices = std::collections::HashMap::from([("anthropic:m".to_string(), (10.0, 20.0))]);
        let c = compare(&models, &tasks[..2], 1, 1, &prices).await;
        let calls = 2.0;
        // Per call: 500 plain + 500 cached input tokens, 10 output.
        let want = calls * (500.0 * 10.0 + 500.0 * 1.0 + 10.0 * 20.0) / 1e6;
        let got = c.rows[0].cost_usd.unwrap();
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
    }

    #[tokio::test]
    async fn without_prices_the_pick_goes_by_tokens_and_errors_are_left_out() {
        let tasks = builtin_tasks();
        let a = metered(Oracle, (900, 10));
        let b = metered(Oracle, (300, 10));
        let models: Vec<(String, &dyn NaturalPlanner)> = vec![
            ("a:x".into(), &a),
            ("b:y".into(), &b),
            ("c:down".into(), &Down),
        ];
        let c = compare(&models, &tasks[..3], 2, 2, &Default::default()).await;
        assert!(c.rows.iter().all(|r| r.cost_usd.is_none()));
        assert_eq!(c.rows[2].report.error_rate, 1.0);
        let rec = c.recommend().unwrap();
        assert_eq!(rec.model, "b:y");
        assert!(rec.why.contains("tokens"), "{rec:?}");
        // Only failing models: nothing to recommend.
        let only_down: Vec<(String, &dyn NaturalPlanner)> = vec![("c:down".into(), &Down)];
        let c = compare(&only_down, &tasks[..2], 1, 1, &Default::default()).await;
        assert!(c.recommend().is_none());
    }

    #[test]
    fn traces_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("velnl-traces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = trace("tap ok");
        write_trace(&dir, &t.step(), &t.screen, None, &t.action).unwrap();
        let loaded = load_traces(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].intent, "tap ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
