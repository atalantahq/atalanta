// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta shard-plan`: the partition `run --shard M/N` uses, for every
//! shard, without devices (D-49; Flank's `--dump-shards`, bktec's `plan`).
//! Each CI shard computes its part independently; the `plan=` fingerprint
//! printed here and by every shard shows whether they agree.

use atalanta_common::{AtalantaError, RuntimeConfig};
use clap::Args;

#[derive(Args)]
pub struct ShardPlanArgs {
    /// Test suite YAML
    #[arg(value_name = "PATH", default_value = "atalanta.yaml")]
    path: String,

    /// Number of shards
    #[arg(long, value_name = "N",
          value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    total: usize,

    /// Balance by these durations (a atalanta-history.json, a JUnit XML
    /// report, or a folder of either), as `run --shard-timings`
    #[arg(long, value_name = "FILE")]
    shard_timings: Option<String>,

    /// Only tests with these tags (comma-separated), as `run --tags`
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,

    /// Only tests whose name matches this glob, as `run --filter`
    #[arg(long)]
    filter: Option<String>,

    /// Environment variable overrides (KEY=VALUE; several per flag as
    /// A=1,B=2), split exactly as `run --env` splits them
    #[arg(long = "env")]
    env_overrides: Vec<String>,

    /// Overrides from a dotenv file (repeatable), read as `run --env-file`
    /// reads them
    #[arg(long = "env-file", value_name = "PATH")]
    env_files: Vec<String>,

    /// Settings or suite config overrides (KEY=VALUE), as `run --set`
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,

    /// Print the plan as JSON
    #[arg(long)]
    json: bool,
}

pub fn execute(args: ShardPlanArgs) -> anyhow::Result<i32> {
    // `run`'s own parsing: `--env A=1,B=2` here set A to `1,B=2`, so the
    // two printed different fingerprints for the same flags (review d4 m8).
    let env = super::run::env_overrides(&args.env_files, &args.env_overrides)?;
    let (mut suite, _) = super::run::load_suite(&args.path, &args.set, &env)?;
    suite.tests = atalanta_core::resolve_flows(&suite)?;
    let shard_timings_ms = match &args.shard_timings {
        Some(path) => Some(std::sync::Arc::new(
            atalanta_runner::history::load_timings(std::path::Path::new(path))
                .map_err(|e| AtalantaError::Config(format!("--shard-timings: {e}")))?
                .durations,
        )),
        None => None,
    };
    let config = RuntimeConfig {
        tags: args.tags.clone(),
        test_filter: args.filter.clone(),
        shard_total: Some(args.total),
        shard_timings: args.shard_timings.clone(),
        shard_timings_ms,
        ..Default::default()
    };
    let plan = atalanta_runner::scheduler::shard_plan(&suite.tests, &config);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(0);
    }
    let planned: usize = plan.shards.iter().map(Vec::len).sum();
    println!(
        "plan={} ({} tests in {} shards, balanced by {})",
        plan.fingerprint,
        planned,
        args.total,
        if plan.by_timings {
            "timings"
        } else {
            "test count"
        }
    );
    for (i, names) in plan.shards.iter().enumerate() {
        println!("shard {}/{} ({} tests)", i + 1, args.total, names.len());
        for n in names {
            println!("  {n}");
        }
    }
    Ok(0)
}
