//! Request guard for the inspector server.
//!
//! The inspector drives a real device and writes files, so a page the
//! developer happens to visit (or anyone on the LAN) must not be able to use
//! it. Every request is checked for:
//! - a `Host` header naming this server (defeats DNS rebinding),
//! - an `Origin`, when the browser sends one, that is this server (or a
//!   loopback dev server such as Vite),
//! - on `/api/*`, `Sec-Fetch-Site` (when sent) of `same-origin` or `none`,
//! - on `/api/*` and the `/review` page, the per-session token, from the
//!   `x-atalanta-token` header, a `token` query parameter or the cookie set
//!   when the UI (or `/review`) is opened with `?token=…`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Header carrying the session token on API calls.
pub const TOKEN_HEADER: &str = "x-atalanta-token";

/// Which `Host` values the server answers to.
#[derive(Debug, Clone, Default)]
pub struct HostPolicy {
    /// Extra host names/addresses besides the loopback ones.
    pub extra_hosts: Vec<String>,
    /// Bound to every interface: accept any `Host` (the token still applies),
    /// but then `Origin` must be exactly this server.
    pub any_host: bool,
}

impl HostPolicy {
    /// The policy for a server bound to `ip`.
    pub fn for_bind_addr(ip: IpAddr) -> Self {
        if ip.is_unspecified() {
            HostPolicy {
                extra_hosts: vec![],
                any_host: true,
            }
        } else if ip.is_loopback() {
            HostPolicy::default()
        } else {
            let host = match ip {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            };
            HostPolicy {
                extra_hosts: vec![host],
                any_host: false,
            }
        }
    }

    fn allows_name(&self, name: &str) -> bool {
        is_loopback_name(name)
            || self
                .extra_hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(name))
    }
}

fn is_loopback_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "localhost" || name == "127.0.0.1" || name == "[::1]"
}

/// `host[:port]` -> `host`, keeping IPv6 brackets.
fn host_name(authority: &str) -> &str {
    if authority.starts_with('[') {
        match authority.find(']') {
            Some(end) => &authority[..=end],
            None => authority,
        }
    } else {
        authority.split(':').next().unwrap_or(authority)
    }
}

