//! Environment reads that a daemon run can redirect without `set_var`.
//!
//! `atalanta daemon` serves each `atalanta run` with the client's
//! environment. Changing the process environment for that (`set_var`) races
//! with every other thread reading it (`getenv` is not thread-safe on macOS
//! or glibc), and the daemon's runtime has many. Instead the daemon installs
//! the client's variables here for the run, and Atalanta reads its variables
//! through [`var`].

use std::collections::HashMap;
use std::env::VarError;
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

static OVERRIDE: RwLock<Option<Arc<HashMap<String, String>>>> = RwLock::new(None);

/// Named settings (`config.settings`, `--set`), read before everything else.
static SETTINGS: RwLock<Option<Arc<HashMap<String, String>>>> = RwLock::new(None);

/// `std::env::var`: a setting first, then the installed environment when
/// there is one, else the process environment.
pub fn var(key: &str) -> Result<String, VarError> {
    if let Some(v) = setting(key) {
        return Ok(v);
    }
    lookup(current().as_deref(), key)
}

/// Make `vars` (variable name → value, from [`crate::settings::to_env`])
/// win over the environment for this process.
pub fn set_settings(vars: HashMap<String, String>) {
    *SETTINGS.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(vars));
}

/// The installed settings (to restore after a test changed them).
pub fn settings_snapshot() -> HashMap<String, String> {
    SETTINGS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .cloned()
        .unwrap_or_default()
}

fn setting(key: &str) -> Option<String> {
    SETTINGS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(key).cloned())
}

fn lookup(installed: Option<&HashMap<String, String>>, key: &str) -> Result<String, VarError> {
    match installed {
        Some(env) => env.get(key).cloned().ok_or(VarError::NotPresent),
        None => std::env::var(key),
    }
}

/// `std::env::var_os`, from the installed environment when there is one.
pub fn var_os(key: &str) -> Option<OsString> {
    if let Some(v) = setting(key) {
        return Some(OsString::from(v));
    }
    match current() {
        Some(env) => env.get(key).map(OsString::from),
        None => std::env::var_os(key),
    }
}

/// Read variables from `env` (the whole environment: a variable missing
/// from it is unset) until [`clear`].
pub fn install(env: HashMap<String, String>) {
    *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(env));
}

/// Back to the process environment.
pub fn clear() {
    *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Variables a CI server sets. Only `CI` was checked once, which Jenkins,
/// Azure Pipelines and CodeBuild don't set (D-7).
pub const CI_VARS: &[&str] = &[
    "CI",
    "JENKINS_URL",
    "TF_BUILD",
    "CODEBUILD_BUILD_ID",
    "BUILDKITE",
    "GITLAB_CI",
    "GITHUB_ACTIONS",
    "TEAMCITY_VERSION",
    "BITBUCKET_BUILD_NUMBER",
    "CIRCLECI",
];

/// Whether a CI variable's value says "CI": set, and not empty, `0` or
/// `false` (`CI=false` is how create-react-app users opt out locally).
pub fn ci_flag(value: &str) -> bool {
    let v = value.trim();
    !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
}

/// Whether any of [`CI_VARS`] says CI, reading them with `var`.
pub fn in_ci_with(var: impl Fn(&str) -> Option<String>) -> bool {
    CI_VARS.iter().any(|k| var(k).is_some_and(|v| ci_flag(&v)))
}

/// Whether this run is on a CI server, through [`var`]: under `atalanta
/// daemon` that is the client's environment, not the daemon's (review
/// MINOR 9). The one detector healing and the result cache share.
pub fn in_ci() -> bool {
    in_ci_with(|k| var(k).ok())
}

fn current() -> Option<Arc<HashMap<String, String>>> {
    OVERRIDE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Through `lookup`, not `install`: installing would change what every
    // other test in this process reads.
    #[test]
    fn an_installed_environment_is_the_whole_environment() {
        let env = HashMap::from([("ATALANTA_X".to_string(), "client".to_string())]);
        assert_eq!(lookup(Some(&env), "ATALANTA_X").as_deref(), Ok("client"));
        // Set in this process, absent from the client's: unset.
        assert!(lookup(Some(&env), "PATH").is_err());
        assert!(lookup(None, "PATH").is_ok());
    }

    /// Review MINOR 9: one CI detector, fed by the installed (daemon
    /// client's) environment; `CI=false`, `0` and empty don't count, and
    /// the other CI variables do.
    #[test]
    fn ci_is_detected_from_the_given_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(in_ci_with(env(&[("CI", "true")])));
        assert!(in_ci_with(env(&[("JENKINS_URL", "http://j")])));
        for v in ["false", "FALSE", "0", "", "  "] {
            let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([("CI", v)]));
            assert!(!in_ci_with(env(pairs)), "{v:?}");
        }
        assert!(!in_ci_with(env(&[])));
        // Through an installed environment: what `in_ci` reads.
        let installed = HashMap::from([("CI".to_string(), "false".to_string())]);
        assert!(!in_ci_with(|k| lookup(Some(&installed), k).ok()));
        let installed = HashMap::from([("GITHUB_ACTIONS".to_string(), "true".to_string())]);
        assert!(in_ci_with(|k| lookup(Some(&installed), k).ok()));
    }
}
