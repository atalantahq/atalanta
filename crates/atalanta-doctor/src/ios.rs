//! iOS (macOS only): Xcode, its license, simctl, simulators, the runner
//! build cache, devicectl and signing for real devices.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::env::Ran;
use crate::{timed, Area, AreaResult, Check, Ctx, Fix, Platform, Status};

const A: Area = Area::Ios;

/// A simulator from `simctl list devices --json`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sim {
    pub udid: String,
    pub name: String,
    pub runtime: String,
    pub booted: bool,
}

pub(crate) fn parse_sims(json: &str) -> Option<Vec<Sim>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut out = Vec::new();
    for (runtime, list) in v.get("devices")?.as_object()? {
        // com.apple.CoreSimulator.SimRuntime.iOS-26-0 → iOS 26.0
        let rt = runtime
            .rsplit('.')
            .next()
            .unwrap_or(runtime)
            .replacen('-', " ", 1)
            .replace('-', ".");
        for d in list.as_array().into_iter().flatten() {
            if d["isAvailable"].as_bool() == Some(false) {
                continue;
            }
            out.push(Sim {
                udid: d["udid"].as_str().unwrap_or_default().to_string(),
                name: d["name"].as_str().unwrap_or("?").to_string(),
                runtime: rt.clone(),
                booted: d["state"].as_str() == Some("Booted"),
            });
        }
    }
    Some(out)
}

pub(crate) fn xcrun(ctx: &Ctx) -> Option<PathBuf> {
    match ctx.env().var("VELOCITY_XCRUN_PATH") {
        Some(p) => ctx.env().which(&p),
        None => ctx.env().which("xcrun"),
    }
}

