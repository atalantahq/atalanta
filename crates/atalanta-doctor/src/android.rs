// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Android: adb, its server, devices, the tree server, the emulator.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::env::Ran;
use crate::{timed, Area, AreaResult, Check, Ctx, Fix, Platform, SafeAction, Status};

const A: Area = Area::Android;

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AdbDevice {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, `recovery`, `no permissions`…
    pub state: String,
    pub model: Option<String>,
}

pub(crate) fn parse_devices(out: &str) -> Vec<AdbDevice> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices attached"))
        .skip(1)
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('*') {
                return None;
            }
            let mut words = l.split_whitespace();
            let serial = words.next()?.to_string();
            let rest: Vec<&str> = words.collect();
            // `no permissions (…)` is two words or more.
            let state = if rest.first() == Some(&"no") {
                "no permissions".to_string()
            } else {
                rest.first()?.to_string()
            };
            let model = rest
                .iter()
                .find_map(|w| w.strip_prefix("model:"))
                .map(String::from);
            Some(AdbDevice {
                serial,
                state,
                model,
            })
        })
        .collect()
}

/// The `adb` Atalanta will run: `ATALANTA_ADB_PATH`, else `adb` on PATH.
/// The second value is an adb found in the SDK but not on PATH.
pub(crate) fn find_adb(ctx: &Ctx) -> (Option<PathBuf>, Option<PathBuf>) {
    let env = ctx.env();
    if let Some(p) = env.var("ATALANTA_ADB_PATH") {
        return (env.which(&p), None);
    }
    if let Some(p) = env.which("adb") {
        return (Some(p), None);
    }
    let home = env.home();
    let sdk = sdk_roots(ctx)
        .into_iter()
        .chain([home.join("Library/Android/sdk"), home.join("Android/Sdk")])
        .map(|r| r.join("platform-tools").join("adb"))
        .find(|p| crate::env::is_executable(p));
    (None, sdk)
}

fn sdk_roots(ctx: &Ctx) -> Vec<PathBuf> {
    ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|k| ctx.env().var(k).map(PathBuf::from))
        .collect()
}

/// The emulator binary, as `atalanta devices up` finds it.
fn find_emulator(ctx: &Ctx) -> Option<PathBuf> {
    let env = ctx.env();
    if let Some(p) = env.var("ATALANTA_EMULATOR_PATH") {
        return env.which(&p);
    }
    let home = env.home();
    sdk_roots(ctx)
        .into_iter()
        .chain([home.join("Library/Android/sdk"), home.join("Android/Sdk")])
        .map(|r| r.join("emulator").join("emulator"))
        .find(|p| crate::env::is_executable(p))
        .or_else(|| env.which("emulator"))
}

