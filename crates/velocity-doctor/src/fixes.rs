//! `velocity doctor --fix`: apply the fixes marked `safe_auto`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Ctx, Options, Report, SafeAction, Status};

/// What applying one fix did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixOutcome {
    /// The check the fix came from.
    pub check_id: String,
    pub action: SafeAction,
    pub ok: bool,
    pub message: String,
}

/// Apply every safe fix of every check that isn't `Ok`, once each, in
/// report order. Nothing interactive, nothing needing privileges; fixes
/// without a [`SafeAction`] are left to the user. Re-run [`crate::run_checks`]
/// afterwards to see the result.
pub async fn apply_safe_fixes(report: &Report, opts: &Options) -> Vec<FixOutcome> {
    let ctx = Ctx {
        opts: opts.clone(),
        required: Vec::new(),
        apps: Vec::new(),
    };
    let mut done: Vec<SafeAction> = Vec::new();
    let mut out = Vec::new();
    for check in &report.checks {
        if matches!(check.status, Status::Ok | Status::Skip) {
            continue;
        }
        let Some(action) = check
            .fix
            .as_ref()
            .filter(|f| f.safe_auto)
            .and_then(|f| f.action.clone())
        else {
            continue;
        };
        if done.contains(&action) {
            continue;
        }
        done.push(action.clone());
        let (ok, message) = apply(&ctx, &action).await;
        out.push(FixOutcome {
            check_id: check.id.clone(),
            action,
            ok,
            message,
        });
    }
    out
}

async fn apply(ctx: &Ctx, action: &SafeAction) -> (bool, String) {
    let env = ctx.env();
    let adb = || crate::android::find_adb(ctx).0;
    match action {
        SafeAction::RestartAdbServer => {
            let Some(adb) = adb() else {
                return (false, "adb not found".into());
            };
            let _ = env
                .run(&adb, &["kill-server"], Duration::from_secs(5))
                .await;
            let started = env
                .run(&adb, &["start-server"], Duration::from_secs(15))
                .await;
            if started.ok() {
                (true, "restarted the adb server".into())
            } else {
                (false, format!("adb start-server: {}", started.why()))
            }
        }
        SafeAction::ReconnectOffline => {
            let Some(adb) = adb() else {
                return (false, "adb not found".into());
            };
            let r = env
                .run(
                    &adb,
                    &["reconnect", "offline"],
                    ctx.opts.timeout.max(Duration::from_secs(10)),
                )
                .await;
            if r.ok() {
                (true, "asked adb to reconnect offline devices".into())
            } else {
                (false, format!("adb reconnect offline: {}", r.why()))
            }
        }
        SafeAction::RemoveStaleLeases { paths } => {
            let mut removed = 0;
            let mut errors = Vec::new();
            for p in paths {
                match velocity_runner::lease::clear_stale(p) {
                    Ok(true) => removed += 1,
                    Ok(false) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push(format!("{}: {e}", p.display())),
                }
            }
            let msg = format!(
                "cleared {removed} of {} stale lease file{}",
                paths.len(),
                if paths.len() == 1 { "" } else { "s" }
            );
            if errors.is_empty() {
                (true, msg)
            } else {
                (false, format!("{msg}; {}", errors.join("; ")))
            }
        }
        SafeAction::RepushTreeServer { device } => {
            let Some(adb) = adb() else {
                return (false, "adb not found".into());
            };
            let server = velocity_android::tree_server::TreeServer::new()
                .with_adb_path(adb.to_string_lossy().into_owned());
            match tokio::time::timeout(Duration::from_secs(30), server.repush(device)).await {
                Ok(Ok(jar)) => (true, format!("pushed the tree server to {device}:{jar}")),
                Ok(Err(e)) => (false, format!("{device}: {e}")),
                Err(_) => (false, format!("{device}: push timed out")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::{Area, Check, Fix};

    #[tokio::test]
    async fn only_safe_fixes_of_failing_checks_are_applied_once_each() {
        let t = Tools::new();
        t.tool(
            "adb",
            &format!("echo \"$*\" >> '{}'", t.path("adb.log").display()),
        );
        let leases = t.path("leases");
        std::fs::create_dir_all(&leases).unwrap();
        let stale = leases.join("emulator-5554.lock");
        std::fs::write(&stale, "pid=999999997 cmd=velocity run\n").unwrap();
        let report = Report::new(
            vec![
                Check::fail("android.adb_server", Area::Android, "wedged")
                    .with_fix(Fix::safe("adb kill-server", SafeAction::RestartAdbServer)),
                Check::warn("android.devices", Area::Android, "offline").with_fix(Fix::safe(
                    "adb reconnect offline",
                    SafeAction::RestartAdbServer,
                )),
                Check::warn("host.leases", Area::Host, "stale").with_fix(Fix::safe(
                    "rm",
                    SafeAction::RemoveStaleLeases {
                        paths: vec![stale.clone()],
                    },
                )),
                Check::fail("ios.license", Area::Ios, "license")
                    .with_fix(Fix::run("sudo xcodebuild -license accept")),
                Check::ok("android.api", Area::Android, "fine")
                    .with_fix(Fix::safe("x", SafeAction::ReconnectOffline)),
            ],
            Duration::ZERO,
        );
        let opts = Options {
            env: t.env(),
            ..Options::default()
        };
        let outcomes = apply_safe_fixes(&report, &opts).await;
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert!(outcomes.iter().all(|o| o.ok), "{outcomes:?}");
        assert_eq!(outcomes[0].check_id, "host.leases");
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            "",
            "cleared, never removed"
        );
        let log = std::fs::read_to_string(t.path("adb.log")).unwrap();
        assert_eq!(log, "kill-server\nstart-server\n");
    }
}
