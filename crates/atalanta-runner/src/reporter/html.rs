// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! A single self-contained HTML report: every test with its steps (time,
//! settle time, error, line in the suite), warnings, earlier failed
//! attempts and failure screenshots. No scripts or external assets, so it
//! opens from disk or a CI artifact as is.
//!
//! A test with a kept trace (`traces/<test>/`) gets an inspector per step:
//! clicking the step opens (CSS `:target`) the failure screenshot, or a
//! wireframe of the screen when there is none, with the matched element
//! boxed (or the nearest ones, for an element that wasn't found) and the
//! screen's tree beside it.

use std::fmt::Write as _;
use std::path::Path;

use atalanta_common::{
    AtalantaError, Element, Rect, Result, StepResult, StepStatus, SuiteResult, TestResult,
    TestStatus,
};
use atalanta_core::trace::{StepTrace, TestTrace, TracedElement};

pub fn write_html(result: &SuiteResult, path: &str) -> Result<()> {
    let base = Path::new(path).parent().unwrap_or(Path::new("."));
    atalanta_common::fsutil::write_atomic(Path::new(path), render(result, base).as_bytes())
        .map_err(|e| AtalantaError::Config(format!("write {path}: {e}")))
}

pub fn render(result: &SuiteResult, base: &Path) -> String {
    let mut h = String::with_capacity(16 * 1024);
    h.push_str(HEAD);
    let _ = write!(
        h,
        r#"<header><h1>Atalanta report</h1><p class="totals">
<span class="pill pass">{} passed</span><span class="pill fail">{} failed</span>
<span class="pill flaky">{} flaky</span><span class="pill skip">{} skipped</span>
<span class="muted">{:.1}s</span></p></header><main>"#,
        result.passed,
        result.failed,
        result.retried,
        result.skipped,
        result.duration.as_secs_f64()
    );
    // Failures first, then flaky, then the rest in run order.
    let mut tests: Vec<&TestResult> = result.tests.iter().collect();
    tests.sort_by_key(|t| match t.status {
        TestStatus::Failed => 0,
        TestStatus::Retried => 1,
        _ if !t.warnings.is_empty() => 2,
        _ => 3,
    });
    // An index of the failed tests' trace viewers.
    let traced: Vec<(&TestResult, &atalanta_common::Attachment)> = tests
        .iter()
        .filter(|t| t.status == TestStatus::Failed)
        .filter_map(|t| {
            t.attachments
                .iter()
                .find(|a| a.kind == atalanta_common::AttachmentKind::Trace)
                .map(|a| (*t, a))
        })
        .collect();
    if !traced.is_empty() {
        h.push_str(r#"<section class="traces"><h2>Traces of failed tests</h2><ul>"#);
        for (t, a) in traced {
            let href = esc(&url_path(&atalanta_core::trace_html::relative(
                &a.path, base,
            )));
            let _ = write!(h, r#"<li><a href="{href}">{}</a></li>"#, esc(&t.test_name));
        }
        h.push_str("</ul></section>");
    }
    for (n, t) in tests.into_iter().enumerate() {
        render_test(&mut h, t, base, n);
    }
    h.push_str("</main></body></html>\n");
    h
}

fn render_test(h: &mut String, t: &TestResult, base: &Path, n: usize) {
    let quarantined = t.quarantined.is_some();
    let healed = t.healed_count();
    let (class, label) = match t.status {
        // D-7 / R4: `PASSED (healed n)`, marked like a flake.
        TestStatus::Passed if healed > 0 => ("flaky", format!("passed (healed {healed})")),
        TestStatus::Passed => ("pass", "passed".to_string()),
        TestStatus::Failed if quarantined => ("skip", "quarantined".to_string()),
        TestStatus::Failed => ("fail", "failed".to_string()),
        TestStatus::Retried => ("flaky", "flaky".to_string()),
        TestStatus::Skipped => ("skip", "skipped".to_string()),
    };
    let open = if t.status == TestStatus::Failed {
        " open"
    } else {
        ""
    };
    let _ = write!(
        h,
        r#"<details class="test"{open}><summary><span class="pill {class}">{label}</span> <b>{}</b> <span class="muted">{:.2}s{}{}</span></summary>"#,
        esc(&t.test_name),
        t.duration.as_secs_f64(),
        t.source
            .as_ref()
            .map(|p| format!(" · {}", esc(&p.to_string())))
            .unwrap_or_default(),
        if t.warnings.is_empty() {
            String::new()
        } else {
            format!(" · {} warning(s)", t.warnings.len())
        }
    );
    if let Some(err) = &t.error_message {
        let _ = write!(h, r#"<pre class="error">{}</pre>"#, esc(err));
    }
    if !t.warnings.is_empty() {
        h.push_str(r#"<ul class="warnings">"#);
        for w in &t.warnings {
            let _ = write!(h, "<li>{}</li>", esc(w));
        }
        h.push_str("</ul>");
    }
    if !t.verification.is_empty() {
        h.push_str("<p><b>AI verification</b></p><ul class=\"verify\">");
        for c in &t.verification {
            let class = if c.outcome.passed() { "pass" } else { "fail" };
            let _ = write!(
                h,
                r#"<li><span class="pill {class}">{}</span> {} <span class="muted">{}{}</span></li>"#,
                esc(&c.outcome.to_string()),
                esc(&format!("{}: {}", c.at, c.claim)),
                esc(&c.reason),
                if c.evidence.is_empty() {
                    String::new()
                } else {
                    format!(" — evidence: {}", esc(&c.evidence.join("; ")))
                }
            );
        }
        h.push_str("</ul>");
    }
    if !t.failed_attempts.is_empty() {
        h.push_str("<p><b>Earlier attempts</b></p><ol class=\"attempts\">");
        for a in &t.failed_attempts {
            let _ = write!(h, "<li><pre>{}</pre></li>", esc(a));
        }
        h.push_str("</ol>");
    }
    // The kept trace, if any; best-effort (a moved or deleted trace just
    // means no inspector).
    let trace: Option<(TestTrace, std::collections::HashMap<String, Element>)> = t
        .trace
        .as_deref()
        .and_then(|p| atalanta_core::trace::load_trace(p).ok());
    if !t.steps.is_empty() {
        h.push_str(
            "<table><thead><tr><th>#</th><th>Step</th><th>Result</th><th>Time</th><th>Settle</th><th>Detail</th></tr></thead><tbody>",
        );
        let mut panels = String::new();
        for s in &t.steps {
            let (sc, sl) = match s.status {
                StepStatus::Passed => ("pass", "ok"),
                StepStatus::Failed => ("fail", "failed"),
                StepStatus::Skipped => ("skip", "skipped"),
            };
            let traced = trace
                .as_ref()
                .and_then(|(tr, trees)| step_panel(t, s, tr, trees, base, n, &mut panels));
            let name = match &traced {
                Some(id) => format!(r##"<a href="#{id}">{}</a>"##, esc(&s.action_name)),
                None => esc(&s.action_name),
            };
            let line = s
                .source
                .as_ref()
                .map(|p| format!(r#"<br><span class="muted">{}</span>"#, esc(&p.to_string())))
                .unwrap_or_default();
            let _ = write!(
                h,
                r#"<tr><td>{}</td><td>{name}{line}</td><td><span class="dot {sc}"></span>{sl}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
                s.step_index + 1,
                if s.status == StepStatus::Skipped {
                    String::new()
                } else {
                    format!("{} ms", s.duration.as_millis())
                },
                s.settle_ms.map(|m| format!("{m} ms")).unwrap_or_default(),
                s.error_message.as_deref().map(esc).unwrap_or_default()
            );
        }
        h.push_str("</tbody></table>");
        h.push_str(&panels);
    }
    for shot in &t.screenshots {
        let rel = shot.strip_prefix(base).unwrap_or(shot);
        let src = esc(&url_path(&rel.to_string_lossy()));
        let _ = write!(
            h,
            r#"<a href="{src}"><img class="shot" src="{src}" alt="failure screenshot" loading="lazy"></a>"#
        );
    }
    // Video, logs and the trace viewer.
    if !t.attachments.is_empty() {
        h.push_str(r#"<p class="evidence">"#);
        for a in &t.attachments {
            let href = esc(&url_path(&atalanta_core::trace_html::relative(
                &a.path, base,
            )));
            let _ = write!(h, r#"<a href="{href}">{}</a> "#, esc(a.title()));
        }
        h.push_str("</p>");
        for a in t
            .attachments
            .iter()
            .filter(|a| a.kind == atalanta_common::AttachmentKind::Video)
        {
            let src = esc(&url_path(&atalanta_core::trace_html::relative(
                &a.path, base,
            )));
            let _ = write!(
                h,
                r#"<video class="shot" controls preload="none" muted src="{src}"></video>"#
            );
        }
    }
    h.push_str("</details>");
}

/// `path` as a relative URL: each byte outside the unreserved set (and
/// `/`) percent-encoded. A screenshot named `a#1.png` or `50%.png` linked
/// to the wrong file or none (VEL-180).
fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Tree lines shown beside a screen; the rest is cut (the full tree is in
/// the trace's `trees/` folder).
const TREE_LINES: usize = 400;

/// Write the inspector panel for step `s` into `out`; returns its anchor id,
/// or `None` when the trace has nothing for that step.
fn step_panel(
    t: &TestResult,
    s: &StepResult,
    trace: &TestTrace,
    trees: &std::collections::HashMap<String, Element>,
    base: &Path,
    n: usize,
    out: &mut String,
) -> Option<String> {
    let st: &StepTrace = trace.steps.iter().find(|x| x.index == s.step_index)?;
    let tree = st.trees.last().and_then(|h| trees.get(h));
    if tree.is_none() && st.matched.is_none() && st.screenshot.is_none() {
        return None;
    }
    let id = format!("t{n}s{}", s.step_index);
    let _ = write!(
        out,
        r##"<section class="inspect" id="{id}"><p><a href="#" class="muted">close</a> · <b>{}</b> {}</p>"##,
        esc(&st.label),
        st.source
            .as_ref()
            .map(|p| esc(&p.to_string()))
            .unwrap_or_default()
    );
    let mut facts = vec![format!("{} ms", st.duration_ms)];
    if st.settle_ms > 0 || st.settle_reads > 0 {
        facts.push(format!(
            "settle {} ms, {} read(s)",
            st.settle_ms, st.settle_reads
        ));
    }
    if let Some(m) = &st.matched {
        facts.push(format!("matched {}", m.describe));
    }
    let _ = write!(out, r#"<p class="muted">{}</p>"#, esc(&facts.join(" · ")));
    for note in st.heals.iter().chain(&st.warnings) {
        let _ = write!(out, r#"<p class="note">{}</p>"#, esc(note));
    }
    out.push_str(r#"<div class="panes">"#);
    // The screen: the failure screenshot when there is one, a wireframe of
    // the tree otherwise, with the element(s) that matter boxed.
    let viewport = tree.map(screen_rect).unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 1080,
        height: 1920,
    });
    let shot = st
        .screenshot
        .as_ref()
        .or(s.screenshot.as_ref())
        .or_else(|| {
            (s.status == StepStatus::Failed)
                .then(|| t.screenshots.last())
                .flatten()
        });
    let _ = write!(
        out,
        r#"<div class="screen" style="aspect-ratio:{} / {}">"#,
        viewport.width.max(1),
        viewport.height.max(1)
    );
    if let Some(shot) = shot {
        let rel = shot.strip_prefix(base).unwrap_or(shot);
        let _ = write!(
            out,
            r#"<img src="{}" alt="screen at this step">"#,
            esc(&rel.to_string_lossy())
        );
    }
    let _ = write!(
        out,
        r#"<svg viewBox="{} {} {} {}" preserveAspectRatio="none">"#,
        viewport.x,
        viewport.y,
        viewport.width.max(1),
        viewport.height.max(1)
    );
    if shot.is_none() {
        if let Some(tree) = tree {
            wireframe(tree, out);
        }
    }
    let boxed: Vec<(&TracedElement, &str)> = match &st.matched {
        Some(m) => vec![(m, "match")],
        None => st.nearest.iter().take(3).map(|e| (e, "near")).collect(),
    };
    for (el, class) in &boxed {
        let b = el.bounds;
        let _ = write!(
            out,
            r#"<rect class="{class}" x="{}" y="{}" width="{}" height="{}"><title>{}</title></rect>"#,
            b.x,
            b.y,
            b.width.max(1),
            b.height.max(1),
            esc(&match el.score {
                Some(score) => format!("{} (score {score:.2})", el.describe),
                None => el.describe.clone(),
            })
        );
    }
    out.push_str("</svg></div>");
    // The tree, with the boxed element's line marked.
    out.push_str(r#"<div class="tree">"#);
    if st.matched.is_none() && !st.nearest.is_empty() {
        out.push_str("<p><b>Nearest on screen</b></p><ol>");
        for e in &st.nearest {
            let _ = write!(
                out,
                "<li>{} <span class=\"muted\">{} [{},{} {}×{}]{}</span></li>",
                esc(&e.describe),
                esc(&e.element_type),
                e.bounds.x,
                e.bounds.y,
                e.bounds.width,
                e.bounds.height,
                e.score
                    .map(|s| format!(" score {s:.2}"))
                    .unwrap_or_default()
            );
        }
        out.push_str("</ol>");
    }
    if let Some(tree) = tree {
        let mark = boxed.first().map(|(e, _)| e.bounds);
        let mut lines = Vec::new();
        outline(tree, 0, mark, &mut lines);
        let total = lines.len();
        out.push_str("<pre>");
        for line in lines.into_iter().take(TREE_LINES) {
            out.push_str(&line);
            out.push('\n');
        }
        if total > TREE_LINES {
            let _ = writeln!(out, "… {} more", total - TREE_LINES);
        }
        out.push_str("</pre>");
    }
    out.push_str("</div></div></section>");
    Some(id)
}

/// The screen's extent: the root's bounds, or the union of its children's.
fn screen_rect(tree: &Element) -> Rect {
    if !tree.bounds.is_empty() {
        return tree.bounds;
    }
    let (mut w, mut h) = (0, 0);
    fn walk(e: &Element, w: &mut i32, h: &mut i32) {
        *w = (*w).max(e.bounds.x + e.bounds.width);
        *h = (*h).max(e.bounds.y + e.bounds.height);
        for c in &e.children {
            walk(c, w, h);
        }
    }
    walk(tree, &mut w, &mut h);
    Rect {
        x: 0,
        y: 0,
        width: w.max(1),
        height: h.max(1),
    }
}

/// Thin outlines of the visible elements, for a screen with no screenshot.
fn wireframe(e: &Element, out: &mut String) {
    if e.visible && !e.bounds.is_empty() {
        let b = e.bounds;
        let _ = write!(
            out,
            r#"<rect class="wire" x="{}" y="{}" width="{}" height="{}"/>"#,
            b.x, b.y, b.width, b.height
        );
    }
    for c in &e.children {
        wireframe(c, out);
    }
}

/// One escaped line per element: `Button "Pay" #pay [0,200 100×40]`.
fn outline(e: &Element, depth: usize, mark: Option<Rect>, out: &mut Vec<String>) {
    let mut line = format!("{}{}", "  ".repeat(depth), esc(&e.element_type));
    if let Some(t) = e.text.as_deref().filter(|t| !t.is_empty()) {
        let _ = write!(line, " {}", esc(&format!("{t:?}")));
    }
    if let Some(l) = e.label.as_deref().filter(|l| !l.is_empty()) {
        let _ = write!(line, " label={}", esc(&format!("{l:?}")));
    }
    if !e.platform_id.is_empty() {
        let _ = write!(line, " #{}", esc(&e.platform_id));
    }
    let b = e.bounds;
    let _ = write!(
        line,
        r#" <span class="muted">[{},{} {}×{}]</span>"#,
        b.x, b.y, b.width, b.height
    );
    if !e.visible {
        line.push_str(r#" <span class="muted">hidden</span>"#);
    }
    if mark == Some(b) && !b.is_empty() {
        line = format!("<mark>{line}</mark>");
    }
    out.push(line);
    for c in &e.children {
        outline(c, depth + 1, mark, out);
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const HEAD: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Atalanta report</title>
<style>
:root{--bg:#fff;--fg:#1b1f24;--muted:#656d76;--line:#d0d7de;--card:#f6f8fa;
--pass:#1a7f37;--fail:#cf222e;--flaky:#9a6700;--skip:#6e7781}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#8d96a0;
--line:#30363d;--card:#161b22;--pass:#3fb950;--fail:#f85149;--flaky:#d29922;--skip:#8d96a0}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);
font:14px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}
header,main{max-width:1100px;margin:0 auto;padding:16px}
h1{font-size:20px;margin:8px 0}.muted{color:var(--muted)}
.pill{display:inline-block;padding:1px 8px;border-radius:999px;font-size:12px;
font-weight:600;color:#fff;margin-right:6px}
.pass{background:var(--pass)}.fail{background:var(--fail)}.flaky{background:var(--flaky)}
.skip{background:var(--skip)}
details.test{border:1px solid var(--line);border-radius:8px;margin:8px 0;
background:var(--card);padding:8px 12px}
summary{cursor:pointer}pre{white-space:pre-wrap;word-break:break-word;margin:6px 0}
pre.error{color:var(--fail)}ul.warnings li{color:var(--flaky)}
table{width:100%;border-collapse:collapse;margin-top:8px;font-size:13px;display:block;
overflow-x:auto}th,td{text-align:left;padding:4px 8px;border-top:1px solid var(--line);
vertical-align:top}.dot{display:inline-block;width:8px;height:8px;border-radius:50%;
margin-right:6px}img.shot,video.shot{max-width:240px;border:1px solid var(--line);border-radius:6px;
margin:8px 8px 0 0}
section.inspect{display:none;border:1px solid var(--line);border-radius:8px;
padding:8px 12px;margin-top:8px;background:var(--bg)}section.inspect:target{display:block}
.panes{display:flex;flex-wrap:wrap;gap:12px;align-items:flex-start}
.screen{position:relative;width:min(320px,100%);border:1px solid var(--line);
border-radius:6px;overflow:hidden;background:var(--card)}
.screen img,.screen svg{position:absolute;inset:0;width:100%;height:100%}
.screen img{object-fit:fill}
rect.wire{fill:none;stroke:var(--muted);stroke-width:2;vector-effect:non-scaling-stroke;opacity:.5}
rect.match{fill:rgba(26,127,55,.15);stroke:var(--pass);stroke-width:3;vector-effect:non-scaling-stroke}
rect.near{fill:rgba(154,103,0,.12);stroke:var(--flaky);stroke-width:2;stroke-dasharray:6 4;
vector-effect:non-scaling-stroke}
.tree{flex:1;min-width:260px;max-height:640px;overflow:auto;font-size:12px}
.tree pre{margin:0}p.note{color:var(--flaky);margin:4px 0}
mark{background:rgba(26,127,55,.25);color:inherit}
</style></head><body>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::StepResult;
    use std::time::Duration;

    fn one_failed_with_shot(shot: &str) -> SuiteResult {
        let mut bad = TestResult::failed("t", "x".into(), Duration::ZERO);
        bad.screenshots.push(std::path::PathBuf::from(shot));
        SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests: vec![bad],
            shard_index: None,
            shard_total: None,
        }
    }

    fn healed() -> TestResult {
        let mut t = TestResult::failed("login", String::new(), Duration::ZERO);
        t.status = TestStatus::Passed;
        t.error_message = None;
        t.warnings = vec!["selector text=\"Log in\" not found; healed to text=\"Sign in\" (confidence 0.92) — update the selector".into()];
        t
    }

    #[test]
    fn healed_passes_say_so() {
        let result = SuiteResult {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests: vec![healed()],
            shard_index: None,
            shard_total: None,
        };
        let html = render(&result, Path::new("."));
        assert!(html.contains("passed (healed 1)"), "{html}");
    }

    #[test]
    fn ai_checks_are_listed_with_their_evidence() {
        let mut t = healed();
        t.verification = vec![atalanta_common::VerifyCheck {
            at: "final".into(),
            claim: "the <cart> is open".into(),
            outcome: atalanta_common::VerifyOutcome::Unknown,
            reason: "cited no element".into(),
            evidence: vec![],
        }];
        let result = SuiteResult {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let html = render(&result, Path::new("."));
        assert!(html.contains("AI verification"), "{html}");
        assert!(
            html.contains(r#"<span class="pill fail">unknown</span>"#),
            "{html}"
        );
        assert!(html.contains("final: the &lt;cart&gt; is open"), "{html}");
    }

    // VEL-180: screenshot links weren't URL-encoded.
    #[test]
    fn screenshot_links_are_url_encoded() {
        let html = render(
            &one_failed_with_shot("out/shots/Pay #1 50%.png"),
            Path::new("out"),
        );
        assert!(
            html.contains(r#"href="shots/Pay%20%231%2050%25.png""#),
            "{html}"
        );
    }

    // VEL-179: the report into a folder that doesn't exist yet.
    #[test]
    fn evidence_is_linked_and_indexed() {
        let mut r = one_failed_with_shot("/r/shot.png");
        r.tests[0].attachments = vec![
            atalanta_common::Attachment::new(
                atalanta_common::AttachmentKind::Video,
                "/r/traces/t/video.mp4",
            ),
            atalanta_common::Attachment::new(
                atalanta_common::AttachmentKind::Trace,
                "/r/traces/t/trace.html",
            ),
        ];
        let html = render(&r, Path::new("/r"));
        assert!(
            html.contains(
                r#"<video class="shot" controls preload="none" muted src="traces/t/video.mp4">"#
            ),
            "{html}"
        );
        assert!(html.contains(r#"<a href="traces/t/trace.html">trace</a>"#));
        assert!(html.contains("Traces of failed tests"));
    }

    #[test]
    fn the_report_creates_its_folder() {
        let dir = std::env::temp_dir().join(format!("vel-html-179-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("a").join("b").join("report.html");
        write_html(&one_failed_with_shot("x.png"), path.to_str().unwrap()).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_failures_first_and_escapes_text() {
        let mut ok = TestResult::failed("fine", String::new(), Duration::ZERO);
        ok.status = TestStatus::Passed;
        ok.error_message = None;
        let mut bad =
            TestResult::failed("<script>", "Element not found: <b>".into(), Duration::ZERO);
        bad.steps.push(StepResult {
            step_index: 0,
            action_name: "tap".into(),
            status: StepStatus::Failed,
            duration: Duration::from_millis(1200),
            screenshot: None,
            error_message: Some("nope".into()),
            resource_delta: None,
            settle_ms: Some(300),
            source: None,
        });
        bad.screenshots
            .push(std::path::PathBuf::from("./atalanta-results/shot.png"));
        let result = SuiteResult {
            total: 2,
            passed: 1,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(2),
            tests: vec![ok, bad],
            shard_index: None,
            shard_total: None,
        };
        let html = render(&result, Path::new("./atalanta-results"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.find("&lt;script&gt;").unwrap() < html.find("fine").unwrap());
        assert!(html.contains("300 ms"));
        assert!(
            html.contains(r#"src="shot.png""#),
            "screenshot path relative to the report"
        );
    }

    #[test]
    fn a_traced_step_opens_its_screen_with_the_nearest_element_boxed() {
        use atalanta_common::{Selector, SourcePos, TraceMode};
        use atalanta_core::trace::TraceRecorder;
        let dir = tempfile::tempdir().unwrap();
        let button = |text: &str, y: i32| Element {
            hint: None,
            platform_id: String::new(),
            label: None,
            text: Some(text.into()),
            element_type: "Button".into(),
            bounds: Rect {
                x: 10,
                y,
                width: 200,
                height: 50,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        };
        let screen = Element {
            text: None,
            element_type: "hierarchy".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            children: vec![button("Cart", 100), button("Pay now", 300)],
            ..button("", 0)
        };
        let step = |i: usize, status, err: Option<&str>| StepResult {
            step_index: i,
            action_name: "tap".into(),
            status,
            duration: Duration::from_millis(20),
            screenshot: None,
            error_message: err.map(String::from),
            resource_delta: None,
            settle_ms: Some(4),
            source: Some(SourcePos::new("tests/app.yaml", 10 + i, 9)),
        };
        let mut rec = TraceRecorder::new(TraceMode::OnFailure);
        rec.begin_test();
        rec.begin_step(0, "tap { text: \"Cart\" }".into(), None);
        rec.record_tree(&screen);
        rec.record_match(&screen.children[0]);
        rec.end_step(&step(0, StepStatus::Passed, None), vec![], vec![]);
        rec.begin_step(
            1,
            "tap { text: \"Pay\" }".into(),
            Some(Selector::Text("Pay".into())),
        );
        rec.record_tree(&screen);
        let failed = step(1, StepStatus::Failed, Some("Element not found"));
        rec.end_step(&failed, vec![], vec![]);
        rec.fill_nearest(None);
        let trace = rec
            .finish(dir.path(), "pay", "d", None, 0, TestStatus::Failed)
            .unwrap();

        let mut t = TestResult::failed("pay", "Element not found".into(), Duration::ZERO);
        t.source = Some(SourcePos::new("tests/app.yaml", 8, 5));
        t.steps = vec![step(0, StepStatus::Passed, None), failed];
        t.trace = Some(trace);
        let result = SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            retried: 0,
            duration: Duration::from_secs(1),
            tests: vec![t],
            shard_index: None,
            shard_total: None,
        };
        let html = render(&result, dir.path());
        assert!(html.contains("tests/app.yaml:8"), "test line shown");
        assert!(html.contains("tests/app.yaml:11"), "step line shown");
        assert!(
            html.contains(r##"<a href="#t0s1">tap</a>"##),
            "step links to its panel"
        );
        assert!(html.contains(r#"<section class="inspect" id="t0s1">"#));
        assert!(
            html.contains(r#"<rect class="match" x="10" y="100""#),
            "matched box"
        );
        assert!(
            html.contains(r#"<rect class="near" x="10" y="300""#),
            "nearest box"
        );
        assert!(html.contains("<mark>"), "boxed element marked in the tree");
        assert!(html.contains(r#"<svg viewBox="0 0 400 800""#));
        assert!(!html.contains("<script"), "still no scripts");
    }
}
