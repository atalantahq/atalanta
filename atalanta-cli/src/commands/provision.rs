//! `atalanta devices up` / `down`, and `atalanta run --provision`: boot the
//! devices a run needs, and stop only what Atalanta started.
//!
//! - Android: N headless, read-only instances of one AVD (one image, N
//!   copy-on-write overlays; see `atalanta_android::emulator`).
//! - iOS: `atalanta-<type>-<n>` clones, reused most recently used first;
//!   new ones cloned from a golden simulator that has booted once (see
//!   `atalanta_ios::provision`).
//! - Before each boot the host must have room: at least ~35% and 3 GB of
//!   memory free, counting ~2 GB for every device still booting, and 3 GB
//!   of disk. A swapping host makes every device on it flaky; refusing with
//!   a reason is better.
//! - Everything booted is recorded in `~/.atalanta/provisioned.json`, so
//!   `devices down` stops those devices and nothing else.
//! - Each entry carries an owner: `--owner`, else `ATALANTA_OWNER`, else the
//!   CI job (GitHub Actions, GitLab, Buildkite, Jenkins, CircleCI), else
//!   none (a person at a terminal). `devices down` stops only the caller's
//!   own devices (`--all` stops every owner's), and `up` counts only the
//!   caller's own emulators as already up, so two CI jobs sharing a host
//!   and a user never stop each other's devices.
//! - Picking an emulator port, launching it and recording it happen under
//!   one host-wide lock, so two `up`s at once never pick the same port;
//!   creating the iOS golden simulator has its own lock for the same reason.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atalanta_android::emulator;
use atalanta_common::{AtalantaError, Platform, PlatformDriver};
use atalanta_ios::provision as sims;
use atalanta_ios::simctl::Simctl;
use colored::Colorize;
use serde::{Deserialize, Serialize};

const GB: u64 = 1 << 30;
/// Memory a booting device will take: a headless API-34 emulator settled
/// at 1.66 GB RSS here, a simulator at ~1.3 GB.
const ANDROID_BYTES: u64 = 2 * GB;
const IOS_BYTES: u64 = 3 * GB / 2;
/// A read-only emulator that hasn't booted in 5 minutes won't.
const ANDROID_BOOT_TIMEOUT: Duration = Duration::from_secs(300);

// ── Host guard ──────────────────────────────────────────────────────────

// The measurement and the thresholds live in `atalanta_common::host`, so
// `atalanta doctor` reports against the same bar.
use atalanta_common::host::limits;
#[cfg(test)]
use atalanta_common::host::Room;
use atalanta_common::host::{check_room, measure};

fn home() -> PathBuf {
    atalanta_common::host::home_dir()
}

/// [`check_room`] against a fresh measurement.
fn guard(what: &str, pending: u64) -> Result<(), AtalantaError> {
    let Some(room) = measure() else {
        println!(
            "  {} can't measure free memory/disk; booting {what} anyway",
            "!".yellow()
        );
        return Ok(());
    };
    check_room(room, pending, limits()).map_err(|reason| AtalantaError::DeviceBootFailed {
        id: what.to_string(),
        reason: format!("not booting: {reason}"),
    })
}

// ── Registry ────────────────────────────────────────────────────────────

/// A device in `provisioned.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// `android` or `ios`.
    pub platform: String,
    /// Serial (`emulator-5556`) or UDID.
    pub id: String,
    pub name: String,
    /// The AVD an emulator runs, checked again before stopping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Atalanta booted it: `devices down` stops it.
    #[serde(default)]
    pub started: bool,
    /// Atalanta created it (a clone or golden): `devices down --delete`
    /// deletes it.
    #[serde(default)]
    pub created: bool,
    /// Unix seconds.
    #[serde(default)]
    pub at: u64,
    /// Who booted it ([`owner`]); `None` for a person at a terminal (and
    /// for entries written before owners existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// The owner new entries get and `down` stops: `explicit` (`--owner`),
/// else `ATALANTA_OWNER`, else the CI job this runs in, else `None`.
pub fn owner(explicit: Option<&str>) -> Option<String> {
    owner_from(explicit, |k| atalanta_common::env::var(k).ok())
}

fn owner_from(explicit: Option<&str>, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let nonempty = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    if let Some(o) = explicit
        .map(str::to_string)
        .filter(|o| !o.trim().is_empty())
    {
        return Some(o);
    }
    if let Some(o) = nonempty(var("ATALANTA_OWNER")) {
        return Some(o);
    }
    let join = |keys: &[&str]| -> Option<String> {
        let parts: Vec<String> = keys.iter().filter_map(|k| nonempty(var(k))).collect();
        (!parts.is_empty()).then(|| parts.join(":"))
    };
    // Matrix jobs share GITHUB_JOB; two runner agents on one host differ
    // in RUNNER_NAME.
    if nonempty(var("GITHUB_RUN_ID")).is_some() {
        return join(&[
            "GITHUB_RUN_ID",
            "GITHUB_RUN_ATTEMPT",
            "GITHUB_JOB",
            "RUNNER_NAME",
        ])
        .map(|o| format!("github:{o}"));
    }
    for (ci, keys) in [
        ("gitlab", &["CI_JOB_ID"][..]),
        ("buildkite", &["BUILDKITE_JOB_ID"][..]),
        ("circleci", &["CIRCLE_WORKFLOW_JOB_ID"][..]),
        ("jenkins", &["BUILD_TAG", "EXECUTOR_NUMBER"][..]),
    ] {
        if nonempty(var(keys[0])).is_some() {
            return join(keys).map(|o| format!("{ci}:{o}"));
        }
    }
    None
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub devices: Vec<Entry>,
}

/// `~/.atalanta/provisioned.json` (`ATALANTA_PROVISIONED_FILE` overrides).
fn registry_path() -> PathBuf {
    atalanta_common::env::var("ATALANTA_PROVISIONED_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".atalanta").join("provisioned.json"))
}

/// An exclusive `flock` on `<registry>.lock`, held while a process reads,
/// changes and writes the registry, so two `devices up` / `run --provision`
/// at once don't drop each other's entries (VEL-S05). Released on drop.
struct RegistryLock {
    _file: Option<std::fs::File>,
}

impl RegistryLock {
    fn take(path: &std::path::Path) -> Self {
        Self::take_named(path, "json.lock")
    }

    /// Another host-wide lock beside the registry (`<registry>.<ext>`),
    /// for work that must not interleave across processes but also calls
    /// [`Registry::update`] (which takes the registry's own lock).
    fn take_named(path: &std::path::Path, ext: &str) -> Self {
        use std::os::unix::io::AsRawFd;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension(ext))
            .ok()
            .filter(|f| {
                // SAFETY: a valid open descriptor, owned by `f`.
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) == 0 }
            });
        RegistryLock { _file: file }
    }
}