/// A fresh random session token (128 bits, hex).
pub fn new_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !from_os {
        // std seeds RandomState from the OS RNG (also on Windows).
        use std::hash::{BuildHasher, Hasher};
        for (i, chunk) in bytes.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
            );
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Name of the cookie holding `token`, unique per session so two inspectors
/// on different ports of `localhost` don't overwrite each other's.
pub fn cookie_name(token: &str) -> String {
    format!("atalanta_inspector_{}", &token[..token.len().min(8)])
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_token(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

fn cookie_tokens(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .filter(|(k, _)| k.starts_with("atalanta_inspector_"))
        .map(|(_, v)| v)
}

fn is_api(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Why a request was refused, or `None` when it may proceed.
pub fn check(
    policy: &HostPolicy,
    token: &str,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Option<(StatusCode, &'static str)> {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return Some((StatusCode::FORBIDDEN, "missing Host header"));
    };
    if !policy.any_host && !policy.allows_name(host_name(host)) {
        return Some((StatusCode::FORBIDDEN, "Host not allowed"));
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let Ok(origin) = origin.to_str() else {
            return Some((StatusCode::FORBIDDEN, "Origin not allowed"));
        };
        let authority = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        // Only the page this server served. Another localhost port is
        // same-site to a browser (it gets the Strict cookie), so a loopback
        // Origin on its own proves nothing. The Vite dev server proxies
        // `/api` without `changeOrigin`, so its requests arrive with Origin
        // equal to Host and pass here too. `null`, `file://`, extensions and
        // the rest never match.
        let ok = authority.is_some_and(|a| a.eq_ignore_ascii_case(host));
        if !ok {
            return Some((StatusCode::FORBIDDEN, "Origin not allowed"));
        }
    }

    let api = is_api(path);
    if api || path == crate::server::REVIEW_PATH {
        // Browsers say where a request came from even when they omit
        // `Origin` (e.g. some GETs): the API is only for this page itself,
        // or for non-browser clients that send no such header.
        // The review page is opened from the printed URL (`none`) like
        // the UI; its token check is what guards it.
        if let Some(site) = headers.get("sec-fetch-site").filter(|_| api) {
            if !matches!(site.to_str(), Ok("same-origin" | "none")) {
                return Some((StatusCode::FORBIDDEN, "cross-site request refused"));
            }
        }
        let presented = headers
            .get(TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .into_iter()
            .chain(query_token(query))
            .chain(cookie_tokens(headers));
        let mut ok = false;
        for t in presented {
            ok |= constant_time_eq(t.as_bytes(), token.as_bytes());
        }
        if !ok {
            return Some((
                StatusCode::UNAUTHORIZED,
                "missing or wrong inspector token (open the URL printed by `atalanta inspect`)",
            ));
        }
    }
    None
}

/// axum middleware applying [`check`]; when the UI is opened with a valid
/// `?token=`, it redirects to the same page without it and sets the session
/// cookie, so the page's own requests (fetch, `<img>`, WebSocket) carry it.
/// Every response gets nosniff / no-referrer / no-framing headers.
pub async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    if let Some((code, msg)) = check(
        &state.host_policy,
        &state.token,
        &path,
        query.as_deref(),
        req.headers(),
    ) {
        tracing::warn!(%path, "inspector refused request: {msg}");
        return (code, msg).into_response();
    }

    let api_path = is_api(&path);
    let set_cookie = !api_path
        && query_token(query.as_deref())
            .is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()));

    let mut resp = if set_cookie {
        // Swap the token in the address bar for the cookie, so it stays out
        // of history and Referer.
        let rest: Vec<&str> = query
            .as_deref()
            .unwrap_or("")
            .split('&')
            .filter(|kv| !kv.is_empty() && !kv.starts_with("token="))
            .collect();
        let location = if rest.is_empty() {
            path.clone()
        } else {
            format!("{path}?{}", rest.join("&"))
        };
        let mut r = StatusCode::FOUND.into_response();
        if let Ok(v) = HeaderValue::from_str(&location) {
            r.headers_mut().insert(header::LOCATION, v);
        }
        let cookie = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict",
            cookie_name(&state.token),
            state.token
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
        r
    } else {
        next.run(req).await
    };

    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Other sites can't embed the screenshot or other API responses.
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    if api_path {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn api_needs_the_token() {
        let p = HostPolicy::default();
        let h = headers(&[("host", "127.0.0.1:9876")]);
        assert_eq!(
            check(&p, T, "/api/devices", None, &h).map(|r| r.0),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert!(check(
            &p,
            T,
            "/api/devices",
            Some("token=".to_owned().as_str()),
            &h
        )
        .is_some());
        assert!(check(&p, T, "/api/devices", Some(&format!("t=1&token={T}")), &h).is_none());
        let h = headers(&[("host", "localhost:9876"), (TOKEN_HEADER, T)]);
        assert!(check(&p, T, "/api/flow/save", None, &h).is_none());
        let h = headers(&[
            ("host", "localhost:9876"),
            ("cookie", &format!("a=b; {}={T}", cookie_name(T))),
        ]);
        assert!(check(&p, T, "/api/ws/dev", None, &h).is_none());
        // Static assets don't need it.
        let h = headers(&[("host", "localhost:9876")]);
        assert!(check(&p, T, "/index.html", None, &h).is_none());
    }

    /// U1: the review page accepts baselines, so it needs the token too;
    /// opening its printed URL (`Sec-Fetch-Site: none`) works.
    #[test]
    fn the_review_page_needs_the_token() {
        let p = HostPolicy::default();
        let h = headers(&[("host", "localhost:9876"), ("sec-fetch-site", "none")]);
        assert_eq!(
            check(&p, T, "/review", None, &h).map(|r| r.0),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert!(check(&p, T, "/review", Some(&format!("token={T}")), &h).is_none());
        let h = headers(&[
            ("host", "localhost:9876"),
            ("sec-fetch-site", "cross-site"),
            ("cookie", &format!("{}={T}", cookie_name(T))),
        ]);
        assert!(check(&p, T, "/review", None, &h).is_none());
    }

    #[test]
    fn foreign_host_is_refused_even_with_the_token() {
        // DNS rebinding: evil.example resolves to 127.0.0.1.
        let p = HostPolicy::default();
        let h = headers(&[("host", "evil.example:9876"), (TOKEN_HEADER, T)]);
        assert_eq!(
            check(&p, T, "/api/devices", None, &h).map(|r| r.0),
            Some(StatusCode::FORBIDDEN)
        );
        assert!(check(&p, T, "/", None, &h).is_some());
        assert!(check(&p, T, "/", None, &HeaderMap::new()).is_some());
    }

    #[test]
    fn cross_origin_is_refused() {
        let p = HostPolicy::default();
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1.evil.example",
        ] {
            let h = headers(&[
                ("host", "127.0.0.1:9876"),
                ("origin", origin),
                (TOKEN_HEADER, T),
            ]);
            assert!(
                check(&p, T, "/api/flow/save", None, &h).is_some(),
                "{origin}"
            );
        }
        let h = headers(&[
            ("host", "127.0.0.1:9876"),
            ("origin", "http://127.0.0.1:9876"),
            (TOKEN_HEADER, T),
        ]);
        assert!(check(&p, T, "/api/flow/save", None, &h).is_none());
    }

    #[test]
    fn cross_site_fetch_metadata_is_refused_on_the_api() {
        let p = HostPolicy::default();
        for site in ["cross-site", "same-site"] {
            let h = headers(&[
                ("host", "127.0.0.1:9876"),
                ("sec-fetch-site", site),
                (TOKEN_HEADER, T),
            ]);
            assert!(check(&p, T, "/api/devices", None, &h).is_some(), "{site}");
        }
        for site in ["same-origin", "none"] {
            let h = headers(&[
                ("host", "127.0.0.1:9876"),
                ("sec-fetch-site", site),
                (TOKEN_HEADER, T),
            ]);
            assert!(check(&p, T, "/api/devices", None, &h).is_none(), "{site}");
        }
        // Following a link to the UI from elsewhere is fine.
        let h = headers(&[("host", "127.0.0.1:9876"), ("sec-fetch-site", "cross-site")]);
        assert!(check(&p, T, "/", None, &h).is_none());
    }

    #[test]
    fn host_policy_follows_the_bind_address() {
        let lan = HostPolicy::for_bind_addr("192.168.1.5".parse().unwrap());
        let h = headers(&[("host", "192.168.1.5:9876"), (TOKEN_HEADER, T)]);
        assert!(check(&lan, T, "/api/devices", None, &h).is_none());
        let h = headers(&[("host", "evil.example"), (TOKEN_HEADER, T)]);
        assert!(check(&lan, T, "/api/devices", None, &h).is_some());

        let any = HostPolicy::for_bind_addr("0.0.0.0".parse().unwrap());
        let h = headers(&[("host", "box.lan:9876"), (TOKEN_HEADER, T)]);
        assert!(check(&any, T, "/api/devices", None, &h).is_none());
        // Origin must then be this exact server, not just any loopback page.
        let h = headers(&[
            ("host", "box.lan:9876"),
            ("origin", "http://localhost:5173"),
            (TOKEN_HEADER, T),
        ]);
        assert!(check(&any, T, "/api/devices", None, &h).is_some());
    }

    /// A page on another localhost port (a dev server, a tool running
    /// third-party code) is not this server, even with the cookie it gets
    /// as a same-site request. The Vite proxy, which keeps the browser's
    /// Host, still passes.
    #[test]
    fn another_localhost_origin_is_refused() {
        let p = HostPolicy::default();
        for (host, origin) in [
            ("127.0.0.1:9876", "http://localhost:3000"),
            ("localhost:9876", "http://localhost:3000"),
            ("localhost:9876", "http://127.0.0.1:9876"),
            ("127.0.0.1:9876", "http://[::1]:9876"),
        ] {
            let h = headers(&[
                ("host", host),
                ("origin", origin),
                ("cookie", &format!("{}={T}", cookie_name(T))),
            ]);
            assert_eq!(
                check(&p, T, "/api/ws/dev", None, &h).map(|r| r.0),
                Some(StatusCode::FORBIDDEN),
                "{host} from {origin}"
            );
        }
        // `npm run dev`: Vite on :5173 proxies /api without changeOrigin.
        let h = headers(&[
            ("host", "localhost:5173"),
            ("origin", "http://localhost:5173"),
            ("sec-fetch-site", "same-origin"),
            ("cookie", &format!("{}={T}", cookie_name(T))),
        ]);
        assert!(check(&p, T, "/api/ws/dev", None, &h).is_none());
    }

    #[test]
    fn tokens_are_random_and_long() {
        let a = new_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, new_token());
    }
}
