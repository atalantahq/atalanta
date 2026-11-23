//! Natural-language planner wiring for the runner.
//!
//! Builds a shared [`NaturalResolver`] when the environment is configured for it,
//! so both the sequential and parallel runners resolve `do:`/`assert:` and
//! bare-string steps through the same on-disk cache.

use std::sync::Arc;

use atalanta_common::VerifyLevel;
use atalanta_nl::{
    AiSettings, CacheMode, NaturalPlanner, NaturalResolver, PlanRequest, PlannedStep, ScreenChecker,
};

/// What every "no model configured" message says to set.
const KEY_HELP: &str =
    "ANTHROPIC_API_KEY, OPENAI_API_KEY, or ATALANTA_NL_API_KEY with ATALANTA_NL_API_URL";

/// The planner when there is no key: never called in read-only mode (a
/// cache miss fails before planning), and says why if it ever is.
struct NoPlanner;

#[async_trait::async_trait]
impl NaturalPlanner for NoPlanner {
    async fn plan(&self, _: &PlanRequest<'_>) -> atalanta_common::Result<PlannedStep> {
        Err(atalanta_common::AtalantaError::Config(format!(
            "natural-language steps need a model key to be planned ({KEY_HELP})"
        )))
    }
}

/// The planner when its settings are bad but the suite has no natural
/// steps: startup carries on, and a step that turns up anyway (a flow file
/// read at run time) fails with the setting's own error.
struct BrokenPlanner(String);

#[async_trait::async_trait]
impl NaturalPlanner for BrokenPlanner {
    async fn plan(&self, _: &PlanRequest<'_>) -> atalanta_common::Result<PlannedStep> {
        Err(atalanta_common::AtalantaError::Config(self.0.clone()))
    }
}

/// Whether any test in `suite` runs with AI verification, given `--verify`.
pub fn needs_checker(suite: &atalanta_common::TestSuite, cli: Option<VerifyLevel>) -> bool {
    suite
        .tests
        .iter()
        .any(|t| VerifyLevel::effective(cli, t.verify, suite.config.verify) != VerifyLevel::Off)
}

/// Whether `suite` has a natural-language step anywhere in its tests or
/// flows (nested steps included).
pub fn uses_natural_steps(suite: &atalanta_common::TestSuite) -> bool {
    fn has(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => m.contains_key("natural") || m.values().any(has),
            serde_json::Value::Array(a) => a.iter().any(has),
            _ => false,
        }
    }
    let parts = (&suite.tests, &suite.flows);
    serde_json::to_value(parts).map_or(true, |v| has(&v))
}

/// Default location for the resolved-step cache (relative to the working dir).
const CACHE_PATH: &str = ".atalanta/nl-cache.json";
/// Where fresh plans are traced for `atalanta nl-eval`.
pub const TRACES_DIR: &str = ".atalanta/nl-traces";

/// Build a resolver if a model key is set (see [`AiSettings`]); otherwise
/// return `None` so suites without natural-language steps run unaffected. A
/// suite that *does* use natural steps without a key fails those steps with
/// a clear message. A key with a bad planner setting (`ATALANTA_NL_EFFORT=hi`)
/// is a configuration error at startup: it left the resolver unset, and every
/// NL step then said the key was missing (NB-4).
pub fn build_resolver() -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    build_resolver_at(CACHE_PATH)
}

/// [`build_resolver`] for a suite: a bad planner setting fails startup only
/// when `uses_nl` (the suite has natural steps); otherwise it's a warning. A
/// global `ANTHROPIC_API_KEY` in CI made pure-YAML suites exit 2 over an NL
/// setting they never use (review M-11).
pub fn build_resolver_for(uses_nl: bool) -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    build_from(AiSettings::from_env(), CACHE_PATH, uses_nl, false)
}

