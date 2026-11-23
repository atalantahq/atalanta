// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::TestCase;

use crate::history::TestHistory;

/// Keep only tests that have at least one tag matching the provided list.
pub fn filter_by_tags(tests: &[TestCase], tags: &[String]) -> Vec<TestCase> {
    if tags.is_empty() {
        return tests.to_vec();
    }
    tests
        .iter()
        .filter(|t| t.tags.iter().any(|tag| tags.contains(tag)))
        .cloned()
        .collect()
}

/// Keep only tests whose name matches a glob-like pattern.
///
/// Supports `*` as a wildcard matching any sequence of characters. The match
/// is case-insensitive.
pub fn filter_by_name(tests: &[TestCase], pattern: &str) -> Vec<TestCase> {
    if pattern.is_empty() {
        return tests.to_vec();
    }
    tests
        .iter()
        .filter(|t| glob_match(pattern, &t.name))
        .cloned()
        .collect()
}

fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let value = value.to_lowercase();

    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return value == pattern;
    }

    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        // The last segment of a pattern not ending in '*' must end the
        // value: greedy first-match let "*_login" miss "a_login_b_login"
        // (VEL-181).
        if i == parts.len() - 1 && i > 0 && !pattern.ends_with('*') {
            return value.len() >= pos + part.len() && value[pos..].ends_with(part);
        }
        match value[pos..].find(part) {
            Some(found) => {
                // The first segment must anchor to the start if the pattern
                // doesn't begin with '*'.
                if i == 0 && found != 0 {
                    return false;
                }
                pos += found + part.len();
            }
            None => return false,
        }
    }

    // If the pattern doesn't end with '*', the value must end at exactly pos.
    if !pattern.ends_with('*') && pos != value.len() {
        return false;
    }

    true
}

/// Add every test that a selected test depends on (transitively), so a tag or
/// name filter can't strip a test of the setup it needs.
pub fn include_dependencies(selected: Vec<TestCase>, all: &[TestCase]) -> Vec<TestCase> {
    use std::collections::HashSet;
    let by_name: std::collections::HashMap<&str, &TestCase> =
        all.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut have: HashSet<String> = selected.iter().map(|t| t.name.clone()).collect();
    let mut out = selected;
    let mut i = 0;
    while i < out.len() {
        let deps = out[i].depends_on.clone();
        for dep in deps {
            if have.insert(dep.clone()) {
                if let Some(t) = by_name.get(dep.as_str()) {
                    out.push((*t).clone());
                }
            }
        }
        i += 1;
    }
    out
}

/// Reorder so every test comes after its dependencies, otherwise keeping the
/// given order (Kahn's algorithm, always taking the earliest ready test).
/// Dependencies outside `tests` are ignored.
pub fn order_with_dependencies(tests: Vec<TestCase>) -> Vec<TestCase> {
    use std::collections::HashMap;
    let index: HashMap<String, usize> = tests
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.clone(), i))
        .collect();
    let mut pending: Vec<usize> = tests
        .iter()
        .map(|t| {
            t.depends_on
                .iter()
                .filter(|d| index.contains_key(*d))
                .count()
        })
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); tests.len()];
    for (i, t) in tests.iter().enumerate() {
        for d in &t.depends_on {
            if let Some(&j) = index.get(d) {
                dependents[j].push(i);
            }
        }
    }
    let mut ready: std::collections::BTreeSet<usize> =
        (0..tests.len()).filter(|&i| pending[i] == 0).collect();
    let mut order = Vec::with_capacity(tests.len());
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &j in &dependents[i] {
            pending[j] -= 1;
            if pending[j] == 0 {
                ready.insert(j);
            }
        }
    }
    // A cycle (rejected by validation) would leave tests behind; keep them.
    let placed: std::collections::HashSet<usize> = order.iter().copied().collect();
    order.extend((0..tests.len()).filter(|i| !placed.contains(i)));
    let mut slots: Vec<Option<TestCase>> = tests.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// Split tests into groups that must run together (connected by `dependsOn`),
