use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atalanta_common::{
    PlatformDriver, Result, RuntimeConfig, SuiteResult, TestCase, TestResult, TestStatus,
};
use atalanta_core::TestExecutor;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::copies::{CopyKind, CopyOutcome, CopyTag};
use crate::farm::DeviceFarm;
use crate::history;
use crate::retry::{
    print_result, run_test_with_retries, run_test_with_retries_from, unmet_dependency,
};
use crate::runner::{Cut, RunOutcome, Termination};
use crate::scheduler;

/// Runs tests in parallel across the devices of a `DeviceFarm`.
///
/// A **supervisor** owns the run: the work queue, retries, quarantine and
/// results. Each leased device gets an **actor**: a task that owns the
/// lease and one warm [`TestExecutor`] (sync state, selector cache, healing
/// snapshots, the batching state) for every test it runs, and talks to the
/// supervisor over channels. Nothing about a device is shared between
/// tasks, so devices never contend for locks.
///
/// - Work is a queue of dependency groups (tests linked by `dependsOn` run
///   in order on one device; independent tests are groups of one), sorted
///   longest-first (LPT).
/// - Retries go back through the queue, to the front, marked with the
///   device they failed on, so the next attempt runs on a different device
///   when there is one (Emcee). Retries share a run-wide budget (Marathon).
/// - An infrastructure failure gets one extra attempt on another device and
///   doesn't use the retry budget.
/// - A device that fails twice in a row for infrastructure reasons is
///   quarantined: its actor stops and a new actor waits for a device (one
///   that passes its readiness probe again, or one that boots mid-run).
pub struct ParallelRunner;

/// Retry budget cap for a whole run (Marathon's default).
const MAX_RUN_RETRIES: usize = 200;

/// One unit of queued work.
struct Item {
    group: Vec<TestCase>,
    /// Retries used (single-test items only).
    attempt: u32,
    infra_rerun: bool,
    /// Devices this item already failed on.
    failed_on: HashSet<String>,
    /// Error of each failed attempt, for flaky/rerun reporting.
    failed_attempts: Vec<String>,
    warnings: Vec<String>,
    started: Option<Instant>,
    /// Trace of the last failed attempt, kept for a flaky pass.
    trace: Option<std::path::PathBuf>,
    /// Set when this item is one of several copies of a test (hunt round,
    /// preventive copy).
    copy: Option<CopyTag>,
    /// D-19: the error of the first attempt that panicked. It sticks: the
    /// test ends as an internal error even if a retry elsewhere passes.
    panicked: Option<String>,
}

impl Item {
    fn new(group: Vec<TestCase>) -> Self {
        Self {
            group,
            attempt: 0,
            infra_rerun: false,
            failed_on: HashSet::new(),
            failed_attempts: Vec::new(),
            warnings: Vec::new(),
            started: None,
            trace: None,
            copy: None,
            panicked: None,
        }
    }

    /// The result for one of this item's tests that ends without running
    /// again (no device left, the run cut short, the queue skipped). A
    /// panic in an earlier attempt still makes it an internal error, exit
    /// 5 (D-19; review-d7 m7): these paths built a fresh result and lost it.
    fn unrun(&self, mut r: TestResult) -> TestResult {
        if self.panicked.is_some() {
            let mut warnings = std::mem::take(&mut r.warnings);
            warnings.push(format!(
                "its retry never ran: {}",
                r.error_message.clone().unwrap_or_default()
            ));
            crate::retry::keep_internal_error(&mut r, self.panicked.clone(), &mut warnings);
            r.warnings = warnings;
            r.infrastructure_failure = false;
            r.retries = self.failed_attempts.len() as u32;
            r.failed_attempts = self.failed_attempts.clone();
        }
        r
    }
}

/// What a parallel run queues besides one item per dependency group.
#[derive(Clone, Copy)]
enum Plan {
    /// One item per group, plus preventive copies of flaky tests when
    /// `--flaky-confidence` is set.
    Normal,
    /// Every group once per round: `--hunt N --parallel M`.
    Hunt { rounds: u32 },
}

/// Where a retry will run, for its console line: "on another device" only
/// when a healthy device is left untried. With one device it said so and
/// then retried on the same one (VEL-190).
fn retry_where(healthy: &[String], device: &str, failed_on: &HashSet<String>) -> &'static str {
    if healthy
        .iter()
        .any(|d| d != device && !failed_on.contains(d))
    {
        " on another device"
    } else {
        ""
    }
}

/// Read-only run settings every actor needs.
struct Shared {
    farm: Arc<DeviceFarm>,
    driver: Arc<dyn PlatformDriver>,
    suite_config: atalanta_common::SuiteConfig,
    app_id: String,
    /// `app:` build every actor installs (by hash) once it has its device.
    app_build: Option<String>,
    retry_count: u32,
    /// `--test-timeout`, per attempt.
    test_timeout: Option<std::time::Duration>,
    resolver: Option<Arc<atalanta_nl::NaturalResolver>>,
    jitter: Option<(u64, u64)>,
    /// `--stress`: each actor loads its device's CPU while it holds it.
    stress: bool,
    update_baselines: atalanta_common::BaselineUpdate,
    /// `--verify`, over the tests' and the suite's levels.
    verify: Option<atalanta_common::VerifyLevel>,
    /// The run's created baselines, shared by every device (D-V1).
    created_baselines: atalanta_common::CreatedBaselines,
    trace: atalanta_common::TraceMode,
    /// The run's start, shared by every device's executor (VEL-205).
    run_started: std::time::SystemTime,
    /// `RuntimeConfig::state_dir`: where self-healing keeps its files.
    state_dir: Option<std::path::PathBuf>,
    /// `--cache-results`, and every test (to tell dependency chains).
    result_cache: Option<std::sync::Mutex<crate::result_cache::ResultCache>>,
    all_tests: Vec<TestCase>,
    /// Each test's saved memory baseline from before this run, for the
    /// memory check (`config.performance`).
    memory_baselines: std::collections::HashMap<String, history::ResourceBaseline>,
}

/// Actor → supervisor.
enum Event {
    /// Leased `device`; send work.
    Ready { actor: usize, device: String },
    /// Finished an item on `device`.
    Done {
        actor: usize,
        device: String,
        outcome: ActorOutcome,
        /// The device was quarantined by this item; the actor has stopped.
        quarantined: bool,
        /// The item's copy tag, if it was a copy.
        copy: Option<CopyTag>,
    },
    /// Could not get a device at all (every device quarantined for good).
    NoDevice { actor: usize, reason: String },
}

enum ActorOutcome {
    Final(Vec<TestResult>),
    /// Failed in a way worth another attempt elsewhere. `used_budget`: a
    /// normal retry (not the free infrastructure re-run).
    Retry {
        item: Box<Item>,
        used_budget: bool,
    },
}

/// Supervisor → actor.
enum Work {
    Run(Box<Item>, RunFlags),
}

#[derive(Clone, Copy)]
struct RunFlags {
    /// The run-wide retry budget has room.
    budget: bool,
    /// `--fail-fast`: a failure isn't retried (infrastructure aside).
    fail_fast: bool,
    /// `--fail-fast` has tripped: skip instead of running.
    fail_fast_tripped: bool,
}

impl ParallelRunner {
    pub async fn run(
        suite: atalanta_common::TestSuite,
        config: RuntimeConfig,
        driver: Arc<dyn PlatformDriver>,
        concurrency: usize,
    ) -> Result<SuiteResult> {
        Self::run_outcome(suite, config, driver, concurrency)
            .await?
            .into_result()
    }

    /// [`run`](Self::run), but a run cut short by `--timeout` or a stop
    /// request still returns its results (see `SuiteRunner::run_outcome`).
    pub async fn run_outcome(
        suite: atalanta_common::TestSuite,
        config: RuntimeConfig,
        driver: Arc<dyn PlatformDriver>,
        concurrency: usize,
    ) -> Result<RunOutcome> {
        Self::run_planned(suite, config, driver, concurrency, Plan::Normal)
            .await
            .map(|(result, _, termination)| RunOutcome {
                result,
                termination,
            })
    }

    /// Flaky hunting across the device pool: every test once per round,
    /// each round with its own order and jitter seed (`base + round`), the
    /// copies of one test spread over different devices. Returns the
    /// latest round's result per test, and every copy's outcome.
    pub async fn hunt(
        suite: atalanta_common::TestSuite,
        config: RuntimeConfig,
        driver: Arc<dyn PlatformDriver>,
        concurrency: usize,
        rounds: u32,
    ) -> Result<(SuiteResult, Vec<CopyOutcome>)> {
        let config = RuntimeConfig {
            retry_count: Some(0),
            fail_fast: false,
            failed_first: false,
            flaky_confidence: None,
            shuffle_seed: Some(config.shuffle_seed.unwrap_or(1)),
            // Hunt rounds are experiments: kept out of the history.
            hunt_rounds: Some(rounds.max(1)),
            // Every copy must really run, or a flaky test passes from the
            // cache (VEL-324).
            cache_results: false,
            ..config
        };
        let rounds = rounds.max(1);
        let (result, copies, termination) =
            Self::run_planned(suite, config, driver, concurrency, Plan::Hunt { rounds }).await?;
        match termination {
            Termination::TimedOut(e) => Err(e),
            _ => Ok((result, copies)),
        }
    }

