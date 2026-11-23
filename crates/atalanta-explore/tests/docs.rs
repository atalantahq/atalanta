//! Every flag of `atalanta explore` is documented in docs/EXPLORE.md
//! (review 06 #11): the two widest opt-ins once were not.

use std::path::Path;

/// The long flags declared in the CLI's `explore` arguments: a field after
/// an `#[arg(...)]` with `long`, named by `long = "…"` or by the field.
fn flags(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if !(t.starts_with("#[arg(") && t.contains("long")) {
            continue;
        }
        if let Some(at) = t.find("long = \"") {
            let rest = &t[at + 8..];
            out.push(rest[..rest.find('"').unwrap()].to_string());
            continue;
        }
        let field = lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.starts_with('#') && !l.starts_with("//") && l.contains(':'))
            .expect("a field after #[arg]");
        let name = field.trim_start_matches("pub ").split(':').next().unwrap();
        out.push(name.trim().replace('_', "-"));
    }
    out
}

#[test]
fn every_explore_flag_is_on_the_explore_page() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = std::fs::read_to_string(root.join("atalanta-cli/src/commands/explore.rs")).unwrap();
    let doc = std::fs::read_to_string(root.join("docs/EXPLORE.md")).unwrap();
    let flags = flags(&src);
    assert!(flags.len() > 20, "found only {flags:?}");
    let missing: Vec<&String> = flags
        .iter()
        .filter(|f| !doc.contains(&format!("--{f}")))
        .collect();
    assert!(missing.is_empty(), "not in docs/EXPLORE.md: {missing:?}");
    assert!(doc.contains("ATALANTA_EXPLORE_SHOTS_DIR"));
}
