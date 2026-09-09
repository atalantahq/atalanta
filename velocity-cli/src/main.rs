mod commands;
mod profile;
mod signals;

use clap::{Parser, Subcommand};
use colored::Colorize;
use tracing_subscriber::EnvFilter;

use commands::{
    api, baselines, daemon, device, doctor, explore, heal, history, init, inspect, lint, mcp,
    migrate, nl_eval, repl, run, schema, settings, shard_plan, signing, trace, validate,
};

#[derive(Parser)]
#[command(
    name = "velocity",
    about = "Velocity — fast, reliable mobile UI testing",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Colour output: auto (a terminal, unless NO_COLOR is set or
    /// CLICOLOR=0; CLICOLOR_FORCE=1 forces it), always or never
    #[arg(
        long,
        global = true,
        value_name = "WHEN",
        default_value = "auto",
        value_parser = |s: &str| s.parse::<velocity_common::color::ColorChoice>()
    )]
    color: velocity_common::color::ColorChoice,
}

#[derive(Subcommand)]
enum Command {
    /// Run a test suite
    Run(Box<run::RunArgs>),

    /// Create a starter project from a running app: a first test read from
    /// its launch screen, a CI example, .gitignore entries (`--run` runs it)
    Init(init::InitArgs),

    /// Check this machine and project: tools, devices, runner caches,
    /// suites; `--fix` applies the safe fixes, `--json` for tools
    Doctor(doctor::DoctorArgs),

    /// Manage devices (simulators/emulators); `devices up` / `down` provision them
    #[command(visible_alias = "devices")]
    Device {
        #[command(subcommand)]
        command: device::DeviceCommand,
    },

    /// Migrate tests from other frameworks
    Migrate {
        #[command(subcommand)]
        command: migrate::MigrateCommand,
    },

    /// Open the console: live device mirror with input, element inspector, run and explore panels, tasks, diagnostics
    Inspect(inspect::InspectArgs),

    /// Start the MCP server for AI agents (stdio); `mcp install` registers it with agent clients
    Mcp(mcp::McpArgs),

    /// Validate a test configuration file
    Validate(validate::ValidateArgs),

    /// Lint selectors for fragile or problematic patterns
    Lint(lint::LintArgs),

    /// Review selectors self-healing fixed in passing tests; accept them into the suite
    Heal(heal::HealArgs),

    /// Review failed assertScreenshot comparisons; accept the actual screen
    /// as the new baseline (`review [--serve]`, `accept <name|all>`)
    Baselines {
        #[command(subcommand)]
        command: baselines::BaselinesCommand,
    },

    /// Share run history between CI jobs: `history merge <files…>`
    History {
        #[command(subcommand)]
        command: history::HistoryCommand,
    },

    /// Open the trace viewer for a failed test (`trace view <dir|report>`):
    /// steps, screenshots, video, logs in one self-contained trace.html
    Trace {
        #[command(subcommand)]
        command: trace::TraceCommand,
    },

    /// Print the JSON Schema for suite YAML (editor completion and validation)
    Schema(schema::SchemaArgs),

    /// Run steps one at a time against a device (interactive)
    Repl(repl::ReplArgs),

    /// An AI agent works through a goal on a device once and writes a
    /// deterministic YAML test with assertions, then replays it with no
    /// model calls
    Explore(Box<explore::ExploreArgs>),

    /// List the named settings (`config: settings:` / `run --set`)
    Settings(settings::SettingsArgs),

    /// API tests from an OpenAPI 3 spec: `api scaffold <spec>` writes a
    /// suite with linked requests and checks read from the response schemas
    Api {
        #[command(subcommand)]
        command: api::ApiCommand,
    },

    /// iOS signing via the App Store Connect API key: check the key, register
    /// devices, create development profiles (no Apple ID password)
    Signing {
        #[command(subcommand)]
        command: signing::SigningCommand,
    },