/// each in dependency order. Independent tests are groups of one. Groups keep
/// the relative order of their first member.
pub fn dependency_groups(tests: Vec<TestCase>) -> Vec<Vec<TestCase>> {
    use std::collections::HashMap;
    let index: HashMap<String, usize> = tests
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.clone(), i))
        .collect();
    // Union-find over dependency edges.
    let mut parent: Vec<usize> = (0..tests.len()).collect();
    fn find(p: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while p[r] != r {
            r = p[r];
        }
        let mut i = i;
        while p[i] != r {
            let next = p[i];
            p[i] = r;
            i = next;
        }
        r
    }
    for (i, t) in tests.iter().enumerate() {
        for d in &t.depends_on {
            if let Some(&j) = index.get(d) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut groups: Vec<(usize, Vec<TestCase>)> = Vec::new();
    let mut slot_of: HashMap<usize, usize> = HashMap::new();
    for (i, t) in tests.into_iter().enumerate() {
        let root = find(&mut parent, i);
        let slot = *slot_of.entry(root).or_insert_with(|| {
            groups.push((i, Vec::new()));
            groups.len() - 1
        });
        groups[slot].1.push(t);
    }
    groups
        .into_iter()
        .map(|(_, g)| order_with_dependencies(g))
        .collect()
}

/// A seeded shuffle that still respects `dependsOn`. Order-dependent tests are
/// common and invisible in a fixed order (82% of suites with such tests flake
/// when order changes — Lam et al. 2020), so a periodic shuffled run is how
/// they're found. The seed is printed so a failing order can be replayed.
/// Dependency groups move as units, so a dependent test still runs right
/// after the tests it builds on.
pub fn shuffle(tests: Vec<TestCase>, seed: u64) -> Vec<TestCase> {
    let mut groups = dependency_groups(tests);
    shuffle_groups(&mut groups, seed);
    groups.into_iter().flatten().collect()
}

fn shuffle_groups(groups: &mut [Vec<TestCase>], seed: u64) {
    let mut rng = SplitMix64(seed);
    // Fisher–Yates.
    for i in (1..groups.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        groups.swap(i, j);
    }
}

/// Tiny deterministic PRNG (SplitMix64) — enough for test ordering and
/// jitter, and reproducible from a printed seed.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Everything that decides which tests run and in what order, shared by the
/// sequential and parallel runners: tag and name filters, pulling in
/// dependencies, sharding (which keeps dependency groups whole), then
/// failed-first or shuffled ordering and dependency order.
///
/// Sharding comes before any reordering, and never looks at this run's own
/// history or seed: every CI shard computes the assignment independently,
/// so it may only depend on what all shards share — the selected tests and,
/// with `config.shard_timings`, a timings file they all read.
pub fn plan_tests(
    all: &[TestCase],
    config: &atalanta_common::RuntimeConfig,
    history: &TestHistory,
) -> Vec<TestCase> {
    let universe = shard_universe(all, config);
    let mut tests = match (config.shard_index, config.shard_total) {
        (Some(idx), Some(total)) => {
            let timings = shard_timings(config);
            shard_one(universe, idx, total, timings.as_ref())
        }
        _ => universe,
    };
    // R-1: history-derived selection runs after the partition, over the
    // tests this shard owns, so every shard partitions the same set.
    if let Some(post) = &config.post_shard {
        let only = config.only_tests.as_deref().unwrap_or(&[]);
        let owned = tests.clone();
        tests.retain(|t| only.contains(&t.name) || post.contains(&t.name));
        // Groups are never split, so dependencies are owned here too.
        tests = include_dependencies(tests, &owned);
    }
    // Reorder whole dependency groups, never their members: a dependent test
    // relies on the state its dependency left, so nothing may run between.
    let mut groups = dependency_groups(tests);
    if let Some(seed) = config.shuffle_seed {
        shuffle_groups(&mut groups, seed);
    } else if config.failed_first {
        groups.sort_by_key(|g| !g.iter().any(|t| history.failed_last(&t.name)));
    }
    groups.into_iter().flatten().collect()
}

/// The tests the shards partition: tag and name filters, then the
/// pre-shard (git-derived) `only_tests` unless a post-shard filter is set
/// (then the shards own the whole filtered suite), with dependencies.
/// NEW-001: a shard whose partition owns no test, while the suite has
/// tests to share, would pass with nothing run (more shards than tests,
/// or groups that can't be split). The reason, or `None` when the shard
/// has work (or there is no sharding, or nothing to share at all; an empty
/// selection is reported elsewhere). Selection made after the partition
/// (`--last-failed`, `--changed-since`) may still leave a shard empty on
/// purpose, so only the partition counts.
pub fn empty_shard(all: &[TestCase], config: &atalanta_common::RuntimeConfig) -> Option<String> {
    let (Some(idx), Some(total)) = (config.shard_index, config.shard_total) else {
        return None;
    };
    let universe = shard_universe(all, config);
    let shared = universe.len();
    if shared == 0 {
        return None;
    }
    let timings = shard_timings(config);
    if !shard_one(universe, idx, total, timings.as_ref()).is_empty() {
        return None;
    }
    Some(format!(
        "shard {}/{total} has no tests: {shared} test(s) (in dependency groups that are never split) \
         were shared out over {total} shards. Run fewer shards, so a shard never passes having run nothing",
        idx + 1
    ))
}

fn shard_universe(all: &[TestCase], config: &atalanta_common::RuntimeConfig) -> Vec<TestCase> {
    let mut tests = filter_by_tags(all, &config.tags);
    if let Some(ref pattern) = config.test_filter {
        tests = filter_by_name(&tests, pattern);
    }
    if config.post_shard.is_none() {
        if let Some(only) = &config.only_tests {
            tests.retain(|t| only.contains(&t.name));
        }
    }
    include_dependencies(tests, all)
}

/// The timings shards balance by: the ones the caller loaded (the CLI,
/// once, failing the run when they can't be read), else read here from
/// `shard_timings` (library callers), else none (balance test counts).
fn shard_timings(config: &atalanta_common::RuntimeConfig) -> Option<TestHistory> {
    if let Some(ms) = &config.shard_timings_ms {
        return Some(TestHistory {
            durations: (**ms).clone(),
            ..TestHistory::default()
        });
    }
    config.shard_timings.as_deref().and_then(|path| {
        crate::history::load_timings(std::path::Path::new(path))
            .map_err(|e| tracing::warn!("--shard-timings: {e}; sharding by name"))
            .ok()
    })
}

fn shard_one(
    tests: Vec<TestCase>,
    idx: usize,
    total: usize,
    timings: Option<&TestHistory>,
) -> Vec<TestCase> {
    match timings {
        Some(t) => shard_tests_by_timings(tests, idx, total, t),
        None => shard_by_count(tests, idx, total),
    }
}

/// Every shard's tests, by name, for `total` shards: what each CI job will
/// own. Each shard computes it independently; [`ShardPlan::fingerprint`]
/// lets CI check that they all computed the same one (D-23, D-49).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ShardPlan {
    /// Tests of shard `i` (0-based), in plan order.
    pub shards: Vec<Vec<String>>,
    /// FNV-1a over the ordered assignment, as 16 hex digits.
    pub fingerprint: String,
    /// Balanced by durations (`true`) or by test count.
    pub by_timings: bool,
}