impl Registry {
    pub fn load() -> Self {
        Self::load_at(&registry_path())
    }

    /// One unreadable entry no longer empties the whole registry (and with
    /// it, `devices down`'s list of what to stop): bad entries are skipped
    /// with a warning, and a file that isn't a registry at all is kept as
    /// `provisioned.json.corrupt` (VEL-037).
    fn load_at(path: &std::path::Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => Some(t),
            // No registry yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            // Bytes that aren't text: kept aside below like any file that
            // isn't a registry, not overwritten by the next update.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => None,
            // Unreadable for another reason (permissions): changed no further.
            Err(_) => return Self::default(),
        };
        let devices = text
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(
                |mut v| match v.get_mut("devices").map(serde_json::Value::take) {
                    Some(serde_json::Value::Array(a)) => Some(a),
                    _ => None,
                },
            );
        let Some(devices) = devices else {
            let aside = path.with_extension("json.corrupt");
            let _ = std::fs::rename(path, &aside);
            println!(
                "  {} {} is not a device registry; moved it to {}",
                "!".yellow(),
                path.display(),
                aside.display()
            );
            return Self::default();
        };
        let mut reg = Self::default();
        for d in devices {
            match serde_json::from_value::<Entry>(d.clone()) {
                Ok(e) => reg.devices.push(e),
                Err(err) => println!(
                    "  {} skipping an unreadable entry in {}: {err} ({d})",
                    "!".yellow(),
                    path.display()
                ),
            }
        }
        reg
    }

    /// Re-read the registry under its lock, apply `change` and write it
    /// back: other processes' entries since this one's last read survive.
    pub fn update(change: impl FnOnce(&mut Registry)) -> Self {
        Self::update_at(&registry_path(), change)
    }

    fn update_at(path: &std::path::Path, change: impl FnOnce(&mut Registry)) -> Self {
        let _lock = RegistryLock::take(path);
        let mut reg = Self::load_at(path);
        change(&mut reg);
        reg.save_at(path);
        reg
    }

    /// Written to a temp file and renamed, so a crash never leaves half a
    /// file (and with it, devices nobody knows to stop).
    fn save_at(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}-{n}.tmp", std::process::id()));
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Add or replace the entry for `e.id`, keeping `created` if it was set.
    pub fn record(&mut self, mut e: Entry) {
        if let Some(old) = self.devices.iter().find(|d| d.id == e.id) {
            e.created |= old.created;
        }
        self.devices.retain(|d| d.id != e.id);
        self.devices.push(e);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Android ─────────────────────────────────────────────────────────────

/// Have `n` Atalanta-started emulators of `avd` running and booted; boot
/// the missing ones (ports from `port_base` up). Returns every ready serial.
pub async fn android_up(
    n: usize,
    avd: Option<&str>,
    port_base: u16,
    owner: Option<&str>,
) -> Result<Vec<String>, AtalantaError> {
    let avd = match avd {
        Some(a) => a.to_string(),
        None => emulator::list_avds()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                AtalantaError::Config(
                    "no Android AVD found (`emulator -list-avds` is empty); create one or pass --avd"
                        .into(),
                )
            })?,
    };
    let running = emulator::running_serials().await;
    let reg = Registry::load();

    // Ours (this owner's) and still up count toward N; forget the ones
    // that are gone. Another owner's emulator is never "ours": its job may
    // stop it at any moment.
    let mut ready = Vec::new();
    let mut gone = Vec::new();
    for e in reg.devices.iter().filter(|e| e.platform == "android") {
        if running.contains(&e.id) && emulator::avd_name(&e.id).await.as_deref() == e.avd.as_deref()
        {
            if e.avd.as_deref() == Some(avd.as_str())
                && e.owner.as_deref() == owner
                && ready.len() < n
            {
                ready.push(e.id.clone());
            }
        } else if !e.pid.is_some_and(pid_alive) {
            // Not in `adb devices` and its process is gone. (One whose
            // emulator process is alive is still booting, maybe for
            // another `up` right now: keep it.)
            gone.push(e.id.clone());
        }
    }
    Registry::update(|r| {
        r.devices
            .retain(|e| e.platform != "android" || !gone.contains(&e.id))
    });
    for id in &ready {
        println!("  {} {id} ({avd}) already up", "✓".green());
    }

    let need = n.saturating_sub(ready.len());
    let mut taken = running.clone();
    let mut booting = tokio::task::JoinSet::new();
    let mut refused = None;
    for i in 0..need {
        let pending = ANDROID_BYTES * i as u64;
        // Held from picking the port until the emulator is recorded: a
        // second `up` waits here, then sees this one's entry (its emulator
        // may not be in `adb devices` yet) and skips the port.
        let _ports = RegistryLock::take_named(&registry_path(), "json.ports.lock");
        for e in Registry::load().devices {
            if e.platform == "android" && !taken.contains(&e.id) {
                taken.push(e.id);
            }
        }
        let Some(port) = emulator::free_port(port_base, &taken) else {
            refused = Some(AtalantaError::DeviceBootFailed {
                id: avd.clone(),
                reason: "no free emulator port pair between 5554 and 5682".into(),
            });
            break;
        };
        let serial = format!("emulator-{port}");
        if let Err(e) = guard(&serial, pending) {
            refused = Some(e);
            break;
        }
        taken.push(serial.clone());
        let log = home()
            .join(".atalanta")
            .join("logs")
            .join(format!("{serial}.log"));
        let mut child = emulator::launch_read_only(&avd, port, &log)?;
        println!(
            "  {} starting {serial} ({avd}, read-only, headless; log {})",
            "↑".cyan(),
            log.display()
        );
        let entry = Entry {
            platform: "android".into(),
            id: serial.clone(),
            name: avd.clone(),
            avd: Some(avd.clone()),
            pid: Some(child.id()),
            started: true,
            created: false,
            at: now(),
            owner: owner.map(String::from),
        };
        Registry::update(|r| r.record(entry));
        drop(_ports);
        booting.spawn(async move {
            let r = emulator::wait_booted(&serial, Some(&mut child), ANDROID_BOOT_TIMEOUT).await;
            (serial, r)
        });
    }

    let mut failed = None;
    while let Some(joined) = booting.join_next().await {
        let Ok((serial, r)) = joined else { continue };
        match r {
            Ok(took) => {
                println!(
                    "  {} {serial} booted in {:.1}s",
                    "✓".green(),
                    took.as_secs_f64()
                );
                ready.push(serial);
            }
            Err(e) => {
                println!("  {} {serial}: {e}", "✗".red());
                let _ = emulator::kill(&serial).await;
                failed = Some(e);
            }
        }
    }
    ready.sort();
    match refused.or(failed) {
        Some(e) if ready.len() < n => {
            println!(
                "  {} {} of {n} Android device(s) up",
                "!".yellow(),
                ready.len()
            );
            Err(e)
        }
        _ => Ok(ready),
    }
}

