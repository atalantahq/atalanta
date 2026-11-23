use std::sync::Arc;

use atalanta_common::Platform;
use atalanta_mcp::install::{self, Client, InstallEnv};
use atalanta_mcp::tasks::{Notifier, TaskManager};
use atalanta_mcp::McpServer;
use clap::{Args, Subcommand};
use colored::Colorize;

use super::create_driver;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct McpArgs {
    #[command(subcommand)]
    command: Option<McpCommand>,

    /// Target device ID
    #[arg(short, long)]
    device: Option<String>,

    /// Target platform (ios, android or web); from the device id if omitted,
    /// else ios
    #[arg(short, long)]
    platform: Option<String>,

    /// Path to test suite config for flow/test tools
    #[arg(short, long)]
    config: Option<String>,

    /// Only serve tools that don't change the device (screenshots, element
    /// queries, summaries): no taps, typing or test runs
    #[arg(long)]
    read_only: bool,

    /// Which tools to serve: core (default: observe, act, run, artifacts,
    /// each standing for several of the full tools) or full (all of them).
    /// Every tool definition costs the agent context on every turn
    #[arg(long, default_value = "core", value_name = "core|full", ignore_case = true,
          value_parser = clap::builder::PossibleValuesParser::new([
              clap::builder::PossibleValue::new("core").alias("default"),
              clap::builder::PossibleValue::new("full").alias("all"),
          ]))]
    profile: String,

    /// Tell someone when a background task (run_task) ends (repeatable):
    /// webhook=<url>, script=<path>, jsonl=<path> or desktop
    #[arg(long, value_name = "KIND[=TARGET]")]
    notify: Vec<String>,

    /// Where run_task keeps each task's log, reports and traces
    /// [default: ./atalanta-results/tasks, or ~/.atalanta/mcp-tasks when
    /// started in / or $HOME]
    #[arg(long, value_name = "DIR")]
    tasks_dir: Option<String>,
}

#[derive(Subcommand)]
enum McpCommand {
    /// Register the MCP server with agent clients and install the Atalanta
    /// agent rules (a skill or rules file) where each reads them
    Install {
        /// all (every client found on this machine), claude, cursor, codex,
        /// windsurf, vscode, cline or gemini
        #[arg(default_value = "all")]
        client: String,
        /// Print what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Remove the server and the rules instead
        #[arg(long)]
        uninstall: bool,
    },
    /// Print a client's MCP config snippet, to add by hand
    Config {
        /// claude, cursor, codex, windsurf, vscode, cline or gemini
        client: String,
    },
    /// Print the agent rules (the Atalanta workflow and YAML cheat sheet)
    Rules,
}

fn parse_client(name: &str) -> anyhow::Result<Client> {
    match Client::parse(name) {
        Some(c) => Ok(c),
        None => usage_bail!(
            "Unknown client '{name}'. Use one of: {}",
            Client::ALL.map(|c| c.name()).join(", ")
        ),
    }
}

/// `atalanta mcp install|config|rules`.
fn manage(command: McpCommand) -> anyhow::Result<i32> {
    let env = || InstallEnv::current(vec!["mcp".to_string()]);
    match command {
        McpCommand::Rules => {
            print!("{}", install::AGENT_RULES);
            Ok(0)
        }
        McpCommand::Config { client } => {
            let client = parse_client(&client)?;
            println!("{}", install::config_snippet(&env()?, client));
            Ok(0)
        }
        McpCommand::Install {
            client,
            dry_run,
            uninstall,
        } => {
            let env = env()?;
            let clients: Vec<Client> = if client.eq_ignore_ascii_case("all") {
                let found: Vec<Client> = Client::ALL
                    .into_iter()
                    .filter(|c| install::detected(&env, *c))
                    .collect();
                if found.is_empty() {
                    eprintln!(
                        "No agent client found under {}. Name one: atalanta mcp install <{}>",
                        env.home.display(),
                        Client::ALL.map(|c| c.name()).join("|")
                    );
                    return Ok(2);
                }
                found
            } else {
                vec![parse_client(&client)?]
            };
            if env.atalanta.components().any(|c| c.as_os_str() == "target") {
                eprintln!(
                    "{} registering a development build ({}); install a release build for daily use",
                    "warning:".yellow().bold(),
                    env.atalanta.display()
                );
            }
            let opts = install::Options { dry_run, uninstall };
            let mut failed = false;
            for c in clients {
                match install::install(&env, c, opts) {
                    Ok(changes) => {
                        for ch in changes {
                            println!("{} {}: {}", "=>".cyan().bold(), c.name(), ch.what);
                        }
                    }
                    Err(e) => {
                        failed = true;
                        eprintln!("{} {}: {e}", "error:".red().bold(), c.name());
                    }
                }
            }
            if !dry_run && !uninstall && !failed {
                println!("Restart the client (or reload its MCP servers) to pick Atalanta up.");
            }
            Ok(if failed { 1 } else { 0 })
        }
    }
}

