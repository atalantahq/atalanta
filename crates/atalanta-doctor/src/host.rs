// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Host: OS, Atalanta, memory and disk, state directories, leases.

use std::path::{Path, PathBuf};

use atalanta_common::host as room;

use crate::{timed, Area, AreaResult, Check, Ctx, Fix};

const A: Area = Area::Host;

pub(crate) async fn checks(ctx: &Ctx) -> AreaResult {
    let out = vec![
        timed(|| os(ctx)).await,
        timed(|| async { atalanta() }).await,
        timed(|| async { resources() }).await,
        timed(|| async { state_dirs(ctx) }).await,
        timed(|| async { leases(ctx) }).await,
    ];
    (out, false)
}

async fn os(ctx: &Ctx) -> Check {
    let arch = std::env::consts::ARCH;
    let (name, version) = match std::env::consts::OS {
        "macos" => {
            let v = ctx
                .env()
                .run(
                    Path::new("/usr/bin/sw_vers"),
                    &["-productVersion"],
                    ctx.opts.timeout,
                )
                .await
                .stdout()
                .map(|s| s.trim().to_string());
            ("macOS", v)
        }
        "linux" => {
            let pretty = std::fs::read_to_string("/etc/os-release")
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                        .map(|v| v.trim_matches('"').to_string())
                });
            ("Linux", pretty)
        }
        other => (other, None),
    };
    let summary = match version {
        Some(v) => format!("{name} {v} ({arch})"),
        None => format!("{name} ({arch})"),
    };
    let c = Check::ok("host.os", A, summary);
    if std::env::consts::OS != "macos" {
        c.with_detail("iOS testing needs macOS with Xcode; Android and web work here")
    } else {
        c
    }
}

fn atalanta() -> Check {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    Check::ok(
        "host.atalanta",
        A,
        format!("atalanta {}", env!("CARGO_PKG_VERSION")),
    )
    .with_detail(exe)
}

fn resources() -> Check {
    let Some(r) = room::measure() else {
        return Check::warn("host.resources", A, "can't measure free memory and disk");
    };
    let pct = r.free_mem as f64 * 100.0 / r.total_mem.max(1) as f64;
    let summary = format!(
        "{} of {} memory free ({pct:.0}%), {} disk free",
        room::gb(r.free_mem),
        room::gb(r.total_mem),
        room::gb(r.free_disk)
    );
    match room::check_room(r, 0, room::limits()) {
        Ok(()) => Check::ok("host.resources", A, summary),
        Err(why) => Check::warn("host.resources", A, summary)
            .with_detail(format!("too little to boot another device: {why}"))
            .with_fix(Fix::guidance(
                "close apps or stop devices you don't need (`atalanta devices down`); \
                 a swapping host makes every device on it flaky",
            )),
    }
}

/// `~/.atalanta` or `ATALANTA_LEASE_DIR`: the lease directory.
pub(crate) fn lease_dir(ctx: &Ctx) -> PathBuf {
    ctx.env()
        .var("ATALANTA_LEASE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.env().home().join(".atalanta").join("leases"))
}

/// Devices a live process holds the lease of now (a run, the console, an MCP
/// session), as "name (pid N cmd)". Whatever such a process is doing with an
/// adb server, restarting it would interrupt.
pub(crate) fn held_leases(ctx: &Ctx) -> Vec<String> {
    atalanta_runner::lease::scan(&lease_dir(ctx))
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.held)
        .map(|l| {
            format!(
                "{} (pid {} {})",
                l.name,
                l.pid.map_or("?".into(), |p| p.to_string()),
                l.cmd.as_deref().unwrap_or("?")
            )
        })
        .collect()
}

