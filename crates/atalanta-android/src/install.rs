//! Install-by-hash on Android (see `atalanta_common::install`).
//!
//! The marker lives on the device, in
//! `/data/local/tmp/atalanta-installed/<package>.sha`: it travels with the
//! device whichever host drives it, and a read-only emulator drops it on
//! exit together with the app it describes. It holds the build's hash and
//! the package's `lastUpdateTime` right after the install; a skip needs the
//! hash to match, `pm path` to find the package, and `lastUpdateTime` to be
//! unchanged (anything else that installed the package moves it).

use atalanta_common::install::{fingerprint as hash_fingerprint, Marker};
use atalanta_common::{AtalantaError, InstallOutcome, Result};
use tracing::debug;

use crate::adb_backend::AdbBackend;

const MARKER_DIR: &str = "/data/local/tmp/atalanta-installed";
const SEP: &str = "---atalanta---";

/// What the device says about `package`, in one `adb shell`.
#[derive(Debug, Default, PartialEq)]
struct Installed {
    marker: Option<Marker>,
    /// `pm path` found the package.
    present: bool,
    /// `lastUpdateTime=` from `dumpsys package` (empty if unknown).
    last_update: String,
}

fn marker_path(package: &str) -> String {
    format!("{MARKER_DIR}/{package}.sha")
}

async fn query(adb: &dyn AdbBackend, device_id: &str, package: &str) -> Result<Installed> {
    crate::async_adb::validate_package_name(package)?;
    // `; true`: a missing package or marker is an answer, not an error.
    let script = format!(
        "cat {m} 2>/dev/null; echo {SEP}; pm path {package} 2>/dev/null; echo {SEP}; \
         dumpsys package {package} | grep -m1 lastUpdateTime; true",
        m = marker_path(package)
    );
    let out = adb.run_device(device_id, &["shell", &script]).await?;
    Ok(parse_query(&out))
}

fn parse_query(out: &str) -> Installed {
    let mut parts = out.split(SEP);
    let marker = parts.next().and_then(Marker::parse);
    let present = parts
        .next()
        .is_some_and(|p| p.lines().any(|l| l.trim().starts_with("package:")));
    let last_update = parts
        .next()
        .and_then(|p| {
            p.lines()
                .find_map(|l| l.trim().strip_prefix("lastUpdateTime="))
        })
        .unwrap_or("")
        .trim()
        .to_string();
    Installed {
        marker,
        present,
        last_update,
    }
}

/// Install `apk` as `package` unless the device already has the build
/// whose hash is `hash`.
pub(crate) async fn install_if_changed(
    adb: &dyn AdbBackend,
    device_id: &str,
    apk: &str,
    package: &str,
    hash: &str,
) -> Result<InstallOutcome> {
    let before = query(adb, device_id, package).await?;
    if before.present
        && before
            .marker
            .as_ref()
            .is_some_and(|m| m.matches(hash, &before.last_update))
    {
        return Ok(InstallOutcome::Skipped);
    }
    debug!(device = device_id, package, ?before, "installing");
    if let Err(e) = adb.install_app(device_id, apk).await {
        // A build signed with another key, or an older versionCode, can't
        // replace the installed one; CI builds hit both. Start over.
        let msg = e.to_string();
        if !(msg.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE")
            || msg.contains("INSTALL_FAILED_VERSION_DOWNGRADE"))
        {
            return Err(e);
        }
        debug!(device = device_id, package, "uninstalling first: {msg}");
        let _ = adb.run_device(device_id, &["uninstall", package]).await;
        adb.install_app(device_id, apk).await?;
    }
    let after = query(adb, device_id, package).await?;
    if !after.present {
        return Err(AtalantaError::Config(format!(
            "installed {apk} on {device_id}, but package {package} isn't there: \
             does appId match the APK's package name?"
        )));
    }
    let marker = Marker {
        hash: hash.to_string(),
        stamp: after.last_update,
    };
    // Best effort: without the marker the next run just installs again.
    let write = format!(
        "mkdir -p {MARKER_DIR} && printf '%s\\n%s\\n' '{}' '{}' > {}",
        marker.hash,
        marker.stamp.replace('\'', ""),
        marker_path(package)
    );
    if let Err(e) = adb.run_device(device_id, &["shell", &write]).await {
        debug!(device = device_id, "could not write install marker: {e}");
    }
    Ok(InstallOutcome::Installed)
}

/// `sha256:<hash>` when the device's marker still vouches for the
/// installed build; `None` otherwise (the caller falls back to version and
/// install time).
pub(crate) async fn fingerprint(
    adb: &dyn AdbBackend,
    device_id: &str,
    package: &str,
) -> Option<String> {
    let now = query(adb, device_id, package).await.ok()?;
    let marker = now.marker?;
    (now.present && marker.matches(&marker.hash, &now.last_update))
        .then(|| hash_fingerprint(&marker.hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_marker_package_and_install_time_from_one_shell() {
        let hash = "c".repeat(64);
        let out = format!(
            "{hash}\n2026-10-06 09:30:01\n{SEP}\npackage:/data/app/~~x/com.example-1/base.apk\n{SEP}\n    lastUpdateTime=2026-10-06 09:30:01\n"
        );
        let q = parse_query(&out);
        assert!(q.present);
        assert_eq!(q.last_update, "2026-10-06 09:30:01");
        assert!(q.marker.unwrap().matches(&hash, &q.last_update));
    }

    #[test]
    fn a_missing_package_or_marker_means_install() {
        let q = parse_query(&format!("{SEP}\n{SEP}\n"));
        assert_eq!(q, Installed::default());
        // Marker left behind by an app someone uninstalled.
        let hash = "d".repeat(64);
        let q = parse_query(&format!("{hash}\nT\n{SEP}\n{SEP}\n"));
        assert!(!q.present);
        assert!(q.marker.is_some());
    }
}
