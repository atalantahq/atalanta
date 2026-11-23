//! Agent guides compiled into the binary, so the one an agent reads always
//! matches the `atalanta` it drives (`atalanta skills get <name>`). A skill
//! file installed into an agent client can go stale on upgrade; this can't.

/// `(name, one-line summary, guide)`.
pub const SKILLS: &[(&str, &str, &str)] = &[
    (
        "cli",
        "Drive a device from the shell: snapshot, act by ref, export a test",
        include_str!("../assets/skills/cli.md"),
    ),
    (
        "yaml",
        "Writing tests: suite layout, steps, selectors, variables and secrets",
        include_str!("../assets/skills/yaml.md"),
    ),
    (
        "mcp",
        "The MCP server's workflow, YAML cheat sheet and rules",
        crate::install::AGENT_RULES,
    ),
    (
        "mcp-tools",
        "Each MCP core tool: what it changes, returns, and whether to repeat it",
        include_str!("../assets/skills/mcp-tools.md"),
    ),
];

/// The guide called `name`.
pub fn get(name: &str) -> Option<&'static str> {
    SKILLS
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, _, body)| *body)
}

/// The short skill file to install into an agent client: it only points at
/// the guides in the binary, so it never needs updating.
pub const STUB: &str = "---
name: atalanta
description: Drive Android emulators, iOS simulators and devices to check mobile UI changes, and save what was done as a YAML test that CI replays with no model. Use when verifying a mobile app change on a device or writing a mobile UI test.
---

# Atalanta

Before the first `atalanta` command in a session, run `atalanta skills get cli` and follow it. It matches the installed binary. Do not probe with `--help` first.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_is_reachable_and_the_stub_points_at_them() {
        for (name, summary, body) in SKILLS {
            assert!(!summary.is_empty() && body.len() > 200, "{name}");
            assert_eq!(get(&name.to_uppercase()), Some(*body));
        }
        assert!(get("nope").is_none());
        assert!(STUB.contains("atalanta skills get cli"));
    }

    /// Review 13: a guide that names `atalanta skills get X` names one that
    /// exists.
    #[test]
    fn every_guide_a_guide_names_exists() {
        for (name, _, body) in SKILLS {
            for part in body.split("atalanta skills get ").skip(1) {
                let wanted: String = part
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                if wanted.is_empty() {
                    continue;
                }
                assert!(
                    get(&wanted).is_some(),
                    "{name} names missing guide {wanted:?}"
                );
            }
        }
    }
}
