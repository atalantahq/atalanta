//! `velocity explore "<goal>"`: an AI agent works through the goal on a
//! device once and writes a deterministic YAML test with assertions, which
//! is then replayed (no model calls) to prove it holds.
//!
//! Exit codes: 0 the goal was reached, the test written and every replay
//! passed; 1 it wasn't (gave up, stuck, unverified, out of budget, a replay
//! failed even after the repair); 2 configuration; 3 device; 4 `--timeout`.

use std::io::{BufRead, Seek, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use velocity_common::{Platform, PlatformDriver, ResilientDriver, VerifyLevel};
use velocity_explore::scripted::{Script, ScriptedChecker, ScriptedPlanner};
use velocity_explore::{
    ExploreEvent, ExploreJob, ExploreOptions, InstructionSource, JobResult, LaunchMode, Outcome,
    ScreenshotMode,
};
use velocity_nl::{AiSettings, NaturalPlanner, ScreenChecker};

#[derive(Args, Debug)]
pub struct ExploreArgs {
    /// What to do, in plain English ("open Network & internet"). Quoted
    /// text in it ("search for \"blue shoes\"") may be typed as is
    goal: String,

    /// The app under test (bundle id / package)
    #[arg(long = "app-id", alias = "app")]
    app_id: String,

    /// ios, android or web (default: from the device id)
    #[arg(short, long)]
    platform: Option<String>,

    /// Device to use (default: the first booted one no other run holds)
    #[arg(short, long)]
    device: Option<String>,

    /// Goal model as provider:model (default: the configured provider's
    /// goal model, see docs/AI-PROVIDERS.md)
    #[arg(long)]
    model: Option<String>,

    /// Play a fixed plan instead of asking a model (no API key needed):
    /// see docs/EXPLORE.md
    #[arg(long, value_name = "PLAN.json", conflicts_with = "model")]
    script: Option<PathBuf>,

    /// Where to write the test (default: <goal as a file name>.yaml)
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Overwrite --out if it exists
    #[arg(long)]
    force: bool,

    /// The test's name (default: the goal)
    #[arg(long)]
    name: Option<String>,

    /// A value the agent may type, by name: written as ${NAME} in the test
    #[arg(long = "data", value_name = "NAME=VALUE")]
    data: Vec<String>,

    /// A secret the agent may type, read from $NAME: never sent to a model,
    /// shown, or written (the test types ${NAME})
    #[arg(long = "secret", value_name = "NAME")]
    secrets: Vec<String>,

    /// Planning turns at most
    #[arg(long, default_value_t = 40)]
    max_steps: usize,

    /// Seconds the exploration may take
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// Stop when the estimated spend reaches this many dollars (needs --price)
    #[arg(long, value_name = "USD")]
    max_cost: Option<f64>,

    /// Stop after this many model tokens (input + output)
    #[arg(long)]
    max_tokens: Option<u64>,

    /// Price for the cost estimate: dollars per million input,output tokens
    #[arg(long, value_name = "IN,OUT")]
    price: Option<String>,

    /// AI check of the goal: off, final (default), checkpoints or strict
    #[arg(long, default_value = "final")]
    verify: String,

    /// Replays of the written test (no model calls); 0 skips the check
    #[arg(long, default_value_t = 1)]
    replay: usize,

    /// Don't try a repair round when a replay fails
    #[arg(long)]
    no_repair: bool,

    /// Start with clearState: true (the test does the same)
    #[arg(long, conflicts_with = "no_launch")]
    clear_state: bool,

    /// Start from the screen as it is (the test has no launchApp)
    #[arg(long)]
    no_launch: bool,

    /// Also write <out>.nl.yaml: the steps in plain English
    #[arg(long)]
    keep_nl: bool,

    /// Explore and report only: no test is written or replayed
    #[arg(long)]
    dry_run: bool,

    /// Print the result as JSON on stdout (live lines go to stderr)
    #[arg(long)]
    json: bool,

    /// Read instructions for the agent from lines appended to this file
    /// while it runs
    #[arg(long, value_name = "PATH")]
    inject_file: Option<PathBuf>,

    /// Seconds to wait for a device another Velocity process holds
    #[arg(long, default_value_t = 600)]
    lease_wait: u64,

    /// Benchmark run (e.g. AndroidWorld): start from the screen as it is,
    /// write and replay no test, no auto-dismissed dialogs, `launch` off
    /// (use --open-app / --apps-file to let the agent open apps), screenshots every turn when
    /// the provider takes images. An outside harness judges the result;
    /// the trajectory goes to $VELOCITY_EVENTS_FILE. See docs/BENCHMARKS.md
    #[arg(long)]
    benchmark: bool,

    /// Let the model propose text to type itself: accepted only when it
    /// occurs in the goal or on a screen seen in this run (or is a number,
    /// or such pieces joined by lines or commas). See docs/EXPLORE.md
    #[arg(long)]
    values_from_goal: bool,

    /// An app the agent may open by name (openApp): NAME=APP_ID, repeatable
    #[arg(long = "open-app", value_name = "NAME=APP_ID")]
    apps: Vec<String>,

    /// A JSON object {"Name": "app.id", …} of apps the agent may open
    #[arg(long, value_name = "PATH")]
    apps_file: Option<PathBuf>,

    /// When a screenshot goes with the screen list: never, poor (only when
    /// the tree says little; the default) or always (default with --benchmark)
    #[arg(long)]
    screenshots: Option<String>,

    /// Don't show the model the project's tests most like the goal (by
    /// default the one or two closest are shown as examples; never with
    /// --benchmark)
    #[arg(long)]
    no_demos: bool,

    /// A folder of suites to take those examples from (repeatable; default:
    /// the folder --out writes to, else the current one)
    #[arg(long = "demos-from", value_name = "DIR")]
    demos_from: Vec<PathBuf>,
}

/// The planner, the checker, whether to send screenshots, and a label.
type Agent = (
    Arc<dyn NaturalPlanner>,
    Option<Arc<dyn ScreenChecker>>,
    bool,
    String,
);

/// Instructions appended to a file while the session runs.
struct FileSource {
    path: PathBuf,
    offset: u64,
}

impl InstructionSource for FileSource {
    fn drain(&mut self) -> Vec<String> {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return vec![];
        };
        if f.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let mut out = Vec::new();
        let mut reader = std::io::BufReader::new(f);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if !line.ends_with('\n') {
                break; // a line still being written
            }
            self.offset += line.len() as u64;
            let t = line.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
            line.clear();
        }
        out
    }
}

