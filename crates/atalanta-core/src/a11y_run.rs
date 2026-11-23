//! The accessibility audit during a run (`config.accessibility.audit`).
//!
//! The executor hands over the trees it has already read (a lookup's
//! cached tree, the ambiguity check's, a natural step's, a `verify:`
//! check's, a failed step's): auditing them costs no device round trip.
//! Each distinct screen (by [`crate::trace::tree_hash`]) is audited once per
//! test, and each problem is reported once per test however many screens
//! show it, up to [`MAX_PER_TEST`] warnings, so a test's report stays
//! readable.

use std::collections::HashSet;

use atalanta_common::a11y::{self, Severity};
use atalanta_common::{AccessibilityConfig, Element, Platform};

/// Findings reported per test; the rest are counted in one closing note.
pub const MAX_PER_TEST: usize = 20;

/// Error findings quoted in a `fail: true` failure.
const QUOTED_ERRORS: usize = 3;

/// One executor's audit state, reset per test.
#[derive(Debug)]
pub struct AuditRun {
    fail: bool,
    density: Option<f64>,
    screens: HashSet<String>,
    reported: HashSet<String>,
    shown: usize,
    left_out: usize,
    errors: Vec<String>,
    audits: usize,
}

impl AuditRun {
    /// `None` when the audit is off (the default).
    pub fn new(cfg: &AccessibilityConfig) -> Option<Self> {
        cfg.audit.then(|| AuditRun {
            fail: cfg.fail,
            density: cfg.density,
            screens: HashSet::new(),
            reported: HashSet::new(),
            shown: 0,
            left_out: 0,
            errors: Vec::new(),
            audits: 0,
        })
    }

    /// Forget the last test's screens and findings.
    pub fn begin_test(&mut self) {
        self.screens.clear();
        self.reported.clear();
        self.shown = 0;
        self.left_out = 0;
        self.errors.clear();
    }

    /// Screens audited since the executor started (for tests).
    pub fn audits(&self) -> usize {
        self.audits
    }

    /// Audit `tree` unless this test already did the same screen; the
    /// warnings to add (new problems only, capped).
    pub fn screen(&mut self, tree: &Element, platform: Platform) -> Vec<String> {
        if !self.screens.insert(crate::trace::tree_hash(tree)) {
            return Vec::new();
        }
        self.audits += 1;
        let opts = a11y::Options {
            platform,
            screen: None,
            density: self.density,
        };
        let mut out = Vec::new();
        for f in a11y::audit_with(tree, &opts) {
            if !self.reported.insert(f.key()) {
                continue;
            }
            let line = f.line();
            if f.severity == Severity::Error {
                self.errors.push(line.clone());
            }
            if self.shown < MAX_PER_TEST {
                self.shown += 1;
                out.push(format!("accessibility: {line}"));
            } else {
                self.left_out += 1;
            }
        }
        out
    }

    /// The closing note when findings were left out.
    pub fn left_out_note(&self) -> Option<String> {
        (self.left_out > 0).then(|| {
            format!(
                "accessibility: {} more finding(s) left out (the first {MAX_PER_TEST} are listed)",
                self.left_out
            )
        })
    }

    /// With `fail: true`, the test's failure when an error was found.
    pub fn failure(&self) -> Option<String> {
        if !self.fail || self.errors.is_empty() {
            return None;
        }
        let quoted: Vec<&str> = self
            .errors
            .iter()
            .take(QUOTED_ERRORS)
            .map(String::as_str)
            .collect();
        let more = self.errors.len().saturating_sub(QUOTED_ERRORS);
        let tail = if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        };
        Some(format!(
            "accessibility audit found {} error(s): {}{tail}",
            self.errors.len(),
            quoted.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::Rect;

    fn screen(buttons: usize) -> Element {
        let children = (0..buttons)
            .map(|i| Element {
                platform_id: format!("b{i}"),
                element_type: "android.widget.ImageButton".into(),
                bounds: Rect {
                    x: 0,
                    y: i as i32 * 200,
                    width: 144,
                    height: 144,
                },
                enabled: true,
                visible: true,
                ..Default::default()
            })
            .collect();
        Element {
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 9000,
            },
            children,
            ..Default::default()
        }
    }

    #[test]
    fn off_unless_asked() {
        assert!(AuditRun::new(&AccessibilityConfig::default()).is_none());
    }

    #[test]
    fn one_audit_per_screen_one_report_per_problem_capped() {
        let cfg = AccessibilityConfig {
            audit: true,
            fail: true,
            density: None,
        };
        let mut run = AuditRun::new(&cfg).unwrap();
        assert_eq!(run.screen(&screen(2), Platform::Android).len(), 2);
        // The same screen again: not audited again.
        assert!(run.screen(&screen(2), Platform::Android).is_empty());
        assert_eq!(run.audits(), 1);
        // A new screen with the same two buttons and one more: only the new one.
        let w = run.screen(&screen(3), Platform::Android);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].starts_with("accessibility: error image-unlabeled"),
            "{w:?}"
        );
        // A big screen: capped at 20, the rest counted.
        let w = run.screen(&screen(30), Platform::Android);
        assert_eq!(w.len(), MAX_PER_TEST - 3);
        assert_eq!(
            run.left_out_note().as_deref(),
            Some("accessibility: 10 more finding(s) left out (the first 20 are listed)")
        );
        let fail = run.failure().unwrap();
        assert!(
            fail.starts_with("accessibility audit found 30 error(s): "),
            "{fail}"
        );
        assert!(fail.ends_with("; and 27 more"), "{fail}");
        // A new test starts clean.
        run.begin_test();
        assert!(run.left_out_note().is_none() && run.failure().is_none());
        assert_eq!(run.screen(&screen(1), Platform::Android).len(), 1);
    }
}
