//! `velocity init`: from nothing to a first passing test.
//!
//! Finds a running device, picks the app (the one in front, or the user's
//! choice), launches it, reads its first screen and writes a suite whose
//! test asserts one or two stable, distinctive elements of that screen:
//! a resource-id / accessibility id first, else a short unique text that
//! isn't volatile (no digits, times or counters). Plus a CI example, a
//! `.gitignore` entry for run artifacts, the suite JSON Schema next to the
//! suite (with a `yaml-language-server` modeline and a `.vscode/settings.json`
//! `yaml.schemas` entry, so editors complete and check it) and, optionally,
//! an impact map.
//!
//! Every YAML document is built as a `serde_yaml::Value` and emitted by
//! serde_yaml (values never spliced into YAML text), then parsed and
//! validated before it is written. Existing files are never overwritten
//! without `--force`.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;
use colored::Colorize;
use serde_yaml::{Mapping, Value};
use velocity_common::selector_build::stable_id;
use velocity_common::selector_match::{is_volatile_id, match_count, min_visible_area, shown_with};
use velocity_common::{DeviceState, Element, Platform, PlatformDriver, Rect, Selector};

use super::create_driver;

#[derive(Args)]
pub struct InitArgs {
    /// ios, android or web [default: the platform with a running device;
    /// asked when there are several]
    #[arg(short, long)]
    platform: Option<String>,

    /// Package / bundle id of the app to test [default: the app in front,
    /// or asked]. On the web: the start URL (same as --url)
    #[arg(long, value_name = "ID")]
    app_id: Option<String>,

    /// Web: the URL to start at
    #[arg(long)]
    url: Option<String>,

    /// Device or simulator to read the app's first screen from [default:
    /// the only running one, or asked]
    #[arg(short, long)]
    device: Option<String>,

    /// Folder for the suite (relative to the current directory)
    #[arg(long, default_value = "velocity", value_name = "PATH")]
    dir: PathBuf,

    /// Don't ask anything: take the defaults (for CI and scripts)
    #[arg(short = 'y', long)]
    yes: bool,

    /// Overwrite files that already exist
    #[arg(long)]
    force: bool,

    /// Run the new suite right away
    #[arg(long)]
    run: bool,

    /// Also write a `velocity-impact.yaml` stub (for `run --changed-since`)
    #[arg(long)]
    impact: bool,
}

