use clap::Subcommand;
use colored::Colorize;
use velocity_migrate::{MaestroMigrator, Severity};

#[derive(Subcommand)]
pub enum MigrateCommand {
    /// Migrate Maestro YAML tests to Velocity format
    Maestro {
        /// Input directory containing Maestro YAML files
        dir: String,

        /// Output directory for Velocity YAML files
        #[arg(short, long, default_value = "./velocity-migrated")]
        output: String,

        /// Output migration report as JSON
        #[arg(long)]
        report_json: bool,

        /// Write nothing: convert every flow and report which Maestro
        /// commands don't convert cleanly, ranked by how many flows they
        /// affect (with --report-json, as JSON)
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove fixed `wait:` steps that sit before a step which already waits
    /// for its element. Prints what it would remove; `--write` edits the file
    Waits {
        /// Suite YAML to rewrite
        file: String,

        /// Rewrite the file in place
        #[arg(long)]
        write: bool,
    },
}

pub fn execute(command: MigrateCommand) -> anyhow::Result<i32> {
    match command {
        MigrateCommand::Waits { file, write } => {
            let yaml = std::fs::read_to_string(&file).map_err(|e| usage_err!("{file}: {e}"))?;
            let (out, removed) = velocity_migrate::waits::remove_redundant_waits(&yaml);
            if removed.is_empty() {
                println!("{} No redundant waits in {file}", "=>".cyan().bold());
                return Ok(0);
            }
            let total: u64 = removed.iter().map(|r| r.ms).sum();
            for r in &removed {
                println!("  {file}:{}  wait {} ms before {}", r.line, r.ms, r.before);
            }
            // Never write a file that no longer parses.
            velocity_core::parser::parse_suite_from_str(&out)
                .map_err(|e| anyhow::anyhow!("rewrite would not parse, nothing written: {e}"))?;
            if write {
                std::fs::write(&file, out)?;
                println!(
                    "{} Removed {} waits ({:.1} s per full run) from {file}",
                    "=>".cyan().bold(),
                    removed.len(),
                    total as f64 / 1000.0
                );
            } else {
                println!(
                    "{} {} redundant waits ({:.1} s per full run); rerun with --write to remove them",
                    "=>".cyan().bold(),
                    removed.len(),
                    total as f64 / 1000.0
                );
            }
            Ok(0)
        }
        MigrateCommand::Maestro {
            dir,
            output: _,
            report_json,
            dry_run: true,
        } => {
            let report = MaestroMigrator::new().dry_run_directory(&dir)?;
            let summary = velocity_migrate::summarize_dry_run(&report);
            if report_json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
                return Ok(0);
            }
            println!(
                "{} Dry run of {}: nothing written",
                "=>".cyan().bold(),
                dir.bold()
            );
            println!(
                "  {} flow(s): {} convert cleanly, {} with steps skipped or changed, {} would not convert",
                summary.files_total,
                summary.files_clean.to_string().green(),
                summary.files_lossy.to_string().yellow(),
                summary.files_failed.to_string().red()
            );
            if summary.gaps.is_empty() {
                println!("  Every command converts.");
            } else {
                println!(
                    "\n  {:<28} {:>6} {:>6}  example",
                    "command", "flows", "times"
                );
                for g in &summary.gaps {
                    let cmd = if g.blocks {
                        format!("{} (blocks)", g.command).red().to_string()
                    } else {
                        g.command.clone()
                    };
                    println!(
                        "  {cmd:<28} {:>6} {:>6}  {}",
                        g.files,
                        g.occurrences,
                        g.example.chars().take(90).collect::<String>().dimmed()
                    );
                }
            }
            println!(
                "\n  Run without --dry-run to write the converted suites (each file's issues are listed then)."
            );
            Ok(0)
        }
        MigrateCommand::Maestro {
            dir,
            output,
            report_json,
            dry_run: false,
        } => {
            println!(
                "{} Migrating Maestro tests from {}",
                "=>".cyan().bold(),
                dir.bold()
            );

            let migrator = MaestroMigrator::new();
            let report = migrator.migrate_directory(&dir, &output)?;

            if report_json {
                let json = velocity_migrate::generate_report_json(&report);
                println!("{json}");
                return Ok(if report.files_failed > 0 { 1 } else { 0 });
            }

            println!();
            println!("{}", "━".repeat(60).dimmed());
            println!(
                "  {} {} total, {} migrated, {} failed",
                "Migration Results:".bold(),
                report.files_total,
                report.files_migrated.to_string().green().bold(),
                report.files_failed.to_string().red().bold()
            );
            println!("{}", "━".repeat(60).dimmed());

            for result in &report.results {
                let icon = if result.success {
                    "✓".green()
                } else {
                    "✗".red()
                };
                println!(
                    "  {icon} {} ({} steps migrated, {} skipped)",
                    result.source_file, result.steps_migrated, result.steps_skipped
                );

                for issue in &result.issues {
                    let severity_str = match issue.severity {
                        Severity::Info => "info".dimmed(),
                        Severity::Warning => "warn".yellow(),
                        Severity::Error => "error".red(),
                    };
                    println!(
                        "    {} [line {}] {}: {}",
                        "↳".dimmed(),
                        issue.line,
                        severity_str,
                        issue.message
                    );
                }
            }

            if report.total_warnings > 0 {
                println!();
                println!(
                    "  {} {} warnings — review migrated files for TODO items",
                    "⚠".yellow(),
                    report.total_warnings
                );
            }

            if report.files_migrated > 0 {
                println!();
                println!(
                    "{} Output written to {}",
                    "=>".green().bold(),
                    output.underline()
                );
            }

            println!();
            Ok(if report.files_failed > 0 { 1 } else { 0 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A suite file that is not there is a usage error (2) like everywhere
    /// else a file is named, not an internal one (5).
    #[test]
    fn a_missing_suite_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let e = execute(MigrateCommand::Waits {
            file: dir.path().join("nope.yaml").display().to_string(),
            write: false,
        })
        .unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2, "{e}");
    }
}
