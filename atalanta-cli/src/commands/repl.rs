//! `velocity repl`: run steps one at a time against a device.
//!
//! Each line is a step in suite syntax (`tap: { text: "Log in" }`) or plain
//! English (with a planner configured). Commands start with `:`:
//! `:tree` prints the screen's elements, `:find <selector>` shows what a
//! selector matches, `:shot <file>` saves a screenshot, `:save <file>`
//! writes the steps that passed as a suite, `:quit` leaves.

use std::io::Write;
use std::sync::Arc;

use clap::Args;
use colored::Colorize;
use tokio::io::{AsyncBufReadExt, BufReader};
use velocity_common::{
    Element, Platform, PlatformDriver, ResilientDriver, Selector, StepStatus, SuiteConfig,
};
use velocity_core::TestExecutor;

#[derive(Args)]
pub struct ReplArgs {
    /// Device to drive
    #[arg(short, long)]
    device: String,

    /// ios or android (detected from the device id if omitted)
    #[arg(short, long)]
    platform: Option<String>,

    /// App the steps act on (launchApp, natural steps)
    #[arg(long, default_value = "")]
    app: String,
}

pub async fn execute(args: ReplArgs) -> anyhow::Result<i32> {
    let platform = match args.platform.as_deref() {
        Some("ios") => Platform::Ios,
        Some("android") => Platform::Android,
        Some(other) => usage_bail!("unknown platform {other}"),
        None => Platform::of_device_id(&args.device),
    };
    let raw: Arc<dyn PlatformDriver> = Arc::from(super::create_driver(platform));
    let driver = ResilientDriver::new(raw);
    driver.prepare(&args.device).await?;
    let config = SuiteConfig {
        platform: Some(platform),
        ..Default::default()
    };
    let mut executor = TestExecutor::new(&driver, config, &args.app);
    if let Some(resolver) = velocity_runner::nl::build_resolver()? {
        executor = executor.with_planner(resolver);
    }

    println!(
        "{} velocity repl on {} ({platform}). Steps in suite syntax or plain English; :help for commands.",
        "=>".cyan().bold(),
        args.device
    );
    let mut passed: Vec<String> = Vec::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut index = 0;
    loop {
        print!("velocity> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix(':') {
            let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
            match name {
                "q" | "quit" | "exit" => break,
                "help" | "h" => println!(
                    "  :tree             elements on screen\n  :find <selector>  e.g. :find {{ text: \"Pay\" }}\n  :shot <file.png>  screenshot\n  :save <file.yaml> passed steps as a suite\n  :quit"
                ),
                "tree" => match driver.get_hierarchy(&args.device).await {
                    Ok(tree) => print_tree(&tree, 0),
                    Err(e) => println!("  {} {e}", "error:".red()),
                },
                "find" => match parse_selector(rest) {
                    Ok(sel) => match driver.get_hierarchy(&args.device).await {
                        Ok(tree) => {
                            let found = velocity_common::selector_match::find_all_ref(&tree, &sel, None);
                            println!("  {} match(es) for {sel}", found.len());
                            for e in found {
                                println!("    {}", describe(e));
                            }
                        }
                        Err(e) => println!("  {} {e}", "error:".red()),
                    },
                    Err(e) => println!("  {} {e}", "error:".red()),
                },
                "shot" => {
                    let path = if rest.is_empty() { "repl.png" } else { rest };
                    match driver.screenshot(&args.device).await {
                        Ok(png) => {
                            if write_reporting(path, &png) {
                                println!("  saved {path}");
                            }
                        }
                        Err(e) => println!("  {} {e}", "error:".red()),
                    }
                }
                "save" => {
                    let path = if rest.is_empty() { "repl.yaml" } else { rest };
                    if write_reporting(path, suite_yaml(&args.app, &passed).as_bytes()) {
                        println!("  wrote {} step(s) to {path}", passed.len());
                    }
                }
                other => println!("  unknown command :{other} (:help)"),
            }
            continue;
        }
        let step = match parse_step(line) {
            Ok(s) => s,
            Err(e) => {
                println!("  {} {e}", "parse error:".red());
                continue;
            }
        };
        let started = std::time::Instant::now();
        match Box::pin(executor.execute_step(&step, &args.device, &args.app, index)).await {
            Ok(r) if r.status == StepStatus::Passed => {
                println!(
                    "  {} {} ({} ms{})",
                    "ok".green(),
                    r.action_name,
                    started.elapsed().as_millis(),
                    r.settle_ms
                        .map(|s| format!(", settle {s} ms"))
                        .unwrap_or_default()
                );
                passed.push(line.to_string());
                index += 1;
            }
            Ok(r) => println!(
                "  {} {}",
                "failed:".red(),
                r.error_message.unwrap_or_default()
            ),
            Err(e) => println!("  {} {e}", "failed:".red()),
        }
    }
    driver.cleanup().await;
    Ok(0)
}

