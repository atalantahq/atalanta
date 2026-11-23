//! `inspect_trace`: read a run's traces (`<artifacts>/traces/<test>/
//! steps*.json`) a piece at a time.
//!
//! - `summary`: one line per step (status, what it did, time, error).
//! - `search`: the steps whose label, selector, error, matched element or
//!   screen text contain `query`.
//! - `step_details`: one step in full: selector, matched element, the
//!   nearest elements when nothing matched, settle timing, heals, warnings.
//! - `step_screenshot`: the screen the step acted on, as a PNG with a red
//!   ring where it tapped (the matched element's centre).

use std::path::{Path, PathBuf};

use serde_json::Value;
use velocity_common::{Element, Rect, Result, StepStatus, VelocityError};
use velocity_core::trace::{load_trace, StepTrace, TestTrace};

/// What `inspect_trace` returns: JSON for the text block and maybe an image.
pub struct Inspected {
    pub value: Value,
    pub png: Option<Vec<u8>>,
}

/// The newest `steps*.json` of `test` (or of any test) under `artifacts`.
pub fn find_trace(artifacts: &Path, test: Option<&str>) -> Result<PathBuf> {
    let mut files = crate::tools::failure::trace_files(artifacts, test);
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files.into_iter().next().map(|(p, _)| p).ok_or_else(|| {
        VelocityError::Config(match test {
            Some(t) => format!(
                "no trace for '{t}' under {}; runs keep traces for failed tests (run_task keeps every test's)",
                artifacts.join("traces").display()
            ),
            None => format!(
                "no trace under {}; run the suite first",
                artifacts.join("traces").display()
            ),
        })
    })
}

