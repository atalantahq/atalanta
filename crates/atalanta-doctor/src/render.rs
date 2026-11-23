//! The human report.

use crate::{Area, FixKind, Report, Status, Verdict};

const RESET: &str = "\x1b[0m";

fn paint(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}{RESET}")
    } else {
        s.to_string()
    }
}

fn mark(status: Status) -> (&'static str, &'static str) {
    match status {
        Status::Ok => ("✓", "32"),
        Status::Warn => ("!", "33"),
        Status::Fail => ("✗", "31"),
        Status::Skip => ("-", "2"),
    }
}

/// The name a check prints under: its id without the area.
fn short(id: &str) -> &str {
    id.split_once('.').map_or(id, |(_, n)| n)
}

/// Text that came from a device, a simulator name, a tool's stderr or a
/// suite file, made safe to print: control characters (an ESC sequence in
/// a model name or error text rewrites the terminal) become `?`, a newline
/// in what should be one line becomes a space. Tabs stay.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' => ' ',
            c if c.is_control() && c != '\t' => '?',
            c => c,
        })
        .collect()
}

pub fn human(report: &Report, color: bool) -> String {
    let width = report
        .checks
        .iter()
        .map(|c| short(&c.id).chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "{} (velocity {}, {:.1} s)\n",
        paint(color, "1", "Velocity doctor"),
        report.velocity_version,
        report.elapsed_ms as f64 / 1000.0
    ));
    let mut area: Option<Area> = None;
    for c in &report.checks {
        if area != Some(c.area) {
            area = Some(c.area);
            out.push('\n');
            out.push_str(&paint(color, "1", c.area.title()));
            out.push('\n');
        }
        let (m, code) = mark(c.status);
        let name = short(&c.id);
        let pad = " ".repeat(width.saturating_sub(name.chars().count()));
        let summary = if c.status == Status::Skip {
            paint(color, "2", &clean(&c.summary))
        } else {
            clean(&c.summary)
        };
        out.push_str(&format!(
            "  {} {name}{pad}  {summary}\n",
            paint(color, &format!("{code};1"), m)
        ));
        let indent = " ".repeat(width + 6);
        if let Some(d) = &c.detail {
            if c.status != Status::Ok || d.lines().count() <= 4 {
                for line in d.lines().take(8) {
                    out.push_str(&format!("{indent}{}\n", paint(color, "2", &clean(line))));
                }
            }
        }
        if let Some(f) = &c.fix {
            if c.status != Status::Ok {
                let (label, text) = match &f.kind {
                    FixKind::Run(cmd) => ("Run:", clean(cmd)),
                    FixKind::Guidance(t) => ("Guidance:", clean(t)),
                };
                let auto = if f.safe_auto {
                    "  (--fix applies it)"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "{indent}{} {text}{}\n",
                    paint(color, "36", label),
                    paint(color, "2", auto)
                ));
            }
        }
    }
    let (word, code) = match report.verdict {
        Verdict::Ready => ("ready", "32;1"),
        Verdict::Degraded => ("degraded", "33;1"),
        Verdict::Blocked => ("blocked", "31;1"),
    };
    let fixable = report
        .checks
        .iter()
        .filter(|c| c.status != Status::Ok && c.fix.as_ref().is_some_and(|f| f.safe_auto))
        .count();
    out.push_str(&format!(
        "\n{} {} ({} ok, {} warning{}, {} failure{}, {} skipped){}\n",
        paint(color, "1", "Verdict:"),
        paint(color, code, word),
        report.count(Status::Ok),
        report.count(Status::Warn),
        if report.count(Status::Warn) == 1 {
            ""
        } else {
            "s"
        },
        report.count(Status::Fail),
        if report.count(Status::Fail) == 1 {
            ""
        } else {
            "s"
        },
        report.count(Status::Skip),
        if fixable > 0 {
            format!(
                "; `velocity doctor --fix` can apply {fixable} safe fix{}",
                if fixable == 1 { "" } else { "es" }
            )
        } else {
            String::new()
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use crate::{Area, Check, Fix, Report, SafeAction};
    use std::time::Duration;

    #[test]
    fn escape_sequences_from_devices_and_tools_never_reach_the_terminal() {
        let r = Report::new(
            vec![Check::ok(
                "android.api",
                Area::Android,
                "emu: Android 14 \x1b[2J\x1b]0;pwned\x07 Pixel",
            )
            .with_detail("line\r\x1b[31mred")],
            Duration::ZERO,
        );
        let plain = r.render(false);
        assert!(
            !plain.contains('\x1b') && !plain.contains('\x07'),
            "{plain:?}"
        );
        assert!(!plain.contains('\r'), "{plain:?}");
        // With colour on, only Velocity's own sequences (`\x1b[…m`) appear.
        let coloured = r.render(true);
        assert!(
            !coloured.contains("\x1b[2J") && !coloured.contains("\x1b]"),
            "{coloured:?}"
        );
    }

    #[test]
    fn lines_align_and_fixes_are_prefixed() {
        let r = Report::new(
            vec![
                Check::ok("host.os", Area::Host, "macOS (aarch64)"),
                Check::warn("host.leases", Area::Host, "2 stale lease files").with_fix(Fix::safe(
                    "velocity doctor --fix",
                    SafeAction::RemoveStaleLeases { paths: vec![] },
                )),
                Check::fail("ios.license", Area::Ios, "Xcode license not accepted")
                    .with_fix(Fix::run("sudo xcodebuild -license accept")),
                Check::skip("ai.network", Area::Ai, "needs --deep"),
            ],
            Duration::from_millis(1200),
        );
        let text = r.render(false);
        assert!(text.contains("  ✓ os       macOS (aarch64)\n"), "{text}");
        assert!(
            text.contains("  ! leases   2 stale lease files\n"),
            "{text}"
        );
        assert!(
            text.contains("Run: sudo xcodebuild -license accept\n"),
            "{text}"
        );
        assert!(text.contains("(--fix applies it)"), "{text}");
        assert!(text.contains("  - network  needs --deep"), "{text}");
        assert!(
            text.contains("Verdict: blocked (1 ok, 1 warning, 1 failure, 1 skipped)"),
            "{text}"
        );
        assert!(!text.contains('\x1b'));
        assert!(r.render(true).contains("\x1b[31;1m✗"));
    }
}
