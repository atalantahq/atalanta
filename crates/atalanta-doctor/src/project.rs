// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Project: the suites parse and validate, their apps are installed on a
//! device, and `atalanta-impact.yaml` reads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use atalanta_core::impact::ImpactMapping;

use crate::env::Ran;
use crate::{Area, Check, Ctx, Fix, Options, Platform, Status};

const A: Area = Area::Project;

#[derive(Debug, Default)]
pub(crate) struct Findings {
    pub checks: Vec<Check>,
    /// Platforms the suites declare.
    pub platforms: Vec<Platform>,
    /// (platform, app id) of every suite with both.
    pub apps: Vec<(Platform, String)>,
}

/// The suite files to check: `opts.project` (a file, or a folder's suites),
/// else `atalanta.yaml` or the `atalanta/` folder in `cwd`.
pub(crate) fn suite_files(opts: &Options) -> Option<Vec<PathBuf>> {
    let in_dir = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && atalanta_core::is_suite_file(p))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };
    match &opts.project {
        Some(p) => {
            let p = if p.is_absolute() {
                p.clone()
            } else {
                opts.cwd.join(p)
            };
            Some(if p.is_dir() { in_dir(&p) } else { vec![p] })
        }
        None => {
            let file = opts.cwd.join("atalanta.yaml");
            let dir = opts.cwd.join("atalanta");
            if file.is_file() {
                Some(vec![file])
            } else if dir.is_dir() {
                Some(in_dir(&dir))
            } else {
                None
            }
        }
    }
}

