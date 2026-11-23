//! Review 10 (web/DOM): the index explore writes resolves to the same
//! element in the page, a page overlay over a same-origin frame takes no
//! click unnoticed, `aria-hidden` decoration stays out of text and names,
//! wrappers don't repeat their children's text in the tree, new tabs are
//! followed, a crashed renderer is a lost connection, and typing presses
//! keys. Against a real headless Chrome (see `common`).

mod common;

use std::time::{Duration, Instant};

use atalanta_common::selector_build::robust_selector;
use atalanta_common::{AtalantaError, Element, Platform, PlatformDriver, Rect, Selector};
use common::{driver, find, serve};

const TWINS: &str = r#"<!doctype html><html><body style="margin:0">
<div style="display:none"><button class="add" id="h">Add to cart</button></div>
<div style="height:1200px"><button class="add" id="above">Add to cart</button></div>
<button class="add" id="a">Add to cart</button>
<button class="add" id="b">Add to cart</button>
<div style="height:2000px"></div>
<button class="add" id="below">Add to cart</button>
</body></html>"#;

fn walk<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    out.push(e);
    for c in &e.children {
        walk(c, out);
    }
}

/// Review 10 #1: explore counts `index:` among the matches on screen in
/// reading order; the page counted every CSS match in document order
/// (hidden ones too) and text page-wide, so a recorded index tapped
/// another element on replay.
#[tokio::test]
async fn the_index_explore_writes_resolves_to_the_same_element() {
    require_chrome!();
    let base = serve(&[("/t.html", TWINS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    // Scrolled so that `#above` is off screen above and `#below` below.
    d.debug_eval_main("window.scrollTo(0, 1100); 1")
        .await
        .unwrap();
    let (w, h) = d.screen_size("web").await.unwrap();
    let screen = Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let tree = d.get_hierarchy("web").await.unwrap();
    let mut all = Vec::new();
    walk(&tree, &mut all);
    for want in ["a", "b"] {
        let target = all
            .iter()
            .find(|e| e.dom_id.as_deref() == Some(want))
            .expect("in the tree");
        // Without its id, as an element with no id would be.
        let mut anon = (*target).clone();
        anon.dom_id = None;
        let mut root = tree.clone();
        fn strip(e: &mut Element) {
            e.dom_id = None;
            e.children.iter_mut().for_each(strip);
        }
        strip(&mut root);
        let sel = robust_selector(&anon, &root, Some(&screen), Platform::Web);
        assert!(matches!(sel, Selector::Index { .. }), "{sel:?}");
        let got = d.find_element("web", &sel).await.unwrap();
        assert_eq!(got.dom_id.as_deref(), Some(want), "{sel:?}");
    }
    // A CSS list counts the same population: the hidden twin and the
    // ones off screen are not counted.
    let second = d
        .find_element(
            "web",
            &Selector::Index {
                selector: Box::new(Selector::ClassName("add".into())),
                index: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(second.dom_id.as_deref(), Some("b"));
    d.cleanup().await;
}

const FRAMED: &str = r#"<!doctype html><html><body style="margin:0">
<iframe id="f" style="border:0;width:400px;height:200px" srcdoc="<button id=pay style='width:300px;height:100px' onclick='parent.hitPay=(parent.hitPay||0)+1'>Pay</button>"></iframe>
<div id="banner" style="position:fixed;left:0;top:0;width:100%;height:150px;background:#fff">Cookies</div>
<button id="accept" style="position:fixed;top:160px;left:500px" onclick="document.getElementById('banner').remove()">Accept</button>
</body></html>"#;

/// Review 10 #2: a banner on the page over a same-origin frame takes the
/// click; the frame's own hit test can't see it, and the step passed.
#[tokio::test]
async fn a_page_banner_over_a_same_origin_frame_fails_the_tap() {
    require_chrome!();
    let base = serve(&[("/f.html", FRAMED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/f.html"), false)
        .await
        .unwrap();
    let pay = d
        .find_element("web", &Selector::Text("Pay".into()))
        .await
        .unwrap();
    let err = d.tap("web", &pay).await.expect_err("the banner covers it");
    assert!(
        matches!(err, AtalantaError::NotActionable { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("div"), "{err}");
    assert_eq!(
        d.debug_eval_main("String(window.hitPay)").await.unwrap(),
        "undefined"
    );
    // Once the banner goes, the tap goes through.
    let accept = find(&d, "#accept").await;
    d.tap("web", &accept).await.unwrap();
    let pay = d
        .find_element("web", &Selector::Text("Pay".into()))
        .await
        .unwrap();
    d.tap("web", &pay).await.unwrap();
    assert_eq!(
        d.debug_eval_main("String(window.hitPay)").await.unwrap(),
        "1"
    );
    d.cleanup().await;
}

const ICONS: &str = r#"<!doctype html><html><body>
<button id="del"><i class="material-icons" aria-hidden="true">delete</i> Delete</button>
<label for="mail">Email <span aria-hidden="true">*</span></label><input id="mail">
<main id="main"><section id="sec"><div id="wrap"><h1 id="title">Dashboard</h1></div>
<div id="price"><span>$</span><span>42</span></div>
<div id="card"><h2>Monthly report for the whole team</h2><p>Revenue went up in every region this quarter</p></div>
</section></main>
</body></html>"#;

/// Review 10 #3 and #4: `aria-hidden` decoration is no part of an
/// element's text or name, and a wrapper doesn't repeat its children's
/// text in the tree.
#[tokio::test]
async fn aria_hidden_text_and_wrapper_text_stay_out() {
    require_chrome!();
    let base = serve(&[("/i.html", ICONS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/i.html"), false)
        .await
        .unwrap();
    let by_name = d
        .find_element("web", &Selector::AccessibilityId("Delete".into()))
        .await
        .unwrap();
    assert_eq!(by_name.dom_id.as_deref(), Some("del"));
    assert_eq!(by_name.text.as_deref(), Some("Delete"));
    let by_text = d
        .find_element("web", &Selector::Text("Delete".into()))
        .await
        .unwrap();
    assert_eq!(by_text.dom_id.as_deref(), Some("del"));
    let mail = d
        .find_element("web", &Selector::AccessibilityId("Email".into()))
        .await
        .unwrap();
    assert_eq!(mail.dom_id.as_deref(), Some("mail"));

    let tree = d.get_hierarchy("web").await.unwrap();
    let mut all = Vec::new();
    walk(&tree, &mut all);
    let text = |id: &str| {
        all.iter()
            .find(|e| e.dom_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("#{id} in the tree"))
            .text
            .clone()
    };
    assert_eq!(tree.text, None, "<body> repeats the page");
    assert_eq!(text("main"), None);
    assert_eq!(text("sec"), None);
    assert_eq!(text("wrap"), None, "it only repeats its <h1>");
    assert_eq!(text("card"), None);
    assert_eq!(text("title").as_deref(), Some("Dashboard"));
    assert_eq!(
        text("price").as_deref(),
        Some("$42"),
        "a short phrase stays"
    );
    assert_eq!(
        text("del").as_deref(),
        Some("Delete"),
        "a button keeps its text"
    );
    d.cleanup().await;
}

const OPENER: &str = r#"<!doctype html><html><body>
<a id="tab" href="/second.html" target="_blank">Open in new tab</a>
<button id="pop" onclick="window.open('/popup.html', 'p', 'width=400,height=400')">Sign in</button>
<p>First page</p>
</body></html>"#;
const SECOND: &str = r#"<!doctype html><html><body><p>Second page</p></body></html>"#;
const POPUP: &str = r#"<!doctype html><html><body>
<p>Popup page</p><button id="done" onclick="window.close()">Done</button>
</body></html>"#;

async fn has_text(d: &impl PlatformDriver, t: &str) -> bool {
    for _ in 0..60 {
        if d.find_element("web", &Selector::Text(t.into()))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Review 10 #5: a tab or popup the page opens is driven from then on,
/// and when it closes the page that opened it is again.
#[tokio::test]
async fn new_tabs_and_popups_are_followed() {
    require_chrome!();
    let base = serve(&[
        ("/o.html", OPENER),
        ("/second.html", SECOND),
        ("/popup.html", POPUP),
    ]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/o.html"), false)
        .await
        .unwrap();
    let pop = find(&d, "#pop").await;
    d.tap("web", &pop).await.unwrap();
    assert!(has_text(&d, "Popup page").await, "the popup is followed");
    let done = find(&d, "#done").await;
    d.tap("web", &done).await.unwrap();
    assert!(has_text(&d, "First page").await, "back on its opener");
    let tab = find(&d, "#tab").await;
    d.tap("web", &tab).await.unwrap();
    assert!(has_text(&d, "Second page").await, "the new tab is followed");
    d.cleanup().await;
}

/// Review 10 #6: after the renderer crashes, a call fails at once as a
/// lost connection (the executor starts a new browser), not after a 10 s
/// DevTools timeout as an internal error.
#[tokio::test]
async fn a_renderer_crash_is_a_lost_connection() {
    require_chrome!();
    let base = serve(&[("/c.html", SECOND)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), d.navigate("web", "chrome://crash")).await;
    let start = Instant::now();
    let mut err = None;
    while start.elapsed() < Duration::from_secs(8) {
        match d
            .find_element("web", &Selector::Text("Second page".into()))
            .await
        {
            Err(e @ AtalantaError::BrowserConnectionLost { .. }) => {
                err = Some(e);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let err = err.expect("a lost connection");
    assert!(err.to_string().contains("crashed"), "{err}");
    // The restart brings a working page back.
    d.restart_session().await.unwrap();
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    assert!(has_text(&d, "Second page").await);
    d.cleanup().await;
}

const KEYS: &str = r#"<!doctype html><html><body>
<input id="q" onkeyup="window.ups=(window.ups||0)+1; document.getElementById('out').textContent=this.value">
<div id="out"></div>
</body></html>"#;

/// Review 10 #7: typed text presses keys, so a box that filters on
/// `keyup` reacts; text outside ASCII still arrives.
#[tokio::test]
async fn typing_presses_keys() {
    require_chrome!();
    let base = serve(&[("/k.html", KEYS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/k.html"), false)
        .await
        .unwrap();
    let q = find(&d, "#q").await;
    d.input_text("web", &q, "Par").await.unwrap();
    assert_eq!(d.debug_eval_main("String(window.ups)").await.unwrap(), "3");
    assert_eq!(common::wait_text(&d, "#out", "Par").await, "Par");
    d.input_text("web", &q, "is, Île-de-France!").await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('q').value")
            .await
            .unwrap(),
        "Paris, Île-de-France!"
    );
    d.cleanup().await;
}
