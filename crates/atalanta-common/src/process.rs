//! Running external commands (adb, simctl, xcodebuild, git) safely.
//!
//! Every call gets a deadline and is killed if it passes: a wedged `adb`
//! or `simctl` otherwise hangs the whole run with no output (a no-output
//! watchdog is what device farms add first). `kill_on_drop` also means a
//! cancelled future — Ctrl-C, a step timeout — never leaves the child behind.

use std::collections::HashSet;
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::Command;

/// Default deadline for a device command; `ATALANTA_COMMAND_TIMEOUT_MS`
/// overrides it.
pub fn default_timeout() -> Duration {
    crate::env::var("ATALANTA_COMMAND_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(60))
}

/// Deadline for slow, legitimate operations (installs, pushes, builds).
pub const LONG_TIMEOUT: Duration = Duration::from_secs(600);

/// `cmd.output()` with a deadline. On timeout the child is killed and an
/// `io::ErrorKind::TimedOut` error is returned.
///
/// On Unix the child runs in its own process group and the whole group is
/// killed, so wrappers like `xcrun` can't leave a wedged `simctl` behind.
pub async fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    use std::process::Stdio;
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    // `output()` semantics: capture stdout/stderr, no stdin.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn()?;
    // Until the child is reaped, dropping this future (Ctrl-C, a step
    // watchdog, a `select!`) kills its whole group: `kill_on_drop` alone
    // killed only the direct child and left its children running (VEL-074).
    let mut guard = GroupGuard(child.id());
    if let Some(pgid) = guard.0 {
        live_groups().insert(pgid);
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => {
            if let Some(pgid) = guard.0.take() {
                live_groups().remove(&pgid);
            }
            result
        }
        Err(_) => {
            // The guard takes the group down with the child.
            drop(guard);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {}s", timeout.as_secs_f32()),
            ))
        }
    }
}

/// Process groups of the commands running now (see [`kill_all_groups`]).
static LIVE_GROUPS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn live_groups() -> LiveGroups {
    LiveGroups(LIVE_GROUPS.lock().unwrap_or_else(|e| e.into_inner()))
}

