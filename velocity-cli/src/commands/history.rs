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
    }
}
