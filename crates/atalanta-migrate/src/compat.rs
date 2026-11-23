use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationIssue {
    pub line: usize,
    pub severity: Severity,
    pub message: String,
    pub maestro_construct: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMigrationResult {
    pub source_file: String,
    pub output_file: Option<String>,
    pub success: bool,
    pub issues: Vec<MigrationIssue>,
    pub steps_migrated: usize,
    pub steps_skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub files_total: usize,
    pub files_migrated: usize,
    pub files_failed: usize,
    pub total_warnings: usize,
    pub total_errors: usize,
    pub results: Vec<FileMigrationResult>,
}

impl MigrationReport {
    pub fn new() -> Self {
        Self {
            files_total: 0,
            files_migrated: 0,
            files_failed: 0,
            total_warnings: 0,
            total_errors: 0,
            results: Vec::new(),
        }
    }

    pub fn add_result(&mut self, result: FileMigrationResult) {
        self.files_total += 1;
        if result.success {
            self.files_migrated += 1;
        } else {
            self.files_failed += 1;
        }
        for issue in &result.issues {
            match issue.severity {
                Severity::Warning => self.total_warnings += 1,
                Severity::Error => self.total_errors += 1,
                Severity::Info => {}
            }
        }
        self.results.push(result);
    }
}

impl Default for MigrationReport {
    fn default() -> Self {
        Self::new()
    }
}

pub fn generate_report_json(report: &MigrationReport) -> String {
    serde_json::to_string_pretty(report)
        .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize report: {e}\"}}"))
}

/// One Maestro command that didn't convert cleanly, across a folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandGap {
    /// The Maestro command (or header key).
    pub command: String,
    /// Flows that use it in a way that didn't convert cleanly.
    pub files: usize,
    /// Times it occurs.
    pub occurrences: usize,
    /// Whether any occurrence stops a flow from converting at all.
    pub blocks: bool,
    /// The first message, as an example.
    pub example: String,
}

/// `migrate maestro --dry-run`: what a folder's migration would hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunSummary {
    pub files_total: usize,
    /// Flows that would convert with no warning.
    pub files_clean: usize,
    /// Flows that would convert, with steps skipped or changed.
    pub files_lossy: usize,
    /// Flows that would not convert.
    pub files_failed: usize,
    /// Commands by how many flows they affect, most first.
    pub gaps: Vec<CommandGap>,
}

/// Rank the warnings and errors in `report` by command.
pub fn summarize_dry_run(report: &MigrationReport) -> DryRunSummary {
    use std::collections::BTreeMap;
    let mut by: BTreeMap<String, CommandGap> = BTreeMap::new();
    let (mut clean, mut lossy) = (0, 0);
    for r in &report.results {
        let mut seen = std::collections::HashSet::new();
        let mut warned = false;
        for i in r.issues.iter().filter(|i| i.severity != Severity::Info) {
            warned = true;
            let g = by
                .entry(i.maestro_construct.clone())
                .or_insert_with(|| CommandGap {
                    command: i.maestro_construct.clone(),
                    files: 0,
                    occurrences: 0,
                    blocks: false,
                    example: i.message.clone(),
                });
            g.occurrences += 1;
            g.blocks |= i.severity == Severity::Error;
            if seen.insert(i.maestro_construct.clone()) {
                g.files += 1;
            }
        }
        match (r.success, warned) {
            (true, false) => clean += 1,
            (true, true) => lossy += 1,
            _ => {}
        }
    }
    let mut gaps: Vec<CommandGap> = by.into_values().collect();
    gaps.sort_by(|a, b| {
        (b.files, b.blocks, b.occurrences, &a.command).cmp(&(
            a.files,
            a.blocks,
            a.occurrences,
            &b.command,
        ))
    });
    DryRunSummary {
        files_total: report.files_total,
        files_clean: clean,
        files_lossy: lossy,
        files_failed: report.files_failed,
        gaps,
    }
}

#[cfg(test)]
mod dry_run_tests {
    use super::*;

    fn file(ok: bool, issues: &[(&str, Severity)]) -> FileMigrationResult {
        FileMigrationResult {
            source_file: "f.yaml".into(),
            output_file: None,
            success: ok,
            issues: issues
                .iter()
                .map(|(c, s)| MigrationIssue {
                    line: 1,
                    severity: *s,
                    message: format!("{c} skipped"),
                    maestro_construct: c.to_string(),
                })
                .collect(),
            steps_migrated: 1,
            steps_skipped: 0,
        }
    }

    #[test]
    fn gaps_are_ranked_by_the_flows_they_affect() {
        let mut r = MigrationReport::new();
        r.add_result(file(true, &[]));
        r.add_result(file(true, &[("hideKeyboard", Severity::Info)]));
        r.add_result(file(
            true,
            &[
                ("runScript", Severity::Warning),
                ("runScript", Severity::Warning),
            ],
        ));
        r.add_result(file(
            true,
            &[
                ("runScript", Severity::Warning),
                ("setLocation", Severity::Warning),
            ],
        ));
        r.add_result(file(false, &[("output", Severity::Error)]));
        let s = summarize_dry_run(&r);
        assert_eq!(
            (s.files_total, s.files_clean, s.files_lossy, s.files_failed),
            (5, 2, 2, 1)
        );
        let order: Vec<(&str, usize, usize)> = s
            .gaps
            .iter()
            .map(|g| (g.command.as_str(), g.files, g.occurrences))
            .collect();
        assert_eq!(
            order,
            [("runScript", 2, 3), ("output", 1, 1), ("setLocation", 1, 1)]
        );
        assert!(s.gaps[1].blocks && !s.gaps[0].blocks);
    }
}