/// `{"Contacts": "com.google.android.contacts", …}`, sorted by name.
fn parse_apps_file(text: &str) -> Result<Vec<(String, String)>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let obj = v
        .as_object()
        .ok_or("must be a JSON object of name: app id")?;
    obj.iter()
        .map(|(k, v)| match v.as_str() {
            Some(id) if !id.trim().is_empty() && !k.trim().is_empty() => {
                Ok((k.trim().to_string(), id.trim().to_string()))
            }
            _ => Err(format!("{k:?}: the app id must be a non-empty string")),
        })
        .collect()
}

fn parse_price(s: &str) -> Result<(f64, f64), String> {
    let bad = || format!("--price {s:?} must be IN,OUT (dollars per million tokens)");
    let (i, o) = s.split_once(',').ok_or_else(bad)?;
    let num = |v: &str| {
        v.trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    Ok((num(i).ok_or_else(bad)?, num(o).ok_or_else(bad)?))
}

/// A value name usable as `${NAME}`.
fn check_name(name: &str, flag: &str) -> anyhow::Result<()> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        usage_bail!("{flag} {name:?}: a name is letters, digits and _ (it becomes ${{{name}}})");
    }
    Ok(())
}

/// The goal as a file name: `open Network & internet` → `open-network-internet.yaml`.
fn default_out(goal: &str) -> PathBuf {
    let mut slug = String::new();
    for c in goal.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    PathBuf::from(format!(
        "{}.yaml",
        if slug.is_empty() { "explore" } else { slug }
    ))
}

