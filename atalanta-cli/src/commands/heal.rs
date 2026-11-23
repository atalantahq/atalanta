//! `atalanta heal`: review the selectors that self-healing fixed during
//! passing tests, and accept them into the suite or discard them.
//!
//! A heal makes a test pass, but must not stay invisible: this lists each
//! one in the suite's own selector form (`text: "Log in"` → `text: "Sign
//! in"`) and `--accept` rewrites the YAML in place, keeping comments and
//! layout, after checking the result still parses.

use atalanta_core::healing::HealedMapping;
use clap::Args;
use colored::Colorize;

const HEALED_FILE: &str = ".atalanta/healed-selectors.json";

#[derive(Args)]
pub struct HealArgs {
    /// Suite YAML the heals belong to
    #[arg(short, long, default_value = "atalanta.yaml")]
    config: String,

    /// Write the proposed selectors into the suite
    #[arg(long, conflicts_with_all = ["discard", "reject"])]
    accept: bool,

    /// Drop the pending heals without changing the suite
    #[arg(long, conflicts_with = "reject")]
    discard: bool,

    /// Drop one pending heal and never propose it again: its number in
    /// the listing (1, 2, …) or its original selector (`text="Log in"` or
    /// `text: "Log in"`); `all` rejects every pending heal. The (selector,
    /// heal) pair goes into `.atalanta/heal-rejected.json`, where runs from
    /// this directory read it
    #[arg(long, value_name = "N|SELECTOR|all")]
    reject: Option<String>,

    /// Healed-selector log written by the last run
    #[arg(long, default_value = HEALED_FILE)]
    file: String,
}

pub fn execute(args: HealArgs) -> anyhow::Result<i32> {
    let mappings: Vec<HealedMapping> = match std::fs::read_to_string(&args.file) {
        Ok(text) => serde_json::from_str(&text)?,
        Err(_) => {
            println!(
                "{} No healed selectors to review ({} not found)",
                "=>".cyan().bold(),
                args.file
            );
            return Ok(0);
        }
    };
    if let Some(which) = &args.reject {
        // Where the healer reads it (m6: it was written beside `--file`,
        // which the healer never looks at).
        return reject(&args.file, mappings, which, &rejected_path());
    }
    if args.discard {
        std::fs::remove_file(&args.file)?;
        println!(
            "{} Discarded {} healed selector(s)",
            "=>".cyan().bold(),
            mappings.len()
        );
        return Ok(0);
    }

    let mut yaml =
        std::fs::read_to_string(&args.config).map_err(|e| usage_err!("{}: {e}", args.config))?;
    // Only the lines of steps are edited: a flow `id:`, a test name or any
    // other `key: value` equal to the old value stays as it is (VEL-032).
    let in_steps = step_lines(&yaml)?;
    let mut applied = 0;
    let mut manual = 0;
    for (i, m) in unique(&mappings).into_iter().enumerate() {
        let no = i + 1;
        let test = m.test.as_deref().unwrap_or("?");
        match m.rewrite() {
            Some((key, old, new)) => {
                let (updated, n) = replace_in_steps(&yaml, &in_steps, &key, &old, &new);
                println!(
                    "  {no}. {} {key}: {old:?} → {key}: {new:?}  ({:.0}% match, test \"{test}\"{})",
                    if n > 0 { "✓".green() } else { "?".yellow() },
                    m.confidence * 100.0,
                    if n == 0 {
                        ", not found in the suite".to_string()
                    } else if n > 1 {
                        format!(", {n} places")
                    } else {
                        String::new()
                    }
                );
                if n > 0 {
                    yaml = updated;
                    applied += 1;
                }
            }
            None => {
                manual += 1;
                println!(
                    "  {no}. {} {} → {}  ({:.0}% match, test \"{test}\"): edit by hand",
                    "?".yellow(),
                    m.original_selector,
                    m.healed_selector,
                    m.confidence * 100.0
                );
            }
        }
    }

    if !args.accept {
        println!(
            "\n{} {applied} can be applied, {manual} need a manual edit. \
             Run with --accept to write them, --discard to drop them, or --reject <N> to drop \
             one for good.",
            "=>".cyan().bold()
        );
        return Ok(0);
    }
    if applied == 0 {
        println!("{} Nothing to write", "=>".cyan().bold());
        return Ok(0);
    }
    check_rewritten(&yaml)?;
    std::fs::write(&args.config, yaml)?;
    std::fs::remove_file(&args.file)?;
    println!(
        "{} Wrote {applied} healed selector(s) to {}",
        "=>".cyan().bold(),
        args.config
    );
    Ok(0)
}

