// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Review MINOR-10: the Chrome profile directory is ours alone (created
//! here, 0700) and doesn't leak when `prepare` runs twice. Its own test
//! binary: it counts this process's profile dirs.

mod common;

use atalanta_common::PlatformDriver;

fn my_profiles() -> Vec<std::path::PathBuf> {
    let mine = format!("atalanta-chrome-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&mine))
        .map(|e| e.path())
        .collect()
}

/// These tests count this process's profile dirs: one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a killed Chrome gets to leave the process table: SIGKILL is
/// immediate but the exit is the kernel's, and a loaded machine is slow.
const GONE: std::time::Duration = std::time::Duration::from_secs(15);

/// Alive and not a zombie waiting to be reaped.
fn running(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            let stat = String::from_utf8_lossy(&o.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
}

fn parallel_driver() -> atalanta_web::WebDriver {
    atalanta_web::WebDriver::new(atalanta_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        instances: 2,
        parallel_mode: atalanta_web::ParallelMode::Processes,
        ..Default::default()
    })
}

#[tokio::test]
async fn profile_dir_is_private_and_not_leaked() {
    let _serial = SERIAL.lock().await;
    require_chrome!();
    let d = common::driver(vec![]).await;
    d.prepare("web").await.unwrap();
    let dirs = my_profiles();
    assert_eq!(
        dirs.len(),
        1,
        "the first session's profile leaked: {dirs:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dirs[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }
    d.cleanup().await;
    assert!(my_profiles().is_empty());
}

/// review-d6 M2: the second signal (`kill_all_groups`, then
/// `process::exit`, which runs no destructors) left every `--parallel`
/// Chrome running with its profile dir: Chrome runs in Atalanta's own
/// process group and was registered nowhere.
#[tokio::test]
async fn the_second_signal_kills_every_chrome_and_removes_its_profile() {
    let _serial = SERIAL.lock().await;
    require_chrome!();
    let d = parallel_driver();
    d.prepare("web-0").await.unwrap();
    d.prepare("web-1").await.unwrap();
    let pids = d.debug_browser_pids().await;
    assert_eq!(pids.len(), 2);
    assert_eq!(my_profiles().len(), 2);
    atalanta_common::process::kill_all_groups();
    for pid in &pids {
        let pid = *pid;
        assert!(
            common::eventually_for(GONE, || async move { !running(pid) }).await,
            "Chrome {pid} survived the second signal"
        );
    }
    assert!(
        my_profiles().is_empty(),
        "profiles left: {:?}",
        my_profiles()
    );
    drop(d);
}

/// review-d6 m5: a driver dropped without `cleanup` (a panic, an early
/// `?`) left one profile dir per browser.
#[tokio::test]
async fn a_dropped_driver_leaves_no_profile_dirs() {
    let _serial = SERIAL.lock().await;
    require_chrome!();
    let d = parallel_driver();
    d.prepare("web-0").await.unwrap();
    d.prepare("web-1").await.unwrap();
    assert_eq!(my_profiles().len(), 2);
    drop(d);
    assert!(
        common::eventually(|| async { my_profiles().is_empty() }).await,
        "profiles left: {:?}",
        my_profiles()
    );
}

fn context_driver() -> atalanta_web::WebDriver {
    atalanta_web::WebDriver::new(atalanta_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        instances: 2,
        ..Default::default()
    })
}

/// W3: in context mode (the default) two devices are one Chrome and one
/// profile; the second signal kills that Chrome and removes the profile.
#[tokio::test]
async fn the_second_signal_kills_the_shared_chrome_and_removes_its_profile() {
    let _serial = SERIAL.lock().await;
    require_chrome!();
    let d = context_driver();
    d.prepare("web-0").await.unwrap();
    d.prepare("web-1").await.unwrap();
    let pids = d.debug_browser_pids().await;
    assert_eq!(pids.len(), 1, "one Chrome for both devices: {pids:?}");
    assert_eq!(my_profiles().len(), 1);
    atalanta_common::process::kill_all_groups();
    let pid = pids[0];
    assert!(
        common::eventually_for(GONE, || async move { !running(pid) }).await,
        "Chrome {pid} survived the second signal"
    );
    assert!(
        my_profiles().is_empty(),
        "profiles left: {:?}",
        my_profiles()
    );
    drop(d);
}

/// W3: a context-mode driver dropped without `cleanup` leaves no profile,
/// and `cleanup` closes the shared Chrome.
#[tokio::test]
async fn context_mode_leaves_no_profile_or_chrome_behind() {
    let _serial = SERIAL.lock().await;
    require_chrome!();
    let d = context_driver();
    d.prepare("web-0").await.unwrap();
    d.prepare("web-1").await.unwrap();
    // A second prepare replaces the context, not the Chrome.
    let pid = d.debug_browser_pids().await;
    d.prepare("web-1").await.unwrap();
    assert_eq!(d.debug_browser_pids().await, pid);
    assert_eq!(my_profiles().len(), 1);
    d.cleanup().await;
    assert!(my_profiles().is_empty(), "{:?}", my_profiles());
    let pid = pid[0];
    assert!(common::eventually_for(GONE, || async move { !running(pid) }).await);

    let d = context_driver();
    d.prepare("web-0").await.unwrap();
    d.prepare("web-1").await.unwrap();
    assert_eq!(my_profiles().len(), 1);
    drop(d);
    assert!(
        common::eventually(|| async { my_profiles().is_empty() }).await,
        "profiles left: {:?}",
        my_profiles()
    );
}
