// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta trace view <dir|report>`: open a test's `trace.html`, or an
//! index of every kept trace of a run.

use std::path::{Path, PathBuf};

use atalanta_core::trace_html::{self, IndexEntry};
use clap::Subcommand;
use colored::Colorize;

#[derive(Subcommand)]
pub enum TraceCommand {
    /// Open the trace viewer: a test's trace folder (`traces/<test>`), its
    /// `steps.json` or `trace.html`, a run's artifacts folder or its
    /// `results.json` (an index of every kept trace)
    View {
        /// What to open [default: ./atalanta-results]
        #[arg(default_value = "./atalanta-results")]
        path: String,

        /// Only write and print the page; don't open a browser
        #[arg(long)]
        no_open: bool,
    },
}

pub fn execute(command: TraceCommand) -> anyhow::Result<i32> {
    match command {
        TraceCommand::View { path, no_open } => {
            let page = page_for(Path::new(&path))?;
            println!("{} {}", "=>".cyan().bold(), page.display());
            if !no_open {
                open(&page);
            }
            Ok(0)
        }
    }
}

/// The page to open for `path`, written (or rewritten) first.
fn page_for(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.exists() {
        usage_bail!("{} does not exist", path.display());
    }
    if path.is_file() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.ends_with(".html") {
            return Ok(path.to_path_buf());
        }
        if name.starts_with("steps") && name.ends_with(".json") {
            return Ok(trace_html::write_for_steps(path)?);
        }
        if name.ends_with(".json") {
            return index_from_results(path);
        }
        usage_bail!(
            "{} is not a trace, a trace folder or a results.json",
            path.display()
        );
    }
    // A test's trace folder: its newest attempt.
    if let Some(steps) = newest_steps(path) {
        return Ok(trace_html::write_for_steps(&steps)?);
    }
    let results = path.join("results.json");
    if results.is_file() {
        return index_from_results(&results);
    }
    let traces = path.join("traces");
    if traces.is_dir() {
        return index_from_folders(&traces, &path.join("trace-index.html"));
    }
    usage_bail!(
        "no trace in {}: expected traces/<test>/steps.json or results.json (run with --report json)",
        path.display()
    )
}

/// The most recently written `steps*.json` in `dir`.
fn newest_steps(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy())
                .is_some_and(|n| n.starts_with("steps") && n.ends_with(".json"))
        })
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// An index of the run in `results.json`, written next to it.
fn index_from_results(results: &Path) -> anyhow::Result<PathBuf> {
    let suite = atalanta_runner::reporter::json::parse_results(&std::fs::read_to_string(results)?)
        .map_err(|e| usage_err!("{}: not a results.json: {e}", results.display()))?;
    let dir = results.parent().unwrap_or(Path::new("."));
    let out = dir.join("trace-index.html");
    let mut entries = Vec::new();
    for t in &suite.tests {
        let html = t
            .attachments
            .iter()
            .find(|a| a.kind == atalanta_common::AttachmentKind::Trace)
            .map(|a| a.path.clone())
            .or_else(|| {
                // `results.json` names the trace; write a page only for one
                // inside this artifacts folder (audit 2026-11 TRACE-1).
                let steps = t.trace.as_ref()?;
                let root = std::fs::canonicalize(dir).ok()?;
                let real = [steps.to_path_buf(), dir.join(steps)]
                    .into_iter()
                    .filter_map(|p| std::fs::canonicalize(p).ok())
                    .find(|p| p.starts_with(&root))?;
                trace_html::write_for_steps(&real).ok()
            });
        let Some(html) = html else { continue };
        entries.push(IndexEntry {
            test: t.test_name.clone(),
            status: format!("{:?}", t.status).to_lowercase(),
            href: trace_html::relative(&html, dir),
            error: t
                .error_message
                .clone()
                .map(|e| atalanta_common::redact::redact(&e).into_owned()),
            attachments: t
                .attachments
                .iter()
                .filter(|a| a.kind != atalanta_common::AttachmentKind::Trace)
                .map(|a| (a.title().to_string(), trace_html::relative(&a.path, dir)))
                .collect(),
        });
    }
    if entries.is_empty() {
        usage_bail!("{} lists no test with a kept trace", results.display());
    }
    atalanta_common::fsutil::write_atomic(
        &out,
        trace_html::render_index("Atalanta traces", &entries).as_bytes(),
    )?;
    Ok(out)
}

/// An index of every `traces/<test>/` folder (no results.json).
fn index_from_folders(traces: &Path, out: &Path) -> anyhow::Result<PathBuf> {
    let base = out.parent().unwrap_or(Path::new("."));
    let mut entries = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(traces)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for d in dirs {
        let Some(steps) = newest_steps(&d) else {
            continue;
        };
        let Ok((trace, _)) = atalanta_core::trace::load_trace(&steps) else {
            continue;
        };
        let html = trace_html::write_for_steps(&steps)?;
        let error = trace.steps.iter().rev().find_map(|s| s.error.clone());
        entries.push(IndexEntry {
            test: trace.test.clone(),
            status: format!("{:?}", trace.status).to_lowercase(),
            href: trace_html::relative(&html, base),
            error,
            attachments: trace
                .evidence
                .iter()
                .flat_map(|e| e.video.iter().chain(e.logs.iter()))
                .map(|f| (f.clone(), trace_html::relative(&d.join(f), base)))
                .collect(),
        });
    }
    if entries.is_empty() {
        usage_bail!("no trace in {}", traces.display());
    }
    atalanta_common::fsutil::write_atomic(
        out,
        trace_html::render_index("Atalanta traces", &entries).as_bytes(),
    )?;
    Ok(out.to_path_buf())
}

fn open(page: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener)
        .arg(page)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        eprintln!("could not open a browser ({opener}: {e}); open the file above yourself");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_folder_gets_its_page_and_a_run_its_index() {
        use atalanta_common::{StepResult, StepStatus, TestStatus, TraceMode};
        let root = tempfile::tempdir().unwrap();
        let mut rec = atalanta_core::trace::TraceRecorder::new(TraceMode::OnFailure);
        rec.begin_test();
        rec.begin_step(0, "tap { text: \"Pay\" }".into(), None);
        rec.end_step(
            &StepResult {
                step_index: 0,
                action_name: "tap".into(),
                status: StepStatus::Failed,
                duration: std::time::Duration::from_millis(5),
                screenshot: None,
                error_message: Some("not found".into()),
                resource_delta: None,
                settle_ms: None,
                source: None,
            },
            vec![],
            vec![],
        );
        let steps = rec
            .finish(root.path(), "pay", "d", None, 0, TestStatus::Failed)
            .unwrap();
        let folder = steps.parent().unwrap();
        std::fs::remove_file(folder.join("trace.html")).unwrap();
        let page = page_for(folder).unwrap();
        assert!(page.ends_with("trace.html") && page.exists());
        let index = page_for(root.path()).unwrap();
        let html = std::fs::read_to_string(&index).unwrap();
        assert!(html.contains("traces/pay/trace.html"), "{html}");
        assert!(page_for(&root.path().join("nope")).is_err());
    }
}