/// Write a `:shot`/`:save` file. A failed write is printed and the session
/// goes on: it used to end the REPL and lose the recorded steps (VEL-040).
fn write_reporting(path: &str, contents: &[u8]) -> bool {
    match std::fs::write(path, contents) {
        Ok(()) => true,
        Err(e) => {
            println!("  {} can't write {path}: {e}", "error:".red());
            false
        }
    }
}

/// Whether a line is a step in suite syntax: the text before its first `:`
/// names an action. Anything else, colons and all, is plain English
/// ("Check the total: 12.50"), not YAML (VEL-169).
fn is_suite_syntax(line: &str) -> bool {
    if line.starts_with('"') {
        return false;
    }
    // Flow style: `{ tap: { id: x } }` (m-7).
    if line.trim_start().starts_with('{') {
        return true;
    }
    line.split_once(':')
        .is_some_and(|(key, _)| velocity_core::schema::ACTIONS.contains(&key.trim()))
}

/// The action a misspelt key was meant to be: `tpa: Login` is a typo, not
/// English for the natural-language planner (m-7). Only a lowercase-first
/// identifier (actions are camelCase; English starts with a capital) close
/// to an action: within 1 edit (a swap counts as one) for keys of up to 4
/// letters, else 2.
fn misspelt_action(line: &str) -> Option<&'static str> {
    let (key, _) = line.split_once(':')?;
    let key = key.trim();
    let mut chars = key.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase())
        || !chars.all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let limit = if key.len() <= 4 { 1 } else { 2 };
    velocity_core::schema::ACTIONS
        .iter()
        .map(|a| (edit_distance(&key.to_lowercase(), &a.to_lowercase()), *a))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, a)| a)
}

/// Edits (insert, delete, substitute, swap two neighbours) from `a` to `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    d[0] = (0..=b.len()).collect();
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

/// `text` as one YAML scalar, escaped by the YAML emitter. Rust's `{:?}`
/// writes a combining accent or a zero-width space as `\u{301}`, which YAML
/// refuses.
fn yaml_scalar(text: &str) -> String {
    serde_yaml::to_string(text)
        .map(|y| y.trim_end().to_string())
        .unwrap_or_else(|_| format!("{text:?}"))
}

/// One step line, parsed with the suite parser (so the syntax is the same).
fn parse_step(line: &str) -> anyhow::Result<velocity_common::Step> {
    if !is_suite_syntax(line) {
        if let Some(action) = misspelt_action(line) {
            let key = line.split_once(':').map_or("", |(k, _)| k.trim());
            anyhow::bail!("unknown action `{key}` (did you mean `{action}`?)");
        }
    }
    let item = if is_suite_syntax(line) {
        line.to_string()
    } else {
        // Plain English.
        yaml_scalar(line.trim_matches('"'))
    };
    let yaml = format!("appId: repl\ntests:\n  - name: repl\n    steps:\n      - {item}\n");
    let mut suite = velocity_core::parser::parse_suite_from_str(&yaml)?;
    let mut step = suite.tests.remove(0).steps.remove(0);
    // Its line is in the YAML made up above, not in anything the user wrote.
    step.source = None;
    Ok(step)
}

fn parse_selector(text: &str) -> anyhow::Result<Selector> {
    match parse_step(&format!("tap: {text}"))?.action {
        velocity_common::Action::Tap { selector } => Ok(selector),
        _ => anyhow::bail!("not a selector"),
    }
}

fn suite_yaml(app: &str, steps: &[String]) -> String {
    let mut out = format!(
        "appId: {}\ntests:\n  - name: \"recorded in repl\"\n    steps:\n",
        if app.is_empty() {
            "com.example.app"
        } else {
            app
        }
    );
    for s in steps {
        let item = if is_suite_syntax(s) {
            s.clone()
        } else {
            yaml_scalar(s.trim_matches('"'))
        };
        out.push_str(&format!("      - {item}\n"));
    }
    out
}

fn describe(e: &Element) -> String {
    let mut parts = vec![e.element_type.clone()];
    if !e.platform_id.is_empty() {
        parts.push(format!("id={:?}", e.platform_id));
    }
    if let Some(t) = e.text.as_deref().filter(|t| !t.is_empty()) {
        parts.push(format!("text={t:?}"));
    }
    if let Some(l) = e.label.as_deref().filter(|l| !l.is_empty()) {
        parts.push(format!("label={l:?}"));
    }
    if !e.enabled {
        parts.push("disabled".into());
    }
    parts.push(format!(
        "[{},{} {}x{}]",
        e.bounds.x, e.bounds.y, e.bounds.width, e.bounds.height
    ));
    parts.join(" ")
}