pub async fn execute(args: InitArgs) -> anyhow::Result<i32> {
    let started = Instant::now();
    let interactive = !args.yes && std::io::stdin().is_terminal();
    let mut prompt = Prompt { interactive };

    // 1. Platform and device.
    let explicit = args
        .platform
        .as_deref()
        .map(|p| match p {
            "ios" => Ok(Platform::Ios),
            "android" => Ok(Platform::Android),
            "web" => Ok(Platform::Web),
            other => Err(usage_err!(
                "unknown platform '{other}': use ios, android or web"
            )),
        })
        .transpose()?;
    let web_url = args.url.clone().or_else(|| {
        args.app_id
            .clone()
            .filter(|a| a.contains("://") || a.starts_with("data:"))
    });
    let platform = match (explicit, &args.device, &web_url) {
        (Some(p), _, _) => p,
        (None, Some(d), _) => Platform::of_device_id(d),
        (None, None, Some(_)) => Platform::Web,
        (None, None, None) => {
            println!("{} Looking for running devices…", "=>".cyan().bold());
            let found = running_devices(&[Platform::Android, Platform::Ios]).await;
            let mut platforms: Vec<Platform> = Vec::new();
            for (p, _) in &found {
                if !platforms.contains(p) {
                    platforms.push(*p);
                }
            }
            match platforms.len() {
                0 if interactive => {
                    println!("  no emulator or simulator is running");
                    if prompt.confirm("Set up a web project instead?", false)? {
                        Platform::Web
                    } else {
                        return Err(super::no_device(
                            "no running device: boot one (`velocity devices up --android 1` or `--ios 1`) and run `velocity init` again",
                            vec![],
                        ));
                    }
                }
                0 => {
                    return Err(super::no_device(
                        "no running device: boot one (`velocity devices up --android 1` or `--ios 1`), or pass --platform web --url <URL>",
                        vec![],
                    ))
                }
                1 => platforms[0],
                _ => {
                    let names: Vec<String> = platforms.iter().map(|p| p.to_string()).collect();
                    platforms[prompt.choose("Which platform?", &names, 0)?]
                }
            }
        }
    };

    let (device, url) = if platform == Platform::Web {
        let url = match web_url {
            Some(u) => u,
            None if interactive => prompt.ask("URL to test", "https://")?,
            None => usage_bail!("web: pass --url <URL> (the page the test starts at)"),
        };
        if url.trim().is_empty() || url.trim() == "https://" {
            usage_bail!("web: a URL is needed");
        }
        (None, Some(url.trim().to_string()))
    } else {
        let device = match &args.device {
            Some(d) => d.clone(),
            None => {
                let found = running_devices(&[platform]).await;
                let ids: Vec<(String, String)> = found
                    .into_iter()
                    .map(|(_, d)| (d.id.clone(), format!("{}  {}", d.name, d.id)))
                    .collect();
                match ids.len() {
                    0 => {
                        return Err(super::no_device(
                            &format!(
                                "no running {platform} device: boot one (`velocity devices up --{platform} 1`)"
                            ),
                            vec![],
                        ))
                    }
                    1 => ids[0].0.clone(),
                    _ => {
                        let labels: Vec<String> = ids.iter().map(|(_, l)| l.clone()).collect();
                        ids[prompt.choose("Which device?", &labels, 0)?].0.clone()
                    }
                }
            }
        };
        (Some(device), None)
    };

    // 2. The app.
    let app_id = match (platform, &device) {
        (Platform::Web, _) => url.clone().unwrap_or_default(),
        (_, Some(device)) => match &args.app_id {
            Some(a) => a.clone(),
            None => pick_app(platform, device, &mut prompt).await?,
        },
        _ => unreachable!("native platforms have a device"),
    };
    if platform == Platform::Android {
        velocity_android::shell::validate_app_id(&app_id).map_err(|e| usage_err!("{e}"))?;
    }

    // 3. Where the files go; refuse to overwrite before touching the device.
    let cwd = std::env::current_dir()?;
    let dir = if args.dir.is_absolute() {
        args.dir.clone()
    } else {
        cwd.join(&args.dir)
    };
    let slug = app_slug(&app_id, platform);
    let suite_path = dir.join(format!("{slug}.yaml"));
    let ci_path = dir.join("ci").join("github-actions.example.yml");
    let impact_path = dir.join("velocity-impact.yaml");
    let want_impact = args.impact
        || (interactive
            && prompt.confirm(
                "Add a velocity-impact.yaml stub (run only affected tests with --changed-since)?",
                false,
            )?);
    let mut targets = vec![suite_path.clone(), ci_path.clone()];
    if want_impact {
        targets.push(impact_path.clone());
    }
    let existing: Vec<String> = targets
        .iter()
        .filter(|p| p.exists())
        .map(|p| shown(p, &cwd))
        .collect();
    if !existing.is_empty() && !args.force {
        usage_bail!(
            "{} already exist{}; pass --force to overwrite",
            existing.join(", "),
            if existing.len() == 1 { "s" } else { "" }
        );
    }

    // 4. Read the first screen.
    let picks = match &device {
        Some(device) => {
            println!(
                "{} Launching {} on {} to read its first screen…",
                "=>".cyan().bold(),
                app_id.bold(),
                device
            );
            capture(platform, device, &app_id, args.run).await?
        }
        None => {
            println!(
                "{} Opening {} in headless Chrome…",
                "=>".cyan().bold(),
                app_id.bold()
            );
            capture_web(&app_id).await?
        }
    };
    if picks.is_empty() {
        println!(
            "  {} found nothing stable to assert on the first screen; the test only launches the app (add assertions with `velocity inspect`)",
            "!".yellow()
        );
    } else {
        for p in &picks {
            println!("  {} assert {}", "✓".green(), describe(&p.selector));
        }
    }

    // 5. Write.
    let suite = suite_yaml(&app_id, platform, &slug, &shown(&suite_path, &cwd), &picks);
    check_suite(&suite)?;
    let mut created = Vec::new();
    let mut notes = Vec::new();
    write_file(&suite_path, &suite, &cwd, &mut created)?;
    // The schema the suite's modeline points at. A different one already
    // there (an older Velocity's) is kept unless --force.
    let schema_path = dir.join(SCHEMA_FILE);
    let schema = schema_json()?;
    match std::fs::read_to_string(&schema_path) {
        Ok(old) if old == schema => {}
        Ok(_) if !args.force => notes.push(format!(
            "kept {} (refresh it with --force or `velocity schema > {0}`)",
            shown(&schema_path, &cwd)
        )),
        _ => write_file(&schema_path, &schema, &cwd, &mut created)?,
    }
    // Re-read from disk the way `run` will: the file, not the string.
    let (parsed, pos) = velocity_core::parse_suite_with_positions(&suite_path.to_string_lossy())?;
    velocity_core::validate_suite_at(&parsed, &pos)?;
    let suite_rel = shown(&suite_path, &cwd);
    let dir_rel = shown(&dir, &cwd);
    write_file(
        &ci_path,
        &ci_example(platform, &dir_rel),
        &cwd,
        &mut created,
    )?;
    if want_impact {
        write_file(
            &impact_path,
            &impact_yaml(&test_name(&slug)),
            &cwd,
            &mut created,
        )?;
    }
    match update_gitignore(&cwd)? {
        GitIgnore::Created => created.push(".gitignore".into()),
        GitIgnore::Updated => created.push(".gitignore (added velocity-results/)".into()),
        GitIgnore::Unchanged => {}
    }
    match update_vscode_settings(&cwd, &dir) {
        Ok(VsCode::Created) => created.push(VSCODE_SETTINGS.into()),
        Ok(VsCode::Updated) => created.push(format!("{VSCODE_SETTINGS} (added yaml.schemas)")),
        Ok(VsCode::Unchanged) => {}
        Ok(VsCode::Skipped(why)) => notes.push(format!(
            "left {VSCODE_SETTINGS} alone ({why}); for completion in VS Code add\n      {}",
            vscode_entry(&cwd, &dir).unwrap_or_default()
        )),
        Err(e) => notes.push(format!("couldn't update {VSCODE_SETTINGS}: {e}")),
    }
    println!("{} Created:", "=>".green().bold());
    for c in &created {
        println!("  {c}");
    }
    for n in &notes {
        println!("  {} {n}", "!".yellow());
    }

    // 6. Run it?
    let run_now = args.run || (interactive && prompt.confirm("Run it now?", true)?);
    let mut code = 0;
    if run_now {
        println!();
        println!("{} velocity run {suite_rel}", "=>".cyan().bold());
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("run").arg(&suite_path);
        if let Some(d) = &device {
            cmd.args(["--device", d]);
        }
        let status = cmd.status()?;
        code = status.code().unwrap_or(5);
        println!();
        if code == 0 {
            println!(
                "{} Green in {:.1} s from `velocity init`.",
                "✓".green().bold(),
                started.elapsed().as_secs_f64()
            );
        } else {
            println!(
                "{} The first run failed (exit {code}); the report is in velocity-results/. `velocity doctor` checks this machine.",
                "✗".red().bold()
            );
        }
    }

    println!();
    println!("{}", "Next steps:".bold());
    let mut steps: Vec<(String, &str)> = Vec::new();
    if !run_now {
        steps.push((
            format!(
                "velocity run {suite_rel}{}",
                device
                    .as_deref()
                    .map(|d| format!(" -d {d}"))
                    .unwrap_or_default()
            ),
            "run the new test",
        ));
    }
    steps.push((
        "velocity inspect".into(),
        "pick selectors from the live screen",
    ));
    steps.push((
        format!("velocity run {dir_rel}/"),
        "run every suite in the folder",
    ));
    steps.push(("velocity doctor".into(), "check this machine and project"));
    steps.push((
        "velocity mcp install".into(),
        "let an AI agent drive your devices (MCP)",
    ));
    let w = steps
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    for (cmd, what) in steps {
        println!("  {cmd:<w$}  {}", what.dimmed());
    }
    Ok(code)
}

// ── Devices and apps ────────────────────────────────────────────────────

/// Booted devices of `platforms` (simulators only for iOS: listing real
/// iPhones can take 30 s; pass `--device` for one).
async fn running_devices(platforms: &[Platform]) -> Vec<(Platform, velocity_common::DeviceInfo)> {
    let mut out = Vec::new();
    for p in platforms {
        let listed = match p {
            Platform::Ios => velocity_ios::simctl::Simctl::new().list_devices().await,
            Platform::Android => velocity_android::adb::Adb::new().list_devices().await,
            Platform::Web => Ok(vec![]),
        };
        for d in listed.unwrap_or_default() {
            if d.state == DeviceState::Booted {
                out.push((*p, d));
            }
        }
    }
    out
}

