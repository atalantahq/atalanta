//! Install-by-hash: reinstall an app only when its build changed.
//!
//! A reinstall is the most expensive way to reset an app (SwiftHand
//! measured 30 s per restart), and CI hands Velocity the same build over and
//! over: every shard, every retry of a job, every device of a parallel run.
//! So the build is hashed on the host (SHA-256 of an `.apk`; a stable hash
//! over the files of an `.app` bundle) and each device remembers which hash
//! it has installed. The drivers keep that marker — Android on the device,
//! iOS on the host next to the simulator — together with a stamp of the
//! installed copy (install time, bundle path and mtime), so an app that was
//! reinstalled or removed behind Velocity's back is never mistaken for the
//! one it hashed.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

/// What [`crate::PlatformDriver::install_app_if_changed`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The build was (re)installed.
    Installed,
    /// The same build was already installed, and still is: nothing done.
    Skipped,
}

/// SHA-256 (hex) of an app build: the bytes of a file (`.apk`, `.ipa`), or
/// every entry of a directory (`.app`) in path order — relative path, kind,
/// executable bit and content — so the hash doesn't depend on the order the
/// filesystem lists files in or on timestamps a rebuild touches.
pub fn hash_app(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    let mut h = Sha256::new();
    if meta.is_file() {
        hash_file(&mut h, path)?;
    } else {
        let mut entries = Vec::new();
        walk(path, path, &mut entries)?;
        entries.sort();
        for rel in entries {
            let full = path.join(&rel);
            let m = std::fs::symlink_metadata(&full)?;
            let name = rel.to_string_lossy();
            if m.file_type().is_symlink() {
                let target = std::fs::read_link(&full)?;
                h.update(format!("L\0{name}\0{}\0", target.to_string_lossy()));
            } else if m.is_dir() {
                h.update(format!("D\0{name}\0"));
            } else {
                h.update(format!("F\0{name}\0{}\0{}\0", m.len(), executable(&m)));
                hash_file(&mut h, &full)?;
            }
        }
    }
    Ok(hex(&h.finalize()))
}

/// [`hash_app`], remembered while the build's file sizes and modification
/// times stay the same: in memory for the life of the process (a parallel
/// run asks once per device, `velocity daemon` across runs) and on disk in
/// `<installs_dir>/hashes.json`, so rerunning a suite against the same
/// build doesn't read the whole build again.
pub fn hash_app_cached(path: &Path) -> std::io::Result<String> {
    static CACHE: Mutex<Option<HashMap<PathBuf, (Stamp, String)>>> = Mutex::new(None);
    let key = std::fs::canonicalize(path)?;
    let now = stamp(&key)?;
    let remembered = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|c| c.get(&key).cloned())
        .or_else(|| {
            load_hashes()
                .get(&key.to_string_lossy().into_owned())
                .cloned()
        });
    if let Some((s, hash)) = remembered {
        if s == now {
            return Ok(hash);
        }
    }
    let hash = hash_app(&key)?;
    let mut disk = load_hashes();
    disk.insert(key.to_string_lossy().into_owned(), (now, hash.clone()));
    // Forget builds that are gone (CI workspaces, deleted temp dirs).
    disk.retain(|p, _| Path::new(p).exists());
    save_hashes(&disk);
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, (now, hash.clone()));
    Ok(hash)
}

type DiskHashes = HashMap<String, (Stamp, String)>;

fn hashes_path() -> PathBuf {
    installs_dir().join("hashes.json")
}

