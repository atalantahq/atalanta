// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `docs/llms.txt` (llmstxt.org): an index of the docs an AI agent can read,
//! made from `docs/README.md`'s table so the two can't drift. Each link is
//! `https://atalanta.sh/docs/<slug>.md`, the page's Markdown on the site,
//! where the slug is the file stem lowercased with `.` replaced by `-`.
//! `ATALANTA_BLESS=1 cargo test -p atalanta-core --test llms_txt` rewrites it.

const SITE: &str = "https://atalanta.sh/docs/";

fn slug_of(target: &str) -> String {
    let stem = target.strip_suffix(".md").unwrap_or(target);
    stem.to_lowercase().replace('.', "-")
}

fn generate(index: &str) -> String {
    let mut out = String::from(
        "# Atalanta\n\n\
         > Mobile (Android, iOS), web and API UI tests in one YAML suite, run by one static \
         binary. It has an MCP server and a CLI for coding agents, an agent that writes \
         deterministic tests replayed with no model, and failure evidence (video, logs, \
         traces) for CI and pull requests.\n\n\
         Exit codes: 0 passed, 1 a test failed, 2 usage or config, 3 device or \
         infrastructure, 4 suite timeout, 5 internal error. Secrets go in YAML as \
         `${NAME}` and never in the file itself.\n\n## Docs\n\n",
    );
    for line in index.lines() {
        // `| [GUIDE.md](GUIDE.md) | What's in it |`
        let Some(rest) = line.strip_prefix("| [") else {
            continue;
        };
        let Some((title, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((target, rest)) = rest.split_once(')') else {
            continue;
        };
        if !target.ends_with(".md") || target.contains('/') {
            continue; // research/ and anything outside docs/
        }
        let what = rest
            .trim()
            .trim_start_matches('|')
            .trim()
            .trim_end_matches('|')
            .trim();
        out.push_str(&format!(
            "- [{title}]({site}{slug}.md): {what}\n",
            title = title,
            site = SITE,
            slug = slug_of(target),
            what = what
        ));
    }
    out.push_str(
        "\n## Optional\n\n\
         - [All docs in one file](https://atalanta.sh/llms-full.txt): every page above as one Markdown file\n",
    );
    out
}

#[test]
fn llms_txt_lists_every_doc_page() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs");
    let index = std::fs::read_to_string(format!("{dir}/README.md")).unwrap();
    let want = generate(&index);
    let path = format!("{dir}/llms.txt");
    if std::env::var_os("ATALANTA_BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        have, want,
        "docs/llms.txt is out of date: ATALANTA_BLESS=1 to rewrite"
    );
    // Every page the index lists exists as a file in docs/.
    for line in index.lines() {
        let Some(rest) = line.strip_prefix("| [") else {
            continue;
        };
        let Some((_title, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((target, _rest)) = rest.split_once(')') else {
            continue;
        };
        if !target.ends_with(".md") || target.contains('/') {
            continue;
        }
        assert!(std::path::Path::new(dir).join(target).is_file(), "{target}");
    }
    assert!(want.contains("AGENTS.md") && want.contains("MCP.md"));
}