/// [`build_resolver_for`] from the suite itself: its `config.ai`, whether
/// it has natural steps, and whether any test runs `verify:` (with
/// `--verify` as `cli`). Verification with no model to ask is a
/// configuration error at startup (exit 2), not a test failure per test.
pub fn build_resolver_for_suite(
    suite: &atalanta_common::TestSuite,
    cli: Option<VerifyLevel>,
) -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    build_from(
        AiSettings::resolve(suite.config.ai.as_ref()),
        CACHE_PATH,
        uses_natural_steps(suite),
        needs_checker(suite, cli),
    )
}

/// [`build_resolver`] with the cache at `cache_path`.
///
/// Without a key, a cache on disk still replays (read-only, or frozen when
/// asked): CI can run steps planned and committed on a developer machine.
/// It used to return `None` without a key, so the cache was never read and
/// even `ATALANTA_NL_CACHE=read-only` ("CI: never call the model") needed
/// the key.
pub fn build_resolver_at(
    cache_path: &str,
) -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    build_from(AiSettings::from_env(), cache_path, true, false)
}

type Built = (Box<dyn NaturalPlanner>, Option<Box<dyn ScreenChecker>>);

/// [`build_with`] from resolved settings: a bad `config.ai` only fails a
/// suite that needs a model.
fn build_from(
    settings: atalanta_common::Result<Option<AiSettings>>,
    cache_path: &str,
    uses_nl: bool,
    verify: bool,
) -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    let settings = match settings {
        Ok(s) => s,
        Err(e) if uses_nl || verify => return Err(e),
        Err(e) => {
            tracing::warn!("atalanta: the AI settings are invalid: {e}");
            None
        }
    };
    // Nothing in the suite asks a model: no resolver. Building one made the
    // HTTP clients, and with `rustls-tls-native-roots` that loads the
    // system's trust roots (3-4 s from the macOS keychain, more under
    // load) on every run of a pure-YAML suite whenever a key was set; the
    // parallel runner's 1.5 s timeout tests failed on it.
    if !uses_nl && !verify {
        return Ok(None);
    }
    if let Some(s) = &settings {
        tracing::info!("atalanta: natural-language model {}", s.label());
    }
    build_with(settings.is_some(), cache_path, uses_nl, verify, || {
        let s = settings.expect("key_set");
        Ok((s.planner()?, Some(s.checker()?)))
    })
}