pub(crate) async fn checks(ctx: &Ctx) -> AreaResult {
    let mut out = Vec::new();
    let (adb, sdk_adb) = find_adb(ctx);
    let Some(adb) = adb else {
        let status = ctx.missing(Platform::Android);
        let mut c = Check::new("android.adb", A, status, "adb not found");
        c = match (sdk_adb, ctx.env().var("ATALANTA_ADB_PATH")) {
            (_, Some(p)) => c
                .with_detail(format!("ATALANTA_ADB_PATH={p} is not an executable"))
                .with_fix(Fix::guidance("point ATALANTA_ADB_PATH at the adb binary, or unset it")),
            (Some(p), None) => {
                let dir = p.parent().unwrap_or(Path::new(".")).display().to_string();
                c.with_detail(format!("the SDK has one at {}, but it is not on PATH", p.display()))
                    .with_fix(Fix::run(format!("export PATH=\"{dir}:$PATH\"")))
            }
            (None, None) => c.with_fix(Fix::guidance(
                "install the Android SDK platform-tools (`brew install --cask android-platform-tools` \
                 on macOS, or Android Studio's SDK Manager) and put adb on PATH",
            )),
        };
        if status == Status::Skip {
            c.summary = "adb not found: Android testing is unavailable".into();
        }
        out.push(c);
        out.push(java());
        return (out, false);
    };
    out.push(Check::ok("android.adb", A, adb.display().to_string()));

    // The server: `adb devices` starts it if needed; one that doesn't
    // answer is wedged.
    let t = ctx.opts.timeout;
    let version = ctx.env().run(&adb, &["version"], t).await;
    let listing = ctx
        .env()
        .run(&adb, &["devices", "-l"], t + Duration::from_secs(2))
        .await;
    let ver = version
        .stdout()
        .map(|s| adb_version(&s))
        .unwrap_or_default();
    // Restarting the one adb server drops every device of every tool on it:
    // not while a live process holds a device lease (a run, the console, an
    // MCP session).
    let busy = crate::host::held_leases(ctx);
    let restart = || {
        if busy.is_empty() {
            Fix::safe(
                format!(
                    "{} kill-server && {} start-server",
                    adb.display(),
                    adb.display()
                ),
                SafeAction::RestartAdbServer,
            )
        } else {
            Fix::guidance(format!(
                "restarting the adb server is withheld: it would cut off {}. Let those finish, \
                 then `{} kill-server && {} start-server`",
                busy.join(", "),
                adb.display(),
                adb.display()
            ))
        }
    };
    let devices = match &listing {
        Ran::Done(o, took) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            let mismatch = err.contains("doesn't match this client") || err.contains("killing");
            let summary = format!("{ver}server answered in {} ms", took.as_millis());
            out.push(if mismatch {
                Check::warn("android.adb_server", A, summary)
                    .with_detail(format!(
                        "another adb version was running the server and was replaced: {}",
                        err.trim()
                    ))
                    .with_fix(Fix::guidance(
                        "use one adb: tools that bundle their own (Android Studio, Genymotion, \
                         an old SDK) restart each other's servers and drop devices mid-run",
                    ))
            } else {
                Check::ok("android.adb_server", A, summary)
            });
            parse_devices(&text)
        }
        Ran::TimedOut(_) => {
            out.push(
                Check::fail(
                    "android.adb_server",
                    A,
                    format!("adb server: {}", listing.why()),
                )
                .with_detail("`adb devices` hung: the server is wedged")
                .with_fix(restart()),
            );
            return (out, false);
        }
        _ => {
            out.push(
                Check::fail(
                    "android.adb_server",
                    A,
                    format!("adb devices failed: {}", listing.why()),
                )
                .with_fix(restart()),
            );
            return (out, false);
        }
    };

    // Devices.
    let online: Vec<&AdbDevice> = devices.iter().filter(|d| d.state == "device").collect();
    let unauthorized: Vec<&AdbDevice> = devices
        .iter()
        .filter(|d| d.state == "unauthorized")
        .collect();
    let offline: Vec<&AdbDevice> = devices.iter().filter(|d| d.state == "offline").collect();
    let other: Vec<&AdbDevice> = devices
        .iter()
        .filter(|d| !matches!(d.state.as_str(), "device" | "unauthorized" | "offline"))
        .collect();
    let leases = crate::host::lease_dir(ctx);
    let leased = atalanta_runner::lease::scan(&leases).unwrap_or_default();
    let in_use = |serial: &str| {
        leased
            .iter()
            .find(|l| l.held && l.path.file_stem().is_some_and(|s| s == serial))
            .map(|l| {
                format!(
                    " [in use: pid {} {}]",
                    l.pid.map_or("?".into(), |p| p.to_string()),
                    l.cmd.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_default()
    };
    let lines: Vec<String> = devices
        .iter()
        .map(|d| {
            format!(
                "{} {}{}{}",
                d.serial,
                d.state,
                d.model
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default(),
                in_use(&d.serial)
            )
        })
        .collect();
    let summary = format!(
        "{} online, {} unauthorized, {} offline{}",
        online.len(),
        unauthorized.len(),
        offline.len(),
        if other.is_empty() {
            String::new()
        } else {
            format!(", {} other", other.len())
        }
    );
    let c = if !unauthorized.is_empty() {
        Check::warn("android.devices", A, summary).with_fix(Fix::guidance(
            "unlock the device and accept \"Allow USB debugging\"; if no prompt shows, \
             revoke USB debugging authorizations in Developer options and reconnect",
        ))
    } else if !offline.is_empty() {
        Check::warn("android.devices", A, summary).with_fix(Fix::safe(
            format!("{} reconnect offline", adb.display()),
            SafeAction::ReconnectOffline,
        ))
    } else if !other.is_empty() {
        Check::warn("android.devices", A, summary).with_fix(Fix::guidance(
            "a device in recovery/sideload or without permissions can't be driven; \
             on Linux, add a udev rule for it",
        ))
    } else if online.is_empty() {
        let s = if ctx.requires(Platform::Android) {
            Status::Warn
        } else {
            Status::Skip
        };
        Check::new(
            "android.devices",
            A,
            s,
            "no Android device or emulator running",
        )
        .with_fix(Fix::run("atalanta devices up --android 1"))
    } else {
        Check::ok("android.devices", A, summary)
    };
    out.push(c.with_detail(lines.join("\n")));

    // API level of each online device (one shell per device).
    if !online.is_empty() {
        let mut lines = Vec::new();
        let mut old = false;
        let mut silent: Vec<&str> = Vec::new();
        for d in &online {
            let args = atalanta_android::adb::target_args(&d.serial);
            let mut argv: Vec<&str> = args.iter().map(String::as_str).collect();
            argv.extend([
                "shell",
                "getprop ro.build.version.sdk; getprop ro.build.version.release; getprop ro.product.model",
            ]);
            let ran = ctx.env().run(&adb, &argv, t).await;
            match ran.stdout() {
                Some(s) => {
                    let v: Vec<&str> = s.lines().map(str::trim).collect();
                    let api: u32 = v.first().and_then(|a| a.parse().ok()).unwrap_or(0);
                    old |= api != 0 && api < 21;
                    lines.push(format!(
                        "{}: Android {} (API {}) {}",
                        d.serial,
                        v.get(1).unwrap_or(&"?"),
                        v.first().unwrap_or(&"?"),
                        v.get(2).unwrap_or(&"")
                    ));
                }
                None => {
                    silent.push(&d.serial);
                    lines.push(format!("{}: can't read: {}", d.serial, ran.why()));
                }
            }
        }
        let summary = lines.first().cloned().unwrap_or_default()
            + &if lines.len() > 1 {
                format!(" (+{} more)", lines.len() - 1)
            } else {
                String::new()
            };
        // Listed as `device` but not answering a shell: frozen, or still
        // booting. It is not usable, whatever its API level.
        let c = if !silent.is_empty() {
            Check::warn("android.api", A, summary).with_fix(Fix::guidance(format!(
                "{} is listed but doesn't answer `adb shell`: wait for it to finish booting, \
                 or restart it (`adb -s {} reconnect`, or cold-boot the emulator)",
                silent.join(", "),
                silent[0]
            )))
        } else if old {
            Check::warn("android.api", A, summary).with_fix(Fix::guidance(
                "Atalanta's tree server needs API 21 or newer",
            ))
        } else {
            Check::ok("android.api", A, summary)
        };
        out.push(c.with_detail(if lines.len() > 1 {
            lines.join("\n")
        } else {
            String::new()
        }));
    }

    // The tree server, on the first online device.
    out.push(match online.first() {
        _ if !ctx.opts.deep => Check::skip(
            "android.tree_server",
            A,
            "not checked (`--deep` pushes it to a device and handshakes)",
        ),
        None => Check::skip("android.tree_server", A, "no online device to check it on"),
        // The opt-out Atalanta itself honours (`TreeServer::new`): usually set
        // because another tool owns the device's one UiAutomation connection.
        Some(_)
            if matches!(
                ctx.env().var("ATALANTA_ANDROID_TREE_SERVER").as_deref(),
                Some("0" | "false" | "off")
            ) =>
        {
            Check::skip(
                "android.tree_server",
                A,
                "disabled by ATALANTA_ANDROID_TREE_SERVER: nothing pushed or started",
            )
        }
        Some(d) => timed(|| tree_server(&adb, &d.serial)).await,
    });

    out.push(timed(|| emulator(ctx, !online.is_empty())).await);
    out.push(java());
    (out, !online.is_empty())
}

/// `Android Debug Bridge version 1.0.41\nVersion 35.0.2-12147458` →
/// "adb 1.0.41 (platform-tools 35.0.2), ".
fn adb_version(out: &str) -> String {
    let proto = out
        .lines()
        .find_map(|l| l.strip_prefix("Android Debug Bridge version "))
        .map(str::trim);
    let tools = out
        .lines()
        .find_map(|l| l.strip_prefix("Version "))
        .map(|v| v.split('-').next().unwrap_or(v).trim());
    match (proto, tools) {
        (Some(p), Some(t)) => format!("adb {p} (platform-tools {t}), "),
        (Some(p), None) => format!("adb {p}, "),
        _ => String::new(),
    }
}

async fn tree_server(adb: &Path, serial: &str) -> Check {
    // Start a server only when nobody else holds the device: another run
    // has its own server there, and a second UiAutomation client would
    // break it. Attaching (a PING) is harmless.
    let lease = atalanta_runner::lease::try_lock(serial).ok();
    let server = atalanta_android::tree_server::TreeServer::new()
        .with_adb_path(adb.to_string_lossy().into_owned());
    let want = atalanta_android::tree_server::TreeServer::server_version();
    let repush = || {
        Fix::safe(
            format!("re-push the tree server to {serial}"),
            SafeAction::RepushTreeServer {
                device: serial.to_string(),
            },
        )
    };
    let probe = tokio::time::timeout(
        Duration::from_secs(15),
        server.probe(serial, lease.is_some()),
    )
    .await;
    drop(lease);
    match probe {
        Err(_) => Check::fail("android.tree_server", A, format!("{serial}: no handshake in 15 s"))
            .with_fix(repush()),
        Ok(Err(e)) => Check::fail("android.tree_server", A, format!("{serial}: {e}"))
            .with_detail(
                "runs fall back to `uiautomator dump` (~2 s per screen read instead of ~3 ms)",
            )
            .with_fix(repush()),
        Ok(Ok(p)) => match p.version {
            None => Check::ok(
                "android.tree_server",
                A,
                format!("{serial}: jar pushed; device in use, start not tried"),
            )
            .with_detail(p.jar),
            Some(v) if v == want => Check::ok(
                "android.tree_server",
                A,
                format!(
                    "{serial}: protocol {v}, handshake ok{}",
                    if p.attached { " (attached to a running server)" } else { "" }
                ),
            )
            .with_detail(p.jar),
            Some(v) => Check::warn(
                "android.tree_server",
                A,
                format!("{serial}: a server speaking protocol {v} is running; this Atalanta speaks {want}"),
            )
            .with_detail("another Atalanta version's server; the next run replaces it"),
        },
    }
}

async fn emulator(ctx: &Ctx, have_devices: bool) -> Check {
    let Some(emu) = find_emulator(ctx) else {
        let status = if have_devices || !ctx.requires(Platform::Android) {
            Status::Skip
        } else {
            Status::Warn
        };
        return Check::new(
            "android.emulator",
            A,
            status,
            "emulator not found (only needed for `atalanta devices up --android`)",
        )
        .with_fix(Fix::guidance(
            "install the Android Emulator from Android Studio's SDK Manager, or set ATALANTA_EMULATOR_PATH",
        ));
    };
    let ran = ctx.env().run(&emu, &["-list-avds"], ctx.opts.timeout).await;
    if !ran.ok() {
        // Not "no AVDs": the emulator itself didn't run (a broken install,
        // a wrong architecture, a hang).
        let status = if have_devices {
            Status::Ok
        } else {
            Status::Warn
        };
        return Check::new(
            "android.emulator",
            A,
            status,
            format!("emulator found, but `-list-avds` failed: {}", ran.why()),
        )
        .with_detail(emu.display().to_string())
        .with_fix(Fix::guidance(
            "reinstall the Android Emulator from Android Studio's SDK Manager, or run it by hand to see why",
        ));
    }
    let avds: Vec<String> = ran
        .text()
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("INFO") && !l.contains(' '))
        .map(String::from)
        .collect();
    if avds.is_empty() {
        let status = if have_devices {
            Status::Ok
        } else {
            Status::Warn
        };
        return Check::new("android.emulator", A, status, "emulator found, no AVDs")
            .with_detail(emu.display().to_string())
            .with_fix(Fix::guidance(
                "create one in Android Studio's Device Manager (or `avdmanager create avd`)",
            ));
    }
    Check::ok(
        "android.emulator",
        A,
        format!(
            "{} AVD{}: {}",
            avds.len(),
            if avds.len() == 1 { "" } else { "s" },
            avds.join(", ")
        ),
    )
    .with_detail(emu.display().to_string())
}

fn java() -> Check {
    Check::ok(
        "android.java",
        A,
        "Java not required (the tree server ships prebuilt in this binary)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::Options;

    fn ctx(t: &Tools, required: bool) -> Ctx {
        Ctx {
            opts: Options {
                env: t.env(),
                cwd: t.path("proj"),
                ..Options::default()
            },
            required: if required {
                vec![Platform::Android]
            } else {
                vec![]
            },
            apps: vec![],
        }
    }

    const ADB: &str = r#"
while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  version) printf 'Android Debug Bridge version 1.0.41\nVersion 35.0.2-12147458\nInstalled as /x/adb\n';;
  devices) printf 'List of devices attached\nemulator-5554          device product:sdk model:Pixel_7 transport_id:1\nR58M123  unauthorized usb:1-1 transport_id:2\nemulator-5556 offline transport_id:3\n';;
  shell) printf '34\n14\nPixel 7\n';;