    async fn run_planned(
        suite: atalanta_common::TestSuite,
        config: RuntimeConfig,
        driver: Arc<dyn PlatformDriver>,
        concurrency: usize,
        plan: Plan,
    ) -> Result<(SuiteResult, Vec<CopyOutcome>, Termination)> {
        let suite_start = Instant::now();
        scheduler::check_shard(&config)?;

        // `--device` limits the pool: an id, a comma-separated list of ids,
        // or a name/id pattern ("*iPhone*"). Ignoring it let `--parallel 2`
        // take any booted device, other people's phones and tablets included.
        let farm = Arc::new(DeviceFarm::with_filter(
            driver.clone(),
            concurrency,
            config.device_id.clone(),
        ));
        let device_count = farm.refresh().await?;
        if device_count == 0 {
            return Err(crate::runner::device_unavailable(
                "A device",
                match &config.device_id {
                    Some(d) => {
                        format!("no booted device matches --device {d} for parallel execution")
                    }
                    None => "no booted devices found for parallel execution".to_string(),
                },
            ));
        }
        info!(
            devices = device_count,
            concurrency, "starting parallel execution"
        );

        let mut hist = history::load(&config.artifacts_dir).unwrap_or_default();
        let tests = scheduler::plan_tests(&suite.tests, &config, &hist);
        crate::live::plan(tests.iter().map(|t| t.name.as_str()));
        let planned: Vec<String> = tests.iter().map(|t| t.name.clone()).collect();
        // Every test in the suite, selected or not: history keeps them (m-6).
        let suite_tests: std::collections::HashSet<String> =
            suite.tests.iter().map(|t| t.name.clone()).collect();
        let total_tests = tests.len();

        // Longest group first: with a shared work queue this is LPT
        // scheduling, whose makespan is within 4/3 of optimal — vs. up to 2x
        // when a long test happens to be dispatched last.
        let all_tests = tests.clone();
        let mut groups = scheduler::dependency_groups(tests);
        let group_ms = |g: &[TestCase]| -> u64 {
            g.iter()
                .map(|t| hist.estimate_ms(&t.name))
                .fold(0, u64::saturating_add)
        };
        if config.shuffle_seed.is_none() {
            groups.sort_by_key(|g| std::cmp::Reverse(group_ms(g)));
        }

        println!("\n  Atalanta v{} (parallel)", env!("CARGO_PKG_VERSION"));
        println!("  Tests: {total_tests}, Devices: {device_count}");
        if let Some(seed) = config.shuffle_seed {
            println!("  Order: shuffled (seed {seed}; rerun with --shuffle --seed {seed})");
        }
        println!();

        let retry_count =
            crate::retry::capped_retries(config.retry_count.unwrap_or(suite.config.retry.count));
        let shared = Arc::new(Shared {
            farm,
            driver,
            suite_config: suite.config.clone(),
            app_id: suite.app_id.clone(),
            app_build: crate::install::build_for(&suite, config.platform),
            retry_count,
            // One shared resolver: its mutex serializes cache reads/writes.
            resolver: crate::nl::build_resolver_for_suite(&suite, config.verify)?,
            verify: config.verify,
            test_timeout: config.test_timeout_ms.map(std::time::Duration::from_millis),
            jitter: config
                .jitter_ms
                .map(|ms| (ms, config.shuffle_seed.unwrap_or(0))),
            stress: config.stress,
            update_baselines: config.update_baselines,
            created_baselines: config.created_baselines.clone(),
            trace: config.trace,
            run_started: config
                .run_started
                .unwrap_or_else(std::time::SystemTime::now),
            state_dir: config.state_dir.clone(),
            // See `SuiteRunner::run`: never a cached pass when updating
            // baselines (VEL-050).
            result_cache: (config.cache_results && !config.update_baselines.is_on()).then(|| {
                std::sync::Mutex::new(crate::result_cache::ResultCache::load(
                    &config.artifacts_dir,
                ))
            }),
            all_tests,
            memory_baselines: hist.resource_baselines.clone(),
        });

        let mut sup = Supervisor {
            shared: shared.clone(),
            queue: build_queue(groups, plan, &config, &hist),
            retries_left: (retry_count as usize * total_tests).min(MAX_RUN_RETRIES),
            fail_fast: config.fail_fast,
            failed: false,
            results: Vec::new(),
            actors: HashMap::new(),
            next_actor: 0,
            target: concurrency.max(1),
            copies: Vec::new(),
            running: HashMap::new(),
            copy_counts: HashMap::new(),
            deaths: 0,
            deadline: config
                .suite_timeout_ms
                .map(|ms| tokio::time::Instant::from_std(suite_start) + Duration::from_millis(ms)),
            stop: config.stop.clone(),
            timeout_ms: config.suite_timeout_ms.unwrap_or(0),
            cut: None,
            unfinished: Vec::new(),
        };
        sup.run().await;

        if let Some(cache) = &shared.result_cache {
            cache.lock().unwrap_or_else(|e| e.into_inner()).save();
        }
        if let Some(resolver) = &shared.resolver {
            let pruned = resolver.finish().await;
            if pruned > 0 {
                println!("  Pruned {pruned} unused natural-language cache entries");
            }
        }
        let copies = std::mem::take(&mut sup.copies);
        let mut results = std::mem::take(&mut sup.results);
        results.extend(merge_copies(&copies, plan, &hist));
        // NEW-001: every planned test has a result, or a failed one.
        crate::completeness::account_for_every_test(&planned, &mut results);
        // Cut short (`--timeout`, Ctrl-C): what never finished is still in
        // the report, as skipped (VEL-043/044).
        let termination = match sup.cut.take() {
            Some(c) => {
                let completed = results.len() - sup.unfinished.len();
                cprintln!(
                    "\n  \x1b[33m⚠ {}; some tests did not finish\x1b[0m",
                    c.not_run(sup.timeout_ms).trim_start_matches("not run: ")
                );
                c.termination(sup.timeout_ms, completed, total_tests)
            }
            None => Termination::Completed,
        };
        let unfinished = std::mem::take(&mut sup.unfinished);
        let suite_duration = suite_start.elapsed();
        // Hunt rounds are deliberate stress experiments, not real outcomes.
        let profiles = match config.hunt_rounds {
            None => Some(history::device_profiles(&*shared.driver).await),
            Some(_) => None,
        };
        let finished: Vec<TestResult> = results
            .iter()
            .filter(|r| !unfinished.contains(&r.test_name))
            .cloned()
            .collect();
        match history::update_saved(&config.artifacts_dir, |h| {
            if let Some(profiles) = &profiles {
                history::update_with_devices(h, &finished, profiles);
            }
            history::forget_stale(h, &suite_tests);
        }) {
            Ok(saved) => hist = saved,
            Err(e) => debug!("failed to save history: {e}"),
        }
        history::note_flakiness(&hist, &mut results);

        let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count();
        let (passed, failed, skipped, retried) = (
            count(TestStatus::Passed),
            count(TestStatus::Failed),
            count(TestStatus::Skipped),
            count(TestStatus::Retried),
        );

        println!();
        println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "  {passed} passed, {failed} failed{}{} — {:.1}s (parallel, {device_count} device{})",
            if retried > 0 {
                format!(" ({retried} flaky)")
            } else {
                String::new()
            },
            if skipped > 0 {
                format!(", {skipped} skipped")
            } else {
                String::new()
            },
            suite_duration.as_secs_f64(),
            if device_count == 1 { "" } else { "s" }
        );
        crate::incidents::print_incidents(&results);
        let flaky = history::flaky_report(&hist, &results);
        if !flaky.is_empty() {
            cprintln!("\n  \x1b[33mFlaky by history\x1b[0m (flip or retry rate ≥ 15%):");
            for line in flaky {
                println!("    {line}");
            }
        }

        Ok((
            SuiteResult {
                total: results.len(),
                passed,
                failed,
                skipped,
                retried,
                duration: suite_duration,
                tests: results,
                shard_index: None,
                shard_total: None,
            },
            copies,
            termination,
        ))
    }
}

/// The work queue: one item per group, then the copies the plan asks for.
fn build_queue(
    groups: Vec<Vec<TestCase>>,
    plan: Plan,
    config: &RuntimeConfig,
    hist: &history::TestHistory,
) -> VecDeque<Item> {
    let key = |g: &[TestCase]| -> String {
        g.iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join("\u{1f}")
    };
    match plan {
        Plan::Hunt { rounds } => {
            let base = config.shuffle_seed.unwrap_or(1);
            let tests: Vec<TestCase> = groups.into_iter().flatten().collect();
            let mut queue = VecDeque::new();
            // Round by round, each in its own order; devices that finish
            // early start on the next round.
            for round in 0..rounds {
                let seed = base.wrapping_add(u64::from(round));
                let order = scheduler::dependency_groups(scheduler::shuffle(tests.clone(), seed));
                for g in order {
                    let mut item = Item::new(g);
                    item.copy = Some(CopyTag {
                        key: key(&item.group),
                        kind: CopyKind::Hunt { round, seed },
                    });
                    queue.push_back(item);
                }
            }
            queue
        }
        Plan::Normal => {
            let target = config.flaky_confidence;
            let mut queue = VecDeque::new();
            for g in groups {
                // Only an independent test can be copied: a dependency chain
                // runs in order on one device.
                let copies = match (target, g.as_slice()) {
                    (Some(target), [t]) if hist.flagged_flaky(&t.name) => hist
                        .fail_rate(&t.name)
                        .map(|p| crate::copies::copies_needed(p, target))
                        .unwrap_or(1),
                    _ => 1,
                };
                if copies <= 1 {
                    queue.push_back(Item::new(g));
                    continue;
                }
                println!(
                    "  {}: flaky by history (fails {:.0}% of runs); {copies} copies for {:.0}% confidence",
                    g[0].name,
                    hist.fail_rate(&g[0].name).unwrap_or(0.0) * 100.0,
                    target.unwrap_or(0.0) * 100.0
                );
                for index in 0..copies {
                    let mut item = Item::new(g.clone());
                    item.copy = Some(CopyTag {
                        key: key(&g),
                        kind: CopyKind::Preventive { index, of: copies },
                    });
                    queue.push_back(item);
                }
            }
            queue
        }
    }
}

/// Fold copies back into one result per test: preventive copies merge
/// (a pass if any copy passed); a hunt keeps each test's latest round.
fn merge_copies(
    copies: &[CopyOutcome],
    plan: Plan,
    hist: &history::TestHistory,
) -> Vec<TestResult> {
    let mut by_test: Vec<(String, Vec<CopyOutcome>)> = Vec::new();
    for c in copies {
        let name = c.result.test_name.clone();
        match by_test.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => v.push(c.clone()),
            None => by_test.push((name, vec![c.clone()])),
        }
    }
    by_test
        .into_iter()
        .filter_map(|(name, cs)| match plan {
            Plan::Normal => {
                crate::copies::merge_preventive(cs, hist.fail_rate(&name).unwrap_or(0.0))
            }
            Plan::Hunt { .. } => cs
                .into_iter()
                .max_by_key(|c| match c.tag.kind {
                    CopyKind::Hunt { round, .. } => round,
                    CopyKind::Preventive { index, .. } => index,
                })
                .map(|c| c.result),
        })
        .collect()
}

/// A live actor as the supervisor sees it.
/// The most a cut-short run waits for its actors to stop and their
/// devices' stress to be turned off before it reports.
const CUT_CLEANUP: Duration = Duration::from_secs(15);

/// The most one of `maintain`'s device calls (a readiness probe, a device
/// listing) may take before the supervisor goes on without it.
const MAINTAIN_LIMIT: Duration = Duration::from_secs(20);

struct ActorHandle {
    /// `Some` once it holds a lease.
    device: Option<String>,
    busy: bool,
    /// Tests of the item it is running, failed if the actor dies with it.
    in_flight: Vec<String>,
    /// The copy tag of that item, if it is a copy.
    copy: Option<CopyTag>,
    work: mpsc::Sender<Work>,
    task: tokio::task::JoinHandle<()>,
}

struct Supervisor {
    shared: Arc<Shared>,
    queue: VecDeque<Item>,
    retries_left: usize,
    fail_fast: bool,
    failed: bool,
    results: Vec<TestResult>,
    actors: HashMap<usize, ActorHandle>,
    next_actor: usize,
    /// Actors to keep alive (the requested parallelism).
    target: usize,
    /// Finished copies (hunt rounds, preventive copies), merged after the run.
    copies: Vec<CopyOutcome>,
    /// Devices running a copy of each copied test right now.
    running: HashMap<String, HashSet<String>>,
    /// Copies of each copied test each device has started.
    copy_counts: HashMap<String, HashMap<String, u32>>,
    /// Actors that died without reporting (see `reap_dead_actors`).
    deaths: usize,
    /// `--timeout`: when the run must stop.
    deadline: Option<tokio::time::Instant>,
    /// A stop request (Ctrl-C / SIGTERM) ends the run like the deadline.
    stop: atalanta_common::StopHandle,
    timeout_ms: u64,
    /// What cut the run short, if anything did.
    cut: Option<Cut>,
    /// Tests cut off while they ran: reported, but kept out of history.
    unfinished: Vec<String>,
}

impl Supervisor {
    async fn run(&mut self) {
        let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
        for _ in 0..self.target {
            self.spawn_actor(&events_tx);
        }
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await;
        loop {
            if self.fail_fast && self.failed {
                self.skip_queued("Skipped due to --fail-fast");
            }
            self.dispatch().await;
            let busy = self.actors.values().filter(|a| a.busy).count();
            if self.queue.is_empty() && busy == 0 {
                break;
            }
            let stop = self.stop.clone();
            let deadline = self.deadline;
            tokio::select! {
                c = Cut::first(&stop, deadline) => {
                    // `--timeout` (VEL-043) or a stop request: what ran is
                    // kept; the rest is reported as not run.
                    while let Ok(ev) = events.try_recv() {
                        self.handle(ev, &events_tx);
                    }
                    self.cut_short(c).await;
                    break;
                }
                ev = events.recv() => match ev {
                    Some(ev) => self.handle(ev, &events_tx),
                    None => break,
                },
                _ = tick.tick() => {
                    // An actor sends Done before it exits: handle what is
                    // queued, so `maintain` only sees actors that died.
                    while let Ok(ev) = events.try_recv() {
                        self.handle(ev, &events_tx);
                    }
                    self.maintain(&mut events, &events_tx).await
                }
            }
        }
        // Done: idle actors exit when their inbox closes (releasing their
        // lease); actors still waiting for a device are cancelled.
        for (_, a) in self.actors.drain() {
            drop(a.work);
            if a.device.is_none() {
                a.task.abort();
            } else {
                let _ = a.task.await;
            }
        }
    }