/// The partition [`plan_tests`] uses, for every shard (`config`'s
/// `shard_index` is ignored; `shard_total` defaults to 1).
pub fn shard_plan(all: &[TestCase], config: &atalanta_common::RuntimeConfig) -> ShardPlan {
    let total = config.shard_total.unwrap_or(1).max(1);
    let universe = shard_universe(all, config);
    let timings = shard_timings(config);
    let shards: Vec<Vec<String>> = (0..total)
        .map(|i| {
            shard_one(universe.clone(), i, total, timings.as_ref())
                .into_iter()
                .map(|t| t.name)
                .collect()
        })
        .collect();
    let mut text = String::new();
    for (i, names) in shards.iter().enumerate() {
        text.push_str(&format!("#{i}\n"));
        for n in names {
            text.push_str(n);
            text.push('\n');
        }
    }
    ShardPlan {
        fingerprint: format!("{:016x}", stable_hash(&text)),
        shards,
        by_timings: timings.is_some(),
    }
}

/// Reject shard settings that would silently run nothing or everything: a
/// total of 0, an index past the last shard, or only one of the two.
pub fn check_shard(config: &atalanta_common::RuntimeConfig) -> atalanta_common::Result<()> {
    let err = |m: String| Err(atalanta_common::AtalantaError::Config(m));
    match (config.shard_index, config.shard_total) {
        (None, None) => Ok(()),
        (Some(_), None) | (None, Some(_)) => {
            err("--shard-index and --shard-total must be given together".into())
        }
        (Some(_), Some(0)) => err("--shard-total must be at least 1".into()),
        (Some(i), Some(t)) if i >= t => err(format!(
            "--shard-index {i} is out of range for --shard-total {t}: shards are numbered 0 to {}",
            t - 1
        )),
        _ => Ok(()),
    }
}

/// Sort `tests` longest-first by historical duration (stable for ties, so
/// suites without history keep their file order).
pub fn order_longest_first(tests: &mut [TestCase], history: &TestHistory) {
    tests.sort_by_key(|t| std::cmp::Reverse(history.estimate_ms(&t.name)));
}

/// Move tests whose last run failed to the front, keeping relative order
/// otherwise. Recently-failed-first beat every learned prioritisation
/// technique in a 59-technique comparison on long CI suites (Cheng et al.,
/// ISSTA 2024), and it needs nothing but outcome history.
pub fn order_failed_first(tests: &mut [TestCase], history: &TestHistory) {
    tests.sort_by_key(|t| !history.failed_last(&t.name));
}

/// Distribute tests across shards, balancing the number of tests.
///
/// The assignment depends only on the set of test names, not on their order
/// (a shuffled run shards the same way). Each CI shard has its own
/// `atalanta-history.json`, and balancing on it made shards disagree, so
/// some tests ran on no shard and others on two. For duration-balanced
/// shards, give every shard the same timings file and use
/// [`shard_tests_by_timings`].
pub fn shard_by_count(
    tests: Vec<TestCase>,
    shard_index: usize,
    shard_total: usize,
) -> Vec<TestCase> {
    shard_with(tests, shard_index, shard_total, |g| g.len() as u64)
}

/// [`shard_by_count`]; `_history` is ignored (see there why).
#[deprecated(
    note = "the history is ignored: use shard_by_count, or shard_tests_by_timings with a shared file"
)]
pub fn shard_tests(
    tests: Vec<TestCase>,
    shard_index: usize,
    shard_total: usize,
    _history: Option<&TestHistory>,
) -> Vec<TestCase> {
    shard_by_count(tests, shard_index, shard_total)
}

