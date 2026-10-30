//! `--cache-results`: skip a test that already passed when nothing it
//! depends on changed (emulator.wtf does this by default).
//!
//! The key hashes the app build (the driver's fingerprint: the build's
//! SHA-256 when Velocity installed it from `app:`, otherwise version and
//! install time on Android and the bundle's modification time on an iOS
//! simulator), the test's steps after flows are inlined and `${VAR}`s
//! substituted, the suite config, and the device model and OS. No
//! fingerprint, no caching. Tests marked `sideEffects: true`, and tests in a
//! `dependsOn` chain, always run.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use velocity_common::{PlatformDriver, SuiteConfig, TestCase, TestResult, TestStatus};

const FILE: &str = "velocity-result-cache.json";
/// Passes older than this are dropped on save: a build that old is gone.
const MAX_AGE_SECS: u64 = 30 * 24 * 3600;
/// And at most this many, newest kept.
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResultCache {
    /// key → (test name, unix seconds of the pass)
    entries: BTreeMap<String, (String, u64)>,
    #[serde(skip)]
    path: PathBuf,
    /// This run's changes, replayed onto the file as it is at save time.
    #[serde(skip)]
    added: BTreeMap<String, (String, u64)>,
    #[serde(skip)]
    removed: std::collections::BTreeSet<String>,
}

impl ResultCache {
    pub fn load(artifacts_dir: &str) -> Self {
        let path = Path::new(artifacts_dir).join(FILE);
        let mut cache: Self = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cache.path = path;
        cache
    }

    /// Save this run's passes and failures onto the cache file as it is
    /// now, under a lock: two runs sharing the folder each keep their
    /// entries, where the last to save used to drop the other's.
    pub fn save(&mut self) {
        let path = self.path.clone();
        let _ = velocity_common::fsutil::with_lock(&path, || {
            let mut on_disk =
                Self::load(&path.parent().unwrap_or(Path::new(".")).to_string_lossy());
            for key in &self.removed {
                on_disk.entries.remove(key);
            }
            on_disk.entries.extend(self.added.clone());
            on_disk.prune(now());
            if let Ok(json) = serde_json::to_string_pretty(&on_disk) {
                if velocity_common::fsutil::write_private(&path, json.as_bytes()).is_ok() {
                    // Saved: a later save must not replay them over newer
                    // changes another run made to the file meanwhile.
                    self.added.clear();
                    self.removed.clear();
                }
            }
            self.entries = on_disk.entries;
        });
    }

    /// Drop passes older than [`MAX_AGE_SECS`], then all but the newest
    /// [`MAX_ENTRIES`].
    fn prune(&mut self, now: u64) {
        self.entries
            .retain(|_, (_, at)| now.saturating_sub(*at) <= MAX_AGE_SECS);
        if self.entries.len() > MAX_ENTRIES {
            let mut times: Vec<u64> = self.entries.values().map(|(_, at)| *at).collect();
            times.sort_unstable_by(|a, b| b.cmp(a));
            let cutoff = times[MAX_ENTRIES - 1];
            self.entries.retain(|_, (_, at)| *at >= cutoff);
        }
    }

    /// A passing result to report instead of running, if `key` passed before.
    pub fn hit(&self, key: &str, test: &TestCase) -> Option<TestResult> {
        let (_, at) = self.entries.get(key)?;
        let mut r = TestResult::failed(&test.name, String::new(), std::time::Duration::ZERO);
        r.status = TestStatus::Passed;
        r.error_message = None;
        r.from_cache = true;
        r.warnings.push(format!(
            "not run: result cache (app, test, config and device unchanged since it passed {})",
            ago(*at)
        ));
        Some(r)
    }

    pub fn record(&mut self, key: String, result: &TestResult) {
        if result.status == TestStatus::Passed && !result.from_cache {
            let entry = (result.test_name.clone(), now());
            self.removed.remove(&key);
            self.added.insert(key.clone(), entry.clone());
            self.entries.insert(key, entry);
        } else if result.status == TestStatus::Failed {
            self.added.remove(&key);
            self.removed.insert(key.clone());
            self.entries.remove(&key);
        }
    }
}

/// Whether `test` may come from the cache at all. A test that compares
/// screenshots always runs: the key doesn't cover its baseline images, so
/// a changed or deleted baseline would never be compared (VEL-050). Nor
/// does it cover a server an `http` step talks to.
pub fn cacheable(test: &TestCase, all: &[TestCase]) -> bool {
    !test
        .tags
        .iter()
        .any(|t| t == velocity_core::parser::SIDE_EFFECTS_TAG)
        && !test.steps.iter().any(|s| {
            matches!(
                s.action,
                velocity_common::Action::AssertScreenshot { .. } | velocity_common::Action::Http(_)
            )
        })
        && test.depends_on.is_empty()
        && !all.iter().any(|t| t.depends_on.contains(&test.name))
}