/// Ask which app (or take the one in front with `--yes`).
async fn pick_app(platform: Platform, device: &str, prompt: &mut Prompt) -> anyhow::Result<String> {
    let (apps, front) = match platform {
        Platform::Android => android_apps(device).await,
        Platform::Ios => ios_apps(device).await,
        Platform::Web => (vec![], None),
    };
    if !prompt.interactive {
        return match front {
            Some(f) => {
                println!("  using the app in front: {}", f.bold());
                Ok(f)
            }
            None => usage_bail!(
                "pass --app-id <id>: no app is in front on {device} to default to{}",
                if apps.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (installed: {})",
                        apps.iter()
                            .take(8)
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ),
        };
    }
    let mut labels: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    if let Some(f) = &front {
        ids.push(f.clone());
        labels.push(format!("{f}  (in front now)"));
    }
    for (id, name) in &apps {
        if Some(id) != front.as_ref() {
            ids.push(id.clone());
            labels.push(match name {
                Some(n) => format!("{id}  ({n})"),
                None => id.clone(),
            });
        }
    }
    if ids.is_empty() {
        let id = prompt.ask("App id (package / bundle id)", "")?;
        if id.trim().is_empty() {
            usage_bail!("an app id is needed");
        }
        return Ok(id.trim().to_string());
    }
    labels.push("another app id…".into());
    let i = prompt.choose("Which app?", &labels, 0)?;
    if i == ids.len() {
        let id = prompt.ask("App id", "")?;
        if id.trim().is_empty() {
            usage_bail!("an app id is needed");
        }
        return Ok(id.trim().to_string());
    }
    Ok(ids[i].clone())
}

/// Third-party packages and the foreground app (not a launcher or System UI).
async fn android_apps(device: &str) -> (Vec<(String, Option<String>)>, Option<String>) {
    let adb = velocity_android::adb::Adb::new();
    let packages = adb
        .run_device(device, &["shell", "pm", "list", "packages", "-3"])
        .await
        .unwrap_or_default();
    let mut apps: Vec<(String, Option<String>)> = packages
        .lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .map(|p| (p.to_string(), None))
        .collect();
    apps.sort();
    let dump = adb
        .run_device(
            device,
            &[
                "shell",
                "dumpsys activity activities | grep -E 'topResumedActivity|mResumedActivity'",
            ],
        )
        .await
        .unwrap_or_default();
    (apps, android_foreground(&dump))
}

/// The package of `topResumedActivity=ActivityRecord{… u0 com.app/.Main t12}`,
/// unless it is a launcher or System UI.
fn android_foreground(dumpsys: &str) -> Option<String> {
    dumpsys.lines().find_map(|l| {
        let rec = l.split_once("ActivityRecord{")?.1;
        let component = rec.split_whitespace().find(|w| w.contains('/'))?;
        let pkg = component.split('/').next()?.to_string();
        let lower = pkg.to_ascii_lowercase();
        (!lower.contains("launcher") && !lower.contains("systemui") && pkg.contains('.'))
            .then_some(pkg)
    })
}

/// User apps on a simulator (`simctl listapps`) and, when exactly one of
/// them is running, that one as the default.
async fn ios_apps(device: &str) -> (Vec<(String, Option<String>)>, Option<String>) {
    let run = |args: Vec<String>| async move {
        let mut cmd = tokio::process::Command::new(
            velocity_common::env::var("VELOCITY_XCRUN_PATH").unwrap_or_else(|_| "xcrun".into()),
        );
        cmd.args(&args);
        velocity_common::process::output_with_timeout(&mut cmd, Duration::from_secs(15))
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let listed = run(vec!["simctl".into(), "listapps".into(), device.into()]).await;
    let apps = ios_user_apps(&listed);
    let running = run(vec![
        "simctl".into(),
        "spawn".into(),
        device.into(),
        "launchctl".into(),
        "list".into(),
    ])
    .await;
    let up: Vec<&String> = apps
        .iter()
        .map(|(id, _)| id)
        .filter(|id| running.contains(&format!("UIKitApplication:{id}[")))
        .collect();
    let front = (up.len() == 1).then(|| up[0].clone());
    (apps, front)
}

/// `(bundle id, display name)` of each `ApplicationType = User` app in a
/// `simctl listapps` plist (the old ASCII format).
fn ios_user_apps(plist: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut current: Option<(String, Option<String>, bool)> = None;
    for line in plist.lines() {
        let top = line.starts_with("    ") && !line.starts_with("     ");
        if top && line.trim_end().ends_with('{') {
            if let Some((id, name, true)) = current.take() {
                out.push((id, name));
            }
            let key = line
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            current = Some((key.to_string(), None, false));
            continue;
        }
        let Some(cur) = current.as_mut() else {
            continue;
        };
        let t = line.trim();
        if let Some(v) = t.strip_prefix("ApplicationType = ") {
            cur.2 = v.trim_end_matches(';').trim() == "User";
        } else if let Some(v) = t.strip_prefix("CFBundleDisplayName = ") {
            cur.1 = Some(v.trim_end_matches(';').trim().trim_matches('"').to_string());
        }
    }
    if let Some((id, name, true)) = current {
        out.push((id, name));
    }
    out.retain(|(id, _)| !id.ends_with(".xctrunner"));
    out.sort();
    out
}

// ── Reading the first screen ────────────────────────────────────────────

/// An element the test asserts, and why it was picked.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub selector: Selector,
}

/// Launch the app and read its screen until two reads agree (or 10 s).
/// Holds the device's lease meanwhile: another Velocity run is never
/// disturbed.
async fn capture(
    platform: Platform,
    device: &str,
    app_id: &str,
    keep_runner: bool,
) -> anyhow::Result<Vec<Pick>> {
    let lease = lease(device).await?;
    if platform == Platform::Ios && keep_runner {
        // Leave the runner up for the `--run` that follows (it reattaches
        // instead of paying ~17 s to launch it again).
        let mut s = velocity_common::env::settings_snapshot();
        s.insert("VELOCITY_KEEP_WDA".into(), "1".into());
        velocity_common::env::set_settings(s);
    }
    let driver = create_driver(platform);
    let result = read_screen(driver.as_ref(), platform, device, app_id).await;
    driver.cleanup().await;
    drop(lease);
    result
}