/// Elements with something to select them by, indented by depth.
fn print_tree(e: &Element, depth: usize) {
    let named = !e.platform_id.is_empty()
        || e.text.as_deref().is_some_and(|t| !t.is_empty())
        || e.label.as_deref().is_some_and(|l| !l.is_empty());
    let next = if named {
        println!("  {}{}", "  ".repeat(depth), describe(e));
        depth + 1
    } else {
        depth
    };
    for c in &e.children {
        print_tree(c, next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_like_suite_steps() {
        let s = parse_step("tap: { text: \"Log in\" }").unwrap();
        assert!(matches!(s.action, velocity_common::Action::Tap { .. }));
        let s = parse_step("Tap the login button").unwrap();
        assert!(matches!(s.action, velocity_common::Action::Natural { .. }));
        assert!(parse_step("tap: { txet: \"x\" }").is_err());
        let sel = parse_selector("{ text: \"Pay\", below: { text: \"Total\" } }").unwrap();
        assert_eq!(sel.to_string(), r#"text="Pay" below(text="Total")"#);
    }

    /// Plain English is quoted for YAML with Rust's `{:?}`, which writes a
    /// combining accent, a zero-width space (pasted from a web page) or a
    /// DEL as `\u{301}`: not a YAML escape, so the step failed to parse (and
    /// `:save` wrote a suite that did not load).
    #[test]
    fn english_with_invisible_or_combining_characters_still_parses() {
        for line in [
            "Tap the Cafe\u{301} button",
            "Tap the\u{200b} button",
            "Tap the \u{1f468}\u{200d}\u{1f469} button",
        ] {
            let s = parse_step(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            assert!(
                matches!(&s.action, velocity_common::Action::Natural { intent, .. } if intent == line),
                "{line:?}: {:?}",
                s.action
            );
            let yaml = suite_yaml("com.app", &[line.to_string()]);
            let suite = velocity_core::parser::parse_suite_from_str(&yaml)
                .unwrap_or_else(|e| panic!("{line:?}: {e}\n{yaml}"));
            assert_eq!(suite.tests[0].steps.len(), 1);
        }
    }

    #[test]
    fn saved_steps_form_a_parseable_suite() {
        let yaml = suite_yaml(
            "com.app",
            &[
                "launchApp: {}".into(),
                "tap: { text: \"Go\" }".into(),
                "check the total".into(),
            ],
        );
        let suite = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
        assert_eq!(suite.tests[0].steps.len(), 3);
    }

    /// VEL-169: a plain-English line with a colon is a natural step, not
    /// YAML that fails to parse.
    #[test]
    fn english_with_a_colon_is_a_natural_step() {
        for line in ["Check the total: 12.50", "Note: tap the login button"] {
            let s = parse_step(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(
                matches!(s.action, velocity_common::Action::Natural { .. }),
                "{line}"
            );
        }
        let yaml = suite_yaml("com.app", &["Check the total: 12.50".into()]);
        let suite = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
        assert!(matches!(
            suite.tests[0].steps[0].action,
            velocity_common::Action::Natural { .. }
        ));
        // A misspelt field is still an error, not a natural step.
        assert!(parse_step("tap: { txet: \"x\" }").is_err());
    }

    /// m-7: a misspelt action key, or a flow-style step, went to the
    /// natural-language planner as plain English.
    #[test]
    fn misspelt_actions_and_flow_style_are_not_english() {
        let e = parse_step("tpa: Login").unwrap_err().to_string();
        assert!(e.contains("did you mean `tap`"), "{e}");
        let e = parse_step("inputTxt: hello").unwrap_err().to_string();
        assert!(e.contains("`inputText`"), "{e}");
        let s = parse_step("{ tap: { id: x } }").unwrap();
        assert!(matches!(s.action, velocity_common::Action::Tap { .. }));
        // English still is.
        for line in [
            "Map: open the settings",
            "Check the total: 12.50",
            "note: tap login",
        ] {
            let s = parse_step(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(
                matches!(s.action, velocity_common::Action::Natural { .. }),
                "{line}"
            );
        }
    }

    /// VEL-040: a `:save`/`:shot` to a path that can't be written reports
    /// the error instead of ending the session.
    #[test]
    fn a_failed_write_is_reported_not_fatal() {
        let dir = std::env::temp_dir().join(format!("vrepl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let missing = dir.join("no-such-dir").join("out.yaml");
        assert!(!write_reporting(missing.to_str().unwrap(), b"x"));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("out.yaml");
        assert!(write_reporting(ok.to_str().unwrap(), b"x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
