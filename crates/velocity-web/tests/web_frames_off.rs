//! review-i2 m4: with `web.frames: off` (no frames connection) a
//! cross-origin iframe's document must not keep the network busy: the
//! VEL-132 heuristic covers it. Its own test binary: it sets the
//! process environment.

mod common;

use std::time::{Duration, Instant};

use common::{css, driver, serve};
use velocity_common::PlatformDriver;

#[tokio::test]
async fn frames_off_a_cross_origin_iframe_does_not_keep_the_network_busy() {
    require_chrome!();
    std::env::set_var("VELOCITY_WEB_FRAMES", "off");
    let inner = serve(&[(
        "/inner.html",
        "<!doctype html><html><body><button id=\"in\">In</button></body></html>",
    )]);
    let port = inner.rsplit(':').next().unwrap();
    let outer = format!(
        "<!doctype html><html><body><iframe src=\"http://localhost:{port}/inner.html\"></iframe></body></html>"
    );
    let top = serve(&[("/outer.html", outer.as_str())]);
    let d = driver(vec![]).await;
    for _ in 0..3 {
        d.launch_app("web", &format!("{top}/outer.html"), false)
            .await
            .unwrap();
        let start = Instant::now();
        while d.network_busy("web").await && start.elapsed() < Duration::from_secs(6) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let waited = start.elapsed();
        assert!(waited < Duration::from_millis(1500), "busy for {waited:?}");
    }
    // Off: the frame's elements aren't looked into.
    assert!(d.find_element("web", &css("#in")).await.is_err());
    d.cleanup().await;
}