esac"#;

    #[test]
    fn adb_devices_lines_parse_with_their_state() {
        let d = parse_devices(
            "* daemon started successfully\nList of devices attached\nemulator-5554 device product:x model:Pixel_7\nabc no permissions (user in plugdev); see [x]\n\n",
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model.as_deref(), Some("Pixel_7"));
        assert_eq!(d[1].state, "no permissions");
        assert_eq!(
            adb_version("Android Debug Bridge version 1.0.41\nVersion 35.0.2-12147458\n"),
            "adb 1.0.41 (platform-tools 35.0.2), "
        );
    }

    #[tokio::test]
    async fn devices_states_api_and_fixes_come_from_adb() {
        let mut t = Tools::new();
        t.tool("adb", ADB);
        t.set("ATALANTA_LEASE_DIR", &t.path("leases").to_string_lossy());
        let (checks, usable) = checks(&ctx(&t, true)).await;
        assert!(usable);
        let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap().clone();
        assert_eq!(get("android.adb").status, Status::Ok);
        let server = get("android.adb_server");
        assert!(
            server
                .summary
                .starts_with("adb 1.0.41 (platform-tools 35.0.2)"),
            "{server:?}"
        );
        let devices = get("android.devices");
        assert_eq!(devices.status, Status::Warn);
        assert_eq!(devices.summary, "1 online, 1 unauthorized, 1 offline");
        assert!(matches!(
            devices.fix.unwrap().kind,
            crate::FixKind::Guidance(_)
        ));
        assert!(get("android.api")
            .summary
            .contains("Android 14 (API 34) Pixel 7"));
        assert_eq!(get("android.tree_server").status, Status::Skip);
        assert_eq!(get("android.java").status, Status::Ok);
    }

    #[tokio::test]
    async fn offline_devices_get_a_safe_reconnect() {
        let t = Tools::new();
        t.tool(
            "adb",
            "case \"$1\" in devices) printf 'List of devices attached\\nemulator-5556 offline\\n';; esac",
        );
        let (checks, usable) = checks(&ctx(&t, false)).await;
        assert!(!usable);
        let d = checks.iter().find(|c| c.id == "android.devices").unwrap();
        assert_eq!(
            d.fix.as_ref().unwrap().action,
            Some(SafeAction::ReconnectOffline)
        );
    }

    #[tokio::test]
    async fn a_wedged_adb_server_fails_fast_with_a_restart() {
        let t = Tools::new();
        t.tool("adb", "case \"$1\" in devices) sleep 30;; esac");
        let mut c = ctx(&t, true);
        c.opts.timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let (checks, _) = checks(&c).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        let s = checks
            .iter()
            .find(|c| c.id == "android.adb_server")
            .unwrap();
        assert_eq!(s.status, Status::Fail, "{s:?}");
        assert_eq!(
            s.fix.as_ref().unwrap().action,
            Some(SafeAction::RestartAdbServer)
        );
    }

    #[tokio::test]
    async fn missing_adb_fails_only_when_android_is_wanted() {
        let mut t = Tools::new();
        let sdk = t.path("sdk");
        crate::env::fake::write_script(&sdk.join("platform-tools/adb"), "exit 0");
        t.set("ANDROID_HOME", &sdk.to_string_lossy());
        let (checks, _) = checks(&ctx(&t, true)).await;
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].detail.as_ref().unwrap().contains("not on PATH"));
        assert!(
            matches!(&checks[0].fix.as_ref().unwrap().kind, crate::FixKind::Run(c) if c.contains("platform-tools"))
        );
        let (checks, _) = super::checks(&ctx(&t, false)).await;
        assert_eq!(checks[0].status, Status::Skip);
    }

    #[tokio::test]
    async fn avds_are_listed_from_the_sdk_emulator() {
        let mut t = Tools::new();
        let sdk = t.path("sdk");
        crate::env::fake::write_script(
            &sdk.join("emulator/emulator"),
            "printf 'INFO    | Storing crashdata\\nPixel_7_API_34\\nTablet\\n'",
        );
        t.set("ANDROID_HOME", &sdk.to_string_lossy());
        let c = emulator(&ctx(&t, true), false).await;
        assert_eq!(c.summary, "2 AVDs: Pixel_7_API_34, Tablet");
    }

    #[tokio::test]
    async fn a_device_that_does_not_answer_a_shell_is_not_reported_ok() {
        let t = Tools::new();
        t.tool(
            "adb",
            "while [ \"$1\" = \"-s\" ]; do shift 2; done
case \"$1\" in
  devices) printf 'List of devices attached\\nemulator-5554 device\\n';;
  shell) echo 'error: closed' >&2; exit 1;;