pub(crate) async fn checks(opts: &Options) -> Findings {
    let mut f = Findings::default();
    let Some(files) = suite_files(opts) else {
        f.checks.push(Check::skip(
            "project.suites",
            A,
            "no project here (atalanta.yaml or atalanta/); `atalanta doctor <path>` checks one",
        ));
        return f;
    };
    if files.is_empty() {
        f.checks.push(
            Check::fail("project.suites", A, "no suite files found")
                .with_fix(Fix::run("atalanta init")),
        );
        return f;
    }
    let started = std::time::Instant::now();
    let mut bad = Vec::new();
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut flows: BTreeSet<String> = BTreeSet::new();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut tests = 0;
    for file in &files {
        let shown = file
            .strip_prefix(&opts.cwd)
            .unwrap_or(file)
            .display()
            .to_string();
        dirs.insert(file.parent().map(Path::to_path_buf).unwrap_or_default());
        let path = file.to_string_lossy();
        let parsed = atalanta_core::parse_suite_with_positions(&path).and_then(|(s, pos)| {
            atalanta_core::validate_suite_at(&s, &pos)?;
            atalanta_core::resolve_flows(&s)?;
            Ok(s)
        });
        match parsed {
            Ok(suite) => {
                tests += suite.tests.len();
                names.extend(suite.tests.iter().map(|t| t.name.clone()));
                flows.extend(suite.flows.iter().map(|fl| fl.id.clone()));
                if let Some(p) = suite.config.platform {
                    if !f.platforms.contains(&p) {
                        f.platforms.push(p);
                    }
                    let app = suite.app_id.trim().to_string();
                    if !app.is_empty() && !app.contains("${") && p != Platform::Web {
                        let pair = (p, app);
                        if !f.apps.contains(&pair) {
                            f.apps.push(pair);
                        }
                    }
                }
            }
            Err(e) => bad.push(format!("{shown}: {e}")),
        }
    }
    let mut c = if bad.is_empty() {
        Check::ok(
            "project.suites",
            A,
            format!(
                "{} suite{} valid ({tests} test{})",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                if tests == 1 { "" } else { "s" }
            ),
        )
        .with_detail(
            files
                .iter()
                .map(|p| p.strip_prefix(&opts.cwd).unwrap_or(p).display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        Check::fail(
            "project.suites",
            A,
            format!("{} of {} suites don't validate", bad.len(), files.len()),
        )
        .with_detail(bad.join("\n"))
        .with_fix(Fix::run(format!(
            "atalanta validate {}",
            files[0]
                .strip_prefix(&opts.cwd)
                .unwrap_or(&files[0])
                .display()
        )))
    };
    c.elapsed_ms = started.elapsed().as_millis() as u64;
    f.checks.push(c);

    // The impact map next to each suite folder.
    for dir in &dirs {
        let map = ["atalanta-impact.yaml", "atalanta-impact.yml"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file());
        let Some(map) = map else { continue };
        let shown = map
            .strip_prefix(&opts.cwd)
            .unwrap_or(&map)
            .display()
            .to_string();
        f.checks.push(match ImpactMapping::load(&map) {
            Err(e) => {
                Check::fail("project.impact", A, format!("{shown}: {e}")).with_fix(Fix::guidance(
                    "it maps `tests:` / `flows:` to source path patterns; see docs/CI.md",
                ))
            }
            Ok(m) => {
                let unknown: Vec<String> = m
                    .tests
                    .keys()
                    .filter(|t| !names.contains(*t))
                    .map(|t| format!("test {t:?}"))
                    .chain(
                        m.flows
                            .keys()
                            .filter(|fl| !flows.contains(*fl))
                            .map(|fl| format!("flow {fl:?}")),
                    )
                    .collect();
                let summary = format!(
                    "{shown}: {} test{} and {} flow{} mapped",
                    m.tests.len(),
                    if m.tests.len() == 1 { "" } else { "s" },
                    m.flows.len(),
                    if m.flows.len() == 1 { "" } else { "s" }
                );
                if unknown.is_empty() {
                    Check::ok("project.impact", A, summary)
                } else {
                    Check::warn("project.impact", A, summary)
                        .with_detail(format!("not in any suite: {}", unknown.join(", ")))
                        .with_fix(Fix::guidance(
                            "rename or remove those entries: they select nothing",
                        ))
                }
            }
        });
    }
    f
}

/// Whether `app` can be put in a device shell command as it is: an Android
/// package name is dotted identifiers, nothing else. A suite's `appId` is
/// user-controlled YAML (a cloned repo's), and `adb shell` joins its
/// arguments into one shell line.
fn safe_package_name(app: &str) -> bool {
    !app.is_empty()
        && app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// Whether each project app is installed on a running device. Run after
/// the platform areas so it can use their tools; quick (one call per app
/// and device).
pub(crate) async fn apps_installed(ctx: &Ctx) -> Option<Check> {
    if ctx.apps.is_empty() {
        return None;
    }
    let env = ctx.env();
    let t = ctx.opts.timeout;
    let mut lines = Vec::new();
    let mut missing = Vec::new();
    let mut unreachable = Vec::new();
    let mut checked = 0;
    for (platform, app) in &ctx.apps {
        match platform {
            Platform::Android => {
                let Some(adb) = crate::android::find_adb(ctx).0 else {
                    continue;
                };
                if !safe_package_name(app) {
                    unreachable.push(format!("{app}: not an Android package name, not looked up"));
                    continue;
                }
                let listed = env
                    .run(&adb, &["devices"], t)
                    .await
                    .stdout()
                    .unwrap_or_default();
                for d in crate::android::parse_devices(&listed)
                    .into_iter()
                    .filter(|d| d.state == "device")
                {
                    let mut argv: Vec<String> = atalanta_android::adb::target_args(&d.serial);
                    argv.extend(["shell".into(), format!("pm path {app}")]);
                    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                    let ran = env.run(&adb, &argv, t).await;
                    // `pm path` of a missing package exits 1 with no output;
                    // a shell that never ran says nothing about the app.
                    if !matches!(ran, Ran::Done(..)) {
                        unreachable.push(format!("{app}: can't ask {}: {}", d.serial, ran.why()));
                        continue;
                    }
                    checked += 1;
                    let out = ran.stdout().unwrap_or_default();
                    if out.contains("package:") {
                        lines.push(format!("{app}: installed on {}", d.serial));
                    } else {
                        missing.push(format!("{app}: not installed on {}", d.serial));
                    }
                }
            }
            Platform::Ios => {
                let Some(xcrun) = crate::ios::xcrun(ctx) else {
                    continue;
                };
                let json = env
                    .run(&xcrun, &["simctl", "list", "devices", "--json"], t)
                    .await
                    .stdout()
                    .unwrap_or_default();
                for s in crate::ios::parse_sims(&json)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.booted)
                {
                    let ran = env
                        .run(&xcrun, &["simctl", "get_app_container", &s.udid, app], t)
                        .await;
                    if !matches!(ran, Ran::Done(..)) {
                        unreachable.push(format!(
                            "{app}: can't ask {} ({}): {}",
                            s.name,
                            s.udid,
                            ran.why()
                        ));
                        continue;
                    }
                    checked += 1;
                    if ran.ok() {
                        lines.push(format!("{app}: installed on {} ({})", s.name, s.udid));
                    } else {
                        missing.push(format!("{app}: not installed on {} ({})", s.name, s.udid));
                    }
                }
            }
            Platform::Web => {}
        }
    }
    if checked == 0 {
        return Some(
            Check::skip(
                "project.app_installed",
                A,
                "no running device to look for the app on",
            )
            .with_detail(unreachable.join("\n")),
        );
    }
    let all_missing = lines.is_empty();
    let summary = if all_missing {
        format!(
            "{} not installed on any running device",
            ctx.apps
                .iter()
                .map(|(_, a)| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        lines[0].clone()
            + &if lines.len() > 1 {
                format!(" (+{} more)", lines.len() - 1)
            } else {
                String::new()
            }
    };
    let all: Vec<String> = lines
        .iter()
        .chain(missing.iter())
        .chain(unreachable.iter())
        .cloned()
        .collect();
    let detail = if all.len() > 1 {
        all.join("\n")
    } else {
        String::new()
    };
    // Missing everywhere: a run fails at launch. Missing on some: only
    // those devices can't be used.
    let status = if all_missing {
        Status::Warn
    } else {
        Status::Ok
    };
    let mut c = Check::new("project.app_installed", A, status, summary).with_detail(detail);
    if all_missing {
        c = c.with_fix(Fix::guidance(
            "install the build: `atalanta run <suite> --app <path to .apk/.app>`, or set `config: appPath:`",
        ));
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;

    fn opts(t: &Tools) -> Options {
        Options {
            env: t.env(),
            cwd: t.path("proj"),
            ..Options::default()
        }
    }

    const SUITE: &str = "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: opens\n    steps:\n      - launchApp: {}\n      - assertVisible: { id: \"title\" }\n";

    #[tokio::test]
    async fn suites_in_the_atalanta_folder_are_validated_and_the_impact_map_read() {
        let t = Tools::new();
        let dir = t.path("proj/atalanta");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.yaml"), SUITE).unwrap();
        std::fs::write(
            dir.join("atalanta-impact.yaml"),
            "tests:\n  opens: [\"src/**\"]\n  gone: [\"x/**\"]\n",
        )
        .unwrap();
        let f = checks(&opts(&t)).await;
        assert_eq!(f.platforms, [Platform::Android]);
        assert_eq!(f.apps, [(Platform::Android, "com.example.app".to_string())]);
        assert_eq!(f.checks[0].summary, "1 suite valid (1 test)");
        let impact = &f.checks[1];
        assert_eq!(impact.status, Status::Warn, "{impact:?}");
        assert!(impact.detail.as_ref().unwrap().contains("\"gone\""));
    }

    #[tokio::test]
    async fn a_broken_suite_or_impact_map_is_a_config_failure() {
        let t = Tools::new();
        std::fs::create_dir_all(t.path("proj")).unwrap();
        std::fs::write(
            t.path("proj/atalanta.yaml"),
            "appId: x\ntests:\n  - name: t\n    steps:\n      - frobnicate: {}\n",
        )
        .unwrap();
        std::fs::write(
            t.path("proj/atalanta-impact.yaml"),
            "tests: [not, a, map]\n",
        )
        .unwrap();
        let f = checks(&opts(&t)).await;
        assert_eq!(f.checks[0].status, Status::Fail);
        assert!(f.checks[0].is_config());
        assert_eq!(f.checks[1].status, Status::Fail);
    }

    #[tokio::test]
    async fn no_project_is_skipped() {
        let t = Tools::new();
        let f = checks(&opts(&t)).await;
        assert_eq!(f.checks[0].status, Status::Skip);
    }

    #[tokio::test]
    async fn apps_are_looked_up_on_running_devices() {
        let t = Tools::new();
        t.tool(
            "adb",
            r#"while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  devices) printf 'List of devices attached\nemulator-5554\tdevice\n';;
  shell) case "$2" in *com.example.app*) echo package:/data/app/base.apk;; esac;;
esac"#,
        );
        let mut ctx = Ctx {
            opts: opts(&t),
            required: vec![],
            apps: vec![(Platform::Android, "com.example.app".into())],
        };
        let c = apps_installed(&ctx).await.unwrap();
        assert_eq!(c.summary, "com.example.app: installed on emulator-5554");
        ctx.apps = vec![(Platform::Android, "com.other".into())];
        let c = apps_installed(&ctx).await.unwrap();
        assert_eq!(c.status, Status::Warn);
    }

    #[tokio::test]
    async fn an_app_id_is_never_put_in_a_device_shell_line_unchecked() {
        let t = Tools::new();
        let log = t.path("adb.log");
        t.tool(
            "adb",
            &format!(
                r#"echo "$*" >> '{}'
while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  devices) printf 'List of devices attached\nemulator-5554\tdevice\n';;
esac"#,
                log.display()
            ),
        );
        let ctx = Ctx {
            opts: opts(&t),
            required: vec![],
            apps: vec![(Platform::Android, "com.x; rm -rf /sdcard".into())],
        };
        let c = apps_installed(&ctx).await.unwrap();
        assert_eq!(c.status, Status::Skip, "{c:?}");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!log.contains("rm -rf"), "{log}");
    }

    #[tokio::test]
    async fn a_device_that_does_not_answer_is_not_reported_as_missing_the_app() {
        let t = Tools::new();
        t.tool(
            "adb",
            r#"while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  devices) printf 'List of devices attached\nemulator-5554\tdevice\n';;
  shell) exit 3;;
esac"#,
        );
        let ctx = Ctx {
            opts: opts(&t),
            required: vec![],
            apps: vec![(Platform::Android, "com.example.app".into())],
        };
        // Exit 3 is a run that ended: only a missing package says "missing".
        let c = apps_installed(&ctx).await.unwrap();
        assert_eq!(c.status, Status::Warn, "{c:?}");
        // A shell that can't even start is "unknown", not "not installed".
        t.tool(
            "adb",
            r#"while [ "$1" = "-s" ]; do shift 2; done
case "$1" in
  devices) printf 'List of devices attached\nemulator-5554\tdevice\n';;
  shell) sleep 5;;
esac"#,
        );
        let mut ctx = ctx;
        ctx.opts.timeout = std::time::Duration::from_millis(300);
        let c = apps_installed(&ctx).await.unwrap();
        assert_eq!(c.status, Status::Skip, "{c:?}");
        assert!(c.detail.unwrap().contains("can't ask"));
    }
}