// ── iOS ─────────────────────────────────────────────────────────────────

/// The device type to provision: `wanted`, else the type of the most
/// recently booted iPhone simulator.
async fn device_type(
    simctl: &Simctl,
    wanted: Option<&str>,
) -> Result<(String, String), AtalantaError> {
    let types = simctl.list_json("devicetypes").await?;
    if let Some(w) = wanted {
        return sims::resolve_device_type(&types, w);
    }
    let all = sims::parse_sims(&simctl.list_json("devices").await?);
    let recent = all
        .iter()
        .filter(|s| s.name.starts_with("iPhone") && s.last_booted_at.is_some())
        .max_by(|a, b| a.last_booted_at.cmp(&b.last_booted_at))
        .ok_or_else(|| {
            AtalantaError::Config(
                "no iPhone simulator has been used yet; pass --device-type (e.g. \"iPhone 17 Pro\")"
                    .into(),
            )
        })?;
    sims::resolve_device_type(&types, &recent.device_type)
}

/// The simulator new clones are copied from: `golden` (UDID or name), else
/// The simulator `--golden` names (a UDID, or a name that only one
/// simulator has), or the default golden of `type_name`. Two simulators
/// with the wanted name is an error listing their UDIDs: it used to take
/// whichever came first (VEL-S06).
fn find_golden<'a>(
    all: &'a [sims::Sim],
    type_name: &str,
    wanted: Option<&str>,
) -> Result<Option<&'a sims::Sim>, AtalantaError> {
    let Some(w) = wanted else {
        let name = sims::golden_name(type_name);
        return Ok(all.iter().find(|s| s.name == name));
    };
    if let Some(s) = all.iter().find(|s| s.udid == w) {
        return Ok(Some(s));
    }
    let named: Vec<&sims::Sim> = all.iter().filter(|s| s.name == w).collect();
    if named.len() > 1 {
        let ids: Vec<String> = named
            .iter()
            .map(|s| format!("{} ({})", s.udid, s.runtime))
            .collect();
        return Err(AtalantaError::Config(format!(
            "{} simulators are named '{w}'; pass one UDID to --golden: {}",
            named.len(),
            ids.join(", ")
        )));
    }
    Ok(named.first().copied())
}

/// `atalanta-<type>-golden`, created and booted once if missing. It must be
/// shut down to be cloned.
async fn golden(
    simctl: &Simctl,
    all: &[sims::Sim],
    type_name: &str,
    type_id: &str,
    wanted: Option<&str>,
) -> Result<String, AtalantaError> {
    if let Some(g) = find_golden(all, type_name, wanted)? {
        if !g.shutdown() {
            return Err(AtalantaError::Config(format!(
                "golden simulator {} ({}) is {}; it must be shut down to be cloned",
                g.name, g.udid, g.state
            )));
        }
        return Ok(g.udid.clone());
    }
    if let Some(w) = wanted {
        return Err(AtalantaError::Config(format!(
            "no simulator '{w}' to clone from"
        )));
    }
    // First time on this host: make the golden and get first-boot
    // migration over with once. Under a host-wide lock, looking again
    // once we hold it: two `up`s at once used to create two goldens of
    // one name, and `find_golden` then picked either.
    let _golden_lock = RegistryLock::take_named(&registry_path(), "json.golden.lock");
    let fresh = sims::parse_sims(&simctl.list_json("devices").await?);
    if let Some(g) = find_golden(&fresh, type_name, None)? {
        if g.shutdown() {
            return Ok(g.udid.clone());
        }
        return Err(AtalantaError::Config(format!(
            "golden simulator {} ({}) is {}; it must be shut down to be cloned \
             (another `atalanta devices up` may be creating it: run again when it finishes)",
            g.name, g.udid, g.state
        )));
    }
    let name = sims::golden_name(type_name);
    guard(&name, 0)?;
    let start = Instant::now();
    let udid = simctl.create(&name, type_id, None).await?;
    Registry::update(|r| {
        r.record(Entry {
            platform: "ios".into(),
            id: udid.clone(),
            name: name.clone(),
            avd: None,
            pid: None,
            started: false,
            created: true,
            at: now(),
            owner: None,
        })
    });
    println!(
        "  {} creating golden simulator {name} and booting it once (first boot is slow)",
        "↑".cyan()
    );
    simctl.bootstatus(&udid).await?;
    simctl.shutdown(&udid).await?;
    println!(
        "  {} golden {name} ready in {:.1}s",
        "✓".green(),
        start.elapsed().as_secs_f64()
    );
    Ok(udid)
}

