use std::borrow::Cow;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

/// The web UI, built by CI and the release job (`npm ci && npm run build`
/// in `frontend/`), never by `build.rs` (D-42). A checkout without
/// `frontend/dist` still compiles: the API works and the page says how to
/// get the UI ([`MISSING_UI`]).
#[derive(Embed)]
#[folder = "frontend/dist"]
#[allow_missing = true]
struct FrontendAssets;

/// What the inspector serves instead of its page when the UI was not built.
pub const MISSING_UI: &str = "The Velocity inspector's web UI is not in this build: \
crates/velocity-inspector/frontend/dist was missing when it was compiled. Release binaries \
include it. From a source checkout, run `npm ci && npm run build` in \
crates/velocity-inspector/frontend, then rebuild velocity. The inspector API (/api/...) works \
without it.";

/// The Content-Security-Policy on the console UI (security I-3): scripts,
/// styles and connections (fetch, the mirror and task WebSockets) from this
/// server only; images may also be `data:`/`blob:` (screenshots). React
/// escapes the device-controlled strings it renders; this is the second
/// line, so an injected string still could not load or run a script.
pub const UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; object-src 'none'; \
base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The web UI is in this build.
pub fn ui_available() -> bool {
    FrontendAssets::get("index.html").is_some()
}

pub async fn static_handler(uri: Uri) -> impl IntoResponse {
    serve(uri.path(), |p| FrontendAssets::get(p).map(|f| f.data))
}

/// The asset at `path`, else `index.html` (SPA routing), else
/// [`MISSING_UI`] as a 503.
fn serve(path: &str, get: impl Fn(&str) -> Option<Cow<'static, [u8]>>) -> Response {
    let path = path.trim_start_matches('/');
    // Try the exact path first, then fall back to index.html for SPA routing
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(data) = get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.as_ref().to_string()),
                (header::CONTENT_SECURITY_POLICY, UI_CSP.to_string()),
            ],
            data.into_owned(),
        )
            .into_response();
    }
    // SPA fallback: serve index.html for all non-file routes
    match get("index.html") {
        Some(data) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html".to_string()),
                (header::CONTENT_SECURITY_POLICY, UI_CSP.to_string()),
            ],
            data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            )],
            MISSING_UI,
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(r: Response) -> String {
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// D-42: without a built `dist/`, the page explains how to get the UI.
    #[tokio::test]
    async fn a_missing_ui_says_how_to_build_it() {
        let r = serve("/", |_| None);
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let text = body(r).await;
        assert!(text.contains("npm ci && npm run build"), "{text}");
        assert!(text.contains("frontend/dist"), "{text}");
    }

    /// Security I-3: the console page and its assets carry a CSP that
    /// allows scripts from this server only.
    #[tokio::test]
    async fn the_ui_is_served_with_a_content_security_policy() {
        for path in ["/", "/assets/index.js", "/some/spa/route"] {
            let r = serve(path, |p| {
                (p == "index.html" || p == "assets/index.js").then(|| Cow::Borrowed(&b"x"[..]))
            });
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            let csp = r
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(csp.contains("script-src 'self'"), "{path}: {csp}");
            assert!(!csp.contains("unsafe"), "{path}: {csp}");
        }
    }

    /// review-d7 m1: a release build without `dist/` tracked no file
    /// under it, so building the UI and rebuilding left the binary without
    /// it. `build.rs` makes cargo watch `frontend/dist`, and never runs npm.
    #[test]
    fn build_rs_watches_dist_and_never_runs_npm() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
        let src = std::fs::read_to_string(&path).expect("velocity-inspector has a build.rs");
        assert!(
            src.contains("\"cargo:rerun-if-changed=frontend/dist\""),
            "{src}"
        );
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        assert!(!code.contains("npm") && !code.contains("Command"), "{code}");
    }

    /// review-d7 m2: a Homebrew source build (`--HEAD`) without an npm step
    /// has no UI. The formula either builds the UI first or says so.
    #[test]
    fn the_homebrew_formula_builds_the_ui_or_says_it_has_none() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Formula/velocity.rb");
        let formula = std::fs::read_to_string(path).unwrap();
        let builds_ui = formula.contains("npm run build")
            || (formula.contains(r#""npm", "ci""#) && formula.contains(r#""npm", "run", "build""#));
        if !builds_ui {
            assert!(formula.contains("def caveats"), "{formula}");
            assert!(
                formula.contains("no `velocity inspect` web UI"),
                "{formula}"
            );
        }
    }

    #[tokio::test]
    async fn assets_and_spa_routes_are_served() {
        let get = |p: &str| match p {
            "index.html" => Some(Cow::Borrowed(&b"<div id=root>"[..])),
            "app.js" => Some(Cow::Borrowed(&b"js"[..])),
            _ => None,
        };
        let js = serve("/app.js", get);
        assert_eq!(js.status(), StatusCode::OK);
        assert_eq!(body(js).await, "js");
        let spa = serve("/devices/1", get);
        assert_eq!(spa.status(), StatusCode::OK);
        assert_eq!(body(spa).await, "<div id=root>");
    }
}
