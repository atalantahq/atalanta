//! Screenshot review (U1): the failed `assertScreenshot` comparisons in the
//! results dir, each with its expected, actual and diff image, and Accept /
//! Reject buttons. Rendered here as plain HTML (no frontend build), so it
//! works in any build, including one without the React UI.
//!
//! `GET /review` is the page; `GET /api/baselines` the same list as JSON;
//! `GET /api/baselines/image?id=…&kind=expected|actual|diff` an image;
//! `POST /api/baselines/accept|reject` (form or JSON `id`) acts on one.
//! All of them need the session token (see `security`). Ids are looked up
//! in a fresh scan of the results dir, never used as paths.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use velocity_core::baselines::{self, PendingBaseline};

use crate::state::AppState;

fn scan(state: &AppState) -> Vec<PendingBaseline> {
    baselines::pending(&state.results_dir)
}

fn find(state: &AppState, id: &str) -> Result<PendingBaseline, (StatusCode, String)> {
    scan(state).into_iter().find(|p| p.id == id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("no failed screenshot comparison {id:?} to review"),
        )
    })
}

/// `GET /api/baselines`
pub async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<PendingBaseline>> {
    Json(scan(&state))
}

#[derive(Deserialize)]
pub struct ImageQuery {
    pub id: String,
    pub kind: String,
}

/// `GET /api/baselines/image`
pub async fn image(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ImageQuery>,
) -> Result<Response, (StatusCode, String)> {
    let p = find(&state, &q.id)?;
    let path = match q.kind.as_str() {
        "expected" => p.expected,
        "actual" => p.actual,
        "diff" => p
            .diff
            .ok_or((StatusCode::NOT_FOUND, "no diff image".to_string()))?,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("kind must be expected, actual or diff, not {other:?}"),
            ))
        }
    };
    let bytes = std::fs::read(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("{}: {e}", path.display())))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], bytes).into_response())
}

#[derive(Deserialize)]
pub struct Target {
    pub id: String,
}

/// `POST /api/baselines/accept`: the actual frame becomes the baseline.
pub async fn accept(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let (id, json) = target(&headers, &body)?;
    let p = find(&state, &id)?;
    let written = baselines::accept_into(&p, state.baselines_dir.as_deref()).map_err(|e| {
        let status = match e {
            // A later run changed the baseline: keep it (and the record).
            baselines::AcceptError::Stale(_) => StatusCode::CONFLICT,
            baselines::AcceptError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, e.to_string())
    })?;
    tracing::info!(name = %p.name, baseline = %written.display(), "baseline accepted");
    Ok(if json {
        Json(serde_json::json!({ "accepted": p.name, "baseline": written })).into_response()
    } else {
        Redirect::to(&format!("/review?accepted={}", encode(&p.name))).into_response()
    })
}

/// `POST /api/baselines/reject`: keep the baseline, drop it from the list.
pub async fn reject(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let (id, json) = target(&headers, &body)?;
    let p = find(&state, &id)?;
    baselines::reject(&p).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(if json {
        Json(serde_json::json!({ "rejected": p.name })).into_response()
    } else {
        Redirect::to(&format!("/review?rejected={}", encode(&p.name))).into_response()
    })
}

/// The `id` a POST names, from a JSON body or the page's form; and whether
/// it was JSON (answered with JSON; a form gets a redirect to the page).
fn target(headers: &HeaderMap, body: &[u8]) -> Result<(String, bool), (StatusCode, String)> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let bad = |e: String| (StatusCode::BAD_REQUEST, format!("expected an id: {e}"));
    let id = if json {
        serde_json::from_slice::<Target>(body)
            .map_err(|e| bad(e.to_string()))?
            .id
    } else {
        String::from_utf8_lossy(body)
            .split('&')
            .find_map(|kv| kv.strip_prefix("id="))
            .map(decode)
            .ok_or_else(|| bad("no id field".into()))?
    };
    Ok((id, json))
}

/// Decode an `application/x-www-form-urlencoded` value.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    pub accepted: Option<String>,
    pub rejected: Option<String>,
}