/// Have `n` booted `atalanta-<type>-<n>` clones; boot or clone the missing
/// ones. Returns every ready UDID.
pub async fn ios_up(
    n: usize,
    wanted_type: Option<&str>,
    wanted_golden: Option<&str>,
    owner: Option<&str>,
) -> Result<Vec<String>, AtalantaError> {
    let simctl = Arc::new(Simctl::new());
    let (type_name, type_id) = device_type(&simctl, wanted_type).await?;
    // Held from reading the simulators until every clone this `up` boots
    // is recorded: a second `up` at once waits, then sees them as another
    // owner's (or this owner's) instead of booting or cloning the same ones.
    let clones_lock = RegistryLock::take_named(&registry_path(), "json.clones.lock");
    let all = sims::parse_sims(&simctl.list_json("devices").await?);
    let foreign = foreign_ios(&Registry::load(), owner);
    let plan = sims::plan(&all, &type_name, n, &foreign);
    let mut ready = plan.already.clone();
    for id in &ready {
        println!("  {} {id} ({type_name}) already booted", "✓".green());
    }

    // Shut-down clones first (most recently used), then new clones.
    let mut to_boot: Vec<(String, String, bool)> = plan
        .reuse
        .iter()
        .map(|udid| {
            let name = all
                .iter()
                .find(|s| &s.udid == udid)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            (udid.clone(), name, false)
        })
        .collect();
    if !plan.create.is_empty() {
        let source = golden(&simctl, &all, &type_name, &type_id, wanted_golden).await?;
        for idx in &plan.create {
            let name = sims::clone_name(&type_name, *idx);
            // A clone is cheap on disk (APFS) until it boots; don't make
            // one that couldn't boot.
            guard(&name, IOS_BYTES * to_boot.len() as u64)?;
            let start = Instant::now();
            let udid = simctl.clone_device(&source, &name).await?;
            println!(
                "  {} cloned {name} ({udid}) in {:.1}s",
                "+".cyan(),
                start.elapsed().as_secs_f64()
            );
            Registry::update(|r| {
                r.record(Entry {
                    platform: "ios".into(),
                    id: udid.clone(),
                    name: name.clone(),
                    avd: None,
                    pid: None,
                    started: false,
                    created: true,
                    at: now(),
                    owner: owner.map(String::from),
                })
            });
            to_boot.push((udid, name, true));
        }
    }

    let mut booting = tokio::task::JoinSet::new();
    let mut refused = None;
    for (i, (udid, name, _)) in to_boot.into_iter().enumerate() {
        if let Err(e) = guard(&name, IOS_BYTES * i as u64) {
            refused = Some(e);
            break;
        }
        println!("  {} booting {name} ({udid})", "↑".cyan());
        Registry::update(|r| {
            r.record(Entry {
                platform: "ios".into(),
                id: udid.clone(),
                name: name.clone(),
                avd: None,
                pid: None,
                started: true,
                created: name.starts_with("atalanta-"),
                at: now(),
                owner: owner.map(String::from),
            })
        });
        let simctl = simctl.clone();
        booting.spawn(async move {
            let start = Instant::now();
            let r = simctl.bootstatus(&udid).await.map(|_| start.elapsed());
            (udid, name, r)
        });
    }
    drop(clones_lock);
    let mut failed = None;
    while let Some(joined) = booting.join_next().await {
        let Ok((udid, name, r)) = joined else {
            continue;
        };
        match r {
            Ok(took) => {
                println!(
                    "  {} {name} booted in {:.1}s",
                    "✓".green(),
                    took.as_secs_f64()
                );
                ready.push(udid);
            }
            Err(e) => {
                println!("  {} {name}: {e}", "✗".red());
                let _ = simctl.shutdown(&udid).await;
                failed = Some(e);
            }
        }
    }
    match refused.or(failed) {
        Some(e) if ready.len() < n => {
            println!(
                "  {} {} of {n} iOS simulator(s) up",
                "!".yellow(),
                ready.len()
            );
            Err(e)
        }
        _ => Ok(ready),
    }
}

// ── Install on provisioned devices ──────────────────────────────────────

/// The bundle id / package a build installs as: read from the build (the
/// APK manifest, the bundle's `Info.plist`), else `fallback`.
pub fn app_identity(path: &str, fallback: Option<&str>) -> Option<String> {
    let p = std::path::Path::new(path);
    let read = if path.ends_with(".apk") {
        atalanta_headless::android::ApkLoader::from_path(p)
            .ok()
            .and_then(|a| a.package_name)
    } else if p.is_dir() {
        atalanta_ios::install::bundle_id(p)
    } else {
        None
    };
    read.or(fallback.map(String::from))
}

