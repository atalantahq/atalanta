//! `trace.html`: a self-contained viewer for one test's trace.
//!
//! Written next to `steps.json` (`traces/<test>/trace.html`) and opened
//! with `atalanta trace view`. One file, inline CSS and JS, no network, so
//! it works from `file://` and as a CI artifact: a timeline of steps with
//! their timing, each step's screenshot with a marker where it acted, the
//! selector and the element it matched (or the nearest ones), heals,
//! warnings and errors, the video seeked to the step, and the log lines
//! around it, with a search box. In the spirit of Playwright's trace
//! viewer, much smaller.
//!
//! Safety: all data goes in one `<script type="application/json">` with
//! `<`, `>`, `&` and the JS line separators escaped, so nothing in it
//! (app text, log lines, selectors) can close the script or become markup;
//! the script builds the page with `textContent` only and accepts only
//! relative paths for media and links.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use atalanta_common::evidence::LogLine;
use atalanta_common::Element;
use serde_json::{json, Value};

use crate::trace::{TestTrace, TracedElement};

/// `s` safe inside a `<script>` element: no `</script>`, `<!--` or line
/// separators that end a JS string.
pub fn script_json(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| "null".into());
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Text for HTML element content and attribute values.
pub fn escape_html(s: &str) -> String {
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

/// `target` relative to the folder `base`, with `/` separators. Relative
/// inputs are taken from the current directory.
pub fn relative(target: &Path, base: &Path) -> String {
    let abs = |p: &Path| -> PathBuf {
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        // Lexical clean-up: `.` dropped, `..` pops.
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    };
    let (t, b) = (abs(target), abs(base));
    let tc: Vec<_> = t.components().collect();
    let bc: Vec<_> = b.components().collect();
    let common = tc.iter().zip(&bc).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..bc.len() {
        parts.push("..".into());
    }
    for c in &tc[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}

fn element_json(e: &TracedElement) -> Value {
    json!({
        "d": e.describe,
        "type": e.element_type,
        "id": e.id,
        "text": e.text,
        "label": e.label,
        "b": [e.bounds.x, e.bounds.y, e.bounds.width, e.bounds.height],
        "score": e.score,
    })
}

/// Actions whose marker is the matched element's centre.
fn acts_on_element(label: &str) -> bool {
    let verb = label.split([' ', '{', ':']).next().unwrap_or("");
    matches!(
        verb,
        "tap" | "doubleTap" | "longPress" | "inputText" | "clearText" | "tapOn" | "press"
    )
}

/// The data the viewer script reads.
pub fn data(
    trace: &TestTrace,
    trees: &HashMap<String, Element>,
    lines: &[LogLine],
    dir: &Path,
) -> Value {
    let viewport = trees
        .values()
        .map(|t| (t.bounds.width, t.bounds.height))
        .filter(|(w, h)| *w > 0 && *h > 0)
        .max_by_key(|(w, h)| i64::from(*w) * i64::from(*h));
    let steps: Vec<Value> = trace
        .steps
        .iter()
        .map(|s| {
            let point = s
                .matched
                .as_ref()
                .filter(|_| acts_on_element(&s.label))
                .map(|m| {
                    let c = m.bounds.center();
                    json!([c.0, c.1])
                });
            json!({
                "i": s.index,
                "label": s.label,
                "status": s.status.map(|st| serde_json::to_value(st).unwrap_or(Value::Null)),
                "start": s.start_ms,
                "dur": s.duration_ms,
                "settle": s.settle_ms,
                "reads": s.settle_reads,
                "selector": s.selector.as_ref().map(|sel| sel.to_string()),
                "src": s.source.as_ref().map(|p| if p.file.is_empty() { format!("line {}", p.line) } else { format!("{}:{}", p.file, p.line) }),
                "matched": s.matched.as_ref().map(element_json),
                "nearest": s.nearest.iter().map(element_json).collect::<Vec<_>>(),
                "heals": s.heals,
                "warnings": s.warnings,
                "error": s.error,
                "shot": s.screenshot.as_ref().map(|p| relative(p, dir)),
                "point": point,
            })
        })
        .collect();
    let ev = trace.evidence.clone().unwrap_or_default();
    let screencast = ev
        .screencast
        .as_ref()
        .and_then(|f| evidence_file(dir, f))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let mut files: Vec<Value> = Vec::new();
    if let Some(v) = &ev.video {
        files.push(json!({"title": "video", "href": v}));
    }
    for l in &ev.logs {
        files.push(json!({"title": l, "href": l}));
    }
    files.push(json!({"title": "steps.json", "href": format!("steps{}.json", suffix_of(trace))}));
    json!({
        "test": trace.test,
        "device": trace.device,
        "platform": trace.platform,
        "attempt": trace.attempt,
        "status": trace.status,
        "viewport": viewport.map(|(w, h)| json!([w, h])),
        "steps": steps,
        "video": ev.video.as_ref().map(|v| json!({"src": v, "offset": ev.video_offset_ms.unwrap_or(0)})),
        "screencast": screencast,
        "logs": lines.iter().map(|l| json!([l.t, l.level.to_string(), l.text])).collect::<Vec<_>>(),
        "notes": ev.notes,
        "files": files,
    })
}

/// The steps file's suffix for this attempt (`-retry1`); tags aren't in
/// the trace, so a copy's own name is found by the caller.
fn suffix_of(trace: &TestTrace) -> String {
    if trace.attempt > 0 {
        format!("-retry{}", trace.attempt)
    } else {
        String::new()
    }
}

/// The whole `trace.html` page for `trace`, whose files are in `dir`.
pub fn render(
    trace: &TestTrace,
    trees: &HashMap<String, Element>,
    lines: &[LogLine],
    dir: &Path,
) -> String {
    let data = data(trace, trees, lines, dir);
    let title = format!("Trace: {}", trace.test);
    fill(&PAGE_FULL, &escape_html(&title), &script_json(&data))
}

/// `page` with its one `{{TITLE}}` and one `{{DATA}}` filled in, each
/// once and in place: neither value is searched for placeholders.
fn fill(page: &str, title: &str, data: &str) -> String {
    let (head, rest) = page.split_once("{{TITLE}}").unwrap_or((page, ""));
    let (mid, tail) = rest.split_once("{{DATA}}").unwrap_or((rest, ""));
    let mut out = String::with_capacity(page.len() + title.len() + data.len());
    out.push_str(head);
    out.push_str(title);
    out.push_str(mid);
    out.push_str(data);
    out.push_str(tail);
    out
}

/// `dir/name` when `name` is a plain file name in the trace folder. A
/// trace's `steps.json` names its evidence files, and a downloaded artifact
/// is not to be trusted: `"screencast": "../../.docker/config.json"`
/// embedded any JSON file the user could read into the page they share
/// (audit 2026-11 TRACE-1).
pub fn evidence_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(_)), None) if !name.contains('\\') => {
            Some(dir.join(name))
        }
        _ => None,
    }
}