fn status_mark(s: Option<StepStatus>) -> &'static str {
    match s {
        Some(StepStatus::Passed) => "ok",
        Some(StepStatus::Failed) => "FAILED",
        Some(StepStatus::Skipped) => "skipped",
        _ => "?",
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut t: String = flat.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn step_line(s: &StepTrace) -> String {
    let mut line = format!(
        "#{} {} {} {}ms",
        s.index + 1,
        status_mark(s.status),
        one_line(&s.label, 100),
        s.duration_ms
    );
    if let Some(e) = &s.error {
        line.push_str(&format!(" — {}", one_line(e, 160)));
    }
    line
}

/// Every text and label on a tree, for search.
fn tree_texts(e: &Element, out: &mut Vec<String>) {
    out.extend(e.text.iter().cloned());
    out.extend(e.label.iter().cloned());
    for c in &e.children {
        tree_texts(c, out);
    }
}

fn step_arg(args: &Value, trace: &TestTrace) -> Result<usize> {
    let n = args.get("step").and_then(Value::as_u64).ok_or_else(|| {
        VelocityError::Config("'step' (1-based, as in the summary) is required".into())
    })? as usize;
    if n == 0 || n > trace.steps.len() {
        return Err(VelocityError::Config(format!(
            "step {n} is out of range: the trace has steps 1..={}",
            trace.steps.len()
        )));
    }
    Ok(n - 1)
}

/// Run one `inspect_trace` view over the trace under `artifacts`.
pub fn inspect(artifacts: &Path, args: &Value) -> Result<Inspected> {
    let test = args.get("test").and_then(Value::as_str);
    let file = find_trace(artifacts, test)?;
    let (trace, trees) =
        load_trace(&file).map_err(|e| VelocityError::Config(format!("{}: {e}", file.display())))?;
    let view = args
        .get("view")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    let head = serde_json::json!({
        "test": trace.test,
        "status": trace.status,
        "device": trace.device,
        "attempt": trace.attempt,
        "trace_file": file,
    });
    let with = |mut v: Value| {
        if let (Value::Object(m), Value::Object(h)) = (&mut v, &head) {
            for (k, val) in h {
                m.insert(k.clone(), val.clone());
            }
        }
        v
    };
    match view {
        "summary" => Ok(Inspected {
            value: with(serde_json::json!({
                "steps": trace.steps.iter().map(step_line).collect::<Vec<_>>().join("\n"),
            })),
            png: None,
        }),
        "search" => {
            let q = args
                .get("query")
                .and_then(Value::as_str)
                .filter(|q| !q.trim().is_empty())
                .ok_or_else(|| VelocityError::Config("search needs a 'query'".into()))?
                .to_lowercase();
            let mut hits = Vec::new();
            for s in &trace.steps {
                let mut fields: Vec<(&str, String)> = vec![("label", s.label.clone())];
                if let Some(sel) = &s.selector {
                    fields.push(("selector", sel.to_string()));
                }
                if let Some(e) = &s.error {
                    fields.push(("error", e.clone()));
                }
                if let Some(m) = &s.matched {
                    fields.push(("matched", m.describe.clone()));
                }
                fields.extend(s.warnings.iter().map(|w| ("warning", w.clone())));
                let mut texts = Vec::new();
                for h in &s.trees {
                    if let Some(t) = trees.get(h) {
                        tree_texts(t, &mut texts);
                    }
                }
                fields.extend(texts.into_iter().map(|t| ("screen", t)));
                let found: Vec<String> = fields
                    .iter()
                    .filter(|(_, v)| v.to_lowercase().contains(&q))
                    .map(|(k, v)| format!("{k}: {}", one_line(v, 120)))
                    .collect();
                if !found.is_empty() {
                    let mut found = found;
                    found.dedup();
                    found.truncate(5);
                    hits.push(serde_json::json!({ "step": step_line(s), "matches": found }));
                }
            }
            Ok(Inspected {
                value: with(serde_json::json!({ "query": q, "count": hits.len(), "hits": hits })),
                png: None,
            })
        }
        "step_details" => {
            let i = step_arg(args, &trace)?;
            let s = &trace.steps[i];
            let mut v = serde_json::to_value(s).unwrap_or_default();
            if let Value::Object(m) = &mut v {
                m.remove("trees");
                m.insert("step".into(), (i + 1).into());
                if let Some(p) = tap_point(s) {
                    m.insert("tap_point".into(), serde_json::json!([p.0, p.1]));
                }
                if let Some((shot, which)) = screenshot_for(artifacts, &trace, i) {
                    m.insert(
                        "screenshot_file".into(),
                        shot.to_string_lossy().into_owned().into(),
                    );
                    m.insert("screenshot_is".into(), which.into());
                }
            }
            Ok(Inspected {
                value: with(v),
                png: None,
            })
        }
        "step_screenshot" => {
            let i = step_arg(args, &trace)?;
            let s = &trace.steps[i];
            let (shot, which) = screenshot_for(artifacts, &trace, i).ok_or_else(|| {
                VelocityError::Config(format!(
                    "no screenshot kept for step {}: screenshots are kept for failing tests \
                     (run_task keeps one per step for them); try step_details",
                    i + 1
                ))
            })?;
            let bytes = std::fs::read(&shot).map_err(|e| VelocityError::io(shot.display(), e))?;
            let root_width = s
                .trees
                .first()
                .and_then(|h| trees.get(h))
                .map(|t| t.bounds.width)
                .filter(|w| *w > 0);
            let point = tap_point(s);
            let png = match point {
                Some(p) => overlay(&bytes, p, root_width).unwrap_or(bytes),
                None => bytes,
            };
            Ok(Inspected {
                value: with(serde_json::json!({
                    "step": step_line(s),
                    "screenshot_file": shot,
                    "screenshot_is": which,
                    "tap_point": point.map(|p| vec![p.0, p.1]),
                    "marker": if point.is_some() { "red ring at the matched element's centre" } else { "none: the step matched no element" },
                })),
                png: Some(png),
            })
        }
        other => Err(VelocityError::Config(format!(
            "unknown view '{other}': use summary, search, step_details or step_screenshot"
        ))),
    }
}

/// The centre of the element the step acted on.
fn tap_point(s: &StepTrace) -> Option<(i32, i32)> {
    let b: &Rect = &s.matched.as_ref()?.bounds;
    (b.width > 0 && b.height > 0).then(|| (b.x + b.width / 2, b.y + b.height / 2))
}

/// The screen step `i` acted on: its own failure screenshot, else the
/// screen after the step before it, else the screen after it.
fn screenshot_for(
    artifacts: &Path,
    trace: &TestTrace,
    i: usize,
) -> Option<(PathBuf, &'static str)> {
    let s = &trace.steps[i];
    if let Some(p) = s.screenshot.as_ref().filter(|p| p.is_file()) {
        return Some((p.clone(), "the screen when the step failed"));
    }
    let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
    let mut dir = artifacts.join("steps").join(clean(&trace.test));
    if trace.attempt > 0 {
        dir =
            artifacts
                .join("steps")
                .join(format!("{}_retry{}", clean(&trace.test), trace.attempt));
    }
    let after = |idx: usize| -> Option<PathBuf> {
        let prefix = format!("step{idx:02}_");
        std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".png"))
            })
    };
    if i > 0 {
        if let Some(p) = after(i - 1) {
            return Some((p, "the screen before the step (after the previous one)"));
        }
    }
    after(i).map(|p| (p, "the screen after the step"))
}

