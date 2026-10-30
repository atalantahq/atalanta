//! Decisions round: `<select>` (D-14), device profiles (D-W3), swipe
//! direction (D-W4) and parallel browsers (D-48), against a real headless
//! Chrome and a local page (see `common` for the fixtures).

mod common;

use common::{driver, find, serve, text};
use velocity_common::{PlatformDriver, VelocityError};

const SELECT: &str = r#"<!doctype html><html><body>
<label for="country">Country</label>
<select id="country"
  oninput="document.getElementById('log').textContent+='input:'+this.value+','"
  onchange="document.getElementById('log').textContent+='change:'+this.value+','">
  <option value="">Choose…</option>
  <option value="us">United   States</option>
  <option value="ca">Canada</option>
  <option value="mx" disabled>Mexico</option>
  <optgroup label="Old" disabled><option value="su">Soviet Union</option></optgroup>
</select>
<select id="off" disabled><option value="a">A</option><option value="b">B</option></select>
<div id="log"></div>
</body></html>"#;

async fn value(d: &velocity_web::WebDriver, id: &str) -> String {
    d.debug_eval_main(&format!("document.getElementById('{id}').value"))
        .await
        .unwrap()
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// D-14 (VEL-133): `inputText` on a `<select>` chooses the option whose
/// value or normalised label is the text, firing `input` and `change`;
/// typing into it did nothing and the step passed. Through its `<label>`
/// too.
#[tokio::test]
async fn input_text_chooses_a_select_option() {
    require_chrome!();
    let base = serve(&[("/s.html", SELECT)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let select = find(&d, "#country").await;
    d.input_text("web", &select, " United States ")
        .await
        .unwrap();
    assert_eq!(value(&d, "country").await, "us", "by label, normalised");
    assert_eq!(text(&d, "#log").await, "input:us,change:us,");

    d.input_text("web", &select, "ca").await.unwrap();
    assert_eq!(value(&d, "country").await, "ca", "by value");

    let label = find(&d, "label").await;
    d.input_text("web", &label, "United States").await.unwrap();
    assert_eq!(value(&d, "country").await, "us", "through the label");
    d.cleanup().await;
}

/// D-14: a disabled option (or one in a disabled `<optgroup>`), a
/// disabled `<select>` and a text no option has are errors, and the value
/// stays; the error lists the options.
#[tokio::test]
async fn a_disabled_or_unknown_option_is_refused() {
    require_chrome!();
    let base = serve(&[("/s.html", SELECT)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let select = find(&d, "#country").await;
    for disabled in ["Mexico", "mx", "Soviet Union"] {
        let e = d.input_text("web", &select, disabled).await.unwrap_err();
        assert!(
            matches!(e, VelocityError::NotActionable { .. }),
            "{disabled}: {e}"
        );
        assert!(e.to_string().contains("disabled"), "{e}");
    }
    let e = d.input_text("web", &select, "France").await.unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("France") && msg.contains("\"Canada\" (value \"ca\")"),
        "{msg}"
    );
    assert_eq!(value(&d, "country").await, "", "nothing chosen");
    assert_eq!(text(&d, "#log").await, "", "no events");

    let off = find(&d, "#off").await;
    assert!(d.input_text("web", &off, "B").await.is_err());
    assert_eq!(value(&d, "off").await, "a");
    d.cleanup().await;
}

/// Whether process `pid` is still running.
fn running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// D-48 (VEL-332): `instances: 2` (`--parallel 2`) is two Chrome
/// processes, the devices `web-0` and `web-1`: each starts on first use
/// (the parallel runner never prepares a leased device), has its own
/// profile (cookies and storage don't leak between them), runs at the same
/// time as the other, and `cleanup` kills both. `--parallel N` silently
/// ran everything on one browser.
#[tokio::test]
async fn parallel_devices_are_independent_browsers() {
    require_chrome!();
    let base = serve(&[
        (
            "/set.html",
            "<html><body><div id=c></div><script>document.cookie='who=zero';\
             localStorage.setItem('who','zero');document.getElementById('c').textContent=document.cookie</script></body></html>",
        ),
        (
            "/read.html",
            "<html><body><div id=c></div><script>document.getElementById('c').textContent=\
             'cookie='+document.cookie+';ls='+localStorage.getItem('who')</script></body></html>",
        ),
        ("/slow.html", "<html><body>slow</body></html>"),
    ]);
    let d = velocity_web::WebDriver::new(velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        instances: 2,
        parallel_mode: velocity_web::ParallelMode::Processes,
        ..Default::default()
    });
    let ids: Vec<String> = d
        .list_devices()
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.id)
        .collect();
    assert_eq!(ids, ["web-0", "web-1"]);
    assert!(d.device_ready("web-1").await, "ready before it runs");
    assert!(!d.device_ready("web-7").await, "not one of its devices");

    // No prepare: both start on first use.
    d.launch_app("web-0", &format!("{base}/set.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/read.html"), false)
        .await
        .unwrap();
    assert_eq!(d.debug_browsers().await, 2);
    let pids = d.debug_browser_pids().await;
    assert_eq!(pids.len(), 2);
    assert_ne!(pids[0], pids[1], "two Chrome processes");
    let read = d.find_element("web-1", &common::css("#c")).await.unwrap();
    assert_eq!(
        d.get_element_text("web-1", &read).await.unwrap(),
        "cookie=;ls=null",
        "web-1 sees nothing web-0 stored"
    );
    // web-0 is still on its own page.
    let set = d.find_element("web-0", &common::css("#c")).await.unwrap();
    assert_eq!(d.get_element_text("web-0", &set).await.unwrap(), "who=zero");

    // Side by side: two 3 s loads take about 3 s, not 6.
    let start = std::time::Instant::now();
    let slow = format!("{base}/slow.html");
    let (a, b) = tokio::join!(d.navigate("web-0", &slow), d.navigate("web-1", &slow));
    a.unwrap();
    b.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_millis(5_500),
        "{:?}",
        start.elapsed()
    );

    // One device's shutdown leaves the other running.
    d.shutdown_device("web-1").await.unwrap();
    assert_eq!(d.debug_browsers().await, 1);
    assert!(d.current_url("web-0").await.is_some());
    d.current_url("web-1").await; // starts it again
    assert_eq!(d.debug_browsers().await, 2);
    let pids = d.debug_browser_pids().await;
    d.cleanup().await;
    assert_eq!(d.debug_browsers().await, 0);
    for pid in pids {
        assert!(
            common::eventually(|| async move { !running(pid) }).await,
            "Chrome {pid} still runs after cleanup"
        );
    }
}

async fn scroll(d: &velocity_web::WebDriver, axis: &str) -> f64 {
    d.debug_eval_main(&format!("window.{axis}"))
        .await
        .unwrap()
        .as_f64()
        .unwrap_or_default()
}

/// The scroll position once `done` holds, or after 3 s. A swipe's wheel
/// event is dispatched, not awaited, and a loaded runner's Chrome applies it
/// a moment later (a run settles after a swipe; this test doesn't).
async fn scroll_until(d: &velocity_web::WebDriver, axis: &str, done: impl Fn(f64) -> bool) -> f64 {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let v = scroll(d, axis).await;
        if done(v) || std::time::Instant::now() >= until {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// D-W4 (VEL-265): `swipe: up` is the finger moving up, as on devices:
/// the page scrolls down and shows what is below. It scrolled the page up
/// (at the top: not at all).
#[tokio::test]
async fn swipe_up_reveals_content_below() {
    require_chrome!();
    let base = serve(&[(
        "/tall.html",
        "<html><body style='margin:0'><div style='height:4000px;width:4000px'>top</div></body></html>",
    )]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/tall.html"), false)
        .await
        .unwrap();
    d.swipe("web", velocity_common::Direction::Up)
        .await
        .unwrap();
    let after_up = scroll_until(&d, "scrollY", |y| y > 0.0).await;
    assert!(
        after_up > 0.0,
        "finger up scrolls the page down: {after_up}"
    );
    d.swipe("web", velocity_common::Direction::Down)
        .await
        .unwrap();
    assert!(
        scroll_until(&d, "scrollY", |y| y < after_up).await < after_up,
        "finger down scrolls back up"
    );
    d.swipe("web", velocity_common::Direction::Left)
        .await
        .unwrap();
    let x = scroll_until(&d, "scrollX", |x| x > 0.0).await;
    assert!(x > 0.0, "finger left shows what's right: {x}");
    d.cleanup().await;
}

const TOUCH: &str = r#"<!doctype html><html><head><meta name="viewport" content="width=device-width"></head><body>
<button id="b" style="width:200px;height:80px"
  ontouchstart="document.getElementById('log').textContent+='touchstart,'"
  ontouchend="document.getElementById('log').textContent+='touchend,'"
  onclick="document.getElementById('log').textContent+='click,'">Tap</button>
<div id="log"></div>
</body></html>"#;

/// D-W3: `web.device: Pixel 7` is the phone's viewport, pixel ratio,
/// user agent, mobile layout and touch, and a tap is a touch (touchstart,
/// touchend, then the click), not a mouse click.
#[tokio::test]
async fn a_device_profile_is_mobile_and_taps_are_touches() {
    require_chrome!();
    let base = serve(&[("/t.html", TOUCH)]);
    let mut config = velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    };
    config.apply_device(velocity_common::web_devices::lookup("Pixel 7").unwrap());
    let d = velocity_web::WebDriver::new(config);
    d.prepare("web").await.unwrap();
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let probe = d
        .debug_eval_main(
            "[innerWidth, devicePixelRatio, navigator.userAgent.includes('Pixel 7'), \
             matchMedia('(pointer: coarse)').matches, navigator.maxTouchPoints > 0].join('|')",
        )
        .await
        .unwrap();
    assert_eq!(probe, "412|2.625|true|true|true");
    assert_eq!(d.screen_size("web").await.unwrap(), (412, 839));
    let b = find(&d, "#b").await;
    d.tap("web", &b).await.unwrap();
    assert_eq!(
        common::wait_text(&d, "#log", "touchstart,touchend,click,").await,
        "touchstart,touchend,click,"
    );
    d.cleanup().await;
}

/// D-W3: without a profile a page is a desktop one: no touch, and a tap
/// is a mouse click (NB-4 stays fixed).
#[tokio::test]
async fn without_a_profile_taps_are_mouse_clicks() {
    require_chrome!();
    let base = serve(&[("/t.html", TOUCH)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let b = find(&d, "#b").await;
    d.tap("web", &b).await.unwrap();
    assert_eq!(common::wait_text(&d, "#log", "click,").await, "click,");
    assert_eq!(
        d.debug_eval_main("[devicePixelRatio, navigator.maxTouchPoints].join('|')")
            .await
            .unwrap(),
        "1|0"
    );
    d.cleanup().await;
}

/// review-d6 m2: with several browsers and none whose socket closed,
/// `restart_session` restarted nothing, so a hung renderer (an endless JS
/// loop keeps the socket open) stayed hung. It pings each browser and
/// restarts only the one that doesn't answer.
#[tokio::test]
async fn restart_session_restarts_only_the_hung_browser() {
    require_chrome!();
    let base = serve(&[
        ("/ok.html", "<html><body>ok</body></html>"),
        (
            "/hang.html",
            "<html><body>hang<script>setTimeout(function(){while(true){}},200)</script></body></html>",
        ),
    ]);
    let d = velocity_web::WebDriver::new(velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        instances: 2,
        parallel_mode: velocity_web::ParallelMode::Processes,
        ..Default::default()
    });
    d.launch_app("web-0", &format!("{base}/ok.html"), false)
        .await
        .unwrap();
    d.launch_app("web-1", &format!("{base}/hang.html"), false)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let healthy = d.debug_browser_pid("web-0").await.unwrap();
    let hung = d.debug_browser_pid("web-1").await.unwrap();
    d.restart_session().await.unwrap();
    assert_eq!(d.debug_browser_pid("web-0").await, Some(healthy));
    let fresh = d.debug_browser_pid("web-1").await.unwrap();
    assert_ne!(fresh, hung, "the hung browser was not restarted");
    assert!(d.device_ready("web-1").await);
    d.cleanup().await;
}