/// The cache key for `test` on `device_id`, or `None` when the app build
/// can't be fingerprinted (then the test always runs).
pub async fn key(
    driver: &dyn PlatformDriver,
    device_id: &str,
    app_id: &str,
    config: &SuiteConfig,
    test: &TestCase,
) -> Option<String> {
    let app = driver.app_fingerprint(device_id, app_id).await?;
    let device = device_profile(driver, device_id).await?;
    // Where a step is written is not what it does: moving a test in the
    // file must not invalidate its cached pass.
    let steps: Vec<velocity_common::Step> = test
        .steps
        .iter()
        .map(|s| velocity_common::Step {
            source: None,
            ..s.clone()
        })
        .collect();
    let steps = serde_json::to_string(&steps).ok()?;
    // A build's path says nothing about the build (CI checkouts move); the
    // app fingerprint above is its hash when Velocity installed it.
    let mut config = config.clone();
    config.app = None;
    config.reset.app_path = None;
    let config = serde_json::to_string(&config).ok()?;
    Some(key_of(
        &[app_id, &app, &device, &test.name, &steps, &config],
        env!("CARGO_PKG_VERSION"),
        &machine_salt(),
    ))
}

/// The key over `parts`. With the Velocity version: a new release may run a
/// step differently, so its passes are not this one's (R-8). With a random
/// per-machine salt: steps hold interpolated values, and an unsalted 64-bit
/// FNV of a step typing a 4-digit PIN gave the PIN away to anyone holding
/// the uploaded cache file (VEL-S12).
fn key_of(parts: &[&str], version: &str, salt: &str) -> String {
    let mut all = vec![salt, version];
    all.extend_from_slice(parts);
    let text = all.join("\u{1f}");
    // Two independent 64-bit hashes: a 128-bit key.
    format!("{:016x}{:016x}", fnv(&text), fnv(&format!("{text}\u{1e}")))
}

/// The cache salt (see [`salt_from`]) for this run. `CI` and
/// `VELOCITY_CACHE_SALT` are read on every call through
/// [`velocity_common::env`], so under `velocity daemon` each client's own
/// environment decides; a process-wide cache let the first client decide
/// for every later one (review m6). Only the salt file's content, which is
/// the machine's, is kept once read.
fn machine_salt() -> String {
    salt_from(|k| velocity_common::env::var(k).ok(), salt_path())
}