async fn read_screen(
    driver: &dyn PlatformDriver,
    platform: Platform,
    device: &str,
    app_id: &str,
) -> anyhow::Result<Vec<Pick>> {
    driver.prepare(device).await?;
    driver.launch_app(device, app_id, false).await?;
    let screen = driver.screen_size(device).await.ok().map(|(w, h)| Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<Vec<Pick>> = None;
    loop {
        let tree = driver.get_hierarchy(device).await?;
        let picks = pick_assertions(&tree, screen.as_ref(), platform, app_id);
        if !picks.is_empty() && last.as_ref() == Some(&picks) {
            return Ok(picks);
        }
        if Instant::now() > deadline {
            return Ok(last.unwrap_or(picks));
        }
        last = Some(picks);
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

async fn capture_web(url: &str) -> anyhow::Result<Vec<Pick>> {
    let driver = create_driver(Platform::Web);
    let device = driver
        .list_devices()
        .await?
        .into_iter()
        .next()
        .map(|d| d.id)
        .unwrap_or_else(|| "web".into());
    let result = read_screen(driver.as_ref(), Platform::Web, &device, url).await;
    driver.cleanup().await;
    result
}

/// Take `device`'s cross-process lease, waiting up to a minute for a run
/// that holds it.
async fn lease(device: &str) -> anyhow::Result<velocity_runner::lease::DeviceLock> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut said = false;
    loop {
        match velocity_runner::lease::try_lock(device) {
            Ok(l) => return Ok(l),
            Err(velocity_runner::lease::LeaseError::Held(holder)) if Instant::now() < deadline => {
                if !said {
                    println!("  {device} is in use ({holder}); waiting for it…");
                    said = true;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                return Err(
                    anyhow::Error::new(velocity_common::VelocityError::DeviceNotFound {
                        id: device.to_string(),
                        available: vec![],
                    })
                    .context(format!("{device} is busy: {e}")),
                )
            }
        }
    }
}

/// Ids every Android app has that say nothing about this one.
fn generic_id(id: &str) -> bool {
    id.starts_with("android:id/")
        || id.starts_with("com.android.systemui:")
        || matches!(
            id.rsplit_once(":id/").map_or(id, |(_, s)| s),
            "content"
                | "action_bar"
                | "action_bar_root"
                | "toolbar"
                | "decor_content_parent"
                | "statusBarBackground"
                | "navigationBarBackground"
                | "main"
                | "root"
                | "container"
        )
}

/// Text that will read the same next run: short, a few words, no digits
/// (times, counts, prices, dates), not a placeholder.
fn stable_text(t: &str) -> bool {
    let t = t.trim();
    let n = t.chars().count();
    (2..=32).contains(&n)
        && t.split_whitespace().count() <= 4
        && !t
            .chars()
            .any(|c| c.is_ascii_digit() || matches!(c, '%' | ':' | '$' | '€' | '£' | '@'))
        && t.chars().filter(|c| c.is_alphabetic()).count() >= 2
        && !t.contains('\n')
}

/// Up to two assertions for the screen in `root`: elements on screen,
/// developer ids (`id:`) first, then short unique texts, each matching
/// exactly one element; at most one per element, top of the screen first.
pub fn pick_assertions(
    root: &Element,
    screen: Option<&Rect>,
    platform: Platform,
    app_id: &str,
) -> Vec<Pick> {
    let mut all = Vec::new();
    collect(root, &mut all);
    // The setting is read once, not per element.
    let min_area = min_visible_area();
    let visible = |e: &Element| {
        e.visible
            && e.bounds.width > 0
            && e.bounds.height > 0
            && screen.is_none_or(|s| shown_with(e, s, min_area))
    };
    let short_prefix = format!("{app_id}:id/");
    let mut ids: Vec<(i32, Selector)> = Vec::new();
    let mut texts: Vec<(i32, Selector, String)> = Vec::new();
    for e in all.into_iter().filter(|e| visible(e)) {
        let editable = e.element_type.to_ascii_lowercase().contains("edit")
            || e.element_type.to_ascii_lowercase().contains("textfield");
        if let Some(id) = stable_id(e, platform) {
            if !is_volatile_id(id) && !generic_id(id) && !id.contains(char::is_whitespace) {
                // The app's own ids in short form (`id: title`); others whole.
                let sel = Selector::Id(id.strip_prefix(&short_prefix).unwrap_or(id).to_string());
                // An iOS id that is only its label is a text in disguise:
                // it must read stable too.
                let label_only = e.label.as_deref() == Some(id) || e.text.as_deref() == Some(id);
                if (!label_only || stable_text(id)) && match_count(root, &sel, screen) == 1 {
                    ids.push((e.bounds.y, sel));
                    continue;
                }
            }
        }
        if editable {
            continue;
        }
        let text = e
            .text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .or(e.label.as_deref());
        if let Some(t) = text.map(str::trim).filter(|t| stable_text(t)) {
            let sel = Selector::Text(t.to_string());
            if match_count(root, &sel, screen) == 1 {
                texts.push((e.bounds.y, sel, t.to_string()));
            }
        }
    }
    // Layout wrappers last: an id naming what the screen shows says more.
    let layout = |s: &Selector| {
        let id = s.to_string().to_ascii_lowercase();
        [
            "container",
            "layout",
            "frame",
            "root",
            "content",
            "wrapper",
            "scroll",
        ]
        .iter()
        .any(|w| id.contains(w))
    };
    ids.sort_by_key(|(y, s)| (layout(s), *y));
    texts.sort_by_key(|(y, _, _)| *y);
    let mut picks: Vec<Pick> = Vec::new();
    for (_, sel) in ids.into_iter().take(1) {
        picks.push(Pick { selector: sel });
    }
    for (_, sel, _) in texts {
        if picks.len() >= 2 {
            break;
        }
        if !picks.iter().any(|p| p.selector == sel) {
            picks.push(Pick { selector: sel });
        }
    }
    picks
}

fn collect<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    out.push(e);
    for c in &e.children {
        collect(c, out);
    }
}

fn describe(s: &Selector) -> String {
    serde_yaml::to_string(&selector_value(s))
        .unwrap_or_default()
        .trim()
        .replace('\n', ", ")
}

// ── Generating ──────────────────────────────────────────────────────────

/// The file name for an app: the last part of a package / bundle id
/// (`com.android.settings` → `settings`), a web URL's host.
fn app_slug(app_id: &str, platform: Platform) -> String {
    let raw = if platform == Platform::Web {
        app_id
            .split_once("://")
            .map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or(r))
            .unwrap_or("web")
            .split(':')
            .next()
            .unwrap_or("web")
            .trim_start_matches("www.")
            .to_string()
    } else {
        app_id.rsplit('.').next().unwrap_or(app_id).to_string()
    };
    let slug: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "app".into()
    } else {
        slug
    }
}

fn test_name(slug: &str) -> String {
    format!("{slug} opens on its first screen")
}

/// A selector as its DSL map (`{ id: … }`). `init` only picks ids and texts.
fn selector_value(s: &Selector) -> Value {
    let (k, v) = match s {
        Selector::Id(v) => ("id", v.clone()),
        Selector::AccessibilityId(v) => ("accessibilityId", v.clone()),
        Selector::TextContains(v) => ("textContains", v.clone()),
        Selector::Text(v) => ("text", v.clone()),
        other => ("text", other.to_string()),
    };
    let mut m = Mapping::new();
    m.insert(Value::from(k), Value::from(v));
    Value::Mapping(m)
}

