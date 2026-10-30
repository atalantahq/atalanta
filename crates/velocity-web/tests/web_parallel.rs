//! W3: `--parallel N` on the web is N browser contexts in one Chrome
//! (`web.parallelMode: contexts`, the default), each with its own
//! cookies, storage and mocks, against a real headless Chrome.

mod common;

use common::serve;
use velocity_common::{MockRule, PlatformDriver};

fn driver() -> velocity_web::WebDriver {
    velocity_web::WebDriver::new(velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        instances: 2,
        ..Default::default()
    })
}

async fn text_of(d: &velocity_web::WebDriver, dev: &str, css: &str) -> String {
    let el = d.find_element(dev, &common::css(css)).await.unwrap();
    d.get_element_text(dev, &el).await.unwrap()
}

const SET: &str = "<html><body><div id=c></div><script>document.cookie='who=zero;max-age=600';\
     localStorage.setItem('who','zero');document.getElementById('c').textContent=document.cookie</script></body></html>";
const READ: &str = "<html><body><div id=c></div><script>document.getElementById('c').textContent=\
     'cookie='+document.cookie+';ls='+localStorage.getItem('who')</script></body></html>";
const API: &str = "<html><body><div id=r>…</div><script>fetch('/api.json').then(r=>r.text())\
     .then(t=>document.getElementById('r').textContent=t)</script></body></html>";

