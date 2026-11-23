//! W2: swipes on a touch profile (`web.device`) are finger strokes
//! (`Input.dispatchTouchEvent` moves), against a real headless Chrome.

mod common;

use common::serve;
use velocity_common::{Direction, PlatformDriver};

/// A tall page that logs its touch events, with a touch-driven carousel
/// (`#track` follows the finger, as Swiper's does, and scrolls nothing).
const PAGE: &str = r#"<!doctype html><html><head><meta name="viewport" content="width=device-width"></head>
<body style="margin:0">
<div id="car" style="height:120px;background:#eee;overflow:hidden;touch-action:pan-y">
  <div id="track" style="width:300%;height:100%;background:linear-gradient(90deg,red,blue)"></div>
</div>
<div style="height:4000px">tall</div>
<script>
window.moves = 0; window.starts = 0; window.ends = 0; window.dx = 0;
document.addEventListener('touchstart', () => window.starts++, {passive:true});
document.addEventListener('touchmove', () => window.moves++, {passive:true});
document.addEventListener('touchend', () => window.ends++, {passive:true});
var car = document.getElementById('car'), x0 = 0;
car.addEventListener('touchstart', e => { x0 = e.touches[0].clientX; }, {passive:true});
car.addEventListener('touchmove', e => {
  window.dx = e.touches[0].clientX - x0;
  document.getElementById('track').style.transform = 'translateX(' + window.dx + 'px)';
}, {passive:true});
</script>
</body></html>"#;

async fn touch_driver() -> velocity_web::WebDriver {
    let mut config = velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    };
    config.apply_device(velocity_common::web_devices::lookup("Pixel 7").unwrap());
    let d = velocity_web::WebDriver::new(config);
    d.prepare("web").await.unwrap();
    d
}

async fn num(d: &velocity_web::WebDriver, js: &str) -> f64 {
    d.debug_eval_main(js)
        .await
        .unwrap()
        .as_f64()
        .unwrap_or(-1.0)
}

/// W2: `swipe: up` on a phone profile is a finger moving up the screen:
/// touchstart, a stream of touchmoves, touchend, and the page scrolls
/// down under it. It was the mouse wheel, which fired no touch event.
#[tokio::test]
async fn a_swipe_on_a_touch_profile_is_a_finger_stroke() {
    require_chrome!();
    let base = serve(&[("/t.html", PAGE)]);
    let d = touch_driver().await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    d.swipe("web", Direction::Up).await.unwrap();
    assert!(
        common::eventually(|| async { num(&d, "window.scrollY").await > 100.0 }).await,
        "the page did not scroll: scrollY={}",
        num(&d, "window.scrollY").await
    );
    assert_eq!(num(&d, "window.starts").await, 1.0);
    assert!(num(&d, "window.moves").await >= 5.0);
    assert_eq!(num(&d, "window.ends").await, 1.0);
    // The finger comes to rest before it lifts: no fling past the stroke
    // (40% of 839 px, give or take Chrome's slop).
    let y = num(&d, "window.scrollY").await;
    assert!(y < 450.0, "flung to {y}");
    d.cleanup().await;
}

/// W2: `swipe from/to` across a touch carousel drags it with the finger
/// (each `touchmove` reports where the finger is); a mouse drag or the
/// wheel never reached its touch handlers.
#[tokio::test]
async fn a_coordinate_swipe_drags_a_touch_carousel() {
    require_chrome!();
    let base = serve(&[("/t.html", PAGE)]);
    let d = touch_driver().await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    d.swipe_coords("web", (300, 60), (100, 60)).await.unwrap();
    let dx = num(&d, "window.dx").await;
    assert!((dx - -200.0).abs() < 1.0, "the carousel moved {dx}px");
    assert!(num(&d, "window.moves").await >= 5.0);
    d.cleanup().await;
}
