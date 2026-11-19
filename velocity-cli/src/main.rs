mod commands;
mod profile;
mod signals;

use clap::{Parser, Subcommand};
use colored::Colorize;
use tracing_subscriber::EnvFilter;

use commands::{
    agent, api, baselines, daemon, device, doctor, explore, heal, history, init, inspect, lint,
    login, mcp, migrate, nl_eval, repl, results, run, schema, settings, shard_plan, signing, trace,
    validate, verify_change,
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

    /// Validate a suite file (or every suite in a folder) without running it
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

    /// Did this change do what it says? Replays a saved test for the
    /// description with no model (or explores and saves one that passes its
    /// trust gates) and writes PR evidence: video, failures.md, a manifest
    VerifyChange(verify_change::VerifyChangeArgs),

    /// Run a stored login flow so an agent starts from a logged-in app
    /// without handling the password: ${NAME}s from the environment,
    /// ${keychain:NAME} or ${totp:NAME}, masked everywhere
    Login(login::LoginArgs),

    /// Check a sharded run's results after the matrix: `results check
    /// --expect-shards N <folders…>` fails when a shard reported nothing
    Results {
        #[command(subcommand)]
        command: results::ResultsCommand,
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

    #[command(flatten)]
    Agent(agent::AgentCommand),
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
        let filter = || {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER))
        };
        // `VELOCITY_LOG_FORMAT=json`: one JSON object per line, with the
        // current span's fields (`test`, `device`, `attempt`), for a farm's
        // log pipeline.
        let json = log_format_is_json(velocity_common::env::var("VELOCITY_LOG_FORMAT").ok());
        let fmt = (!json).then(|| {
            tracing_subscriber::fmt::layer()
                .with_ansi(velocity_common::color::resolve(
                    cli.color,
                    std::io::stderr().is_terminal(),
                ))
                .with_writer(|| velocity_common::redact::RedactingWriter::new(std::io::stderr()))
                .with_target(false)
                .with_filter(filter())
        });
        let fmt_json = json.then(|| {
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(|| velocity_common::redact::RedactingWriter::new(std::io::stderr()))
                .with_filter(filter())
        });
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
        tracing_subscriber::registry()
            .with(fmt)
            .with(fmt_json)
            .with(chrome)
            .init();
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
            if let Some(hint) = next_action(&e) {
                eprintln!("  {} {hint}", "Try:".bold());
            }
            std::process::exit(commands::exit_code_for(&e));
        }
    }
}

/// What to do about a command's error, printed under it as `Try: …`, for
/// the errors whose message names what broke but not the way out. `None`
/// when the message already says (a locked device) or there is no one
/// next step.
fn next_action(e: &anyhow::Error) -> Option<&'static str> {
    use velocity_common::VelocityError as V;
    // adb missing reaches here as a plain configuration or device error;
    // it is the first thing a Maestro user without platform-tools sees.
    if e.chain()
        .any(|c| c.to_string().contains("Failed to execute adb"))
    {
        return Some(
            "adb is not installed or not on PATH: velocity doctor -p android says where it is \
             (or set VELOCITY_ADB_PATH)",
        );
    }
    let v = e.chain().find_map(|c| c.downcast_ref::<V>())?;
    Some(match v {
        V::MissingEnvVars { .. } => "--env NAME=value, --env-file .env, or export the variable",
        V::UnknownFlowRef { .. } => {
            "check the runFlow id against the suite's flows: (velocity validate lists them)"
        }
        V::WdaUnhealthy { .. } | V::WdaUnreachable { .. } | V::RunnerFailing { .. } => {
            "velocity doctor -p ios (Xcode license, xcode-select, simulator runtime)"
        }
        V::CircuitOpen { .. } | V::AdbConnectionLost { .. } => {
            "velocity doctor --fix (restarts adb), then run again"
        }
        V::DeviceNotFound { available, .. } if available.is_empty() => {
            "velocity devices up --android 1 (or --ios 1) boots one; velocity devices list shows what is connected"
        }
        V::DeviceBootFailed { .. } => "velocity doctor checks the SDK, the AVDs and free memory/disk",
        V::SuiteTimeout { .. } => {
            "raise --timeout (it takes a unit: --timeout 30m); the tests it cut off are in the report"
        }
        V::BrowserConnectionLost { .. } => "velocity doctor -p web checks Chrome",
        _ => return None,
    })
}