esac",
        );
        let (checks, usable) = checks(&ctx(&t, true)).await;
        assert!(usable);
        let api = checks.iter().find(|c| c.id == "android.api").unwrap();
        assert_eq!(api.status, Status::Warn, "{api:?}");
        assert!(api.summary.contains("can't read"), "{api:?}");
    }

    #[tokio::test]
    async fn a_failing_emulator_binary_is_not_reported_as_having_no_avds() {
        let mut t = Tools::new();
        let sdk = t.path("sdk");
        crate::env::fake::write_script(&sdk.join("emulator/emulator"), "echo boom >&2; exit 1");
        t.set("ANDROID_HOME", &sdk.to_string_lossy());
        let c = emulator(&ctx(&t, true), false).await;
        assert_eq!(c.status, Status::Warn, "{c:?}");
        assert!(c.summary.contains("failed"), "{c:?}");
        assert!(!c.summary.contains("no AVDs"), "{c:?}");
    }

    #[tokio::test]
    async fn deep_leaves_the_device_alone_when_the_tree_server_is_switched_off() {
        let mut t = Tools::new();
        let log = t.path("adb.log");
        t.tool(
            "adb",
            &format!(
                "echo \"$*\" >> '{}'
while [ \"$1\" = \"-s\" ]; do shift 2; done
case \"$1\" in
  devices) printf 'List of devices attached\\nemulator-5554 device\\n';;
  shell) printf '34\\n14\\nPixel 7\\n';;
esac",
                log.display()
            ),
        );
        t.set("ATALANTA_ANDROID_TREE_SERVER", "off");
        t.set("ATALANTA_LEASE_DIR", &t.path("leases").to_string_lossy());
        let mut c = ctx(&t, true);
        c.opts.deep = true;
        let (checks, _) = checks(&c).await;
        let tree = checks
            .iter()
            .find(|c| c.id == "android.tree_server")
            .unwrap();
        assert_eq!(tree.status, Status::Skip, "{tree:?}");
        let log = std::fs::read_to_string(&log).unwrap();
        assert!(!log.contains("push") && !log.contains("forward"), "{log}");
    }

    #[tokio::test]
    async fn an_adb_restart_is_not_offered_while_a_live_process_holds_a_lease() {
        let mut t = Tools::new();
        t.tool("adb", "case \"$1\" in devices) sleep 30;; esac");
        let leases = t.path("leases");
        std::fs::create_dir_all(&leases).unwrap();
        t.set("ATALANTA_LEASE_DIR", &leases.to_string_lossy());
        let path = leases.join("emulator-5554.lock");
        std::fs::write(
            &path,
            format!("pid={} cmd=atalanta run\n", std::process::id()),
        )
        .unwrap();
        let f = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.try_lock().unwrap();
        let mut c = ctx(&t, true);
        c.opts.timeout = Duration::from_millis(300);
        let (checks, _) = checks(&c).await;
        let s = checks
            .iter()
            .find(|c| c.id == "android.adb_server")
            .unwrap();
        assert_eq!(s.status, Status::Fail);
        let fix = s.fix.as_ref().unwrap();
        assert!(!fix.safe_auto && fix.action.is_none(), "{fix:?}");
        assert!(
            matches!(&fix.kind, crate::FixKind::Guidance(g) if g.contains("withheld") && g.contains("emulator-5554")),
            "{fix:?}"
        );
        drop(f);
    }
}