/// [`build_resolver_at`], with whether a key is set and how to make the
/// planner (and checker) passed in (tests).
fn build_with(
    key_set: bool,
    cache_path: &str,
    uses_nl: bool,
    verify: bool,
    make: impl FnOnce() -> atalanta_common::Result<Built>,
) -> atalanta_common::Result<Option<Arc<NaturalResolver>>> {
    if !key_set {
        if verify {
            return Err(atalanta_common::AtalantaError::Config(format!(
                "verify: needs a model to check with; set {KEY_HELP}, or run with --verify off"
            )));
        }
        if !std::path::Path::new(cache_path).exists() {
            return Ok(None);
        }
        let mode = match CacheMode::from_env() {
            CacheMode::Frozen => CacheMode::Frozen,
            _ => CacheMode::ReadOnly,
        };
        tracing::info!(
            "atalanta: no model key; replaying natural-language steps from {cache_path}"
        );
        return Ok(Some(Arc::new(
            NaturalResolver::new(Box::new(NoPlanner), cache_path).with_mode(mode),
        )));
    }
    // The cache at `cache_path`, not always the default one (NB-5).
    let (planner, checker) = match make() {
        Ok(p) => p,
        Err(e) if uses_nl || verify => return Err(e),
        Err(e) => {
            tracing::warn!("atalanta: natural-language steps are misconfigured: {e}");
            (
                Box::new(BrokenPlanner(e.to_string())) as Box<dyn NaturalPlanner>,
                None,
            )
        }
    };
    let mut resolver = NaturalResolver::new(planner, cache_path);
    if let Some(checker) = checker {
        resolver = resolver.with_checker(checker);
    }
    if atalanta_common::env::var("ATALANTA_NL_TRACES").as_deref() != Ok("0") {
        resolver = resolver.with_traces(TRACES_DIR);
    }
    Ok(Some(Arc::new(resolver)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // NB-4: a key with a bad planner setting failed every NL step as "no
    // planner configured"; NB-5: the keyed resolver ignored `cache_path`.
    #[test]
    fn a_bad_planner_setting_fails_at_startup_and_the_cache_path_is_used() {
        let err = build_with(true, "unused.json", true, false, || {
            Err(atalanta_common::AtalantaError::Config(
                "ATALANTA_NL_EFFORT=\"hi\" is not one of low, medium, high".into(),
            ))
        })
        .err()
        .expect("a config error");
        assert_eq!(err.exit_code(), 2, "{err}");

        let dir = std::env::temp_dir().join(format!("vel-nl-path-{}", std::process::id()));
        let path = dir.join("custom-cache.json");
        let r = build_with(true, &path.to_string_lossy(), true, false, || {
            Ok((Box::new(NoPlanner), None))
        })
        .unwrap()
        .unwrap();
        assert_eq!(r.cache_path(), path.as_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Review M-11: a key in CI plus a bad NL setting made every suite,
    // pure-YAML ones too, exit 2 before running.
    #[test]
    fn a_bad_planner_setting_only_fails_suites_with_natural_steps() {
        let bad = || -> atalanta_common::Result<Built> {
            Err(atalanta_common::AtalantaError::Config(
                "ATALANTA_NL_EFFORT=xhigh isn't supported".into(),
            ))
        };
        assert!(build_with(true, "unused.json", true, false, bad).is_err());
        assert!(build_with(true, "unused.json", false, true, bad).is_err());
        let r = build_with(true, "unused.json", false, false, bad).expect("starts");
        assert!(r.is_some(), "a late natural step still gets the real error");

        let yaml = |steps: &str| {
            atalanta_core::parser::parse_suite_from_str(&format!(
                "appId: com.x\ntests:\n  - name: t\n    steps:\n{steps}"
            ))
            .unwrap()
        };
        assert!(!uses_natural_steps(&yaml(
            "      - tap:\n          text: OK\n"
        )));
        assert!(uses_natural_steps(&yaml("      - do: tap the OK button\n")));
    }

    // `verify:` with no model to ask fails at startup (exit 2), whether
    // or not a cache is on disk; without verify nothing changes.
    #[test]
    fn verify_without_a_model_is_a_config_error() {
        let ok = || -> atalanta_common::Result<Built> { Ok((Box::new(NoPlanner), None)) };
        let e = build_with(false, "unused.json", false, true, ok)
            .err()
            .unwrap();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--verify off"), "{e}");
        assert!(build_with(false, "/nonexistent/nl.json", true, false, ok)
            .unwrap()
            .is_none());
        let yaml = |level: &str| {
            atalanta_core::parser::parse_suite_from_str(&format!(
                "appId: com.x\nconfig:\n  verify: {level}\ntests:\n  - name: a\n    steps: []\n  - name: b\n    verify: off\n    steps: []\n"
            ))
            .unwrap()
        };
        assert!(needs_checker(&yaml("final"), None));
        assert!(!needs_checker(&yaml("off"), None));
        assert!(!needs_checker(&yaml("final"), Some(VerifyLevel::Off)));
        assert!(needs_checker(&yaml("off"), Some(VerifyLevel::Strict)));
    }

    // The settings are passed as "no key" directly: reading them from the
    // environment made this test return early (and assert nothing) on any
    // machine or CI job that has a model key set.
    #[test]
    fn without_a_key_a_cache_on_disk_still_replays() {
        let ok = || -> atalanta_common::Result<Built> { Ok((Box::new(NoPlanner), None)) };
        let dir = std::env::temp_dir().join(format!("vel-nl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nl-cache.json");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();
        assert!(
            build_with(false, &p, true, false, ok).unwrap().is_none(),
            "no key, no cache: no resolver"
        );
        std::fs::write(&path, "{}").unwrap();
        assert!(
            build_with(false, &p, true, false, ok).unwrap().is_some(),
            "no key, a cache: replay it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
