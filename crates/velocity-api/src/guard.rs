//! Where an `http` step may go, and what a redirect may carry.
//!
//! Two rules (security audit 2026-11):
//!
//! - **Redirects.** reqwest drops `Authorization` and `Cookie` when a
//!   redirect changes host, but not a suite's own headers (`X-Api-Key`), and
//!   it replays a 307/308 body. So a request that carries headers of its
//!   own (`config.http.headers` or the step's) follows redirects on the same
//!   origin only; a hop to another origin stops there, and the step sees
//!   the 3xx. A redirect from `https` to `http` always stops.
//! - **Cloud metadata.** With [`BLOCK_METADATA_ENV`] set (the MCP server
//!   sets it for a suite an agent wrote), no request reaches a link-local,
//!   unspecified or metadata address, by IP literal, by a name that resolves
//!   there, or through a redirect: an agent's suite could otherwise read the
//!   host's cloud credentials from `169.254.169.254` and send them on.

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::{Attempt, Policy};
use reqwest::Url;

/// Set to `1` to refuse cloud-metadata and link-local destinations.
pub const BLOCK_METADATA_ENV: &str = "VELOCITY_HTTP_BLOCK_METADATA";

/// Most redirects one request follows.
const MAX_REDIRECTS: usize = 10;

/// Whether this process refuses metadata destinations.
pub fn metadata_blocked() -> bool {
    std::env::var(BLOCK_METADATA_ENV).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Host names that are a cloud metadata service whatever they resolve to.
const METADATA_NAMES: &[&str] = &[
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
    "instance-data.ec2.internal",
];

/// Whether `ip` is a link-local, unspecified or metadata address.
pub fn blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            v6.is_unspecified()
                // fe80::/10, link-local.
                || (seg[0] & 0xffc0) == 0xfe80
                // AWS's IPv6 metadata endpoint, fd00:ec2::254.
                || (seg[0] == 0xfd00 && seg[1] == 0x0ec2)
        }
    }
}

/// Why `url` may not be requested in this process, or `None`.
pub fn refused_url(url: &Url) -> Option<String> {
    refused_when(url, metadata_blocked())
}

fn refused_when(url: &Url, blocked: bool) -> Option<String> {
    if !blocked {
        return None;
    }
    let host = url.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let name = bare.trim_end_matches('.').to_ascii_lowercase();
    let bad = match bare.parse::<IpAddr>() {
        Ok(ip) => blocked_ip(ip),
        Err(_) => METADATA_NAMES.contains(&name.as_str()),
    };
    bad.then(|| {
        format!(
            "http: {host} is a link-local or cloud-metadata address, which a suite an agent \
             wrote may not reach ({BLOCK_METADATA_ENV})"
        )
    })
}

/// A resolver that drops link-local and metadata addresses, so a name
/// pointing at `169.254.169.254` connects nowhere.
pub struct FilteringResolver;

impl Resolve for FilteringResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| !blocked_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!(
                    "{host} resolves only to link-local or metadata addresses ({BLOCK_METADATA_ENV})"
                )
                .into());
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// What to do with one redirect hop.
fn decide(attempt: Attempt, same_origin_only: bool) -> reqwest::redirect::Action {
    let previous = attempt.previous();
    if previous.len() > MAX_REDIRECTS {
        return attempt.error("too many redirects");
    }
    let next = attempt.url();
    if let Some(why) = refused_url(next) {
        return attempt.error(why);
    }
    let Some(from) = previous.last() else {
        return attempt.follow();
    };
    if from.scheme() == "https" && next.scheme() == "http" {
        return attempt.stop();
    }
    if same_origin_only && !same_origin(previous.first().unwrap_or(from), next) {
        return attempt.stop();
    }
    attempt.follow()
}

/// The policy for a request: `same_origin_only` when it carries headers
/// of its own.
pub fn redirect_policy(same_origin_only: bool) -> Policy {
    Policy::custom(move |attempt| decide(attempt, same_origin_only))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_and_metadata_addresses_are_blocked() {
        for ip in [
            "169.254.169.254",
            "169.254.0.1",
            "0.0.0.0",
            "::",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
        ] {
            assert!(blocked_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "93.184.216.34",
            "::1",
            "2001:db8::1",
        ] {
            assert!(!blocked_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn an_http_step_can_not_reach_the_metadata_service() {
        for u in [
            "http://169.254.169.254/latest/api/token",
            "http://[fd00:ec2::254]/",
            "http://metadata.google.internal./computeMetadata/v1/",
            "http://[::ffff:a9fe:a9fe]/",
        ] {
            let url = Url::parse(u).unwrap();
            assert!(refused_when(&url, true).is_some(), "{u}");
            assert!(refused_when(&url, false).is_none(), "{u}");
        }
        let fine = Url::parse("http://127.0.0.1:8080/api").unwrap();
        assert!(refused_when(&fine, true).is_none());
    }
}
