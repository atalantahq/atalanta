use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use velocity_common::{Action, NaturalKind, Platform};

/// On-disk format version. Files with another version are ignored (their
/// keys were computed differently), so an upgrade re-plans once.
///
/// v3: keys carry no test data. A step is keyed by its text as written
/// (`${VAR}`s unresolved, quoted values that were typed as data replaced by
/// slots) and whether it has `data`; the values are filled in on replay.
/// v2 hashed the data value into the key, so every new value re-planned.
const FORMAT_VERSION: u32 = 3;

/// A disk-backed map from a natural-language step to the action it resolved to.
///
/// This is what makes natural-language tests fast on reruns: the LLM is consulted
/// once per step, and every subsequent run replays the cached action with no model
/// call. Self-healing is [`NlCache::remove`] — drop a step whose action stopped
/// working so it re-plans.
///
/// Entries are written only for plans the run verified (see
/// [`crate::NaturalResolver::prepare`]); this type doesn't know about that,
/// it only stores.
///
/// Saving merges with whatever is on disk, so parallel shards writing the same
/// file don't drop each other's entries, and writes go through a temp file +
/// rename so a crash never leaves a half-written cache.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NlCache {
    #[serde(default)]
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
    /// Keys removed since load, so a merge on save doesn't resurrect them.
    #[serde(skip)]
    removed: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    /// The step as written (its template), kept for readability of the
    /// on-disk file. Never the resolved values.
    intent: String,
    #[serde(default)]
    platform: String,
    /// The action, with the step's data and data-like quoted values
    /// replaced by placeholders.
    action: Action,
    /// For an indexed selector: how many elements its base matched when it
    /// was planned. A different count on replay means the "twin" rows moved
    /// and the index may now hit another one (Arbigent's TwinsChanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    twins: Option<usize>,
    /// Whether the action changed the screen when it was planned. A replay
    /// that changes nothing when it used to is treated as a miss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect: Option<bool>,
    /// The entry fits only the value it was planned with: the selector
    /// holds part of it (`text="Hunter-Value-17 field"`), so it can't be
    /// filled in for another value (passmark's overfit guard). A
    /// fingerprint of that value, never the value; a step with a secret
    /// value is never stored this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound: Option<String>,
}

/// A cached resolution and what was observed when it was planned.
#[derive(Debug, Clone)]
pub struct Hit {
    pub action: Action,
    pub twins: Option<usize>,
    pub effect: Option<bool>,
    /// See [`NlCache::put_bound`]: the fingerprint of the only value this
    /// entry fits, if it fits only one.
    pub bound: Option<String>,
}

