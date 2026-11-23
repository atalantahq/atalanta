//! The web driver against a real headless Chrome and a local page.

mod common;

use std::time::{Duration, Instant};

use atalanta_common::{AtalantaError, Key, MockRule, PlatformDriver, Selector};
use common::{css, driver, eventually, find, serve, text, wait_text};

const FORM: &str = r#"<!doctype html><html><body>
<form id="f" onsubmit="event.preventDefault();document.getElementById('out').textContent='submitted:'+document.getElementById('q').value">
  <input id="q" value="" oninput="document.getElementById('val').textContent=this.value">
</form>
<div id="out">idle</div><div id="val"></div>
</body></html>"#;

/// VEL-021: Enter submits the focused form and Backspace deletes a
/// character. A bare `key` reached the page as `keyCode 0`, so neither did
/// anything while the step passed. (D-9: Backspace, not Back.)
#[tokio::test]
async fn press_key_enter_submits_and_backspace_deletes() {
    require_chrome!();
    let base = serve(&[("/form.html", FORM)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/form.html"), false)
        .await
        .unwrap();
    let q = find(&d, "#q").await;
    d.input_text("web", &q, "abc").await.unwrap();
    assert_eq!(wait_text(&d, "#val", "abc").await, "abc");

    d.press_key("web", Key::Backspace).await.unwrap();
    assert_eq!(wait_text(&d, "#val", "ab").await, "ab", "Backspace deletes");

    d.press_key("web", Key::Enter).await.unwrap();
    assert_eq!(
        wait_text(&d, "#out", "submitted:ab").await,
        "submitted:ab",
        "Enter submits"
    );
    d.cleanup().await;
}

/// D-9 (VEL-270): `pressKey: back` is the browser's back button. It sent
/// Backspace, so the page never changed and a focused field lost a
/// character.
#[tokio::test]
async fn press_key_back_goes_back_in_history() {
    require_chrome!();
    let base = serve(&[
        ("/one.html", "<html><body><h1 id=t>one</h1></body></html>"),
        (
            "/two.html",
            "<html><body><h1 id=t>two</h1><input id=q value=abc autofocus></body></html>",
        ),
    ]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/one.html"), false)
        .await
        .unwrap();
    d.navigate("web", &format!("{base}/two.html"))
        .await
        .unwrap();
    let q = find(&d, "#q").await;
    d.tap("web", &q).await.unwrap();
    d.press_key("web", Key::Back).await.unwrap();
    assert_eq!(text(&d, "#t").await, "one", "back left the page");
    assert!(d
        .current_url("web")
        .await
        .unwrap_or_default()
        .ends_with("/one.html"));
    d.cleanup().await;
}

/// D-9: tab, escape, delete and the arrows reach the page as real keys
/// (`key` and `code` set), and a volume key is a config error on the web
/// rather than a silent pass.
#[tokio::test]
async fn editing_and_arrow_keys_reach_the_page() {
    require_chrome!();
    let base = serve(&[(
        "/keys.html",
        "<html><body><input id=q autofocus><div id=log></div><script>\
         document.addEventListener('keydown',function(e){var l=document.getElementById('log');\
         l.textContent+=(l.textContent?',':'')+e.key+'/'+e.code;},true)</script></body></html>",
    )]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/keys.html"), false)
        .await
        .unwrap();
    let q = find(&d, "#q").await;
    d.tap("web", &q).await.unwrap();
    for key in [
        Key::Escape,
        Key::Delete,
        Key::ArrowUp,
        Key::ArrowDown,
        Key::ArrowLeft,
        Key::ArrowRight,
        Key::Tab,
    ] {
        d.press_key("web", key).await.unwrap();
    }
    let want = "Escape/Escape,Delete/Delete,ArrowUp/ArrowUp,ArrowDown/ArrowDown,\
                ArrowLeft/ArrowLeft,ArrowRight/ArrowRight,Tab/Tab";
    assert_eq!(wait_text(&d, "#log", want).await, want);
    let e = d.press_key("web", Key::VolumeUp).await.unwrap_err();
    assert!(matches!(e, AtalantaError::Config(_)), "{e}");
    d.cleanup().await;
}

const PAGE: &str = r#"<!doctype html><html><body>
<div id="a" class="md:block card">first</div>
<p id='q"x'>He said "it's" \ ok</p>
<div id="long"></div>
<input id="pre" style="width:80px" value="hello there friend" oninput="document.getElementById('preval').textContent=this.value">
<div id="preval">hello there friend</div>
<fieldset disabled><button id="off" onclick="document.getElementById('clicked').textContent='yes'">Off</button></fieldset>
<div id="clicked">no</div>
<button id="clone" onclick="var a=document.getElementById('a');var c=a.cloneNode(true);a.remove();document.body.appendChild(c)">clone</button>
<button id="grow" onclick="document.getElementById('a').textContent='second'">grow</button>
<script>document.getElementById('long').textContent='x'.repeat(500)+'END';</script>
</body></html>"#;

/// VEL-338: a selector the browser can't parse is an error, not "nothing
/// matched", which made `assertNotVisible` with a typo pass. The same for
/// XPath and for `find_elements`.
#[tokio::test]
async fn invalid_css_selector_is_an_error() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    for bad in [
        css("div["),
        css("a:::b"),
        css("##"),
        Selector::XPath("//div[".into()),
    ] {
        let err = d.find_element("web", &bad).await.unwrap_err();
        assert!(
            matches!(err, AtalantaError::InvalidSelector { .. }),
            "{bad:?}: {err}"
        );
    }
    let err = d.find_elements("web", &css("div[")).await.unwrap_err();
    assert!(
        matches!(err, AtalantaError::InvalidSelector { .. }),
        "{err}"
    );
    // A valid selector that matches nothing is still "not found".
    let err = d.find_element("web", &css("#nope")).await.unwrap_err();
    assert!(
        matches!(err, AtalantaError::ElementNotFound { .. }),
        "{err}"
    );
    d.cleanup().await;
}

/// VEL-119/272: selector values with quotes, backslashes and CSS
/// metacharacters reach the page as data, not as code.
#[tokio::test]
async fn selector_escaping() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let el = d
        .find_element("web", &Selector::Text(r#"He said "it's" \ ok"#.into()))
        .await
        .unwrap();
    assert_eq!(el.text.as_deref(), Some(r#"He said "it's" \ ok"#));
    d.find_element("web", &Selector::Id(r#"q"x"#.into()))
        .await
        .unwrap();
    // A Tailwind class with a colon.
    let el = d
        .find_element("web", &Selector::ClassName("md:block".into()))
        .await
        .unwrap();
    assert_eq!(el.text.as_deref(), Some("first"));
    d.cleanup().await;
}

/// VEL-118/057: an element's identity doesn't travel with `cloneNode`: once
/// the original is gone it is gone, even though an exact copy is on the
/// page. The `data-vel-id` attribute was copied, so the copy answered for it.
#[tokio::test]
async fn cloned_node_identity() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let a = find(&d, "#a").await;
    assert!(d.is_element_visible("web", &a).await.unwrap());
    let b = find(&d, "#clone").await;
    d.tap("web", &b).await.unwrap();
    assert!(
        eventually(|| async { !d.is_element_visible("web", &a).await.unwrap() }).await,
        "the original left the page"
    );
    let copy = find(&d, "#a").await;
    assert_ne!(copy.platform_id, a.platform_id, "the copy is a new element");
    d.cleanup().await;
}

/// VEL-124: text is read in full and live, not the 200-character capture.
#[tokio::test]
async fn element_text_is_full_and_live() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let long = text(&d, "#long").await;
    assert_eq!(long.len(), 503);
    assert!(long.ends_with("END"));

    let a = find(&d, "#a").await;
    let grow = find(&d, "#grow").await;
    d.tap("web", &grow).await.unwrap();
    assert!(
        eventually(|| async { d.get_element_text("web", &a).await.unwrap() == "second" }).await
    );
    d.cleanup().await;
}

/// VEL-131: `inputText` into a prefilled field appends; the click at the
/// field's centre put the caret mid-value.
#[tokio::test]
async fn input_text_appends_to_a_prefilled_field() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let pre = find(&d, "#pre").await;
    d.input_text("web", &pre, "!").await.unwrap();
    let want = "hello there friend!";
    assert_eq!(wait_text(&d, "#preval", want).await, want);
    d.cleanup().await;
}

/// VEL-134: a button in a `<fieldset disabled>` is disabled: tapping it
/// fails instead of passing without effect.
#[tokio::test]
async fn a_button_in_a_disabled_fieldset_is_not_tapped() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let off = find(&d, "#off").await;
    assert!(d.tap("web", &off).await.is_err());
    assert_eq!(text(&d, "#clicked").await, "no");
    d.cleanup().await;
}

const COOKIE: &str = r#"<!doctype html><html><body><div id="c"></div>
<script>document.getElementById('c').textContent=document.cookie||'none';
document.cookie='seen=1; path=/';localStorage.setItem('k','v');</script></body></html>"#;

/// VEL-266: `launchApp clearState` clears cookies, not only the current
/// page's storage.
#[tokio::test]
async fn clear_state_clears_cookies() {
    require_chrome!();
    let base = serve(&[("/c.html", COOKIE)]);
    let url = format!("{base}/c.html");
    let d = driver(vec![]).await;
    // Each read waits for the page it expects: a lookup right after a
    // launch can still be served by the document being replaced (or find
    // none yet) on a loaded machine; the cookie it reports is the page's.
    d.launch_app("web", &url, false).await.unwrap();
    assert_eq!(wait_text(&d, "#c", "none").await, "none");
    d.launch_app("web", &url, false).await.unwrap();
    assert_eq!(
        wait_text(&d, "#c", "seen=1").await,
        "seen=1",
        "the cookie stays without it"
    );
    // Also after stopApp, when the page is about:blank.
    d.stop_app("web", &url).await.unwrap();
    d.launch_app("web", &url, true).await.unwrap();
    assert_eq!(
        wait_text(&d, "#c", "none").await,
        "none",
        "clearState cleared the cookie"
    );
    d.cleanup().await;
}

const HASH: &str = r#"<!doctype html><html><body><div id="h"></div>
<script>function show(){document.getElementById('h').textContent=location.hash||'none'}
show();addEventListener('hashchange',show)</script></body></html>"#;

/// VEL-331: `launchApp` of a URL that differs only in its fragment from
/// the current page is a fresh load, not a 30 s wait for a load event that
/// a same-document navigation never fires.
#[tokio::test]
async fn launch_app_with_a_fragment_is_a_fresh_load() {
    require_chrome!();
    let base = serve(&[("/h.html", HASH)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/h.html#one"), false)
        .await
        .unwrap();
    assert_eq!(text(&d, "#h").await, "#one");
    let start = Instant::now();
    d.launch_app("web", &format!("{base}/h.html#two"), false)
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(text(&d, "#h").await, "#two");
    d.cleanup().await;
}

/// VEL-123: a page that can't load is a runtime failure, not a
/// configuration error (exit 2, never retried).
#[tokio::test]
async fn an_unreachable_app_url_is_not_a_config_error() {
    require_chrome!();
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let d = driver(vec![]).await;
    let err = d
        .launch_app("web", &format!("http://127.0.0.1:{port}/"), false)
        .await
        .unwrap_err();
    assert!(!matches!(err, AtalantaError::Config(_)), "{err}");
    // `navigate` too (review MINOR-6): it mapped every goto error to Config.
    let err = d
        .navigate("web", &format!("http://127.0.0.1:{port}/x"))
        .await
        .unwrap_err();
    assert!(!matches!(err, AtalantaError::Config(_)), "{err}");
    d.cleanup().await;
}

const LIST: &str = r#"<!doctype html><html><body style="margin:0">
<div id="box" style="height:200px;width:300px;overflow:auto">
<div style="height:2000px">tall</div></div>
<div id="pos">0</div>
<script>document.getElementById('box').addEventListener('scroll',function(){
document.getElementById('pos').textContent=String(Math.round(this.scrollTop))})</script>
</body></html>"#;

/// VEL-347: a `swipe` stroke inside a scrollable `<div>` scrolls it; the
/// mouse drag it was selected text and the step passed.
#[tokio::test]
async fn swipe_coords_scrolls_a_div() {
    require_chrome!();
    let base = serve(&[("/l.html", LIST)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/l.html"), false)
        .await
        .unwrap();
    // Finger up from (150,150) to (150,50): the content moves up.
    d.swipe_coords("web", (150, 150), (150, 50)).await.unwrap();
    let pos = wait_text(&d, "#pos", "100").await;
    assert_ne!(pos, "0", "the div scrolled");
    d.cleanup().await;
}

const SLIDER: &str = r#"<!doctype html><html><body style="margin:0;height:3000px">
<div id="track" style="position:absolute;left:0;top:0;width:400px;height:40px;background:#ccc;touch-action:pan-y"></div>
<div id="val">0</div>
<div id="box" style="position:absolute;left:0;top:100px;height:200px;width:300px;overflow:auto">
<div style="height:2000px">tall</div></div>
<div id="pos" style="position:absolute;top:400px">0</div>
<script>
var t=document.getElementById('track'),down=false,x0=0;
t.addEventListener('pointerdown',function(e){down=true;x0=e.clientX;t.setPointerCapture(e.pointerId)});
t.addEventListener('pointermove',function(e){if(down)document.getElementById('val').textContent=String(Math.round(e.clientX-x0))});
t.addEventListener('pointerup',function(){down=false});
document.getElementById('box').addEventListener('scroll',function(){
document.getElementById('pos').textContent=String(Math.round(this.scrollTop))});
</script></body></html>"#;

/// Review MAJOR-1: a `swipe` across a pointer-driven widget (a slider, a
/// carousel) drags it; the wheel that VEL-347 made of every stroke moved
/// nothing there and the step passed. A stroke in a scrollable `<div>` on
/// the same (scrollable) page is still the wheel.
#[tokio::test]
async fn swipe_coords_drags_a_pointer_widget_and_scrolls_a_div() {
    require_chrome!();
    let base = serve(&[("/s.html", SLIDER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    d.swipe_coords("web", (50, 20), (250, 20)).await.unwrap();
    assert_eq!(
        wait_text(&d, "#val", "200").await,
        "200",
        "the slider moved"
    );

    d.swipe_coords("web", (150, 250), (150, 150)).await.unwrap();
    assert_eq!(
        wait_text(&d, "#pos", "100").await,
        "100",
        "the div scrolled"
    );
    d.cleanup().await;
}

/// `extra_args` (`web.args`) reach Chrome. chromiumoxide adds the `--`
/// itself, so `--js-flags=--expose-gc` arrived as `----js-flags=...` and
/// was ignored, like every configured argument.
#[tokio::test]
async fn extra_args_reach_chrome() {
    require_chrome!();
    let base = serve(&[(
        "/g.html",
        "<html><body><div id=g></div><script>document.getElementById('g').textContent=typeof gc</script></body></html>",
    )]);
    let d = atalanta_web::WebDriver::new(atalanta_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into(), "--js-flags=--expose-gc".into()],
        ..Default::default()
    });
    d.prepare("web").await.unwrap();
    d.launch_app("web", &format!("{base}/g.html"), false)
        .await
        .unwrap();
    assert_eq!(text(&d, "#g").await, "function");
    d.cleanup().await;
}

/// VEL-117: two drivers run at once. Both used chromiumoxide's one fixed
/// profile dir, and Chrome's lock on it failed the second launch.
#[tokio::test]
async fn two_drivers_run_at_once() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let (a, b) = tokio::join!(driver(vec![]), driver(vec![]));
    for d in [&a, &b] {
        d.launch_app("web", &format!("{base}/p.html"), false)
            .await
            .unwrap();
        assert_eq!(text(d, "#a").await, "first");
    }
    a.cleanup().await;
    b.cleanup().await;
}

const API: &str = r#"<!doctype html><html><body><div id="r">pending</div><div id="b">pending</div>
<script>
fetch('http://api.atalanta.invalid/api/save',{method:'POST',credentials:'include',
  headers:{'content-type':'application/json','x-token':'t'},body:'{}'})
 .then(r=>r.json()).then(j=>{document.getElementById('r').textContent='ok:'+j.saved})
 .catch(e=>{document.getElementById('r').textContent='error'});
fetch('http://127.0.0.1:9/broken').then(()=>{document.getElementById('b').textContent='answered'})
 .catch(()=>{document.getElementById('b').textContent='failed'});
</script></body></html>"#;

/// VEL-129: a mocked cross-origin request that needs a CORS preflight, sent
/// with credentials, reaches the page. VEL-121: a mock that can't be
/// answered (its file is missing) fails the request instead of leaving it
/// paused forever.
#[tokio::test]
async fn cross_origin_mocks_pass_cors_and_broken_mocks_fail() {
    require_chrome!();
    let base = serve(&[("/api.html", API)]);
    let d = driver(vec![
        MockRule {
            url: "/api/save".into(),
            method: Some("POST".into()),
            json: Some(serde_json::json!({ "saved": true })),
            times: Some(1),
            ..Default::default()
        },
        MockRule {
            url: "/broken".into(),
            file: Some("/nonexistent/atalanta-mock.json".into()),
            ..Default::default()
        },
    ])
    .await;
    d.launch_app("web", &format!("{base}/api.html"), false)
        .await
        .unwrap();
    assert_eq!(wait_text(&d, "#r", "ok:true").await, "ok:true");
    assert_eq!(wait_text(&d, "#b", "failed").await, "failed");
    d.cleanup().await;
}

/// Review MAJOR-2 (VEL-122): after `restart_session` (how a crashed Chrome
/// is recovered) the suite's mocks still answer. The "Fetch is on" flag
/// stayed set from the dead page, so the new page never enabled Fetch and
/// requests went to the real network.
#[tokio::test]
async fn mocks_survive_a_session_restart() {
    require_chrome!();
    let base = serve(&[("/api.html", API)]);
    let d = driver(vec![MockRule {
        url: "/api/save".into(),
        method: Some("POST".into()),
        json: Some(serde_json::json!({ "saved": true })),
        ..Default::default()
    }])
    .await;
    let url = format!("{base}/api.html");
    d.launch_app("web", &url, false).await.unwrap();
    assert_eq!(wait_text(&d, "#r", "ok:true").await, "ok:true");

    d.restart_session().await.unwrap();
    d.launch_app("web", &url, false).await.unwrap();
    assert_eq!(
        wait_text(&d, "#r", "ok:true").await,
        "ok:true",
        "the mock answers after the restart"
    );
    d.cleanup().await;
}

/// The DOM walker's script began with a newline right after `return`, so
/// it returned `undefined` and every `get_hierarchy` (and the Index and
/// text-compound fallbacks) failed with "No value found".
#[tokio::test]
async fn get_hierarchy_returns_the_tree() {
    require_chrome!();
    let base = serve(&[("/p.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/p.html"), false)
        .await
        .unwrap();
    let tree = d.get_hierarchy("web").await.unwrap();
    assert_eq!(tree.element_type, "body");
    assert!(
        atalanta_common::selector_match::find_in_tree(&tree, &Selector::Text("first".into()), None)
            .is_some(),
        "{tree:?}"
    );
    // An Index selector over a text match goes through the tree.
    let el = d
        .find_element(
            "web",
            &Selector::Index {
                selector: Box::new(Selector::Text("first".into())),
                index: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(el.text.as_deref(), Some("first"));
    d.cleanup().await;
}

const STATE: &str = r#"<!doctype html><html><body style="margin:0">
<input id="pw" type="password"><input id="name">
<input id="cb" type="checkbox" checked><div id="sw" role="switch" aria-checked="false">sw</div>
<div id="plain">plain text</div>
<div id="low" style="position:absolute;top:0;left:0;width:50px;height:5000px">tall</div>
</body></html>"#;

/// D-39: the web tree reports focus, password fields, checkbox / switch
/// state and the on-screen part of an element.
#[tokio::test]
async fn element_state_fields_on_web() {
    require_chrome!();
    let base = serve(&[("/s.html", STATE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let pw = find(&d, "#pw").await;
    assert!(pw.obscured && pw.focused == Some(false));
    assert_eq!(pw.checked, None);
    d.input_text("web", &pw, "secret").await.unwrap();
    assert_eq!(
        find(&d, "#pw").await.focused,
        Some(true),
        "focused after typing"
    );
    assert_eq!(find(&d, "#name").await.focused, Some(false));
    assert_eq!(find(&d, "#cb").await.checked, Some(true));
    assert_eq!(find(&d, "#sw").await.checked, Some(false));
    let tall = find(&d, "#low").await;
    let vb = tall.visible_bounds.expect("visible rect");
    assert!(
        vb.height < tall.bounds.height,
        "{vb:?} vs {:?}",
        tall.bounds
    );
    d.cleanup().await;
}

/// D-5 (VEL-133): inputText on something that takes no text fails, rather
/// than passing with nothing typed.
#[tokio::test]
async fn input_text_into_a_div_fails() {
    require_chrome!();
    let base = serve(&[("/s.html", STATE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let plain = find(&d, "#plain").await;
    let err = d.input_text("web", &plain, "x").await.unwrap_err();
    assert!(
        matches!(err, AtalantaError::AssertionFailed { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("takes no text"), "{err}");
    let name = find(&d, "#name").await;
    d.input_text("web", &name, "ok").await.unwrap();
    // `web.focusCheck: off`: typed wherever the focus is, as before D-5.
    let saved = atalanta_common::env::settings_snapshot();
    let mut off = saved.clone();
    off.insert("ATALANTA_WEB_FOCUS_CHECK".into(), "0".into());
    atalanta_common::env::set_settings(off);
    let r = d.input_text("web", &plain, "x").await;
    atalanta_common::env::set_settings(saved);
    r.unwrap();
    d.cleanup().await;
}

const CARD_FIELD: &str = r#"<!doctype html><html><body style="margin:0">
<input id="n" style="width:300px;height:60px"></body></html>"#;

const SHADOW_FIELD: &str = r#"<!doctype html><html><body style="margin:0">
<x-field id="h"></x-field>
<script>customElements.define('x-field', class extends HTMLElement {
  constructor() { super(); this.attachShadow({mode: 'open'}).innerHTML =
    '<input id="i" style="width:300px;height:60px">'; }
});</script></body></html>"#;

/// Review MAJOR 3: a field inside a cross-origin iframe (a Stripe card
/// field) left the outer page's focus on the `<iframe>`, which read as
/// "takes no text"; a shadow-DOM field inside a same-origin iframe left it
/// on the custom element's host. The first is unknown, the second is found.
#[tokio::test]
async fn input_text_into_a_field_inside_an_iframe_is_not_refused() {
    require_chrome!();
    let inner = serve(&[("/card.html", CARD_FIELD), ("/shadow.html", SHADOW_FIELD)]);
    let port = inner.rsplit(':').next().unwrap();
    // `localhost` vs the outer page's 127.0.0.1: another origin.
    let cross = format!(
        "<!doctype html><html><body style=\"margin:0\"><iframe id=\"card\" \
         src=\"http://localhost:{port}/card.html\" style=\"width:320px;height:80px;border:0\"></iframe></body></html>"
    );
    let same = format!(
        "<!doctype html><html><body style=\"margin:0\"><iframe id=\"sh\" onload=\"window.shLoaded=1\" \
         src=\"{inner}/shadow.html\" style=\"width:320px;height:80px;border:0\"></iframe></body></html>"
    );
    let outer = serve(&[
        ("/cross.html", cross.as_str()),
        ("/same.html", same.as_str()),
    ]);
    let d = driver(vec![]).await;
    for (page, frame) in [("cross.html", "#card"), ("same.html", "#sh")] {
        d.launch_app("web", &format!("{outer}/{page}"), false)
            .await
            .unwrap();
        // The frame's document loaded: its field is found (another
        // origin's frame), or its `load` fired (the shadow-DOM one).
        let ready = if page == "cross.html" {
            common::wait_found(&d, "#n").await
        } else {
            common::eventually(|| async {
                d.debug_eval_main("window.shLoaded === 1")
                    .await
                    .is_ok_and(|v| v == true)
            })
            .await
        };
        assert!(ready, "{page}: the frame never loaded");
        let f = find(&d, frame).await;
        d.input_text("web", &f, "4242")
            .await
            .unwrap_or_else(|e| panic!("{page}: {e}"));
    }
    d.cleanup().await;
}
