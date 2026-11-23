//! AI: which provider keys are set (names only, never values), the
//! provider and models natural-language steps resolve to, and (`--deep`)
//! whether the API host answers.

use std::time::{Duration, Instant};

use atalanta_nl::provider::{AiSettings, PRESETS};

use crate::{timed, Area, AreaResult, Check, Ctx, Fix};

const A: Area = Area::Ai;

pub(crate) async fn checks(ctx: &Ctx) -> AreaResult {
    let env = ctx.env();
    let mut out = Vec::new();
    let mut set: Vec<&str> = PRESETS
        .iter()
        .flat_map(|p| p.key_envs.iter().copied())
        .filter(|k| env.is_set(k))
        .collect();
    set.dedup();
    let lookup = |k: &str| env.var(k);
    let cache = ctx.opts.cwd.join(".atalanta").join("nl-cache.json");
    let settings = match AiSettings::resolve_with(None, &lookup) {
        Ok(s) => s,
        Err(e) => {
            out.push(
                Check::fail("ai.keys", A, format!("AI settings: {e}")).with_fix(Fix::guidance(
                    "fix ATALANTA_NL_PROVIDER / ATALANTA_NL_MODEL (see docs/AI-PROVIDERS.md)",
                )),
            );
            return (out, false);
        }
    };
    let Some(settings) = settings else {
        out.push(
            Check::skip(
                "ai.keys",
                A,
                format!(
                    "no provider key set: natural-language steps run only from {}",
                    if cache.exists() {
                        "the committed .atalanta/nl-cache.json"
                    } else {
                        "a .atalanta/nl-cache.json (none here)"
                    }
                ),
            )
            .with_fix(Fix::guidance(
                "export ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY or another \
                 provider's key (docs/AI-PROVIDERS.md); only its name is ever shown",
            )),
        );
        return (out, false);
    };
    out.push(
        Check::ok(
            "ai.keys",
            A,
            match &settings.api_key_env {
                Some(var) => format!("{} via {var}", settings.preset.display),
                None => format!("{} (no key needed)", settings.preset.display),
            },
        )
        .with_detail(if set.len() > 1 {
            format!("also set: {}", set.join(", "))
        } else {
            String::new()
        }),
    );
    out.push(Check::ok(
        "ai.model",
        A,
        format!(
            "{} (explore: {}){}",
            settings.label(),
            settings.explore_model,
            if env.is_set("ATALANTA_NL_MODEL") {
                " (ATALANTA_NL_MODEL)"
            } else {
                ""
            }
        ),
    ));

    let url = settings
        .base_url
        .clone()
        .unwrap_or_else(|| settings.preset.base_url.to_string());
    out.push(if !ctx.opts.deep {
        Check::skip(
            "ai.network",
            A,
            "not checked (`--deep` connects to the API host)",
        )
    } else if url.is_empty() {
        Check::skip(
            "ai.network",
            A,
            format!(
                "{}: endpoint comes from region/project",
                settings.preset.display
            ),
        )
    } else {
        timed(|| reachable(&url, ctx.opts.timeout)).await
    });
    (out, false)
}

/// `https://host[:port]/path` → (host, port).
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let default = if scheme.eq_ignore_ascii_case("http") {
        80
    } else {
        443
    };
    match authority.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => {
            Some((h.trim_matches(['[', ']']).to_string(), p.parse().ok()?))
        }
        _ => Some((authority.trim_matches(['[', ']']).to_string(), default)),
    }
}

/// A TCP connection to the API host: no request is sent, so no key is used.
async fn reachable(url: &str, timeout: Duration) -> Check {
    let Some((host, port)) = host_port(url) else {
        return Check::warn("ai.network", A, format!("can't read the API URL {url}"));
    };
    let started = Instant::now();
    match tokio::time::timeout(
        timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(_)) => Check::ok(
            "ai.network",
            A,
            format!(
                "{host}:{port} reachable ({} ms)",
                started.elapsed().as_millis()
            ),
        ),
        Ok(Err(e)) => Check::warn("ai.network", A, format!("{host}:{port}: {e}")).with_fix(
            Fix::guidance("check the network, proxy, or ATALANTA_NL_API_URL"),
        ),
        Err(_) => Check::warn(
            "ai.network",
            A,
            format!(
                "{host}:{port}: no connection in {:.0} s",
                timeout.as_secs_f32()
            ),
        )
        .with_fix(Fix::guidance(
            "check the network, proxy, or ATALANTA_NL_API_URL",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::{Options, Status};

    fn ctx(t: &Tools, deep: bool) -> Ctx {
        Ctx {
            opts: Options {
                env: t.env(),
                cwd: t.path("proj"),
                deep,
                ..Options::default()
            },
            required: vec![],
            apps: vec![],
        }
    }

    #[tokio::test]
    async fn keys_are_reported_by_name_and_never_by_value() {
        let mut t = Tools::new();
        t.set("ANTHROPIC_API_KEY", "sk-ant-SECRET-VALUE");
        t.set("OPENAI_API_KEY", "sk-OTHER-SECRET");
        t.set("ATALANTA_NL_MODEL", "claude-test");
        let (checks, _) = checks(&ctx(&t, false)).await;
        let json = serde_json::to_string(&checks).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
        assert_eq!(checks[0].summary, "Anthropic API via ANTHROPIC_API_KEY");
        assert!(checks[0]
            .detail
            .as_ref()
            .unwrap()
            .contains("OPENAI_API_KEY"));
        assert!(
            checks[1]
                .summary
                .starts_with("anthropic:claude-test (explore: "),
            "{}",
            checks[1].summary
        );
        assert_eq!(checks[2].status, Status::Skip);
    }

    #[tokio::test]
    async fn without_a_key_it_is_skipped_and_deep_connects_to_the_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let t = Tools::new();
        let (none, _) = checks(&ctx(&t, true)).await;
        assert_eq!(none[0].status, Status::Skip);
        assert_eq!(none.len(), 1);
        let mut t = Tools::new();
        t.set("ATALANTA_NL_PROVIDER", "ollama");
        t.set(
            "ATALANTA_NL_API_URL",
            &format!("http://127.0.0.1:{port}/v1"),
        );
        let (local, _) = checks(&ctx(&t, true)).await;
        assert_eq!(local[0].status, Status::Ok, "{:?}", local[0]);
        assert_eq!(local[2].status, Status::Ok, "{:?}", local[2]);
    }

    #[test]
    fn api_urls_give_host_and_port() {
        assert_eq!(
            host_port("https://api.anthropic.com/v1/messages"),
            Some(("api.anthropic.com".into(), 443))
        );
        assert_eq!(
            host_port("http://localhost:8080/x"),
            Some(("localhost".into(), 8080))
        );
        // Schemes are case-insensitive; a query or fragment ends the authority.
        assert_eq!(
            host_port("HTTP://localhost/v1"),
            Some(("localhost".into(), 80))
        );
        assert_eq!(
            host_port("http://localhost:11434?x=1"),
            Some(("localhost".into(), 11434))
        );
    }
}