pub async fn execute(args: McpArgs) -> anyhow::Result<i32> {
    if let Some(command) = args.command {
        return manage(command);
    }
    let notifiers = args
        .notify
        .iter()
        .map(|n| n.parse::<Notifier>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| usage_err!("{e}"))?;
    let platform = platform_of(args.platform.as_deref(), args.device.as_deref())?;
    let profile: atalanta_mcp::profile::Profile = args
        .profile
        .parse()
        .map_err(|e: String| usage_err!("{e}"))?;

    eprintln!(
        "{} Starting MCP server (platform={}, transport=stdio)",
        "=>".cyan().bold(),
        platform
    );

    let driver = create_driver(platform);

    let device_id = match args.device {
        Some(id) => id,
        None => {
            let devices = driver.list_devices().await?;
            let booted = devices
                .iter()
                .find(|d| d.state == atalanta_common::DeviceState::Booted);
            match booted {
                Some(d) => {
                    eprintln!(
                        "{} Auto-selected device: {} ({})",
                        "=>".cyan().bold(),
                        d.name,
                        d.id
                    );
                    d.id.clone()
                }
                None => {
                    return Err(super::no_device(
                        "No booted device found. Boot a device first with: atalanta device boot <name>",
                        devices.iter().map(|d| d.id.clone()).collect(),
                    ));
                }
            }
        }
    };

    // The web driver has no browser until `prepare`; mobile drivers set up
    // on first use (an iOS runner build can outlast a client's
    // `initialize` timeout, so not up front).
    if platform == Platform::Web {
        driver.prepare(&device_id).await?;
    }
    McpServer::init_logging();
    let driver_arc: Arc<dyn atalanta_common::PlatformDriver> = Arc::from(driver);
    let tasks_root = args
        .tasks_dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(TaskManager::default_root);
    let mut server = McpServer::new(driver_arc, device_id, args.config)
        .read_only(args.read_only)
        .profile(profile)
        .with_platform(platform)
        .with_leases(true)
        .with_tasks(TaskManager::new(tasks_root, notifiers));
    Box::pin(server.run_stdio()).await?;
    Ok(0)
}

/// `-p`, else from the device id, else iOS.
fn platform_of(platform: Option<&str>, device: Option<&str>) -> anyhow::Result<Platform> {
    Ok(match platform {
        Some("ios") => Platform::Ios,
        Some("android") => Platform::Android,
        // The server drives web too (VEL-172).
        Some("web") => Platform::Web,
        Some(other) => usage_bail!("Unknown platform '{other}'. Use 'ios', 'android' or 'web'."),
        None => device.map(Platform::of_device_id).unwrap_or(Platform::Ios),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VEL-172: `atalanta mcp -p web` is accepted.
    #[test]
    fn web_is_a_platform() {
        assert_eq!(platform_of(Some("web"), None).unwrap(), Platform::Web);
        assert_eq!(
            platform_of(Some("android"), None).unwrap(),
            Platform::Android
        );
        assert!(platform_of(Some("tv"), None).is_err());
    }
}
