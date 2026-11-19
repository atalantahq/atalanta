use std::io::Write;

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use velocity_common::{Result, StepStatus, SuiteResult, TestResult, TestStatus, VelocityError};

/// What a JUnit report carries besides the results.
#[derive(Debug, Clone, Default)]
pub struct JunitOptions {
    /// `<properties>` of the `<testsuite>`, in order (`velocity.termination`,
    /// `velocity.shard`, `velocity.seed`). Jenkins keeps them with
    /// `keepProperties`.
    pub properties: Vec<(String, String)>,
    /// `classname` of every testcase; `None` keeps `velocity` (A-10:
    /// `--junit-classname suite` passes the suite file's stem).
    pub classname: Option<String>,
}

/// Write a JUnit XML report for the suite result to the given file path.
pub fn write_junit(result: &SuiteResult, path: &str) -> Result<()> {
    write_junit_with(result, path, &JunitOptions::default())
}

/// [`write_junit`] with `options`.
pub fn write_junit_with(result: &SuiteResult, path: &str, options: &JunitOptions) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| VelocityError::io("failed to create report directory", e))?;
    }

    // Rendered in memory and then put in place whole: CI reading the
    // report (or a second run) never sees half a file.
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    // `failures` (the test's own checks failed) and `errors` (the device,
    // the driver or Velocity failed) are disjoint, as in the schema Jenkins
    // and GitLab read: an emulator dying mid-run is not 120 assertion
    // failures in the test trend.
    let errors = result
        .tests
        .iter()
        .filter(|t| error_kind(t).is_some())
        .count();
    let failures = result.failed.saturating_sub(errors).to_string();
    let errors = errors.to_string();

    // <testsuites>
    let mut testsuites = BytesStart::new("testsuites");
    testsuites.push_attribute(("tests", result.total.to_string().as_str()));
    testsuites.push_attribute(("failures", failures.as_str()));
    testsuites.push_attribute(("errors", errors.as_str()));
    testsuites.push_attribute((
        "time",
        format!("{:.3}", result.duration.as_secs_f64()).as_str(),
    ));
    writer
        .write_event(Event::Start(testsuites))
        .map_err(xml_err)?;

    // <testsuite>
    let mut testsuite = BytesStart::new("testsuite");
    testsuite.push_attribute(("name", "velocity"));
    testsuite.push_attribute(("tests", result.total.to_string().as_str()));
    testsuite.push_attribute(("failures", failures.as_str()));
    testsuite.push_attribute(("errors", errors.as_str()));
    testsuite.push_attribute(("skipped", result.skipped.to_string().as_str()));
    testsuite.push_attribute((
        "time",
        format!("{:.3}", result.duration.as_secs_f64()).as_str(),
    ));
    // UTC, so say so: without the zone parsers read it as local time.
    // When the run started (the report is written at its end), so
    // dashboards ordering by timestamp place the run where it began.
    let started = chrono::Utc::now()
        - chrono::Duration::from_std(result.duration).unwrap_or_else(|_| chrono::Duration::zero());
    let timestamp = started.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    testsuite.push_attribute(("timestamp", timestamp.as_str()));
    writer
        .write_event(Event::Start(testsuite))
        .map_err(xml_err)?;

    if !options.properties.is_empty() {
        writer
            .write_event(Event::Start(BytesStart::new("properties")))
            .map_err(xml_err)?;
        for (name, value) in &options.properties {
            let mut p = BytesStart::new("property");
            p.push_attribute(("name", name.as_str()));
            p.push_attribute(("value", value.as_str()));
            writer.write_event(Event::Empty(p)).map_err(xml_err)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("properties")))
            .map_err(xml_err)?;
    }

    let classname = options.classname.as_deref().unwrap_or("velocity");
    for test in &result.tests {
        write_testcase(&mut writer, test, classname)?;
    }

    // </testsuite>
    writer
        .write_event(Event::End(BytesEnd::new("testsuite")))
        .map_err(xml_err)?;

    // </testsuites>
    writer
        .write_event(Event::End(BytesEnd::new("testsuites")))
        .map_err(xml_err)?;

    let xml = xml_chars(&String::from_utf8_lossy(&writer.into_inner()));
    velocity_common::fsutil::write_atomic(std::path::Path::new(path), xml.as_bytes())
        .map_err(|e| VelocityError::io(format!("failed to write JUnit report at {path}"), e))
}

