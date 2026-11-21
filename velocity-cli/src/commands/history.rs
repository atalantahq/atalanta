//! `velocity history`: share the run history (flaky flags, durations,
//! last failures) between CI jobs without a server (C1). Each job uploads
//! its `velocity-history.json` as an artifact; `history merge` combines
//! the downloaded ones (`velocity run --history-from` reads them
//! directly). The merge rule is in `velocity_runner::shared_history`.

use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// Merge history files from several CI jobs into one: per test, the
    /// job that ran it last wins; device profiles are kept from every job
    Merge {
        /// History files, folders holding a velocity-history.json, or
        /// globs (`history/*`, `artifacts/**/velocity-history.json`)
        #[arg(required = true, value_name = "FILES")]
        inputs: Vec<String>,

        /// Where to write the result: a `.json` file, or a folder that
        /// gets a velocity-history.json
        #[arg(short, long, value_name = "PATH", default_value = "./velocity-results")]
        output: String,
    },

    /// Show each test's flake rate over its last N recorded runs.
    ///
    /// A run counts as flaky when it passed only after a retry, or failed
    /// then passed with no code change (whatever history.rs already counts
    /// as flaky). Infrastructure failures are excluded from the rate and
    /// shown apart. Exits 1 when any test with at least 5 runs exceeds
    /// --max-rate, exit 0 otherwise, exit 2 when the file is missing or
    /// unreadable.
    Flaky {
        /// History file (a velocity-history.json, or a folder holding one);
        /// defaults to where `velocity run` writes it
        #[arg(
            long,
            value_name = "FILE",
            default_value = "./velocity-results/velocity-history.json"
        )]
        file: String,

        /// Number of recent runs per test
        #[arg(long, value_name = "N", default_value = "20")]
        window: usize,

        /// Fail when any test with at least 5 runs exceeds this rate
        #[arg(long, value_name = "R", default_value = "0.05")]
        max_rate: f64,

        /// Print one JSON object instead of a table
        #[arg(long)]
        json: bool,
    },
}

pub fn execute(command: HistoryCommand) -> anyhow::Result<i32> {
    match command {
        HistoryCommand::Merge { inputs, output } => {
            let (files, merged) = velocity_runner::shared_history::merge_files(
                &inputs,
                std::path::Path::new(&output),
            )?;
            for f in &files {
                println!("  {} {}", "+".green(), f.display());
            }
            println!(
                "{} Merged {} history file(s) into {output}: {}",
                "=>".cyan().bold(),
                files.len(),
                velocity_runner::shared_history::describe(&merged)
            );
            Ok(0)
        }
        HistoryCommand::Flaky {
            file,
            window,
            max_rate,
            json,
        } => {
            let file_path = std::path::Path::new(&file);
            let history = match velocity_runner::history::load_file(file_path) {
                Ok(h) => h,
                Err(e) => return Err(usage_err!("history flaky: {e}")),
            };
            let tests = velocity_runner::history::flaky_rate(&history, window);
            let over: Vec<String> = tests
                .iter()
                .filter(|t| t.runs >= 5 && t.rate > max_rate)
                .map(|t| t.name.clone())
                .collect();
            if json {
                let value = serde_json::json!({
                    "window": window,
                    "maxRate": max_rate,
                    "tests": tests
                        .iter()
                        .map(|t| {
                            serde_json::json!({
                                "name": t.name,
                                "runs": t.runs,
                                "flaky": t.flaky,
                                "rate": t.rate,
                            })
                        })
                        .collect::<Vec<_>>(),
                    "over": over,
                });
                println!("{}", serde_json::to_string(&value).unwrap());
            } else {
                println!("{:<32} {:>4} {:>5} {:>6}", "test", "runs", "flaky", "rate");
                for t in &tests {
                    let flag = if t.runs >= 5 && t.rate > max_rate {
                        "  *"
                    } else {
                        ""
                    };
                    println!(
                        "{:<32} {:>4} {:>5} {:>6.3}{}",
                        t.name, t.runs, t.flaky, t.rate, flag
                    );
                }
                if !over.is_empty() {
                    eprintln!(
                        "{} flaky rate above {max_rate}: {}",
                        "warning:".yellow().bold(),
                        over.join(", ")
                    );
                }
            }
            if over.is_empty() {
                Ok(0)
            } else {
                Ok(1)
            }
        }
    }
}