/// The denylist, at the one path the healer reads
/// (`atalanta_core::healing::REJECTED_FILE`, relative to where runs start).
fn rejected_path() -> std::path::PathBuf {
    std::path::PathBuf::from(atalanta_core::healing::REJECTED_FILE)
}

/// `--reject <which>`: move the named heals from the log at `file` to the
/// denylist at `denylist`; the others stay pending.
fn reject(
    file: &str,
    mappings: Vec<HealedMapping>,
    which: &str,
    denylist: &std::path::Path,
) -> anyhow::Result<i32> {
    let pairs = pick(&mappings, which)?;
    let (gone, kept): (Vec<HealedMapping>, Vec<HealedMapping>) = mappings
        .into_iter()
        .partition(|m| pairs.contains(&(m.original_selector.clone(), m.healed_selector.clone())));
    let added = atalanta_core::healing::reject_heals(denylist, &gone)?;
    if kept.is_empty() {
        std::fs::remove_file(file)?;
    } else {
        std::fs::write(file, serde_json::to_string_pretty(&kept)?)?;
    }
    for (from, to) in &pairs {
        println!("  {} {from} → {to}", "✗".red());
    }
    println!(
        "{} Rejected {} healed selector(s) ({added} new in {}); they won't be proposed again. \
         {} still pending",
        "=>".cyan().bold(),
        pairs.len(),
        denylist.display(),
        kept.len()
    );
    Ok(0)
}

/// The pending heals once each, in log order: the listing's numbering.
fn unique(mappings: &[HealedMapping]) -> Vec<&HealedMapping> {
    let mut seen = std::collections::HashSet::new();
    mappings
        .iter()
        .filter(|m| seen.insert((m.original_selector.clone(), m.healed_selector.clone())))
        .collect()
}

/// The (selector, heal) pairs `--reject <which>` names: a listing number,
/// an original selector in either form, or `all`.
fn pick(mappings: &[HealedMapping], which: &str) -> anyhow::Result<Vec<(String, String)>> {
    let which = which.trim();
    let list = unique(mappings);
    let pair = |m: &HealedMapping| (m.original_selector.clone(), m.healed_selector.clone());
    if which == "all" {
        return Ok(list.into_iter().map(pair).collect());
    }
    if let Ok(n) = which.parse::<usize>() {
        return match list.get(n.wrapping_sub(1)) {
            Some(m) => Ok(vec![pair(m)]),
            None => anyhow::bail!(
                "--reject {n}: there are {} pending heal(s); run `atalanta heal` to list them",
                list.len()
            ),
        };
    }
    let named: Vec<_> =
        list.into_iter()
            .filter(|m| {
                m.original_selector == which
                    || atalanta_core::healing::parse_simple_selector(&m.original_selector)
                        .is_some_and(|(key, old)| {
                            which == format!("{key}: {old:?}") || which == format!("{key}: {old}")
                        })
            })
            .map(pair)
            .collect();
    if named.is_empty() {
        anyhow::bail!(
            "--reject {which:?}: no pending heal of that selector; run `atalanta heal` to list \
             them, and reject one by its number"
        );
    }
    Ok(named)
}