/// Live output: one line per turn.
fn printer(to_stderr: bool) -> velocity_explore::EventSink {
    let mut planned: Option<(u64, u64, u64)> = None;
    Box::new(move |e: &ExploreEvent| {
        let line = match e {
            ExploreEvent::Prefix {
                step,
                action,
                ok,
                ms,
                ..
            } => Some(format!(
                "  {} {action} {} ms{}",
                format!("·{step}").dimmed(),
                ms,
                if *ok { "" } else { " FAILED" }
            )),
            ExploreEvent::Planned {
                model_ms,
                input_tokens,
                output_tokens,
                ..
            } => {
                planned = Some((*model_ms, *input_tokens, *output_tokens));
                None
            }
            ExploreEvent::Acted {
                turn,
                action,
                ok,
                ms,
                changed,
                error,
                ..
            } => {
                let model = planned
                    .take()
                    .filter(|(ms, i, o)| ms + i + o > 0)
                    .map(|(ms, i, o)| format!("  model {:.1} s, {i}/{o} tok", ms as f64 / 1000.0))
                    .unwrap_or_default();
                Some(format!(
                    "  {} {action}  {ms} ms{}{}{}",
                    format!("#{turn}").cyan(),
                    if *changed { "" } else { "  (no change)" },
                    model.dimmed(),
                    match (ok, error) {
                        (false, Some(e)) => format!("\n      {} {e}", "failed:".red()),
                        (false, None) => format!(" {}", "failed".red()),
                        _ => String::new(),
                    }
                ))
            }
            ExploreEvent::Asserted {
                turn,
                assertion,
                ok,
                ..
            } => Some(format!(
                "  {} {assertion}  {}",
                format!("#{turn}").cyan(),
                if *ok { "holds".green() } else { "failed".red() }
            )),
            ExploreEvent::Rejected { turn, why, .. } => {
                planned = None;
                Some(format!(
                    "  {} {} {why}",
                    format!("#{turn}").cyan(),
                    "rejected:".yellow()
                ))
            }
            ExploreEvent::Injected { instruction, .. } => {
                Some(format!("  {} {instruction}", "instruction:".magenta()))
            }
            ExploreEvent::Note { note, .. } => Some(format!("  {} {note}", "note:".yellow())),
            ExploreEvent::Checked {
                outcome, reason, ..
            } => Some(format!("  {} {outcome}: {reason}", "check".bold())),
            ExploreEvent::ReplayRun {
                run,
                passed,
                ms,
                failed_step,
                error,
                ..
            } => Some(format!(
                "  {} {} in {:.1} s{}",
                format!("replay {run}").bold(),
                if *passed {
                    "passed".green()
                } else {
                    "failed".red()
                },
                *ms as f64 / 1000.0,
                match (failed_step, error) {
                    (Some(k), Some(e)) => format!(" at step {}: {e}", k + 1),
                    _ => String::new(),
                }
            )),
            ExploreEvent::Repair { action, reason, .. } => Some(format!(
                "  {} {action} ({reason})",
                "repair:".yellow().bold()
            )),
            ExploreEvent::Answered {
                turn,
                answer,
                sources,
                ..
            } => Some(format!(
                "  {} answer {answer:?}  {} ({})",
                format!("#{turn}").cyan(),
                "accepted".green(),
                sources.join(", ")
            )),
            ExploreEvent::Observed { .. } | ExploreEvent::Finished { .. } => None,
        };
        if let Some(l) = line {
            if to_stderr {
                eprintln!("{l}");
            } else {
                println!("{l}");
            }
        }
    })
}

async fn pick_device(driver: &dyn PlatformDriver) -> anyhow::Result<String> {
    let mut devices = driver.list_devices().await?;
    devices.sort_by_key(|d| d.state != velocity_common::DeviceState::Booted);
    devices
        .iter()
        .find(|d| {
            d.state == velocity_common::DeviceState::Booted
                && velocity_runner::lease::try_lock(&d.id).is_ok()
        })
        .or(devices.first())
        .map(|d| d.id.clone())
        .ok_or_else(|| {
            super::no_device("no device to explore on; boot one or pass --device", vec![])
        })
}