/// Install `app` on every device in `devices`, all at once, by hash.
pub async fn install_all(
    driver: Arc<dyn PlatformDriver>,
    devices: &[String],
    app: &str,
    app_id: &str,
) -> Result<(), AtalantaError> {
    let mut set = tokio::task::JoinSet::new();
    for d in devices {
        let (driver, d, app, id) = (
            driver.clone(),
            d.clone(),
            app.to_string(),
            app_id.to_string(),
        );
        set.spawn(async move {
            atalanta_runner::install::ensure_installed(&*driver, &d, &app, &id)
                .await
                .map_err(|e| (d, e))
        });
    }
    let mut first_err = None;
    while let Some(joined) = set.join_next().await {
        if let Ok(Err((d, e))) = joined {
            println!("  {} install on {d} failed: {e}", "✗".red());
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

// ── devices up / down ───────────────────────────────────────────────────

pub struct UpArgs {
    pub android: usize,
    pub ios: usize,
    pub avd: Option<String>,
    pub device_type: Option<String>,
    pub golden: Option<String>,
    pub app: Option<String>,
    pub app_id: Option<String>,
    pub port: u16,
    /// `--owner`; see [`owner`].
    pub owner: Option<String>,
}

pub async fn up(args: UpArgs) -> anyhow::Result<i32> {
    if args.android == 0 && args.ios == 0 {
        usage_bail!("nothing to provision: pass --android N and/or --ios N");
    }
    let start = Instant::now();
    let who = owner(args.owner.as_deref());
    if let Some(o) = &who {
        println!(
            "{} Owner: {o} (`atalanta devices down` from the same job stops these)",
            "=>".cyan().bold()
        );
    }
    if args.android > 0 {
        println!(
            "{} Android: {} read-only emulator(s)",
            "=>".cyan().bold(),
            args.android
        );
        let serials =
            android_up(args.android, args.avd.as_deref(), args.port, who.as_deref()).await?;
        install_up(Platform::Android, &serials, &args).await?;
    }
    if args.ios > 0 {
        println!(
            "{} iOS: {} simulator clone(s)",
            "=>".cyan().bold(),
            args.ios
        );
        let udids = ios_up(
            args.ios,
            args.device_type.as_deref(),
            args.golden.as_deref(),
            who.as_deref(),
        )
        .await?;
        install_up(Platform::Ios, &udids, &args).await?;
    }
    println!(
        "{} Devices up in {:.1}s (`atalanta devices down` stops them)",
        "=>".green().bold(),
        start.elapsed().as_secs_f64()
    );
    Ok(0)
}

async fn install_up(platform: Platform, devices: &[String], args: &UpArgs) -> anyhow::Result<()> {
    let Some(app) = &args.app else {
        return Ok(());
    };
    let wrong = match platform {
        Platform::Android => !app.ends_with(".apk"),
        _ => app.ends_with(".apk"),
    };
    if wrong || devices.is_empty() {
        return Ok(());
    }
    let id = app_identity(app, args.app_id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("can't tell the bundle id of {app}; pass --app-id"))?;
    let driver: Arc<dyn PlatformDriver> = Arc::from(super::create_driver(platform));
    install_all(driver, devices, app, &id).await?;
    Ok(())
}

/// What `devices down` acts on.
pub struct DownArgs {
    /// Only this platform.
    pub platform: Option<Platform>,
    /// Also delete the simulators Atalanta created.
    pub delete: bool,
    /// `--owner`; see [`owner`].
    pub owner: Option<String>,
    /// `--all`: every owner's devices, not only the caller's.
    pub all: bool,
}

/// Whether `down` for `owner` (or `--all`) acts on `e`. Shared goldens
/// (owner `None`, created, never started) belong to whoever runs
/// `down --delete`.
fn owned_by(e: &Entry, owner: Option<&str>, all: bool) -> bool {
    all || e.owner.as_deref() == owner || (e.owner.is_none() && !e.started && e.created)
}

/// Whether `golden` (a shared `atalanta-<type>-golden`) still backs a clone
/// that `down` for `owner` leaves alone. Such a golden is kept: deleting it
/// made the other owner's next `up` create and boot it again (2026-11-11,
/// Finding D). `--all` deletes it with the clones.
fn golden_in_use(golden: &Entry, entries: &[Entry], owner: Option<&str>, all: bool) -> bool {
    if all || golden.platform != "ios" {
        return false;
    }
    let Some(prefix) = golden.name.strip_suffix("-golden") else {
        return false;
    };
    entries.iter().any(|c| {
        c.platform == "ios"
            && c.id != golden.id
            && !owned_by(c, owner, all)
            && c.name
                .strip_prefix(prefix)
                .and_then(|n| n.strip_prefix('-'))
                .is_some_and(|n| n.parse::<u32>().is_ok())
    })
}

/// Stop what Atalanta started for this owner (`platform` limits it to
/// one), and with `delete` also delete the simulators it created.
pub async fn down(args: DownArgs) -> anyhow::Result<i32> {
    let DownArgs {
        platform,
        delete,
        owner: explicit,
        all,
    } = args;
    let who = owner(explicit.as_deref());
    let taken = Registry::load().devices;
    let simctl = Simctl::new();
    let running = emulator::running_serials().await;
    let mut kept = Vec::new();
    let mut stopped = 0;
    let mut others: Vec<String> = Vec::new();
    for e in taken.iter().cloned() {
        let mine = match platform {
            Some(Platform::Android) => e.platform == "android",
            Some(Platform::Ios) => e.platform == "ios",
            _ => true,
        };
        if !mine {
            kept.push(e);
            continue;
        }
        if !owned_by(&e, who.as_deref(), all) {
            if e.started {
                others.push(format!(
                    "{} ({})",
                    e.id,
                    e.owner.as_deref().unwrap_or("no owner")
                ));
            }
            kept.push(e);
            continue;
        }
        if e.platform == "android" {
            if !emulator::is_emulator_serial(&e.id) {
                continue; // never stop anything but an emulator
            }
            if !running.contains(&e.id) {
                println!("  {} {} already stopped", "·".dimmed(), e.id);
                continue;
            }
            // The port may have been reused by an emulator someone else
            // started since; only stop the AVD Atalanta started there.
            let avd = emulator::avd_name(&e.id).await;
            if avd.is_some() && avd != e.avd {
                println!(
                    "  {} {} now runs {}, not Atalanta's {}; leaving it",
                    "!".yellow(),
                    e.id,
                    avd.unwrap_or_default(),
                    e.avd.clone().unwrap_or_default()
                );
                continue;
            }
            let start = Instant::now();
            match emulator::kill(&e.id).await {
                Ok(()) => {
                    wait_gone(&e.id).await;
                    println!(
                        "  {} stopped {} in {:.1}s",
                        "■".green(),
                        e.id,
                        start.elapsed().as_secs_f64()
                    );
                    stopped += 1;
                }
                Err(err) => {
                    println!("  {} {}: {err}", "✗".red(), e.id);
                    kept.push(e);
                }
            }
            continue;
        }
        // iOS
        let mut e = e;
        if e.started {
            match simctl.shutdown(&e.id).await {
                Ok(()) => {
                    println!("  {} shut down {} ({})", "■".green(), e.name, e.id);
                    stopped += 1;
                }
                Err(err) if err.to_string().contains("current state: Shutdown") => {}
                Err(err) => println!("  {} {}: {err}", "!".yellow(), e.name),
            }
            e.started = false;
        }
        if e.created && delete && golden_in_use(&e, &taken, who.as_deref(), all) {
            println!(
                "  {} kept {} ({}): another owner's clone still uses it",
                "·".dimmed(),
                e.name,
                e.id
            );
            kept.push(e);
        } else if e.created && delete {
            match simctl.delete(&e.id).await {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(atalanta_common::install::device_markers_dir(
                        &e.id,
                    ));
                    println!("  {} deleted {} ({})", "-".green(), e.name, e.id)
                }
                Err(err) => {
                    println!("  {} {}: {err}", "!".yellow(), e.name);
                    kept.push(e);
                }
            }
        } else if e.created {
            kept.push(e); // reused by the next `up`; `--delete` removes it
        }
    }
    // Merge into the registry as it is now: entries another process added
    // meanwhile stay (VEL-S05).
    let seen: Vec<String> = taken.iter().map(|e| e.id.clone()).collect();
    let reg = Registry::update(|r| {
        r.devices.retain(|e| !seen.contains(&e.id));
        r.devices.extend(kept);
    });
    if !others.is_empty() {
        println!(
            "  {} left {} device(s) another owner started: {}. \
             `atalanta devices down --all` stops them too",
            "·".dimmed(),
            others.len(),
            others.join(", ")
        );
    }
    println!(
        "{} Stopped {stopped} device(s){}",
        "=>".green().bold(),
        if reg.devices.iter().any(|e| e.created) && !delete {
            " (clones kept for reuse; --delete removes them)"
        } else {
            ""
        }
    );
    Ok(0)
}

/// Emulators Atalanta started that are still running `avd`. Stale entries
/// (gone, or another AVD) don't count: they inflated the number
/// `run --provision` asked android_up for, which booted extra emulators
/// (VEL-S01).
fn ours_running(
    reg: &Registry,
    running: &[String],
    avd: Option<&str>,
    owner: Option<&str>,
) -> usize {
    reg.devices
        .iter()
        .filter(|e| e.platform == "android" && e.started && running.contains(&e.id))
        .filter(|e| avd.is_none() || e.avd.as_deref() == avd)
        .filter(|e| e.owner.as_deref() == owner)
        .count()
}

/// Simulators another owner booted through Atalanta: `ios_up` neither
/// counts them as up nor boots them (see `sims::plan`).
fn foreign_ios(reg: &Registry, owner: Option<&str>) -> Vec<String> {
    reg.devices
        .iter()
        .filter(|e| e.platform == "ios" && e.started && e.owner.as_deref() != owner)
        .map(|e| e.id.clone())
        .collect()
}

/// Whether process `pid` still exists (signal 0 checks without sending).
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks for the process.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

async fn wait_gone(serial: &str) {
    wait_gone_with(serial, emulator::running_serials, Duration::from_secs(20)).await
}

/// Poll `running` until `serial` is gone, for at most `limit` in all. It
/// used to try 40 times, each `adb devices` with its own timeout, so a
/// hung adb could hold `devices down` for 40 × that timeout (VEL-S52).
async fn wait_gone_with<F, Fut>(serial: &str, running: F, limit: Duration)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<String>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    let _ = tokio::time::timeout_at(deadline, async {
        while running().await.iter().any(|s| s == serial) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
}

// ── atalanta run: app: / --app ─────────────────────────────────────────

/// Settle which build the run installs: `--app` over `app:`, the entry for
/// `platform`, as a checked path (relative `app:` paths are relative to the
/// suite file, `--app` to the working directory). Leaves `suite.config.app`
/// as that single path, or `None` when there's nothing to install.
pub fn resolve_app(
    suite: &mut atalanta_common::TestSuite,
    cli_app: Option<&str>,
    config_path: &str,
    platform: Platform,
    on_devices: bool,
) -> Result<(), AtalantaError> {
    use atalanta_common::AppSource;
    if let Some(p) = cli_app {
        suite.config.app = Some(AppSource::Path(p.to_string()));
    }
    let Some(source) = suite.config.app.take() else {
        return Ok(());
    };
    if !on_devices || platform == Platform::Web {
        return Ok(());
    }
    let Some(raw) = source.for_platform(Some(platform)) else {
        println!(
            "{} app: has no {platform} build; not installing",
            "=>".cyan().bold()
        );
        return Ok(());
    };
    let mut path = PathBuf::from(raw);
    if cli_app.is_none() && path.is_relative() {
        let beside_suite = std::path::Path::new(config_path)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(&path);
        if beside_suite.exists() || !path.exists() {
            path = beside_suite;
        }
    }
    let shown = path.display().to_string();
    // By extension, not the string: `Foo.app/` from shell completion is
    // still an .app bundle (VEL-038).
    let ext = |want: &str| {
        path.extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(want))
    };
    let ok = match platform {
        Platform::Android => path.is_file() && ext("apk"),
        _ => (path.is_dir() && ext("app")) || (path.is_file() && ext("ipa")),
    };
    if !path.exists() {
        return Err(AtalantaError::Config(format!(
            "app build {shown} not found"
        )));
    }
    if !ok {
        return Err(AtalantaError::Config(format!(
            "app build {shown} isn't a {platform} build ({})",
            if platform == Platform::Android {
                "an .apk"
            } else {
                "an .app bundle, or an .ipa for a device"
            }
        )));
    }
    let shown = std::fs::canonicalize(&path)
        .map(|p| p.display().to_string())
        .unwrap_or(shown);
    if let Some(id) = app_identity(&shown, None) {
        if id != suite.app_id {
            println!(
                "  {} {shown} installs as {id}, but the suite's appId is {}",
                "!".yellow(),
                suite.app_id
            );
        }
    }
    println!("{} App: {}", "=>".cyan().bold(), shown);
    // `reset: { strategy: reinstall }` reinstalls this same build.
    if suite.config.reset.app_path.is_none() {
        suite.config.reset.app_path = Some(shown.clone());
    }
    suite.config.app = Some(AppSource::Path(shown));
    Ok(())
}

/// Before the runners start: `--provision` boots the missing devices while
/// the app build is hashed (the runners then install by that hash).
pub async fn before_run(
    provision: bool,
    platform: Platform,
    want: usize,
    driver: &dyn PlatformDriver,
    app: Option<String>,
) -> Result<(), AtalantaError> {
    let hash = async {
        let Some(app) = &app else {
            return Ok(None);
        };
        let start = Instant::now();
        atalanta_runner::install::hash(app)
            .await
            .map(|h| Some((h, start.elapsed())))
    };
    let boot = async {
        if provision {
            for_run(platform, want, driver).await
        } else {
            Ok(())
        }
    };
    let (hash, boot) = tokio::join!(hash, boot);
    if let Some((h, took)) = hash? {
        println!(
            "{} App sha256 {} (hashed in {})",
            "=>".cyan().bold(),
            atalanta_common::install::short(&h),
            atalanta_runner::install::secs(took)
        );
    }
    boot
}

// ── run --provision ─────────────────────────────────────────────────────

/// Boot enough devices of `platform` that `want` are booted (counting any
/// already booted, as the parallel runner will use them).
pub async fn for_run(
    platform: Platform,
    want: usize,
    driver: &dyn PlatformDriver,
) -> Result<(), AtalantaError> {
    let booted = driver
        .list_devices()
        .await?
        .into_iter()
        .filter(|d| d.state == atalanta_common::DeviceState::Booted)
        .count();
    if booted >= want {
        println!(
            "{} Provision: {booted} {platform} device(s) already booted",
            "=>".cyan().bold()
        );
        return Ok(());
    }
    let need = want - booted;
    let start = Instant::now();
    println!(
        "{} Provision: booting {need} more {platform} device(s)",
        "=>".cyan().bold()
    );
    match platform {
        Platform::Android => {
            // The AVD android_up would pick, so only its emulators count.
            let avd = match atalanta_common::env::var("ATALANTA_AVD") {
                Ok(a) => Some(a),
                Err(_) => emulator::list_avds()
                    .await
                    .ok()
                    .and_then(|a| a.into_iter().next()),
            };
            let who = owner(None);
            let ours = ours_running(
                &Registry::load(),
                &emulator::running_serials().await,
                avd.as_deref(),
                who.as_deref(),
            );
            let port = atalanta_common::env::var("ATALANTA_EMULATOR_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(emulator::FIRST_PORT);
            android_up(ours + need, avd.as_deref(), port, who.as_deref()).await?;
        }
        Platform::Ios => {
            let simctl = Simctl::new();
            let (type_name, _) = device_type(
                &simctl,
                atalanta_common::env::var("ATALANTA_DEVICE_TYPE")
                    .ok()
                    .as_deref(),
            )
            .await?;
            let all = sims::parse_sims(&simctl.list_json("devices").await?);
            let who = owner(None);
            let foreign = foreign_ios(&Registry::load(), who.as_deref());
            let up = sims::clones(&all, &type_name)
                .iter()
                .filter(|s| s.booted() && !foreign.contains(&s.udid))
                .count();
            ios_up(up + need, Some(&type_name), None, who.as_deref()).await?;
        }
        Platform::Web => {}
    }
    println!(
        "{} Provisioned in {:.1}s",
        "=>".cyan().bold(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_comes_from_the_flag_then_the_env_then_the_ci_job() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(owner_from(None, env(&[])), None);
        assert_eq!(
            owner_from(Some("job-7"), env(&[("ATALANTA_OWNER", "x")])).as_deref(),
            Some("job-7")
        );
        assert_eq!(
            owner_from(None, env(&[("ATALANTA_OWNER", "x"), ("CI_JOB_ID", "9")])).as_deref(),
            Some("x")
        );
        assert_eq!(
            owner_from(None, env(&[("CI_JOB_ID", "9")])).as_deref(),
            Some("gitlab:9")
        );
        assert_eq!(
            owner_from(
                None,
                env(&[
                    ("GITHUB_RUN_ID", "1"),
                    ("GITHUB_RUN_ATTEMPT", "2"),
                    ("GITHUB_JOB", "e2e"),
                    ("RUNNER_NAME", "mac-3"),
                ])
            )
            .as_deref(),
            Some("github:1:2:e2e:mac-3")
        );
    }

    /// One CI job's `devices down` leaves another job's emulator alone.
    #[test]
    fn down_acts_only_on_the_callers_own_devices_unless_all() {
        let mut a = android_entry("emulator-5554");
        a.owner = Some("job-a".into());
        let mut b = android_entry("emulator-5556");
        b.owner = Some("job-b".into());
        let none = android_entry("emulator-5558");
        assert!(owned_by(&a, Some("job-a"), false));
        assert!(!owned_by(&b, Some("job-a"), false));
        assert!(!owned_by(&none, Some("job-a"), false));
        assert!(owned_by(&none, None, false));
        assert!(!owned_by(&a, None, false));
        assert!(owned_by(&b, Some("job-a"), true));
        // A shared golden (never started, no owner) is anyone's to delete.
        let golden = Entry {
            platform: "ios".into(),
            id: "G".into(),
            name: "atalanta-iPhone-golden".into(),
            avd: None,
            pid: None,
            started: false,
            created: true,
            at: 0,
            owner: None,
        };
        assert!(owned_by(&golden, Some("job-a"), false));
    }

    /// Finding D (2026-11-11): `down --owner b --delete` deleted the golden
    /// that owner a's booted clone came from.
    #[test]
    fn a_golden_another_owners_clone_uses_is_kept() {
        let ios = |id: &str, name: &str, owner: Option<&str>, started: bool| Entry {
            platform: "ios".into(),
            id: id.into(),
            name: name.into(),
            avd: None,
            pid: None,
            started,
            created: true,
            at: 0,
            owner: owner.map(Into::into),
        };
        let golden = ios("G", "atalanta-iPhone-17-golden", None, false);
        let a1 = ios("A1", "atalanta-iPhone-17-1", Some("a"), true);
        let b2 = ios("B2", "atalanta-iPhone-17-2", Some("b"), true);
        let other_type = ios("P1", "atalanta-iPhone-17-Pro-1", Some("a"), true);
        let all = vec![golden.clone(), a1.clone(), b2.clone()];
        // b's down leaves a's clone, so the golden stays.
        assert!(golden_in_use(&golden, &all, Some("b"), false));
        // With only b's own clone left, b may delete it.
        let only_b = vec![golden.clone(), b2];
        assert!(!golden_in_use(&golden, &only_b, Some("b"), false));
        // `--all` deletes everything.
        assert!(!golden_in_use(&golden, &all, Some("b"), true));
        // A clone of another device type does not hold this golden.
        let pro = vec![golden.clone(), other_type];
        assert!(!golden_in_use(&golden, &pro, Some("b"), false));
        // a's own down of its clone does not keep the golden for itself.
        assert!(!golden_in_use(
            &golden,
            &[golden.clone(), a1],
            Some("a"),
            false
        ));
    }

    #[test]
    fn an_old_registry_without_owners_still_loads() {
        let e: Entry = serde_json::from_str(
            r#"{"platform":"android","id":"emulator-5554","name":"P","started":true}"#,
        )
        .unwrap();
        assert_eq!(e.owner, None);
        assert!(!serde_json::to_string(&e).unwrap().contains("owner"));
    }

    /// VEL-S06: `--golden <name>` with two simulators of that name is an
    /// error naming both, not the first one found.
    #[test]
    fn an_ambiguous_golden_name_is_an_error() {
        let sim = |udid: &str, name: &str, runtime: &str| sims::Sim {
            udid: udid.into(),
            name: name.into(),
            state: "Shutdown".into(),
            device_type: "iPhone-15".into(),
            runtime: runtime.into(),
            last_booted_at: None,
        };
        let all = vec![
            sim("A-1", "Base", "iOS-17-5"),
            sim("B-2", "Base", "iOS-18-0"),
            sim("C-3", "Other", "iOS-18-0"),
        ];
        let err = find_golden(&all, "iPhone 15", Some("Base"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("A-1") && err.contains("B-2"), "{err}");
        assert_eq!(
            find_golden(&all, "iPhone 15", Some("B-2"))
                .unwrap()
                .unwrap()
                .udid,
            "B-2"
        );
        assert_eq!(
            find_golden(&all, "iPhone 15", Some("Other"))
                .unwrap()
                .unwrap()
                .udid,
            "C-3"
        );
        assert!(find_golden(&all, "iPhone 15", Some("None"))
            .unwrap()
            .is_none());
    }

    /// VEL-038: `--app Foo.app/` (shell completion's trailing slash) is an
    /// iOS build.
    #[test]
    fn an_app_bundle_with_a_trailing_slash_is_accepted() {
        let dir = std::env::temp_dir().join(format!("vprov-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Foo.app")).unwrap();
        let mut suite = atalanta_core::parse_suite_from_str(
            "appId: com.x\ntests:\n  - name: t\n    steps:\n      - launchApp\n",
        )
        .unwrap();
        let app = format!("{}/", dir.join("Foo.app").display());
        resolve_app(&mut suite, Some(&app), "atalanta.yaml", Platform::Ios, true)
            .unwrap_or_else(|e| panic!("{app}: {e}"));
        assert!(matches!(
            suite.config.app,
            Some(atalanta_common::AppSource::Path(ref p)) if p.ends_with("Foo.app")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VEL-S01: only running emulators of the target AVD count as ours.
    #[test]
    fn stale_and_other_avd_entries_dont_count_toward_provisioning() {
        let entry = |id: &str, avd: &str, started: bool| Entry {
            platform: "android".into(),
            id: id.into(),
            name: id.into(),
            avd: Some(avd.into()),
            pid: None,
            started,
            created: false,
            at: 0,
            owner: None,
        };
        let reg = Registry {
            devices: vec![
                entry("emulator-5554", "Pixel", true),
                entry("emulator-5556", "Pixel", true), // gone
                entry("emulator-5558", "Tablet", true),
                entry("emulator-5560", "Pixel", false), // not ours to count
            ],
        };
        let running: Vec<String> = ["emulator-5554", "emulator-5558", "emulator-5560"]
            .map(String::from)
            .to_vec();
        assert_eq!(ours_running(&reg, &running, Some("Pixel"), None), 1);
        assert_eq!(ours_running(&reg, &running, None, None), 2);
        assert_eq!(ours_running(&reg, &running, None, Some("job-2")), 0);
    }

    const LIMITS: (f64, u64, u64) = (35.0, 3 * GB, 3 * GB);

    #[test]
    fn boots_only_with_enough_memory_and_disk_counting_devices_still_booting() {
        let room = Room {
            total_mem: 24 * GB,
            free_mem: 10 * GB,
            free_disk: 9 * GB,
        };
        assert!(check_room(room, 0, LIMITS).is_ok(), "42% free");
        let err = check_room(room, 2 * GB, LIMITS).unwrap_err();
        assert!(err.contains("33% of memory free"), "{err}");
        assert!(err.contains("counting 2.0 GB"), "{err}");

        // Plenty of percent on a small machine is still too little memory.
        let small = Room {
            total_mem: 4 * GB,
            free_mem: 2 * GB,
            free_disk: 50 * GB,
        };
        assert!(check_room(small, 0, LIMITS).unwrap_err().contains("3.0 GB"));

        let full_disk = Room {
            free_disk: 2 * GB,
            ..room
        };
        let err = check_room(full_disk, 0, LIMITS).unwrap_err();
        assert!(err.contains("2.0 GB of disk free"), "{err}");
    }

    #[test]
    fn this_host_can_be_measured() {
        let room = measure().expect("memory and disk");
        assert!(room.total_mem > 0 && room.free_mem <= room.total_mem);
        assert!(room.free_disk > 0);
    }

    fn android_entry(id: &str) -> Entry {
        Entry {
            platform: "android".into(),
            id: id.into(),
            name: "Pixel".into(),
            avd: Some("Pixel".into()),
            pid: None,
            started: true,
            created: false,
            at: 0,
            owner: None,
        }
    }

    fn registry_scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vreg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("provisioned.json")
    }

    /// VEL-S52: a slow `adb devices` that always lists the serial costs
    /// the overall limit, not 40 × each call's time.
    #[tokio::test]
    async fn waiting_for_an_emulator_to_go_has_one_deadline() {
        let start = Instant::now();
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            vec!["emulator-5554".to_string()]
        };
        wait_gone_with("emulator-5554", slow, Duration::from_millis(700)).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    /// A registry that can't be read as text (bytes damaged) is kept aside
    /// like any file that isn't a registry. It read as empty and the next
    /// `update` overwrote it with one entry: every device Atalanta had
    /// started was forgotten.
    #[test]
    fn a_registry_that_is_not_utf8_is_kept_aside_not_overwritten() {
        let path = registry_scratch("not-utf8");
        std::fs::write(&path, b"{\"devices\": [\xff\xfe]}").unwrap();
        let reg = Registry::load_at(&path);
        assert!(reg.devices.is_empty());
        let aside = path.with_extension("json.corrupt");
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"{\"devices\": [\xff\xfe]}",
            "the unreadable file is kept for the user, not left to be overwritten"
        );
        let _ = std::fs::remove_file(&aside);
    }

    /// VEL-037: one bad entry costs that entry, not the whole registry; a
    /// file that isn't a registry is kept aside, not silently dropped.
    #[test]
    fn a_bad_registry_entry_is_skipped_and_a_corrupt_file_kept() {
        let path = registry_scratch("corrupt");
        std::fs::write(
            &path,
            r#"{"devices":[{"platform":"android","id":"emulator-5554","name":"Pixel","started":true},{"platform":"android","id":7}]}"#,
        )
        .unwrap();
        let reg = Registry::load_at(&path);
        assert_eq!(reg.devices.len(), 1);
        assert_eq!(reg.devices[0].id, "emulator-5554");

        std::fs::write(&path, "{ not json").unwrap();
        assert!(Registry::load_at(&path).devices.is_empty());
        assert!(path.with_extension("json.corrupt").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// VEL-S05: concurrent read-modify-write cycles keep every entry.
    #[test]
    fn concurrent_registry_updates_keep_every_entry() {
        let path = registry_scratch("race");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        Registry::update_at(&path, |r| {
                            r.record(android_entry(&format!("emulator-{t}-{i}")))
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(Registry::load_at(&path).devices.len(), 100);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_registry_keeps_created_when_a_clone_is_booted_again() {
        let mut reg = Registry::default();
        let e = |started, created| Entry {
            platform: "ios".into(),
            id: "U".into(),
            name: "atalanta-iPhone-17-1".into(),
            avd: None,
            pid: None,
            started,
            created,
            at: 0,
            owner: None,
        };
        reg.record(e(false, true));
        reg.record(e(true, false));
        assert_eq!(reg.devices, vec![e(true, true)]);
        let json = serde_json::to_string(&reg).unwrap();
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.devices, reg.devices);
    }
}