fn step(action: &str, args: Value) -> Value {
    let mut m = Mapping::new();
    m.insert(Value::from(action), args);
    Value::Mapping(m)
}

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(Value::from(*k), v.clone());
    }
    Value::Mapping(m)
}

fn yaml(v: &Value) -> String {
    serde_yaml::to_string(v).expect("a Value always serializes")
}

/// `# ` before every line of `text`.
fn commented(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                "#".to_string()
            } else {
                format!("# {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The starter suite.
pub fn suite_yaml(
    app_id: &str,
    platform: Platform,
    slug: &str,
    file: &str,
    picks: &[Pick],
) -> String {
    let mut steps = vec![step("launchApp", map(&[("appId", Value::from(app_id))]))];
    for p in picks {
        steps.push(step("assertVisible", selector_value(&p.selector)));
    }
    let test = map(&[
        ("name", Value::from(test_name(slug))),
        ("tags", Value::Sequence(vec![Value::from("smoke")])),
        ("steps", Value::Sequence(steps)),
    ]);
    let doc = map(&[
        ("appId", Value::from(app_id)),
        (
            "config",
            map(&[("platform", Value::from(platform.to_string()))]),
        ),
        ("tests", Value::Sequence(vec![test])),
    ]);

    // Commented examples of what to add next, emitted the same way.
    let target = picks
        .first()
        .map(|p| selector_value(&p.selector))
        .unwrap_or_else(|| map(&[("text", Value::from("Sign in"))]));
    let field = if platform == Platform::Web {
        map(&[("css", Value::from("input[name=email]"))])
    } else {
        map(&[("id", Value::from("email"))])
    };
    let examples = Value::Sequence(vec![
        step("tap", target),
        step(
            "inputText",
            map(&[("selector", field), ("text", Value::from("${TEST_EMAIL}"))]),
        ),
        step(
            "inputText",
            map(&[
                ("selector", map(&[("id", Value::from("password"))])),
                ("text", Value::from("${TEST_PASSWORD}")),
            ]),
        ),
        step(
            "assertVisible",
            map(&[("textContains", Value::from("Welcome"))]),
        ),
    ]);
    let header = format!(
        "Starter suite written by `velocity init`. Run it: velocity run {file}\n\
         The assertions were read from {}'s first screen; `velocity inspect` shows\n\
         the live tree to pick more. Reference: `velocity schema --markdown`.\n",
        app_id
    );
    format!(
        "{SCHEMA_MODELINE}\n{}\n\n{}\n\n# Next steps for this test, e.g. (pass values with `--env TEST_EMAIL=…`):\n{}\n",
        commented(&header),
        yaml(&doc).trim_end(),
        commented(yaml(&examples).trim_end())
    )
}

/// The suite JSON Schema's file name, next to the suite.
const SCHEMA_FILE: &str = "velocity.schema.json";
const VSCODE_SETTINGS: &str = ".vscode/settings.json";

/// The modeline that points the YAML language server at the schema.
const SCHEMA_MODELINE: &str = "# yaml-language-server: $schema=./velocity.schema.json";

/// `velocity schema`'s output, for `<dir>/velocity.schema.json`.
fn schema_json() -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&velocity_core::schema::suite_schema())? + "\n")
}

/// Parse and validate generated YAML: a bug here must never reach a file.
fn check_suite(yaml: &str) -> anyhow::Result<()> {
    let suite = velocity_core::parse_suite_from_str(yaml)
        .map_err(|e| anyhow::anyhow!("velocity init generated a suite that doesn't parse: {e}"))?;
    velocity_core::validate_suite(&suite)
        .map_err(|e| anyhow::anyhow!("velocity init generated an invalid suite: {e}"))?;
    Ok(())
}

/// `velocity-impact.yaml`: nothing mapped yet (so every test still runs:
/// `impact.unmapped: run`), with `test` in a commented example.
pub fn impact_yaml(test: &str) -> String {
    // Nothing mapped yet: a test mapped to no paths would never be
    // selected, while an unmapped one always runs.
    let doc = map(&[
        ("tests", Value::Mapping(Mapping::new())),
        ("flows", Value::Mapping(Mapping::new())),
        ("runAll", Value::Sequence(vec![])),
        ("impact", map(&[("unmapped", Value::from("run"))])),
    ]);
    let example = map(&[(
        "tests",
        map(&[(
            test,
            Value::Sequence(vec![
                Value::from("app/src/main/**"),
                Value::from("app/build.gradle"),
            ]),
        )]),
    )]);
    format!(
        "{}\n{}\n{}\n",
        commented(
            "Test impact map for `velocity run --changed-since <ref>`: which source paths each\n\
             test exercises. Changed files select the tests mapped to them, plus tests that\n\
             failed or flaked recently, are new, or are unmapped (impact.unmapped: run).\n\
             Fill in path patterns, e.g.:"
        ),
        commented(yaml(&example).trim_end()),
        yaml(&doc).trim_end()
    )
}

/// A GitHub Actions workflow to copy, for the platform.
pub fn ci_example(platform: Platform, dir: &str) -> String {
    let header = "\
# Example GitHub Actions workflow written by `velocity init`.
# Copy it to .github/workflows/velocity.yml and adjust. It runs the suites in
# shards (`--shard M/N`) and uploads JUnit reports; `--report github` adds
# annotations on the PR.
";
    let (runs_on, setup, run) = match platform {
        Platform::Android => (
            "ubuntu-latest",
            "      - name: Enable KVM
        run: |
          echo 'KERNEL==\"kvm\", GROUP=\"kvm\", MODE=\"0666\", OPTIONS+=\"static_node=kvm\"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm
",
            "      - name: Run Velocity on an emulator
        uses: reactivecircus/android-emulator-runner@v2
        with:
          api-level: 34
          arch: x86_64
          script: velocity run DIR --shard ${{ matrix.shard }}/2 --report junit,github --artifacts-dir velocity-results
",
        ),
        Platform::Ios => (
            "macos-15",
            "      - name: Boot a simulator
        run: velocity devices up --ios 1
",
            "      - name: Run Velocity
        run: velocity run DIR --shard ${{ matrix.shard }}/2 --report junit,github --artifacts-dir velocity-results
",
        ),
        Platform::Web => (
            "ubuntu-latest",
            "",
            "      - name: Run Velocity (headless Chrome)
        run: velocity run DIR --shard ${{ matrix.shard }}/2 --report junit,github --artifacts-dir velocity-results
",
        ),
    };
    // The folder is the only value from outside; it is quoted for YAML.
    let dir_q = serde_yaml::to_string(&Value::from(dir)).unwrap_or_default();
    let dir_q = dir_q.trim();
    format!(
        "{header}
name: velocity

on:
  pull_request:
  push:
    branches: [main]

jobs:
  ui-tests:
    runs-on: {runs_on}
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2]
    steps:
      - uses: actions/checkout@v4
      - name: Install Velocity
        run: curl -fsSL https://raw.githubusercontent.com/omkar-dev/velocity/main/install.sh | bash
{setup}{run}      - name: Upload reports
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: velocity-results-${{{{ matrix.shard }}}}
          path: velocity-results/
",
        run = run.replace("DIR", dir_q),
    )
}