    /// Keep devices warm between runs (`velocity run --daemon` uses it)
    Daemon(daemon::DaemonArgs),

    /// Measure natural-language planner consistency (pass^k) on saved traces
    NlEval(nl_eval::NlEvalArgs),

    /// Print how `run --shard M/N` splits the suite, every shard, and the
    /// plan fingerprint each shard prints (no devices needed)
    ShardPlan(shard_plan::ShardPlanArgs),

    /// Print version information
    Version,
}

fn main() {
    let cli = Cli::parse();
    // One colour decision for every crate's output (C4): `colored` in the
    // CLI, the runner's own escapes, and the log lines on stderr.
    let color_on = velocity_common::color::init(cli.color);
    colored::control::set_override(color_on);

    // `velocity run --daemon`: hand the run to a running daemon.
    if let Command::Run(args) = &cli.command {
        if args.via_daemon() {
            let argv: Vec<String> = std::env::args()
                .skip_while(|a| a != "run")
                .skip(1)
                .filter(|a| a != "--daemon")
                .collect();
            // `--color` is the client's: the daemon gets the decision.
            let argv = without_color_flag(argv);
            match commands::daemon::client_run(argv) {
                Ok(Some(code)) => std::process::exit(code),
                Ok(None) => eprintln!("=> no velocity daemon running; running here"),
                Err(e) => {
                    eprintln!("{} {}", "error:".red().bold(), redacted(&e));
                    std::process::exit(5);
                }
            }
        }
    }

    {
        use tracing_subscriber::prelude::*;
        // stderr: stdout is the program's output, and for `velocity mcp`
        // the JSON-RPC stream, which a log line would corrupt.
        // Through `redact`: a log line can quote an interpolated selector
        // or a failed command's arguments (VEL-066).
        use std::io::IsTerminal;
        let fmt = tracing_subscriber::fmt::layer()
            .with_ansi(velocity_common::color::resolve(
                cli.color,
                std::io::stderr().is_terminal(),
            ))
            .with_writer(|| velocity_common::redact::RedactingWriter::new(std::io::stderr()))
            .with_target(false)
            .with_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
            );
        // `--profile` records Velocity's own spans at debug level whatever
        // RUST_LOG says, without printing them.
        let chrome = match &cli.command {
            Command::Run(args) => args.profile_path().map(|p| {
                profile::layer(p).with_filter(tracing_subscriber::filter::filter_fn(|m| {
                    m.target().starts_with("velocity") && m.level() <= &tracing::Level::DEBUG
                }))
            }),
            _ => None,
        };
        tracing_subscriber::registry().with(fmt).with(chrome).init();
    }

    // A panic anywhere in Velocity is an internal error: exit 5 (D-19),
    // not Rust's 101, which CI reads as an unknown failure.
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_command(cli))) {
        Ok(r) => r,
        Err(panic) => {
            profile::flush();
            velocity_runner::reporter::github::resume_commands();
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            eprintln!(
                "{} internal error: Velocity panicked: {}",
                "error:".red().bold(),
                velocity_common::redact::redact(&msg)
            );
            std::process::exit(5);
        }
    };
    profile::flush();

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), redacted(&e));
            std::process::exit(commands::exit_code_for(&e));
        }
    }
}

/// The log filter without RUST_LOG. chromiumoxide 0.9.1 can't parse some
/// CDP messages newer Chrome sends (156: "data did not match any variant of
/// untagged enum Message"); it skips them, but warned on every web run.
/// Its connection errors are `error!` and still print.
const DEFAULT_LOG_FILTER: &str = "warn,chromiumoxide::handler=error";

/// `argv` without `--color WHEN` / `--color=WHEN` (a global flag the
/// daemon's `run` parser doesn't take).
fn without_color_flag(argv: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        if a == "--color" {
            it.next();
        } else if !a.starts_with("--color=") {
            out.push(a);
        }
    }
    out
}