/// The rewritten suite must still parse, validate and resolve its flows;
/// otherwise nothing is written and the heal log is kept.
fn check_rewritten(yaml: &str) -> anyhow::Result<()> {
    let suite = atalanta_core::parser::parse_suite_from_str(yaml)
        .and_then(|suite| atalanta_core::validate_suite(&suite).map(|()| suite))
        .and_then(|suite| atalanta_core::resolve_flows(&suite))
        .map_err(|e| {
            anyhow::anyhow!("the rewritten suite would not be valid; nothing written: {e}")
        });
    suite.map(|_| ())
}

/// For each line of `yaml` (0-based), whether it belongs to a step of a test
/// or a flow: the step's own line and the lines indented deeper than it
/// below it.
fn step_lines(yaml: &str) -> anyhow::Result<Vec<bool>> {
    let suite = atalanta_core::parser::parse_suite_from_str(yaml)?;
    let lines: Vec<&str> = yaml.lines().collect();
    let mut mask = vec![false; lines.len()];
    let indent = |l: &str| l.len() - l.trim_start().len();
    let steps = suite
        .tests
        .iter()
        .flat_map(|t| &t.steps)
        .chain(suite.flows.iter().flat_map(|f| &f.steps));
    for step in steps {
        let Some(first) = step.source.as_ref().map(|s| s.line.saturating_sub(1)) else {
            continue;
        };
        let Some(line) = lines.get(first) else {
            continue;
        };
        let base = indent(line);
        mask[first] = true;
        for (i, l) in lines.iter().enumerate().skip(first + 1) {
            if !l.trim().is_empty() && indent(l) <= base {
                break;
            }
            mask[i] = true;
        }
    }
    Ok(mask)
}

/// `replace_selector_value` on the lines `in_steps` marks only.
fn replace_in_steps(
    yaml: &str,
    in_steps: &[bool],
    key: &str,
    old: &str,
    new: &str,
) -> (String, usize) {
    let mut out = String::with_capacity(yaml.len());
    let mut count = 0;
    for (i, line) in yaml.split_inclusive('\n').enumerate() {
        if in_steps.get(i).copied().unwrap_or(false) {
            let (l, n) = replace_selector_value(line, key, old, new);
            out.push_str(&l);
            count += n;
        } else {
            out.push_str(line);
        }
    }
    (out, count)
}

