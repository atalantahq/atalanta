//! Round 2: what the web driver reads, matches and acts on, against a real
//! headless Chrome and a local page (see `common` for the fixtures).

mod common;

use std::time::{Duration, Instant};

use atalanta_common::{AtalantaError, PlatformDriver, Relation, Selector};
use common::{css, driver, find, serve, text, wait_text};

fn text_sel(s: &str) -> Selector {
    Selector::Text(s.into())
}

const FIELDS: &str = r#"<!doctype html><html><body>
<input id="in" value="hello">
<textarea id="ta">line one</textarea>
<select id="sel"><option value="us">United States</option><option value="ca" selected>Canada</option></select>
<input id="pw" type="password" value="hunter2">
<input id="cb" type="checkbox" value="on">
</body></html>"#;

/// VEL-130: a form field's text is its value (the selected option's text
/// for a `<select>`), as on Android and iOS. It was `textContent`, which is
/// always "" for a field, so `assertText` could never check one.
#[tokio::test]
async fn form_fields_report_their_value() {
    require_chrome!();
    let base = serve(&[("/f.html", FIELDS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/f.html"), false)
        .await
        .unwrap();
    assert_eq!(text(&d, "#in").await, "hello");
    assert_eq!(text(&d, "#ta").await, "line one");
    assert_eq!(text(&d, "#sel").await, "Canada");
    // A password's value is never read out, nor a checkbox's "on".
    assert_eq!(text(&d, "#pw").await, "");
    assert_eq!(text(&d, "#cb").await, "");
    let input = find(&d, "#in").await;
    assert_eq!(input.text.as_deref(), Some("hello"), "the capture too");
    d.input_text("web", &input, " world").await.unwrap();
    assert_eq!(wait_text(&d, "#in", "hello world").await, "hello world");
    // `text:` finds the field by what it shows.
    let found = d
        .find_element("web", &text_sel("hello world"))
        .await
        .unwrap();
    assert_eq!(found.element_type, "input");
    d.cleanup().await;
}

const RENDERED: &str = "<!doctype html><html><body>\
<button id=\"gear\"><svg width=\"10\" height=\"10\"><title>gear</title><rect width=\"10\" height=\"10\"/></svg> Settings</button>\
<button id=\"sign\">Sign\n      in</button>\
<p id=\"cafe\">Cafe\u{301} au lait</p>\
<p id=\"zw\">Pay\u{200B}now</p>\
</body></html>";

/// VEL-273/267: text is what the page renders. `textContent` added an svg
/// `<title>` ("gear Settings") and kept source line breaks and indentation
/// ("Sign\n      in"), so the text a user sees never matched; a decomposed
/// accent or a zero-width space did the same.
#[tokio::test]
async fn text_is_what_the_page_renders() {
    require_chrome!();
    let base = serve(&[("/r.html", RENDERED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/r.html"), false)
        .await
        .unwrap();
    for (needle, id) in [
        ("Settings", "gear"),
        ("Sign in", "sign"),
        ("Caf\u{e9} au lait", "cafe"),
        ("Paynow", "zw"),
    ] {
        let el = d
            .find_element("web", &text_sel(needle))
            .await
            .unwrap_or_else(|e| panic!("{needle}: {e}"));
        assert_eq!(el.text.as_deref(), Some(needle), "{id}");
    }
    assert_eq!(text(&d, "#sign").await, "Sign in");
    d.cleanup().await;
}

/// 60 nested `<div>`s around a button, then a decoy.
fn deep_page() -> String {
    let mut html = String::from("<!doctype html><html><body>");
    for _ in 0..60 {
        html.push_str("<div class=\"lvl\">");
    }
    html.push_str("<button class=\"primary\">Save</button>");
    for _ in 0..60 {
        html.push_str("</div>");
    }
    html.push_str("<button class=\"secondary\">Cancel</button></body></html>");
    html
}

/// VEL-126: an element 60 levels deep is found by a selector that needs
/// more than CSS, and is in the hierarchy. The walker stopped at depth 40,
/// so the compound was "not found" and `index:` hit an ancestor.
#[tokio::test]
async fn deep_elements_are_found() {
    require_chrome!();
    let page = deep_page();
    let base = serve(&[("/d.html", &page)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/d.html"), false)
        .await
        .unwrap();
    let el = d
        .find_element(
            "web",
            &Selector::Compound(vec![
                text_sel("Save"),
                Selector::ClassName("primary".into()),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(el.element_type, "button");
    let el = d
        .find_element(
            "web",
            &Selector::Index {
                selector: Box::new(text_sel("Save")),
                index: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(el.element_type, "button");
    let tree = d.get_hierarchy("web").await.unwrap();
    let found =
        atalanta_common::selector_match::find_in_tree(&tree, &text_sel("Save"), None).unwrap();
    assert_eq!(found.element_type, "button");
    d.cleanup().await;
}

const TWINS: &str = r#"<!doctype html><html><body>
<div style="display:none"><button class="primary">Save</button></div>
<div id="panel"><button id="p" class="primary"><span>Save</span></button>
<button id="s" class="secondary">Save</button></div>
<div id="other"><button class="primary">Other</button></div>
</body></html>"#;

/// VEL-128/127: a selector part means the same on every path: `className`
/// is a CSS class (not the tag) and `id` includes `data-testid`, also when
/// a text part or a relation sends it past plain CSS. And a hidden twin
/// never wins over the visible element.
#[tokio::test]
async fn selector_parts_mean_the_same_on_every_path() {
    require_chrome!();
    let base = serve(&[("/t.html", TWINS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let el = d
        .find_element(
            "web",
            &Selector::Compound(vec![
                text_sel("Save"),
                Selector::ClassName("primary".into()),
            ]),
        )
        .await
        .unwrap();
    assert!(el.visible && el.bounds.width > 0, "{el:?}");
    assert_eq!(el.element_type, "button");
    // The second visible "Save" (index counts visible matches).
    let el = d
        .find_element(
            "web",
            &Selector::Index {
                selector: Box::new(text_sel("Save")),
                index: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(el.dom_id.as_deref(), Some("s"));
    // A relation: the primary button inside #panel, not the one in #other.
    let el = d
        .find_element(
            "web",
            &Selector::Relative {
                target: Box::new(Selector::ClassName("primary".into())),
                relation: Relation::ChildOf,
                anchor: Box::new(Selector::Id("panel".into())),
            },
        )
        .await
        .unwrap();
    assert_eq!(el.dom_id.as_deref(), Some("p"));
    d.cleanup().await;
}

/// VEL-120: a substring search over 20 000 matching rows answers in under
/// 2 s. Every match was checked against every other (`contains`): 14-23 s
/// in the report, 4.5 s here.
#[tokio::test]
async fn text_search_is_linear() {
    require_chrome!();
    // Growth, not a wall-clock limit: a shared CI runner took 3.3 s for what
    // a laptop does in under 1 s. Four times the rows should take about four
    // times as long; the quadratic version took about sixteen.
    let d = driver(vec![]).await;
    let mut took = Vec::new();
    for n in [5_000, 20_000] {
        let rows: String = (0..n)
            .map(|i| format!("<div class=\"row\">lorem {i}</div>"))
            .collect();
        let page =
            format!("<!doctype html><html><body><div id=\"list\">{rows}</div></body></html>");
        let base = serve(&[("/big.html", &page)]);
        d.launch_app("web", &format!("{base}/big.html"), false)
            .await
            .unwrap();
        // Best of three, so one hiccup of a busy runner isn't measured.
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            let all = d
                .find_elements("web", &Selector::TextContains("lorem".into()))
                .await
                .unwrap();
            best = best.min(start.elapsed());
            assert_eq!(all.len(), n);
        }
        took.push(best);
    }
    let ratio = took[1].as_secs_f64() / took[0].as_secs_f64().max(1e-3);
    assert!(
        ratio < 8.0,
        "4x the rows took {ratio:.1}x as long ({:?} -> {:?}): not linear",
        took[0],
        took[1]
    );
    d.cleanup().await;
}

const HOSTILE: &str = r#"<!doctype html><html><body>
<div id="a" class="card">first</div>
<button id="probe" onclick="document.getElementById('seen').textContent=typeof window.__atalantaIds">probe</button>
<div id="seen">-</div>
<script>
Document.prototype.querySelectorAll=function(){return []};
Element.prototype.querySelectorAll=function(){return []};
Document.prototype.evaluate=function(){throw new Error('nope')};
Element.prototype.getBoundingClientRect=function(){return {x:0,y:0,left:0,top:0,width:0,height:0,right:0,bottom:0}};
JSON.stringify=function(){return '"tampered"'};
</script></body></html>"#;

/// P4: Atalanta's queries run in an isolated world, so a page that patches
/// DOM prototypes (polyfills, test doubles, hostile scripts) can't break
/// them, and the page can't see Atalanta's own state.
#[tokio::test]
async fn queries_run_in_an_isolated_world() {
    require_chrome!();
    let base = serve(&[("/h.html", HOSTILE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/h.html"), false)
        .await
        .unwrap();
    let a = find(&d, "#a").await;
    assert!(a.bounds.width > 0, "{a:?}");
    d.find_element("web", &Selector::XPath("//div[@id='a']".into()))
        .await
        .unwrap();
    assert_eq!(
        d.find_elements("web", &css(".card")).await.unwrap().len(),
        1
    );
    let probe = find(&d, "#probe").await;
    d.tap("web", &probe).await.unwrap();
    assert_eq!(wait_text(&d, "#seen", "undefined").await, "undefined");
    d.cleanup().await;
}

const CUSTOM_BOX: &str = r#"<!doctype html><html><body>
<label for="custom" style="display:inline-block;padding:8px;border:1px solid">
<input id="custom" type="checkbox" style="opacity:0;position:absolute;width:1px;height:1px">Custom box</label>
<div id="state">off</div>
<script>document.getElementById('custom').addEventListener('change',function(){
document.getElementById('state').textContent=this.checked?'on':'off'})</script>
</body></html>"#;

/// VEL-275: a visually hidden native control (an opacity-0 custom checkbox)
/// is tapped by its id through its label, as a user would. It failed "not
/// visible".
#[tokio::test]
async fn a_hidden_control_is_tapped_through_its_label() {
    require_chrome!();
    let base = serve(&[("/c.html", CUSTOM_BOX)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    let cb = find(&d, "#custom").await;
    d.tap("web", &cb).await.unwrap();
    assert_eq!(wait_text(&d, "#state", "on").await, "on");
    d.cleanup().await;
}

const BANNER: &str = r#"<!doctype html><html><body style="margin:0">
<div style="position:fixed;left:0;right:0;top:350px;height:100px;background:#c00;z-index:5">banner</div>
<div style="height:2000px"></div>
<button id="go" style="height:30px" onclick="document.getElementById('out').textContent='tapped'">Go</button>
<div style="height:2000px"></div>
<div id="out" style="position:fixed;top:0;left:0">idle</div>
</body></html>"#;

/// P6: a target that lands under a fixed banner when centred is tapped
/// after trying the other scroll alignments (Playwright cycles them too).
/// The one centred scroll left it covered, and the tap failed "obscured".
#[tokio::test]
async fn a_fixed_banner_does_not_block_a_tap() {
    require_chrome!();
    let base = serve(&[("/b.html", BANNER)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/b.html"), false)
        .await
        .unwrap();
    let go = find(&d, "#go").await;
    d.tap("web", &go).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "tapped").await, "tapped");
    d.cleanup().await;
}

// The button moves 80 px on each of 4 steps once `slide()` is called, then
// stays put. A step is the next animation frame, or 50 ms after the last
// step if no frame came: the probe (`act` in `injected.js`) waits for two
// animation frames but stops waiting for one after 100 ms (a hidden page
// never paints), so on a loaded machine whose renderer misses frames it
// compares boxes 200 ms apart. A slide counted in frames alone doesn't move
// in that time, and the probe saw two equal boxes mid-slide. Few steps, so
// the slide is over well inside the tap's one second of patience. A click
// that arrives before the last step reports "early".
const SLIDING: &str = r#"<!doctype html><html><body style="margin:0">
<button id="mv" style="position:absolute;top:100px;left:0;width:30px;height:30px;padding:0"
 onclick="document.getElementById('out').textContent = left === 320 ? 'tapped' : 'early'"></button>
<div id="out" style="position:absolute;top:0">idle</div>
<script>
var left = 0, log = [];
function note(s) { log.push(s + ' left=' + left + ' t=' + performance.now().toFixed(0)); }
['pointerdown', 'mousedown', 'click'].forEach(function (k) {
  addEventListener(k, function (e) {
    note(k + '@' + e.clientX + ',' + e.clientY + ' on=' + (e.target.id || e.target.tagName));
  }, true);
});
function slide() {
  var b = document.getElementById('mv');
  (function step() {
    if (left >= 320) return;
    left += 80; b.style.left = left + 'px'; note('step');
    var next = function () { if (!next.done) { next.done = true; step(); } };
    requestAnimationFrame(next);
    setTimeout(next, 50);
  })();
}
</script></body></html>"#;

/// P6: an element still moving (a slide-in) is tapped where it comes to
/// rest: the probe waits for the same box on two animation frames. The
/// slide starts from the test just before the tap and is driven by frames
/// (or, when the renderer misses them, by a short timer), so the outcome
/// doesn't depend on how fast the machine is (it flaked under parallel
/// load as a 600 ms CSS animation from first render, and again as a slide
/// counted in frames only).
#[tokio::test]
async fn a_moving_element_is_tapped_where_it_stops() {
    require_chrome!();
    let base = serve(&[("/m.html", SLIDING)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/m.html"), false)
        .await
        .unwrap();
    let mv = find(&d, "#mv").await;
    d.debug_eval_main("slide()").await.unwrap();
    d.tap("web", &mv).await.unwrap();
    let out = wait_text(&d, "#out", "tapped").await;
    // What the page saw, in order: each animation step and each pointer
    // event with where it landed, so a miss says why.
    let log = d.debug_eval_main("JSON.stringify(log)").await.unwrap();
    assert_eq!(out, "tapped", "page log: {log}");
    d.cleanup().await;
}

const NAV_A: &str = r#"<!doctype html><html><body><div id="where">a</div>
<div id="h"></div><script>function show(){document.getElementById('h').textContent=decodeURIComponent(location.hash)}
show();addEventListener('hashchange',show)</script></body></html>"#;
const NAV_B: &str = r#"<!doctype html><html><body><div id="where">b</div></body></html>"#;

/// VEL-271/125: `goto` with a relative URL resolves against the page, a
/// fragment the browser re-encodes doesn't wait out the 20 s timeout, and
/// `goBack` with nothing to go back to fails at once. Each took 20 s and
/// passed without doing anything.
#[tokio::test]
async fn navigation_resolves_urls_and_fails_fast() {
    require_chrome!();
    let base = serve(&[("/dir/a.html", NAV_A), ("/dir/b.html", NAV_B)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/dir/a.html"), false)
        .await
        .unwrap();

    let start = Instant::now();
    d.navigate("web", "b.html").await.unwrap();
    assert_eq!(text(&d, "#where").await, "b", "relative to the page");
    d.navigate("web", "/dir/a.html").await.unwrap();
    assert_eq!(text(&d, "#where").await, "a", "root-relative");
    d.navigate("web", &format!("{base}/dir/a.html#caf\u{e9} au lait"))
        .await
        .unwrap();
    assert_eq!(
        wait_text(&d, "#h", "#caf\u{e9} au lait").await,
        "#caf\u{e9} au lait"
    );
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "{:?}",
        start.elapsed()
    );

    // History: a, b, a, a#café (the first load replaced the blank page).
    // Back to the very start, then once more.
    for want in ["a", "b", "a"] {
        d.go_back("web").await.unwrap();
        assert_eq!(text(&d, "#where").await, want);
    }
    let start = Instant::now();
    let err = d.go_back("web").await.unwrap_err();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert!(
        !matches!(err, AtalantaError::BrowserConnectionLost { .. }),
        "{err}"
    );
    d.cleanup().await;
}

const TABS: &str = r#"<!doctype html><html><body>
<button id="tb" onclick="show('B')">Tab B</button>
<div id="panel"><h2>Panel A</h2></div>
<script>function show(t){setTimeout(function(){
document.getElementById('panel').innerHTML='<h2>Panel '+t+'</h2>'},400)}</script>
</body></html>"#;

/// VEL-067: the web driver can tell when the page has finished reacting to
/// an action (a MutationObserver plus running animations, checked each
/// frame), so the next step doesn't act on the old screen. It said it
/// couldn't, and web waited only for the network.
#[tokio::test]
async fn settle_waits_for_a_delayed_render() {
    require_chrome!();
    let base = serve(&[("/t.html", TABS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let tb = find(&d, "#tb").await;
    d.tap("web", &tb).await.unwrap();
    // The render lands 400 ms after the click, long after the tap returns
    // even on a loaded machine (a 150 ms delay raced the tap's own
    // actionability checks under parallel test load); a 1.5 s reaction
    // window still has to see it.
    let r = d
        .settle_on_device("web", 1500, 100, 3000)
        .await
        .expect("the web driver settles on the page");
    assert!(r.settled && r.reacted, "{r:?}");
    assert_eq!(text(&d, "#panel").await, "Panel B");

    // Nothing happening: done after the reaction window, not the cap.
    let start = Instant::now();
    let r = d.settle_on_device("web", 200, 100, 3000).await.unwrap();
    assert!(r.settled && !r.reacted, "{r:?}");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    d.cleanup().await;
}

const STYLED: &str = "<!doctype html><html><body>\
<button id=\"go\" style=\"text-transform:uppercase\">Submit</button>\
<a id=\"nav\" style=\"text-transform:capitalize\">my orders</a>\
<p id=\"price\">10,00\u{202F}\u{20AC}</p>\
<p id=\"usd\">$\u{A0}5</p>\
<p id=\"zwj\">Team \u{1F468}\u{200D}\u{1F469}</p>\
<p id=\"multi\">first line\n   second line</p>\
</body></html>";

/// B-1: `innerText` applies CSS `text-transform`, so an uppercase-styled
/// button (MUI's default) read "SUBMIT" and `tapOn: "Submit"` /
/// `assertText … "Submit"` stopped matching. Either the rendered or the
/// source text matches now.
#[tokio::test]
async fn text_transform_does_not_hide_the_source_text() {
    require_chrome!();
    let base = serve(&[("/s.html", STYLED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    for (needle, id) in [("Submit", "go"), ("SUBMIT", "go"), ("my orders", "nav")] {
        let el = d
            .find_element("web", &text_sel(needle))
            .await
            .unwrap_or_else(|e| panic!("{needle}: {e}"));
        assert_eq!(
            el.platform_id,
            find(&d, &format!("#{id}")).await.platform_id
        );
    }
    let el = d
        .find_element("web", &Selector::TextContains("Subm".into()))
        .await
        .expect("textContains on the source text");
    assert_eq!(el.element_type, "button");
    let go = find(&d, "#go").await;
    for want in ["Submit", "SUBMIT"] {
        let (hit, actual) = d.element_text_matches("web", &go, want).await.unwrap();
        assert!(hit, "assertText {want:?} (read {actual:?})");
    }
    let (hit, _) = d.element_text_matches("web", &go, "Cancel").await.unwrap();
    assert!(!hit);
    d.cleanup().await;
}

/// M-1: `assertText` normalises the expected value the way the page text
/// is (NBSP / U+202F and other space runs, zero-width characters, NFC), so
/// an `Intl.NumberFormat` price or a YAML block scalar that `text:` finds
/// also passes `assertText` on the same element.
#[tokio::test]
async fn assert_text_normalises_the_expected_value_like_the_page() {
    require_chrome!();
    let base = serve(&[("/s.html", STYLED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    for (id, want) in [
        ("price", "10,00\u{202F}\u{20AC}"),
        ("price", "10,00 \u{20AC}"),
        ("usd", "$\u{A0}5"),
        ("zwj", "Team \u{1F468}\u{200D}\u{1F469}"),
        ("multi", "first line\nsecond line\n"),
    ] {
        let el = find(&d, &format!("#{id}")).await;
        let (hit, actual) = d.element_text_matches("web", &el, want).await.unwrap();
        assert!(hit, "#{id}: {want:?} vs {actual:?}");
        // `text:` finds it by the same value.
        d.find_element("web", &text_sel(want))
            .await
            .unwrap_or_else(|e| panic!("text: {want:?}: {e}"));
    }
    d.cleanup().await;
}

/// M-2: `goBack` after following a link to another origin (OAuth, a
/// payment page) goes back. The Navigation API's `canGoBack` only sees
/// same-origin entries and said there was nothing to go back to.
#[tokio::test]
async fn go_back_works_across_origins() {
    require_chrome!();
    let a = serve(&[("/a.html", NAV_A)]);
    let b = serve(&[("/b.html", NAV_B)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{a}/a.html"), false)
        .await
        .unwrap();
    d.navigate("web", &format!("{b}/b.html")).await.unwrap();
    assert_eq!(text(&d, "#where").await, "b");
    d.go_back("web").await.expect("back to the other origin");
    assert_eq!(wait_text(&d, "#where", "a").await, "a");
    // And at the start of the history it still fails at once.
    let err = d.go_back("web").await.unwrap_err();
    assert!(matches!(err, AtalantaError::CommandFailed { .. }), "{err}");
    d.cleanup().await;
}

const TICKING: &str = r#"<!doctype html><html><body>
<div id="n">0</div><div id="bar" style="width:0px;height:4px;background:red"></div>
<button id="tb" onclick="later()">Tab B</button><div id="panel">Panel A</div>
<script>
var i = 0;
(function f(){ i++; document.getElementById('n').textContent = String(i);
  document.getElementById('bar').style.width = (i % 300) + 'px';
  requestAnimationFrame(f); })();
function later(){ setTimeout(function(){ document.getElementById('panel').textContent = 'Panel B'; }, 120); }
</script></body></html>"#;

/// M-3: a page with a script-driven endless animation (a counter, a
/// progress bar written every frame) settles instead of running every
/// action to the cap, still waits for a real reaction, and a static page
/// costs about nothing.
#[tokio::test]
async fn settle_ignores_script_animation_and_is_cheap_on_a_static_page() {
    require_chrome!();
    let base = serve(&[("/t.html", TICKING), ("/s.html", STYLED)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let start = Instant::now();
    let r = d.settle_on_device("web", 30, 150, 4000).await.unwrap();
    assert!(r.settled, "{r:?}");
    assert!(
        start.elapsed() < Duration::from_millis(2500),
        "ran towards the cap: {:?}",
        start.elapsed()
    );
    // A real reaction on the same page is still waited for.
    let tb = find(&d, "#tb").await;
    d.tap("web", &tb).await.unwrap();
    let r = d.settle_on_device("web", 300, 100, 4000).await.unwrap();
    assert!(r.settled && r.reacted, "{r:?}");
    assert_eq!(text(&d, "#panel").await, "Panel B");

    // A static page: the reaction window and nothing after it.
    d.navigate("web", &format!("{base}/s.html")).await.unwrap();
    let start = Instant::now();
    for _ in 0..10 {
        let r = d.settle_on_device("web", 30, 150, 1000).await.unwrap();
        assert!(r.settled && !r.reacted, "{r:?}");
    }
    assert!(
        start.elapsed() < Duration::from_millis(2000),
        "10 settles on a static page took {:?}",
        start.elapsed()
    );
    d.cleanup().await;
}
