//! Maestro flows to Atalanta suites: converts each command to the Atalanta
//! DSL, turns `runFlow` files into the suite's `flows:`, and reports what
//! it couldn't convert, with the line. Internal to the `atalanta` CLI
//! (`atalanta migrate`); its API has no stability promise.
#![forbid(unsafe_code)]

mod compat;
mod maestro;
pub mod waits;

pub use compat::{
    generate_report_json, summarize_dry_run, CommandGap, DryRunSummary, FileMigrationResult,
    MigrationIssue, MigrationReport, Severity,
};
pub use maestro::MaestroMigrator;
