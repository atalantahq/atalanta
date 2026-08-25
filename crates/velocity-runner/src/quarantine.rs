//! A checked-in quarantine list: known failures that don't fail the build.
//!
//! `velocity-quarantine.yaml` next to the suite:
//!
//! ```yaml
//! quarantine:
//!   - test: "checkout with saved card"
//!     signature: "Element not found: text=\"Pay now\""
//!     reason: "PAY-142: button renamed on staging"
//!     expires: 2026-12-31     # optional: after this date it warns and stops forgiving
//!   - test: "search*"
//!     signature: "timed out"
//!     flaky-only: true        # forgives only a flaky pass, never a failure
//! ```
//!
//! Two rules keep it from hiding new breakage (Trunk, Chromium):
//! - an entry forgives only a failure whose error contains its `signature`;
//!   the same test failing some other way still fails the run;
//! - the run's exit code is forgiven only when *every* failure is
//!   quarantined. Reports still show quarantined tests as failed.

use serde::Deserialize;
use velocity_common::{Result, SuiteResult, TestResult, TestStatus, VelocityError};

pub const FILE_NAME: &str = "velocity-quarantine.yaml";

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// Test name; `*` matches any run of characters.
    pub test: String,
    /// Substring the failure's error message must contain.
    pub signature: String,
    #[serde(default)]
    pub reason: Option<String>,
    /// Last day the entry forgives (`YYYY-MM-DD`). After it, the entry
    /// warns and forgives nothing, so a quarantine can't rot (A-11).
    #[serde(default)]
    pub expires: Option<chrono::NaiveDate>,
    /// Forgive only a flaky outcome (failed, then passed on retry) whose
    /// failed attempt matches; a hard failure is never forgiven (A-11).
    /// Matters with `--fail-on-flaky` / `retry.flakyResult: fail`.
    #[serde(default, rename = "flaky-only", alias = "flakyOnly")]
    pub flaky_only: bool,
}