/// (Re)write the `trace*.html` for a `steps*.json` from its files: the
/// steps, their trees and the timed log lines. Returns the page's path.
pub fn write_for_steps(steps_file: &Path) -> std::io::Result<PathBuf> {
    let (trace, trees) = crate::trace::load_trace(steps_file)?;
    let dir = steps_file.parent().unwrap_or(Path::new("."));
    let lines: Vec<LogLine> = trace
        .evidence
        .as_ref()
        .and_then(|e| e.log_lines.as_ref())
        .and_then(|f| evidence_file(dir, f))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let stem = steps_file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffix = stem.strip_prefix("steps").unwrap_or("");
    let path = dir.join(format!("trace{suffix}.html"));
    atalanta_common::fsutil::write_atomic(&path, render(&trace, &trees, &lines, dir).as_bytes())?;
    Ok(path)
}

/// One test in an index page.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub test: String,
    pub status: String,
    /// The test's `trace.html`, relative to the index.
    pub href: String,
    pub error: Option<String>,
    pub attachments: Vec<(String, String)>,
}

/// A page listing traces (`atalanta trace view <artifacts dir>`).
pub fn render_index(title: &str, entries: &[IndexEntry]) -> String {
    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "test": e.test,
                "status": e.status,
                "href": e.href,
                "error": e.error,
                "files": e.attachments.iter().map(|(t, h)| json!({"title": t, "href": h})).collect::<Vec<_>>(),
            })
        })
        .collect();
    fill(
        INDEX,
        &escape_html(title),
        &script_json(&json!({"title": title, "rows": rows})),
    )
}