/// Draw a red ring (and dot) at `point` (in the tree's coordinates; scaled
/// to the image when the tree's width is known, e.g. iOS points on a 3x
/// screenshot).
pub fn overlay(png_bytes: &[u8], point: (i32, i32), tree_width: Option<i32>) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().ok()?;
    // A header claiming a huge size would abort on allocation (audit
    // 2026-11 CORE-2).
    let (w, h) = (reader.info().width, reader.info().height);
    if u64::from(w) * u64::from(h) > velocity_core::visual::MAX_PNG_PIXELS {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let mut rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|ga| [ga[0], ga[0], ga[0], ga[1]])
            .collect(),
        _ => return None,
    };
    let (w, h) = (info.width as i64, info.height as i64);
    let scale = tree_width.map_or(1.0, |tw| w as f64 / tw as f64);
    let cx = (point.0 as f64 * scale) as i64;
    let cy = (point.1 as f64 * scale) as i64;
    let radius = (w.min(h) / 18).max(14);
    let thick = (radius / 5).max(3);
    for y in (cy - radius - thick).max(0)..(cy + radius + thick + 1).min(h) {
        for x in (cx - radius - thick).max(0)..(cx + radius + thick + 1).min(w) {
            let d2 = (x - cx).pow(2) + (y - cy).pow(2);
            let ring = d2 >= (radius - thick).pow(2) && d2 <= (radius + thick).pow(2);
            let dot = d2 <= (thick + 1).pow(2);
            if ring || dot {
                let o = ((y * w + x) * 4) as usize;
                rgba[o..o + 4].copy_from_slice(&[255, 32, 32, 255]);
            }
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, info.width, info.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;
    writer.finish().ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut wr = e.write_header().unwrap();
        wr.write_image_data(&vec![255u8; (w * h * 3) as usize])
            .unwrap();
        wr.finish().unwrap();
        out
    }

    fn pixel(png_bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
        let mut d = png::Decoder::new(std::io::Cursor::new(png_bytes));
        d.set_transformations(
            png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
        );
        let mut r = d.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        let o = ((y * info.width + x) * 4) as usize;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    }

    #[test]
    fn the_marker_is_drawn_at_the_scaled_tap_point() {
        let shot = png(300, 600);
        // A 100-point-wide tree on a 300-pixel screenshot: 3x.
        let out = overlay(&shot, (50, 100), Some(100)).unwrap();
        assert_eq!(
            pixel(&out, 150, 300),
            [255, 32, 32, 255],
            "dot at the centre"
        );
        assert_eq!(
            pixel(&out, 10, 10),
            [255, 255, 255, 255],
            "elsewhere untouched"
        );
        // Off-screen points don't panic.
        assert!(overlay(&shot, (5000, -40), None).is_some());
    }

    fn write_trace(dir: &Path) -> PathBuf {
        use velocity_common::{Selector, TestStatus};
        use velocity_core::trace::TracedElement;
        let tree = Element {
            element_type: "Frame".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 300,
                height: 600,
            },
            visible: true,
            children: vec![Element {
                text: Some("Welcome back".into()),
                element_type: "TextView".into(),
                visible: true,
                bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 300,
                    height: 40,
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let hash = velocity_core::trace::tree_hash(&tree);
        let step = |i: usize, label: &str, status, error: Option<&str>| StepTrace {
            index: i,
            label: label.into(),
            selector: Some(Selector::Id("pay".into())),
            source: None,
            status: Some(status),
            duration_ms: 120,
            settle_ms: 30,
            settle_reads: 2,
            matched: (status == StepStatus::Passed).then(|| TracedElement {
                describe: "Button \"Pay\"".into(),
                element_type: "Button".into(),
                id: "pay".into(),
                text: Some("Pay".into()),
                label: None,
                bounds: Rect {
                    x: 100,
                    y: 200,
                    width: 100,
                    height: 50,
                },
                score: None,
            }),
            nearest: vec![],
            heals: vec![],
            warnings: vec![],
            trees: vec![hash.clone()],
            error: error.map(String::from),
            screenshot: None,
            start_ms: 0,
        };
        let trace = TestTrace {
            version: velocity_core::trace::TRACE_VERSION,
            test: "checkout".into(),
            device: "emulator-5554".into(),
            platform: None,
            attempt: 0,
            status: TestStatus::Failed,
            evidence: None,
            steps: vec![
                step(0, "tap { id: \"pay\" }", StepStatus::Passed, None),
                step(
                    1,
                    "assertVisible { text: \"Done\" }",
                    StepStatus::Failed,
                    Some("Element not found: text=Done"),
                ),
            ],
        };
        let tdir = velocity_core::trace::trace_dir(dir, "checkout");
        std::fs::create_dir_all(tdir.join("trees")).unwrap();
        std::fs::write(tdir.join("steps.json"), serde_json::to_vec(&trace).unwrap()).unwrap();
        std::fs::write(
            tdir.join("trees").join(format!("{hash}.json")),
            serde_json::to_vec(&tree).unwrap(),
        )
        .unwrap();
        let shots = dir.join("steps").join("checkout");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("step00_tap.png"), png(300, 600)).unwrap();
        tdir
    }

    #[test]
    fn every_view_reads_the_trace() {
        let tmp = tempfile::tempdir().unwrap();
        write_trace(tmp.path());
        let v = |args: Value| inspect(tmp.path(), &args).unwrap_or_else(|e| panic!("{e}"));
        let summary = v(serde_json::json!({}));
        let lines = summary.value["steps"].as_str().unwrap();
        assert!(
            lines.starts_with("#1 ok tap { id: \"pay\" } 120ms"),
            "{lines}"
        );
        assert!(
            lines.contains("#2 FAILED") && lines.contains("Element not found"),
            "{lines}"
        );
        let found = v(serde_json::json!({ "view": "search", "query": "welcome" }));
        assert_eq!(found.value["count"], 2, "{}", found.value);
        let details = v(serde_json::json!({ "view": "step_details", "step": 1 }));
        assert_eq!(details.value["tap_point"], serde_json::json!([150, 225]));
        assert_eq!(details.value["matched"]["id"], "pay");
        assert!(details.value.get("trees").is_none());
        let shot =
            v(serde_json::json!({ "view": "step_screenshot", "step": 1, "test": "checkout" }));
        let png_bytes = shot.png.unwrap();
        assert_eq!(pixel(&png_bytes, 150, 225), [255, 32, 32, 255]);
        assert!(inspect(
            tmp.path(),
            &serde_json::json!({ "view": "step_details", "step": 9 })
        )
        .is_err());
        assert!(inspect(tmp.path(), &serde_json::json!({ "view": "nope" })).is_err());
        assert!(inspect(tmp.path(), &serde_json::json!({ "test": "other" })).is_err());
    }
}
