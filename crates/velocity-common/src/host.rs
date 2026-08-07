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
    PathBuf::from(crate::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
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
