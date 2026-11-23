//! VEL-020: when Chrome dies mid-run, every driver call fails at once as a
//! lost connection, and cleanup returns. Calls hung until the step watchdog
//! and cleanup hung forever. Its own test binary: it kills a Chrome.

mod common;

use std::time::{Duration, Instant};

use atalanta_common::{AtalantaError, HealthStatus, PlatformDriver};
use common::{css, driver, serve};

#[tokio::test]
async fn chrome_killed_calls_return_within_deadline() {
    require_chrome!();
    let base = serve(&[("/p.html", "<html><body><div id=a>a</div></body></html>")]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    d.find_element("web", &css("#a")).await.unwrap();

    // The browser process itself, by its pid: no `pkill` needed, which
    // minimal containers lack and whose absence skipped the test (review
    // 07 M6).
    let pid = d.debug_browser_pid("web").await.expect("Chrome's pid");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "Chrome {pid} was running");

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
        matches!(err, AtalantaError::BrowserConnectionLost { .. }),
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
        matches!(err, AtalantaError::BrowserConnectionLost { .. }),
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
            matches!(r, Err(AtalantaError::BrowserConnectionLost { .. })),
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