const STYLE: &str = r#"
:root{--bg:#fff;--fg:#1d1d1f;--mut:#6b6b76;--line:#e3e3e8;--panel:#f6f6f8;--sel:#e6f0ff;--acc:#2f6fed;--ok:#1a7f37;--bad:#cf222e;--skip:#8c8c96;--warn:#9a6700;--mark:#ff2d55}
@media (prefers-color-scheme:dark){:root{--bg:#141417;--fg:#e8e8ec;--mut:#9a9aa6;--line:#2c2c33;--panel:#1c1c21;--sel:#1f2e4d;--acc:#6f9dff;--ok:#3fb950;--bad:#f85149;--skip:#7d7d88;--warn:#d29922}}
*{box-sizing:border-box}html,body{margin:0;height:100%}
body{background:var(--bg);color:var(--fg);font:13px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif}
header{display:flex;gap:12px;align-items:center;flex-wrap:wrap;padding:10px 16px;border-bottom:1px solid var(--line)}
header h1{font-size:15px;margin:0;font-weight:600;overflow-wrap:anywhere}
.badge{padding:1px 8px;border-radius:10px;font-size:12px;font-weight:600;color:#fff;background:var(--skip)}
.badge.passed{background:var(--ok)}.badge.failed{background:var(--bad)}
.meta{color:var(--mut)}
header input{margin-left:auto;min-width:220px;padding:5px 9px;border:1px solid var(--line);border-radius:6px;background:var(--panel);color:var(--fg)}
.files a{margin-right:10px}
a{color:var(--acc)}
.notes{padding:6px 16px;background:color-mix(in srgb,var(--bad) 12%,var(--bg));border-bottom:1px solid var(--line)}
.notes div{color:var(--bad);font-family:ui-monospace,Menlo,monospace;font-size:12px;overflow-wrap:anywhere}
main{display:grid;grid-template-columns:minmax(240px,340px) 1fr;height:calc(100% - 52px)}
#steps{overflow:auto;border-right:1px solid var(--line)}
.step{display:grid;grid-template-columns:28px 10px 1fr auto;gap:6px;align-items:center;padding:4px 10px;cursor:pointer;border-bottom:1px solid var(--line)}
.step:hover{background:var(--panel)}.step.sel{background:var(--sel)}.step.play{box-shadow:inset 3px 0 var(--acc)}
.step .n{color:var(--mut);text-align:right;font-variant-numeric:tabular-nums}
.step .l{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-family:ui-monospace,Menlo,monospace;font-size:12px}
.step .d{color:var(--mut);font-size:11px;font-variant-numeric:tabular-nums}
.dot{width:8px;height:8px;border-radius:50%;background:var(--skip)}
.dot.passed{background:var(--ok)}.dot.failed{background:var(--bad)}
.step.hidden{display:none}
#right{display:grid;grid-template-rows:minmax(200px,55%) 1fr;min-width:0;min-height:0}
#media{display:flex;gap:12px;padding:12px;overflow:auto;background:var(--panel);border-bottom:1px solid var(--line);align-items:flex-start}
.frame{position:relative;display:inline-block;max-height:100%}
.frame img,.frame video{display:block;max-height:calc(55vh - 60px);max-width:100%;border:1px solid var(--line);border-radius:6px;background:#000}
.cap{font-size:11px;color:var(--mut);margin-bottom:4px}
.marker{position:absolute;width:22px;height:22px;margin:-11px 0 0 -11px;border:3px solid var(--mark);border-radius:50%;pointer-events:none;box-shadow:0 0 0 3px rgba(255,45,85,.3)}
.box{position:absolute;border:2px dashed var(--mark);pointer-events:none}
.empty{color:var(--mut);padding:20px}
#tabs{display:flex;gap:2px;border-bottom:1px solid var(--line);padding:0 12px}
#tabs button{border:0;background:none;color:var(--mut);padding:8px 10px;cursor:pointer;font:inherit;border-bottom:2px solid transparent}
#tabs button.on{color:var(--fg);border-bottom-color:var(--acc)}
#pane{overflow:auto;padding:10px 16px;min-height:0}
#bottom{display:grid;grid-template-rows:auto 1fr;min-height:0}
dl{display:grid;grid-template-columns:130px 1fr;gap:4px 12px;margin:0}
dt{color:var(--mut)}dd{margin:0;overflow-wrap:anywhere}
pre,code,.mono{font-family:ui-monospace,Menlo,monospace;font-size:12px}
pre{white-space:pre-wrap;margin:0;overflow-wrap:anywhere}
.err{color:var(--bad)}.warn{color:var(--warn)}
table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:3px 6px;border-bottom:1px solid var(--line);vertical-align:top}
.log{display:grid;grid-template-columns:70px 14px 1fr;gap:6px;font-family:ui-monospace,Menlo,monospace;font-size:12px;padding:1px 0}
.log .t{color:var(--mut);text-align:right}.log.E .x,.log.F .x{color:var(--bad)}.log.W .x{color:var(--warn)}.log.D .x,.log.V .x{color:var(--mut)}
.log .x{white-space:pre-wrap;overflow-wrap:anywhere}
.log.cur{background:var(--sel)}
@media (max-width:760px){main{grid-template-columns:1fr;height:auto}#steps{max-height:40vh;border-right:0;border-bottom:1px solid var(--line)}#right{grid-template-rows:auto auto}}
"#;

const PAGE: &str = concat!(
    r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{TITLE}}</title><style>"#,
    "{{STYLE}}",
    r#"</style></head><body>
<header><h1 id="title"></h1><span id="badge" class="badge"></span><span id="meta" class="meta"></span><span id="files" class="files"></span><input id="q" type="search" placeholder="Search steps and logs (/)" autocomplete="off"></header>
<div id="notes" class="notes" hidden></div>
<main><div id="steps" role="listbox" aria-label="Steps"></div>
<div id="right"><div id="media"></div><div id="bottom"><div id="tabs"><button data-t="details" class="on">Step</button><button data-t="logs">Logs</button></div><div id="pane"></div></div></div></main>
<script type="application/json" id="data">{{DATA}}</script>
<script>"#,
    "{{SCRIPT}}",
    r#"</script></body></html>
"#
);

const SCRIPT: &str = r#"
"use strict";
const D = JSON.parse(document.getElementById('data').textContent);
const $ = (id) => document.getElementById(id);
function el(tag, cls, text) { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = String(text); return e; }
// Only relative paths: no scheme (javascript:, data:, http:) and not rooted.
function safe(p) { if (typeof p !== 'string' || !p) return null; if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(p) || p.startsWith('/') || p.startsWith('\\')) return null; return p.split('/').map(encodeURIComponent).join('/'); }
function ms(n) { if (n == null) return ''; return n < 1000 ? n + ' ms' : (n / 1000).toFixed(2) + ' s'; }
const steps = D.steps || [];
const logs = D.logs || [];
let sel = 0, tab = 'details', query = '';
$('title').textContent = D.test;
const badge = $('badge'); badge.textContent = D.status || ''; badge.classList.add(String(D.status || ''));
$('meta').textContent = [D.device, D.platform, D.attempt ? 'retry ' + D.attempt : null, steps.length + ' steps'].filter(Boolean).join(' · ');
for (const f of D.files || []) { const h = safe(f.href); if (!h) continue; const a = el('a', null, f.title); a.href = h; $('files').append(a); }
if ((D.notes || []).length) { const n = $('notes'); n.hidden = false; for (const t of D.notes) n.append(el('div', null, t)); }
document.title = 'Trace: ' + D.test;

// Step list, built once.
const rows = steps.map((s, i) => {
  const r = el('div', 'step'); r.setAttribute('role', 'option');
  r.append(el('span', 'n', s.i + 1), el('span', 'dot ' + (s.status || '')), el('span', 'l', s.label), el('span', 'd', ms(s.dur)));
  r.title = s.label; r.addEventListener('click', () => select(i, true));
  return r;
});
$('steps').append(...rows);
const firstFail = steps.findIndex(s => s.status === 'failed');
const starts = steps.map(s => s.start || 0);

// Media: video (seeked per step) or screencast frames, plus the step's screenshot.
const media = $('media');
let video = null, videoMarker = null, cast = null, castImg = null, castMarker = null, shotBox = null;
function frame(caption) { const w = el('div'); w.append(el('div', 'cap', caption)); const f = el('div', 'frame'); w.append(f); media.append(w); return f; }
if (D.video && safe(D.video.src)) {
  const f = frame('Video');
  video = el('video'); video.src = safe(D.video.src); video.controls = true; video.preload = 'metadata'; video.muted = true;
  videoMarker = el('div', 'marker'); videoMarker.hidden = true; f.append(video, videoMarker);
  video.addEventListener('timeupdate', () => { const t = video.currentTime * 1000 + (D.video.offset || 0); playing(t); });
} else if (D.screencast && (D.screencast.frames || []).length) {
  cast = D.screencast.frames.filter(fr => safe(fr.file));
  const f = frame('Screencast');
  castImg = el('img'); castImg.alt = 'screencast frame'; castMarker = el('div', 'marker'); castMarker.hidden = true; f.append(castImg, castMarker);
  const bar = el('div', 'cap'); const play = el('button', null, 'Play'); bar.append(play); f.parentNode.append(bar);
  let timer = null;
  play.addEventListener('click', () => {
    if (timer) { clearTimeout(timer); timer = null; play.textContent = 'Play'; return; }
    let k = Math.max(0, castIndex(starts[sel] || 0)); play.textContent = 'Pause';
    const next = () => { if (k >= cast.length) { timer = null; play.textContent = 'Play'; return; } showCast(k); playing(cast[k].t); const d = k + 1 < cast.length ? Math.min(1000, cast[k + 1].t - cast[k].t) : 0; k++; timer = setTimeout(next, Math.max(16, d)); };
    next();
  });
}
const shotFrame = frame('Screenshot');
const shotImg = el('img'); shotImg.alt = 'step screenshot'; const shotMarker = el('div', 'marker'); shotMarker.hidden = true;
shotBox = el('div', 'box'); shotBox.hidden = true;
const noShot = el('div', 'empty', 'No screenshot for this step');
shotFrame.append(shotImg, shotMarker, shotBox, noShot);
function castIndex(t) { let lo = 0, hi = cast.length - 1, best = 0; while (lo <= hi) { const m = (lo + hi) >> 1; if (cast[m].t <= t) { best = m; lo = m + 1; } else hi = m - 1; } return best; }
function showCast(k) { if (cast[k]) castImg.src = safe(cast[k].file); }
function placeMarker(m, s) {
  const vp = D.viewport; if (!m) return;
  if (!vp || !s || !s.point) { m.hidden = true; return; }
  m.hidden = false; m.style.left = (s.point[0] / vp[0] * 100) + '%'; m.style.top = (s.point[1] / vp[1] * 100) + '%';
}
function placeBox(s) {
  const vp = D.viewport, b = s && s.matched && s.matched.b;
  if (!vp || !b) { shotBox.hidden = true; return; }
  shotBox.hidden = false; shotBox.style.left = (b[0] / vp[0] * 100) + '%'; shotBox.style.top = (b[1] / vp[1] * 100) + '%';
  shotBox.style.width = (b[2] / vp[0] * 100) + '%'; shotBox.style.height = (b[3] / vp[1] * 100) + '%';
}
let playIdx = -1;
function playing(t) {
  let lo = 0, hi = starts.length - 1, k = -1;
  while (lo <= hi) { const m = (lo + hi) >> 1; if (starts[m] <= t) { k = m; lo = m + 1; } else hi = m - 1; }
  if (k === playIdx) return; if (playIdx >= 0 && rows[playIdx]) rows[playIdx].classList.remove('play');
  playIdx = k; if (k >= 0 && rows[k]) rows[k].classList.add('play');
}

function select(i, fromClick) {
  if (!steps.length) return;
  i = Math.max(0, Math.min(steps.length - 1, i));
  rows[sel] && rows[sel].classList.remove('sel'); sel = i; rows[i].classList.add('sel');
  if (!fromClick) rows[i].scrollIntoView({ block: 'nearest' });
  const s = steps[i];
  if (video) { const t = Math.max(0, ((s.start || 0) - (D.video.offset || 0)) / 1000); try { video.pause(); video.currentTime = t; } catch (e) {} placeMarker(videoMarker, s); }
  if (cast) { showCast(castIndex((s.start || 0) + (s.dur || 0))); placeMarker(castMarker, s); }
  // The step's own screenshot, else the nearest earlier one.
  let shot = null; for (let k = i; k >= 0 && !shot; k--) shot = safe(steps[k].shot);
  if (shot) { shotImg.hidden = false; noShot.hidden = true; shotImg.src = shot; placeMarker(shotMarker, s); placeBox(s); }
  else { shotImg.hidden = true; noShot.hidden = false; shotMarker.hidden = true; shotBox.hidden = true; }
  render();
}

function kv(dl, k, v, cls) { if (v == null || v === '' || (Array.isArray(v) && !v.length)) return; dl.append(el('dt', null, k)); const d = el('dd', cls); if (v instanceof Node) d.append(v); else d.textContent = String(v); dl.append(d); }
function elem(e) { const parts = [e.d]; if (e.type) parts.push(e.type); if (e.b) parts.push('@ ' + e.b.join(',')); return parts.join('  '); }
function details() {
  const s = steps[sel], box = el('div'); if (!s) return box;
  const dl = el('dl');
  kv(dl, 'Step', (s.i + 1) + '. ' + s.label, 'mono');
  kv(dl, 'Status', s.status || 'not run', s.status === 'failed' ? 'err' : null);
  kv(dl, 'Started', '+' + ms(s.start));
  kv(dl, 'Duration', ms(s.dur) + (s.settle ? '  (settle ' + ms(s.settle) + ', ' + s.reads + ' reads)' : ''));
  kv(dl, 'Source', s.src, 'mono');
  kv(dl, 'Selector', s.selector, 'mono');
  kv(dl, 'Resolved to', s.matched ? elem(s.matched) : null, 'mono');
  if (s.point) kv(dl, 'Acted at', s.point.join(', '), 'mono');
  if (s.error) { const p = el('pre', 'err', s.error); kv(dl, 'Error', p); }
  for (const h of s.heals || []) kv(dl, 'Healed', h, 'warn');
  for (const w of s.warnings || []) kv(dl, 'Warning', w, 'warn');
  box.append(dl);
  if ((s.nearest || []).length) {
    box.append(el('h4', null, 'Closest elements on screen'));
    const t = el('table'); const h = el('tr'); h.append(el('th', null, 'score'), el('th', null, 'element'), el('th', null, 'bounds')); t.append(h);
    for (const n of s.nearest) { const r = el('tr'); r.append(el('td', null, n.score != null ? n.score.toFixed(2) : ''), el('td', 'mono', n.d + (n.type ? '  ' + n.type : '')), el('td', 'mono', (n.b || []).join(','))); t.append(r); }
    box.append(t);
  }
  return box;
}
function logRow(l, cur) { const r = el('div', 'log ' + (l[1] || '') + (cur ? ' cur' : '')); r.append(el('span', 't', '+' + (l[0] / 1000).toFixed(3)), el('span', null, l[1] || ''), el('span', 'x', l[2])); return r; }
function logView() {
  const box = el('div');
  if (!logs.length) { box.append(el('div', 'empty', 'No log lines were captured for this test.')); return box; }
  const q = query.toLowerCase();
  if (q) {
    let n = 0; for (const l of logs) { if (String(l[2]).toLowerCase().includes(q)) { box.append(logRow(l)); if (++n >= 500) { box.append(el('div', 'empty', 'First 500 matches shown')); break; } } }
    if (!n) box.append(el('div', 'empty', 'No log line matches'));
    return box;
  }
  const s = steps[sel], a = (s.start || 0) - 1000, b = (s.start || 0) + (s.dur || 0) + 1000;
  box.append(el('div', 'cap', 'Log lines from 1 s before to 1 s after step ' + (s.i + 1) + ' (search to see all)'));
  let n = 0;
  for (const l of logs) { if (l[0] < a) continue; if (l[0] > b) break; box.append(logRow(l, l[0] >= (s.start || 0) && l[0] <= (s.start || 0) + (s.dur || 0))); if (++n >= 2000) break; }
  if (!n) box.append(el('div', 'empty', 'No log lines around this step'));
  return box;
}
function render() { const p = $('pane'); p.replaceChildren(tab === 'logs' ? logView() : details()); }
for (const b of document.querySelectorAll('#tabs button')) b.addEventListener('click', () => { tab = b.dataset.t; for (const o of document.querySelectorAll('#tabs button')) o.classList.toggle('on', o === b); render(); });
$('q').addEventListener('input', (e) => {
  query = e.target.value.trim(); const q = query.toLowerCase();
  steps.forEach((s, i) => { const hay = (s.label + ' ' + (s.selector || '') + ' ' + (s.error || '') + ' ' + (s.matched ? s.matched.d : '')).toLowerCase(); rows[i].classList.toggle('hidden', !!q && !hay.includes(q)); });
  if (q && logs.length) { tab = 'logs'; for (const o of document.querySelectorAll('#tabs button')) o.classList.toggle('on', o.dataset.t === 'logs'); }
  render();
});
document.addEventListener('keydown', (e) => {
  if (e.target === $('q')) { if (e.key === 'Escape') e.target.blur(); return; }
  if (e.key === '/') { e.preventDefault(); $('q').focus(); }
  else if (e.key === 'ArrowDown' || e.key === 'j') { e.preventDefault(); select(sel + 1); }
  else if (e.key === 'ArrowUp' || e.key === 'k') { e.preventDefault(); select(sel - 1); }
});
if (steps.length) select(firstFail >= 0 ? firstFail : 0); else $('pane').append(el('div', 'empty', 'This trace has no steps.'));
"#;

const INDEX: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{TITLE}}</title><style>
:root{--bg:#fff;--fg:#1d1d1f;--mut:#6b6b76;--line:#e3e3e8;--acc:#2f6fed;--ok:#1a7f37;--bad:#cf222e}
@media (prefers-color-scheme:dark){:root{--bg:#141417;--fg:#e8e8ec;--mut:#9a9aa6;--line:#2c2c33;--acc:#6f9dff;--ok:#3fb950;--bad:#f85149}}
body{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif}
h1{font-size:18px}a{color:var(--acc)}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line);vertical-align:top}
.failed{color:var(--bad);font-weight:600}.passed{color:var(--ok);font-weight:600}.err{color:var(--mut);font-family:ui-monospace,Menlo,monospace;font-size:12px;overflow-wrap:anywhere}
.files a{margin-right:10px}
</style></head><body><h1 id="h"></h1><table id="t"><tr><th>Test</th><th>Status</th><th>Evidence</th></tr></table>
<script type="application/json" id="data">{{DATA}}</script>
<script>
"use strict";
const D = JSON.parse(document.getElementById('data').textContent);
function el(tag, cls, text) { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = String(text); return e; }
function safe(p) { if (typeof p !== 'string' || !p) return null; if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(p) || p.startsWith('/') || p.startsWith('\\')) return null; return p.split('/').map(encodeURIComponent).join('/'); }
document.getElementById('h').textContent = D.title;
const t = document.getElementById('t');
for (const r of D.rows) {
  const tr = el('tr'); const a = el('a', null, r.test); const h = safe(r.href); if (h) a.href = h;
  const name = el('td'); name.append(a); if (r.error) name.append(el('div', 'err', r.error));
  const files = el('td', 'files'); for (const f of r.files || []) { const fh = safe(f.href); if (!fh) continue; const fa = el('a', null, f.title); fa.href = fh; files.append(fa); }
  tr.append(name, el('td', r.status, r.status), files); t.append(tr);
}
</script></body></html>
"#;

/// The page with its style and script in place (built once).
static PAGE_FULL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    PAGE.replace("{{STYLE}}", STYLE)
        .replace("{{SCRIPT}}", SCRIPT)
});