pub(crate) async fn checks(ctx: &Ctx) -> AreaResult {
    if std::env::consts::OS != "macos" {
        let c = Check::new(
            "ios.platform",
            A,
            ctx.missing(Platform::Ios),
            "iOS testing needs macOS with Xcode",
        );
        return (vec![c], false);
    }
    let env = ctx.env();
    let t = ctx.opts.timeout;
    let mut out = Vec::new();

    // Xcode and its license.
    let select = match env.which("xcode-select") {
        Some(p) => env.run(&p, &["-p"], t).await,
        None => Ran::NotFound,
    };
    let developer_dir = select.stdout().map(|s| PathBuf::from(s.trim()));
    let xcodebuild = env.which("xcodebuild");
    let version = match &xcodebuild {
        Some(p) => env.run(p, &["-version"], t + Duration::from_secs(2)).await,
        None => Ran::NotFound,
    };
    let Some(dev) = developer_dir else {
        out.push(
            Check::new(
                "ios.xcode",
                A,
                ctx.missing(Platform::Ios),
                "Xcode not found",
            )
            .with_detail(format!("xcode-select -p: {}", select.why()))
            .with_fix(Fix::guidance(
                "install Xcode from the App Store, open it once, then \
                     `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`",
            )),
        );
        return (out, false);
    };
    if dev.to_string_lossy().contains("CommandLineTools") {
        out.push(
            Check::new(
                "ios.xcode",
                A,
                ctx.missing(Platform::Ios),
                "only the Command Line Tools are selected, not Xcode",
            )
            .with_detail(dev.display().to_string())
            .with_fix(Fix::run(
                "sudo xcode-select -s /Applications/Xcode.app/Contents/Developer",
            )),
        );
        return (out, false);
    }
    let text = version.text().unwrap_or_default();
    let license_blocked = text.to_ascii_lowercase().contains("license");
    let (xcode_version, xcode_build) = parse_xcode_version(&text);
    if version.ok() {
        out.push(
            Check::ok(
                "ios.xcode",
                A,
                format!("Xcode {xcode_version} ({xcode_build})"),
            )
            .with_detail(dev.display().to_string()),
        );
    } else if !license_blocked {
        out.push(
            Check::fail(
                "ios.xcode",
                A,
                format!("xcodebuild -version: {}", version.why()),
            )
            .with_detail(dev.display().to_string())
            .with_fix(Fix::guidance(
                "open Xcode once to finish installing its components",
            )),
        );
        return (out, false);
    }
    let license = match &xcodebuild {
        Some(p) => env.run(p, &["-license", "check"], t).await,
        None => Ran::NotFound,
    };
    if license.ok() && !license_blocked {
        out.push(Check::ok("ios.license", A, "Xcode license accepted"));
    } else if !license_blocked && !matches!(license, Ran::Done(..)) {
        // No answer is not "not accepted": `sudo xcodebuild -license accept`
        // would be the wrong advice for a cold or hung Xcode.
        out.push(
            Check::fail(
                "ios.license",
                A,
                format!("xcodebuild -license check: {}", license.why()),
            )
            .with_fix(Fix::guidance(
                "run `xcodebuild -license check` by hand; the first call after a boot can be slow",
            )),
        );
        return (out, false);
    } else {
        out.push(
            Check::fail("ios.license", A, "Xcode license not accepted")
                .with_detail(license.why())
                .with_fix(Fix::run("sudo xcodebuild -license accept")),
        );
        return (out, false);
    }

    // simctl and simulators.
    let Some(xcrun) = xcrun(ctx) else {
        out.push(Check::fail("ios.simctl", A, "xcrun not found"));
        return (out, false);
    };
    let listed = env
        .run(
            &xcrun,
            &["simctl", "list", "devices", "--json"],
            t + Duration::from_secs(2),
        )
        .await;
    let sims = match listed.stdout().as_deref().and_then(parse_sims) {
        Some(s) => s,
        None => {
            // CoreSimulatorService takes 10-20 s to start after a boot, so
            // a slow first answer is not a reason to kill it (and every
            // running simulator with it).
            let fix = if matches!(listed, Ran::TimedOut(_)) {
                Fix::guidance(
                    "CoreSimulator can be slow right after a boot: run `xcrun simctl list devices` \
                     once, then `velocity doctor` again. If it still hangs, \
                     `sudo killall -9 com.apple.CoreSimulator.CoreSimulatorService` restarts it \
                     (and stops every running simulator)",
                )
            } else {
                Fix::run("sudo killall -9 com.apple.CoreSimulator.CoreSimulatorService")
            };
            out.push(
                Check::fail(
                    "ios.simctl",
                    A,
                    format!("simctl list failed: {}", listed.why()),
                )
                .with_fix(fix),
            );
            return (out, false);
        }
    };
    let ms = listed.elapsed().map_or(0, |d| d.as_millis());
    if sims.is_empty() {
        out.push(
            Check::warn(
                "ios.simctl",
                A,
                "simctl works, but no simulator is available",
            )
            .with_fix(Fix::run("xcodebuild -downloadPlatform iOS")),
        );
    } else {
        out.push(Check::ok(
            "ios.simctl",
            A,
            format!(
                "simctl answered in {ms} ms; {} simulators available",
                sims.len()
            ),
        ));
    }
    let booted: Vec<&Sim> = sims.iter().filter(|s| s.booted).collect();
    let leases = velocity_runner::lease::scan(&crate::host::lease_dir(ctx)).unwrap_or_default();
    let in_use = |udid: &str| {
        leases
            .iter()
            .find(|l| l.held && l.path.file_stem().is_some_and(|s| s == udid))
            .map(|l| {
                format!(
                    " [in use: pid {}]",
                    l.pid.map_or("?".into(), |p| p.to_string())
                )
            })
            .unwrap_or_default()
    };
    if booted.is_empty() {
        let first = sims
            .iter()
            .find(|s| s.name.starts_with("iPhone"))
            .map(|s| s.name.clone());
        let status = if ctx.requires(Platform::Ios) {
            Status::Warn
        } else {
            Status::Skip
        };
        out.push(
            Check::new("ios.simulators", A, status, "no simulator booted").with_fix(Fix::run(
                match first {
                    Some(n) => format!("xcrun simctl boot \"{n}\""),
                    None => "velocity devices up --ios 1".into(),
                },
            )),
        );
    } else {
        let lines: Vec<String> = booted
            .iter()
            .map(|s| format!("{} ({}) {}{}", s.name, s.runtime, s.udid, in_use(&s.udid)))
            .collect();
        out.push(
            Check::ok(
                "ios.simulators",
                A,
                format!(
                    "{} booted: {}",
                    booted.len(),
                    booted
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .with_detail(lines.join("\n")),
        );
    }

    out.push(runner(ctx, &dev, &xcode_build));
    let (devicectl, real) = timed_devicectl(ctx, &xcrun).await;
    out.push(devicectl);
    out.push(signing(ctx, real));
    (out, !booted.is_empty() || real > 0)
}

/// `Xcode 26.0.1\nBuild version 17A400` → ("26.0.1", "17A400").
fn parse_xcode_version(out: &str) -> (String, String) {
    let v = out
        .lines()
        .find_map(|l| l.strip_prefix("Xcode "))
        .unwrap_or("?")
        .trim()
        .to_string();
    let b = out
        .lines()
        .find_map(|l| l.strip_prefix("Build version "))
        .unwrap_or("unknown")
        .trim()
        .to_string();
    (v, b)
}

fn runner(ctx: &Ctx, dev: &Path, xcode_build: &str) -> Check {
    let cache = ctx.env().home().join(".velocity").join("wda");
    let r = velocity_ios::wda_bootstrap::runner_cache(&cache, dev, xcode_build);
    let what = format!("{} {}", r.runner, r.wda_ref);
    let detail = format!(
        "{}{}",
        r.dir.display(),
        r.pin_note.map(|n| format!("\n{n}")).unwrap_or_default()
    );
    let unverified = r.testing_interop && r.runner == "WebDriverAgent";
    let c = if r.built {
        Check::ok(
            "ios.runner",
            A,
            format!("{what} built for Xcode {xcode_build}"),
        )
    } else {
        let git = ctx.env().which("git").is_some() || r.runner == "VelocityRunner";
        if !git {
            return Check::fail(
                "ios.runner",
                A,
                format!("{what} not built, and git is missing to fetch it"),
            )
            .with_detail(detail)
            .with_fix(Fix::run("xcode-select --install"));
        }
        Check::ok(
            "ios.runner",
            A,
            format!("{what} not built yet: the first iOS run builds it once (~1 min)"),
        )
    };
    if unverified {
        Check {
            status: Status::Warn,
            ..c
        }
        .with_detail(detail)
        .with_fix(Fix::guidance(
            "this Xcode needs the newer pinned WebDriverAgent, not yet verified here; \
             if its build fails, use Velocity's own runner: VELOCITY_IOS_RUNNER=velocity",
        ))
    } else {
        c.with_detail(detail)
    }
}

/// devicectl: is it there, and which real devices are paired. Returns the
/// number of real devices.
async fn timed_devicectl(ctx: &Ctx, xcrun: &Path) -> (Check, usize) {
    let mut real = 0;
    let c = timed(|| async {
        let env = ctx.env();
        let found = env.run(xcrun, &["--find", "devicectl"], ctx.opts.timeout).await;
        if !found.ok() {
            return Check::skip(
                "ios.devicectl",
                A,
                "devicectl not found (Xcode 15 or newer drives real iPhones)",
            );
        }
        let tmp = std::env::temp_dir().join(format!(
            "velocity-doctor-devicectl-{}.json",
            std::process::id()
        ));
        let tmp_s = tmp.to_string_lossy().into_owned();
        let ran = env
            .run(
                xcrun,
                &["devicectl", "list", "devices", "--json-output", &tmp_s],
                ctx.opts.timeout + Duration::from_secs(3),
            )
            .await;
        let json = std::fs::read_to_string(&tmp).ok();
        let _ = std::fs::remove_file(&tmp);
        // A devicectl that ran and failed (CoreDevice not up, a crash) is
        // not "no real devices paired".
        if let Ran::Done(..) | Ran::Error(_) = &ran {
            if !ran.ok() {
                return Check::warn(
                    "ios.devicectl",
                    A,
                    format!("devicectl failed: {}", ran.why()),
                )
                .with_detail("real iPhones may be missed until it works")
                .with_fix(Fix::guidance(
                    "run `xcrun devicectl list devices` to see the error, then `velocity doctor` again",
                ));
            }
        }
        if let Ran::TimedOut(d) = ran {
            return Check::warn(
                "ios.devicectl",
                A,
                format!("devicectl didn't answer in {:.0} s", d.as_secs_f32()),
            )
            .with_detail("CoreDevice can take 10-20 s after a boot; real iPhones may be missed until it answers")
            .with_fix(Fix::guidance("run `xcrun devicectl list devices` once, then `velocity doctor` again"));
        }
        let devices = json
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .map(|v| velocity_ios::devicectl::parse_devices(&v))
            .unwrap_or_default();
        // Only connected, developer-mode devices need signing now.
        real = devices
            .iter()
            .filter(|d| d.state == velocity_common::DeviceState::Booted)
            .count();
        if devices.is_empty() {
            Check::ok("ios.devicectl", A, "devicectl works; no real devices paired")
        } else {
            Check::ok(
                "ios.devicectl",
                A,
                format!(
                    "{} real device{} paired, {real} connected and ready",
                    devices.len(),
                    if devices.len() == 1 { "" } else { "s" },
                ),
            )
            .with_detail(
                devices
                    .iter()
                    .map(|d| {
                        let state = if d.state == velocity_common::DeviceState::Booted {
                            "connected"
                        } else {
                            "not connected, or developer mode / DDI not ready"
                        };
                        format!("{} {} ({state})", d.name, d.id)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    })
    .await;
    (c, real)
}

fn signing(ctx: &Ctx, real: usize) -> Check {
    if real == 0 {
        return Check::skip("ios.signing", A, "no real iPhone connected");
    }
    let env = ctx.env();
    let asc: Vec<&str> = [
        "VELOCITY_ASC_KEY_PATH",
        "VELOCITY_ASC_KEY_ID",
        "VELOCITY_ASC_ISSUER_ID",
    ]
    .into_iter()
    .filter(|k| env.is_set(k))
    .collect();
    match env.var("VELOCITY_IOS_TEAM_ID") {
        Some(team) => Check::ok(
            "ios.signing",
            A,
            format!("VELOCITY_IOS_TEAM_ID={team}"),
        )
        .with_detail(if asc.is_empty() {
            "no App Store Connect key set: Xcode's signed-in Apple ID signs".to_string()
        } else {
            format!("set: {}", asc.join(", "))
        }),
        None => Check::warn(
            "ios.signing",
            A,
            "a real iPhone is connected but VELOCITY_IOS_TEAM_ID is not set",
        )
        .with_fix(Fix::guidance(
            "set VELOCITY_IOS_TEAM_ID (Xcode → Settings → Accounts → Team ID) so the runner can be \
             signed; without an Apple ID in Xcode also set VELOCITY_ASC_KEY_PATH / _KEY_ID / _ISSUER_ID \
             (`velocity signing check`)",
        )),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::Options;

    fn ctx(t: &Tools) -> Ctx {
        Ctx {
            opts: Options {
                env: t.env(),
                cwd: t.path("proj"),
                ..Options::default()
            },
            required: vec![Platform::Ios],
            apps: vec![],
        }
    }

    const SIMS: &str = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-0":[
      {"udid":"AAAA","name":"iPhone 17 Pro","state":"Booted","isAvailable":true},
      {"udid":"BBBB","name":"iPhone 17","state":"Shutdown","isAvailable":true},
      {"udid":"CCCC","name":"Old","state":"Shutdown","isAvailable":false}]}}"#;

    fn xcode(t: &Tools, license_ok: bool) {
        let dev = t.path("Xcode.app/Contents/Developer");
        std::fs::create_dir_all(&dev).unwrap();
        t.tool("xcode-select", &format!("echo '{}'", dev.display()));
        t.tool(
            "xcodebuild",
            &format!(
                "case \"$1\" in -version) printf 'Xcode 26.0.1\\nBuild version 17A400\\n';; -license) exit {};; esac",
                if license_ok { 0 } else { 1 }
            ),
        );
    }

    fn xcrun(t: &Tools, sims: &str, devicectl: &str) {
        std::fs::write(t.path("sims.json"), sims).unwrap();
        t.tool(
            "xcrun",
            &format!(
                "case \"$1\" in
  simctl) cat '{}';;
  --find) echo /x/devicectl;;
  devicectl) {devicectl};;
esac",
                t.path("sims.json").display()
            ),
        );
    }

    #[tokio::test]
    async fn xcode_simulators_and_runner_cache_are_reported() {
        let t = Tools::new();
        xcode(&t, true);
        xcrun(&t, SIMS, "exit 0");
        t.tool("git", "exit 0");
        let (checks, usable) = checks(&ctx(&t)).await;
        assert!(usable);
        let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap().clone();
        assert_eq!(get("ios.xcode").summary, "Xcode 26.0.1 (17A400)");
        assert_eq!(get("ios.license").status, Status::Ok);
        assert!(get("ios.simctl").summary.contains("2 simulators available"));
        assert_eq!(get("ios.simulators").summary, "1 booted: iPhone 17 Pro");
        let r = get("ios.runner");
        assert!(r.summary.contains("not built yet"), "{r:?}");
        assert_eq!(
            get("ios.devicectl").summary,
            "devicectl works; no real devices paired"
        );
        assert_eq!(get("ios.signing").status, Status::Skip);
    }

    #[tokio::test]
    async fn an_unaccepted_license_blocks_with_the_command() {
        let t = Tools::new();
        xcode(&t, false);
        xcrun(&t, SIMS, "exit 0");
        let (checks, _) = checks(&ctx(&t)).await;
        let l = checks.iter().find(|c| c.id == "ios.license").unwrap();
        assert_eq!(l.status, Status::Fail);
        assert_eq!(
            l.fix.as_ref().unwrap().kind,
            crate::FixKind::Run("sudo xcodebuild -license accept".into())
        );
    }

    #[tokio::test]
    async fn a_license_check_that_does_not_answer_is_not_called_unaccepted() {
        let t = Tools::new();
        let dev = t.path("Xcode.app/Contents/Developer");
        std::fs::create_dir_all(&dev).unwrap();
        t.tool("xcode-select", &format!("echo '{}'", dev.display()));
        t.tool(
            "xcodebuild",
            "case \"$1\" in -version) printf 'Xcode 26.0.1\\nBuild version 17A400\\n';; -license) sleep 5;; esac",
        );
        let mut c = ctx(&t);
        c.opts.timeout = Duration::from_secs(1);
        let (checks, _) = checks(&c).await;
        let l = checks.iter().find(|c| c.id == "ios.license").unwrap();
        assert_eq!(l.status, Status::Fail);
        assert!(!l.summary.contains("not accepted"), "{l:?}");
        assert!(
            matches!(l.fix.as_ref().unwrap().kind, crate::FixKind::Guidance(_)),
            "{l:?}"
        );
    }

    #[tokio::test]
    async fn a_slow_simctl_is_not_met_with_a_killall_of_the_simulator_service() {
        let t = Tools::new();
        xcode(&t, true);
        t.tool("xcrun", "sleep 5");
        let mut c = ctx(&t);
        c.opts.timeout = Duration::from_secs(1);
        let (checks, _) = checks(&c).await;
        let s = checks.iter().find(|c| c.id == "ios.simctl").unwrap();
        assert_eq!(s.status, Status::Fail, "{s:?}");
        assert!(
            matches!(s.fix.as_ref().unwrap().kind, crate::FixKind::Guidance(_)),
            "{s:?}"
        );
        // A simctl that answered with garbage keeps the restart command.
        t.tool("xcrun", "echo nope");
        let (checks, _) = super::checks(&ctx(&t)).await;
        let s = checks.iter().find(|c| c.id == "ios.simctl").unwrap();
        assert!(
            matches!(&s.fix.as_ref().unwrap().kind, crate::FixKind::Run(c) if c.contains("killall")),
            "{s:?}"
        );
    }

    #[tokio::test]
    async fn command_line_tools_only_is_not_xcode() {
        let t = Tools::new();
        t.tool("xcode-select", "echo /Library/Developer/CommandLineTools");
        let (checks, _) = checks(&ctx(&t)).await;
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].summary.contains("Command Line Tools"));
    }

    #[tokio::test]
    async fn no_booted_simulator_suggests_booting_one_and_a_real_device_needs_a_team() {
        let t = Tools::new();
        xcode(&t, true);
        let shutdown = SIMS.replace("\"Booted\"", "\"Shutdown\"");
        let json = r#"{"result":{"devices":[{"identifier":"X","hardwareProperties":{"udid":"00008101-0004581134D2001E","platform":"iOS","reality":"physical"},"deviceProperties":{"name":"Beth's iPhone","developerModeStatus":"enabled"},"connectionProperties":{"tunnelState":"connected","pairingState":"paired","transportType":"wired"}}]}}"#;
        std::fs::write(t.path("dc.json"), json).unwrap();
        xcrun(
            &t,
            &shutdown,
            &format!(
                "while [ \"$1\" != --json-output ]; do shift; done; cp '{}' \"$2\"",
                t.path("dc.json").display()
            ),
        );
        let (checks, _) = checks(&ctx(&t)).await;
        let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap().clone();
        let s = get("ios.simulators");
        assert_eq!(s.status, Status::Warn);
        assert_eq!(
            s.fix.unwrap().kind,
            crate::FixKind::Run("xcrun simctl boot \"iPhone 17 Pro\"".into())
        );
        let dc = get("ios.devicectl");
        assert_eq!(
            dc.summary, "1 real device paired, 1 connected and ready",
            "{dc:?}"
        );
        assert_eq!(get("ios.signing").status, Status::Warn);
    }

    #[tokio::test]
    async fn a_failing_devicectl_is_not_reported_as_no_paired_devices() {
        let t = Tools::new();
        xcode(&t, true);
        xcrun(&t, SIMS, "echo 'CoreDevice error' >&2; exit 1");
        let (checks, _) = checks(&ctx(&t)).await;
        let dc = checks.iter().find(|c| c.id == "ios.devicectl").unwrap();
        assert_eq!(dc.status, Status::Warn, "{dc:?}");
        assert!(dc.summary.contains("failed"), "{dc:?}");
    }

    #[test]
    fn runtimes_read_as_versions() {
        let s = parse_sims(SIMS).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].runtime, "iOS 26.0");
    }
}