/// Duration-balanced sharding (longest group first onto the lightest shard)
/// from `timings`, which every shard must read identically (a history file
/// shared between CI jobs). Ties break on a stable hash of the name, never
/// on input order.
pub fn shard_tests_by_timings(
    tests: Vec<TestCase>,
    shard_index: usize,
    shard_total: usize,
    timings: &TestHistory,
) -> Vec<TestCase> {
    shard_with(tests, shard_index, shard_total, |g| {
        g.iter()
            .map(|t| timings.estimate_ms(&t.name))
            .fold(0u64, u64::saturating_add)
    })
}

/// Longest-processing-time assignment of whole dependency groups (dependent
/// tests must land on the same shard and device), in an order fixed by
/// (weight, name hash, name) so it is the same on every shard. Returns the
/// tests of `shard_index` in their input order.
fn shard_with(
    tests: Vec<TestCase>,
    shard_index: usize,
    shard_total: usize,
    weight: impl Fn(&[TestCase]) -> u64,
) -> Vec<TestCase> {
    if shard_total <= 1 {
        return tests;
    }
    let groups = dependency_groups(tests);
    let mut keyed: Vec<(usize, u64, u64, String)> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            // A group is named by its smallest member, whatever the order.
            let name = g.iter().map(|t| t.name.as_str()).min().unwrap_or("");
            (i, weight(g), stable_hash(name), name.to_string())
        })
        .collect();
    keyed.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    let mut loads = vec![0u64; shard_total];
    let mut assignment = vec![0usize; groups.len()];
    for (i, w, _, _) in keyed {
        // Lightest shard; the lowest index on a tie.
        let target = (0..shard_total).min_by_key(|&s| (loads[s], s)).unwrap_or(0);
        assignment[i] = target;
        loads[target] = loads[target].saturating_add(w.max(1));
    }
    groups
        .into_iter()
        .enumerate()
        .filter(|(i, _)| assignment[*i] == shard_index)
        .flat_map(|(_, g)| g)
        .collect()
}