/// W3: two devices, one Chrome process, two contexts. Neither sees the
/// other's cookies or storage, they load side by side, a mock on one is
/// not on the other, `launchApp clearState` on one leaves the other's
/// cookies, and shutting one down leaves the other running.
#[tokio::test]
async fn parallel_devices_are_isolated_contexts_of_one_chrome() {
    require_chrome!();
    let base = serve(&[
        ("/set.html", SET),
        ("/read.html", READ),
        ("/api.html", API),
        ("/api.json", "real"),
        ("/slow.html", "<html><body>slow</body></html>"),
    ]);
    let d = driver();
    d.launch_app("web-0", &format!("{base}/set.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/read.html"), false)
        .await
        .unwrap();
    assert_eq!(d.debug_browsers().await, 2);
    assert_eq!(d.debug_browser_pids().await.len(), 1, "one Chrome");
    let (c0, c1) = (
        d.debug_browser_context("web-0").await.unwrap(),
        d.debug_browser_context("web-1").await.unwrap(),
    );
    assert_ne!(c0, c1);
    assert_eq!(text_of(&d, "web-1", "#c").await, "cookie=;ls=null");
    assert_eq!(text_of(&d, "web-0", "#c").await, "who=zero");

    // web-1 stores its own, then web-0 clears its state: web-1 keeps it.
    d.launch_app("web-1", &format!("{base}/set.html"), false)
        .await
        .unwrap();
    d.launch_app("web-0", &format!("{base}/read.html"), true)
        .await
        .unwrap();
    assert_eq!(text_of(&d, "web-0", "#c").await, "cookie=;ls=null");
    d.launch_app("web-1", &format!("{base}/read.html"), false)
        .await
        .unwrap();
    assert_eq!(text_of(&d, "web-1", "#c").await, "cookie=who=zero;ls=zero");

    // A mock on web-0 only.
    d.add_mock(
        "web-0",
        MockRule {
            url: "*/api.json".into(),
            body: Some("mocked".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    d.launch_app("web-0", &format!("{base}/api.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/api.html"), false)
        .await
        .unwrap();
    assert!(
        common::eventually(|| async { text_of(&d, "web-0", "#r").await == "mocked" }).await,
        "web-0: {}",
        text_of(&d, "web-0", "#r").await
    );
    assert!(
        common::eventually(|| async { text_of(&d, "web-1", "#r").await == "real" }).await,
        "web-1: {}",
        text_of(&d, "web-1", "#r").await
    );

    // Side by side: two 3 s loads take about 3 s, not 6.
    let start = std::time::Instant::now();
    let slow = format!("{base}/slow.html");
    let (a, b) = tokio::join!(d.navigate("web-0", &slow), d.navigate("web-1", &slow));
    a.unwrap();
    b.unwrap();
    assert!(start.elapsed() < std::time::Duration::from_millis(5_500));

    d.shutdown_device("web-1").await.unwrap();
    assert_eq!(d.debug_browsers().await, 1);
    assert!(d.current_url("web-0").await.is_some());
    d.current_url("web-1").await; // a fresh context
    assert_ne!(d.debug_browser_context("web-1").await.unwrap(), c1);
    let pids = d.debug_browser_pids().await;
    assert_eq!(pids.len(), 1);
    d.cleanup().await;
    assert_eq!(d.debug_browsers().await, 0);
    let pid = pids[0];
    assert!(
        common::eventually(|| async move {
            !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .await,
        "Chrome {pid} still runs after cleanup"
    );
}

/// W3: a hung page in one context is restarted as a new context in the
/// same Chrome; the other device's page is untouched.
#[tokio::test]
async fn restart_session_replaces_only_the_hung_context() {
    require_chrome!();
    let base = serve(&[
        ("/ok.html", "<html><body>ok</body></html>"),
        (
            "/hang.html",
            "<html><body>hang<script>setTimeout(function(){while(true){}},200)</script></body></html>",
        ),
    ]);
    let d = driver();
    d.launch_app("web-0", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    d.debug_eval_on("web-0", "window.mark = 42").await.unwrap();
    d.launch_app("web-1", &format!("{base}/hang.html"), false)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let (c0, c1) = (
        d.debug_browser_context("web-0").await,
        d.debug_browser_context("web-1").await,
    );
    let pid = d.debug_browser_pids().await;
    d.restart_session().await.unwrap();
    assert_eq!(d.debug_browser_context("web-0").await, c0);
    assert_ne!(d.debug_browser_context("web-1").await, c1);
    assert_eq!(d.debug_browser_pids().await, pid, "same Chrome");
    assert_eq!(d.debug_eval_on("web-0", "window.mark").await.unwrap(), 42);
    assert!(d.device_ready("web-1").await);
    d.cleanup().await;
}

/// W3: when the shared Chrome dies, a restart brings both devices back on
/// a new one.
#[tokio::test]
async fn a_dead_shared_chrome_is_replaced_on_restart() {
    require_chrome!();
    let base = serve(&[("/ok.html", "<html><body>ok</body></html>")]);
    let d = driver();
    d.launch_app("web-0", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    let old = d.debug_browser_pids().await[0];
    std::process::Command::new("kill")
        .args(["-9", &old.to_string()])
        .status()
        .unwrap();
    assert!(
        common::eventually(|| async { !d.device_ready("web-0").await }).await,
        "the dead Chrome still answers"
    );
    d.restart_session().await.unwrap();
    let new = d.debug_browser_pids().await;
    assert_eq!(new.len(), 1);
    assert_ne!(new[0], old);
    d.launch_app("web-0", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    d.cleanup().await;
}

/// review-i2 m2 and m3: the shared Chrome hangs (stopped, its socket
/// still open). A restart kills it and brings both devices back on a new
/// one, and a cleanup with a hung Chrome stays bounded. Before, every
/// context creation timed out against the hung Chrome and the restart
/// failed, test after test.
#[tokio::test]
async fn a_hung_shared_chrome_is_replaced_on_restart() {
    require_chrome!();
    let base = serve(&[("/ok.html", "<html><body>ok</body></html>")]);
    let d = driver();
    for dev in ["web-0", "web-1"] {
        d.launch_app(dev, &format!("{base}/ok.html"), false)
            .await
            .unwrap();
    }
    let old = d.debug_browser_pids().await[0];
    let signal = |sig: &str, pid: u32| {
        let _ = std::process::Command::new("kill")
            .args([sig, &pid.to_string()])
            .status();
    };
    signal("-STOP", old);
    let start = std::time::Instant::now();
    let restarted =
        tokio::time::timeout(std::time::Duration::from_secs(40), d.restart_session()).await;
    let took = start.elapsed();
    let ok = matches!(restarted, Ok(Ok(())));
    let new = d.debug_browser_pids().await;
    if !ok {
        signal("-KILL", old);
    }
    assert!(
        ok,
        "restart with a hung Chrome: {restarted:?} after {took:?}"
    );
    assert!(took < std::time::Duration::from_secs(25), "took {took:?}");
    assert_eq!(new.len(), 1);
    assert_ne!(new[0], old);
    for dev in ["web-0", "web-1"] {
        d.launch_app(dev, &format!("{base}/ok.html"), false)
            .await
            .unwrap();
    }
    // Now the new one hangs: the cleanup is still bounded.
    signal("-STOP", new[0]);
    let start = std::time::Instant::now();
    d.cleanup().await;
    let took = start.elapsed();
    signal("-KILL", new[0]);
    signal("-KILL", old);
    assert!(
        took < std::time::Duration::from_secs(15),
        "cleanup took {took:?}"
    );
}

/// review-fi2 F2: a healthy shared Chrome whose browser lock another
/// device holds for longer than the ping's 2 s (a slow context disposal
/// or page creation under load) is not taken for hung. Devices launching
/// and restarting side by side, as `--parallel 4` does, keep the one
/// Chrome, and the device already in it keeps working. Before, the ping
/// timed the wait for the lock: the Chrome was killed with every device
/// in it.
#[tokio::test]
async fn a_busy_shared_chrome_is_not_killed() {
    require_chrome!();
    let base = serve(&[("/ok.html", "<html><body><p id=ok>ok</p></body></html>")]);
    let d = std::sync::Arc::new(velocity_web::WebDriver::new(
        velocity_web::WebDriverConfig {
            chrome_path: common::chrome_path(),
            extra_args: vec!["--no-sandbox".into()],
            instances: 4,
            ..Default::default()
        },
    ));
    let url = format!("{base}/ok.html");
    d.launch_app("web-0", &url, false).await.unwrap();
    let pid = d.debug_browser_pids().await;
    assert_eq!(pid.len(), 1);
    let hold = {
        let d = d.clone();
        tokio::spawn(async move {
            d.debug_hold_shared_browser(std::time::Duration::from_millis(3500))
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // web-1..3 launch while the lock is held; web-1 also restarts.
    let launches = ["web-1", "web-2", "web-3"].map(|dev| {
        let (d, url) = (d.clone(), url.clone());
        tokio::spawn(async move {
            d.launch_app(dev, &url, false).await?;
            if dev == "web-1" {
                d.prepare(dev).await?;
                d.launch_app(dev, &url, false).await?;
            }
            Ok::<_, velocity_common::VelocityError>(())
        })
    });
    for l in launches {
        l.await.unwrap().unwrap();
    }
    hold.await.unwrap();
    assert_eq!(
        d.debug_browser_pids().await,
        pid,
        "the busy Chrome was replaced"
    );
    assert_eq!(text_of(&d, "web-0", "#ok").await, "ok");
    d.cleanup().await;
}