struct LiveGroups(std::sync::MutexGuard<'static, Option<HashSet<u32>>>);

impl LiveGroups {
    fn insert(&mut self, pgid: u32) {
        self.0.get_or_insert_with(HashSet::new).insert(pgid);
    }
    fn remove(&mut self, pgid: &u32) {
        if let Some(set) = self.0.as_mut() {
            set.remove(pgid);
        }
    }
    fn take(&mut self) -> Vec<u32> {
        self.0
            .take()
            .map(|s| s.into_iter().collect())
            .unwrap_or_default()
    }
}

/// SIGKILL the process group of every command still running (D-20).
/// `std::process::exit` runs no destructors, so without this a second
/// Ctrl-C left in-flight `adb`, `simctl` and `git` calls (and their
/// children, which the terminal's SIGINT never reaches: they run in their
/// own group) behind.
pub fn kill_all_groups() {
    let groups = live_groups().take();
    #[cfg(unix)]
    for pgid in groups {
        kill_group(pgid);
    }
    #[cfg(not(unix))]
    drop(groups);
    kill_all_pids();
}

/// Processes Atalanta didn't spawn in a group of its own (Chrome, which
/// chromiumoxide starts in Atalanta's own group), each with a directory to
/// remove once it is dead. Keyed by a registration id.
static LIVE_PIDS: Mutex<Vec<(u64, u32, Option<std::path::PathBuf>)>> = Mutex::new(Vec::new());
static NEXT_PID_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn live_pids() -> std::sync::MutexGuard<'static, Vec<(u64, u32, Option<std::path::PathBuf>)>> {
    LIVE_PIDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a running child process `pid` (and `dir`, a directory only it
/// uses) for [`kill_all_groups`]: the second signal SIGKILLs it and removes
/// `dir`. For children that can't be put in a group of their own, such as
/// the web driver's Chrome (review-d6 M2). Dropping the guard deregisters
/// without killing: the owner stops the process itself. The owner must
/// keep the child unreaped while the guard lives, so the pid can't be
/// reused under it.
pub fn register_pid(pid: u32, dir: Option<std::path::PathBuf>) -> PidGuard {
    let id = NEXT_PID_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    live_pids().push((id, pid, dir));
    PidGuard(id)
}

/// Deregisters a [`register_pid`] entry when dropped.
#[derive(Debug)]
pub struct PidGuard(u64);

impl Drop for PidGuard {
    fn drop(&mut self) {
        live_pids().retain(|(id, _, _)| *id != self.0);
    }
}

fn kill_all_pids() {
    let entries = std::mem::take(&mut *live_pids());
    #[cfg(unix)]
    for (_, pid, _) in &entries {
        if let Ok(pid) = libc::pid_t::try_from(*pid) {
            if pid > 1 {
                // Its helpers first (Linux: Chrome's zygote and renderers
                // outlive the browser and keep its profile in use), read
                // before the browser dies and they are re-parented.
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                for child in descendants(pid) {
                    // SAFETY: kill(2) on a process descended from our child.
                    unsafe {
                        libc::kill(child, libc::SIGKILL);
                    }
                }
                // SAFETY: kill(2) on a child the owner keeps unreaped.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    // A dying Chrome's helpers can still write into the profile for a
    // moment: retry until the dirs stay gone. 500 ms left profiles behind
    // on a loaded Linux CI runner, where the helpers outlive the browser
    // longer; this returns as soon as they are gone.
    // Gone once is not gone: a helper still dying can recreate the dir
    // right after it was removed, so the dirs must still be absent after
    // a short grace, or the profile was left behind by the second signal.
    let dirs: Vec<_> = entries.into_iter().filter_map(|(_, _, d)| d).collect();
    if dirs.is_empty() {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        for d in &dirs {
            let _ = std::fs::remove_dir_all(d);
        }
        std::thread::sleep(Duration::from_millis(50));
        if dirs.iter().all(|d| !d.exists()) || std::time::Instant::now() >= deadline {
            return;
        }
    }
}

/// Every process below `root`, from each process's parent in
/// `/proc/<pid>/stat` (the field after the parenthesised command name, which
/// may itself hold spaces or parentheses).
#[cfg(target_os = "linux")]
fn descendants(root: libc::pid_t) -> Vec<libc::pid_t> {
    let mut parent_of = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let ppid = stat
                .rfind(')')
                .and_then(|i| stat.get(i + 1..))
                .and_then(|rest| rest.split_whitespace().nth(1))
                .and_then(|p| p.parse::<libc::pid_t>().ok());
            if let Some(ppid) = ppid {
                parent_of.push((pid, ppid));
            }
        }
    }
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(child, parent) in &parent_of {
            if parent == p && !found.contains(&child) {
                found.push(child);
                frontier.push(child);
            }
        }
    }
    found
}

/// Every process below `root` (macOS: libproc's child list, walked down).
#[cfg(target_os = "macos")]
fn descendants(root: libc::pid_t) -> Vec<libc::pid_t> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        let mut buf = [0 as libc::pid_t; 1024];
        let bytes = std::mem::size_of_val(&buf) as libc::c_int;
        // SAFETY: the buffer is valid for `bytes` bytes; libproc writes at
        // most that many and returns how many pids it wrote (or -1).
        let n = unsafe { libc::proc_listchildpids(p, buf.as_mut_ptr().cast(), bytes) };
        for &child in buf.iter().take(usize::try_from(n).unwrap_or(0)) {
            if child > 1 && !found.contains(&child) {
                found.push(child);
                frontier.push(child);
            }
        }
    }
    found
}

