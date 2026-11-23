// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta api scaffold`: an API test suite from an OpenAPI 3 document
//! (the work is in `atalanta_api::openapi`).

use std::path::PathBuf;

use atalanta_api::openapi::{self, Options, Spec};
use clap::{Args, Subcommand};
use colored::Colorize;

#[derive(Subcommand)]
pub enum ApiCommand {
    /// Write a suite from an OpenAPI 3 spec (YAML or JSON): one test per
    /// resource, created ids captured for later requests, checks read from
    /// the response schemas; `--confirm` drops the checks a real server's
    /// GET responses disprove
    Scaffold(ScaffoldArgs),
}

#[derive(Args)]
pub struct ScaffoldArgs {
    /// The OpenAPI 3 document (.yaml, .yml or .json)
    spec: PathBuf,
    /// Where the suite goes (default: stdout)
    #[arg(short = 'o', long = "out")]
    out: Option<PathBuf>,
    /// Overwrite `--out` if it exists
    #[arg(long)]
    force: bool,
    /// `config.http.baseUrl` (default: the spec's first server)
    #[arg(long = "base-url", value_name = "URL")]
    base_url: Option<String>,
    /// Send the GET and HEAD requests (never any other method) to the base
    /// url and drop each check its response disproves; `${VARS}` come from
    /// the environment
    #[arg(long)]
    confirm: bool,
    /// Most `expect.json` checks per request
    #[arg(long = "max-checks", default_value_t = 12)]
    max_checks: usize,
}

pub async fn execute(command: ApiCommand) -> anyhow::Result<i32> {
    match command {
        ApiCommand::Scaffold(args) => scaffold(args).await,
    }
}

async fn scaffold(args: ScaffoldArgs) -> anyhow::Result<i32> {
    if let Some(out) = args.out.as_ref().filter(|o| o.exists() && !args.force) {
        usage_bail!("{} exists (--force overwrites it)", out.display());
    }
    let text = std::fs::read_to_string(&args.spec)
        .map_err(|e| usage_err!("can't read {}: {e}", args.spec.display()))?;
    let spec = Spec::parse(&text).map_err(|e| usage_err!("{}: {e}", args.spec.display()))?;
    let opts = Options {
        base_url: args.base_url.clone(),
        max_checks: args.max_checks,
        source: args.spec.file_name().map_or_else(
            || args.spec.display().to_string(),
            |n| n.to_string_lossy().into(),
        ),
    };
    let mut suite = openapi::scaffold(&spec, &opts).map_err(|e| usage_err!("{e}"))?;

    if args.confirm {
        let report = openapi::confirm(&mut suite, &|name| std::env::var(name).ok()).await?;
        print_report(&report);
        if report.sent == 0 {
            if let Some(why) = &report.unreachable {
                usage_bail!("--confirm reached no server: {why}");
            }
        }
    }

    let yaml = openapi::render(&suite).map_err(|e| anyhow::anyhow!("writing the suite: {e}"))?;
    check(&yaml)?;
    match &args.out {
        Some(out) => {
            std::fs::write(out, &yaml)?;
            eprintln!(
                "{} {}: {} tests, {} requests, {} checks; {} variables to set (listed at its top)",
                "✓".green().bold(),
                out.display(),
                suite.tests.len(),
                suite.requests(),
                suite.checks(),
                suite.variables.len()
            );
        }
        None => print!("{yaml}"),
    }
    Ok(0)
}

/// The suite must load like any other: a scaffold that doesn't is a bug
/// here, not the spec's fault.
fn check(yaml: &str) -> anyhow::Result<()> {
    let suite = atalanta_core::parse_suite_from_str(yaml)
        .map_err(|e| anyhow::anyhow!("the scaffolded suite doesn't parse: {e}"))?;
    atalanta_core::validate_suite(&suite)
        .map_err(|e| anyhow::anyhow!("the scaffolded suite doesn't validate: {e}"))?;
    Ok(())
}

fn print_report(r: &openapi::ConfirmReport) {
    eprintln!(
        "{} --confirm: {} requests answered, {} checks held, {} dropped",
        "=>".cyan().bold(),
        r.sent,
        r.held,
        r.dropped.len()
    );
    for note in &r.notes {
        eprintln!("  {} {note}", "!".yellow());
    }
    for f in &r.dropped {
        eprintln!(
            "  {} {} › {} `{}`: {}",
            "✗".red(),
            f.test,
            f.request,
            f.path,
            f.why
        );
    }
    for f in &r.unconfirmed {
        eprintln!("  {} {} › {}: {}", "?".yellow(), f.test, f.request, f.why);
    }
    let not_sent = r
        .skipped
        .iter()
        .filter(|f| f.why.contains("never sent"))
        .count();
    if not_sent > 0 {
        eprintln!("  {not_sent} requests not sent (confirm mode sends only GET and HEAD)");
    }
    for f in r.skipped.iter().filter(|f| !f.why.contains("never sent")) {
        eprintln!("  - {} › {}: {}", f.test, f.request, f.why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through the same loader and validator as `atalanta run`.
    #[test]
    fn a_scaffolded_suite_parses_and_validates() {
        let text = include_str!("../../../crates/atalanta-api/tests/fixtures/petstore.yaml");
        let spec = Spec::parse(text).unwrap();
        let suite = openapi::scaffold(&spec, &Options::default()).unwrap();
        let yaml = openapi::render(&suite).unwrap();
        check(&yaml).unwrap();
        let parsed = atalanta_core::parse_suite_from_str(&yaml).unwrap();
        assert!(parsed.app_id.is_empty());
        assert_eq!(parsed.tests.len(), 2);
        assert!(atalanta_core::http_only_tests(&parsed.tests));
    }
}