    fn spawn_actor(&mut self, events: &mpsc::UnboundedSender<Event>) {
        let id = self.next_actor;
        self.next_actor += 1;
        let (work_tx, work_rx) = mpsc::channel::<Work>(1);
        let task = tokio::spawn(actor(id, self.shared.clone(), work_rx, events.clone()));
        self.actors.insert(
            id,
            ActorHandle {
                device: None,
                busy: false,
                in_flight: Vec::new(),
                copy: None,
                work: work_tx,
                task,
            },
        );
    }

    /// Give each idle actor the first item that suits its device: one that
    /// hasn't failed there — or any, once no healthy device held by a live
    /// actor is left untried.
    ///
    /// Only devices this run actually holds count. Counting every healthy
    /// device hung the run when the one untried device was leased by another
    /// process (VEL-042), and counting quarantined ones made a retry wait for
    /// a device that would never come back (VEL-S09).
    async fn dispatch(&mut self) {
        let healthy: HashSet<String> = self.shared.farm.healthy_ids().into_iter().collect();
        let idle: Vec<(usize, String)> = self
            .actors
            .iter()
            .filter(|(_, a)| !a.busy)
            .filter_map(|(id, a)| a.device.clone().map(|d| (*id, d)))
            .collect();
        let leased: Vec<String> = self
            .actors
            .values()
            .filter_map(|a| a.device.clone())
            .collect();
        let held: Vec<&String> = leased.iter().filter(|d| healthy.contains(*d)).collect();
        for (id, device) in idle {
            let Some(pos) = self.queue.iter().position(|it| {
                (!it.failed_on.contains(&device) || held.iter().all(|d| it.failed_on.contains(*d)))
                    && self.copy_fits(it, &device, &leased)
            }) else {
                continue;
            };
            let Some(item) = self.queue.remove(pos) else {
                continue;
            };
            if let Some(tag) = &item.copy {
                self.running
                    .entry(tag.key.clone())
                    .or_default()
                    .insert(device.clone());
                *self
                    .copy_counts
                    .entry(tag.key.clone())
                    .or_default()
                    .entry(device.clone())
                    .or_default() += 1;
            }
            let flags = RunFlags {
                budget: self.retries_left > 0,
                fail_fast: self.fail_fast,
                fail_fast_tripped: self.fail_fast && self.failed,
            };
            if let Some(a) = self.actors.get_mut(&id) {
                let names = item.group.iter().map(|t| t.name.clone()).collect();
                let copy_tag = item.copy.clone();
                match a.work.send(Work::Run(Box::new(item), flags)).await {
                    Ok(()) => {
                        a.busy = true;
                        a.in_flight = names;
                        a.copy = copy_tag;
                    }
                    // The actor is gone; `maintain` replaces it. The item
                    // goes back, never lost.
                    Err(mpsc::error::SendError(Work::Run(item, _))) => {
                        self.queue.push_front(*item);
                    }
                }
            }
        }
    }

    /// Whether a copy may go to `device`: never while another copy of the
    /// same test runs there, and only to a device that has run no more
    /// copies of it than any other leased device, so copies spread over the
    /// pool. Items that aren't copies always fit.
    fn copy_fits(&self, item: &Item, device: &str, leased: &[String]) -> bool {
        let Some(tag) = &item.copy else {
            return true;
        };
        if self
            .running
            .get(&tag.key)
            .is_some_and(|r| r.contains(device))
        {
            return false;
        }
        let counts = self.copy_counts.get(&tag.key);
        let count = |d: &str| counts.and_then(|c| c.get(d)).copied().unwrap_or(0);
        let fewest = leased.iter().map(|d| count(d)).min().unwrap_or(0);
        count(device) <= fewest
    }

    fn handle(&mut self, ev: Event, events: &mpsc::UnboundedSender<Event>) {
        match ev {
            Event::Ready { actor, device } => {
                debug!(actor, device, "actor ready");
                if let Some(a) = self.actors.get_mut(&actor) {
                    a.device = Some(device);
                }
            }
            Event::Done {
                actor,
                device,
                outcome,
                quarantined,
                copy,
            } => {
                if let Some(a) = self.actors.get_mut(&actor) {
                    a.busy = false;
                    a.in_flight.clear();
                    a.copy = None;
                }
                if let Some(tag) = &copy {
                    if let Some(r) = self.running.get_mut(&tag.key) {
                        r.remove(&device);
                    }
                }
                match outcome {
                    ActorOutcome::Final(results) if copy.is_some() => {
                        let tag = copy.expect("checked");
                        let passed = results
                            .iter()
                            .all(|r| matches!(r.status, TestStatus::Passed | TestStatus::Retried));
                        // A preventive copy that passed settles its test:
                        // the copies still queued are no longer needed.
                        if passed && matches!(tag.kind, CopyKind::Preventive { .. }) {
                            self.queue
                                .retain(|it| it.copy.as_ref().is_none_or(|c| c.key != tag.key));
                        }
                        for result in results {
                            self.copies.push(CopyOutcome {
                                tag: tag.clone(),
                                device: device.clone(),
                                result,
                            });
                        }
                    }
                    ActorOutcome::Final(results) => {
                        self.failed |= results.iter().any(|r| r.status == TestStatus::Failed);
                        self.results.extend(results);
                    }
                    ActorOutcome::Retry {
                        mut item,
                        used_budget,
                    } => {
                        if used_budget {
                            self.retries_left = self.retries_left.saturating_sub(1);
                        }
                        item.failed_on.insert(device.clone());
                        self.queue.push_front(*item);
                    }
                }
                if quarantined {
                    cprintln!(
                        "  \x1b[31m▲ device {device} quarantined after repeated infrastructure failures\x1b[0m"
                    );
                    // Its actor has stopped; another waits for a device.
                    self.actors.remove(&actor);
                    self.spawn_actor(events);
                }
            }
            Event::NoDevice { actor, reason } => {
                self.actors.remove(&actor);
                let waiting = self.actors.values().any(|a| a.device.is_none());
                let leased = self.actors.values().any(|a| a.device.is_some());
                if !leased && !waiting {
                    let msg = format!("No device available: {reason}");
                    for item in self.queue.drain(..) {
                        for t in &item.group {
                            // A device problem, not the test's (VEL-046):
                            // exit 3, and kept out of the test's history.
                            let mut r = TestResult::failed(&t.name, msg.clone(), Duration::ZERO);
                            r.infrastructure_failure = true;
                            let r = item.unrun(r);
                            print_result(&r, None);
                            self.results.push(r);
                        }
                    }
                }
            }
        }
    }

    /// Re-probe quarantined devices, pick up newly booted ones, and keep the
    /// requested number of actors (one may be waiting for a device).
    async fn maintain(
        &mut self,
        inbox: &mut mpsc::UnboundedReceiver<Event>,
        events: &mpsc::UnboundedSender<Event>,
    ) {
        self.reap_dead_actors(inbox, events);
        // Device calls here are bounded and give way to `--timeout` and a
        // stop request: a wedged `adb` or `simctl` must not hold the
        // supervisor (which handles `Done` events and the cut) for as long
        // as the driver's own limit.
        let back = self
            .until_cut(self.shared.farm.probe_quarantined())
            .await
            .unwrap_or_default();
        for id in back {
            cprintln!(
                "  \x1b[32m▲ device {id} passed its readiness check; back in the pool\x1b[0m"
            );
        }
        if let Some(Err(e)) = self.until_cut(self.shared.farm.refresh()).await {
            debug!("device refresh failed: {e}");
        }
        while self.actors.len() < self.target && !self.queue.is_empty() {
            self.spawn_actor(events);
        }
    }

    /// `fut`, unless the run is cut (`--timeout`, a stop request) or it
    /// takes longer than [`MAINTAIN_LIMIT`]: then `None`, and the future is
    /// dropped (the farm holds no lock across a driver call).
    async fn until_cut<T>(&self, fut: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::select! {
            r = tokio::time::timeout(MAINTAIN_LIMIT, fut) => r.ok(),
            _ = Cut::first(&self.stop, self.deadline) => None,
        }
    }

    /// Actors whose task ended without telling the supervisor (a panic
    /// outside a test): fail what they were running, as infrastructure, and
    /// drop them so `maintain` starts replacements. Before this, a dead
    /// actor stayed busy forever and the run never finished.
    ///
    /// An actor that reports and then returns (quarantined, no device) can
    /// finish after the caller drained the inbox: its last event is in the
    /// inbox by the time its task shows finished, so handle the inbox again
    /// first. Handling removes such actors; only the rest died.
    fn reap_dead_actors(
        &mut self,
        inbox: &mut mpsc::UnboundedReceiver<Event>,
        events: &mpsc::UnboundedSender<Event>,
    ) {
        let finished: Vec<usize> = self
            .actors
            .iter()
            .filter(|(_, a)| a.task.is_finished())
            .map(|(id, _)| *id)
            .collect();
        if !finished.is_empty() {
            while let Ok(ev) = inbox.try_recv() {
                self.handle(ev, events);
            }
        }
        for id in finished {
            let Some(a) = self.actors.remove(&id) else {
                continue;
            };
            // Its copy never reports `Done`, so the device would stay marked
            // as running a copy of that test for ever, and no copy of it
            // could be dispatched there again.
            if let Some(device) = &a.device {
                for running in self.running.values_mut() {
                    running.remove(device);
                }
            }
            self.deaths += 1;
            let device = a.device.as_deref().unwrap_or("no device");
            cprintln!("  \x1b[31m▲ the worker for {device} stopped unexpectedly\x1b[0m");
            for name in a.in_flight {
                let mut r = TestResult::failed(
                    &name,
                    format!("internal error: the worker running it on {device} stopped"),
                    Duration::ZERO,
                );
                // Kept out of history and device health like a device
                // failure, but it is Atalanta's own bug: exit 5 (D-19).
                r.infrastructure_failure = true;
                r.internal_error = true;
                print_result(&r, a.device.as_deref());
                match &a.copy {
                    // A copy's result belongs with the test's other copies:
                    // pushed to `results` too, the test was reported twice.
                    Some(tag) => self.copies.push(CopyOutcome {
                        tag: tag.clone(),
                        device: a.device.clone().unwrap_or_default(),
                        result: r,
                    }),
                    None => self.results.push(r),
                }
            }
        }
        // Replacements keep dying too: stop instead of respawning forever.
        if self.deaths > self.target * 3 && !self.queue.is_empty() {
            for item in self.queue.drain(..) {
                for t in &item.group {
                    let mut r = TestResult::failed(
                        &t.name,
                        "not run: device workers keep stopping (internal error)".into(),
                        Duration::ZERO,
                    );
                    r.infrastructure_failure = true;
                    r.internal_error = true;
                    print_result(&r, None);
                    self.results.push(r);
                }
            }
        }
    }

    /// End the run now: the tests running are cut off, the queued ones
    /// skipped. The actors are stopped, and each device's CPU stress is
    /// turned off (an aborted actor never gets to it, so a timed-out
    /// `--stress` run left it on for later runs).
    async fn cut_short(&mut self, cut: Cut) {
        let mut done: HashSet<String> = self
            .results
            .iter()
            .chain(self.copies.iter().map(|c| &c.result))
            .map(|r| r.test_name.clone())
            .collect();
        let mut actors: Vec<ActorHandle> = self.actors.drain().map(|(_, a)| a).collect();
        for a in &actors {
            a.task.abort();
        }
        // All at once, under one deadline: one device after another, each
        // adb call with its own 60 s limit, kept a run with N wedged devices
        // N minutes past its own deadline before the reports (review 01 #8).
        let stress = self.shared.stress;
        let driver = self.shared.driver.clone();
        let stops = actors.iter_mut().map(|a| {
            let driver = driver.clone();
            let device = a.device.clone();
            let task = &mut a.task;
            async move {
                let _ = task.await;
                if let (true, Some(device)) = (stress, device) {
                    driver.set_stress(&device, false).await;
                }
            }
        });
        if tokio::time::timeout(CUT_CLEANUP, futures::future::join_all(stops))
            .await
            .is_err()
        {
            warn!(
                "turning device CPU stress off took over {}s; reporting without waiting",
                CUT_CLEANUP.as_secs()
            );
        }
        for a in actors {
            for name in a.in_flight {
                if done.insert(name.clone()) {
                    let r = cut.cut_test(&name, self.timeout_ms, Duration::ZERO);
                    print_result(&r, a.device.as_deref());
                    self.unfinished.push(name);
                    self.results.push(r);
                }
            }
        }
        let why = cut.not_run(self.timeout_ms);
        for item in self.queue.drain(..) {
            for t in &item.group {
                if done.insert(t.name.clone()) {
                    self.results
                        .push(item.unrun(TestResult::skipped(&t.name, &why)));
                }
            }
        }
        self.cut = Some(cut);
    }

