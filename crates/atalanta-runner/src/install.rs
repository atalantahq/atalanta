//! Installing the suite's `app:` build before the first test on a device,
//! by hash (see `atalanta_common::install`): the sequential runner installs
//! on its device, the parallel runner's actors each on the device they
//! lease, so a parallel run installs on all its devices at once.

use std::path::Path;
use std::time::Instant;

use atalanta_common::install::{hash_app_cached, short};
use atalanta_common::{AtalantaError, InstallOutcome, Platform, PlatformDriver, Result, TestSuite};

/// The build to install for this run, if the suite (or `--app`) names one
/// for `platform`.
pub fn build_for(suite: &TestSuite, platform: Option<Platform>) -> Option<String> {
    let platform = platform.or(suite.config.platform);
    suite
        .config
        .app
        .as_ref()?
        .for_platform(platform)
        .map(String::from)
}

/// Hash the build (remembered per process while the files are unchanged).
pub async fn hash(path: &str) -> Result<String> {
    let p = path.to_string();
    tokio::task::spawn_blocking(move || hash_app_cached(Path::new(&p)))
        .await
        .map_err(|e| AtalantaError::Config(format!("hashing {path}: {e}")))?
        .map_err(|e| AtalantaError::Config(format!("app {path}: {e}")))
}

/// Make sure `device_id` runs the build at `path` (as `app_id`): install it
/// unless that exact build is already there. Prints what happened and how
/// long it took.
pub async fn ensure_installed(
    driver: &dyn PlatformDriver,
    device_id: &str,
    path: &str,
    app_id: &str,
) -> Result<InstallOutcome> {
    let start = Instant::now();
    let hash = hash(path).await?;
    let hashed = start.elapsed();
    // The driver's error keeps its classification (device vs config).
    let outcome = driver
        .install_app_if_changed(device_id, path, app_id, &hash)
        .await?;
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let took = start.elapsed();
    match outcome {
        InstallOutcome::Installed => cprintln!(
            "  \x1b[36m↓\x1b[0m installed {name} on {device_id} in {} (sha256 {}, hashed in {})",
            secs(took),
            short(&hash),
            secs(hashed)
        ),
        InstallOutcome::Skipped => cprintln!(
            "  \x1b[32m✓\x1b[0m {name} already installed on {device_id}; skipped (sha256 {}, checked in {})",
            short(&hash),
            secs(took)
        ),
    }
    Ok(outcome)
}

/// `850ms`, `4.2s`.
pub fn secs(d: std::time::Duration) -> String {
    if d.as_millis() < 1000 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::{AppPaths, AppSource, SuiteConfig};

    fn suite(app: Option<AppSource>, platform: Option<Platform>) -> TestSuite {
        TestSuite {
            app_id: "x".into(),
            config: SuiteConfig {
                app,
                platform,
                ..Default::default()
            },
            flows: vec![],
            tests: vec![],
        }
    }

    #[test]
    fn the_build_comes_from_the_platform_the_run_targets() {
        let both = AppSource::PerPlatform(AppPaths {
            android: Some("a.apk".into()),
            ios: Some("A.app".into()),
        });
        let s = suite(Some(both.clone()), None);
        assert_eq!(build_for(&s, Some(Platform::Ios)).as_deref(), Some("A.app"));
        assert_eq!(build_for(&s, None), None);
        let s = suite(Some(both), Some(Platform::Android));
        assert_eq!(build_for(&s, None).as_deref(), Some("a.apk"));
        assert_eq!(build_for(&suite(None, None), Some(Platform::Ios)), None);
    }
}