enum GitIgnore {
    Created,
    Updated,
    Unchanged,
}

/// Velocity's run artifacts in `.gitignore`, added once.
fn update_gitignore(root: &Path) -> anyhow::Result<GitIgnore> {
    const ENTRIES: &[&str] = &[
        "velocity-results/",
        ".velocity/healed-selectors.json",
        ".velocity/healer-snapshots.json",
    ];
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let have: Vec<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&str> = ENTRIES
        .iter()
        .copied()
        .filter(|e| !have.contains(e) && !have.contains(&e.trim_end_matches('/')))
        .collect();
    if missing.is_empty() {
        return Ok(GitIgnore::Unchanged);
    }
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# Velocity: run reports, traces, screenshots, history; local heal state\n");
    for m in missing {
        text.push_str(m);
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    Ok(if existing.is_empty() {
        GitIgnore::Created
    } else {
        GitIgnore::Updated
    })
}

enum VsCode {
    Created,
    Updated,
    Unchanged,
    /// Not touched, and why.
    Skipped(String),
}

/// The `yaml.schemas` key and glob for `dir`, relative to `root` with `/`
/// (VS Code resolves them from the workspace folder); `None` when `dir` is
/// outside `root`.
fn vscode_schema_key(root: &Path, dir: &Path) -> Option<(String, String)> {
    let rel = dir.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let prefix: String = parts.iter().map(|p| format!("{p}/")).collect();
    Some((
        format!("./{prefix}{SCHEMA_FILE}"),
        format!("{prefix}**/*.yaml"),
    ))
}

/// The entry to add by hand, as JSON.
fn vscode_entry(root: &Path, dir: &Path) -> Option<String> {
    let (key, glob) = vscode_schema_key(root, dir)?;
    Some(serde_json::json!({ "yaml.schemas": { key: [glob] } }).to_string())
}

/// Map `dir`'s YAML to its schema in `root/.vscode/settings.json` (for the
/// Red Hat YAML extension). Creates the file, or merges into one that is
/// plain JSON; never touches a file with comments (JSONC) or that doesn't
/// parse, nor an entry for the schema the user already has.
fn update_vscode_settings(root: &Path, dir: &Path) -> anyhow::Result<VsCode> {
    let Some((key, glob)) = vscode_schema_key(root, dir) else {
        return Ok(VsCode::Skipped(format!(
            "{} is outside this folder",
            dir.display()
        )));
    };
    let path = root.join(VSCODE_SETTINGS);
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let entry = serde_json::json!([glob]);
    let Some(text) = existing.filter(|t| !t.trim().is_empty()) else {
        let doc = serde_json::json!({ "yaml.schemas": { key.clone(): entry } });
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&doc)? + "\n")?;
        return Ok(VsCode::Created);
    };
    let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(VsCode::Skipped(
            "it has comments or isn't plain JSON".into(),
        ));
    };
    let Some(settings) = doc.as_object_mut() else {
        return Ok(VsCode::Skipped("it isn't a JSON object".into()));
    };
    let new_text = match settings.get_mut("yaml.schemas") {
        None => {
            settings.insert(
                "yaml.schemas".into(),
                serde_json::json!({ key.clone(): entry }),
            );
            // Append before the closing brace so the user's keys keep their
            // order and formatting.
            let body = text.trim_end();
            let body = body.strip_suffix('}').unwrap_or(body).trim_end();
            let comma = if body.ends_with('{') { "" } else { "," };
            let schemas = serde_json::to_string_pretty(&serde_json::json!({ key: [glob] }))?
                .replace('\n', "\n  ");
            format!("{body}{comma}\n  \"yaml.schemas\": {schemas}\n}}\n")
        }
        Some(serde_json::Value::Object(schemas)) => {
            if schemas.contains_key(&key) {
                return Ok(VsCode::Unchanged);
            }
            schemas.insert(key, entry);
            // Rewritten whole: no key is lost, but serde_json sorts them.
            serde_json::to_string_pretty(&doc)? + "\n"
        }
        Some(_) => return Ok(VsCode::Skipped("its yaml.schemas isn't an object".into())),
    };
    // The merge must read back as exactly the old settings plus the entry.
    let reread: serde_json::Value = serde_json::from_str(&new_text)?;
    anyhow::ensure!(
        reread == doc,
        "merging into {VSCODE_SETTINGS} would change other settings"
    );
    std::fs::write(&path, new_text)?;
    Ok(VsCode::Updated)
}

fn write_file(
    path: &Path,
    text: &str,
    cwd: &Path,
    created: &mut Vec<String>,
) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, text)?;
    created.push(shown(path, cwd));
    Ok(())
}

fn shown(p: &Path, cwd: &Path) -> String {
    p.strip_prefix(cwd).unwrap_or(p).display().to_string()
}

// ── Prompts ─────────────────────────────────────────────────────────────

struct Prompt {
    interactive: bool,
}

impl Prompt {
    fn line(&mut self) -> anyhow::Result<String> {
        let mut s = String::new();
        std::io::stdin().lock().read_line(&mut s)?;
        Ok(s.trim().to_string())
    }

    fn ask(&mut self, q: &str, default: &str) -> anyhow::Result<String> {
        if !self.interactive {
            return Ok(default.to_string());
        }
        if default.is_empty() {
            print!("{} {q}: ", "?".cyan().bold());
        } else {
            print!("{} {q} [{default}]: ", "?".cyan().bold());
        }
        std::io::stdout().flush()?;
        let a = self.line()?;
        Ok(if a.is_empty() { default.to_string() } else { a })
    }

    fn confirm(&mut self, q: &str, default: bool) -> anyhow::Result<bool> {
        if !self.interactive {
            return Ok(default);
        }
        print!(
            "{} {q} [{}]: ",
            "?".cyan().bold(),
            if default { "Y/n" } else { "y/N" }
        );
        std::io::stdout().flush()?;
        let a = self.line()?.to_ascii_lowercase();
        Ok(match a.as_str() {
            "" => default,
            _ => a.starts_with('y'),
        })
    }

