// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta doctor`: is this machine ready to run Atalanta? The probes
//! live in the `atalanta-doctor` crate (shared with the MCP server); this
//! is the command around them.

use std::path::PathBuf;
use std::time::Duration;

use atalanta_doctor::{apply_safe_fixes, run_checks, Options, Platform};
use clap::Args;

#[derive(Args)]
pub struct DoctorArgs {
    /// Suite file or folder to check too [default: atalanta.yaml or the
    /// atalanta/ folder here, if there is one]
    path: Option<PathBuf>,

    /// Print the report as JSON
    #[arg(long)]
    json: bool,

    /// Also push the Android tree server to a device and handshake, and
    /// connect to the AI API host (slower; touches a device)
    #[arg(long)]
    deep: bool,

    /// Apply the safe fixes (restart a wedged adb server, reconnect
    /// offline devices, clear stale leases, re-push the tree server),
    /// then check again. Never anything interactive or needing sudo
    #[arg(long)]
    fix: bool,

    /// Exit non-zero when degraded (warnings), not only when blocked
    #[arg(long)]
    strict: bool,

    /// Platforms you test on (comma-separated): a missing toolchain for
    /// one of them fails instead of being skipped
    #[arg(short, long, value_delimiter = ',', value_name = "ios|android|web")]
    platform: Vec<String>,

    /// Deadline for each external command. Takes a unit (3s, 500ms); a
    /// bare number is milliseconds
    #[arg(long, default_value = "3000", value_name = "DURATION", value_parser = super::duration_arg::bare_millis)]
    timeout: Duration,
}

pub async fn execute(args: DoctorArgs) -> anyhow::Result<i32> {
    let platforms = args
        .platform
        .iter()
        .map(|p| match p.trim() {
            "ios" => Ok(Platform::Ios),
            "android" => Ok(Platform::Android),
            "web" => Ok(Platform::Web),
            other => Err(usage_err!(
                "unknown platform '{other}': use ios, android or web"
            )),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if let Some(p) = &args.path {
        if !p.exists() {
            usage_bail!("{}: no such file or folder", p.display());
        }
    }
    let opts = Options {
        deep: args.deep,
        platforms,
        project: args.path.clone(),
        timeout: args.timeout.max(Duration::from_millis(100)),
        ..Options::default()
    };
    let color = atalanta_common::color::enabled();

    let mut report = Box::pin(run_checks(&opts)).await;
    let mut fixes = Vec::new();
    if args.fix {
        fixes = apply_safe_fixes(&report, &opts).await;
        if !fixes.is_empty() {
            if !args.json {
                println!("{}", report.render(color));
                println!("Applying safe fixes:");
                for f in &fixes {
                    println!(
                        "  {} {}: {}",
                        if f.ok { "✓" } else { "✗" },
                        f.check_id,
                        f.message
                    );
                }
                println!("\nChecking again…\n");
            }
            report = Box::pin(run_checks(&opts)).await;
        } else if !args.json {
            println!("(no safe fixes to apply)\n");
        }
    }

    if args.json {
        let mut v = serde_json::to_value(&report)?;
        if args.fix {
            v["fixes"] = serde_json::to_value(&fixes)?;
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", report.render(color));
    }
    Ok(report.exit_code(args.strict))
}
