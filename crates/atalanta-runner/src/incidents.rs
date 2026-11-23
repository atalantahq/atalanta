//! Group failures that share a cause.
//!
//! Flaky and environmental failures come in clusters — 75% of flaky tests
//! belong to a co-failing group, mean size 13.5, mostly network or a shared
//! dependency (Parry et al. 2025). Twelve red tests with the same error are one
//! problem (a backend down, a device gone), not twelve, and reporting them as
//! one incident points at the fix.

use std::collections::BTreeMap;

use velocity_common::{TestResult, TestStatus};

/// Failures that share one normalized error.
#[derive(Debug, Clone, PartialEq)]
pub struct Incident {
    /// The error, with volatile parts (numbers, quoted values) masked.
    pub signature: String,
    pub tests: Vec<String>,
    /// Every failure in the group came from the device or its connection.
    pub infrastructure: bool,
}

/// Smallest group reported as one incident.
const MIN_CLUSTER: usize = 3;

/// Failures grouped by error signature, largest first; groups smaller than
/// three (two for infrastructure failures) are left out.
pub fn incidents(results: &[TestResult]) -> Vec<Incident> {
    let mut groups: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
    for r in results.iter().filter(|r| r.status == TestStatus::Failed) {
        let sig = signature(r.error_message.as_deref().unwrap_or("unknown error"));
        let entry = groups.entry(sig).or_insert_with(|| (Vec::new(), true));
        entry.0.push(r.test_name.clone());
        entry.1 &= r.infrastructure_failure;
    }
    let mut out: Vec<Incident> = groups
        .into_iter()
        .filter(|(_, (tests, infra))| tests.len() >= if *infra { 2 } else { MIN_CLUSTER })
        .map(|(signature, (tests, infrastructure))| Incident {
            signature,
            tests,
            infrastructure,
        })
        .collect();
    out.sort_by_key(|i| std::cmp::Reverse(i.tests.len()));
    out
}

/// Mask what varies between occurrences of the same error: quoted values,
/// numbers, and hex ids.
fn signature(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A quote opens a value only where a value can start, and only if
        // it closes: an apostrophe ("didn't") swallowed the rest of the
        // message and merged unrelated failures (VEL-054).
        let opens = match c {
            '"' => true,
            '\'' => i == 0 || matches!(chars[i - 1], ' ' | '\t' | '=' | ':' | '(' | '['),
            _ => false,
        };
        if opens {
            if let Some(end) = chars[i + 1..].iter().position(|&d| d == c) {
                out.push_str("\"…\"");
                i += end + 2;
                continue;
            }
        }
        if c.is_ascii_alphanumeric() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '-' | '_'))
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if c.is_ascii_digit() || is_hex_id(&word) {
                out.push('#');
            } else {
                out.push_str(&word);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A hex id or UUID (`3fa85f64-5717-…`, `deadbeef01`): eight or more hex
/// digits, at least one a decimal digit. The doc promised these were
/// masked; only ids starting with a digit were (VEL-185).
fn is_hex_id(word: &str) -> bool {
    let hex: Vec<char> = word.chars().filter(|c| *c != '-').collect();
    hex.len() >= 8
        && hex.iter().all(|c| c.is_ascii_hexdigit())
        && hex.iter().any(|c| c.is_ascii_digit())
}

pub(crate) fn print_incidents(results: &[TestResult]) {
    for inc in incidents(results) {
        cprintln!(
            "  \x1b[31m▲ {} tests failed the same way{}\x1b[0m — likely one shared cause:",
            inc.tests.len(),
            if inc.infrastructure {
                " (device/connection)"
            } else {
                ""
            }
        );
        println!("    {}", inc.signature);
        println!("    {}", inc.tests.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn failed(name: &str, msg: &str) -> TestResult {
        TestResult::failed(name, msg.into(), Duration::ZERO)
    }

    #[test]
    fn same_error_with_different_values_is_one_incident() {
        let results = vec![
            failed("a", "Element not found: text=\"Pay\" after 10000ms"),
            failed("b", "Element not found: text=\"Cart\" after 5000ms"),
            failed("c", "Element not found: text=\"Login\" after 10000ms"),
            failed("d", "Assertion failed: expected \"1\", got \"2\""),
        ];
        let inc = incidents(&results);
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].tests, ["a", "b", "c"]);
        assert!(!inc[0].infrastructure);
    }

    #[test]
    fn two_infrastructure_failures_are_enough() {
        let mut a = failed("a", "ADB connection lost to device 'emulator-5554': closed");
        let mut b = failed("b", "ADB connection lost to device 'emulator-5556': closed");
        a.infrastructure_failure = true;
        b.infrastructure_failure = true;
        let inc = incidents(&[a, b]);
        assert_eq!(inc.len(), 1);
        assert!(inc[0].infrastructure);
    }

    // VEL-054: an apostrophe swallowed the rest of the message.
    #[test]
    fn an_apostrophe_does_not_merge_different_failures() {
        let results = vec![
            failed("a", "App didn't launch: crashed in LoginActivity"),
            failed("b", "App didn't launch: crashed in LoginActivity"),
            failed("c", "App didn't launch: crashed in LoginActivity"),
            failed("d", "App didn't respond: ANR in CartActivity"),
        ];
        let inc = incidents(&results);
        assert_eq!(inc.len(), 1, "{inc:?}");
        assert_eq!(inc[0].tests, ["a", "b", "c"]);
        assert_eq!(signature("set 'x' to 3"), "set \"…\" to #");
    }

    // VEL-185: hex ids and UUIDs are masked like numbers.
    #[test]
    fn hex_ids_are_masked() {
        assert_eq!(
            signature("session deadbeef01 lost"),
            signature("session cafebabe02 lost")
        );
        assert_eq!(
            signature("req a3fa85f6-5717-4562-b3fc-2c963f66afa6 failed"),
            signature("req e1c2d3f4-1111-2222-3333-444455556666 failed")
        );
        assert_eq!(signature("Element not found"), "Element not found");
    }

    #[test]
    fn scattered_failures_are_not_incidents() {
        let results = vec![failed("a", "boom"), failed("b", "bang")];
        assert!(incidents(&results).is_empty());
    }
}
