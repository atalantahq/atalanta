use clap::Args;
use colored::Colorize;
use velocity_core::{parse_suite_with_positions, resolve_flows, validate_suite_at};

#[derive(Args)]
pub struct ValidateArgs {
    /// Suite YAML to validate, or a folder: every *.yaml / *.yml in it
    config: String,
}

pub fn execute(args: ValidateArgs) -> anyhow::Result<i32> {
    let path = std::path::Path::new(&args.config);
    if !path.is_dir() {
        return validate_one(&args.config);
    }
    let files = suite_files(path)?;
    if files.is_empty() {
        println!(
            "  {} no *.yaml or *.yml files in {}",
            "✗".red().bold(),
            args.config
        );
        return Ok(2);
    }
    let mut bad = Vec::new();
    for f in &files {
        let f = f.to_string_lossy().to_string();
        if validate_one(&f)? != 0 {
            bad.push(f);
        }
    }
    println!(
        "{} {} of {} valid",
        "=>".cyan().bold(),
        files.len() - bad.len(),
        files.len()
    );
    for f in &bad {
        println!("  {} {f}", "✗".red().bold());
    }
    Ok(if bad.is_empty() { 0 } else { 2 })
}

/// The suite files directly in `dir`, sorted (subfolders hold flows and
/// fixtures, not suites, so they are not walked).
fn suite_files(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && velocity_core::is_suite_file(p))
        .collect();
    files.sort();
    Ok(files)
}

fn validate_one(config: &str) -> anyhow::Result<i32> {
    println!("{} Validating {}", "=>".cyan().bold(), config.bold());

    let (suite, tests) = match parse_suite_with_positions(config) {
        Ok(s) => s,
        Err(e) => {
            println!();
            println!(
                "  {} Parse error: {}",
                "✗".red().bold(),
                super::load_error(config, &e)
            );
            return Ok(2);
        }
    };

    if let Err(e) = validate_suite_at(&suite, &tests) {
        println!();
        println!("  {} Validation error: {e}", "✗".red().bold());
        return Ok(2);
    }

    if let Err(e) = resolve_flows(&suite) {
        println!();
        println!("  {} Flow resolution error: {e}", "✗".red().bold());
        return Ok(2);
    }

    println!();
    println!("  {} Configuration is valid", "✓".green().bold());
    print_unknown_config_keys(config);
    print_suite_warnings(&suite, config);
    println!();
    let http_only = velocity_core::http_only(&suite);
    if !http_only {
        println!("  App ID:    {}", suite.app_id.cyan());
    }
    let platform = match suite.config.platform {
        _ if http_only => "none (http steps only)".to_string(),
        Some(p) => p.to_string(),
        None => "auto-detect".to_string(),
    };
    println!("  Platform:  {}", platform.cyan());
    println!("  Flows:     {}", suite.flows.len());
    println!("  Tests:     {}", suite.tests.len());

    let total_steps: usize = suite.tests.iter().map(|t| t.steps.len()).sum();
    println!("  Steps:     {total_steps}");

    let all_tags: Vec<&str> = suite
        .tests
        .iter()
        .flat_map(|t| t.tags.iter().map(|s| s.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !all_tags.is_empty() {
        println!("  Tags:      {}", all_tags.join(", ").dimmed());
    }

    println!();
    Ok(0)
}

/// Warn about what validates but is almost certainly a mistake (a test
/// with no steps, a swipe without a direction).
pub fn print_suite_warnings(suite: &velocity_common::TestSuite, path: &str) {
    for w in velocity_core::validator::suite_warnings(suite) {
        println!("  {} {path}: {w}", "warning:".yellow().bold());
    }
}

/// Warn about suite, test, flow and `config:` keys nothing reads (serde
/// ignores them).
pub fn print_unknown_config_keys(path: &str) {
    let Ok(yaml) = std::fs::read_to_string(path) else {
        return;
    };
    for w in velocity_core::config_keys::unknown_keys(&yaml) {
        println!("  {} {path}: {w}", "warning:".yellow().bold());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_yields_its_yaml_files_sorted_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("vel-validate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("flows")).unwrap();
        for f in ["b.yaml", "a.yml", "notes.md", "flows/login.yaml"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names: Vec<String> = suite_files(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, ["a.yml", "b.yaml"]);
    }
}