fn load_hashes() -> DiskHashes {
    std::fs::read_to_string(hashes_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best effort, through a temp file so concurrent runs never read half a
/// file.
fn save_hashes(h: &DiskHashes) {
    if let Ok(json) = serde_json::to_string(h) {
        let _ = crate::fsutil::write_private(&hashes_path(), json.as_bytes());
    }
}

/// The first 12 hex digits, for messages. Cut by characters: anything
/// else passed in must not panic mid-character.
pub fn short(hash: &str) -> &str {
    hash.char_indices()
        .nth(12)
        .map_or(hash, |(i, _)| &hash[..i])
}

/// Where host-side install markers live: `~/.velocity/installs`
/// (`VELOCITY_INSTALLS_DIR` overrides it).
pub fn installs_dir() -> PathBuf {
    crate::env::var("VELOCITY_INSTALLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::host::home_dir().join(".velocity").join("installs"))
}

/// The host-side marker for `app_id` on `device_id`:
/// `<installs_dir>/<device>/<app>.sha`.
pub fn host_marker_path(device_id: &str, app_id: &str) -> PathBuf {
    device_markers_dir(device_id).join(format!("{}.sha", file_safe(app_id)))
}

/// Every host-side marker of `device_id` (removed with the simulator).
pub fn device_markers_dir(device_id: &str) -> PathBuf {
    installs_dir().join(file_safe(device_id))
}

/// What a device has installed: the build's hash, and a stamp of the
/// installed copy taken right after the install (Android `lastUpdateTime`,
/// the simulator bundle's path and mtime). The marker only counts while the
/// stamp still matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub hash: String,
    pub stamp: String,
}

impl Marker {
    /// Two lines: hash, stamp.
    pub fn render(&self) -> String {
        format!("{}\n{}\n", self.hash, self.stamp)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut lines = s.lines().map(str::trim);
        let hash = lines.next().filter(|h| is_sha256(h))?.to_string();
        let stamp = lines.next().unwrap_or("").to_string();
        Some(Self { hash, stamp })
    }

    pub fn read(path: &Path) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.render())
    }

    /// Whether this marker says `hash` is installed and the installed copy
    /// still carries `stamp`. An empty stamp never matches: no evidence the
    /// app is still the one installed.
    pub fn matches(&self, hash: &str, stamp: &str) -> bool {
        self.hash == hash && !stamp.is_empty() && self.stamp == stamp
    }
}

/// 64 lowercase hex digits.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Fingerprint for the result cache when a marker vouches for the build.
pub fn fingerprint(hash: &str) -> String {
    format!("sha256:{hash}")
}

/// Sizes, entry count and newest mtime: cheap to collect (metadata only),
/// and any rebuild changes at least one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stamp {
    entries: u64,
    bytes: u64,
    /// Nanoseconds since the epoch.
    newest: u64,
}

fn stamp(path: &Path) -> std::io::Result<Stamp> {
    let mut s = Stamp {
        entries: 0,
        bytes: 0,
        newest: 0,
    };
    let mut add = |m: &std::fs::Metadata| {
        s.entries += 1;
        s.bytes += m.len();
        let t = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        s.newest = s.newest.max(t);
    };
    let meta = std::fs::metadata(path)?;
    add(&meta);
    if meta.is_dir() {
        let mut entries = Vec::new();
        walk(path, path, &mut entries)?;
        for rel in entries {
            add(&std::fs::symlink_metadata(path.join(rel))?);
        }
    }
    Ok(s)
}

/// Every entry under `dir` as a path relative to `root`, not following
/// symlinks. Finder's `.DS_Store` files are left out: opening the folder
/// shouldn't change the build's hash.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let is_dir = entry.file_type()?.is_dir();
        out.push(rel);
        if is_dir {
            walk(root, &path, out)?;
        }
    }
    Ok(())
}

fn hash_file(h: &mut Sha256, path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        h.update(&buf[..n]);
    }
}

