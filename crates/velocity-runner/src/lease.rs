//! Device leases shared across processes.
//!
//! The farm already hands each device to one test at a time *within* a run.
//! These locks extend that to every Velocity process on the machine — two CLI
//! runs, or a run and an MCP agent, can't drive the same device at once. An
//! OS file lock (`flock`) per device under `~/.velocity/leases/` is released
//! automatically when the holder exits, even if it crashes, so a stale lease
//! can't wedge a device.
//!
//! Where the OS can't lock, Velocity follows cargo (D-25): a lease dir on
//! NFS skips cross-process leasing (`flock` there can block forever, even
//! non-blocking); a lock call that returns ENOTSUP/ENOSYS warns and runs
//! without one; any other error fails at once (exit 3) with a hint.
//! `VELOCITY_LEASES=off` turns cross-process leasing off.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// A held lease; dropping it releases the device. Without a file when
/// cross-process leasing is off or unsupported where the leases live.
pub struct DeviceLock {
    _file: Option<File>,
}

impl DeviceLock {
    pub(crate) fn unleased() -> Self {
        Self { _file: None }
    }
}

/// What to tell a user whose lease dir can't be used.
const HINT: &str =
    "set VELOCITY_LEASE_DIR to a writable local directory (or VELOCITY_LEASES=off to run without cross-process leases)";

/// `VELOCITY_LEASES=off` (or `0`, `false`, `no`).
fn leasing_off() -> bool {
    velocity_common::env::var("VELOCITY_LEASES").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    })
}

/// Print `msg` once per process (every device would repeat it).
fn warn_once(msg: String) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!("{msg}");
        eprintln!("warning: {msg}");
    });
}

/// How a failed lock call is handled (cargo's `flock.rs` rules).
#[derive(Debug, PartialEq, Eq)]
enum LockFailure {
    /// The filesystem can't lock (ENOTSUP, EOPNOTSUPP, ENOSYS): run
    /// without a cross-process lease.
    Unsupported,
    /// Anything else: fail with a hint.
    Fatal,
}

fn classify(e: &std::io::Error) -> LockFailure {
    if e.kind() == std::io::ErrorKind::Unsupported {
        return LockFailure::Unsupported;
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        if code == libc::ENOTSUP || code == libc::EOPNOTSUPP || code == libc::ENOSYS {
            return LockFailure::Unsupported;
        }
    }
    LockFailure::Fatal
}