    fn choose(&mut self, q: &str, options: &[String], default: usize) -> anyhow::Result<usize> {
        if !self.interactive {
            return Ok(default);
        }
        println!("{} {q}", "?".cyan().bold());
        for (i, o) in options.iter().enumerate() {
            println!("  {:>2}) {o}", i + 1);
        }
        loop {
            print!("  choose 1-{} [{}]: ", options.len(), default + 1);
            std::io::stdout().flush()?;
            let a = self.line()?;
            if a.is_empty() {
                return Ok(default);
            }
            match a.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => println!("  a number from 1 to {}", options.len()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: &str, text: Option<&str>, y: i32, ty: &str) -> Element {
        Element {
            platform_id: id.into(),
            label: None,
            text: text.map(String::from),
            element_type: ty.into(),
            bounds: Rect {
                x: 0,
                y,
                width: 100,
                height: 40,
            },
            enabled: true,
            visible: true,
            children: vec![],
            hint: None,
            focused: None,
            obscured: false,
            checked: None,
            visible_bounds: None,
            selected: None,
            role: None,
        }
    }

    fn settings_tree() -> Element {
        let mut root = el("hierarchy", None, 0, "FrameLayout");
        root.bounds = Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        };
        root.children = vec![
            el("android:id/content", None, 0, "FrameLayout"),
            el(
                "com.android.systemui:id/clock",
                Some("12:45"),
                0,
                "TextView",
            ),
            el(
                "com.android.settings:id/search_action_bar",
                None,
                100,
                "Toolbar",
            ),
            el(
                "com.android.settings:id/search_action_bar_title",
                Some("Search settings"),
                110,
                "TextView",
            ),
            el(
                "android:id/title",
                Some("Network & internet"),
                300,
                "TextView",
            ),
            el(
                "android:id/summary",
                Some("Mobile, Wi‑Fi, hotspot"),
                340,
                "TextView",
            ),
            el(
                "android:id/title",
                Some("Connected devices"),
                500,
                "TextView",
            ),
            el("android:id/title", Some("Battery"), 700, "TextView"),
            el("android:id/summary", Some("82%"), 740, "TextView"),
            el(
                "com.android.settings:id/row_12345",
                Some("Storage"),
                900,
                "TextView",
            ),
            el("", Some("Search settings"), 2000, "TextView"),
        ];
        root
    }

    #[test]
    fn picks_an_app_id_first_then_a_short_unique_text() {
        let tree = settings_tree();
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        };
        let picks = pick_assertions(
            &tree,
            Some(&screen),
            Platform::Android,
            "com.android.settings",
        );
        assert_eq!(
            picks,
            vec![
                Pick {
                    selector: Selector::Id("search_action_bar".into())
                },
                Pick {
                    selector: Selector::Text("Network & internet".into())
                },
            ]
        );
    }

    #[test]
    fn volatile_and_ambiguous_texts_are_never_picked() {
        assert!(!stable_text("12:45"));
        assert!(!stable_text("82%"));
        assert!(!stable_text("3 new messages"));
        assert!(!stable_text("A very long sentence that goes on and on"));
        assert!(stable_text("Network & internet"));
        // Only duplicated texts and digits: nothing stable to assert.
        let mut root = settings_tree();
        root.children = vec![
            el("", Some("Same"), 10, "TextView"),
            el("", Some("Same"), 20, "TextView"),
            el("", Some("Today 10:42"), 30, "TextView"),
        ];
        assert!(pick_assertions(&root, None, Platform::Android, "x").is_empty());
    }