    fn skip_queued(&mut self, reason: &str) {
        for item in self.queue.drain(..) {
            for t in &item.group {
                let r = item.unrun(TestResult::skipped(&t.name, reason));
                print_result(&r, None);
                self.results.push(r);
            }
        }
    }
}

/// One device: lease it, then run whatever the supervisor sends with one
/// executor that stays warm across tests, until the inbox closes.
async fn actor(
    id: usize,
    shared: Arc<Shared>,
    mut work: mpsc::Receiver<Work>,
    events: mpsc::UnboundedSender<Event>,
) {
    let lease = match shared.farm.acquire().await {
        Ok(l) => l,
        Err(e) => {
            let _ = events.send(Event::NoDevice {
                actor: id,
                reason: e.to_string(),
            });
            return;
        }
    };
    let device = lease.device_id().to_string();
    let driver = shared.driver.clone();
    // Every actor installs as soon as it has a device, so installs on all
    // devices overlap; the first test only starts on the build under test.
    if let Some(app) = &shared.app_build {
        if let Err(e) =
            crate::install::ensure_installed(&*driver, &device, app, &shared.app_id).await
        {
            cprintln!("  \x1b[31m▲ device {device} left out: the app didn't install: {e}\x1b[0m");
            shared.farm.retire(&device);
            drop(lease);
            let _ = events.send(Event::NoDevice {
                actor: id,
                reason: format!("the app didn't install on {device}: {e}"),
            });
            return;
        }
    }
    let mut executor = TestExecutor::new(&*driver, shared.suite_config.clone(), &shared.app_id)
        .with_update_baselines(shared.update_baselines)
        .with_created_baselines(shared.created_baselines.clone())
        .with_trace(shared.trace)
        .with_run_started(shared.run_started)
        .with_verify(shared.verify);
    if let Some(r) = &shared.resolver {
        executor = executor.with_planner(r.clone());
    }
    if let Some(dir) = &shared.state_dir {
        executor = executor.with_state_dir(dir);
    }
    if let Some((ms, seed)) = shared.jitter {
        executor = executor.with_jitter(ms, seed);
    }
    if events
        .send(Event::Ready {
            actor: id,
            device: device.clone(),
        })
        .is_err()
    {
        return;
    }
    let stressed = shared.stress && driver.set_stress(&device, true).await;
    while let Some(Work::Run(item, flags)) = work.recv().await {
        let copy = item.copy.clone();
        // A hunt round has its own jitter seed; other copies keep the run's.
        if let (Some(CopyKind::Hunt { seed, .. }), Some((ms, _))) =
            (copy.as_ref().map(|c| &c.kind), shared.jitter)
        {
            executor.set_jitter(ms, *seed);
        }
        let (outcome, quarantined) = if item.group.len() == 1 {
            run_single(&shared, &mut executor, *item, &device, flags).await
        } else {
            run_group(&shared, &mut executor, &item.group, &device, flags).await
        };
        if let (Some(CopyKind::Hunt { .. }), Some((ms, seed))) =
            (copy.as_ref().map(|c| &c.kind), shared.jitter)
        {
            executor.set_jitter(ms, seed);
        }
        let _ = events.send(Event::Done {
            actor: id,
            device: device.clone(),
            outcome,
            quarantined,
            copy,
        });
        if quarantined {
            break;
        }
    }
    if stressed {
        driver.set_stress(&device, false).await;
    }
    drop(lease);
}

/// How long a device that disappeared mid-run gets to come back.
const REATTACH_WINDOW: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(10)
};

/// Whether an infrastructure failure's error says the device itself went
/// away (adb's "device not found" / "offline" / "no devices", a USB
/// disconnect), not a driver hiccup (a WDA or uiautomator restart): only
/// then is it worth listing devices to see whether it comes back (review
/// d4 m5: every hiccup cost a full device listing).
fn device_seems_gone(error: Option<&str>) -> bool {
    const GONE: [&str; 7] = [
        "not found",
        "offline",
        "no devices",
        "disconnected",
        "not connected",
        "unable to locate device",
        "could not find device",
    ];
    // adb's other disconnect messages (`error: closed`, a reset, a protocol
    // fault, a host-side timeout), which `classify_failure` turns into
    // "ADB connection lost": counted only there, since a WDA or HTTP error
    // can say "connection reset" or "timed out" for a mere hiccup (review-d7
    // m9).
    const ADB_GONE: [&str; 5] = [
        "closed",
        "connection reset",
        "protocol fault",
        "cannot connect",
        "timed out",
    ];
    let lower = error.unwrap_or("").to_lowercase();
    GONE.iter().any(|m| lower.contains(m))
        || (lower.contains("adb connection lost") && ADB_GONE.iter().any(|m| lower.contains(m)))
}

/// Whether `device` is (again) listed as booted within `window`. A driver
/// that can't list devices says nothing: treat the device as present. The
/// window is a real deadline: each listing gets only the time left in it
/// (on Android one listing asks every device for its version, each up to
/// the 60 s command timeout), and a listing cut short by it says nothing
/// either.
async fn reattach(driver: &dyn PlatformDriver, device: &str, window: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, driver.list_devices()).await {
            Ok(Ok(list)) => {
                if list
                    .iter()
                    .any(|d| d.id == device && d.state == atalanta_common::DeviceState::Booted)
                {
                    return true;
                }
            }
            Ok(Err(_)) | Err(_) => return true,
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250).min(deadline - now)).await;
    }
}

/// Whether `test` starts by launching the suite's app (a plain or
/// `clearState` `launchApp` with no other app id).
fn launches_suite_app(test: &atalanta_common::TestCase, app_id: &str) -> bool {
    !app_id.is_empty()
        && matches!(
            test.steps.first().map(|s| &s.action),
            Some(atalanta_common::Action::LaunchApp { app_id: a, .. }) if a.is_empty() || a == app_id
        )
}

/// How many attempts of this item already ran (at least 1 for a retry).
fn attempts_so_far(item: &Item) -> u32 {
    // One per failed attempt: the retries used plus the free infrastructure
    // re-run. (`failed_on` counts devices, which stays 1 on a single device.)
    (item.attempt + u32::from(item.infra_rerun)).max(1)
}

/// One independent test: a single attempt here; a retry goes back to the
/// supervisor for another device.
async fn run_single(
    shared: &Shared,
    executor: &mut TestExecutor<'_>,
    mut item: Item,
    device: &str,
    flags: RunFlags,
) -> (ActorOutcome, bool) {
    let test = item.group[0].clone();
    item.started.get_or_insert_with(Instant::now);
    debug!(test = %test.name, device, attempt = item.attempt, "executing test");
    let cache_key = match &shared.result_cache {
        Some(_) if crate::result_cache::cacheable(&test, &shared.all_tests) => {
            crate::result_cache::key(
                &*shared.driver,
                device,
                &shared.app_id,
                &shared.suite_config,
                &test,
            )
            .await
        }
        _ => None,
    };
    if let (Some(cache), Some(key)) = (&shared.result_cache, &cache_key) {
        let hit = cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hit(key, &test);
        if let Some(hit) = hit {
            print_result(&hit, Some(device));
            return (ActorOutcome::Final(vec![hit]), false);
        }
    }
    let is_retry = item.attempt > 0 || item.infra_rerun;
    // The how-many-th attempt this is across devices: the executor names
    // the attempt's files `-retry<N>` from it, and the device keeps two
    // attempts apart.
    let base_attempt = if is_retry { attempts_so_far(&item) } else { 0 };
    let retry_tag = is_retry.then(|| device.to_string());
    let copy_tag = item.copy.as_ref().map(|c| c.trace_tag(device));
    executor.set_trace_tag(match (copy_tag, retry_tag) {
        (Some(c), Some(r)) => Some(format!("{c}-{r}")),
        (c, r) => c.or(r),
    });
    // A retry on another device is an independent trial only if it starts
    // the way the first attempt did: from a launch. Stop the app first so
    // the test's own `launchApp` is a cold start (a plain launch on Android
    // only brings a running app forward), and tell the executor it is a
    // retry so batching never skips that launch. App data, permissions and
    // the clipboard are not reset: use `launchApp: {clearState: true}` (or
    // `isolated: true`) for tests that need that.
    if is_retry && launches_suite_app(&test, &shared.app_id) {
        if let Err(e) = shared.driver.stop_app(device, &shared.app_id).await {
            debug!(test = %test.name, device, error = %e, "stop before retry failed");
        }
    }
    let mut result = run_test_with_retries_from(
        executor,
        &test,
        device,
        &shared.app_id,
        0,
        base_attempt,
        shared.test_timeout,
    )
    .await;
    executor.set_trace_tag(None);
    // A memory breach fails the test before it is cached, retried or shown.
    history::apply_memory_check(
        &mut result,
        shared.memory_baselines.get(&test.name),
        shared.suite_config.performance.pss_growth_threshold_pct,
    );
    if let (Some(cache), Some(key)) = (&shared.result_cache, cache_key.clone()) {
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(key, &result);
    }
    let infra = result.infrastructure_failure;
    let mut quarantined = shared.farm.record_outcome(device, infra);
    // A device that dropped off the bus gets a short window to come back
    // (an adb reconnect, a WDA restart); after that it is lost for this
    // run and its work moves now, not after more failures (D-25).
    if infra
        && !quarantined
        && device_seems_gone(result.error_message.as_deref())
        && !reattach(&*shared.driver, device, REATTACH_WINDOW).await
    {
        cprintln!(
            "  \x1b[31m▲ device {device} is gone (not back within {}s); its tests move to other devices\x1b[0m",
            REATTACH_WINDOW.as_secs_f32()
        );
        shared.farm.retire(device);
        quarantined = true;
    }
    item.warnings.append(&mut result.warnings);
    if result.internal_error && item.panicked.is_none() {
        item.panicked = result.error_message.clone();
    }

    if result.status == TestStatus::Failed && !flags.fail_fast_tripped {
        let (retry, used_budget) = if infra && !item.infra_rerun {
            // Says nothing about the app: one free re-run elsewhere.
            item.infra_rerun = true;
            item.warnings.push(format!(
                "infrastructure failure on {device}; re-queued for another device"
            ));
            (true, false)
        } else if item.copy.is_none()
            && !flags.fail_fast
            && flags.budget
            && item.attempt < shared.retry_count
        {
            // Copies don't retry: the other copies are the retries.
            item.attempt += 1;
            (true, true)
        } else {
            (false, false)
        };
        if retry {
            if result.trace.is_some() {
                item.trace = result.trace.clone();
            }
            if !infra {
                item.failed_attempts.push(
                    result
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "failed".into()),
                );
            }
            let elsewhere = retry_where(&shared.farm.healthy_ids(), device, &item.failed_on);
            cprintln!(
                "  \x1b[33m↻ {} [{device}] {}; retrying{}\x1b[0m",
                test.name,
                if infra {
                    "hit an infrastructure failure"
                } else {
                    "failed"
                },
                elsewhere
            );
            return (
                ActorOutcome::Retry {
                    item: Box::new(item),
                    used_budget,
                },
                quarantined,
            );
        }
    }

    // Final outcome.
    if result.status == TestStatus::Passed && !item.failed_attempts.is_empty() {
        result.status = TestStatus::Retried;
        let mut devices: Vec<&str> = item.failed_on.iter().map(String::as_str).collect();
        devices.sort_unstable();
        item.warnings.insert(
            0,
            format!(
                "flaky: failed {}x before passing on {device} (failed on: {})",
                item.failed_attempts.len(),
                devices.join(", ")
            ),
        );
    }
    crate::retry::keep_internal_error(&mut result, item.panicked.take(), &mut item.warnings);
    result.retries = item.failed_attempts.len() as u32;
    result.failed_attempts = std::mem::take(&mut item.failed_attempts);
    if result.trace.is_none() {
        result.trace = item.trace.take();
    }
    item.warnings.dedup();
    result.warnings = item.warnings;
    if let Some(started) = item.started {
        result.duration = started.elapsed();
    }
    print_result(&result, Some(device));
    (ActorOutcome::Final(vec![result]), quarantined)
}

