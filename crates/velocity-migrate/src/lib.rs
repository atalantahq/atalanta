//! Maestro flows to Velocity suites: converts each command to the Velocity
//! DSL, turns `runFlow` files into the suite's `flows:`, and reports what
//! it couldn't convert, with the line. Internal to the `velocity` CLI
//! (`velocity migrate`); its API has no stability promise.

mod compat;
mod maestro;
pub mod waits;

pub use compat::{
    generate_report_json, FileMigrationResult, MigrationIssue, MigrationReport, Severity,
};
pub use maestro::MaestroMigrator;