#[cfg(test)]
mod tests {

    #[test]
    fn an_evidence_file_outside_the_trace_folder_is_not_read() {
        let d = Path::new("/r/traces/t");
        assert_eq!(
            evidence_file(d, "screencast.json"),
            Some(d.join("screencast.json"))
        );
        for bad in [
            "../../.docker/config.json",
            "/etc/passwd",
            "a/b.json",
            "..",
            "",
            "a\\b",
        ] {
            assert_eq!(evidence_file(d, bad), None, "{bad}");
        }
    }
    use super::*;
    use crate::trace::{StepTrace, TraceEvidence, TRACE_VERSION};
    use atalanta_common::{Rect, StepStatus, TestStatus};

    fn trace_with(label: &str, error: &str) -> TestTrace {
        TestTrace {
            version: TRACE_VERSION,
            test: "pay </script><script>alert(1)</script>".into(),
            device: "emulator-5554".into(),
            platform: None,
            attempt: 0,
            status: TestStatus::Failed,
            steps: vec![
                StepTrace {
                    index: 0,
                    label: "launchApp".into(),
                    status: Some(StepStatus::Passed),
                    start_ms: 0,
                    duration_ms: 900,
                    ..Default::default()
                },
                StepTrace {
                    index: 1,
                    label: label.into(),
                    status: Some(StepStatus::Failed),
                    start_ms: 950,
                    duration_ms: 10_000,
                    error: Some(error.into()),
                    matched: Some(TracedElement {
                        describe: "text=\"Pay\"".into(),
                        element_type: "Button".into(),
                        id: String::new(),
                        text: Some("Pay".into()),
                        label: None,
                        bounds: Rect {
                            x: 100,
                            y: 200,
                            width: 50,
                            height: 20,
                        },
                        score: None,
                    }),
                    screenshot: Some(PathBuf::from("/tmp/vel-results/failure.png")),
                    ..Default::default()
                },
            ],
            evidence: Some(TraceEvidence {
                video: Some("video.mp4".into()),
                video_offset_ms: Some(-120),
                logs: vec!["app.log".into()],
                notes: vec!["crash (pid 1): boom".into()],
                ..Default::default()
            }),
        }
    }