/// The device's lease, waiting up to `wait` for another holder.
async fn lease(
    device: &str,
    wait: Duration,
    quiet: bool,
) -> anyhow::Result<velocity_runner::lease::DeviceLock> {
    let started = std::time::Instant::now();
    let mut told = false;
    loop {
        match velocity_runner::lease::try_lock(device) {
            Ok(l) => return Ok(l),
            Err(velocity_runner::lease::LeaseError::Held(holder)) => {
                if started.elapsed() >= wait {
                    return Err(super::no_device(
                        &format!("device {device} is still held by {holder} after {wait:?}"),
                        vec![],
                    ));
                }
                if !told && !quiet {
                    eprintln!("=> device {device} is in use ({holder}); waiting for it");
                    told = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(super::no_device(&e.to_string(), vec![])),
        }
    }
}

pub async fn execute(args: ExploreArgs) -> anyhow::Result<i32> {
    let verify: VerifyLevel = args
        .verify
        .parse()
        .map_err(|e: String| usage_err!("--verify: {e}"))?;
    let price = args
        .price
        .as_deref()
        .map(parse_price)
        .transpose()
        .map_err(|e| usage_err!("{e}"))?;
    if args.max_cost.is_some() && price.is_none() {
        usage_bail!(
            "--max-cost needs --price IN,OUT (dollars per million tokens) to estimate the spend"
        );
    }
    let mut data = Vec::new();
    for d in &args.data {
        let Some((k, v)) = d.split_once('=') else {
            usage_bail!("--data {d:?} must be NAME=VALUE");
        };
        check_name(k, "--data")?;
        data.push((k.to_string(), v.to_string()));
    }
    let mut secrets = Vec::new();
    for name in &args.secrets {
        check_name(name, "--secret")?;
        let value = velocity_common::env::var(name)
            .map_err(|_| usage_err!("--secret {name}: ${name} is not set"))?;
        velocity_common::redact::register_secret(&value);
        secrets.push((name.clone(), value));
    }
    let mut apps: Vec<(String, String)> = Vec::new();
    if let Some(path) = &args.apps_file {
        let text = std::fs::read_to_string(path)
            .map_err(|e| usage_err!("--apps-file {}: {e}", path.display()))?;
        apps = parse_apps_file(&text)
            .map_err(|e| usage_err!("--apps-file {}: {e}", path.display()))?;
    }
    for a in &args.apps {
        let Some((name, id)) = a.split_once('=') else {
            usage_bail!("--open-app {a:?} must be NAME=APP_ID");
        };
        if name.trim().is_empty() || id.trim().is_empty() {
            usage_bail!("--open-app {a:?} must be NAME=APP_ID");
        }
        apps.push((name.trim().to_string(), id.trim().to_string()));
    }
    let screenshots: Option<ScreenshotMode> = args
        .screenshots
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|e: String| usage_err!("--screenshots {e}"))?;
    // A benchmark run writes nothing: it is judged by its harness.
    let dry_run = args.dry_run || args.benchmark;
    let out = args.out.clone().unwrap_or_else(|| default_out(&args.goal));
    if !dry_run && out.exists() && !args.force {
        usage_bail!(
            "{} exists; pass --force to overwrite it or --out another file",
            out.display()
        );
    }

    // The agent: a model, or a script.
    let (planner, checker, images, model_label): Agent = match &args.script {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| usage_err!("--script {}: {e}", path.display()))?;
            let script = Script::from_json(&text)?;
            let checker = script
                .check
                .clone()
                .map(|c| Arc::new(ScriptedChecker::new(c)) as Arc<dyn ScreenChecker>);
            (
                Arc::new(ScriptedPlanner::new(&script)),
                checker,
                false,
                format!("scripted plan {}", path.display()),
            )
        }
        None => {
            let settings = match &args.model {
                Some(spec) => {
                    let mut s = AiSettings::for_spec(spec)?;
                    s.explore_model = s.model.clone();
                    s
                }
                None => AiSettings::from_env()?.ok_or_else(|| {
                    usage_err!(
                        "velocity explore needs a model: set a provider's key (e.g. ANTHROPIC_API_KEY), \
                         or pass --model provider:model; see docs/AI-PROVIDERS.md (or --script for a fixed plan)"
                    )
                })?,
            };
            let checker: Option<Arc<dyn ScreenChecker>> = if verify == VerifyLevel::Off {
                None
            } else {
                Some(Arc::from(settings.checker()?))
            };
            let label = format!("{}:{}", settings.preset.name, settings.explore_model);
            (
                Arc::from(settings.explore_planner()?),
                checker,
                settings.images && settings.preset.caps.images,
                label,
            )
        }
    };
    if verify != VerifyLevel::Off && checker.is_none() {
        usage_bail!(
            "--verify {verify} needs a checker: the script has no \"check\" (or pass --verify off)"
        );
    }

    // The device.
    let platform = match args.platform.as_deref() {
        Some("ios") => Some(Platform::Ios),
        Some("android") => Some(Platform::Android),
        Some("web") => Some(Platform::Web),
        Some(other) => usage_bail!("unknown platform {other} (ios, android or web)"),
        None => None,
    };
    let platform = platform
        .or(args.device.as_deref().map(Platform::of_device_id))
        .unwrap_or(Platform::Android);
    let raw: Arc<dyn PlatformDriver> = Arc::from(super::create_driver(platform));
    let driver: Arc<dyn PlatformDriver> = Arc::new(ResilientDriver::new(raw));
    let device = match &args.device {
        Some(d) => d.clone(),
        None if platform == Platform::Web => "web".to_string(),
        None => pick_device(&*driver).await?,
    };
    let _lease = lease(&device, Duration::from_secs(args.lease_wait), args.json).await?;
    driver.prepare(&device).await?;

    let mut opts = ExploreOptions::new(&args.goal, &args.app_id, &device, platform);
    opts.launch = if args.no_launch || (args.benchmark && !args.clear_state) {
        LaunchMode::Attach
    } else if args.clear_state {
        LaunchMode::Clean
    } else {
        LaunchMode::Launch
    };
    opts.max_steps = args.max_steps.max(1);
    opts.timeout = Duration::from_secs(args.timeout.max(1));
    opts.max_cost_usd = args.max_cost;
    opts.max_tokens = args.max_tokens;
    opts.price = price;
    opts.verify = verify;
    opts.data = data;
    opts.secrets = secrets;
    opts.images = images;
    opts.benchmark = args.benchmark;
    opts.values_from_goal = args.values_from_goal;
    opts.apps = apps;
    opts.demos = !args.no_demos;
    // The project's own tests, wherever the new one goes: not whatever
    // folder the command happens to run in.
    opts.demo_dirs = if !args.demos_from.is_empty() {
        args.demos_from.clone()
    } else {
        let dir = args
            .out
            .as_deref()
            .and_then(std::path::Path::parent)
            .filter(|p| !p.as_os_str().is_empty());
        vec![dir.map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)]
    };
    opts.screenshots = screenshots.unwrap_or(if args.benchmark {
        ScreenshotMode::Always
    } else {
        ScreenshotMode::Poor
    });
    if args.benchmark {
        // Every action is the agent's own: no dialog is tapped for it, and
        // long tasks (several items through one form) may revisit screens.
        opts.config.dialog.enabled = false;
        opts.max_visits = 20;
    }

    let say = |s: String| {
        if args.json {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    };
    say(format!(
        "{} exploring {:?} on {device} ({platform}, {}) with {model_label}",
        "=>".cyan().bold(),
        args.goal,
        args.app_id
    ));

    // Live lines, and the same events as JSON lines for a supervisor
    // (`VELOCITY_EVENTS_FILE`: the MCP server's tasks, the console).
    let mut print = printer(args.json);
    let events: velocity_explore::EventSink = Box::new(move |e: &ExploreEvent| {
        if velocity_common::live_events::enabled() {
            if let Ok(v) = serde_json::to_value(e) {
                velocity_common::live_events::emit(v);
            }
        }
        print(e);
    });
    let mut job = ExploreJob::new(driver.clone(), planner, checker, opts).with_events(events);
    // Ctrl-C (or a supervisor's stop) ends the session at its next turn as
    // `stopped`, still with a report; a second one exits at once.
    crate::signals::install();
    let seen = crate::signals::count();
    let handle = job.handle();
    let on_signal = tokio::spawn(async move {
        crate::signals::after(seen).await;
        handle.stop();
    });
    if let Some(path) = &args.inject_file {
        // Only lines appended from now on.
        let offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        job = job.with_instructions(Box::new(FileSource {
            path: path.clone(),
            offset,
        }));
    }
    job.gen.name = args.name.clone();
    job.replays = if dry_run { 0 } else { args.replay };
    job.repair = !args.no_repair && !args.benchmark;
    job.dry_run = dry_run;
    job.keep_nl = args.keep_nl;
    let result = Box::pin(job.run()).await;
    on_signal.abort();
    crate::signals::cleaning_up();
    driver.cleanup().await;
    let result = result?;

    let mut written = Vec::new();
    if let Some(yaml) = &result.yaml {
        std::fs::write(&out, yaml).map_err(|e| usage_err!("writing {}: {e}", out.display()))?;
        written.push(out.display().to_string());
    }
    if let Some(nl) = &result.nl_yaml {
        let nl_out = out.with_extension("nl.yaml");
        std::fs::write(&nl_out, nl).map_err(|e| usage_err!("writing {}: {e}", nl_out.display()))?;
        written.push(nl_out.display().to_string());
    }

    if args.json {
        let mut v = serde_json::to_value(&result)?;
        v["files"] = serde_json::json!(written);
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}", serde_json::to_string_pretty(&v)?)?;
    } else {
        print_summary(&result, &written);
    }
    if crate::signals::count() > seen {
        return Ok(crate::signals::exit_code());
    }
    Ok(exit_code(&result))
}

