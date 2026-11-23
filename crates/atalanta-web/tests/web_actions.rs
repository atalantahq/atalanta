// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Overnight-2: how the web driver acts (retargeting, the pointer's
//! arrival, the hit target at dispatch time, input types, touch emulation,
//! settles, captures), against a real headless Chrome and a local page
//! (see `common` for the fixtures).

mod common;

use std::time::{Duration, Instant};

use atalanta_common::{AtalantaError, PlatformDriver, Selector};
use common::{driver, find, serve, text, wait_text};

const DISABLED: &str = r#"<!doctype html><html><body>
<button id="save" disabled onclick="window.hit=1"><span>Save</span></button>
<label>Name <input id="name" disabled></label>
<button id="ok" onclick="document.getElementById('out').textContent='ok'"><span>Fine</span></button>
<div id="out">idle</div>
</body></html>"#;

/// WA-1 (NB-1): the enabled check runs on the control a matched node
/// stands for. A `text:` tap resolved to the `<span>` inside a disabled
/// button, which isn't `:disabled`, and passed without the button acting.
#[tokio::test]
async fn a_span_inside_a_disabled_button_is_not_tapped() {
    require_chrome!();
    let base = serve(&[("/d.html", DISABLED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/d.html"), false)
        .await
        .unwrap();
    let save = d
        .find_element("web", &Selector::Text("Save".into()))
        .await
        .unwrap();
    assert_eq!(save.element_type, "span");
    // Review M-4: the span reads as disabled, so the executor waits for
    // the button to enable instead of tapping at once.
    assert!(!save.enabled, "a span in a disabled button is disabled");
    let err = d.tap("web", &save).await.expect_err("a disabled button");
    assert!(err.to_string().contains("disabled"), "{err}");
    // Review T-1: the variant, not just the message.
    assert!(
        matches!(err, AtalantaError::NotActionable { .. }),
        "{err:?}"
    );
    assert_eq!(
        d.debug_eval_main("String(window.hit)").await.unwrap(),
        "undefined"
    );
    // Text in a <label> stands for its control.
    let name = d
        .find_element("web", &Selector::Text("Name".into()))
        .await
        .unwrap();
    let err = d
        .input_text("web", &name, "x")
        .await
        .expect_err("a disabled field");
    assert!(err.to_string().contains("disabled"), "{err}");
    // A span in an enabled button still taps the button.
    let fine = d
        .find_element("web", &Selector::Text("Fine".into()))
        .await
        .unwrap();
    assert!(fine.enabled);
    d.tap("web", &fine).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "ok").await, "ok");
    d.cleanup().await;
}

const POINTER: &str = r#"<!doctype html><html><body>
<button id="b" style="width:120px;height:40px">Go</button>
<div id="out">idle</div>
<script>
var over = false, buttons = -1, b = document.getElementById('b');
b.addEventListener('pointerover', function () { over = true; });
b.addEventListener('mousedown', function (e) { buttons = e.buttons; });
b.addEventListener('click', function () {
  document.getElementById('out').textContent = (over ? 'over' : 'no-over') + ':' + buttons;
});
</script></body></html>"#;

/// WA-2 (NB-5): the pointer arrives before it presses, and the press
/// reports the left button held. Without the move no `pointerover` fired
/// (`:hover` never applied), and `mousedown` saw `buttons == 0`.
#[tokio::test]
async fn the_pointer_arrives_before_it_presses() {
    require_chrome!();
    let base = serve(&[("/p.html", POINTER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let b = find(&d, "#b").await;
    d.tap("web", &b).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "over:1").await, "over:1");
    d.cleanup().await;
}

// The first pointer move over the page inserts a banner above the button,
// which pushes the button down: the press lands on the banner.
const SHIFT: &str = r#"<!doctype html><html><body style="margin:0">
<div id="top"></div>
<button id="b" style="display:block;width:200px;height:40px"
 onclick="document.getElementById('out').textContent='button'">Buy</button>
<div id="out" style="position:fixed;bottom:0">idle</div>
<script>
var shifted = false;
addEventListener('pointermove', function () {
  if (shifted) return; shifted = true;
  var ban = document.createElement('div');
  ban.id = 'ban'; ban.style.height = '120px'; ban.textContent = 'Sale!';
  ban.onclick = function () { window.bannerHit = 1; };
  document.getElementById('top').appendChild(ban);
});
</script></body></html>"#;

/// WA-2: a layout shift between the actionability check and the click is
/// caught in the page at dispatch time; the click on the banner is blocked
/// and the tap is tried again, landing on the button. It clicked the
/// banner and passed.
#[tokio::test]
async fn a_layout_shift_between_check_and_click_is_retried() {
    require_chrome!();
    let base = serve(&[("/s.html", SHIFT)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let b = find(&d, "#b").await;
    d.tap("web", &b).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "button").await, "button");
    assert_eq!(
        d.debug_eval_main("String(window.bannerHit)").await.unwrap(),
        "undefined",
        "the banner got the click"
    );
    d.cleanup().await;
}

const INPUTS: &str = r##"<!doctype html><html><body>
<input id="d" type="date">
<input id="c" type="color" value="#000000">
<input id="e" type="email" value="ab@c.d" style="width:30px;font-size:20px">
<input id="dt" type="datetime-local">
<div id="log"></div>
<script>['d', 'dt'].forEach(function (id) {
  document.getElementById(id).addEventListener('change', function (e) {
    document.getElementById('log').textContent = 'changed:' + e.target.value; }); });</script>
</body></html>"##;

/// WA-3 (NB-2, NB-7): a date input is set by value (typed text never
/// reached a date picker's `.value`), with `change` fired; a value in the
/// wrong format fails; and text typed into a narrow email field is
/// appended, not inserted where the click put the caret.
#[tokio::test]
async fn input_types_are_filled_the_way_they_take_values() {
    require_chrome!();
    let base = serve(&[("/i.html", INPUTS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/i.html"), false)
        .await
        .unwrap();
    let date = find(&d, "#d").await;
    d.input_text("web", &date, "2026-10-15").await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('d').value")
            .await
            .unwrap(),
        "2026-10-15"
    );
    assert_eq!(text(&d, "#log").await, "changed:2026-10-15");
    let err = d
        .input_text("web", &date, "15/10/2026")
        .await
        .expect_err("not yyyy-mm-dd");
    assert!(err.to_string().contains("yyyy-mm-dd"), "{err}");

    // Review M-3: Chrome normalises a datetime-local value; comparing it
    // with the text as given failed a valid value with no events fired.
    let dt = find(&d, "#dt").await;
    d.input_text("web", &dt, "2024-01-15T10:30:00")
        .await
        .unwrap();
    assert_eq!(text(&d, "#log").await, "changed:2024-01-15T10:30");
    let err = d
        .input_text("web", &dt, "tomorrow")
        .await
        .expect_err("not a datetime");
    assert!(err.to_string().contains("yyyy-mm-ddThh:mm"), "{err}");
    assert_eq!(
        d.debug_eval_main("document.getElementById('dt').value")
            .await
            .unwrap(),
        "2024-01-15T10:30",
        "a refused value leaves the old one"
    );

    let color = find(&d, "#c").await;
    // N-6: a colour name was set as black, and the step passed.
    let err = d
        .input_text("web", &color, "red")
        .await
        .expect_err("not #rrggbb");
    assert!(err.to_string().contains("#rrggbb"), "{err}");
    d.input_text("web", &color, "#ff0000").await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('c').value")
            .await
            .unwrap(),
        "#ff0000"
    );

    let email = find(&d, "#e").await;
    d.input_text("web", &email, "x").await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('e').value")
            .await
            .unwrap(),
        "ab@c.dx"
    );
    d.cleanup().await;
}

/// WA-5 (NB-4): a desktop page is not a touch device. chromiumoxide turned
/// touch emulation on for every page.
#[tokio::test]
async fn a_desktop_page_reports_no_touch() {
    require_chrome!();
    let base = serve(&[("/p.html", POINTER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    assert_eq!(
        d.debug_eval_main("navigator.maxTouchPoints + ':' + ('ontouchstart' in window)")
            .await
            .unwrap(),
        "0:false"
    );
    d.cleanup().await;
}

const INNER_LIST: &str = r#"<!doctype html><html><body>
<div style="height:3000px"></div>
<div id="inner2" style="height:200px;overflow:auto;border:1px solid"></div>
<div style="height:300px"></div>
<script>for (var i = 1; i <= 50; i++) { var r = document.createElement('div');
r.style.height = '30px'; r.textContent = 'Low ' + i; document.getElementById('inner2').appendChild(r); }</script>
</body></html>"#;

/// WA-6 (VEL-069): `reveal` scrolls a container below the fold into view
/// and reports its fresh bounds, which `scrollUntilVisible within:` now
/// aims at. The pre-scroll box (y ≈ 3000) was off screen and the wheel
/// there scrolled nothing.
#[tokio::test]
async fn reveal_brings_a_container_below_the_fold_on_screen() {
    require_chrome!();
    let base = serve(&[("/l.html", INNER_LIST)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/l.html"), false)
        .await
        .unwrap();
    let inner = find(&d, "#inner2").await;
    assert!(inner.bounds.y > 2000, "{:?}", inner.bounds);
    let shown = d.reveal("web", &inner).await.unwrap();
    assert_eq!(shown.platform_id, inner.platform_id);
    let (_, h) = d.screen_size("web").await.unwrap();
    assert!(
        shown.bounds.y >= 0 && shown.bounds.y + shown.bounds.height <= h,
        "{:?}",
        shown.bounds
    );
    // A stroke inside the revealed box scrolls the list.
    let b = shown.bounds;
    let (cx, cy) = (b.x + b.width / 2, b.y + b.height / 2);
    d.scroll_coords("web", (cx, cy + 60), (cx, cy - 60))
        .await
        .unwrap();
    let top = d
        .debug_eval_main("document.getElementById('inner2').scrollTop")
        .await
        .unwrap();
    assert!(top.as_f64().unwrap_or(0.0) > 0.0, "scrollTop {top}");
    d.cleanup().await;
}

const SHADOW: &str = r#"<!doctype html><html><body>
<x-tabs id="tabs"></x-tabs>
<script>
customElements.define('x-tabs', class extends HTMLElement {
  constructor() { super(); var r = this.attachShadow({ mode: 'open' });
    r.innerHTML = '<button id="b">Tab B</button><div id="panel">Panel A</div>';
    var p = r.getElementById('panel');
    r.getElementById('b').onclick = function () { setTimeout(function () { p.textContent = 'Panel B'; }, 300); };
  }
});
</script></body></html>"#;

/// WA-7: a render inside an open shadow root is a reaction. The settle's
/// observer watched only the document, so a web component's update was
/// never waited for.
#[tokio::test]
async fn settle_waits_for_a_render_inside_a_web_component() {
    require_chrome!();
    let base = serve(&[("/w.html", SHADOW)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/w.html"), false)
        .await
        .unwrap();
    let b = d
        .find_element("web", &Selector::Text("Tab B".into()))
        .await
        .unwrap();
    d.tap("web", &b).await.unwrap();
    let r = d
        .settle_on_device("web", 1500, 100, 3000)
        .await
        .expect("the web driver settles on the page");
    assert!(r.settled && r.reacted, "{r:?}");
    let panel = d
        .find_element("web", &Selector::Text("Panel B".into()))
        .await;
    assert!(panel.is_ok(), "settled before the shadow render");
    d.cleanup().await;
}

// #root churns on 12 consecutive frames when #hydrate is clicked (like a
// framework hydrating), then is still; #go swaps its child 60 ms later.
const CHURN: &str = r#"<!doctype html><html><body>
<button id="hydrate" onclick="churn(12)">Hydrate</button>
<button id="go" onclick="setTimeout(function(){document.getElementById('root').innerHTML='<p>Route B</p>'},60)">Go</button>
<div id="root"><p>Route A</p></div>
<script>
function churn(n) { if (n <= 0) return;
  var r = document.getElementById('root'); r.className = 'c' + n;
  requestAnimationFrame(function () { churn(n - 1); }); }
</script></body></html>"#;

/// P10 (flakiness NB-9): a node marked restless in one settle isn't
/// ignored in the next. The memory lived for the whole document, so a
/// route swap in a container that churned at load was never waited for.
#[tokio::test]
async fn a_container_that_churned_once_is_watched_again() {
    require_chrome!();
    let base = serve(&[("/c.html", CHURN)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    let hydrate = find(&d, "#hydrate").await;
    d.tap("web", &hydrate).await.unwrap();
    let r = d.settle_on_device("web", 300, 150, 3000).await.unwrap();
    assert!(r.settled, "{r:?}");
    let go = find(&d, "#go").await;
    d.tap("web", &go).await.unwrap();
    let r = d.settle_on_device("web", 1000, 100, 3000).await.unwrap();
    assert!(r.settled && r.reacted, "the swap was not seen: {r:?}");
    assert_eq!(text(&d, "#root").await, "Route B");
    d.cleanup().await;
}

const ANIMATED: &str = r#"<!doctype html><html><head><style>
@keyframes spin { from { transform: rotate(0deg) } to { transform: rotate(360deg) } }
#s { width:80px;height:80px;background:linear-gradient(90deg,red,blue);animation: spin 700ms linear infinite }
</style></head><body>
<div id="s"></div><input id="f" autofocus style="font-size:30px">
<script>document.getElementById('f').focus()</script>
</body></html>"#;

const TOAST: &str = r#"<!doctype html><html><head><style>
@keyframes fade { from { opacity: 1 } to { opacity: 0.2 } }
#toast { width:120px;height:40px;background:green;animation: fade 3s linear forwards }
</style></head><body>
<div id="toast">Saved</div>
<script>document.getElementById('toast').addEventListener('animationend', function (e) {
  e.target.remove(); });</script>
</body></html>"#;

/// Review M-12: the capture finished every finite animation, firing
/// `animationend`, so a toast dismissed by it was gone after an
/// `assertScreenshot`; the animation now resumes from where it was.
#[tokio::test]
async fn a_capture_for_comparison_does_not_end_animations() {
    require_chrome!();
    let base = serve(&[("/t.html", TOAST)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let a = d.screenshot_for_compare("web").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = d.screenshot_for_compare("web").await.unwrap();
    assert!(a == b, "two captures for comparison differ");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = d
        .debug_eval_main(
            "(document.getElementById('toast') ? 'there' : 'gone') + ':' + \
             document.getAnimations().map(function (a) { \
               return a.playState + '@' + (a.currentTime < 2500); }).join()",
        )
        .await
        .unwrap();
    assert_eq!(state, "there:running@true");
    d.cleanup().await;
}

/// VT-8: `assertScreenshot`'s capture holds the page still (animations
/// paused, caret hidden), so two captures 300 ms apart are identical; the
/// endless animation runs again afterwards. Plain captures differ.
#[tokio::test]
async fn captures_for_comparison_hold_animations_and_the_caret_still() {
    require_chrome!();
    let base = serve(&[("/a.html", ANIMATED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/a.html"), false)
        .await
        .unwrap();
    let a = d.screenshot_for_compare("web").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = d.screenshot_for_compare("web").await.unwrap();
    assert!(a == b, "two captures for comparison differ");
    assert_eq!(
        d.debug_eval_main(
            "document.getAnimations().length + ':' + document.getAnimations()[0].playState"
        )
        .await
        .unwrap(),
        "1:running"
    );
    let a = d.screenshot("web").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = d.screenshot("web").await.unwrap();
    assert!(a != b, "the fixture doesn't animate");
    d.cleanup().await;
}

// What a Flutter web app shows before semantics are on: the placeholder
// button; clicking it builds the semantics nodes.
const FLUTTER: &str = r#"<!doctype html><html><body>
<flt-semantics-placeholder role="button" aria-label="Enable accessibility"
 style="position:absolute;left:-1px;top:-1px;width:1px;height:1px"
 onclick="document.getElementById('host').innerHTML='<flt-semantics flt-semantics-identifier=&quot;login&quot; role=&quot;button&quot; style=&quot;display:block;width:80px;height:30px&quot;>Log in</flt-semantics>'"></flt-semantics-placeholder>
<flt-semantics-host id="host"></flt-semantics-host>
</body></html>"#;

/// UI-frameworks P6: on a Flutter web app, semantics are enabled after
/// the page loads, and `id:` matches `Semantics(identifier:)`, which
/// Flutter renders as `flt-semantics-identifier`.
#[tokio::test]
async fn flutter_web_semantics_are_enabled_and_ids_match() {
    require_chrome!();
    let base = serve(&[("/f.html", FLUTTER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/f.html"), false)
        .await
        .unwrap();
    let el = d
        .find_element("web", &Selector::Id("login".into()))
        .await
        .expect("the semantics node by its identifier");
    assert_eq!(el.text.as_deref(), Some("Log in"));
    d.cleanup().await;
}

// A real Flutter app adds the placeholder when its engine starts, after
// the load event.
const FLUTTER_LATE: &str = r#"<!doctype html><html><body>
<flt-semantics-host id="host"></flt-semantics-host>
<script>setTimeout(function () {
  var p = document.createElement('flt-semantics-placeholder');
  p.setAttribute('role', 'button');
  p.onclick = function () {
    document.getElementById('host').innerHTML = '<flt-semantics flt-semantics-identifier="login" ' +
      'role="button" style="display:block;width:80px;height:30px">Log in</flt-semantics>';
  };
  document.body.appendChild(p);
}, 700);</script>
</body></html>"#;

/// Review M-8: semantics were enabled once, right after `load`; a
/// placeholder that appeared later was never clicked.
#[tokio::test]
async fn flutter_semantics_are_enabled_when_the_placeholder_comes_late() {
    require_chrome!();
    let base = serve(&[("/f.html", FLUTTER_LATE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/f.html"), false)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let el = loop {
        match d.find_element("web", &Selector::Id("login".into())).await {
            Ok(el) => break el,
            Err(e) if Instant::now() > deadline => panic!("never enabled: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    assert_eq!(el.text.as_deref(), Some("Log in"));
    d.cleanup().await;
}

/// WA-10 (VEL-132): a cross-origin iframe's document isn't "in flight"
/// until networkIdleMaxMs: its end is reported on the frame's own target,
/// so every load with one waited ~5 s. With the frames attached (W4) that
/// end is seen there, so every load clears: no "one in a few" tolerance
/// any more (the old guard tried up to 3 loads and passed on any).
#[tokio::test]
async fn a_cross_origin_iframe_does_not_keep_the_network_busy() {
    require_chrome!();
    let base = serve(&[("/inner.html", POINTER)]);
    let port = base.rsplit(':').next().unwrap();
    let outer = format!(
        "<!doctype html><html><body><iframe src=\"http://localhost:{port}/inner.html\"></iframe></body></html>"
    );
    let base2 = serve(&[("/outer.html", outer.as_str())]);
    let d = driver(vec![]).await;
    // Before the fix every load stayed busy ~5 s. The residual flake (one
    // load in a few) was the browser's `/favicon.ico` fetch, whose end
    // never reached the page's session (C7); every load must clear now.
    for _ in 0..5 {
        d.launch_app("web", &format!("{base2}/outer.html"), false)
            .await
            .unwrap();
        let start = Instant::now();
        while d.network_busy("web").await && start.elapsed() < Duration::from_secs(6) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let waited = start.elapsed();
        assert!(waited < Duration::from_millis(1500), "busy for {waited:?}");
    }
    d.cleanup().await;
}

const TEXTAREA: &str = r#"<!doctype html><html><body><textarea id="t"></textarea></body></html>"#;

/// WA-4 (NB-3): Enter in a textarea adds one newline on macOS too: the
/// `insertNewline` command no longer rides along with the key's text.
#[tokio::test]
async fn enter_in_a_textarea_adds_one_newline() {
    require_chrome!();
    let base = serve(&[("/t.html", TEXTAREA)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let t = find(&d, "#t").await;
    d.input_text("web", &t, "a").await.unwrap();
    d.press_key("web", atalanta_common::Key::Enter)
        .await
        .unwrap();
    d.input_raw("web", "b").await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('t').value")
            .await
            .unwrap(),
        "a\nb"
    );
    d.cleanup().await;
}

const OVERLAY: &str = r#"<!doctype html><html><head><style>
.wrap { position: relative; display: inline-block; }
.wrap::before { content: ""; position: absolute; inset: 0; z-index: 1; }
</style></head><body>
<div class="wrap" onclick="window.hitWrap=(window.hitWrap||0)+1"><button id="go" onclick="window.hitGo=1">Go</button></div>
<div aria-disabled="true"><button id="off" onclick="window.hitOff=1">Off</button></div>
</body></html>"#;

/// WA-15 (D-3): under the strict rule a wrapper whose `::before` overlay
/// takes the click, and a control inside an `aria-disabled` subtree, fail
/// as NotActionable; the lenient rule taps them with a warning, as before;
/// `force: true` taps without asking.
#[tokio::test]
async fn ancestor_overlay_blocks_tap_and_force_taps_anyway() {
    use atalanta_common::actionability::{with_mode, ActMode};
    require_chrome!();
    let base = serve(&[("/o.html", OVERLAY)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/o.html"), false)
        .await
        .unwrap();
    let go = find(&d, "#go").await;
    let off = find(&d, "#off").await;
    let err = with_mode(ActMode::Strict, d.tap("web", &go))
        .await
        .expect_err("an overlay takes the click");
    assert!(
        matches!(err, AtalantaError::NotActionable { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("ancestor"), "{err}");
    let err = with_mode(ActMode::Strict, d.tap("web", &off))
        .await
        .expect_err("inside aria-disabled");
    assert!(err.to_string().contains("aria-disabled"), "{err}");
    assert_eq!(
        d.debug_eval_main("String(window.hitWrap) + String(window.hitOff)")
            .await
            .unwrap(),
        "undefinedundefined",
        "a refused tap clicks nothing"
    );
    // Lenient (and a driver called outside the executor): as before.
    with_mode(ActMode::Lenient, d.tap("web", &go))
        .await
        .unwrap();
    d.tap("web", &off).await.unwrap();
    // force: true.
    with_mode(ActMode::Force, d.tap("web", &off)).await.unwrap();
    with_mode(ActMode::Force, d.tap("web", &go)).await.unwrap();
    assert_eq!(
        d.debug_eval_main("String(window.hitWrap) + String(window.hitOff)")
            .await
            .unwrap(),
        "21"
    );
    d.cleanup().await;
}

const HOLDER: &str = r#"<!doctype html><html><head><style>
.np { pointer-events: none; }
</style></head><body>
<button id="pay" onclick="window.hitPay=(window.hitPay||0)+1"><span id="payl" class="np">Pay</span></button>
<input id="agree" type="checkbox"><label for="agree"><span id="agreel" class="np">Agree</span></label>
<div id="plain" onclick="window.hitPlain=1"><span id="plainl" class="np">Plain</span></div>
</body></html>"#;

/// Review M3: a label or icon with `pointer-events: none` inside a button
/// (Tailwind's `pointer-events-none`) sends the click to the button. Strict
/// took that for an overlay; Playwright retargets the hit check to the
/// enclosing control. A plain ancestor that is no control still fails.
#[tokio::test]
async fn strict_accepts_a_click_on_the_control_holding_the_target() {
    use atalanta_common::actionability::{with_mode, ActMode};
    require_chrome!();
    let base = serve(&[("/h.html", HOLDER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/h.html"), false)
        .await
        .unwrap();
    let pay = find(&d, "#payl").await;
    with_mode(ActMode::Strict, d.tap("web", &pay))
        .await
        .expect("the button takes the click");
    let agree = find(&d, "#agreel").await;
    with_mode(ActMode::Strict, d.tap("web", &agree))
        .await
        .expect("the label takes the click");
    assert_eq!(
        d.debug_eval_main(
            "String(window.hitPay) + String(document.getElementById('agree').checked)"
        )
        .await
        .unwrap(),
        "1true"
    );
    let plain = find(&d, "#plainl").await;
    let err = with_mode(ActMode::Strict, d.tap("web", &plain))
        .await
        .expect_err("a plain ancestor is no control");
    assert!(err.to_string().contains("ancestor"), "{err}");
    d.cleanup().await;
}
