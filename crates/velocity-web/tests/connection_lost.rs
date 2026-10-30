//! VEL-020: when Chrome dies mid-run, every driver call fails at once as a
//! lost connection, and cleanup returns. Calls hung until the step watchdog
//! and cleanup hung forever. Its own test binary: it kills this process's
//! Chrome by its profile dir, which would hit the other tests' browsers.

mod common;

use std::time::{Duration, Instant};

use common::{css, driver, serve};
use velocity_common::{HealthStatus, PlatformDriver, VelocityError};

#[tokio::test]
async fn chrome_killed_calls_return_within_deadline() {
    require_chrome!();
    // Minimal containers often lack procps.
    if std::process::Command::new("pkill")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("skipped: no pkill");
        return;
    }
    let base = serve(&[("/p.html", "<html><body><div id=a>a</div></body></html>")]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    d.find_element("web", &css("#a")).await.unwrap();

    // Only Chrome processes started with this process's profile dirs.
    let mine = format!("velocity-chrome-{}-", std::process::id());
    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", &mine])
        .status()
        .unwrap();
    assert!(killed.success(), "Chrome was running with {mine}");

    let start = Instant::now();
    let err = loop {
        match d.find_element("web", &css("#a")).await {
            Err(e) => break e,
            Ok(_) if start.elapsed() < Duration::from_secs(5) => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Ok(_) => panic!("Chrome still answers after being killed"),
        }
    };
    assert!(
        matches!(err, VelocityError::BrowserConnectionLost { .. }),
        "{err}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(12),
        "{:?}",
        start.elapsed()
    );

    let start = Instant::now();
    let err = d.tap_xy("web", 1, 1).await.unwrap_err();
    assert!(
        matches!(err, VelocityError::BrowserConnectionLost { .. }),
        "{err}"
    );
    // Navigation steps too: they swallowed the error and passed.
    for (what, r) in [
        ("go_back", d.go_back("web").await),
        ("refresh_page", d.refresh_page("web").await),
        (
            "navigate",
            d.navigate("web", &format!("{base}/p.html")).await,
        ),
    ] {
        assert!(
            matches!(r, Err(VelocityError::BrowserConnectionLost { .. })),
            "{what}: {r:?}"
        );
    }
    assert!(matches!(d.health_check().await, HealthStatus::Unhealthy));
    d.cleanup().await;
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "later calls and cleanup are quick: {:?}",
        start.elapsed()
    );
}