impl NlCache {
    /// Load a cache from disk, or return an empty one if the file is missing,
    /// unreadable or from another format version (a bad cache should never fail
    /// a test run).
    /// A file that doesn't parse (say, left with merge-conflict markers) is
    /// reported, not silently treated as empty (VEL-250).
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let cache = Self::read(path).unwrap_or_else(|e| {
            tracing::warn!(
                "velocity-nl: ignoring {} ({e}); it is moved aside on the next save",
                path.display()
            );
            Self::default()
        });
        if cache.version == FORMAT_VERSION {
            return cache;
        }
        if !cache.entries.is_empty() {
            // Not migrated: the old keys hashed values this file never
            // stored, so they can't be recomputed. Each step re-plans once.
            tracing::warn!(
                "velocity-nl: {} is cache format v{} ({} entries); ignoring them, so those \
                 steps re-plan once and are stored in format v{FORMAT_VERSION} (keys without \
                 test data)",
                path.display(),
                cache.version,
                cache.entries.len()
            );
        }
        Self {
            version: FORMAT_VERSION,
            ..Self::default()
        }
    }

    /// The file as stored: empty when it is missing, an error when it can't
    /// be read or parsed.
    fn read(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Persist the cache (best-effort; creates parent dirs). Entries another
    /// process wrote since we loaded are kept; ours win on conflict.
    ///
    /// The load-merge-write runs under a lock beside the file, so parallel
    /// shards never drop each other's entries (VEL-094). A file that doesn't
    /// parse is kept as `<name>.corrupt` rather than overwritten (VEL-250).
    pub fn save(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        velocity_common::fsutil::with_lock(path, || {
            let on_disk = match Self::read(path) {
                Ok(c) if c.version == FORMAT_VERSION => c,
                Ok(_) => Self::default(),
                Err(e) => {
                    let mut aside = path.as_os_str().to_owned();
                    aside.push(".corrupt");
                    tracing::warn!(
                        "velocity-nl: {} doesn't load ({e}); kept as {}",
                        path.display(),
                        Path::new(&aside).display()
                    );
                    std::fs::rename(path, &aside)?;
                    Self::default()
                }
            };
            for (key, entry) in on_disk.entries {
                if !self.removed.contains(&key) {
                    self.entries.entry(key).or_insert(entry);
                }
            }
            self.version = FORMAT_VERSION;
            let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
            velocity_common::fsutil::write_state(path, json.as_bytes())
        })?
    }

    /// Stable key for a step on a platform. Uses FNV-1a rather than
    /// `DefaultHasher`, whose output may change between Rust releases and
    /// would silently empty the cache on a toolchain upgrade. The platform is
    /// part of the key: the same step resolves to different selectors on web,
    /// iOS and Android. `scope` separates identical step text used on different
    /// screens (the executor passes the test name and occurrence number).
    ///
    /// `template` is the step as written, with no test data in it (see
    /// [`crate::NaturalStep::template`]); `has_data` is whether the step
    /// has a `data:` value. The value itself is never part of the key: it
    /// is filled into the cached action on replay.
    pub fn key(
        platform: Platform,
        kind: NaturalKind,
        template: &str,
        has_data: bool,
        scope: &str,
    ) -> String {
        let kind = match kind {
            NaturalKind::Act => "act",
            NaturalKind::Assert => "assert",
        };
        let mut h = Fnv1a::default();
        let platform = platform.to_string();
        for part in [
            platform.as_str(),
            kind,
            template.trim(),
            if has_data { "\u{1}data" } else { "" },
            scope,
        ] {
            h.write(part.as_bytes());
            h.write(&[0xff]); // separator so ("ab","c") != ("a","bc")
        }
        format!("{:016x}", h.0)
    }

    /// A fingerprint of a value an entry is bound to (see [`Hit::bound`]).
    pub fn fingerprint(value: &str) -> String {
        let mut h = Fnv1a::default();
        h.write(b"velocity-nl-bound\xff");
        h.write(value.as_bytes());
        format!("{:016x}", h.0)
    }

    pub fn get(&self, key: &str) -> Option<Action> {
        self.entries.get(key).map(|e| e.action.clone())
    }

    pub fn hit(&self, key: &str) -> Option<Hit> {
        self.entries.get(key).map(|e| Hit {
            action: e.action.clone(),
            twins: e.twins,
            effect: e.effect,
            bound: e.bound.clone(),
        })
    }

    pub fn put(&mut self, key: String, intent: &str, platform: Platform, action: Action) {
        self.put_observed(key, intent, platform, action, None, None);
    }

    /// Store with what the run observed (see [`Hit`]).
    pub fn put_observed(
        &mut self,
        key: String,
        intent: &str,
        platform: Platform,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
    ) {
        self.put_bound(key, intent, platform, action, twins, effect, None);
    }

    /// [`put_observed`](Self::put_observed) for an entry that fits only the
    /// value whose [`fingerprint`](Self::fingerprint) is `bound`.
    #[allow(clippy::too_many_arguments)]
    pub fn put_bound(
        &mut self,
        key: String,
        intent: &str,
        platform: Platform,
        action: Action,
        twins: Option<usize>,
        effect: Option<bool>,
        bound: Option<String>,
    ) {
        self.removed.remove(&key);
        self.entries.insert(
            key,
            CacheEntry {
                intent: intent.to_string(),
                platform: platform.to_string(),
                action,
                twins,
                effect,
                bound,
            },
        );
    }

    /// Drop every entry whose key isn't in `used`; returns how many went.
    pub fn retain_used(&mut self, used: &HashSet<String>) -> usize {
        let unused: Vec<String> = self
            .entries
            .keys()
            .filter(|k| !used.contains(*k))
            .cloned()
            .collect();
        for k in &unused {
            self.remove(k);
        }
        unused.len()
    }

    pub fn remove(&mut self, key: &str) -> bool {
        self.removed.insert(key.to_string());
        self.entries.remove(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::Selector;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("velnl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nl-cache.json")
    }

    fn tap(t: &str) -> Action {
        Action::Tap {
            selector: Selector::Text(t.into()),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp("roundtrip");
        let key = NlCache::key(Platform::Web, NaturalKind::Act, "tap login", false, "");
        let mut cache = NlCache::default();
        cache.put(key.clone(), "tap login", Platform::Web, tap("Login"));
        cache.save(&path).unwrap();

        let reloaded = NlCache::load(&path);
        assert_eq!(reloaded.len(), 1);
        assert!(matches!(
            reloaded.get(&key),
            Some(Action::Tap { selector: Selector::Text(t) }) if t == "Login"
        ));
    }

    #[test]
    fn key_is_stable_and_distinguishes_kind_and_platform() {
        let a1 = NlCache::key(Platform::Web, NaturalKind::Act, "do X", false, "");
        let a2 = NlCache::key(Platform::Web, NaturalKind::Act, " do X ", false, "");
        assert_eq!(a1, a2);
        assert_ne!(
            a1,
            NlCache::key(Platform::Web, NaturalKind::Assert, "do X", false, "")
        );
        assert_ne!(
            a1,
            NlCache::key(Platform::Android, NaturalKind::Act, "do X", false, "")
        );
        assert_ne!(
            a1,
            NlCache::key(Platform::Web, NaturalKind::Act, "do X", false, "t#1")
        );
        assert_ne!(
            a1,
            NlCache::key(Platform::Web, NaturalKind::Act, "do X", true, "")
        );
        // Pinned value: changing the key scheme must be a deliberate version bump.
        assert_eq!(a1, "d352acfaef89a801");
        assert_eq!(a1.len(), 16);
    }

    // The v2 format keyed on the data value; its entries can't be re-keyed
    // (the values were only hashed), so they are dropped with a log line
    // and overwritten by the next save, never a crash.
    #[test]
    fn a_v2_file_loads_empty_and_is_replaced_on_save() {
        let path = tmp("v2");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":2,"entries":{"0123456789abcdef":{"intent":"type the email","platform":"web","action":{"inputText":{"selector":{"id":"email"},"text":"ann@example.com"}}}}}"#,
        )
        .unwrap();
        let mut c = NlCache::load(&path);
        assert!(c.is_empty());
        c.put("k".into(), "tap", Platform::Web, tap("X"));
        c.save(&path).unwrap();
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("\"version\": 3"), "{file}");
        assert!(!file.contains("ann@example.com"), "{file}");
        assert_eq!(NlCache::load(&path).len(), 1);
    }

    #[test]
    fn bound_entries_keep_only_a_fingerprint() {
        let path = tmp("bound");
        let mut c = NlCache::default();
        let fp = NlCache::fingerprint("Hunter-Value-17");
        assert_ne!(fp, NlCache::fingerprint("Hunter-Value-18"));
        c.put_bound(
            "b".into(),
            "pick it",
            Platform::Web,
            tap("x"),
            None,
            None,
            Some(fp.clone()),
        );
        c.save(&path).unwrap();
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("Hunter-Value"));
        assert_eq!(NlCache::load(&path).hit("b").unwrap().bound, Some(fp));
    }

    #[test]
    fn missing_file_loads_empty() {
        assert!(NlCache::load("/nonexistent/path/nl-cache.json").is_empty());
    }

    #[test]
    fn old_format_is_ignored() {
        let path = tmp("v1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"entries":{"abc":{"intent":"x","action":{"refresh":null}}}}"#,
        )
        .unwrap();
        assert!(NlCache::load(&path).is_empty());
    }

    // VEL-094: load-merge-write raced, so parallel savers lost entries.
    #[test]
    fn parallel_savers_keep_every_entry() {
        let path = tmp("parallel");
        std::thread::scope(|s| {
            for i in 0..32 {
                let path = &path;
                s.spawn(move || {
                    let mut c = NlCache::load(path);
                    c.put(format!("k{i}"), "x", Platform::Web, tap("X"));
                    c.save(path).unwrap();
                });
            }
        });
        assert_eq!(NlCache::load(&path).len(), 32);
    }

    // VEL-250: a corrupt (merge-conflicted) file is moved aside, not lost.
    #[test]
    fn a_corrupt_file_is_kept_aside_not_overwritten() {
        let path = tmp("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conflicted = "<<<<<<< HEAD\n{\"version\":3}\n=======\n>>>>>>> b\n";
        std::fs::write(&path, conflicted).unwrap();
        let mut c = NlCache::load(&path);
        assert!(c.is_empty());
        c.put("c".into(), "c", Platform::Web, tap("C"));
        c.save(&path).unwrap();
        let aside = path.with_file_name("nl-cache.json.corrupt");
        assert_eq!(std::fs::read_to_string(aside).unwrap(), conflicted);
        assert_eq!(NlCache::load(&path).len(), 1);
    }

    #[test]
    fn save_merges_concurrent_writers_and_keeps_removals() {
        let path = tmp("merge");
        let (ka, kb) = ("a".to_string(), "b".to_string());

        let mut first = NlCache::load(&path);
        let mut second = NlCache::load(&path);
        first.put(ka.clone(), "a", Platform::Web, tap("A"));
        first.save(&path).unwrap();
        second.put(kb.clone(), "b", Platform::Web, tap("B"));
        second.save(&path).unwrap();

        let merged = NlCache::load(&path);
        assert!(merged.get(&ka).is_some() && merged.get(&kb).is_some());

        let mut third = NlCache::load(&path);
        assert!(third.remove(&ka));
        third.save(&path).unwrap();
        assert!(
            NlCache::load(&path).get(&ka).is_none(),
            "removal must persist"
        );
    }
}
