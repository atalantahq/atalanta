//! Free memory and disk on this host, and the bar a device boot must clear
//! (`velocity devices up` refuses below it; `velocity doctor` reports
//! against it): at least ~35% and 3 GB of memory free, and 3 GB of disk.
//! A swapping host makes every device on it flaky.

use std::path::{Path, PathBuf};

pub const GB: u64 = 1 << 30;

/// Free memory and disk on this host.
#[derive(Debug, Clone, Copy)]
pub struct Room {
    pub total_mem: u64,
    pub free_mem: u64,
    pub free_disk: u64,
}

fn env_num(key: &str, default: f64) -> f64 {
    crate::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The thresholds: `VELOCITY_MIN_FREE_MEMORY_PCT` (35),
/// `VELOCITY_MIN_FREE_MEMORY_GB` (3), `VELOCITY_MIN_FREE_DISK_GB` (3).
pub fn limits() -> (f64, u64, u64) {
    (
        env_num("VELOCITY_MIN_FREE_MEMORY_PCT", 35.0),
        (env_num("VELOCITY_MIN_FREE_MEMORY_GB", 3.0) * GB as f64) as u64,
        (env_num("VELOCITY_MIN_FREE_DISK_GB", 3.0) * GB as f64) as u64,
    )
}

/// Measure the host. macOS: the kernel's free-memory level (what
/// `memory_pressure` reports: free plus reclaimable); Linux: `MemAvailable`.
pub fn measure() -> Option<Room> {
    let (total_mem, free_mem) = memory()?;
    Some(Room {
        total_mem,
        free_mem,
        free_disk: free_disk(&home())?,
    })
}

#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, u64)> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize", "kern.memorystatus_level"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let total: u64 = lines.next()?.trim().parse().ok()?;
    let pct: u64 = lines.next()?.trim().parse().ok()?;
    Some((total, total / 100 * pct.min(100)))
}

#[cfg(not(target_os = "macos"))]
fn memory() -> Option<(u64, u64)> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| -> Option<u64> {
        info.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|k| k * 1024)
    };
    Some((kb("MemTotal:")?, kb("MemAvailable:")?))
}

/// Bytes free to an unprivileged user on the filesystem holding `path`.
#[cfg(unix)]
pub fn free_disk(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is a C struct of integers: all zeroes is a valid
    // value, and the call below fills it in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_disk(_: &Path) -> Option<u64> {
    None
}

fn home() -> PathBuf {
    home_dir()
}

/// The user's home directory: `HOME`, or, when it is unset (some CI
/// containers and service managers), a directory of this user's own under
/// the temp dir. Every `~/.velocity` path used to fall back to a shared
/// `/tmp/.velocity`, which another local user could create first and own:
/// its lease files, daemon socket and state then followed their symlinks
/// (audit 2026-11 FS-1). The fallback is `velocity-home-<uid>`, made `0700`;
/// one that exists but isn't ours alone is not used, and a fresh, unique
/// directory is made instead.
pub fn home_dir() -> PathBuf {
    match crate::env::var("HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h),
        _ => private_tmp_home(),
    }
}

#[cfg(unix)]
fn private_tmp_home() -> PathBuf {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY: geteuid has no preconditions and can't fail.
    let uid = unsafe { libc::geteuid() };
    let tmp = std::env::temp_dir();
    let ours = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
    };
    let fixed = tmp.join(format!("velocity-home-{uid}"));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&fixed);
    if ours(&fixed) {
        return fixed;
    }
    tracing::warn!(
        "HOME is unset and {} is not this user's own; using a fresh directory",
        fixed.display()
    );
    for n in 0u32.. {
        let p = tmp.join(format!("velocity-home-{uid}-{}-{n}", std::process::id()));
        if std::fs::DirBuilder::new().mode(0o700).create(&p).is_ok() && ours(&p) {
            return p;
        }
        if n > 1000 {
            break;
        }
    }
    fixed
}

#[cfg(not(unix))]
fn private_tmp_home() -> PathBuf {
    std::env::temp_dir()
}

pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GB as f64)
}

/// Whether one more device may boot, given `pending` bytes that devices
/// still booting will take. `Err` says why not.
pub fn check_room(room: Room, pending: u64, limits: (f64, u64, u64)) -> Result<(), String> {
    let (min_pct, min_mem, min_disk) = limits;
    let free = room.free_mem.saturating_sub(pending);
    let pct = free as f64 * 100.0 / room.total_mem.max(1) as f64;
    let counting = if pending > 0 {
        format!(", counting {} for devices still booting", gb(pending))
    } else {
        String::new()
    };
    if pct < min_pct || free < min_mem {
        return Err(format!(
            "only {pct:.0}% of memory free ({} of {}{counting}); booting needs {min_pct:.0}% and {} \
             (VELOCITY_MIN_FREE_MEMORY_PCT / _GB lower the bar)",
            gb(free),
            gb(room.total_mem),
            gb(min_mem)
        ));
    }
    if room.free_disk < min_disk {
        return Err(format!(
            "only {} of disk free; booting needs {} (VELOCITY_MIN_FREE_DISK_GB)",
            gb(room.free_disk),
            gb(min_disk)
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod home_tests {
    #[test]
    fn without_home_the_fallback_is_this_users_own_directory() {
        use std::os::unix::fs::MetadataExt;
        let p = super::private_tmp_home();
        let m = std::fs::symlink_metadata(&p).unwrap();
        assert!(m.is_dir());
        // SAFETY: geteuid has no preconditions.
        assert_eq!(m.uid(), unsafe { libc::geteuid() });
        assert_eq!(m.mode() & 0o077, 0, "{:o}", m.mode());
        assert!(!p.starts_with("/tmp/.velocity"));
    }
}
