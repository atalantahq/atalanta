// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta results check`: after a sharded CI run, check that every shard
//! reported (NEW-001). The rules are in `atalanta_runner::results_check`.

use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum ResultsCommand {
    /// Check a run's downloaded results: every shard reported, none ran
    /// zero tests, every results.json and junit.xml is readable. Exit 1
    /// when anything is missing, so a job that died or uploaded nothing
    /// can't leave the run green
    Check {
        /// Results files or folders holding them (searched to any depth,
        /// e.g. the folder `actions/download-artifact` wrote)
        #[arg(required = true, value_name = "PATHS")]
        inputs: Vec<String>,

        /// How many shards the run had: that many results files must
        /// arrive, one per shard
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        expect_shards: Option<u64>,
    },
}

pub fn execute(command: ResultsCommand) -> anyhow::Result<i32> {
    match command {
        ResultsCommand::Check {
            inputs,
            expect_shards,
        } => {
            use atalanta_runner::results_check::{check, find};
            let files = find(&inputs);
            let c = check(&files, expect_shards.map(|n| n as usize));
            for (f, shard, tests) in &c.files {
                println!("{}", file_line(f, *shard, *tests));
            }
            if c.problems.is_empty() {
                println!(
                    "{} Results complete: {} file(s), {} test(s), {} failed",
                    "=>".cyan().bold(),
                    c.files.len(),
                    c.tests,
                    c.failed
                );
                return Ok(0);
            }
            for p in &c.problems {
                eprintln!("{} {}", "error:".red().bold(), one_line(p));
            }
            eprintln!(
                "{} Results incomplete: this run is not green whatever the jobs said",
                "=>".red().bold()
            );
            Ok(1)
        }
    }
}

/// One results file as listed: the path comes from the downloaded
/// artifacts, so it is written as [`one_line`] writes problems.
fn file_line(file: &std::path::Path, shard: Option<usize>, tests: usize) -> String {
    let s = shard
        .map(|i| format!("shard {}: ", i + 1))
        .unwrap_or_default();
    format!(
        "  {} {s}{tests} test(s) in {}",
        "+".green(),
        one_line(&file.display().to_string())
    )
}

/// `text` on one line: a problem quotes `results.json` fields and file
/// names, which come from the jobs being checked. A newline in them could
/// start a line with `::` that GitHub Actions runs as a workflow command
/// (`::add-mask::`, `::error`, `::stop-commands::`) in the aggregating job
/// (audit 2026-11 FS-4); control characters are written escaped.
fn one_line(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod audit_tests {
    /// The listing of a found file is a line of stdout too: a folder named
    /// with a newline must not start a line with `::`.
    #[test]
    fn a_path_with_a_newline_can_not_start_a_workflow_command() {
        let path = std::path::Path::new("dl/x\n::add-mask::v\r\n::error::forged/results.json");
        let line = super::file_line(path, Some(0), 3);
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(line.contains("shard 1: 3 test(s)"), "{line:?}");
    }

    #[test]
    fn a_problem_quoting_a_newline_can_not_start_a_workflow_command() {
        let p = "shard 1/2: fingerprint x\n::error file=a::forged\r\n::add-mask::v";
        let out = super::one_line(p);
        assert!(!out.contains('\n') && !out.contains('\r'), "{out}");
        assert!(out.lines().all(|l| !l.starts_with("::")));
    }
}
