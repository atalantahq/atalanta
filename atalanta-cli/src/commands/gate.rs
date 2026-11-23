//! `atalanta gate <file> --goal "<text>"`: trust-gates any one-test suite
//! YAML — hand-written or exported from an agent session — without exploring.
//! It replays the test N times (the later ones with timing jitter), replays it
//! once on the wrong path (a neighbouring tap instead of the last navigational
//! one, which must fail), and checks an assertion names text from the goal.
//! Exit 0 trusted, 1 not trusted, 2 configuration/usage, 3 device.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use atalanta_common::{Platform, PlatformDriver, ResilientDriver};
use atalanta_core::parse_suite_from_str;
use atalanta_explore::gates::{check, CheckOptions};
use clap::Args;

#[derive(Args, Debug)]
pub struct GateArgs {
    /// The suite YAML to gate: exactly one test.
    file: PathBuf,

    /// What the test is meant to show ("open Network & internet"): an
    /// assertion must name text from it.
    #[arg(long)]
    goal: String,

    /// ios, android or web (default: from the device id)
    #[arg(short, long)]
    platform: Option<String>,

    /// Device to use (default: the first booted one no other run holds)
    #[arg(short, long)]
    device: Option<String>,

    /// How long to wait for a device another Atalanta process holds. Takes a
    /// unit (30s, 10m); a bare number is seconds
    #[arg(long, default_value = "600", value_name = "DURATION", value_parser = super::duration_arg::bare_secs)]
    lease_wait: std::time::Duration,

    /// Replays of the test (no model calls; all must pass, the later ones with
    /// timing jitter); 0 is a config error
    #[arg(long, default_value_t = 5)]
    replay: usize,

    /// Upper bound of the random pause before each step, on replays after the
    /// first (ms). The same default explore burns in with.
    #[arg(long, default_value_t = atalanta_explore::gates::DEFAULT_JITTER_MS)]
    jitter_ms: u64,

    /// A value for a `${NAME}` (repeatable), NAME=VALUE
    #[arg(long = "env", value_name = "NAME=VALUE")]
    env: Vec<String>,

    /// KEY=VALUE lines for the `${NAME}`s (repeatable). --env wins over a
    /// file, a later file over an earlier one
    #[arg(long = "env-file", value_name = "PATH")]
    env_files: Vec<String>,

    /// Print the result as JSON on stdout
    #[arg(long)]
    json: bool,
}

pub async fn execute(args: GateArgs) -> anyhow::Result<i32> {
    // The file first: a wrong path or a second test is found before a
    // device is leased and prepared (on iOS that can build a runner).
    let yaml = std::fs::read_to_string(&args.file)
        .map_err(|e| usage_err!("{}: {e}", args.file.display()))?;
    let suite =
        parse_suite_from_str(&yaml).map_err(|e| usage_err!("{}: {e}", args.file.display()))?;
    if suite.tests.len() != 1 {
        usage_bail!(
            "{}: atalanta gate checks a file with one test; this one has {}",
            args.file.display(),
            suite.tests.len()
        );
    }
    let overrides: HashMap<String, String> = super::run::env_overrides(&args.env_files, &args.env)?
        .into_iter()
        .collect();

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
        None => super::explore::pick_device(&*driver).await?,
    };
    let _lease = super::explore::lease(&device, args.lease_wait, args.json).await?;
    driver.prepare(&device).await?;

    let opts = CheckOptions {
        device_id: &device,
        platform,
        overrides: &overrides,
        config: &suite.config,
        replays: args.replay,
        jitter_ms: args.jitter_ms,
    };
    let goal = args.goal;
    let result = Box::pin(check(&*driver, &yaml, &goal, &opts)).await;
    // As explore does: close the browser or session whatever the outcome.
    driver.cleanup().await;
    // Errors keep their kind: a device error exits 3, a config error 2.
    let result = result?;

    if args.json {
        let out = serde_json::json!({
            "file": args.file,
            "goal": goal,
            "trusted": result.gates.trusted,
            "gates": result.gates,
            "replay": result.replay,
        });
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}", serde_json::to_string_pretty(&out)?)?;
    } else {
        super::explore::print_gates(&result.gates);
        if result.gates.trusted {
            println!("\n   next: atalanta run {}", args.file.display());
        } else {
            println!(
                "\n   not trusted: do not commit {} as a regression test until it passes the gates",
                args.file.display()
            );
        }
    }

    Ok(if result.gates.trusted { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: GateArgs,
    }

    #[test]
    fn flags_parse() {
        let c = Cli::try_parse_from([
            "x",
            "suite.yaml",
            "--goal",
            "open Network & internet",
            "--platform",
            "android",
            "--device",
            "emulator-5554",
            "--replay",
            "3",
            "--jitter-ms",
            "10",
            "--env",
            "EMAIL=a@b.c",
            "--env-file",
            ".env",
            "--json",
        ])
        .unwrap();
        assert_eq!(c.args.platform.as_deref(), Some("android"));
        assert_eq!(c.args.device.as_deref(), Some("emulator-5554"));
        assert_eq!(c.args.replay, 3);
        assert_eq!(c.args.jitter_ms, 10);
        assert_eq!(c.args.env, vec!["EMAIL=a@b.c"]);
        assert_eq!(c.args.env_files, vec![".env"]);
        assert!(c.args.json);
        assert_eq!(c.args.goal, "open Network & internet");
        assert_eq!(c.args.file, PathBuf::from("suite.yaml"));
    }

    #[test]
    fn goal_is_required() {
        assert!(Cli::try_parse_from(["x", "suite.yaml"]).is_err());
    }

    #[test]
    fn replay_defaults_to_five() {
        let c = Cli::try_parse_from(["x", "suite.yaml", "--goal", "open X"]).unwrap();
        assert_eq!(c.args.replay, 5);
    }
}