/// An error as printed: never with a secret in it (VEL-211).
pub fn redacted(e: &anyhow::Error) -> String {
    velocity_common::redact::redact(&e.to_string()).into_owned()
}

#[tokio::main]
async fn run_command(cli: Cli) -> anyhow::Result<i32> {
    // The biggest commands' futures (run, explore: ~50 KB) live on the heap,
    // not on the main thread's stack (clippy::large_futures).
    match cli.command {
        Command::Run(args) => Box::pin(run::execute(*args)).await,
        Command::Init(args) => init::execute(args).await,
        Command::Doctor(args) => Box::pin(doctor::execute(args)).await,
        Command::Device { command } => device::execute(command).await,
        Command::Migrate { command } => migrate::execute(command),
        Command::Inspect(args) => Box::pin(inspect::execute(args)).await,
        Command::Mcp(args) => mcp::execute(args).await,
        Command::Validate(args) => validate::execute(args),
        Command::Lint(args) => lint::execute(args),
        Command::Heal(args) => heal::execute(args),
        Command::History { command } => history::execute(command),
        Command::Baselines { command } => baselines::execute(command).await,
        Command::Schema(args) => schema::execute(args),
        Command::Trace { command } => trace::execute(command),
        Command::Repl(args) => Box::pin(repl::execute(args)).await,
        Command::Explore(args) => Box::pin(explore::execute(*args)).await,
        Command::Signing { command } => signing::execute(command).await,
        Command::Settings(args) => settings::execute(args),
        Command::Api { command } => api::execute(command).await,
        Command::Daemon(args) => daemon::execute(args).await,
        Command::NlEval(args) => nl_eval::execute(args).await,
        Command::ShardPlan(args) => shard_plan::execute(args),
        Command::Version => {
            println!("{} {}", "velocity".bold(), env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_color_flag_is_not_forwarded_to_the_daemon() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            super::without_color_flag(argv(&["s.yaml", "--color", "never", "--tags", "a"])),
            argv(&["s.yaml", "--tags", "a"])
        );
        assert_eq!(
            super::without_color_flag(argv(&["--color=always", "s.yaml"])),
            argv(&["s.yaml"])
        );
    }

    /// `EnvFilter::new` drops a directive it can't parse without a word.
    #[test]
    fn the_default_log_filter_parses() {
        let f = tracing_subscriber::EnvFilter::try_new(super::DEFAULT_LOG_FILTER).unwrap();
        assert!(
            f.to_string().contains("chromiumoxide::handler=error"),
            "{f}"
        );
    }

    /// The inspector console's Run panel starts `velocity run` with these
    /// arguments: they must be ones `run` takes (a renamed flag failed
    /// every console run with exit 2).
    #[test]
    fn the_console_run_command_parses() {
        use clap::Parser;
        use velocity_inspector::console::tasks::{RunRequest, SuiteLauncher, VelocityBinary};
        let launcher = VelocityBinary {
            exe: "velocity".into(),
        };
        let req = RunRequest {
            suite: "flows/a.yaml".into(),
            device_id: "emulator-5554".into(),
            filter: Some("log*".into()),
        };
        let cmd = launcher.command(
            std::path::Path::new("/p/flows/a.yaml"),
            &req,
            std::path::Path::new("/p/velocity-results/console/1"),
            std::path::Path::new("/p/velocity-results/console/1/events.jsonl"),
        );
        let std_cmd = cmd.as_std();
        let mut argv = vec![std_cmd.get_program().to_string_lossy().to_string()];
        argv.extend(std_cmd.get_args().map(|a| a.to_string_lossy().to_string()));
        match super::Cli::try_parse_from(&argv) {
            Ok(cli) => assert!(matches!(cli.command, super::Command::Run(_)), "{argv:?}"),
            Err(e) => panic!("{argv:?}: {e}"),
        }
        assert!(std_cmd
            .get_envs()
            .any(|(k, _)| k == velocity_common::live_events::EVENTS_FILE_ENV));
    }
}
