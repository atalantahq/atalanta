//! `McpServer::hold_lease`: one lease for several calls (an agent command's
//! action, settle and delta). Its own test binary: it points the
//! process-wide ATALANTA_LEASE_DIR at a scratch directory.

use std::sync::Arc;

use atalanta_common::{Platform, PlatformDriver};
use atalanta_mcp::McpServer;
use serde_json::json;

const LAYOUT: &str = r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
  android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
  <Button android:id="@+id/go" android:layout_width="200dp" android:layout_height="48dp" android:text="Continue"/>
</LinearLayout>"#;

async fn server(layout: &std::path::Path, device: &str) -> McpServer {
    let config = atalanta_headless::HeadlessConfig {
        app_path: Some(layout.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let driver: Arc<dyn PlatformDriver> = Arc::new(atalanta_headless::HeadlessDriver::new(
        Platform::Android,
        config,
    ));
    driver.launch_app(device, "app", false).await.unwrap();
    McpServer::new(driver, device.to_string(), None)
        .with_platform(Platform::Android)
        .with_leases(true)
}

#[tokio::test]
async fn a_held_lease_covers_the_holders_calls_and_keeps_others_out() {
    let dir = tempfile::tempdir().unwrap();
    // Only test in this binary: nothing else reads the variable meanwhile.
    std::env::set_var("ATALANTA_LEASE_DIR", dir.path().join("leases"));
    // A short wait for the busy case (the default is 10 s).
    std::env::set_var("ATALANTA_LEASE_WAIT_MS", "1000");
    std::env::remove_var("ATALANTA_LEASES");
    let layout = dir.path().join("form.xml");
    std::fs::write(&layout, LAYOUT).unwrap();
    let device = format!("lease-test-{}", std::process::id());

    let mut a = server(&layout, &device).await;
    let mut b = server(&layout, &device).await;

    let held = a.hold_lease().await.expect("free device");
    assert!(held.is_some(), "leases are on: a guard comes back");

    // The holder's own calls don't wait for the lease it holds.
    let started = std::time::Instant::now();
    let shot = a.call("snapshot", json!({})).await.unwrap();
    assert_ne!(shot["isError"], true, "{shot}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));

    // Anyone else waits, then gets DEVICE_BUSY with a safe retry.
    let Err(busy) = b.hold_lease().await else {
        panic!("b got the lease a holds");
    };
    assert_eq!(busy["isError"], true, "{busy}");
    let error = &busy["structuredContent"]["error"];
    assert_eq!(error["code"], "DEVICE_BUSY", "{busy}");
    assert_eq!(error["retry"], "safe");
    assert_eq!(error["delivered"], "no");

    drop(held);
    assert!(b.hold_lease().await.expect("released").is_some());
}

#[tokio::test]
async fn without_leases_there_is_nothing_to_hold() {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("form.xml");
    std::fs::write(&layout, LAYOUT).unwrap();
    let mut s = server(&layout, "headless").await.with_leases(false);
    assert!(s.hold_lease().await.unwrap().is_none());
}
