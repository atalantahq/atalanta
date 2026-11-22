//! `velocity migrate waits`: delete fixed `wait:` steps that sit right
//! before a step which already waits for its element (tap, assertVisible,
//! inputText, …). Those waits are pure cost: the next step polls for its
//! target anyway.
//!
//! The rewrite is line-based, so comments and layout survive (a YAML
//! round-trip would drop them). Handles both forms:
//!
//! ```yaml
//! - wait: { ms: 1500 }
//! - wait:
//!     ms: 1500
//! ```

/// Steps that wait for their own element before acting.
const CONDITION_STEPS: &[&str] = &[
    "tap",
    "doubleTap",
    "longPress",
    "inputText",
    "clearText",
    "assertVisible",
    "assertText",
    "scrollUntilVisible",
];

/// A removed wait: 1-based line, its duration, the step it preceded.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovedWait {
    pub line: usize,
    pub ms: u64,
    pub before: String,
}

/// The rewritten YAML and what was removed.
pub fn remove_redundant_waits(yaml: &str) -> (String, Vec<RemovedWait>) {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut drop = vec![false; lines.len()];
    let mut removed = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let Some((indent, key, rest)) = step_head(lines[i]) else {
            i += 1;
            continue;
        };
        if key != "wait" {
            i += 1;
            continue;
        }
        // The wait's own lines: the head, plus deeper-indented ones (block form).
        let mut end = i + 1;
        while end < lines.len() && (is_blank(lines[end]) || indent_of(lines[end]) > indent) {
            if is_blank(lines[end]) {
                break;
            }
            end += 1;
        }
        let ms = wait_ms(rest, &lines[i + 1..end]);
        // The next step at the same level.
        let next = lines[end..]
            .iter()
            .filter(|l| !is_blank(l) && !is_comment(l))
            .find_map(|l| {
                let (ni, nk, _) = step_head(l)?;
                (ni == indent).then_some(nk)
            });
        let next_at_same_level = lines[end..]
            .iter()
            .find(|l| !is_blank(l) && !is_comment(l))
            .is_some_and(|l| step_head(l).is_some_and(|(ni, _, _)| ni == indent));
        if let (Some(next), Some(ms)) = (next, ms) {
            if next_at_same_level && CONDITION_STEPS.contains(&next) {
                for d in drop.iter_mut().take(end).skip(i) {
                    *d = true;
                }
                removed.push(RemovedWait {
                    line: i + 1,
                    ms,
                    before: next.to_string(),
                });
            }
        }
        i = end;
    }

    // `lines()` drops each `\r`: keep a CRLF file CRLF, not one rewritten
    // line by line to LF.
    let eol = if yaml.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: String = lines
        .iter()
        .zip(&drop)
        .filter(|(_, d)| !**d)
        .map(|(l, _)| *l)
        .collect::<Vec<_>>()
        .join(eol);
    if yaml.ends_with('\n') {
        out.push_str(eol);
    }
    (out, removed)
}

/// `- key: rest` → (indent of the dash, key, rest).
fn step_head(line: &str) -> Option<(usize, &str, &str)> {
    let indent = indent_of(line);
    let body = line[indent..].strip_prefix("- ")?;
    let (key, rest) = body.split_once(':')?;
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((indent, key, rest.trim()))
}

/// Duration of a wait, inline (`{ ms: 1500 }`, `1500`) or from its block.
fn wait_ms(rest: &str, block: &[&str]) -> Option<u64> {
    let digits = |s: &str| -> Option<u64> {
        let s = s.split('#').next().unwrap_or("");
        let start = s.find(|c: char| c.is_ascii_digit())?;
        s[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    if !rest.is_empty() {
        return digits(rest);
    }
    block
        .iter()
        .find(|l| l.trim_start().starts_with("ms:"))
        .and_then(|l| digits(l))
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_a_wait_before_a_step_that_waits_itself() {
        let yaml = "\
tests:
  - name: t
    steps:
      - tap: { text: \"Log in\" }
      - wait: { ms: 1500 }
      # code arrives by SMS
      - assertVisible: { text: \"Enter code\" }
      - wait:
          ms: 800
      - tap: { text: \"Next\" }
";
        let (out, removed) = remove_redundant_waits(yaml);
        assert_eq!(removed.len(), 2);
        assert_eq!(
            removed[0],
            RemovedWait {
                line: 5,
                ms: 1500,
                before: "assertVisible".into()
            }
        );
        assert_eq!(removed[1].ms, 800);
        assert!(!out.contains("wait"));
        assert!(out.contains("# code arrives by SMS"), "comments survive");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn a_crlf_file_stays_crlf() {
        let yaml = "tests:\r\n  - name: t\r\n    steps:\r\n      - wait: { ms: 500 }\r\n      - tap: { text: \"Go\" }\r\n";
        let (out, removed) = remove_redundant_waits(yaml);
        assert_eq!(removed.len(), 1);
        assert_eq!(
            out,
            "tests:\r\n  - name: t\r\n    steps:\r\n      - tap: { text: \"Go\" }\r\n"
        );
    }

    #[test]
    fn keeps_waits_that_nothing_after_them_covers() {
        let yaml = "\
flows:
  - id: open
    steps:
      - tap: { text: \"Menu\" }
      - wait: { ms: 1000 }

  - id: other
    steps:
      - pressKey: { key: back }
      - wait: { ms: 500 }
      - pressKey: { key: back }
";
        let (out, removed) = remove_redundant_waits(yaml);
        assert!(removed.is_empty(), "{removed:?}");
        assert_eq!(out, yaml);
    }
}