/// A dependency group runs in order on one device. It can't move devices
/// mid-group (later tests rely on the state earlier ones built), so its
/// tests retry in place.
async fn run_group(
    shared: &Shared,
    executor: &mut TestExecutor<'_>,
    group: &[TestCase],
    device: &str,
    flags: RunFlags,
) -> (ActorOutcome, bool) {
    let mut results: Vec<TestResult> = Vec::with_capacity(group.len());
    let mut quarantined = false;
    for test in group {
        if flags.fail_fast_tripped
            || flags.fail_fast && results.iter().any(|r| r.status == TestStatus::Failed)
        {
            results.push(TestResult::skipped(
                &test.name,
                "Skipped due to --fail-fast",
            ));
            continue;
        }
        if let Some(dep) = unmet_dependency(test, &results) {
            let r = TestResult::skipped(
                &test.name,
                &format!("Skipped: dependency '{dep}' did not pass"),
            );
            print_result(&r, Some(device));
            results.push(r);
            continue;
        }
        debug!(test = %test.name, device, "executing test");
        let result = run_test_with_retries(
            executor,
            test,
            device,
            &shared.app_id,
            shared.retry_count,
            shared.test_timeout,
        )
        .await;
        quarantined |= shared
            .farm
            .record_outcome(device, result.infrastructure_failure);
        print_result(&result, Some(device));
        results.push(result);
        if quarantined {
            break;
        }
    }
    // Tests the group never reached (the device was quarantined) fail as
    // infrastructure, so history ignores them.
    for test in group.iter().skip(results.len()) {
        let mut r = TestResult::failed(
            &test.name,
            format!("not run: {device} was quarantined mid-group"),
            Duration::ZERO,
        );
        r.infrastructure_failure = true;
        results.push(r);
    }
    (ActorOutcome::Final(results), quarantined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::AtalantaError;
    use atalanta_common::{
        DeviceInfo, DeviceState, Direction, Element, Key, Platform, Rect, Selector,
    };

    /// A supervisor over `driver`'s devices, with no actors yet.
    fn bare_supervisor(driver: Arc<dyn PlatformDriver>) -> Supervisor {
        bare_supervisor_n(driver, 1)
    }

    /// The same, over up to `max_devices` devices.
    fn bare_supervisor_n(driver: Arc<dyn PlatformDriver>, max_devices: usize) -> Supervisor {
        let shared = Arc::new(Shared {
            farm: Arc::new(DeviceFarm::new(driver.clone(), max_devices)),
            driver,
            suite_config: Default::default(),
            app_id: "app".into(),
            app_build: None,
            retry_count: 0,
            test_timeout: None,
            resolver: None,
            jitter: None,
            stress: false,
            update_baselines: atalanta_common::BaselineUpdate::Off,
            verify: None,
            created_baselines: Default::default(),
            trace: Default::default(),
            run_started: std::time::SystemTime::now(),
            state_dir: None,
            result_cache: None,
            all_tests: Vec::new(),
            memory_baselines: Default::default(),
        });
        Supervisor {
            shared,
            queue: VecDeque::new(),
            retries_left: 0,
            fail_fast: false,
            failed: false,
            results: Vec::new(),
            actors: HashMap::new(),
            next_actor: 0,
            target: 1,
            copies: Vec::new(),
            running: HashMap::new(),
            copy_counts: HashMap::new(),
            deaths: 0,
            deadline: None,
            stop: Default::default(),
            timeout_ms: 0,
            cut: None,
            unfinished: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_actor_that_reported_then_exited_is_not_a_death() {
        // VEL-006 race: the actor sent Done{quarantined} and returned after
        // the supervisor's drain, before the reap. It was failed as "worker
        // stopped" and then its real result was recorded too.
        let driver = Arc::new(SplitDriver {
            devices: vec!["d1".into()],
            bad: "none",
            infra: false,
        });
        let mut sup = bare_supervisor(driver);
        let (tx, mut inbox) = mpsc::unbounded_channel::<Event>();
        let (work, _work_rx) = mpsc::channel::<Work>(1);
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        sup.actors.insert(
            0,
            ActorHandle {
                device: Some("d1".into()),
                busy: true,
                in_flight: vec!["t".into()],
                copy: None,
                work,
                task,
            },
        );
        sup.next_actor = 1;
        let mut pass = TestResult::failed("t", String::new(), Duration::ZERO);
        pass.status = TestStatus::Passed;
        pass.error_message = None;
        tx.send(Event::Done {
            actor: 0,
            device: "d1".into(),
            outcome: ActorOutcome::Final(vec![pass]),
            quarantined: true,
            copy: None,
        })
        .unwrap();
        sup.reap_dead_actors(&mut inbox, &tx);
        assert_eq!(sup.deaths, 0);
        assert_eq!(sup.results.len(), 1, "one result for one test");
        assert_eq!(sup.results[0].status, TestStatus::Passed);
    }

    // V9: the failure of a copy whose actor died went to `results`, and the
    // test's other copies were merged into `results` again after the run: the
    // test appeared twice. It is now one of the test's copies.
    #[tokio::test]
    async fn a_dead_copy_actor_reports_as_a_copy_not_a_second_result() {
        let driver = Arc::new(SplitDriver {
            devices: vec!["d1".into()],
            bad: "none",
            infra: false,
        });
        let mut sup = bare_supervisor(driver);
        let (tx, mut inbox) = mpsc::unbounded_channel::<Event>();
        let (work, _work_rx) = mpsc::channel::<Work>(1);
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let tag = CopyTag {
            key: "t".into(),
            kind: CopyKind::Hunt { round: 1, seed: 2 },
        };
        sup.actors.insert(
            0,
            ActorHandle {
                device: Some("d1".into()),
                busy: true,
                in_flight: vec!["t".into()],
                copy: Some(tag.clone()),
                work,
                task,
            },
        );
        sup.next_actor = 1;
        sup.reap_dead_actors(&mut inbox, &tx);
        assert!(sup.results.is_empty(), "{:#?}", sup.results);
        assert_eq!(sup.copies.len(), 1);
        assert_eq!(sup.copies[0].tag, tag);
        assert_eq!(sup.copies[0].device, "d1");
        assert!(sup.copies[0].result.internal_error);
    }

    // V10: with one device `failed_on` stays at one entry, so every retry was
    // "retry 1" and the second one overwrote the first one's trace files.
    #[test]
    fn each_retry_on_one_device_has_its_own_attempt_number() {
        let mut item = failed_item(&["A"]);
        item.attempt = 1;
        assert_eq!(attempts_so_far(&item), 1);
        item.attempt = 2;
        assert_eq!(attempts_so_far(&item), 2);
        // The free infrastructure re-run is an attempt too.
        item.infra_rerun = true;
        assert_eq!(attempts_so_far(&item), 3);
        // Another device failed too: still one number per attempt.
        item.failed_on.insert("B".into());
        assert_eq!(attempts_so_far(&item), 3);
    }

    // V8: `maintain` awaited the device listing with no bound, in the part of
    // the loop where `--timeout` and Ctrl-C are not watched, so a wedged
    // listing held the run past its stop request.
    #[tokio::test]
    async fn a_wedged_device_listing_does_not_hold_the_supervisor_past_a_stop() {
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-maintain-hanglist".into()],
            bad: "none",
            infra: false,
        });
        let mut sup = bare_supervisor(driver);
        let (tx, mut inbox) = mpsc::unbounded_channel::<Event>();
        sup.stop.stop("stop requested");
        tokio::time::timeout(Duration::from_secs(5), sup.maintain(&mut inbox, &tx))
            .await
            .expect("maintain returned after the stop request");
    }

    // V5: an actor that dies mid-copy never sends `Done`, which is what
    // cleared its device from `running`; copies of that test then never fit
    // on the device again and, with nothing else queued, the run never ended.
    #[tokio::test]
    async fn a_dead_actor_does_not_keep_its_device_marked_as_running_a_copy() {
        let driver = Arc::new(SplitDriver {
            devices: vec!["d1".into()],
            bad: "none",
            infra: false,
        });
        let mut sup = bare_supervisor(driver);
        let (tx, mut inbox) = mpsc::unbounded_channel::<Event>();
        let (work, _work_rx) = mpsc::channel::<Work>(1);
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        sup.actors.insert(
            0,
            ActorHandle {
                device: Some("d1".into()),
                busy: true,
                in_flight: vec!["t".into()],
                copy: None,
                work,
                task,
            },
        );
        sup.next_actor = 1;
        sup.running
            .entry("t".into())
            .or_default()
            .insert("d1".into());
        sup.reap_dead_actors(&mut inbox, &tx);
        assert_eq!(sup.deaths, 1);
        assert!(
            !sup.running.get("t").is_some_and(|r| r.contains("d1")),
            "d1 still marked as running a copy of t"
        );
    }

    /// A supervisor over `devices`, with an idle or busy actor on each of
    /// `actors` (device, busy); returns the idle actors' inboxes by device.
    async fn supervisor_with_actors(
        devices: &[&str],
        actors: &[(&str, bool)],
    ) -> (Supervisor, HashMap<String, mpsc::Receiver<Work>>) {
        let driver = Arc::new(SplitDriver {
            devices: devices.iter().map(|d| d.to_string()).collect(),
            bad: "none",
            infra: false,
        });
        let mut sup = bare_supervisor_n(driver, devices.len());
        sup.shared.farm.refresh().await.unwrap();
        let mut inboxes = HashMap::new();
        for (i, (device, busy)) in actors.iter().enumerate() {
            let (work, rx) = mpsc::channel::<Work>(1);
            inboxes.insert(device.to_string(), rx);
            sup.actors.insert(
                i,
                ActorHandle {
                    device: Some(device.to_string()),
                    busy: *busy,
                    in_flight: Vec::new(),
                    copy: None,
                    work,
                    task: tokio::spawn(async {}),
                },
            );
        }
        (sup, inboxes)
    }

    fn failed_item(on: &[&str]) -> Item {
        let suite = atalanta_core::parser::parse_suite_from_str(
            "appId: a\ntests:\n  - name: t\n    steps:\n      - wait: { ms: 1 }\n",
        )
        .unwrap();
        let mut item = Item::new(suite.tests);
        item.failed_on = on.iter().map(|d| d.to_string()).collect();
        item
    }

    fn panicked_item() -> Item {
        let mut item = failed_item(&["A"]);
        item.panicked = Some("test panicked: boom".into());
        item.failed_attempts = vec!["test panicked: boom".into()];
        item
    }

    fn assert_internal(results: &[TestResult]) {
        assert_eq!(results.len(), 1, "{results:#?}");
        let r = &results[0];
        assert_eq!(r.status, TestStatus::Failed, "{r:#?}");
        assert!(r.internal_error, "{r:#?}");
        assert!(!r.infrastructure_failure, "{r:#?}");
        assert_eq!(r.error_message.as_deref(), Some("test panicked: boom"));
    }

    // review-d7 m7: a panicked test re-queued for its retry, then finalized
    // without running (no device left, cut short, skipped), became an
    // infrastructure failure or a skip, and the panic (exit 5) was lost.
    #[tokio::test]
    async fn a_panicked_retry_that_never_runs_is_still_an_internal_error() {
        // No device left.
        let (mut sup, _inbox) = supervisor_with_actors(&["A"], &[]).await;
        sup.queue.push_back(panicked_item());
        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        sup.handle(
            Event::NoDevice {
                actor: 0,
                reason: "gone".into(),
            },
            &tx,
        );
        assert_internal(&sup.results);
        // Skipped.
        let (mut sup, _inbox) = supervisor_with_actors(&["A"], &[]).await;
        sup.queue.push_back(panicked_item());
        sup.skip_queued("fail fast");
        assert_internal(&sup.results);
        // Cut short.
        let (mut sup, _inbox) = supervisor_with_actors(&["A"], &[]).await;
        sup.queue.push_back(panicked_item());
        sup.cut_short(Cut::Stop("stopped".into())).await;
        assert_internal(&sup.results);
        // A queued item without a panic is skipped, as before.
        let (mut sup, _inbox) = supervisor_with_actors(&["A"], &[]).await;
        sup.queue.push_back(failed_item(&["A"]));
        sup.skip_queued("fail fast");
        assert_eq!(sup.results[0].status, TestStatus::Skipped);
    }

    // VEL-042: the retry's only untried device is leased by another
    // process, so no actor of this run ever holds it. The item waited for it
    // forever; it now runs again on the device this run holds.
    #[tokio::test]
    async fn a_retry_does_not_wait_for_a_device_this_run_cannot_get() {
        let (mut sup, mut inbox) = supervisor_with_actors(&["A", "B"], &[("A", false)]).await;
        sup.queue.push_back(failed_item(&["A"]));
        sup.dispatch().await;
        assert!(sup.queue.is_empty(), "the retry was dispatched");
        assert!(inbox.get_mut("A").unwrap().try_recv().is_ok());
    }

    #[test]
    fn a_retry_only_promises_another_device_when_one_is_left() {
        let ids = |v: &[&str]| v.iter().map(|d| d.to_string()).collect::<Vec<_>>();
        let failed = |v: &[&str]| v.iter().map(|d| d.to_string()).collect::<HashSet<_>>();
        assert_eq!(retry_where(&ids(&["A"]), "A", &failed(&[])), "");
        assert_eq!(retry_where(&ids(&["A", "B"]), "A", &failed(&["B"])), "");
        assert_eq!(
            retry_where(&ids(&["A", "B"]), "A", &failed(&[])),
            " on another device"
        );
    }

    // VEL-S09: avoidance counted quarantined devices, so after failing on A
    // and B with A quarantined, the retry went back to B although C, held by
    // a busy actor, was untried.
    #[tokio::test]
    async fn a_retry_waits_for_an_untried_healthy_device_not_a_retired_one() {
        let (mut sup, mut inbox) =
            supervisor_with_actors(&["A", "B", "C"], &[("B", false), ("C", true)]).await;
        sup.shared.farm.record_outcome("A", true);
        sup.shared.farm.record_outcome("A", true);
        assert_eq!(sup.shared.farm.healthy_count(), 2, "A is quarantined");
        sup.queue.push_back(failed_item(&["A", "B"]));
        sup.dispatch().await;
        assert_eq!(sup.queue.len(), 1, "kept for C");
        assert!(inbox.get_mut("B").unwrap().try_recv().is_err());
    }

    /// Devices that dropped off (see `…-gone` in [`SplitDriver`]).
    static GONE: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());

    /// Devices [`SplitDriver::stop_app`] was called on.
    static STOPPED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// Every device works except `bad`, where nothing is ever found.
    struct SplitDriver {
        devices: Vec<String>,
        bad: &'static str,
        /// The bad device fails with a lost connection (infrastructure)
        /// instead of a missing element.
        infra: bool,
    }

    fn button() -> Element {
        Element {
            hint: None,
            platform_id: "go".into(),
            label: None,
            text: Some("Go".into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn not_found() -> AtalantaError {
        AtalantaError::ElementNotFound {
            selector: "text=Go".into(),
            timeout_ms: 0,
            screenshot: None,
            hierarchy_snapshot: None,
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for SplitDriver {
        async fn collect_resource_metrics(&self, _: &str, _: &str) -> Result<(u64, u64, u64, f32)> {
            Ok((
                0,
                0,
                SPLIT_PSS_KB.load(std::sync::atomic::Ordering::SeqCst),
                0.0,
            ))
        }
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            // A wedged listing (`…-hanglist`): never answers.
            if self.devices.iter().any(|d| d.ends_with("-hanglist")) {
                std::future::pending::<()>().await;
            }
            let gone = GONE.lock().unwrap().clone();
            Ok(self
                .devices
                .iter()
                .filter(|id| !gone.contains(*id))
                .map(|id| DeviceInfo {
                    id: id.clone(),
                    name: id.clone(),
                    platform: Platform::Android,
                    state: DeviceState::Booted,
                    os_version: None,
                    device_type: Default::default(),
                })
                .collect())
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
        async fn stop_app(&self, d: &str, _: &str) -> Result<()> {
            STOPPED.lock().unwrap().push(d.to_string());
            Ok(())
        }
        async fn find_element(&self, d: &str, _: &Selector) -> Result<Element> {
            // A driver bug: the device named `…-panic` panics mid-test.
            if d.ends_with("-panic") {
                panic!("driver bug on {d}");
            }
            // The device named `…-gone` drops off the bus mid-test, for good.
            if d.ends_with("-gone") {
                GONE.lock().unwrap().insert(d.to_string());
                return Err(AtalantaError::AdbConnectionLost {
                    device_id: d.into(),
                    reason: "device not found".into(),
                });
            }
            if d == self.bad && self.infra {
                Err(AtalantaError::AdbConnectionLost {
                    device_id: d.into(),
                    reason: "device offline".into(),
                })
            } else if d == self.bad {
                Err(not_found())
            } else {
                Ok(button())
            }
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            Ok(self.find_element(d, s).await.into_iter().collect())
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            let mut root = button();
            root.platform_id = "root".into();
            root.text = None;
            root.bounds.width = 1080;
            root.bounds.height = 2400;
            if d != self.bad {
                root.children.push(button());
            }
            Ok(root)
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
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
            Ok((1080, 2400))
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            Ok("Go".into())
        }
        async fn is_element_visible(&self, d: &str, _: &Element) -> Result<bool> {
            Ok(d != self.bad)
        }
        async fn set_stress(&self, d: &str, on: bool) -> bool {
            if d.ends_with("-stresspanic") {
                panic!("stress bug on {d}");
            }
            if d.starts_with("par-stresslog") {
                STRESS_LOG.lock().unwrap().push((d.to_string(), on));
                return true;
            }
            false
        }
    }

    /// `set_stress` calls on `par-stresslog*` devices.
    static STRESS_LOG: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());

    /// Two quick tests, then four that wait 3 s.
    fn quick_then_slow() -> atalanta_common::TestSuite {
        let mut yaml = String::from(
            "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\ntests:\n",
        );
        for i in 0..2 {
            yaml.push_str(&format!(
                "  - name: q{i}\n    steps:\n      - assertVisible: {{ text: \"Go\" }}\n"
            ));
        }
        for i in 0..4 {
            yaml.push_str(&format!(
                "  - name: s{i}\n    steps:\n      - wait: {{ ms: 3000 }}\n"
            ));
        }
        atalanta_core::parser::parse_suite_from_str(&yaml).unwrap()
    }

    // VEL-043/044 and R-m2: a timed-out parallel run returns what ran,
    // reports the rest, and turns CPU stress off on its devices (aborted
    // actors never did, so stress stayed on for later runs).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timed_out_parallel_run_keeps_results_and_resets_stress() {
        let dir = scratch("timeout-partial");
        let devices = vec!["par-stresslog-a".to_string(), "par-stresslog-b".to_string()];
        let driver = Arc::new(SplitDriver {
            devices: devices.clone(),
            bad: "none",
            infra: false,
        });
        let cfg = RuntimeConfig {
            suite_timeout_ms: Some(1500),
            stress: true,
            ..config(&dir)
        };
        let started = Instant::now();
        let out = ParallelRunner::run_outcome(quick_then_slow(), cfg, driver, 2)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(out.termination, Termination::TimedOut(_)));
        let r = &out.result;
        let mut names: Vec<&str> = r.tests.iter().map(|t| t.test_name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["q0", "q1", "s0", "s1", "s2", "s3"]);
        assert_eq!(r.passed, 2, "{:?}", r.tests);
        assert_eq!(r.failed, 2, "the two cut off: {:?}", r.tests);
        assert_eq!(r.skipped, 2, "{:?}", r.tests);
        let log = STRESS_LOG.lock().unwrap().clone();
        for d in &devices {
            assert!(log.contains(&(d.clone(), false)), "{d}: {log:?}");
        }
        let hist = history::load(&dir.to_string_lossy()).unwrap();
        assert!(hist.outcomes.contains_key("q0") && !hist.outcomes.contains_key("s0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_request_ends_a_parallel_run_with_its_results() {
        let dir = scratch("stop");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-stop-a".into(), "par-stop-b".into()],
            bad: "none",
            infra: false,
        });
        let cfg = config(&dir);
        let stop = cfg.stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            stop.stop("interrupted by SIGTERM");
        });
        let out = ParallelRunner::run_outcome(quick_then_slow(), cfg, driver, 2)
            .await
            .unwrap();
        assert!(matches!(&out.termination, Termination::Stopped(r) if r.contains("SIGTERM")));
        assert_eq!(out.result.tests.len(), 6);
        assert_eq!(out.result.passed, 2);
        assert_eq!(out.result.skipped, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn suite(tests: usize, retries: u32) -> atalanta_common::TestSuite {
        let mut yaml = format!(
            "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\n  retry:\n    count: {retries}\ntests:\n"
        );
        for i in 0..tests {
            yaml.push_str(&format!(
                "  - name: t{i}\n    steps:\n      - assertVisible: {{ text: \"Go\" }}\n"
            ));
        }
        atalanta_core::parser::parse_suite_from_str(&yaml).unwrap()
    }

    fn config(dir: &std::path::Path) -> RuntimeConfig {
        RuntimeConfig {
            artifacts_dir: dir.to_string_lossy().into_owned(),
            // Healing state in the scratch dir, not this crate's `.atalanta/`.
            state_dir: Some(dir.join(".atalanta")),
            ..Default::default()
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("atalanta-par-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The total PSS [`SplitDriver`] reports (read only by suites with
    /// `performance.enabled`).
    static SPLIT_PSS_KB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // NEW-021: the memory check runs in the parallel runner too; a breach
    // fails, and keeps failing on the next run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_memory_breach_fails_in_the_parallel_runner_too() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = scratch("membreach");
        let mut statuses = Vec::new();
        for pss in [100_000, 130_000, 130_000] {
            SPLIT_PSS_KB.store(pss, SeqCst);
            let driver = Arc::new(SplitDriver {
                devices: vec!["par-mem-a".into(), "par-mem-b".into()],
                bad: "none",
                infra: false,
            });
            let mut s = suite(1, 0);
            s.config.performance.enabled = true;
            let r = ParallelRunner::run(s, config(&dir), driver, 2)
                .await
                .unwrap();
            statuses.push((r.tests[0].status, r.tests[0].error_message.clone()));
        }
        assert_eq!(statuses[0].0, TestStatus::Passed, "{statuses:?}");
        for (status, msg) in &statuses[1..] {
            assert_eq!(*status, TestStatus::Failed, "{statuses:?}");
            assert!(
                msg.as_deref().unwrap().contains("memory regression"),
                "{msg:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retry_moves_to_another_device() {
        let dir = scratch("move");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-move-good".into(), "par-move-bad".into()],
            bad: "par-move-bad",
            infra: false,
        });
        let result = ParallelRunner::run(suite(4, 1), config(&dir), driver, 2)
            .await
            .unwrap();
        assert_eq!(result.total, 4);
        assert_eq!(result.failed, 0, "{:#?}", result.tests);
        // Without this the loop below asserts nothing when no test happened
        // to start on the bad device.
        assert!(
            result.tests.iter().any(|t| t.status == TestStatus::Retried),
            "{:#?}",
            result.tests
        );
        for t in &result.tests {
            if t.status == TestStatus::Retried {
                assert_eq!(t.failed_attempts.len(), 1);
                assert!(
                    t.warnings[0].contains("failed on: par-move-bad"),
                    "{:?}",
                    t.warnings
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review 12 #4: a retry on another device starts from a cold launch,
    /// not from whatever that device's previous test left on screen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retry_elsewhere_stops_the_app_before_its_launch() {
        let dir = scratch("coldretry");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-cold-good".into(), "par-cold-bad".into()],
            bad: "par-cold-bad",
            infra: false,
        });
        let yaml = "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\n  retry:\n    count: 1\ntests:\n  - name: t0\n    steps:\n      - launchApp\n      - assertVisible: { text: \"Go\" }\n  - name: t1\n    steps:\n      - launchApp\n      - assertVisible: { text: \"Go\" }\n";
        let s = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        let result = ParallelRunner::run(s, config(&dir), driver, 2)
            .await
            .unwrap();
        assert_eq!(result.failed, 0, "{:#?}", result.tests);
        let retried = result
            .tests
            .iter()
            .filter(|t| t.status == TestStatus::Retried)
            .count();
        let stopped = STOPPED
            .lock()
            .unwrap()
            .iter()
            .filter(|d| *d == "par-cold-good")
            .count();
        assert!(retried >= 1, "{:#?}", result.tests);
        assert!(
            stopped >= retried,
            "{retried} retried, app stopped {stopped}x on the good device"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-006: a panic inside a test used to kill its actor, which stayed
    // "busy" forever, so the run never finished.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_test_fails_and_the_parallel_run_finishes() {
        let dir = scratch("panic");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-a-panic".into(), "par-b-panic".into()],
            bad: "none",
            infra: false,
        });
        let run = ParallelRunner::run(suite(3, 0), config(&dir), driver, 2);
        let result = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the run hung after a panic")
            .unwrap();
        assert_eq!(result.total, 3);
        assert_eq!(result.failed, 3, "{:#?}", result.tests);
        assert!(result.tests.iter().all(|t| t
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("panicked"))));
        // D-19: Atalanta's own bug, exit 5 (it was 1).
        assert!(result.tests.iter().all(|t| t.internal_error));
        assert_eq!(result.exit_code(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // D-19 (review M1): a test that panics on one device and passes on its
    // retry elsewhere ended FLAKY with exit 0. The panic now sticks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panic_then_a_pass_elsewhere_is_still_an_internal_error() {
        let dir = scratch("panicpass");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-pp-panic".into(), "par-pp-good".into()],
            bad: "none",
            infra: false,
        });
        let run = ParallelRunner::run(suite(4, 1), config(&dir), driver, 2);
        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the run hung")
            .unwrap();
        assert_eq!(result.total, 4);
        // The panicking device takes the first test of the run at least.
        let panicked: Vec<_> = result
            .tests
            .iter()
            .filter(|t| t.failed_attempts.iter().any(|m| m.contains("panicked")))
            .collect();
        assert!(!panicked.is_empty(), "{:#?}", result.tests);
        for t in &panicked {
            assert_eq!(t.status, TestStatus::Failed, "{t:#?}");
            assert!(t.internal_error, "{t:#?}");
        }
        assert!(result.tests.iter().all(|t| t.status != TestStatus::Retried));
        assert_eq!(result.exit_code(), 5);
        assert_eq!(result.exit_code_with(true), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-006: an actor that dies outside a test is noticed, and a pool
    // whose workers keep dying ends the run instead of hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actors_that_die_outside_a_test_end_the_run() {
        let dir = scratch("dead");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-dead-stresspanic".into()],
            bad: "none",
            infra: false,
        });
        let cfg = RuntimeConfig {
            stress: true,
            ..config(&dir)
        };
        let run = ParallelRunner::run(suite(2, 0), cfg, driver, 1);
        let result = tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("the run hung after its actors died")
            .unwrap();
        assert_eq!(result.failed, 2, "{:#?}", result.tests);
        // D-19: an actor dying is Atalanta's bug: exit 5 (it was 3).
        assert_eq!(result.exit_code(), 5, "{:#?}", result.tests);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-043: `--timeout` stops a parallel run (exit 4) instead of being
    // ignored. Every test here waits 2 s; the suite may take 300 ms.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_suite_timeout_stops_a_parallel_run() {
        let dir = scratch("timeout");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-timeout-a".into(), "par-timeout-b".into()],
            bad: "none",
            infra: false,
        });
        let mut yaml = String::from("appId: com.example\ntests:\n");
        for i in 0..6 {
            yaml.push_str(&format!(
                "  - name: t{i}\n    steps:\n      - wait: {{ ms: 2000 }}\n"
            ));
        }
        let suite = atalanta_core::parser::parse_suite_from_str(&yaml).unwrap();
        let cfg = RuntimeConfig {
            suite_timeout_ms: Some(300),
            ..config(&dir)
        };
        let started = Instant::now();
        let err = ParallelRunner::run(suite, cfg, driver, 2)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 4, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retries_stop_at_the_test_limit_on_a_single_bad_device() {
        let dir = scratch("limit");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-limit-bad".into()],
            bad: "par-limit-bad",
            infra: false,
        });
        let result = ParallelRunner::run(suite(1, 2), config(&dir), driver, 1)
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        // Two retries were allowed; the attempts after the first ran on the
        // same device because there is no other.
        assert_eq!(result.tests[0].failed_attempts.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // F28 at the call site: three attempts on one device (the run and two
    // retries) each keep their own trace files. With the old
    // `failed_on.len().max(1)` both retries were "retry 1" and the second
    // overwrote the first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_device_retries_keep_a_trace_each() {
        let dir = scratch("retrytrace");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-retrytrace-bad".into()],
            bad: "par-retrytrace-bad",
            infra: false,
        });
        let cfg = RuntimeConfig {
            trace: atalanta_common::TraceMode::Always,
            ..config(&dir)
        };
        let mut s = suite(1, 2);
        s.config.artifacts.output_dir = dir.to_string_lossy().into_owned();
        let result = ParallelRunner::run(s, cfg, driver, 1).await.unwrap();
        assert_eq!(result.tests[0].failed_attempts.len(), 2);
        let trace_dir = dir.join("traces").join("t0");
        let mut steps: Vec<String> = std::fs::read_dir(&trace_dir)
            .unwrap_or_else(|e| panic!("{}: {e}", trace_dir.display()))
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("steps") && n.ends_with(".json"))
            .collect();
        steps.sort();
        // Each attempt has its own numbered file, none shared.
        assert_eq!(steps.len(), 3, "{steps:?}");
        assert!(
            steps.iter().any(|n| n.contains("-retry2")),
            "no file for the second retry: {steps:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // m-5: the per-test retry cap applied to sequential runs only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_retries_are_capped_per_test() {
        let dir = scratch("cap");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-cap-bad".into()],
            bad: "par-cap-bad",
            infra: false,
        });
        let cfg = RuntimeConfig {
            retry_count: Some(50),
            ..config(&dir)
        };
        let result = ParallelRunner::run(suite(1, 0), cfg, driver, 1)
            .await
            .unwrap();
        assert_eq!(result.failed, 1);
        assert_eq!(
            result.tests[0].failed_attempts.len(),
            crate::retry::MAX_TEST_RETRIES as usize
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_with_infrastructure_failures_is_quarantined_and_its_work_moves() {
        let dir = scratch("quar");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-quar-good".into(), "par-quar-bad".into()],
            bad: "par-quar-bad",
            infra: true,
        });
        let result = ParallelRunner::run(suite(6, 0), config(&dir), driver, 2)
            .await
            .unwrap();
        assert_eq!(result.total, 6);
        // With no retries allowed, the free infrastructure re-run still moves
        // each hit test to the good device.
        assert_eq!(result.failed, 0, "{:#?}", result.tests);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // D-25: a device that disappears mid-run gets a short reattach window,
    // then is lost: its work moves at once instead of after another
    // failure there.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_that_disappears_is_lost_after_a_short_reattach() {
        let dir = scratch("gone");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-gone-ok".into(), "par-gone-x-gone".into()],
            bad: "none",
            infra: false,
        });
        let started = Instant::now();
        let result = ParallelRunner::run(suite(6, 0), config(&dir), driver, 2)
            .await
            .unwrap();
        assert_eq!(result.total, 6);
        assert_eq!(result.failed, 0, "{:#?}", result.tests);
        let hit = result
            .tests
            .iter()
            .filter(|t| t.warnings.iter().any(|w| w.contains("par-gone-x-gone")))
            .count();
        assert_eq!(hit, 1, "{:#?}", result.tests);
        assert!(started.elapsed() < Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Review d4 m5: reattach was bounded by "window plus one listing", and a
    // listing that never answers held the actor (its lease, its permit)
    // for ever.
    #[tokio::test]
    async fn reattach_ends_at_its_deadline_even_when_listing_hangs() {
        let driver = SplitDriver {
            devices: vec!["par-reattach-hanglist".into()],
            bad: "none",
            infra: false,
        };
        let started = Instant::now();
        let back = tokio::time::timeout(
            Duration::from_secs(5),
            reattach(&driver, "par-reattach-hanglist", Duration::from_millis(300)),
        )
        .await
        .expect("reattach outlived its window");
        assert!(back, "a listing that says nothing keeps the device");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    // Review d4 m5: only an error saying the device went away is worth a
    // device listing; a driver hiccup is not.
    #[test]
    fn only_a_device_that_went_away_is_looked_for() {
        assert!(device_seems_gone(Some(
            "ADB connection lost to device 'emulator-5554': device not found"
        )));
        assert!(device_seems_gone(Some(
            "ADB connection lost to device 'x': device offline"
        )));
        assert!(device_seems_gone(Some("Device 'x' not found. Available: ")));
        assert!(!device_seems_gone(Some(
            "Lost the connection to WebDriverAgent at http://localhost:8100: reset"
        )));
        assert!(!device_seems_gone(Some(
            "WDA health check failed after 30s on device 'x'"
        )));
        assert!(!device_seems_gone(None));
        // review-d7 m9: adb's commonest disconnect messages.
        for reason in [
            "error: closed",
            "error: connection reset by peer",
            "error: protocol fault (couldn't read status): Undefined error: 0",
            "error: cannot connect to daemon",
            "Failed to execute adb: timed out after 60.0s",
        ] {
            assert!(
                device_seems_gone(Some(&format!(
                    "ADB connection lost to device 'emulator-5554': {reason}"
                ))),
                "{reason}"
            );
        }
        // The same words from WDA are a hiccup, not a lost device.
        assert!(!device_seems_gone(Some(
            "Lost the connection to WebDriverAgent at http://localhost:8100: connection reset by peer"
        )));
        assert!(!device_seems_gone(Some(
            "WebDriverAgent request timed out after 30s"
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_actor_keeps_one_warm_executor_across_tests() {
        let dir = scratch("warm");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-warm-only".into()],
            bad: "none",
            infra: false,
        });
        let yaml = "appId: com.example\nconfig:\n  batch: true\n  sync:\n    timeoutMs: 200\ntests:\n  - name: a\n    steps:\n      - launchApp: {}\n      - assertVisible: { text: \"Go\" }\n  - name: b\n    steps:\n      - launchApp: {}\n      - assertVisible: { text: \"Go\" }\n";
        let suite = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        let result = ParallelRunner::run(suite, config(&dir), driver, 1)
            .await
            .unwrap();
        assert_eq!(result.passed, 2);
        let batched = result
            .tests
            .iter()
            .filter(|t| t.steps[0].action_name.contains("batched"))
            .count();
        assert_eq!(
            batched, 1,
            "the second test reused the first one's executor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`SplitDriver`] that logs installs and lookups per device, and can
    /// refuse installs on one device.
    struct Installing {
        inner: SplitDriver,
        broken_install: &'static str,
        log: std::sync::Mutex<Vec<String>>,
    }

    impl Installing {
        fn note(&self, s: String) {
            self.log.lock().unwrap().push(s);
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for Installing {
        async fn install_app_if_changed(
            &self,
            d: &str,
            _: &str,
            _: &str,
            hash: &str,
        ) -> Result<atalanta_common::InstallOutcome> {
            assert!(atalanta_common::install::is_sha256(hash));
            if d == self.broken_install {
                return Err(AtalantaError::Config(
                    "INSTALL_FAILED_INSUFFICIENT_STORAGE".into(),
                ));
            }
            self.note(format!("install {d}"));
            Ok(atalanta_common::InstallOutcome::Installed)
        }
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            self.inner.list_devices().await
        }
        async fn boot_device(&self, d: &str) -> Result<()> {
            self.inner.boot_device(d).await
        }
        async fn shutdown_device(&self, d: &str) -> Result<()> {
            self.inner.shutdown_device(d).await
        }
        async fn install_app(&self, d: &str, a: &str) -> Result<()> {
            self.inner.install_app(d, a).await
        }
        async fn launch_app(&self, d: &str, a: &str, c: bool) -> Result<()> {
            self.inner.launch_app(d, a, c).await
        }
        async fn stop_app(&self, d: &str, a: &str) -> Result<()> {
            self.inner.stop_app(d, a).await
        }
        async fn find_element(&self, d: &str, s: &Selector) -> Result<Element> {
            self.note(format!("find {d}"));
            self.inner.find_element(d, s).await
        }
        async fn find_elements(&self, d: &str, s: &Selector) -> Result<Vec<Element>> {
            self.note(format!("find {d}"));
            self.inner.find_elements(d, s).await
        }
        async fn get_hierarchy(&self, d: &str) -> Result<Element> {
            self.note(format!("find {d}"));
            self.inner.get_hierarchy(d).await
        }
        async fn tap(&self, d: &str, e: &Element) -> Result<()> {
            self.inner.tap(d, e).await
        }
        async fn double_tap(&self, d: &str, e: &Element) -> Result<()> {
            self.inner.double_tap(d, e).await
        }
        async fn long_press(&self, d: &str, e: &Element, ms: u64) -> Result<()> {
            self.inner.long_press(d, e, ms).await
        }
        async fn input_text(&self, d: &str, e: &Element, t: &str) -> Result<()> {
            self.inner.input_text(d, e, t).await
        }
        async fn clear_text(&self, d: &str, e: &Element) -> Result<()> {
            self.inner.clear_text(d, e).await
        }
        async fn swipe(&self, d: &str, dir: Direction) -> Result<()> {
            self.inner.swipe(d, dir).await
        }
        async fn swipe_coords(&self, d: &str, a: (i32, i32), b: (i32, i32)) -> Result<()> {
            self.inner.swipe_coords(d, a, b).await
        }
        async fn press_key(&self, d: &str, k: Key) -> Result<()> {
            self.inner.press_key(d, k).await
        }
        async fn screenshot(&self, d: &str) -> Result<Vec<u8>> {
            self.inner.screenshot(d).await
        }
        async fn screen_size(&self, d: &str) -> Result<(i32, i32)> {
            self.inner.screen_size(d).await
        }
        async fn get_element_text(&self, d: &str, e: &Element) -> Result<String> {
            self.inner.get_element_text(d, e).await
        }
        async fn is_element_visible(&self, d: &str, e: &Element) -> Result<bool> {
            self.inner.is_element_visible(d, e).await
        }
    }

    fn suite_with_app(tests: usize, dir: &std::path::Path) -> atalanta_common::TestSuite {
        let apk = dir.join("app.apk");
        std::fs::write(&apk, b"build").unwrap();
        let mut s = suite(tests, 0);
        s.config.app = Some(atalanta_common::AppSource::Path(
            apk.to_string_lossy().into_owned(),
        ));
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_leased_device_installs_the_app_before_its_first_test() {
        let dir = scratch("install");
        let driver = Arc::new(Installing {
            inner: SplitDriver {
                devices: vec!["par-inst-a".into(), "par-inst-b".into()],
                bad: "none",
                infra: false,
            },
            broken_install: "none",
            log: Default::default(),
        });
        let result = ParallelRunner::run(suite_with_app(6, &dir), config(&dir), driver.clone(), 2)
            .await
            .unwrap();
        assert_eq!(result.passed, 6, "{:#?}", result.tests);
        let log = driver.log.lock().unwrap().clone();
        for d in ["par-inst-a", "par-inst-b"] {
            let mine: Vec<&String> = log.iter().filter(|l| l.ends_with(d)).collect();
            assert_eq!(
                mine.iter().filter(|l| l.starts_with("install")).count(),
                1,
                "{log:?}"
            );
            if let Some(first) = mine.first() {
                assert!(
                    first.starts_with("install"),
                    "{d} ran a step first: {log:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_the_app_wont_install_on_is_left_out() {
        let dir = scratch("noinst");
        let driver = Arc::new(Installing {
            inner: SplitDriver {
                devices: vec!["par-noinst-good".into(), "par-noinst-full".into()],
                bad: "none",
                infra: false,
            },
            broken_install: "par-noinst-full",
            log: Default::default(),
        });
        let result = ParallelRunner::run(suite_with_app(4, &dir), config(&dir), driver.clone(), 2)
            .await
            .unwrap();
        assert_eq!(result.passed, 4, "{:#?}", result.tests);
        let log = driver.log.lock().unwrap().clone();
        assert!(
            !log.iter().any(|l| l.ends_with("par-noinst-full")),
            "{log:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn when_the_app_installs_nowhere_every_test_fails_with_the_reason() {
        let dir = scratch("nowhere");
        let driver = Arc::new(Installing {
            inner: SplitDriver {
                devices: vec!["par-nowhere-full".into()],
                bad: "none",
                infra: false,
            },
            broken_install: "par-nowhere-full",
            log: Default::default(),
        });
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            ParallelRunner::run(suite_with_app(2, &dir), config(&dir), driver, 1),
        )
        .await
        .expect("a run with no installable device must end, not wait for one")
        .unwrap();
        assert_eq!(result.failed, 2);
        assert!(
            result.tests[0]
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("didn't install"),
            "{:?}",
            result.tests[0].error_message
        );
        // VEL-046: a device problem, not the tests': exit 3, and kept out
        // of the history as infrastructure.
        assert!(
            result.tests.iter().all(|t| t.infrastructure_failure),
            "{:?}",
            result.tests
        );
        assert_eq!(result.exit_code(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parallel_hunt_spreads_each_tests_rounds_over_the_devices() {
        let dir = scratch("hunt");
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-hunt-good".into(), "par-hunt-bad".into()],
            bad: "par-hunt-bad",
            infra: false,
        });
        let cfg = RuntimeConfig {
            shuffle_seed: Some(7),
            jitter_ms: Some(5),
            ..config(&dir)
        };
        let (last, copies) = ParallelRunner::hunt(suite(2, 3), cfg, driver, 2, 4)
            .await
            .unwrap();
        assert_eq!(copies.len(), 8, "2 tests x 4 rounds");
        assert_eq!(last.total, 2, "one result per test");
        for name in ["t0", "t1"] {
            let mine: Vec<&CopyOutcome> = copies
                .iter()
                .filter(|c| c.result.test_name == name)
                .collect();
            let rounds: HashSet<u32> = mine
                .iter()
                .map(|c| match c.tag.kind {
                    CopyKind::Hunt { round, .. } => round,
                    _ => panic!("not a hunt copy"),
                })
                .collect();
            assert_eq!(rounds.len(), 4, "every round ran once");
            let on_bad = mine.iter().filter(|c| c.device == "par-hunt-bad").count();
            assert_eq!(on_bad, 2, "copies split evenly over the two devices");
            let by_device = crate::copies::tally_by_device(mine.iter().copied());
            assert!(
                crate::copies::device_effect(&by_device)
                    .unwrap()
                    .starts_with("fails only on par-hunt-bad (2/2)"),
                "{by_device:?}"
            );
        }
        assert!(
            !dir.join("atalanta-history.json").exists()
                || history::load(dir.to_str().unwrap())
                    .unwrap()
                    .outcomes
                    .is_empty(),
            "hunt rounds stay out of history"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// History in `dir` that flags `t0` flaky, failing 50% of its runs.
    fn flaky_history(dir: &std::path::Path) {
        use crate::history::HistoryOutcome::{Fail, Pass};
        let mut h = history::TestHistory::default();
        h.outcomes
            .insert("t0".into(), vec![Pass, Fail, Pass, Fail, Pass, Fail]);
        history::save(dir.to_str().unwrap(), &h).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flaky_test_gets_preventive_copies_on_different_devices() {
        let dir = scratch("copies");
        flaky_history(&dir);
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-copy-bad".into(), "par-copy-good".into()],
            bad: "par-copy-bad",
            infra: false,
        });
        let yaml = "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 200\ntests:\n  - name: t0\n    steps:\n      - assertVisible: { text: \"Go\" }\n  - name: t1\n    steps:\n      - assertVisible: { text: \"Go\" }\n";
        let suite = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        let cfg = RuntimeConfig {
            flaky_confidence: Some(0.99),
            ..config(&dir)
        };
        let result = ParallelRunner::run(suite, cfg, driver, 2).await.unwrap();
        assert_eq!(
            result.total, 2,
            "copies fold back into one result: {:#?}",
            result.tests
        );
        let t0 = result.tests.iter().find(|t| t.test_name == "t0").unwrap();
        let t1 = result.tests.iter().find(|t| t.test_name == "t1").unwrap();
        assert!(
            t1.warnings.iter().all(|w| !w.contains("copies")),
            "t1 isn't flagged"
        );
        // p_fail 0.5 at 99% needs 7 copies; the first passing copy (on the
        // good device) cancels the ones still queued. Whether the bad
        // device's copy started first is a race, so both endings are fine.
        let first = &t0.warnings[0];
        assert!(
            matches!(t0.status, TestStatus::Retried | TestStatus::Passed),
            "{t0:#?}"
        );
        assert!(first.contains("fails 50% of runs"), "{first}");
        if t0.status == TestStatus::Retried {
            assert!(
                first.contains("failed (on par-copy-bad), passed on par-copy-good"),
                "{first}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn copies_of_a_test_that_fails_everywhere_all_run_and_it_fails() {
        let dir = scratch("copyfail");
        flaky_history(&dir);
        let driver = Arc::new(SplitDriver {
            devices: vec!["par-copyfail-a".into(), "par-copyfail-b".into()],
            bad: "none",
            infra: false,
        });
        let yaml = "appId: com.example\nconfig:\n  artifacts:\n    onFailure: false\n  sync:\n    timeoutMs: 100\ntests:\n  - name: t0\n    steps:\n      - assertText: { selector: { text: \"Go\" }, expected: \"Stop\" }\n";
        let suite = atalanta_core::parser::parse_suite_from_str(yaml).unwrap();
        let cfg = RuntimeConfig {
            flaky_confidence: Some(0.9),
            ..config(&dir)
        };
        let result = ParallelRunner::run(suite, cfg, driver, 2).await.unwrap();
        assert_eq!(result.total, 1);
        let t0 = &result.tests[0];
        assert_eq!(t0.status, TestStatus::Failed, "{t0:#?}");
        // p_fail 0.5 at 90%: ceil(ln 0.1 / ln 0.5) = 4 copies, all failing.
        assert!(
            t0.warnings[0].starts_with("all 4 copies failed (on par-copyfail-a, par-copyfail-b)"),
            "{:?}",
            t0.warnings
        );
        assert_eq!(t0.failed_attempts.len(), 3);
        // Spread evenly: with the last copy's device, two per device.
        let on_a = t0
            .failed_attempts
            .iter()
            .filter(|e| e.starts_with("[par-copyfail-a]"))
            .count()
            + usize::from(t0.device.as_deref() == Some("par-copyfail-a"));
        assert_eq!(on_a, 2, "{t0:#?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
