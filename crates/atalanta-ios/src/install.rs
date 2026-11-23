//! Install-by-hash on iOS (see `atalanta_common::install`).
//!
//! A simulator's apps live on this Mac, so the marker does too:
//! `~/.atalanta/installs/<udid>/<bundle>.sha`, holding the build's hash and
//! the installed bundle's path and `Info.plist` mtime right after the
//! install. `simctl install` puts every install in a fresh container, so a
//! reinstall by anyone else changes the stamp and the next run installs
//! again. A real device's marker is stamped with what `devicectl` reports
//! for the installed app (its bundle URL and versions).

use std::path::Path;

use atalanta_common::install::{fingerprint as hash_fingerprint, host_marker_path, Marker};
use atalanta_common::{InstallOutcome, Result};
use tracing::debug;

use crate::devicectl::Devicectl;
use crate::simctl::Simctl;

/// `CFBundleIdentifier` of an `.app` bundle (binary or XML plist).
pub fn bundle_id(app: &Path) -> Option<String> {
    bundle_id_with("plutil", app, PLUTIL_TIMEOUT)
}

/// How long `plutil` may take: a wedged one held the caller's thread (a
/// tokio worker) with no deadline (VEL-S50).
const PLUTIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn bundle_id_with(plutil: &str, app: &Path, timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(plutil)
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(app.join("Info.plist"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    // The answer is one short line: it never fills the pipe before exit.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                debug!(app = %app.display(), "plutil gave no answer in time");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let id = out.trim().to_string();
    (status.success() && !id.is_empty()).then_some(id)
}

/// The installed copy of `bundle` on a simulator: its path and the mtime of
/// its `Info.plist`. `None` when it isn't installed.
async fn simulator_stamp(simctl: &Simctl, udid: &str, bundle: &str) -> Option<String> {
    let path = simctl.get_app_container(udid, bundle, "app").await.ok()?;
    let mtime = std::fs::metadata(Path::new(&path).join("Info.plist"))
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{path}@{mtime}"))
}

async fn stamp(
    simctl: &Simctl,
    devicectl: &Devicectl,
    udid: &str,
    bundle: &str,
    physical: bool,
) -> Option<String> {
    if physical {
        devicectl.app_stamp(udid, bundle).await
    } else {
        simulator_stamp(simctl, udid, bundle).await
    }
}

/// Install `app` as `bundle` on a simulator or device, unless the build
/// whose hash is `hash` is installed there already.
pub(crate) async fn install_if_changed(
    simctl: &Simctl,
    devicectl: &Devicectl,
    udid: &str,
    app: &str,
    bundle: &str,
    hash: &str,
    physical: bool,
) -> Result<InstallOutcome> {
    let marker_path = host_marker_path(udid, bundle);
    let before = stamp(simctl, devicectl, udid, bundle, physical).await;
    if let (Some(marker), Some(now)) = (Marker::read(&marker_path), before.as_deref()) {
        if marker.matches(hash, now) {
            return Ok(InstallOutcome::Skipped);
        }
    }
    debug!(udid, bundle, ?before, "installing");
    if physical {
        devicectl.install(udid, app).await?;
    } else {
        simctl.install(udid, app).await?;
    }
    if let Some(after) = stamp(simctl, devicectl, udid, bundle, physical).await {
        let marker = Marker {
            hash: hash.to_string(),
            stamp: after,
        };
        if let Err(e) = marker.write(&marker_path) {
            debug!("could not write {}: {e}", marker_path.display());
        }
    } else {
        // Installed, but not under `bundle`: the marker would never match.
        let _ = std::fs::remove_file(&marker_path);
        debug!(udid, bundle, "installed app not found under its bundle id");
    }
    Ok(InstallOutcome::Installed)
}

/// `sha256:<hash>` when the marker still vouches for the installed build.
pub(crate) async fn fingerprint(
    simctl: &Simctl,
    devicectl: &Devicectl,
    udid: &str,
    bundle: &str,
    physical: bool,
) -> Option<String> {
    let marker = Marker::read(&host_marker_path(udid, bundle))?;
    let now = stamp(simctl, devicectl, udid, bundle, physical).await?;
    marker
        .matches(&marker.hash, &now)
        .then(|| hash_fingerprint(&marker.hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_bundle_id_from_info_plist() {
        if std::process::Command::new("plutil")
            .arg("-help")
            .output()
            .is_err()
        {
            return; // not macOS
        }
        let dir = std::env::temp_dir().join(format!("atalanta-bundle-{}", std::process::id()));
        let app = dir.join("Demo.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>dev.atalanta.demo</string></dict></plist>"#,
        )
        .unwrap();
        assert_eq!(bundle_id(&app).as_deref(), Some("dev.atalanta.demo"));
        assert_eq!(bundle_id(&dir.join("Missing.app")), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_plutil_that_never_answers_is_given_up_on() {
        // VEL-S50: plutil ran with no deadline on the caller's thread.
        let dir = std::env::temp_dir().join(format!("atalanta-plutil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("plutil");
        crate::write_stub(&stub, "exec sleep 60");
        let t = std::time::Instant::now();
        let id = bundle_id_with(
            stub.to_str().unwrap(),
            &dir.join("Demo.app"),
            std::time::Duration::from_millis(500),
        );
        assert_eq!(id, None);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t.elapsed()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