fn state_dirs(ctx: &Ctx) -> Check {
    let env = ctx.env();
    let home = env.home().join(".atalanta");
    let state = env
        .var("ATALANTA_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("state"));
    let mut dirs = vec![
        ("state", home.clone()),
        ("app state", state),
        ("leases", lease_dir(ctx)),
        ("runner cache", home.join("wda")),
    ];
    let results = ctx.opts.cwd.join("atalanta-results");
    dirs.push(("artifacts", results));
    let mut bad = Vec::new();
    for (what, dir) in &dirs {
        if let Err(e) = writable(dir) {
            bad.push(format!("{what}: {} ({e})", dir.display()));
        }
    }
    let detail = dirs
        .iter()
        .map(|(w, d)| format!("{w}: {}", d.display()))
        .collect::<Vec<_>>()
        .join("\n");
    if bad.is_empty() {
        Check::ok(
            "host.state_dirs",
            A,
            "state, lease and artifact directories are writable",
        )
        .with_detail(detail)
    } else {
        Check::fail("host.state_dirs", A, format!("{} not writable", bad.len()))
            .with_detail(bad.join("\n"))
            .with_fix(Fix::guidance(
                "make them writable, or point Atalanta elsewhere: ATALANTA_STATE_DIR, \
             ATALANTA_LEASE_DIR, --artifacts-dir",
            ))
    }
}

/// Whether files can be created in `dir`, or (when it doesn't exist yet)
/// in the nearest ancestor that does, where it would be created.
fn writable(dir: &Path) -> Result<(), String> {
    let existing = dir
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| "no existing parent".to_string())?;
    if !existing.is_dir() {
        return Err(format!("{} is not a directory", existing.display()));
    }
    let probe = existing.join(format!(".atalanta-doctor-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn leases(ctx: &Ctx) -> Check {
    let off = ctx.env().var("ATALANTA_LEASES").is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    });
    if off {
        return Check::ok(
            "host.leases",
            A,
            "cross-process device leases are off (ATALANTA_LEASES)",
        );
    }
    let dir = lease_dir(ctx);
    let files = match atalanta_runner::lease::scan(&dir) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check::ok("host.leases", A, "no device leases yet")
                .with_detail(dir.display().to_string());
        }
        Err(e) => {
            return Check::fail(
                "host.leases",
                A,
                format!("can't read {}: {e}", dir.display()),
            )
            .with_fix(Fix::guidance(
                "set ATALANTA_LEASE_DIR to a writable local directory",
            ))
        }
    };
    let name = |p: &Path| {
        p.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let held: Vec<String> = files
        .iter()
        .filter(|l| l.held)
        .map(|l| {
            format!(
                "{} held by pid {} ({}){}",
                name(&l.path),
                l.pid.map_or("?".into(), |p| p.to_string()),
                l.cmd.as_deref().unwrap_or("?"),
                if l.owner_dead {
                    ": that pid is gone; a child process it started still holds the lock"
                } else {
                    ""
                }
            )
        })
        .collect();
    let stale: Vec<PathBuf> = files
        .iter()
        .filter(|l| l.is_stale())
        .map(|l| l.path.clone())
        .collect();
    let orphan_held = files.iter().any(|l| l.held && l.owner_dead);
    let summary = format!(
        "{} lease file{}: {} in use, {} left by exited processes",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        held.len(),
        stale.len()
    );
    let detail = held.join("\n");
    if orphan_held {
        return Check::warn("host.leases", A, summary)
            .with_detail(detail)
            .with_fix(Fix::guidance(format!(
                "find the process with `lsof +D {}` and stop it",
                dir.display()
            )));
    }
    // A lease file whose owner exited and that nobody holds blocks nobody
    // (`LeaseFile::is_stale`: the lock went with its holder); only the
    // leftover holder line is stale. Say so, don't warn: every finished run
    // leaves one, and a warning made a plain doctor "degraded" (exit 3 with
    // `--strict`) after any run.
    if !stale.is_empty() {
        let detail = held
            .into_iter()
            .chain(
                stale
                    .iter()
                    .map(|p| format!("{}: left by an exited process; blocks nothing", name(p))),
            )
            .collect::<Vec<_>>()
            .join("\n");
        return Check::ok("host.leases", A, summary).with_detail(detail);
    }
    Check::ok("host.leases", A, summary).with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::{Options, Status};

    fn ctx(t: &Tools) -> Ctx {
        Ctx {
            opts: Options {
                env: t.env(),
                cwd: t.path("proj"),
                ..Options::default()
            },
            required: vec![],
            apps: vec![],
        }
    }

    #[test]
    fn stale_leases_block_nobody_and_live_ones_are_named() {
        let mut t = Tools::new();
        let dir = t.path("leases");
        t.set("ATALANTA_LEASE_DIR", &dir.to_string_lossy());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("emulator-5554.lock"),
            "pid=999999998 cmd=atalanta run\n",
        )
        .unwrap();
        // Left by an exited process, held by nobody: blocks nothing, so
        // not a warning (and no fix to offer).
        let c = leases(&ctx(&t));
        assert_eq!(c.status, Status::Ok, "{c:?}");
        assert!(c.summary.contains("1 left by exited processes"), "{c:?}");
        assert!(c.detail.unwrap().contains("blocks nothing"));

        // Our own pid: alive, not stale.
        std::fs::write(
            dir.join("emulator-5554.lock"),
            format!("pid={} cmd=atalanta run\n", std::process::id()),
        )
        .unwrap();
        assert_eq!(leases(&ctx(&t)).status, Status::Ok);

        t.set("ATALANTA_LEASES", "off");
        assert!(leases(&ctx(&t)).summary.contains("off"));
    }

    #[test]
    fn a_missing_lease_dir_is_fine_and_unwritable_state_fails() {
        let mut t = Tools::new();
        t.set(
            "ATALANTA_LEASE_DIR",
            &t.path("nope/leases").to_string_lossy(),
        );
        assert_eq!(leases(&ctx(&t)).status, Status::Ok);
        assert_eq!(state_dirs(&ctx(&t)).status, Status::Ok);
        // A file where the state dir should be.
        std::fs::write(t.path("state-file"), "x").unwrap();
        t.set(
            "ATALANTA_STATE_DIR",
            &t.path("state-file/sub").to_string_lossy(),
        );
        let c = state_dirs(&ctx(&t));
        assert_eq!(c.status, Status::Fail, "{c:?}");
        assert!(c.detail.unwrap().contains("app state"));
    }

    /// A lease a live process holds is what blocks; its holder line naming a
    /// dead pid (a child kept the lock) is still a warning.
    #[test]
    fn a_lease_that_is_really_held_still_warns() {
        let mut t = Tools::new();
        let dir = t.path("leases");
        t.set("ATALANTA_LEASE_DIR", &dir.to_string_lossy());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("emulator-5554.lock");
        std::fs::write(&path, "pid=999999998 cmd=atalanta run\n").unwrap();
        let f = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.try_lock().unwrap();
        let c = leases(&ctx(&t));
        assert_eq!(c.status, crate::Status::Warn, "{c:?}");
        assert_eq!(held_leases(&ctx(&t)).len(), 1);
        drop(f);
        assert!(held_leases(&ctx(&t)).is_empty());
    }
}