/// The cache salt:
/// - `VELOCITY_CACHE_SALT` when set (from a CI secret, so a cache file
///   restored between CI runs still hits, and stays salted);
/// - otherwise in CI (`CI` set) or with no `HOME`: empty. A per-machine salt
///   made there is new on every ephemeral runner, so every key missed and
///   `--cache-results` silently did nothing (M-5). Nothing stable on a CI
///   runner is secret (repository, workflow and runner names are public),
///   so a salt derived from them would only look like protection; empty is
///   the honest choice, and the keys are then as before VEL-S12;
/// - otherwise this machine's own salt, `~/.velocity/cache-salt`
///   (owner-only), made on first use. Never under `/tmp`, which is shared.
///   Empty if it can't be read or written: the cache still works.
fn salt_from(var: impl Fn(&str) -> Option<String>, path: Option<std::path::PathBuf>) -> String {
    let set = |k: &str| {
        var(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let Some(salt) = set("VELOCITY_CACHE_SALT") {
        return salt;
    }
    // The detector healing uses (review MINOR 9): every CI variable, and
    // `CI=""` is not CI. It checked only `CI` and counted an empty one.
    let in_ci = velocity_common::env::in_ci_with(&var);
    match path {
        Some(path) if !in_ci => cached_file_salt(&path),
        _ => String::new(),
    }
}

/// [`file_salt`], read once per path per process (a key is worked out for
/// every test). An empty salt (the file couldn't be read or written) is
/// not kept, so a later run tries again.
fn cached_file_salt(path: &std::path::Path) -> String {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static SALTS: Mutex<Option<HashMap<std::path::PathBuf, String>>> = Mutex::new(None);
    let mut salts = SALTS.lock().unwrap_or_else(|e| e.into_inner());
    let salts = salts.get_or_insert_with(HashMap::new);
    if let Some(s) = salts.get(path) {
        return s.clone();
    }
    let salt = file_salt(path);
    if !salt.is_empty() {
        salts.insert(path.to_path_buf(), salt.clone());
    }
    salt
}

/// The salt stored at `path`, created when missing. Created complete under
/// a temporary name and hard-linked into place, which fails if another
/// process got there first: then that one's salt is read and used, so two
/// processes starting together agree on one salt.
fn file_salt(path: &std::path::Path) -> String {
    let read = || {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(s) = read() {
        return s;
    }
    let salt = random_hex();
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if velocity_common::fsutil::write_private(&tmp, salt.as_bytes()).is_err() {
        return String::new();
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => salt,
        Err(_) => read().unwrap_or_default(),
    }
}

#[cfg(not(test))]
fn salt_path() -> Option<std::path::PathBuf> {
    let home = velocity_common::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())?;
    Some(
        std::path::PathBuf::from(home)
            .join(".velocity")
            .join("cache-salt"),
    )
}

/// Never the user's `~/.velocity` from unit tests.
#[cfg(test)]
fn salt_path() -> Option<std::path::PathBuf> {
    Some(std::env::temp_dir().join(format!("velocity-test-salt-{}", std::process::id())))
}

/// 32 hex characters from std's per-process random hash keys.
fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2u8)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(i);
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
            );
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// Model and OS of `device_id`, from a cache refreshed every minute: one
/// `list_devices` per device, not per test.
async fn device_profile(driver: &dyn PlatformDriver, device_id: &str) -> Option<String> {
    static PROFILES: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(60);
    if let Some((p, at)) = PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(device_id)
    {
        if at.elapsed() < TTL {
            return Some(p.clone());
        }
    }
    let profile = driver
        .list_devices()
        .await
        .ok()?
        .into_iter()
        .find(|d| d.id == device_id)
        .map(|d| format!("{}|{}", d.name, d.os_version.unwrap_or_default()))?;
    PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(device_id.to_string(), (profile.clone(), Instant::now()));
    Some(profile)
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ago(at: u64) -> String {
    let s = now().saturating_sub(at);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    // R-8 / VEL-S12: the key changes with the Velocity version and with the
    // machine's salt, so a PIN can't be brute-forced from a shared cache
    // file without the salt.
    #[test]
    fn the_key_covers_version_and_salt() {
        let parts = ["app", "build", "dev", "t", "inputText 1234", "{}"];
        let k = key_of(&parts, "1.0.0", "salt-a");
        assert_eq!(k.len(), 32);
        assert_eq!(k, key_of(&parts, "1.0.0", "salt-a"));
        assert_ne!(k, key_of(&parts, "1.0.1", "salt-a"));
        assert_ne!(k, key_of(&parts, "1.0.0", "salt-b"));
        assert_eq!(machine_salt(), machine_salt());
    }

    // M-5: the salt lived in ~/.velocity, new on every hosted CI runner, so
    // a restored cache file never hit; with no HOME it went to shared /tmp.
    #[test]
    fn the_salt_is_stable_in_ci_and_overridable() {
        let dir = std::env::temp_dir().join(format!("vel-salt-m5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cache-salt");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        // The override wins everywhere.
        let over = env(&[("VELOCITY_CACHE_SALT", "from-secret"), ("CI", "true")]);
        assert_eq!(salt_from(over, Some(path.clone())), "from-secret");
        // CI without one: the same (empty) salt on every runner, no file.
        assert_eq!(salt_from(env(&[("CI", "true")]), Some(path.clone())), "");
        assert!(!path.exists(), "no per-runner salt file in CI");
        // No HOME: no salt file anywhere (never /tmp).
        assert_eq!(salt_from(env(&[]), None), "");
        // A workstation: one random salt, reused, also by a racing second
        // process that finds the file already there.
        // Another CI's variable counts too (Jenkins sets no `CI`).
        assert_eq!(
            salt_from(env(&[("JENKINS_URL", "http://ci")]), Some(path.clone())),
            ""
        );
        assert!(!path.exists(), "no per-runner salt file on Jenkins either");
        let a = salt_from(env(&[("CI", "false")]), Some(path.clone()));
        assert_eq!(a.len(), 32);
        assert_eq!(salt_from(env(&[]), Some(path.clone())), a);
        assert_eq!(file_salt(&path), a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Review m6: under `velocity daemon` the salt was decided once per
    // process, by the first client; a later client's `CI` and
    // `VELOCITY_CACHE_SALT` were ignored.
    #[test]
    fn each_client_environment_gets_its_own_salt() {
        // The process environment plus one variable, so tests running
        // alongside read what they would anyway.
        let client = |salt: Option<&str>| {
            let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
            env.remove("VELOCITY_CACHE_SALT");
            if let Some(s) = salt {
                env.insert("VELOCITY_CACHE_SALT".into(), s.into());
            }
            velocity_common::env::install(env);
            let salt = machine_salt();
            velocity_common::env::clear();
            salt
        };
        let local = client(None);
        assert_eq!(client(Some("from-ci-secret")), "from-ci-secret");
        assert_eq!(client(Some("another-secret")), "another-secret");
        assert_eq!(client(None), local, "the machine's salt is stable");
        assert_ne!(local, "from-ci-secret");
    }

    use super::*;

    fn case(name: &str, tags: &[&str], deps: &[&str]) -> TestCase {
        TestCase {
            name: name.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            isolated: false,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            steps: vec![],
            description: None,
            verify: None,
        }
    }

    #[test]
    fn side_effects_and_dependency_chains_always_run() {
        let a = case("a", &[], &[]);
        let b = case("b", &[], &["a"]);
        let mail = case("mail", &["side-effects"], &[]);
        let solo = case("solo", &[], &[]);
        let all = vec![a.clone(), b.clone(), mail.clone(), solo.clone()];
        assert!(!cacheable(&a, &all), "others depend on it");
        assert!(!cacheable(&b, &all));
        assert!(!cacheable(&mail, &all));
        assert!(cacheable(&solo, &all));
    }

    #[test]
    fn old_and_excess_passes_are_pruned_on_save() {
        let mut cache = ResultCache::default();
        let now = 10 * MAX_AGE_SECS;
        cache
            .entries
            .insert("stale".into(), ("t".into(), now - MAX_AGE_SECS - 1));
        for i in 0..MAX_ENTRIES + 10 {
            cache
                .entries
                .insert(format!("k{i}"), ("t".into(), now - i as u64));
        }
        cache.prune(now);
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(!cache.entries.contains_key("stale"));
        assert!(cache.entries.contains_key("k0"), "newest kept");
        assert!(!cache.entries.contains_key(&format!("k{}", MAX_ENTRIES)));
    }

    #[test]
    fn a_pass_is_served_until_the_test_fails() {
        let dir = std::env::temp_dir().join(format!("vel-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let t = case("solo", &[], &[]);
        let mut cache = ResultCache::load(dir.to_str().unwrap());
        assert!(cache.hit("k", &t).is_none());
        let mut pass = TestResult::failed("solo", String::new(), std::time::Duration::from_secs(3));
        pass.status = TestStatus::Passed;
        cache.record("k".into(), &pass);
        cache.save();
        let cache = ResultCache::load(dir.to_str().unwrap());
        let hit = cache.hit("k", &t).unwrap();
        assert_eq!(hit.status, TestStatus::Passed);
        assert!(hit.from_cache);
        assert!(hit.warnings[0].starts_with("not run: result cache"));
        let mut cache = cache;
        let fail = TestResult::failed("solo", "boom".into(), std::time::Duration::ZERO);
        cache.record("k".into(), &fail);
        assert!(cache.hit("k", &t).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-050: the key doesn't cover baseline images, so screenshot tests
    // always run.
    #[test]
    fn screenshot_tests_always_run() {
        let suite = velocity_core::parser::parse_suite_from_str(
            "appId: a\ntests:\n  - name: shot\n    steps:\n      - assertScreenshot: home\n  - name: plain\n    steps:\n      - wait: { ms: 1 }\n",
        )
        .unwrap();
        assert!(!cacheable(&suite.tests[0], &suite.tests));
        assert!(cacheable(&suite.tests[1], &suite.tests));
    }

    // Two runs sharing the artifacts folder: the second to save kept only
    // what it loaded at its start plus its own passes.
    #[test]
    fn concurrent_runs_keep_each_others_entries() {
        let dir = std::env::temp_dir().join(format!("velocity-rc-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_string_lossy().into_owned();
        let pass = |n: &str| {
            let mut r = TestResult::failed(n, String::new(), Duration::ZERO);
            r.status = TestStatus::Passed;
            r
        };
        let mut a = ResultCache::load(&d);
        let mut b = ResultCache::load(&d);
        a.record("ka".into(), &pass("a"));
        b.record("kb".into(), &pass("b"));
        a.save();
        b.save();
        let both = ResultCache::load(&d);
        assert!(both.entries.contains_key("ka") && both.entries.contains_key("kb"));
        // A failure removes only its own key.
        let mut c = ResultCache::load(&d);
        c.record(
            "ka".into(),
            &TestResult::failed("a", "x".into(), Duration::ZERO),
        );
        c.save();
        let after = ResultCache::load(&d);
        assert!(!after.entries.contains_key("ka") && after.entries.contains_key("kb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // N-3: a second save replayed the first save's changes over what
    // another run had written since.
    #[test]
    fn a_second_save_does_not_replay_the_first() {
        let dir = std::env::temp_dir().join(format!("velocity-rc-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_string_lossy().into_owned();
        let mut pass = TestResult::failed("a", String::new(), Duration::ZERO);
        pass.status = TestStatus::Passed;
        let mut a = ResultCache::load(&d);
        a.record("ka".into(), &pass);
        a.save();
        // Another run finds "a" broken and removes its pass.
        let mut b = ResultCache::load(&d);
        b.record(
            "ka".into(),
            &TestResult::failed("a", "x".into(), Duration::ZERO),
        );
        b.save();
        a.save();
        assert!(!ResultCache::load(&d).entries.contains_key("ka"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