/// Whether `dir` (or its nearest existing ancestor) is on NFS.
fn on_nfs(dir: &std::path::Path) -> bool {
    let Some(existing) = dir.ancestors().find(|p| p.exists()) else {
        return false;
    };
    fs_type_is_nfs(existing)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn fs_type_is_nfs(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `statfs` is plain old data; all zeroes is a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: statfs(2) fills the struct we pass; `c` is a valid
    // NUL-terminated path.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: f_fstypename is a NUL-terminated C string.
        let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) };
        is_nfs_name(&name.to_string_lossy())
    }
    #[cfg(target_os = "linux")]
    {
        const NFS_SUPER_MAGIC: i64 = 0x6969;
        st.f_type as i64 == NFS_SUPER_MAGIC
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn fs_type_is_nfs(_: &std::path::Path) -> bool {
    false
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_nfs_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("nfs") || name.to_ascii_lowercase().starts_with("nfs")
}

fn lease_dir() -> PathBuf {
    velocity_common::env::var("VELOCITY_LEASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_lease_dir())
}

#[cfg(not(test))]
fn default_lease_dir() -> PathBuf {
    let home = velocity_common::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".velocity").join("leases")
}

/// This crate's unit tests (the runner and farm tests lease devices like
/// `run-<name>-<pid>`): a scratch directory of this test process, never
/// the user's `~/.velocity/leases`, where every `cargo test` left a set of
/// lock files (m-15). Fixed at compile time, so no test has to set the
/// process-wide VELOCITY_LEASE_DIR, which raced the others.
#[cfg(test)]
fn default_lease_dir() -> PathBuf {
    std::env::temp_dir().join(format!("velocity-test-leases-{}", std::process::id()))
}

/// The id a device's lease is keyed by: `127.0.0.1:5037/emulator-5554` is
/// `emulator-5554` reached through the local adb server, so one process
/// listing it remotely and another locally must contend for one lock.
fn lease_key(device_id: &str) -> &str {
    let Some((server, serial)) = device_id.split_once('/') else {
        return device_id;
    };
    let local_port = velocity_common::env::var("VELOCITY_ADB_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5037);
    match server.rsplit_once(':') {
        Some((host, port))
            if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
                && port.parse::<u16>().ok() == Some(local_port) =>
        {
            serial
        }
        _ => device_id,
    }
}

fn lease_path(device_id: &str) -> PathBuf {
    let key = lease_key(device_id);
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // An id with nothing replaced keeps its plain `<id>.lock`, the name
    // older versions use, so an old daemon and a new CLI still exclude each
    // other on the common ids (`emulator-5554`, m-4).
    if name == key {
        return lease_dir().join(format!("{name}.lock"));
    }
    // Ids that differ only in replaced characters (`a:b` and `a_b`) shared
    // one lock file, so one blocked the other (VEL-177).
    let hash = key.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    lease_dir().join(format!("{name}-{hash:08x}.lock"))
}

/// Why a lease wasn't taken.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseError {
    /// Another process holds it (its pid and command, if it wrote them).
    Held(String),
    /// The lease file can't be made or locked (an unwritable lease dir):
    /// waiting won't help, so callers fail at once instead of treating it
    /// as a busy device for minutes (VEL-047).
    Io(String),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held(holder) => write!(f, "{holder}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Take the lease on `device_id` without waiting. `Held` names the
/// current holder (pid and command) when another process has it.
pub fn try_lock(device_id: &str) -> Result<DeviceLock, LeaseError> {
    // The stand-in of a run with only `http` steps: nothing to share.
    if leasing_off() || velocity_common::is_no_device(device_id) {
        return Ok(DeviceLock::unleased());
    }
    let path = lease_path(device_id);
    if path.parent().is_some_and(on_nfs) {
        warn_once(format!(
            "the device lease dir {} is on NFS, where file locks can hang: running without cross-process device leases ({HINT})",
            lease_dir().display()
        ));
        return Ok(DeviceLock::unleased());
    }
    try_lock_at(&path)
}

fn try_lock_at(path: &std::path::Path) -> Result<DeviceLock, LeaseError> {
    if let Some(dir) = path.parent() {
        velocity_common::fsutil::create_private_dir(dir)
            .map_err(|e| LeaseError::Io(format!("lease dir {}: {e}; {HINT}", dir.display())))?;
    }
    let mut open = OpenOptions::new();
    open.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let mut file = open
        .open(path)
        .map_err(|e| LeaseError::Io(format!("lease file {}: {e}; {HINT}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {
            // The subcommand only: the full command line can carry
            // `--env PASSWORD=…`, and the file outlives the run (VEL-049).
            let holder = format!("pid={} cmd={}\n", std::process::id(), command_name());
            let _ = file.set_len(0);
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.write_all(holder.as_bytes());
            Ok(DeviceLock { _file: Some(file) })
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            Err(LeaseError::Held(if holder.trim().is_empty() {
                "another Velocity process".to_string()
            } else {
                holder.trim().to_string()
            }))
        }
        Err(std::fs::TryLockError::Error(e)) => match classify(&e) {
            LockFailure::Unsupported => {
                warn_once(format!(
                    "file locks aren't supported where device leases live ({}: {e}): running without cross-process device leases",
                    path.display()
                ));
                Ok(DeviceLock::unleased())
            }
            LockFailure::Fatal => Err(LeaseError::Io(format!(
                "lease file {}: {e}; {HINT}",
                path.display()
            ))),
        },
    }
}

/// `velocity run`: the program and its subcommand, nothing after.
fn command_name() -> String {
    let mut args = std::env::args();
    let program = args
        .next()
        .map(|p| {
            std::path::Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(p)
        })
        .unwrap_or_default();
    match args.next().filter(|a| !a.starts_with('-')) {
        Some(sub) => format!("{program} {sub}"),
        None => program,
    }
}

/// One lease file found on disk ([`scan`]), for `velocity doctor` and the
/// MCP `diagnose` tool.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseFile {
    pub path: PathBuf,
    /// The file name without `.lock` (the device id, maybe with a hash).
    pub name: String,
    /// Another open file holds the lock: a live Velocity process.
    pub held: bool,
    /// The holder line its last owner wrote (`pid=… cmd=…`), if any.
    pub holder: Option<String>,
    /// The pid in that line.
    pub pid: Option<u32>,
    /// The command in that line.
    pub cmd: Option<String>,
    /// The pid it names is not running.
    pub owner_dead: bool,
}

impl LeaseFile {
    /// Left behind by a process that has exited and holds nothing: safe to
    /// clear with [`clear_stale`]. (A lease releases itself when its holder
    /// exits, so such a file blocks nobody; clearing only tidies the line.)
    pub fn is_stale(&self) -> bool {
        !self.held && self.owner_dead
    }
}

/// The directory device leases live in, for `velocity doctor`.
pub fn dir() -> PathBuf {
    lease_dir()
}

/// The `pid=` of a holder line.
pub fn holder_pid(holder: &str) -> Option<u32> {
    holder
        .split_whitespace()
        .find_map(|w| w.strip_prefix("pid="))
        .and_then(|p| p.parse().ok())
}

/// Every `*.lock` in `dir`, with whether it is held now. A free file is
/// locked for a moment to tell, and nothing is written to it.
pub fn scan(dir: &std::path::Path) -> std::io::Result<Vec<LeaseFile>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".lock"))
            .map(String::from)
        else {
            continue;
        };
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        let holder = Some(text.trim().to_string()).filter(|t| !t.is_empty());
        let held = matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock));
        let pid = holder.as_deref().and_then(holder_pid);
        out.push(LeaseFile {
            cmd: holder
                .as_deref()
                .and_then(|h| h.split_once("cmd="))
                .map(|(_, c)| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            owner_dead: pid.is_some_and(|p| !pid_alive(p)),
            pid,
            path,
            name,
            held,
            holder,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Clear the holder line of a lease nobody holds whose owner has exited. The
/// file itself stays: removing a lock file can let two processes lock two
/// different files for one device. Returns whether it was cleared; a lease
/// taken meanwhile, or one whose owner is alive or unknown, is left alone.
pub fn clear_stale(path: &std::path::Path) -> std::io::Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    if holder_pid(&text).is_none_or(pid_alive) {
        return Ok(false);
    }
    file.set_len(0)?;
    Ok(true)
}

/// Whether a process with this pid exists (EPERM: it does, owned by
/// someone else).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let r = unsafe { libc::kill(pid, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_tells_held_from_stale_and_clears_only_stale() {
        let dir = std::env::temp_dir().join(format!("vel-lease-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let held = try_lock_at(&dir.join("emulator-1.lock")).unwrap();
        std::fs::write(
            dir.join("emulator-2.lock"),
            "pid=999999999 cmd=velocity run\n",
        )
        .unwrap();
        let found = scan(&dir).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].held && found[0].pid == Some(std::process::id()));
        assert!(!found[1].held && found[1].pid == Some(999_999_999));
        assert!(
            !clear_stale(&found[0].path).unwrap(),
            "a held lease is left alone"
        );
        assert!(found[1].is_stale() && !found[0].is_stale());
        assert_eq!(found[1].cmd.as_deref(), Some("velocity run"));
        std::fs::write(dir.join("emulator-3.lock"), "").unwrap();
        assert!(
            !clear_stale(&dir.join("emulator-3.lock")).unwrap(),
            "no pid: not provably stale"
        );
        assert!(clear_stale(&found[1].path).unwrap());
        assert_eq!(std::fs::read_to_string(&found[1].path).unwrap(), "");
        assert!(found[1].path.exists(), "cleared, never removed");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_that_differ_only_in_replaced_characters_have_their_own_lease() {
        assert_ne!(lease_path("host:5555"), lease_path("host_5555"));
        assert_ne!(lease_path("a b"), lease_path("a/b"));
        assert_eq!(lease_path("emulator-5554"), lease_path("emulator-5554"));
    }

    // m-4: every lock file was renamed, so older versions no longer
    // excluded newer ones on the same device.
    #[test]
    fn plain_ids_keep_the_lock_name_older_versions_use() {
        assert_eq!(
            lease_path("emulator-5554"),
            lease_dir().join("emulator-5554.lock")
        );
        assert_eq!(lease_path("host_5555"), lease_dir().join("host_5555.lock"));
        assert_ne!(lease_path("host:5555"), lease_dir().join("host_5555.lock"));
    }

    #[test]
    fn a_device_can_be_leased_once_at_a_time() {
        // An explicit path, not VELOCITY_LEASE_DIR: tests run in parallel and
        // the farm tests resolve the lease dir from the same process env.
        let dir = std::env::temp_dir().join(format!("vel-leases-{}", std::process::id()));
        let path = dir.join("emulator-9999.lock");
        let first = try_lock_at(&path).expect("first lease");
        // A second open file description on the same file can't take the lock,
        // exactly as another process couldn't.
        let held = try_lock_at(&path).err().expect("second lease must fail");
        let LeaseError::Held(held) = held else {
            panic!("{held:?}");
        };
        assert!(
            held.contains(&format!("pid={}", std::process::id())),
            "{held}"
        );
        // The holder line names the command, never its arguments.
        assert_eq!(held.lines().count(), 1);
        assert!(!held.contains("--"), "{held}");
        drop(first);
        assert!(try_lock_at(&path).is_ok(), "released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // m-15: the runner and farm tests lease devices; never under $HOME.
    #[test]
    fn tests_never_lease_in_the_users_home() {
        if std::env::var_os("VELOCITY_LEASE_DIR").is_some() {
            return;
        }
        let dir = lease_dir();
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
        assert!(!dir.ends_with(".velocity/leases"), "{}", dir.display());
    }

    #[test]
    fn a_device_reached_through_the_local_adb_server_has_one_lease() {
        assert_eq!(lease_key("127.0.0.1:5037/emulator-5554"), "emulator-5554");
        assert_eq!(lease_key("localhost:5037/emulator-5554"), "emulator-5554");
        assert_eq!(
            lease_key("farm-1:5037/emulator-5554"),
            "farm-1:5037/emulator-5554"
        );
        assert_eq!(lease_key("emulator-5554"), "emulator-5554");
        assert_eq!(
            lease_path("127.0.0.1:5037/emulator-5554"),
            lease_path("emulator-5554")
        );
    }

    // VEL-047: an unwritable lease dir is an error, not a busy device.
    #[cfg(unix)]
    #[test]
    fn an_unusable_lease_dir_is_an_io_error_not_a_busy_device() {
        let dir = std::env::temp_dir().join(format!("vel-leases-io-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A file where the lease dir should be.
        std::fs::write(&dir, "not a dir").unwrap();
        let err = try_lock_at(&dir.join("leases").join("emulator-7777.lock"))
            .err()
            .expect("must fail");
        assert!(matches!(err, LeaseError::Io(_)), "{err:?}");
        // D-25: with a hint at what to do.
        assert!(err.to_string().contains("VELOCITY_LEASE_DIR"), "{err}");
        let _ = std::fs::remove_file(&dir);
    }

    // D-25: cargo's rules. A filesystem that can't lock runs unleased; any
    // other lock error is fatal.
    #[cfg(unix)]
    #[test]
    fn lock_errors_follow_cargos_rules() {
        use std::io::Error;
        for code in [libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS] {
            assert_eq!(
                classify(&Error::from_raw_os_error(code)),
                LockFailure::Unsupported,
                "{code}"
            );
        }
        assert_eq!(
            classify(&Error::from(std::io::ErrorKind::Unsupported)),
            LockFailure::Unsupported
        );
        for code in [libc::EACCES, libc::EROFS, libc::ENOSPC, libc::ENOLCK] {
            assert_eq!(
                classify(&Error::from_raw_os_error(code)),
                LockFailure::Fatal,
                "{code}"
            );
        }
        assert!(is_nfs_name("nfs"));
        assert!(!is_nfs_name("apfs"));
        // A local temp dir is not NFS.
        assert!(!on_nfs(&std::env::temp_dir().join("no/such/dir")));
    }
}