fn exit_code(r: &JobResult) -> i32 {
    match r.report.outcome {
        Outcome::TimedOut => 4,
        _ if r.ok() => 0,
        Outcome::Success if r.yaml.is_none() => 0, // dry run
        _ => 1,
    }
}

fn print_summary(r: &JobResult, written: &[String]) {
    let rep = &r.report;
    let outcome = if rep.outcome.is_success() {
        rep.outcome.to_string().green().bold()
    } else {
        rep.outcome.to_string().red().bold()
    };
    println!("\n{} {outcome}: {}", "=>".cyan().bold(), rep.reason);
    let cost = rep
        .cost_usd
        .map(|c| format!(", est. ${c:.4}"))
        .unwrap_or_else(|| ", cost not estimated (pass --price IN,OUT)".into());
    println!(
        "   {} turns, {} actions, {} model calls, {}/{} tokens{cost}, {:.1} s",
        rep.turns,
        rep.steps.len(),
        rep.model_calls,
        rep.input_tokens,
        rep.output_tokens,
        rep.elapsed_ms as f64 / 1000.0
    );
    println!("   {}", rep.expectations);
    for f in written {
        let assertions = r
            .generated
            .as_ref()
            .map(|g| {
                format!(
                    " ({} assertion{})",
                    g.assertions,
                    if g.assertions == 1 { "" } else { "s" }
                )
            })
            .unwrap_or_default();
        println!(
            "   wrote {f}{}",
            if f.ends_with(".nl.yaml") {
                String::new()
            } else {
                assertions
            }
        );
    }
    if let Some(rp) = &r.replay {
        let calls = rp.model_calls;
        println!(
            "   replayed {}x in {:.1} s ({} passed, {calls} model calls){}",
            rp.runs.len(),
            rp.total_ms() as f64 / 1000.0,
            rp.passed(),
            rp.runs
                .iter()
                .map(|run| format!(" [{:.1} s]", run.ms as f64 / 1000.0))
                .collect::<String>()
        );
        if let Some(last) = rp.runs.last() {
            let steps: Vec<String> = last
                .steps
                .iter()
                .map(|s| format!("{} {} ms", s.action, s.ms))
                .collect();
            println!("   last replay per step: {}", steps.join(", "));
        }
    }
    if let Some(repair) = &r.repair {
        println!("   repair: {repair}");
    }
    if let Some(f) = written.first() {
        if r.ok() {
            println!("\n   next: velocity run {f}   (commit it; CI replays it with no model)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: ExploreArgs,
    }

    #[test]
    fn flags_parse() {
        let c = Cli::try_parse_from([
            "x",
            "open Network & internet",
            "--app-id",
            "com.android.settings",
            "--device",
            "emulator-5554",
            "--data",
            "EMAIL=a@b.c",
            "--secret",
            "PASSWORD",
            "--max-steps",
            "12",
            "--verify",
            "strict",
            "--replay",
            "3",
            "--json",
        ])
        .unwrap();
        assert_eq!(c.args.max_steps, 12);
        assert_eq!(c.args.replay, 3);
        assert_eq!(c.args.secrets, vec!["PASSWORD"]);
        assert!(
            Cli::try_parse_from(["x", "g", "--app-id", "a", "--clear-state", "--no-launch"])
                .is_err()
        );
    }

    #[test]
    fn flag_names_are_unique() {
        // clap checks this only when the real command is built; a clash
        // (`--app` is `--app-id`'s alias) panicked at startup.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn benchmark_flags_parse() {
        let c = Cli::try_parse_from([
            "x",
            "Create a contact for Ana",
            "--app-id",
            "com.google.android.apps.nexuslauncher",
            "--benchmark",
            "--values-from-goal",
            "--open-app",
            "Contacts=com.google.android.contacts",
            "--screenshots",
            "never",
        ])
        .unwrap();
        assert!(c.args.benchmark && c.args.values_from_goal);
        assert_eq!(c.args.apps, vec!["Contacts=com.google.android.contacts"]);
        assert_eq!(c.args.screenshots.as_deref(), Some("never"));
        assert_eq!(
            "always".parse::<ScreenshotMode>(),
            Ok(ScreenshotMode::Always)
        );
        assert!("sometimes".parse::<ScreenshotMode>().is_err());
    }

    #[test]
    fn apps_file_parses_in_order_and_rejects_bad_ids() {
        let apps = parse_apps_file(r#"{"Simple Calendar Pro":"com.simplemobiletools.calendar.pro","Contacts":"com.google.android.contacts"}"#).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].0, "Contacts");
        assert!(parse_apps_file(r#"{"X": 3}"#).is_err());
        assert!(parse_apps_file(r#"{"X": ""}"#).is_err());
        assert!(parse_apps_file("[1]").is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(
            default_out("open Network & internet"),
            PathBuf::from("open-network-internet.yaml")
        );
        assert_eq!(default_out("!!!"), PathBuf::from("explore.yaml"));
        assert_eq!(parse_price("3,15").unwrap(), (3.0, 15.0));
        assert!(parse_price("3").is_err());
        assert!(check_name("EMAIL_2", "--data").is_ok());
        assert!(check_name("2X", "--data").is_err());
        assert!(check_name("A-B", "--data").is_err());
    }

    #[test]
    fn injected_lines_are_read_once_and_only_when_complete() {
        let dir = std::env::temp_dir().join(format!("vexplore-inject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inject.txt");
        std::fs::write(&path, "old line\n").unwrap();
        let mut src = FileSource {
            path: path.clone(),
            offset: std::fs::metadata(&path).unwrap().len(),
        };
        assert!(src.drain().is_empty());
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "use the search box\npartial").unwrap();
        assert_eq!(src.drain(), vec!["use the search box"]);
        writeln!(f, " line").unwrap();
        assert_eq!(src.drain(), vec!["partial line"]);
        assert!(src.drain().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