/// The log filter without RUST_LOG. chromiumoxide 0.9.1 can't parse some
/// CDP messages newer Chrome sends (156: "data did not match any variant of
/// untagged enum Message"); it skips them, but warned on every web run.
/// Its connection errors are `error!` and still print.
const DEFAULT_LOG_FILTER: &str = "warn,chromiumoxide::handler=error";

/// `argv` without `--color WHEN` / `--color=WHEN` (a global flag the
/// daemon's `run` parser doesn't take).
pub(crate) fn without_color_flag(argv: Vec<String>) -> Vec<String> {
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
        Command::Results { command } => results::execute(command),
        Command::Login(args) => Box::pin(login::execute(args)).await,
        Command::VerifyChange(args) => Box::pin(verify_change::execute(args)).await,
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
        Command::Agent(cmd) => Box::pin(agent::execute(cmd)).await,
        Command::Version => {
            println!("{} {}", "velocity".bold(), env!("CARGO_PKG_VERSION"));
            // Build identity on line 2 (line 1's format is a contract):
            // release.yml sets both variables; other builds say "unknown".
            println!(
                "commit {}, {}-{}, rustc {}",
                option_env!("VELOCITY_GIT_SHA").unwrap_or("unknown"),
                std::env::consts::ARCH,
                std::env::consts::OS,
                option_env!("VELOCITY_RUSTC_VERSION").unwrap_or("unknown"),
            );
            Ok(0)
        }
    }
}

/// Whether `VELOCITY_LOG_FORMAT` asks for JSON log lines (`json`, any
/// case). Anything else, or unset, is the text format.
fn log_format_is_json(value: Option<String>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    /// Review 13 H3 / 06 #11: every long flag of the commands people script
    /// is in the docs, so `--help` is never the only description.
    #[test]
    fn every_long_flag_of_the_main_commands_is_documented() {
        use clap::CommandFactory;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
        let cli_md = read("docs/CLI.md");
        let mcp_md = read("docs/MCP.md");
        let cmd = super::Cli::command();
        let mut missing = Vec::new();
        let both = format!("{cli_md}\n{mcp_md}");
        for (name, doc) in [
            ("run", &cli_md),
            ("explore", &cli_md),
            ("init", &cli_md),
            ("doctor", &cli_md),
            ("mcp", &both),
        ] {
            let sub = cmd
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("no `{name}` subcommand"));
            for arg in sub.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if arg.is_hide_set() || long == "help" {
                    continue;
                }
                let flag = format!("--{long}");
                let documented = doc.match_indices(&flag).any(|(i, _)| {
                    !doc[i + flag.len()..]
                        .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
                });
                if !documented {
                    missing.push(format!("{name} {flag}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "flags missing from docs/CLI.md (mcp: CLI.md or MCP.md): {missing:?}"
        );
    }

    #[test]
    fn json_logs_are_opt_in() {
        assert!(super::log_format_is_json(Some("json".into())));
        assert!(super::log_format_is_json(Some(" JSON ".into())));
        assert!(!super::log_format_is_json(Some("text".into())));
        assert!(!super::log_format_is_json(None));
    }

    #[test]
    fn device_and_env_errors_say_what_to_do_next() {
        use velocity_common::VelocityError as V;
        let e: anyhow::Error = V::MissingEnvVars {
            vars: vec!["TOKEN".into()],
        }
        .into();
        assert!(super::next_action(&e).unwrap().contains("--env"));
        // Through a context too.
        let e = anyhow::Error::new(V::DeviceNotFound {
            id: String::new(),
            available: vec![],
        })
        .context("no device to run on");
        assert!(super::next_action(&e).unwrap().contains("devices up"));
        let e: anyhow::Error = V::DeviceLocked { id: "x".into() }.into();
        assert_eq!(super::next_action(&e), None, "its message already says");
        let e: anyhow::Error =
            V::Config("Failed to execute adb: No such file or directory".into()).into();
        assert!(super::next_action(&e)
            .unwrap()
            .contains("VELOCITY_ADB_PATH"));
    }

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