/// Replace `key: old` with `key: "new"` wherever `old` is written quoted
/// (either quote) or bare. Returns the text and the number of replacements.
fn replace_selector_value(yaml: &str, key: &str, old: &str, new: &str) -> (String, usize) {
    let replacement = format!("{key}: {}", quote(new));
    let mut out = yaml.to_string();
    let mut count = 0;
    for written in [
        format!("{key}: {}", quote(old)),
        format!("{key}: '{}'", old.replace('\'', "''")),
    ] {
        count += out.matches(&written).count();
        out = out.replace(&written, &replacement);
    }
    // Bare scalars end at `,`, `}` or the end of the line.
    let bare = format!("{key}: {old}");
    let mut result = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find(&bare) {
        let after = &rest[at + bare.len()..];
        let ends = after.is_empty()
            || after.starts_with([',', '}', '\n', '\r'])
            || after
                .trim_start_matches(' ')
                .starts_with(['#', '}', ',', '\n']);
        result.push_str(&rest[..at]);
        if ends {
            result.push_str(&replacement);
            count += 1;
        } else {
            result.push_str(&bare);
        }
        rest = after;
    }
    result.push_str(rest);
    (result, count)
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--config` naming a suite that is not there is a usage error (2),
    /// not an internal one (5).
    #[test]
    fn a_missing_suite_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("healed.json");
        std::fs::write(&log, "[]").unwrap();
        let e = execute(HealArgs {
            config: dir.path().join("nope.yaml").display().to_string(),
            accept: false,
            discard: false,
            reject: None,
            file: log.display().to_string(),
        })
        .unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2, "{e}");
    }

    #[test]
    fn replaces_quoted_and_bare_forms_only_as_whole_values() {
        let yaml = "\
steps:
  - tap: { text: \"Log in\" }   # entry
  - tap: { text: 'Log in' }
  - assertVisible:
      text: Log in
  - tap: { text: \"Log in now\" }
";
        let (out, n) = replace_selector_value(yaml, "text", "Log in", "Sign in");
        assert_eq!(n, 3, "{out}");
        assert!(out.contains("{ text: \"Sign in\" }   # entry"));
        assert!(out.contains("      text: \"Sign in\"\n"));
        assert!(
            out.contains("text: \"Log in now\""),
            "a longer value is left alone"
        );
    }

    /// D-7: `--reject <which>` turns one pending heal into a denylist entry
    /// the healer reads, and keeps the others pending (m6).
    #[test]
    fn reject_denies_only_the_named_heal() {
        let dir = std::env::temp_dir().join(format!("atalanta-heal-reject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("healed-selectors.json");
        let heal = |from: &str, to: &str| {
            format!(
                r#"{{"original_selector":"text=\"{from}\"","healed_selector":"text=\"{to}\"",
                "confidence":0.9,"matched_attributes":{{"platform_id":"b","label":null,
                "text":"{to}","element_type":"Button","bounds":[0,0,1,1]}},
                "healed_at":"now","test":"t"}}"#
            )
        };
        let text = format!(
            "[{},{},{}]",
            heal("Log in", "Sign in"),
            heal("Pay", "Pay now"),
            heal("Log in", "Sign in")
        );
        std::fs::write(&log, &text).unwrap();
        let mappings = |p: &std::path::Path| -> Vec<HealedMapping> {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let deny = dir.join("deny.json");
        let file = log.to_string_lossy().into_owned();
        // By number: the second in the listing (duplicates listed once).
        reject(&file, mappings(&log), "2", &deny).unwrap();
        let denied = atalanta_core::healing::load_rejected(&deny);
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].original_selector, r#"text="Pay""#);
        assert_eq!(mappings(&log).len(), 2, "the others stay pending");
        // By selector, in the suite's form: both copies go.
        assert!(reject(&file, mappings(&log), "text: \"Nope\"", &deny).is_err());
        reject(&file, mappings(&log), "text: \"Log in\"", &deny).unwrap();
        assert!(!log.exists(), "nothing left pending");
        assert_eq!(atalanta_core::healing::load_rejected(&deny).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
        // Always where the healer reads it, whatever `--file` says.
        assert_eq!(
            rejected_path(),
            std::path::PathBuf::from(atalanta_core::healing::REJECTED_FILE)
        );
    }

    /// VEL-032: an `id` heal `login` -> `login_button` must not rename the
    /// flow `login` (or anything else outside steps), and the result must
    /// still validate.
    #[test]
    fn accept_edits_only_steps_and_leaves_flow_ids_alone() {
        let yaml = "\
appId: com.example.app
flows:
  - id: login
    steps:
      - tap: { id: login }
tests:
  - name: t
    steps:
      - runFlow: login
      - assertVisible:
          id: login
";
        let mask = step_lines(yaml).unwrap();
        let (out, n) = replace_in_steps(yaml, &mask, "id", "login", "login_button");
        assert_eq!(n, 2, "{out}");
        assert!(out.contains("  - id: login\n"), "flow id renamed:\n{out}");
        assert!(out.contains("- tap: { id: \"login_button\" }"), "{out}");
        assert!(out.contains("          id: \"login_button\"\n"), "{out}");
        assert!(out.contains("- runFlow: login"), "{out}");
        check_rewritten(&out).unwrap();
        // The old whole-file replace renamed the flow and broke the suite.
        let (broken, _) = replace_selector_value(yaml, "id", "login", "login_button");
        assert!(check_rewritten(&broken).is_err());
    }
}