/// FNV-1a: stable across Rust releases and processes, unlike `DefaultHasher`
/// — every shard must compute the same assignment.
fn stable_hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::{Action, Step};

    fn make_test(name: &str, tags: Vec<&str>) -> TestCase {
        TestCase {
            name: name.to_string(),
            tags: tags.into_iter().map(String::from).collect(),
            isolated: false,
            depends_on: vec![],
            steps: vec![Step {
                action: Action::Wait { ms: 100 },
                timeout_ms: None,
                settle: None,
                source: None,
                modifiers: Default::default(),
            }],
            description: None,
            verify: None,
        }
    }

    #[test]
    fn failed_first_moves_last_failures_forward_stably() {
        use atalanta_common::{TestResult, TestStatus};
        let mut tests = vec![
            make_test("a", vec![]),
            make_test("b", vec![]),
            make_test("c", vec![]),
        ];
        let mut hist = TestHistory::default();
        let mut failed = TestResult::failed("c", "x".into(), std::time::Duration::ZERO);
        crate::history::update(&mut hist, std::slice::from_ref(&failed));
        failed.test_name = "a".into();
        failed.status = TestStatus::Passed;
        crate::history::update(&mut hist, &[failed]);
        order_failed_first(&mut tests, &hist);
        let names: Vec<_> = tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b"]);
    }

    #[test]
    fn longest_first_uses_history_and_keeps_order_without_it() {
        let mut tests = vec![
            make_test("short", vec![]),
            make_test("long", vec![]),
            make_test("new", vec![]),
        ];
        let mut hist = TestHistory::default();
        order_longest_first(&mut tests, &hist);
        assert_eq!(tests[0].name, "short", "no history: file order");

        hist.durations.insert("short".into(), 1000);
        hist.durations.insert("long".into(), 60_000);
        order_longest_first(&mut tests, &hist);
        let names: Vec<_> = tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["long", "new", "short"]);
    }

    #[test]
    fn test_filter_by_tags_empty_tags_returns_all() {
        let tests = vec![make_test("a", vec!["smoke"]), make_test("b", vec!["reg"])];
        let result = filter_by_tags(&tests, &[]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_by_tags_matches() {
        let tests = vec![
            make_test("a", vec!["smoke", "fast"]),
            make_test("b", vec!["regression"]),
            make_test("c", vec!["smoke"]),
        ];
        let result = filter_by_tags(&tests, &["smoke".to_string()]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "a");
        assert_eq!(result[1].name, "c");
    }

    #[test]
    fn test_filter_by_tags_no_match() {
        let tests = vec![make_test("a", vec!["smoke"])];
        let result = filter_by_tags(&tests, &["nightly".to_string()]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_by_name_wildcard() {
        let tests = vec![
            make_test("Login flow", vec![]),
            make_test("Login error handling", vec![]),
            make_test("Dashboard view", vec![]),
        ];
        let result = filter_by_name(&tests, "Login*");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_by_name_case_insensitive() {
        let tests = vec![make_test("Login Flow", vec![])];
        let result = filter_by_name(&tests, "login*");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_filter_by_name_middle_wildcard() {
        let tests = vec![
            make_test("User login test", vec![]),
            make_test("User signup test", vec![]),
            make_test("Admin login test", vec![]),
        ];
        let result = filter_by_name(&tests, "User*test");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_shard_by_hash_deterministic() {
        let tests: Vec<TestCase> = (0..20)
            .map(|i| make_test(&format!("test_{i}"), vec![]))
            .collect();

        let shard0 = shard_by_count(tests.clone(), 0, 3);
        let shard1 = shard_by_count(tests.clone(), 1, 3);
        let shard2 = shard_by_count(tests.clone(), 2, 3);

        // Every test should be in exactly one shard
        let total = shard0.len() + shard1.len() + shard2.len();
        assert_eq!(total, 20);

        // Running again should produce the same assignment
        let shard0_again = shard_by_count(tests.clone(), 0, 3);
        assert_eq!(
            shard0.iter().map(|t| &t.name).collect::<Vec<_>>(),
            shard0_again.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_shard_by_hash_no_overlap() {
        let tests: Vec<TestCase> = (0..10)
            .map(|i| make_test(&format!("test_{i}"), vec![]))
            .collect();
        let shard0 = shard_by_count(tests.clone(), 0, 2);
        let shard1 = shard_by_count(tests.clone(), 1, 2);

        let names0: Vec<&str> = shard0.iter().map(|t| t.name.as_str()).collect();
        let names1: Vec<&str> = shard1.iter().map(|t| t.name.as_str()).collect();

        for name in &names0 {
            assert!(!names1.contains(name), "test {name} appears in both shards");
        }
    }

    #[test]
    fn test_shard_by_duration_balanced() {
        use std::collections::HashMap;

        let tests = vec![
            make_test("slow", vec![]),
            make_test("medium", vec![]),
            make_test("fast1", vec![]),
            make_test("fast2", vec![]),
        ];

        let mut durations = HashMap::new();
        durations.insert("slow".to_string(), 10000u64);
        durations.insert("medium".to_string(), 5000);
        durations.insert("fast1".to_string(), 1000);
        durations.insert("fast2".to_string(), 1000);

        let history = TestHistory {
            durations,
            ..Default::default()
        };

        let shard0 = shard_tests_by_timings(tests.clone(), 0, 2, &history);
        let shard1 = shard_tests_by_timings(tests.clone(), 1, 2, &history);

        assert_eq!(shard0.len() + shard1.len(), 4);

        // The slow test (10s) should be alone or paired with the smallest
        // tests, while medium gets the other fast tests. This checks that
        // the bin-packing doesn't put slow + medium in the same shard.
        let shard0_names: Vec<&str> = shard0.iter().map(|t| t.name.as_str()).collect();
        let shard1_names: Vec<&str> = shard1.iter().map(|t| t.name.as_str()).collect();

        let has_slow_and_medium_together = (shard0_names.contains(&"slow")
            && shard0_names.contains(&"medium"))
            || (shard1_names.contains(&"slow") && shard1_names.contains(&"medium"));
        assert!(
            !has_slow_and_medium_together,
            "slow and medium should be in different shards for balance"
        );
    }

    #[test]
    fn test_shard_single_shard_returns_all() {
        let tests = vec![make_test("a", vec![]), make_test("b", vec![])];
        let result = shard_by_count(tests.clone(), 0, 1);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_glob_exact_match() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_trailing_wildcard() {
        assert!(glob_match("hello*", "hello world"));
        assert!(glob_match("hello*", "hello"));
        assert!(!glob_match("hello*", "hi hello"));
    }

    #[test]
    fn test_glob_leading_wildcard() {
        assert!(glob_match("*world", "hello world"));
        // VEL-181: the last segment matches at the end, not first.
        assert!(glob_match("*_login", "a_login_b_login"));
        assert!(glob_match("x*y*z", "x_z_y_z"));
        assert!(!glob_match("*_login", "a_login_b"));
        assert!(!glob_match("*world", "world!"));
    }

    fn dep(name: &str, deps: &[&str]) -> TestCase {
        TestCase {
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            ..make_test(name, vec![])
        }
    }

    fn names(tests: &[TestCase]) -> Vec<&str> {
        tests.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn dependencies_run_first_and_order_is_otherwise_kept() {
        let tests = vec![
            dep("checkout", &["login", "cart"]),
            dep("x", &[]),
            dep("cart", &["login"]),
            dep("login", &[]),
        ];
        assert_eq!(
            names(&order_with_dependencies(tests)),
            ["x", "login", "cart", "checkout"]
        );
    }

    #[test]
    fn filtered_tests_pull_in_their_dependencies() {
        let all = vec![
            dep("login", &[]),
            dep("cart", &["login"]),
            dep("other", &[]),
        ];
        let picked = include_dependencies(vec![all[1].clone()], &all);
        assert_eq!(names(&order_with_dependencies(picked)), ["login", "cart"]);
    }

    #[test]
    fn groups_keep_dependent_tests_together() {
        let tests = vec![
            dep("a", &[]),
            dep("b", &["a"]),
            dep("c", &[]),
            dep("d", &["b"]),
        ];
        let groups = dependency_groups(tests);
        let g: Vec<Vec<&str>> = groups.iter().map(|g| names(g)).collect();
        assert_eq!(g, [vec!["a", "b", "d"], vec!["c"]]);
    }

    #[test]
    fn sharding_never_splits_a_group() {
        let tests: Vec<TestCase> = (0..20)
            .flat_map(|i| {
                [
                    dep(&format!("setup{i}"), &[]),
                    dep(&format!("use{i}"), &[&format!("setup{i}")]),
                ]
            })
            .collect();
        for total in [2, 3, 4] {
            let mut seen = 0;
            for idx in 0..total {
                let shard = shard_by_count(tests.clone(), idx, total);
                for t in &shard {
                    for d in &t.depends_on {
                        assert!(
                            shard.iter().any(|s| &s.name == d),
                            "{} split from {d}",
                            t.name
                        );
                    }
                }
                seen += shard.len();
            }
            assert_eq!(seen, tests.len());
        }
    }

    #[test]
    fn shuffle_is_seeded_and_respects_dependencies() {
        let tests: Vec<TestCase> = (0..10)
            .map(|i| dep(&format!("t{i}"), &[]))
            .chain([dep("after", &["t3"])])
            .collect();
        let a = shuffle(tests.clone(), 42);
        let b = shuffle(tests.clone(), 42);
        let c = shuffle(tests.clone(), 7);
        assert_eq!(names(&a), names(&b), "same seed, same order");
        assert_ne!(names(&a), names(&c), "different seed, different order");
        let pos = |v: &[TestCase], n: &str| v.iter().position(|t| t.name == n).unwrap();
        assert_eq!(
            pos(&a, "t3") + 1,
            pos(&a, "after"),
            "a group stays contiguous"
        );
        assert_ne!(names(&a), names(&tests), "actually shuffled");
    }

    #[test]
    fn planned_order_keeps_groups_contiguous_under_shuffle_and_failed_first() {
        use atalanta_common::RuntimeConfig;
        let all = vec![
            dep("net", &[]),
            dep("back", &["net"]),
            dep("battery", &[]),
            dep("wifi", &[]),
        ];
        let mut hist = TestHistory::default();
        crate::history::update(
            &mut hist,
            &[atalanta_common::TestResult::failed(
                "back",
                "x".into(),
                std::time::Duration::ZERO,
            )],
        );
        let failed_first = RuntimeConfig {
            failed_first: true,
            ..Default::default()
        };
        for config in (0..20u64)
            .map(|seed| RuntimeConfig {
                shuffle_seed: Some(seed),
                ..Default::default()
            })
            .chain([failed_first.clone()])
        {
            let order = plan_tests(&all, &config, &hist);
            let n = names(&order);
            let i = n.iter().position(|x| *x == "net").unwrap();
            assert_eq!(n[i + 1], "back", "{n:?}");
        }
        let ff = plan_tests(&all, &failed_first, &hist);
        assert_eq!(&names(&ff)[..2], ["net", "back"], "failed group first");
    }

    fn coverage(all: &[TestCase], seen: &[TestCase]) -> (Vec<String>, usize) {
        let uniq: std::collections::HashSet<&str> = seen.iter().map(|t| t.name.as_str()).collect();
        let missing = all
            .iter()
            .filter(|t| !uniq.contains(t.name.as_str()))
            .map(|t| t.name.clone())
            .collect();
        (missing, seen.len() - uniq.len())
    }

    // VEL-004: each CI shard has its own atalanta-history.json; the
    // assignment must not depend on it.
    #[test]
    fn shards_with_different_local_history_still_partition_the_suite() {
        let all: Vec<_> = (0..12)
            .map(|i| make_test(&format!("t{i:02}"), vec![]))
            .collect();
        let mut seen = Vec::new();
        for idx in 0..2usize {
            let mut h = TestHistory::default();
            for i in (idx..12).step_by(2) {
                h.durations
                    .insert(format!("t{i:02}"), 1000 + 500 * i as u64);
            }
            let config = atalanta_common::RuntimeConfig {
                shard_index: Some(idx),
                shard_total: Some(2),
                ..Default::default()
            };
            #[allow(deprecated)]
            seen.extend(shard_tests(all.clone(), idx, 2, Some(&h)));
            let planned = plan_tests(&all, &config, &h);
            assert_eq!(planned.len(), 6, "count-balanced");
        }
        assert_eq!(coverage(&all, &seen), (vec![], 0));
    }

    // VEL-004: shuffling with a per-shard seed must not move tests between
    // shards (shard first, then order).
    #[test]
    fn per_shard_shuffle_seeds_still_partition_the_suite() {
        let all: Vec<_> = (0..30)
            .map(|i| {
                dep(
                    &format!("test_{i:02}"),
                    if i % 7 == 1 { &["test_00"] } else { &[] },
                )
            })
            .collect();
        let mut hist = TestHistory::default();
        hist.durations.insert("test_00".into(), 9000);
        hist.durations.insert("test_01".into(), 3000);
        let mut seen = Vec::new();
        for idx in 0..3usize {
            let cfg = atalanta_common::RuntimeConfig {
                shuffle_seed: Some(1000 + idx as u64 * 7919),
                shard_index: Some(idx),
                shard_total: Some(3),
                ..Default::default()
            };
            seen.extend(plan_tests(&all, &cfg, &hist));
        }
        assert_eq!(coverage(&all, &seen), (vec![], 0));
    }

    // VEL-004: timings-balanced sharding breaks ties by name, not by input
    // order, so reordered input gives the same shards.
    #[test]
    fn timing_shards_ignore_input_order_and_tie_on_name() {
        let all: Vec<_> = (0..9)
            .map(|i| make_test(&format!("t{i}"), vec![]))
            .collect();
        let mut timings = TestHistory::default();
        for t in &all {
            timings.durations.insert(t.name.clone(), 1000);
        }
        let mut reversed = all.clone();
        reversed.reverse();
        for idx in 0..3 {
            let mut a = names(&shard_tests_by_timings(all.clone(), idx, 3, &timings))
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>();
            let mut b = names(&shard_tests_by_timings(reversed.clone(), idx, 3, &timings))
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>();
            a.sort();
            b.sort();
            assert_eq!(a, b);
            assert_eq!(a.len(), 3);
        }
    }

    #[test]
    fn plan_uses_a_shared_timings_file_for_balance() {
        let dir = std::env::temp_dir().join(format!("vel-timings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut timings = TestHistory::default();
        timings.durations.insert("slow".into(), 10_000);
        for n in ["a", "b", "c"] {
            timings.durations.insert(n.into(), 3000);
        }
        crate::history::save(dir.to_str().unwrap(), &timings).unwrap();
        let all: Vec<_> = ["a", "b", "c", "slow"]
            .iter()
            .map(|n| make_test(n, vec![]))
            .collect();
        let shard = |idx| {
            let cfg = atalanta_common::RuntimeConfig {
                shard_index: Some(idx),
                shard_total: Some(2),
                shard_timings: Some(dir.to_string_lossy().into_owned()),
                ..Default::default()
            };
            plan_tests(&all, &cfg, &TestHistory::default())
        };
        let (s0, s1) = (shard(0), shard(1));
        let _ = std::fs::remove_dir_all(&dir);
        let slow_alone = [&s0, &s1].iter().any(|s| names(s) == ["slow"]);
        assert!(slow_alone, "{:?} / {:?}", names(&s0), names(&s1));
    }

    // NEW-001: more shards than tests leaves a shard that would pass empty.
    #[test]
    fn a_shard_that_owns_no_test_is_named() {
        let all: Vec<TestCase> = ["a", "b"]
            .iter()
            .map(|n| TestCase {
                name: n.to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![],
                description: None,
                verify: None,
            })
            .collect();
        let cfg = |i, t| atalanta_common::RuntimeConfig {
            shard_index: Some(i),
            shard_total: Some(t),
            ..Default::default()
        };
        assert!(empty_shard(&all, &cfg(0, 2)).is_none());
        assert!(empty_shard(&all, &cfg(1, 2)).is_none());
        let empty: Vec<_> = (0..3)
            .filter_map(|i| empty_shard(&all, &cfg(i, 3)))
            .collect();
        assert_eq!(empty.len(), 1, "{empty:?}");
        assert!(
            empty[0].contains("has no tests") && empty[0].contains("over 3 shards"),
            "{empty:?}"
        );
        assert!(empty_shard(&all, &atalanta_common::RuntimeConfig::default()).is_none());
        assert!(empty_shard(&[], &cfg(2, 3)).is_none());
    }

    // VEL-003: out-of-range or partial shard settings are errors, not an
    // empty (or full) green run.
    #[test]
    fn bad_shard_settings_are_rejected() {
        let cfg = |i, t| atalanta_common::RuntimeConfig {
            shard_index: i,
            shard_total: t,
            ..Default::default()
        };
        assert!(check_shard(&cfg(None, None)).is_ok());
        assert!(check_shard(&cfg(Some(1), Some(2))).is_ok());
        for bad in [
            cfg(Some(5), Some(2)),
            cfg(Some(2), Some(2)),
            cfg(Some(0), Some(0)),
            cfg(Some(0), None),
            cfg(None, Some(3)),
        ] {
            let e = check_shard(&bad).unwrap_err();
            assert_eq!(e.exit_code(), 2, "{e}");
        }
    }
    fn owned_names(tests: &[TestCase]) -> Vec<String> {
        tests.iter().map(|t| t.name.clone()).collect()
    }

    // R-1: each shard's history differs. Filtering by it before the
    // partition dropped a test shard A's history chose but shard B owned;
    // now the history filter runs on the tests each shard owns.
    #[test]
    fn history_selection_never_drops_a_test_across_shards() {
        use std::collections::HashSet;
        use std::sync::Arc;
        let all: Vec<TestCase> = (0..24)
            .map(|i| make_test(&format!("t{i:02}"), vec![]))
            .collect();
        let git: Vec<String> = vec!["t01".into(), "t05".into(), "t17".into()];
        let total = 4;
        // Shard k's local history flags its own set of risky tests.
        let risky: Vec<HashSet<String>> = (0..total)
            .map(|k| {
                (0..24)
                    .filter(|i| (i * 7 + k * 3) % 5 == 0)
                    .map(|i| format!("t{i:02}"))
                    .collect()
            })
            .collect();
        let owner = shard_plan(
            &all,
            &atalanta_common::RuntimeConfig {
                shard_total: Some(total),
                ..Default::default()
            },
        );
        let mut ran: Vec<String> = Vec::new();
        for (k, risky_k) in risky.iter().enumerate() {
            let cfg = atalanta_common::RuntimeConfig {
                shard_index: Some(k),
                shard_total: Some(total),
                only_tests: Some(git.clone()),
                post_shard: Some(Arc::new(risky_k.clone())),
                ..Default::default()
            };
            let mine = owned_names(&plan_tests(&all, &cfg, &TestHistory::default()));
            // Every risky test this shard owns runs here.
            for t in risky_k.iter().filter(|t| owner.shards[k].contains(t)) {
                assert!(
                    mine.contains(t),
                    "shard {k} dropped its risky {t}: {mine:?}"
                );
            }
            ran.extend(mine);
        }
        // Every git-selected test runs, and no test runs twice.
        for g in &git {
            assert_eq!(ran.iter().filter(|r| *r == g).count(), 1, "{g}: {ran:?}");
        }
        let unique: HashSet<&String> = ran.iter().collect();
        assert_eq!(unique.len(), ran.len(), "shards overlap: {ran:?}");
    }

    // R-1: `--last-failed` on a shard reruns the failures it owns.
    #[test]
    fn last_failed_reruns_only_owned_failures() {
        use std::sync::Arc;
        let all: Vec<TestCase> = (0..10)
            .map(|i| make_test(&format!("t{i}"), vec![]))
            .collect();
        let failed: std::collections::HashSet<String> = ["t1", "t2", "t3", "t8"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let full = atalanta_common::RuntimeConfig {
            shard_total: Some(2),
            ..Default::default()
        };
        let owner = shard_plan(&all, &full);
        for k in 0..2 {
            let cfg = atalanta_common::RuntimeConfig {
                shard_index: Some(k),
                post_shard: Some(Arc::new(failed.clone())),
                ..full.clone()
            };
            let mine = owned_names(&plan_tests(&all, &cfg, &TestHistory::default()));
            let expect: Vec<String> = owner.shards[k]
                .iter()
                .filter(|t| failed.contains(*t))
                .cloned()
                .collect();
            assert_eq!(mine, expect, "shard {k}");
        }
    }

    // D-23: timings loaded by the caller are used as is, and the plan's
    // fingerprint is the same for the same inputs and differs otherwise.
    #[test]
    fn the_plan_fingerprint_follows_the_assignment() {
        use std::sync::Arc;
        let all: Vec<TestCase> = (0..8)
            .map(|i| make_test(&format!("t{i}"), vec![]))
            .collect();
        let base = atalanta_common::RuntimeConfig {
            shard_total: Some(3),
            ..Default::default()
        };
        let a = shard_plan(&all, &base);
        assert_eq!(a, shard_plan(&all, &base));
        assert!(!a.by_timings);
        assert_eq!(a.shards.iter().map(Vec::len).sum::<usize>(), 8);
        let skewed: std::collections::HashMap<String, u64> = (0..8)
            .map(|i| (format!("t{i}"), if i == 0 { 600_000u64 } else { 1_000 }))
            .collect();
        let timed = atalanta_common::RuntimeConfig {
            shard_timings_ms: Some(Arc::new(skewed)),
            // Never read: the loaded timings win.
            shard_timings: Some("/no/such/file".into()),
            ..base.clone()
        };
        let b = shard_plan(&all, &timed);
        assert!(b.by_timings);
        assert_ne!(a.fingerprint, b.fingerprint);
        // The long test is alone on its shard.
        assert!(
            b.shards.iter().any(|s| s == &vec!["t0".to_string()]),
            "{b:?}"
        );
        // Each shard's plan_tests matches the plan.
        for (i, s) in b.shards.iter().enumerate() {
            let cfg = atalanta_common::RuntimeConfig {
                shard_index: Some(i),
                ..timed.clone()
            };
            assert_eq!(
                &owned_names(&plan_tests(&all, &cfg, &TestHistory::default())),
                s
            );
        }
    }
}