    fn embedded(html: &str) -> Value {
        let start = html
            .find(r#"<script type="application/json" id="data">"#)
            .unwrap()
            + r#"<script type="application/json" id="data">"#.len();
        let end = start + html[start..].find("</script>").unwrap();
        serde_json::from_str(&html[start..end]).unwrap()
    }

    #[test]
    fn untrusted_text_cannot_leave_the_data_script() {
        let evil = "</script><img src=x onerror=alert(1)><!-- \u{2028}";
        let t = trace_with(&format!("tap {{ text: \"{evil}\" }}"), evil);
        let lines = vec![LogLine {
            t: 1000,
            level: 'E',
            text: evil.into(),
        }];
        let html = render(
            &t,
            &HashMap::new(),
            &lines,
            Path::new("/tmp/vel-results/traces/pay"),
        );
        // Exactly the two scripts of the page (data and code): nothing the
        // data holds closed one or opened another.
        assert_eq!(html.matches("</script>").count(), 2, "{html}");
        assert_eq!(html.matches("<script").count(), 2);
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains('\u{2028}'));
        // The title is escaped too.
        assert!(html.contains("<title>Trace: pay &lt;/script&gt;"));
        // And the data still reads back exactly.
        let d = embedded(&html);
        assert_eq!(d["steps"][1]["error"], evil);
        assert_eq!(d["logs"][0][2], evil);
        assert_eq!(d["test"], t.test);
        // The script never uses innerHTML on data.
        assert!(!SCRIPT.contains("innerHTML"));
        assert!(!SCRIPT.contains("insertAdjacentHTML"));
        assert!(!SCRIPT.contains("document.write"));
    }

