// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! File writes that readers never see half done.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` through a temp file and a rename, creating the
/// parent directory. A concurrent reader sees the old file or the new one,
/// never a torn one; the temp name is unique per process and per call, so
/// two writers in one process (one per device actor) never share it.
///
/// - The temp file is created new (`O_EXCL`), so a link planted at its
///   predictable name is never written through, and the rename replaces a
///   link at `path` rather than following it.
/// - It keeps the mode `path` already had (a new file gets the umask's).
/// - Temp files a killed writer left behind are removed after an hour
///   (looked for once per directory per process).
/// - Not synced: reports and traces are rewritten by the next run, and a
///   full sync per file (`F_FULLFSYNC` on macOS, 5-30 ms) made a `--trace`
///   run of a large suite pay it thousands of times (m-7). State files go
///   through [`write_state`] or [`write_private`].
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mode = existing_mode(path);
    write_with_mode(path, bytes, mode, false)
}

/// [`write_atomic`] for state a crash must not lose (healed selectors, a
/// cache): the data and the rename are synced, so a crash leaves the old
/// file or the new one, never an empty one.
pub fn write_state(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mode = existing_mode(path);
    write_with_mode(path, bytes, mode, true)
}

/// [`write_state`] for state only its owner should read (history, the
/// result cache, a profile): mode 0600, and a parent created 0700.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        create_private_dir(dir)?;
    }
    write_with_mode(path, bytes, Some(0o600), true)
}

/// `create_dir_all`, with the directories it creates 0700 (on Unix).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// The permission bits of the regular file at `path`, if there is one.
fn existing_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// A temp-name suffix no other process will pick: the pid and a
/// per-process sequence alone collided when a container reused the pid of
/// a writer killed mid-write (its temp file still there), and every later
/// write failed with `AlreadyExists` (m-6).
fn random_suffix() -> u64 {
    use std::hash::BuildHasher;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::collections::hash_map::RandomState::new().hash_one(nanos)
}

fn write_with_mode(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    durable: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    remove_stale_temps(dir.unwrap_or(Path::new(".")), &name);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    // A fresh name on the (unlikely) clash with a leftover.
    let mut attempt = 0;
    let (tmp, file) = loop {
        let tmp = path.with_file_name(format!(
            ".{name}.{}.{}.{:016x}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            random_suffix()
        ));
        match open.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 8 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let written = (|| {
        let mut f = file;
        f.write_all(bytes)?;
        // The umask may have narrowed the mode; the existing file's wins.
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        if durable {
            f.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename itself survives a crash once the directory is synced.
    if durable {
        if let Some(d) = dir.and_then(|d| std::fs::File::open(d).ok()) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Remove `.<name>.*.tmp` files older than an hour from `dir`: a writer
/// killed between write and rename leaves its temp file behind. Once per
/// directory and name per process: a sweep per write made writing n files
/// into one directory (trace trees) O(n^2).
fn remove_stale_temps(dir: &Path, name: &str) {
    static SWEPT: std::sync::Mutex<Option<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::Mutex::new(None);
    {
        let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
        if !swept
            .get_or_insert_with(Default::default)
            .insert(dir.join(name))
        {
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!(".{name}.");
    let hour = std::time::Duration::from_secs(3600);
    for e in entries.flatten() {
        let file = e.file_name();
        let file = file.to_string_lossy();
        if !(file.starts_with(&prefix) && file.ends_with(".tmp")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > hour);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Run `f` holding an exclusive lock for `path` (an OS lock on a
/// `.<name>.lock` file beside it, released on return), for a
/// read-modify-write of a state file several runs share: each re-reads the
/// file inside `f`, so no run overwrites another's update with the copy it
/// read at its start.
pub fn with_lock<T>(path: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut open = std::fs::OpenOptions::new();
    open.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let lock = open.open(path.with_file_name(format!(".{name}.lock")))?;
    lock.lock()?;
    Ok(f())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_leave_one_whole_file_and_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("atalanta-fsutil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("f.json");
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..50 {
                        write_atomic(path, &vec![b'a' + i; 4096]).unwrap();
                    }
                });
            }
        });
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(bytes.iter().all(|b| *b == bytes[0]), "torn write");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("f.json")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn writes_keep_the_mode_never_follow_links_and_private_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atalanta-fsmode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // An existing file keeps its mode (it was reset to the umask's).
        let report = dir.join("junit.xml");
        std::fs::write(&report, "old").unwrap();
        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&report, b"new").unwrap();
        assert_eq!(mode(&report), 0o640);
        assert_eq!(std::fs::read(&report).unwrap(), b"new");

        // A link at the target is replaced, never written through.
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let state = dir.join("state").join("atalanta-history.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &state).unwrap();
        write_private(&state, b"{}").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(!std::fs::symlink_metadata(&state).unwrap().is_symlink());
        assert_eq!(mode(&state), 0o600);

        // A private file's new folder is 0700.
        let deep = dir.join("new-private").join("cache.json");
        write_private(&deep, b"{}").unwrap();
        assert_eq!(mode(deep.parent().unwrap()), 0o700);

        // A stale temp file from a killed writer is cleaned up (on the
        // first write of that file in this process).
        let fresh = dir.join("fresh.xml");
        let stale = dir.join(".fresh.xml.1.0.tmp");
        std::fs::write(&stale, "x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        write_atomic(&fresh, b"again").unwrap();
        assert!(!stale.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // m-6: a temp file a killed writer left under the very name this
    // process would pick next (a container reusing the pid) doesn't stop
    // the write.
    #[test]
    fn a_leftover_temp_file_with_our_pid_does_not_block_a_write() {
        let dir = std::env::temp_dir().join(format!("atalanta-fspid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Every name the old scheme (`.<name>.<pid>.<seq>.tmp`) could use
        // for the next few thousand writes of this process.
        let pid = std::process::id();
        for seq in 0..3000 {
            std::fs::write(dir.join(format!(".h.json.{pid}.{seq}.tmp")), "").unwrap();
        }
        write_state(&dir.join("h.json"), b"{}").expect("write despite leftovers");
        assert_eq!(std::fs::read(dir.join("h.json")).unwrap(), b"{}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