    #[test]
    fn ios_label_ids_must_read_stable_and_offscreen_elements_are_skipped() {
        let mut root = el("ios_hierarchy", None, 0, "Application");
        root.bounds = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 800,
        };
        let mut general = el("General", None, 200, "Cell");
        general.label = Some("General".into());
        let mut time = el("9:41", None, 0, "StaticText");
        time.label = Some("9:41".into());
        let mut below = el("Privacy", None, 5000, "Cell");
        below.label = Some("Privacy".into());
        root.children = vec![time, general, below];
        let screen = root.bounds;
        let picks = pick_assertions(&root, Some(&screen), Platform::Ios, "com.apple.Preferences");
        assert_eq!(
            picks,
            vec![Pick {
                selector: Selector::Id("General".into())
            }]
        );
    }

    fn golden(name: &str, actual: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/init")
            .join(name);
        if std::env::var_os("VELOCITY_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, actual).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} missing: run with VELOCITY_BLESS=1", path.display()));
        assert_eq!(actual, want, "{name} changed: VELOCITY_BLESS=1 to accept");
    }

    #[test]
    fn generated_files_match_their_goldens_and_parse() {
        let picks = vec![
            Pick {
                selector: Selector::Id("search_action_bar".into()),
            },
            Pick {
                selector: Selector::Text("Network & internet".into()),
            },
        ];
        let suite = suite_yaml(
            "com.android.settings",
            Platform::Android,
            "settings",
            "velocity/settings.yaml",
            &picks,
        );
        check_suite(&suite).unwrap();
        golden("settings.yaml", &suite);

        let ios = suite_yaml(
            "com.apple.Preferences",
            Platform::Ios,
            "preferences",
            "velocity/preferences.yaml",
            &[Pick {
                selector: Selector::Id("General".into()),
            }],
        );
        check_suite(&ios).unwrap();
        golden("preferences.yaml", &ios);

        // A value YAML would misread is quoted by the emitter, not spliced.
        let tricky = suite_yaml(
            "com.example.app",
            Platform::Android,
            "app",
            "app.yaml",
            &[Pick {
                selector: Selector::Text("yes: \"no\" # really".into()),
            }],
        );
        let parsed = velocity_core::parse_suite_from_str(&tricky).unwrap();
        match &parsed.tests[0].steps[1].action {
            velocity_common::Action::AssertVisible { selector } => {
                assert_eq!(selector, &Selector::Text("yes: \"no\" # really".into()))
            }
            other => panic!("{other:?}"),
        }

        let impact = impact_yaml("settings opens on its first screen");
        let m: velocity_core::impact::ImpactMapping = serde_yaml::from_str(&impact).unwrap();
        assert!(
            m.tests.is_empty(),
            "an empty mapping would deselect the test"
        );
        assert!(impact.contains("#   settings opens on its first screen:"));
        golden("velocity-impact.yaml", &impact);

        let ci = ci_example(Platform::Android, "velocity");
        let _: Value = serde_yaml::from_str(&ci).expect("the CI example is YAML");
        assert!(ci
            .contains("velocity run velocity --shard ${{ matrix.shard }}/2 --report junit,github"));
        golden("github-actions.android.yml", &ci);
        for p in [Platform::Ios, Platform::Web] {
            let _: Value = serde_yaml::from_str(&ci_example(p, "my tests")).unwrap();
        }
    }

    #[test]
    fn foreground_apps_and_slugs() {
        assert_eq!(
            android_foreground(
                "  topResumedActivity=ActivityRecord{9e9 u0 com.android.settings/.Settings t12}"
            ),
            Some("com.android.settings".into())
        );
        assert_eq!(
            android_foreground("  topResumedActivity=ActivityRecord{9e9 u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity t1}"),
            None
        );
        assert_eq!(
            app_slug("com.android.settings", Platform::Android),
            "settings"
        );
        assert_eq!(
            app_slug("com.apple.Preferences", Platform::Ios),
            "preferences"
        );
        assert_eq!(
            app_slug("https://www.example.com:8080/a?b", Platform::Web),
            "example-com"
        );
        assert_eq!(app_slug("data:text/html,x", Platform::Web), "web");
    }

    #[test]
    fn ios_user_apps_come_from_the_listapps_plist() {
        let plist = "{\n    \"com.apple.Bridge\" =     {\n        ApplicationType = System;\n        CFBundleDisplayName = Watch;\n    };\n    \"com.example.shop\" =     {\n        ApplicationType = User;\n        CFBundleDisplayName = \"My Shop\";\n        GroupContainers =         {\n        };\n    };\n    \"dev.velocity.runner.xctrunner\" =     {\n        ApplicationType = User;\n    };\n}\n";
        assert_eq!(
            ios_user_apps(plist),
            vec![("com.example.shop".to_string(), Some("My Shop".to_string()))]
        );
    }

    #[test]
    fn the_suite_points_at_the_schema_written_next_to_it() {
        let suite = suite_yaml(
            "com.example.app",
            Platform::Android,
            "app",
            "velocity/app.yaml",
            &[],
        );
        assert_eq!(suite.lines().next(), Some(SCHEMA_MODELINE));
        check_suite(&suite).unwrap();
        let schema: serde_json::Value = serde_json::from_str(&schema_json().unwrap()).unwrap();
        assert_eq!(schema, velocity_core::schema::suite_schema());
    }

    fn vscode(root: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root.join(VSCODE_SETTINGS)).unwrap()).unwrap()
    }

    #[test]
    fn vscode_settings_are_created_when_absent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("velocity");
        assert!(matches!(
            update_vscode_settings(root.path(), &dir).unwrap(),
            VsCode::Created
        ));
        assert_eq!(
            vscode(root.path()),
            serde_json::json!({ "yaml.schemas": {
                "./velocity/velocity.schema.json": ["velocity/**/*.yaml"]
            }})
        );
        // Twice: nothing to add.
        assert!(matches!(
            update_vscode_settings(root.path(), &dir).unwrap(),
            VsCode::Unchanged
        ));
        // A nested folder, and the current one.
        assert_eq!(
            vscode_schema_key(root.path(), &root.path().join("e2e/./ui")),
            Some((
                "./e2e/ui/velocity.schema.json".into(),
                "e2e/ui/**/*.yaml".into()
            ))
        );
        assert_eq!(
            vscode_schema_key(root.path(), root.path()),
            Some(("./velocity.schema.json".into(), "**/*.yaml".into()))
        );
        assert!(matches!(
            update_vscode_settings(root.path(), Path::new("/elsewhere/velocity")).unwrap(),
            VsCode::Skipped(_)
        ));
    }

    #[test]
    fn vscode_settings_are_merged_keeping_every_key() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("velocity");
        std::fs::create_dir_all(root.path().join(".vscode")).unwrap();
        let path = root.path().join(VSCODE_SETTINGS);

        // No yaml.schemas: appended, the user's text kept as it was.
        let user = "{\n    \"editor.tabSize\": 4,\n    \"a.b\": [1, 2]\n}\n";
        std::fs::write(&path, user).unwrap();
        assert!(matches!(
            update_vscode_settings(root.path(), &dir).unwrap(),
            VsCode::Updated
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n    \"editor.tabSize\": 4,\n    \"a.b\": [1, 2],\n"),
            "{text}"
        );
        assert_eq!(
            vscode(root.path()),
            serde_json::json!({
                "editor.tabSize": 4,
                "a.b": [1, 2],
                "yaml.schemas": { "./velocity/velocity.schema.json": ["velocity/**/*.yaml"] }
            })
        );

        // An empty object.
        std::fs::write(&path, "{}").unwrap();
        update_vscode_settings(root.path(), &dir).unwrap();
        assert_eq!(
            vscode(root.path()),
            serde_json::json!({ "yaml.schemas": {
                "./velocity/velocity.schema.json": ["velocity/**/*.yaml"]
            }})
        );

        // Other schemas already mapped: ours is added beside them.
        std::fs::write(
            &path,
            r#"{"yaml.schemas": {"other.json": "ci/*.yml"}, "x": true}"#,
        )
        .unwrap();
        assert!(matches!(
            update_vscode_settings(root.path(), &dir).unwrap(),
            VsCode::Updated
        ));
        assert_eq!(
            vscode(root.path()),
            serde_json::json!({
                "x": true,
                "yaml.schemas": {
                    "other.json": "ci/*.yml",
                    "./velocity/velocity.schema.json": ["velocity/**/*.yaml"]
                }
            })
        );

        // The user's own mapping for our schema is never replaced.
        let mine = r#"{"yaml.schemas": {"./velocity/velocity.schema.json": "tests/*.yaml"}}"#;
        std::fs::write(&path, mine).unwrap();
        assert!(matches!(
            update_vscode_settings(root.path(), &dir).unwrap(),
            VsCode::Unchanged
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
    }

    #[test]
    fn jsonc_or_odd_vscode_settings_are_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("velocity");
        std::fs::create_dir_all(root.path().join(".vscode")).unwrap();
        let path = root.path().join(VSCODE_SETTINGS);
        for text in [
            "{\n  // my settings\n  \"editor.tabSize\": 2\n}\n",
            "{\"editor.tabSize\": 2,}",
            "{ not json",
            "[1, 2]",
            r#"{"yaml.schemas": ["x"]}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(
                matches!(
                    update_vscode_settings(root.path(), &dir).unwrap(),
                    VsCode::Skipped(_)
                ),
                "{text}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        assert_eq!(
            vscode_entry(root.path(), &dir).unwrap(),
            r#"{"yaml.schemas":{"./velocity/velocity.schema.json":["velocity/**/*.yaml"]}}"#
        );
    }

    #[test]
    fn gitignore_entries_are_added_once() {
        let dir = std::env::temp_dir().join(format!("vel-init-gi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/").unwrap();
        assert!(matches!(
            update_gitignore(&dir).unwrap(),
            GitIgnore::Updated
        ));
        assert!(matches!(
            update_gitignore(&dir).unwrap(),
            GitIgnore::Unchanged
        ));
        let text = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(text.starts_with("target/\n#"), "{text}");
        assert_eq!(text.matches("velocity-results/").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