/// The document with ANSI colour codes stripped and every character XML
/// 1.0 forbids (C0 controls but tab, LF and CR; U+FFFE/FFFF) dropped. Error
/// messages carry device output, and one ESC made the whole report
/// unparsable for Jenkins and GitLab (VEL-048). The markup itself has none
/// of these, so cleaning the rendered text is safe.
fn xml_chars(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut chars = xml.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ params intermediates final-byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        let allowed =
            matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}');
        if allowed {
            out.push(c);
        }
    }
    out
}

/// The test's `<properties>`: `velocity.healed` when selectors were healed
/// (see [`write_healed`]) and its AI checks (`verify:`), one `verify.<at>`
/// property each, `outcome: claim (reason)`, which Jenkins and GitLab show
/// beside the test. One block, as the schema allows.
fn write_verification<W: Write>(writer: &mut Writer<W>, test: &TestResult) -> Result<()> {
    if test.verification.is_empty() {
        return write_healed(writer, test);
    }
    writer
        .write_event(Event::Start(BytesStart::new("properties")))
        .map_err(xml_err)?;
    let healed = test.healed_count();
    if healed > 0 {
        let mut prop = BytesStart::new("property");
        prop.push_attribute(("name", "velocity.healed"));
        prop.push_attribute(("value", healed.to_string().as_str()));
        writer.write_event(Event::Empty(prop)).map_err(xml_err)?;
    }
    for c in &test.verification {
        let mut p = BytesStart::new("property");
        let name = format!("verify.{}", c.at.replace(' ', "-"));
        let value = if c.reason.is_empty() {
            format!("{}: {}", c.outcome, c.claim)
        } else {
            format!("{}: {} ({})", c.outcome, c.claim, c.reason)
        };
        p.push_attribute(("name", name.as_str()));
        p.push_attribute(("value", value.as_str()));
        writer.write_event(Event::Empty(p)).map_err(xml_err)?;
    }
    writer
        .write_event(Event::End(BytesEnd::new("properties")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_testcase<W: Write>(
    writer: &mut Writer<W>,
    test: &TestResult,
    classname: &str,
) -> Result<()> {
    let mut testcase = BytesStart::new("testcase");
    testcase.push_attribute(("name", test.test_name.as_str()));
    testcase.push_attribute(("classname", classname));
    // `file` and `line` (pytest, Surefire) let CI tools link the test to
    // its source: the test's `- name:` line in the suite.
    if let Some(src) = &test.source {
        if !src.file.is_empty() {
            testcase.push_attribute(("file", src.file.as_str()));
        }
        testcase.push_attribute(("line", src.line.to_string().as_str()));
    }
    testcase.push_attribute((
        "time",
        format!("{:.3}", test.duration.as_secs_f64()).as_str(),
    ));

    match test.status {
        TestStatus::Failed => {
            writer
                .write_event(Event::Start(testcase))
                .map_err(xml_err)?;
            write_verification(writer, test)?;

            // `<error>` for a device/driver or internal failure,
            // `<failure>` for the test's own checks.
            let (tag, kind) = match error_kind(test) {
                Some(kind) => ("error", kind),
                None => ("failure", "AssertionError"),
            };
            let mut failure = BytesStart::new(tag);
            let msg = test.error_message.as_deref().unwrap_or("Test failed");
            failure.push_attribute(("message", msg));
            failure.push_attribute(("type", kind));

            // Build failure body with step details
            let body = build_failure_body(test);
            writer.write_event(Event::Start(failure)).map_err(xml_err)?;
            writer
                .write_event(Event::Text(BytesText::new(&body)))
                .map_err(xml_err)?;
            writer
                .write_event(Event::End(BytesEnd::new(tag)))
                .map_err(xml_err)?;
            for msg in &test.failed_attempts {
                write_attempt(writer, "rerunFailure", msg)?;
            }

            // system-out with screenshot and evidence paths (the
            // `[[ATTACHMENT|path]]` convention Jenkins and GitLab read)
            if !test.screenshots.is_empty() || !test.attachments.is_empty() {
                writer
                    .write_event(Event::Start(BytesStart::new("system-out")))
                    .map_err(xml_err)?;
                let screenshot_text = test
                    .screenshots
                    .iter()
                    .chain(test.attachments.iter().map(|a| &a.path))
                    .map(|p| format!("[[ATTACHMENT|{}]]", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                writer
                    .write_event(Event::Text(BytesText::new(&screenshot_text)))
                    .map_err(xml_err)?;
                writer
                    .write_event(Event::End(BytesEnd::new("system-out")))
                    .map_err(xml_err)?;
            }

            writer
                .write_event(Event::End(BytesEnd::new("testcase")))
                .map_err(xml_err)?;
        }
        TestStatus::Skipped => {
            writer
                .write_event(Event::Start(testcase))
                .map_err(xml_err)?;
            write_verification(writer, test)?;
            let mut skipped = BytesStart::new("skipped");
            // Why it was skipped (dependency failed, --fail-fast, timeout),
            // which Jenkins and GitLab show (VEL-178).
            // Always a message: GitLab shows only the attribute, and an
            // empty `<skipped/>` read as an unexplained skip (C2).
            let why = test
                .error_message
                .as_deref()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("skipped");
            skipped.push_attribute(("message", why));
            writer.write_event(Event::Empty(skipped)).map_err(xml_err)?;
            writer
                .write_event(Event::End(BytesEnd::new("testcase")))
                .map_err(xml_err)?;
        }
        TestStatus::Retried => {
            // Passed on retry. Surefire's <flakyFailure> keeps the test green
            // while CI dashboards (GitLab, Jenkins, Buildkite, Trunk…) still see
            // that it flaked and why.
            writer
                .write_event(Event::Start(testcase))
                .map_err(xml_err)?;
            write_verification(writer, test)?;
            for msg in &test.failed_attempts {
                write_attempt(writer, "flakyFailure", msg)?;
            }
            write_system_out(writer, test)?;
            writer
                .write_event(Event::End(BytesEnd::new("testcase")))
                .map_err(xml_err)?;
        }
        TestStatus::Passed => {
            if test.warnings.is_empty() && test.verification.is_empty() {
                writer
                    .write_event(Event::Empty(testcase))
                    .map_err(xml_err)?;
            } else {
                writer
                    .write_event(Event::Start(testcase))
                    .map_err(xml_err)?;
                write_verification(writer, test)?;
                write_system_out(writer, test)?;
                writer
                    .write_event(Event::End(BytesEnd::new("testcase")))
                    .map_err(xml_err)?;
            }
        }
    }

    Ok(())
}

/// The JUnit error type of a failed test that is not the test's own fault:
/// `InternalError` (a bug in Velocity, exit 5) or `InfrastructureError`
/// (the device or driver failed). `None` for an assertion failure.
fn error_kind(test: &TestResult) -> Option<&'static str> {
    if test.status != TestStatus::Failed {
        return None;
    }
    if test.internal_error {
        Some("InternalError")
    } else if test.infrastructure_failure {
        Some("InfrastructureError")
    } else {
        None
    }
}

/// One earlier attempt (`flakyFailure` / `rerunFailure`, Surefire format).
fn write_attempt<W: Write>(writer: &mut Writer<W>, tag: &str, message: &str) -> Result<()> {
    let mut el = BytesStart::new(tag);
    el.push_attribute(("message", first_line(message)));
    el.push_attribute(("type", "AssertionError"));
    writer.write_event(Event::Start(el)).map_err(xml_err)?;
    writer
        .write_event(Event::Text(BytesText::new(message)))
        .map_err(xml_err)?;
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .map_err(xml_err)
}

/// `<properties><property name="velocity.healed" value="n"/></properties>`
/// for a passing test whose selectors were healed (D-7 / R4).
fn write_healed<W: Write>(writer: &mut Writer<W>, test: &TestResult) -> Result<()> {
    let n = test.healed_count();
    if n == 0 {
        return Ok(());
    }
    writer
        .write_event(Event::Start(BytesStart::new("properties")))
        .map_err(xml_err)?;
    let mut prop = BytesStart::new("property");
    prop.push_attribute(("name", "velocity.healed"));
    prop.push_attribute(("value", n.to_string().as_str()));
    writer.write_event(Event::Empty(prop)).map_err(xml_err)?;
    writer
        .write_event(Event::End(BytesEnd::new("properties")))
        .map_err(xml_err)
}

/// Warnings (heals, re-plans, infrastructure re-runs) in <system-out>,
/// after `PASSED (healed n)` when selectors were healed.
fn write_system_out<W: Write>(writer: &mut Writer<W>, test: &TestResult) -> Result<()> {
    if test.warnings.is_empty() {
        return Ok(());
    }
    writer
        .write_event(Event::Start(BytesStart::new("system-out")))
        .map_err(xml_err)?;
    let text = test
        .healed_label()
        .into_iter()
        .chain(test.warnings.iter().map(|w| format!("WARNING: {w}")))
        .collect::<Vec<_>>()
        .join("\n");
    writer
        .write_event(Event::Text(BytesText::new(&text)))
        .map_err(xml_err)?;
    writer
        .write_event(Event::End(BytesEnd::new("system-out")))
        .map_err(xml_err)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn build_failure_body(test: &TestResult) -> String {
    let mut lines = Vec::new();
    for step in &test.steps {
        let status = match step.status {
            StepStatus::Passed => "PASS",
            StepStatus::Failed => "FAIL",
            StepStatus::Skipped => "SKIP",
        };
        lines.push(format!(
            "Step {}: {} [{}] ({:.1}s)",
            step.step_index,
            step.action_name,
            status,
            step.duration.as_secs_f64()
        ));
        if let Some(ref err) = step.error_message {
            lines.push(format!("  Error: {err}"));
        }
    }
    lines.join("\n")
}

fn xml_err(e: std::io::Error) -> VelocityError {
    VelocityError::Config(format!("XML write error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn suite(tests: Vec<TestResult>) -> SuiteResult {
        SuiteResult {
            total: tests.len(),
            passed: 0,
            failed: tests
                .iter()
                .filter(|t| t.status == TestStatus::Failed)
                .count(),
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests,
            shard_index: None,
            shard_total: None,
        }
    }

    fn render(tests: Vec<TestResult>) -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "vel-junit-{}-{}.xml",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        write_junit(&suite(tests), path.to_str().unwrap()).unwrap();
        std::fs::read_to_string(path).unwrap()
    }

    /// Review 12 #7: a dead device is an `<error>`, counted in `errors`,
    /// not an assertion failure.
    #[test]
    fn infrastructure_and_internal_failures_are_errors_not_failures() {
        let assertion = TestResult::failed("a", "expected X".into(), Duration::ZERO);
        let mut infra = TestResult::failed("b", "ADB connection lost".into(), Duration::ZERO);
        infra.infrastructure_failure = true;
        let mut internal = TestResult::failed("c", "panicked".into(), Duration::ZERO);
        internal.internal_error = true;
        let xml = render(vec![assertion, infra, internal]);
        assert!(xml.contains(r#"failures="1""#), "{xml}");
        assert!(xml.contains(r#"errors="2""#), "{xml}");
        assert!(
            xml.contains(r#"<error message="ADB connection lost" type="InfrastructureError">"#),
            "{xml}"
        );
        assert!(xml.contains(r#"type="InternalError""#), "{xml}");
        assert!(
            xml.contains(r#"<failure message="expected X" type="AssertionError">"#),
            "{xml}"
        );
    }

    #[test]
    fn verification_checks_become_testcase_properties() {
        let mut t = TestResult::failed("buy", "AI verification failed".into(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.verification = vec![velocity_common::VerifyCheck {
            at: "final".into(),
            claim: "the order is placed".into(),
            outcome: velocity_common::VerifyOutcome::Holds,
            reason: "\"Order #12\" shown".into(),
            evidence: vec!["[3] <text> text=\"Order #12\" @1,1".into()],
        }];
        let dir = std::env::temp_dir().join(format!("vel-junit-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.xml");
        let _ = (&dir, &path);
        let mut both = healed();
        both.verification = t.verification.clone();
        let xml = render(vec![t, both]);
        assert!(
            xml.contains(r#"<property name="verify.final" value="holds: the order is placed"#),
            "{xml}"
        );
        // A healed test with checks keeps one <properties> block.
        assert_eq!(xml.matches("<properties>").count(), 2, "{xml}");
        assert!(
            xml.contains(r#"<property name="velocity.healed" value="1"/>"#),
            "{xml}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn healed() -> TestResult {
        let mut t = TestResult::failed("login", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.warnings = vec!["selector text=\"Log in\" not found; healed to text=\"Sign in\" (confidence 0.92) — update the selector".into()];
        t
    }

    /// D-7 / R4: a heal shows as `PASSED (healed n)` and a property.
    #[test]
    fn healed_passes_carry_a_property_and_say_so() {
        let xml = render(vec![healed()]);
        assert!(
            xml.contains(r#"<property name="velocity.healed" value="1"/>"#),
            "{xml}"
        );
        assert!(xml.contains("PASSED (healed 1)"), "{xml}");
    }

    #[test]
    fn evidence_is_attached_in_system_out() {
        let mut t = TestResult::failed("t", "boom".into(), std::time::Duration::ZERO);
        t.attachments = vec![velocity_common::Attachment::new(
            velocity_common::AttachmentKind::Video,
            "velocity-results/traces/t/video.mp4",
        )];
        let xml = render(vec![t]);
        assert!(
            xml.contains("[[ATTACHMENT|velocity-results/traces/t/video.mp4]]"),
            "{xml}"
        );
    }

    #[test]
    fn flaky_tests_carry_flaky_failures() {
        let mut flaky = TestResult::failed("checkout", String::new(), Duration::ZERO);
        flaky.status = TestStatus::Retried;
        flaky.error_message = None;
        flaky.failed_attempts = vec!["Element not found: text=\"Pay\"".into()];
        flaky.warnings = vec!["flaky: failed 1x before passing".into()];
        let xml = render(vec![flaky]);
        assert!(xml.contains("<flakyFailure"), "{xml}");
        assert!(
            !xml.contains("<failure"),
            "a flaky pass is not a failure: {xml}"
        );
        assert!(xml.contains("WARNING: flaky"), "{xml}");
    }

    #[test]
    fn failed_tests_carry_rerun_failures() {
        let mut t = TestResult::failed("login", "final error".into(), Duration::ZERO);
        t.failed_attempts = vec!["first error".into()];
        let xml = render(vec![t, TestResult::failed("x", "y".into(), Duration::ZERO)]);
        assert!(xml.contains("<failure"), "{xml}");
        assert!(
            xml.contains("<rerunFailure message=\"first error\""),
            "{xml}"
        );
    }

    #[test]
    fn testcases_carry_the_file_and_line_of_their_test() {
        let mut t = TestResult::failed("login", "boom".into(), Duration::ZERO);
        t.source = Some(velocity_common::SourcePos::new("tests/app.yaml", 12, 5));
        let other = TestResult::failed("x", "y".into(), Duration::ZERO);
        let third = TestResult::failed("z", "y".into(), Duration::ZERO);
        let xml = render(vec![t, other, third]);
        assert!(
            xml.contains(r#"name="login" classname="velocity" file="tests/app.yaml" line="12""#),
            "{xml}"
        );
        assert!(
            !xml.contains(r#"name="x" classname="velocity" file="#),
            "{xml}"
        );
    }

    // VEL-048: ANSI colours and control characters from device output made
    // the report ill-formed XML.
    #[test]
    fn control_characters_never_reach_the_xml() {
        let msg = "\u{1b}[31mElement not found\u{1b}[0m: got \u{0}\u{8}\u{b}x\ty";
        let mut t = TestResult::failed("ctl", msg.into(), Duration::ZERO);
        t.failed_attempts = vec!["first\u{1}".into()];
        let xml = render(vec![t]);
        assert!(
            !xml.chars()
                .any(|c| c < ' ' && !matches!(c, '\t' | '\n' | '\r')),
            "{xml:?}"
        );
        assert!(xml.contains("Element not found: got x\ty"), "{xml}");
        assert!(!xml.contains("[31m"), "{xml}");
    }

    // VEL-178: <skipped/> dropped the reason.
    #[test]
    fn skipped_tests_say_why() {
        let t = TestResult::skipped("later", "Skipped: dependency 'login' did not pass");
        let xml = render(vec![t]);
        assert!(
            xml.contains(
                r#"<skipped message="Skipped: dependency &apos;login&apos; did not pass"/>"#
            ) || xml.contains(r#"<skipped message="Skipped: dependency 'login' did not pass"/>"#),
            "{xml}"
        );
    }

    // C2: a skip with no reason (or an empty one) still has a message.
    #[test]
    fn every_skipped_element_has_a_message() {
        let mut none = TestResult::skipped("a", "");
        none.error_message = None;
        let blank = TestResult::skipped("b", "  ");
        let xml = render(vec![none, blank]);
        assert_eq!(
            xml.matches(r#"<skipped message="skipped"/>"#).count(),
            2,
            "{xml}"
        );
        assert!(!xml.contains("<skipped/>"), "{xml}");
    }
}