#[cfg(unix)]
fn executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn file_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "velocity-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn short_cuts_at_twelve_characters() {
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
        // Byte 12 falls inside the `ü`; a byte cut gave six `é`s.
        assert_eq!(short("ééééééééééééé"), "éééééééééééé");
        assert_eq!(short("01234567890ü2345"), "01234567890ü");
    }

    #[test]
    fn a_file_hashes_to_its_sha256() {
        let dir = scratch("file");
        let apk = dir.join("app.apk");
        std::fs::write(&apk, b"abc").unwrap();
        assert_eq!(
            hash_app(&apk).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_bundle_hash_ignores_timestamps_and_ds_store_but_not_content_or_names() {
        let dir = scratch("bundle");
        let app = dir.join("App.app");
        std::fs::create_dir_all(app.join("Frameworks")).unwrap();
        std::fs::write(app.join("Info.plist"), b"plist").unwrap();
        std::fs::write(app.join("Frameworks/lib.dylib"), b"code").unwrap();
        let first = hash_app(&app).unwrap();
        assert!(is_sha256(&first));

        // Rewriting the same bytes (a no-op rebuild) and Finder's litter
        // leave it alone.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(app.join("Info.plist"), b"plist").unwrap();
        std::fs::write(app.join(".DS_Store"), b"finder").unwrap();
        assert_eq!(hash_app(&app).unwrap(), first);

        std::fs::write(app.join("Frameworks/lib.dylib"), b"CODE").unwrap();
        let changed = hash_app(&app).unwrap();
        assert_ne!(changed, first);

        std::fs::rename(
            app.join("Frameworks/lib.dylib"),
            app.join("Frameworks/b.dylib"),
        )
        .unwrap();
        assert_ne!(hash_app(&app).unwrap(), changed, "a rename is a change");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_cached_hash_follows_the_build_when_it_changes() {
        let dir = scratch("cached");
        let apk = dir.join("app.apk");
        std::fs::write(&apk, b"one").unwrap();
        let a = hash_app_cached(&apk).unwrap();
        assert_eq!(hash_app_cached(&apk).unwrap(), a);
        std::fs::write(&apk, b"two!").unwrap();
        assert_ne!(hash_app_cached(&apk).unwrap(), a);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_marker_counts_only_while_the_installed_copy_is_unchanged() {
        let hash = "a".repeat(64);
        let m = Marker {
            hash: hash.clone(),
            stamp: "2026-10-06 10:00:00".into(),
        };
        let back = Marker::parse(&m.render()).unwrap();
        assert_eq!(back, m);
        assert!(back.matches(&hash, "2026-10-06 10:00:00"));
        assert!(
            !back.matches(&hash, "2026-10-06 11:00:00"),
            "reinstalled since"
        );
        assert!(!back.matches(&"b".repeat(64), "2026-10-06 10:00:00"));
        assert!(!back.matches(&hash, ""), "no stamp, no proof");
        assert!(Marker::parse("not a hash\nx").is_none());
        assert!(Marker::parse("").is_none());
    }

    #[test]
    fn app_is_one_path_or_one_per_platform() {
        use crate::{AppSource, Platform};
        let one: AppSource = serde_json::from_str(r#""build/app.apk""#).unwrap();
        assert_eq!(one.for_platform(None), Some("build/app.apk"));
        assert_eq!(one.for_platform(Some(Platform::Ios)), Some("build/app.apk"));

        let both: AppSource =
            serde_json::from_str(r#"{"android": "a.apk", "ios": "App.app"}"#).unwrap();
        assert_eq!(both.for_platform(Some(Platform::Android)), Some("a.apk"));
        assert_eq!(both.for_platform(Some(Platform::Ios)), Some("App.app"));
        assert_eq!(both.for_platform(None), None, "which one is ambiguous");

        let android_only: AppSource = serde_json::from_str(r#"{"android": "a.apk"}"#).unwrap();
        assert_eq!(android_only.for_platform(Some(Platform::Ios)), None);
        assert!(serde_json::from_str::<AppSource>(r#"{"andriod": "a.apk"}"#).is_err());
    }

    #[test]
    fn host_markers_are_one_file_per_device_and_app() {
        let p = host_marker_path("ABC-123", "com.example.app");
        assert!(
            p.ends_with("ABC-123/com.example.app.sha"),
            "{}",
            p.display()
        );
        let odd = host_marker_path("host:5555/emulator-5554", "a b");
        assert!(
            odd.ends_with("host_5555_emulator-5554/a_b.sha"),
            "{}",
            odd.display()
        );
    }
}