/// A long-lived child (an `adb shell` stress loop, the tree server's
/// shell) in its own process group, registered for [`kill_all_groups`]:
/// a second Ctrl-C or a SIGINT-then-SIGTERM cancel exits through
/// `std::process::exit`, which skips `kill_on_drop`, and these children
/// outlived Atalanta (review d4 m2). Dropping it kills the group, as
/// `kill_on_drop` did. Derefs to the [`tokio::process::Child`].
pub struct GroupChild {
    child: tokio::process::Child,
    guard: GroupGuard,
}

impl Drop for GroupChild {
    /// A child that already exited (and was reaped) no longer owns its
    /// pgid: after a pid wraparound that number can lead an unrelated
    /// group, so it is deregistered and not killed (review-d7 m11).
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            if let Some(pgid) = self.guard.0.take() {
                live_groups().remove(&pgid);
            }
        }
    }
}

impl std::ops::Deref for GroupChild {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for GroupChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

/// Spawn `cmd` as a [`GroupChild`]: its own process group (Unix), killed
/// with it when dropped or on the second signal.
pub fn spawn_group(cmd: &mut Command) -> std::io::Result<GroupChild> {
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let guard = GroupGuard(child.id());
    if let Some(pgid) = guard.0 {
        live_groups().insert(pgid);
    }
    Ok(GroupChild { child, guard })
}

/// Process groups registered now, for tests.
#[cfg(test)]
fn registered(pgid: u32) -> bool {
    LIVE_GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| s.contains(&pgid))
}