/// `GET /review`
pub async fn page(State(state): State<Arc<AppState>>, Query(q): Query<PageQuery>) -> Response {
    let html = render(&state.results_dir, &scan(&state), &q);
    let mut r = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    // Nothing but this page's own images, styles and forms.
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; \
             form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn render(results_dir: &Path, list: &[PendingBaseline], q: &PageQuery) -> String {
    let mut body = String::new();
    if let Some(n) = &q.accepted {
        body.push_str(&format!(
            "<p class=note>Accepted <b>{}</b>: its baseline is now the actual screen. \
             Commit the baseline file.</p>",
            esc(n)
        ));
    }
    if let Some(n) = &q.rejected {
        body.push_str(&format!(
            "<p class=note>Rejected <b>{}</b>: the baseline is unchanged.</p>",
            esc(n)
        ));
    }
    if list.is_empty() {
        body.push_str(&format!(
            "<p>No failed screenshot comparisons to review in <code>{}</code>.</p>",
            esc(&results_dir.display().to_string())
        ));
    }
    for p in list {
        let img = |kind: &str| {
            format!(
                "<figure><figcaption>{kind}</figcaption>\
                 <img alt=\"{kind}\" src=\"/api/baselines/image?id={}&amp;kind={kind}\"></figure>",
                encode(&p.id)
            )
        };
        let diff = if p.diff.is_some() {
            img("diff")
        } else {
            String::new()
        };
        let form = |action: &str, label: &str| {
            format!(
                "<form method=post action=\"/api/baselines/{action}\">\
                 <input type=hidden name=id value=\"{}\">\
                 <button class={action}>{label}</button></form>",
                esc(&p.id)
            )
        };
        body.push_str(&format!(
            "<section><h2>{name}</h2><p class=meta>{detail}<br>baseline: <code>{base}</code></p>\
             <div class=imgs>{e}{a}{diff}</div><div class=actions>{acc}{rej}</div></section>",
            name = esc(&p.name),
            detail = esc(&p.detail),
            base = esc(&p.baseline.display().to_string()),
            e = img("expected"),
            a = img("actual"),
            acc = form("accept", "Accept actual as baseline"),
            rej = form("reject", "Reject (keep baseline)"),
        ));
    }
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width, initial-scale=1\">\
         <title>Screenshot review</title><style>{CSS}</style></head><body>\
         <h1>Screenshot review <small>{n} pending</small></h1>{body}</body></html>",
        n = list.len()
    )
}

const CSS: &str = "body{font:14px/1.4 system-ui,sans-serif;margin:16px;color:#111;background:#fff}\
h1 small{font-weight:normal;color:#666;font-size:14px}\
section{border:1px solid #ddd;border-radius:6px;padding:12px;margin:12px 0}\
h2{margin:0 0 4px;font-size:16px}.meta{color:#555;margin:0 0 8px;word-break:break-all}\
.imgs{display:flex;gap:8px;flex-wrap:wrap}figure{margin:0;flex:1 1 200px;max-width:360px}\
figcaption{color:#666;font-size:12px}img{width:100%;border:1px solid #ccc}\
.actions{display:flex;gap:8px;margin-top:8px}button{padding:6px 12px;cursor:pointer}\
button.accept{background:#1a7f37;color:#fff;border:0;border-radius:4px}\
.note{background:#eef6ee;padding:8px;border-radius:4px}\
@media (prefers-color-scheme:dark){body{background:#111;color:#eee}section{border-color:#333}\
.meta,figcaption,h1 small{color:#aaa}.note{background:#1d2b1d}}";

/// HTML-escape text and attribute values.
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

/// Percent-encode a query parameter value.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_escaped_in_the_page() {
        assert_eq!(esc("<a href=\"x\">'"), "&lt;a href=&quot;x&quot;&gt;&#39;");
        assert_eq!(encode("diffs/a b&c"), "diffs%2Fa%20b%26c");
        assert_eq!(decode("diffs%2Fa+b%26c%"), "diffs/a b&c%");
        assert_eq!(decode(&encode("x/ü é")), "x/ü é");
        let html = render(
            Path::new("r"),
            &[],
            &PageQuery {
                accepted: Some("<script>".into()),
                rejected: None,
            },
        );
        assert!(!html.contains("<script>") && html.contains("&lt;script&gt;"));
    }
}