impl Entry {
    fn expired(&self, today: chrono::NaiveDate) -> bool {
        self.expires.is_some_and(|d| today > d)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Quarantine {
    #[serde(default)]
    pub quarantine: Vec<Entry>,
}

/// What the list did to a run.
#[derive(Debug, Default)]
pub struct Verdict {
    /// (test, reason) of each forgiven failure.
    pub forgiven: Vec<(String, String)>,
    /// Failures no entry covers.
    pub unforgiven: usize,
    /// Entries past their `expires` date (test pattern, date), which
    /// forgave nothing.
    pub expired: Vec<(String, chrono::NaiveDate)>,
}

impl Verdict {
    /// Every failure was a known one.
    pub fn all_forgiven(&self) -> bool {
        self.unforgiven == 0 && !self.forgiven.is_empty()
    }
}

impl Quarantine {
    /// Load `path`; `Ok(None)` if it doesn't exist.
    pub fn load(path: &std::path::Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| VelocityError::Config(format!("{}: {e}", path.display())))?;
        let q: Quarantine = serde_yaml::from_str(&text)
            .map_err(|e| VelocityError::Config(format!("{}: {e}", path.display())))?;
        if let Some(bad) = q.quarantine.iter().find(|e| e.signature.trim().is_empty()) {
            return Err(VelocityError::Config(format!(
                "{}: entry for '{}' needs a non-empty signature (a quarantine forgives a known failure, not a test)",
                path.display(),
                bad.test
            )));
        }
        Ok(Some(q))
    }

    /// The entry forgiving `t` today, if any.
    pub fn entry_for(&self, t: &TestResult) -> Option<&Entry> {
        self.entry_on(t, chrono::Local::now().date_naive())
    }

    fn entry_on(&self, t: &TestResult, today: chrono::NaiveDate) -> Option<&Entry> {
        // Velocity's own bug (a panic) is never a known app failure (D-19).
        if t.internal_error {
            return None;
        }
        let live = |e: &&Entry| !e.expired(today) && glob(&e.test, &t.test_name);
        match t.status {
            TestStatus::Failed => {
                let err = t.error_message.as_deref().unwrap_or("");
                self.quarantine
                    .iter()
                    .filter(live)
                    .find(|e| !e.flaky_only && err.contains(&e.signature))
            }
            // A flaky pass: its failed attempts carry the signature.
            TestStatus::Retried => self.quarantine.iter().filter(live).find(|e| {
                t.failed_attempts.iter().any(|m| m.contains(&e.signature))
                    || t.warnings
                        .iter()
                        .any(|w| w.starts_with("flaky") && w.contains(&e.signature))
            }),
            _ => None,
        }
    }

    /// Mark forgiven failures with a warning and count the rest.
    pub fn apply(&self, result: &mut SuiteResult) -> Verdict {
        self.apply_on(result, chrono::Local::now().date_naive())
    }

    /// [`apply`](Self::apply) as of `today`.
    pub fn apply_on(&self, result: &mut SuiteResult, today: chrono::NaiveDate) -> Verdict {
        let mut v = Verdict {
            expired: self
                .quarantine
                .iter()
                .filter(|e| e.expired(today))
                .filter_map(|e| e.expires.map(|d| (e.test.clone(), d)))
                .collect(),
            ..Verdict::default()
        };
        for t in &mut result.tests {
            if t.status == TestStatus::Retried {
                // A known flaky test: marked, so `--fail-on-flaky` forgives it.
                if let Some(e) = self.entry_on(t, today).cloned() {
                    let reason = e.reason.unwrap_or_else(|| "no reason given".into());
                    t.warnings.push(format!("quarantined (flaky): {reason}"));
                    t.quarantined = Some(reason);
                }
                continue;
            }
            if t.status != TestStatus::Failed {
                continue;
            }
            match self.entry_on(t, today).cloned() {
                Some(e) => {
                    let reason = e.reason.unwrap_or_else(|| "no reason given".into());
                    t.warnings.push(format!("quarantined: {reason}"));
                    t.quarantined = Some(reason.clone());
                    v.forgiven.push((t.test_name.clone(), reason));
                }
                None => v.unforgiven += 1,
            }
        }
        v
    }
}

/// `*` wildcard match, no other metacharacters.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn failed(name: &str, err: &str) -> TestResult {
        TestResult::failed(name, err.into(), Duration::ZERO)
    }

    fn suite(tests: Vec<TestResult>) -> SuiteResult {
        SuiteResult {
            total: tests.len(),
            passed: 0,
            failed: tests.len(),
            skipped: 0,
            retried: 0,
            duration: Duration::ZERO,
            tests,
            shard_index: None,
            shard_total: None,
        }
    }

    fn list() -> Quarantine {
        serde_yaml::from_str(
            r#"
quarantine:
  - test: "checkout*"
    signature: "text=\"Pay now\""
    reason: "PAY-142"
"#,
        )
        .unwrap()
    }

    #[test]
    fn forgives_only_the_known_failure() {
        let q = list();
        let mut r = suite(vec![failed(
            "checkout with card",
            "Element not found: text=\"Pay now\" after 10000ms",
        )]);
        let v = q.apply(&mut r);
        assert!(v.all_forgiven());
        assert!(r.tests[0].warnings[0].contains("PAY-142"));

        // Same test, different failure: not forgiven.
        let mut r = suite(vec![failed(
            "checkout with card",
            "Assertion failed: total",
        )]);
        assert!(!q.apply(&mut r).all_forgiven());
    }

    #[test]
    fn one_unknown_failure_fails_the_run() {
        let q = list();
        let mut r = suite(vec![
            failed("checkout with card", "text=\"Pay now\""),
            failed("login", "boom"),
        ]);
        let v = q.apply(&mut r);
        assert_eq!(v.forgiven.len(), 1);
        assert_eq!(v.unforgiven, 1);
        assert!(!v.all_forgiven());
    }

    // D-19: a panic matching an entry's signature is still not forgiven.
    #[test]
    fn an_internal_error_is_never_forgiven() {
        let q = list();
        let mut t = failed("checkout with card", "text=\"Pay now\" then panicked");
        t.internal_error = true;
        let mut r = suite(vec![t]);
        let v = q.apply(&mut r);
        assert!(!v.all_forgiven());
        assert_eq!(v.unforgiven, 1);
    }

    fn day(s: &str) -> chrono::NaiveDate {
        s.parse().unwrap()
    }

    // A-11: an expired entry warns and stops forgiving.
    #[test]
    fn an_expired_entry_stops_forgiving() {
        let q: Quarantine = serde_yaml::from_str(
            "quarantine:\n  - test: checkout*\n    signature: Pay now\n    expires: 2026-03-31\n",
        )
        .unwrap();
        let make = || suite(vec![failed("checkout", "Element not found: Pay now")]);
        let mut r = make();
        let v = q.apply_on(&mut r, day("2026-03-31"));
        assert!(v.all_forgiven());
        assert!(v.expired.is_empty());
        let mut r = make();
        let v = q.apply_on(&mut r, day("2026-04-01"));
        assert!(!v.all_forgiven());
        assert_eq!(v.unforgiven, 1);
        assert_eq!(
            v.expired,
            vec![("checkout*".to_string(), day("2026-03-31"))]
        );
        assert!(r.tests[0].quarantined.is_none());
        // A date that isn't one is a config error.
        assert!(serde_yaml::from_str::<Quarantine>(
            "quarantine:\n  - test: x\n    signature: y\n    expires: soon\n"
        )
        .is_err());
    }

    // A-11: `flaky-only` forgives a flaky pass, never a hard failure.
    #[test]
    fn flaky_only_never_forgives_a_hard_failure() {
        let q: Quarantine = serde_yaml::from_str(
            "quarantine:\n  - test: search*\n    signature: timed out\n    flaky-only: true\n",
        )
        .unwrap();
        let mut r = suite(vec![failed("search box", "Step timeout: timed out")]);
        let v = q.apply_on(&mut r, day("2026-01-01"));
        assert_eq!(v.unforgiven, 1);
        assert!(r.tests[0].quarantined.is_none());

        let mut flaky = failed("search box", "");
        flaky.status = TestStatus::Retried;
        flaky.failed_attempts = vec!["Step timeout: timed out".into()];
        let mut r = suite(vec![flaky]);
        r.failed = 0;
        let v = q.apply_on(&mut r, day("2026-01-01"));
        assert_eq!(v.unforgiven, 0);
        assert!(r.tests[0].quarantined.is_some());
    }

    #[test]
    fn globs() {
        assert!(glob("a*", "abc"));
        assert!(glob("*c", "abc"));
        assert!(glob("a*c", "abc"));
        assert!(glob("a*b*d", "aXbYd"));
        assert!(!glob("a*c", "abd"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
    }

    #[test]
    fn an_entry_without_a_signature_is_rejected() {
        let dir = std::env::temp_dir().join(format!("vel-q-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "quarantine:\n  - test: x\n    signature: \"\"\n").unwrap();
        assert!(Quarantine::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