/// Kills process group `.0` when dropped, unless cleared.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            live_groups().remove(&pgid);
            #[cfg(unix)]
            kill_group(pgid);
        }
    }
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    if pgid <= 1 {
        return;
    }
    // SAFETY: killpg(2) on a group this process created (`process_group(0)`).
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn descendants_include_grandchildren() {
        // sh -> sleep: the shell's child is found below this process.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5 & wait"])
            .spawn()
            .unwrap();
        let me = libc::pid_t::try_from(std::process::id()).unwrap();
        let shell = libc::pid_t::try_from(child.id()).unwrap();
        let mut seen = Vec::new();
        for _ in 0..50 {
            seen = super::descendants(me);
            if seen.contains(&shell) && seen.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen.contains(&shell), "{seen:?}");
        assert!(seen.len() >= 2, "the shell's sleep too: {seen:?}");
    }

    use super::*;

    /// `kill_all_groups` kills every test's commands: run these one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn a_hung_command_is_killed_at_the_deadline() {
        let _serial = SERIAL.lock().await;
        let start = std::time::Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = output_with_timeout(&mut cmd, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_fast_command_returns_its_output() {
        let _serial = SERIAL.lock().await;
        let mut cmd = Command::new("echo");
        cmd.arg("hi");
        let out = output_with_timeout(&mut cmd, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_grandchildren_too() {
        let _serial = SERIAL.lock().await;
        let pidfile = std::env::temp_dir().join(format!("atalanta-pg-{}", std::process::id()));
        let mut cmd = Command::new("sh");
        // The background sleep outlives the shell unless its group is killed.
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        let err = output_with_timeout(&mut cmd, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let _ = std::fs::remove_file(&pidfile);
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "grandchild {pid} survived the timeout");
    }

    // D-20: a second signal SIGKILLs every running command's group,
    // grandchildren included, before the process exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_all_groups_takes_down_running_commands() {
        let _serial = SERIAL.lock().await;
        let pidfile =
            std::env::temp_dir().join(format!("atalanta-pg-killall-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        let call =
            tokio::spawn(
                async move { output_with_timeout(&mut cmd, Duration::from_secs(60)).await },
            );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pidfile.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "command never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        kill_all_groups();
        let out = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the command survived kill_all_groups")
            .unwrap()
            .unwrap();
        assert!(!out.status.success());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let _ = std::fs::remove_file(&pidfile);
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "grandchild {pid} survived kill_all_groups");
    }

    // Review d4 m2: long-lived spawns (the adb stress loops, the tree
    // server's shell) weren't registered, so the second signal's exit left
    // them running. A `spawn_group` child is in its own group and killed
    // by `kill_all_groups`; dropping it deregisters and kills it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_long_lived_child_is_killed_by_the_second_signal() {
        let _serial = SERIAL.lock().await;
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let mut child = spawn_group(&mut cmd).unwrap();
        let pid = child.id().unwrap();
        assert!(registered(pid));
        kill_all_groups();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("the child survived kill_all_groups")
            .unwrap();
        assert!(!status.success());
        drop(child);
        assert!(!registered(pid));

        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let child = spawn_group(&mut cmd).unwrap();
        let pid = child.id().unwrap();
        drop(child);
        assert!(!registered(pid), "a dropped child stays registered");
    }

    // review-d6 M2: the web driver's Chrome isn't in a group of its own,
    // so the second signal's exit left it (and its profile dir) behind. A
    // registered pid is SIGKILLed and its dir removed; a dropped guard
    // deregisters without killing.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_second_signal_kills_registered_pids_and_removes_their_dirs() {
        let _serial = SERIAL.lock().await;
        let dir = std::env::temp_dir().join(format!("atalanta-pid-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _guard = register_pid(child.id(), Some(dir.clone()));
        let mut kept = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        drop(register_pid(kept.id(), None));
        kill_all_groups();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(std::time::Instant::now() < deadline, "the child survived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(!status.success());
        assert!(!dir.exists(), "its dir survived");
        assert!(
            kept.try_wait().unwrap().is_none(),
            "a deregistered pid was killed"
        );
        kept.kill().unwrap();
        let _ = kept.wait();
    }

    // A dying Chrome's helpers write into the profile for a moment after
    // the dir was removed: the second signal must not return the first
    // time the dir is absent, or the helper's next write brings it back.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dir_a_dying_helper_recreates_is_removed_again() {
        let _serial = SERIAL.lock().await;
        let dir = std::env::temp_dir().join(format!("atalanta-pid-redo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _guard = register_pid(child.id(), Some(dir.clone()));
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            let end = std::time::Instant::now() + Duration::from_millis(300);
            while std::time::Instant::now() < end {
                let _ = std::fs::create_dir_all(writer_dir.join("Default"));
                let _ = std::fs::write(writer_dir.join("Default/Preferences"), b"{}");
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        kill_all_groups();
        writer.join().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!dir.exists(), "the helper's write brought the dir back");
    }

    // review-d7 m11: a GroupChild whose process already exited no longer
    // owns its pgid (it can be reused after a wraparound), so dropping it
    // must not killpg that number.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_exited_group_child_does_not_kill_a_reused_group() {
        let _serial = SERIAL.lock().await;
        // The "reused" group: another live group leader.
        let mut other = Command::new("sleep");
        other.arg("30");
        let mut other = spawn_group(&mut other).unwrap();
        let other_pgid = other.id().unwrap();
        // An exited child that (as after a wraparound) carries that pgid.
        let mut done = Command::new("true");
        done.kill_on_drop(true);
        let mut done = done.spawn().unwrap();
        done.wait().await.unwrap();
        live_groups().insert(other_pgid);
        let stale = GroupChild {
            child: done,
            guard: GroupGuard(Some(other_pgid)),
        };
        drop(stale);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            other.try_wait().unwrap().is_none(),
            "an exited child's drop killed another group"
        );
        // A running one is still killed with its group.
        drop(other);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &other_pgid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "a running group child survived its drop");
    }

    // VEL-074: a cancelled call (the future dropped, as Ctrl-C or a step
    // watchdog do) left the command's own children running.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_command_takes_its_children_with_it() {
        let _serial = SERIAL.lock().await;
        let pidfile =
            std::env::temp_dir().join(format!("atalanta-pg-cancel-{}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        let call = output_with_timeout(&mut cmd, Duration::from_secs(60));
        assert!(tokio::time::timeout(Duration::from_millis(500), call)
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let _ = std::fs::remove_file(&pidfile);
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "child {pid} survived the cancelled call");
    }
}