    #[test]
    fn steps_carry_times_markers_and_relative_media() {
        let t = trace_with("tap { text: \"Pay\" }", "not found");
        let mut trees = HashMap::new();
        let root = Element {
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            },
            ..Default::default()
        };
        trees.insert("h".to_string(), root);
        let d = data(&t, &trees, &[], Path::new("/tmp/vel-results/traces/pay"));
        assert_eq!(d["viewport"], json!([1080, 2400]));
        assert_eq!(d["steps"][1]["start"], 950);
        assert_eq!(d["steps"][1]["point"], json!([125, 210]));
        assert_eq!(
            d["steps"][0]["point"],
            Value::Null,
            "launchApp has no tap point"
        );
        assert_eq!(d["steps"][1]["shot"], "../../failure.png");
        assert_eq!(d["video"], json!({"src": "video.mp4", "offset": -120}));
        assert_eq!(d["notes"][0], "crash (pid 1): boom");
        assert_eq!(d["files"][0]["href"], "video.mp4");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(Path::new("/a/b/c.png"), Path::new("/a/b")),
            "c.png"
        );
        assert_eq!(
            relative(Path::new("/a/x/c.png"), Path::new("/a/b/d")),
            "../../x/c.png"
        );
        assert_eq!(
            relative(Path::new("/a/./b/../c.png"), Path::new("/a")),
            "c.png"
        );
    }

    #[test]
    fn index_escapes_its_rows() {
        let html = render_index(
            "Traces",
            &[IndexEntry {
                test: "</script><b>x</b>".into(),
                status: "failed".into(),
                href: "traces/x/trace.html".into(),
                error: Some("<i>boom</i>".into()),
                attachments: vec![("video".into(), "traces/x/video.mp4".into())],
            }],
        );
        assert_eq!(html.matches("</script>").count(), 2);
        assert!(!html.contains("<b>x</b>") && !html.contains("<i>boom</i>"));
    }

    #[test]
    fn the_page_is_self_contained() {
        let mut t = trace_with("tap", "e");
        t.test = "{{DATA}} {{TITLE}}".into();
        let html = render(&t, &HashMap::new(), &[], Path::new("."));
        assert_eq!(embedded(&html)["test"], "{{DATA}} {{TITLE}}");
        assert!(html.contains("const D = JSON.parse"), "script inlined");
        assert!(html.contains(".marker{"), "style inlined");
        assert!(!html.contains("{{STYLE}}") && !html.contains("{{SCRIPT}}"));
        for remote in ["http://", "https://", "//cdn", "@import"] {
            assert!(!html.contains(remote), "{remote}");
        }
    }
}
