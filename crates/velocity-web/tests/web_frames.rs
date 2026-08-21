//! W4 (D-36 c′): cross-origin iframes, against a real headless Chrome and
//! a two-origin fixture: the top page on `127.0.0.1:<a>`, the frame on
//! `localhost:<b>` (another site, so Chrome runs it out of process).

mod common;

use std::time::{Duration, Instant};

use common::{css, driver, serve};
use velocity_common::{PlatformDriver, Selector};

/// The frame: a button that logs where the click landed (in the frame's
/// own coordinates) and starts a slow request, a card field, a hidden
/// element.
const INNER: &str = r#"<!doctype html><html><body style="margin:0">
<button id="pay" style="position:absolute;left:20px;top:20px;width:100px;height:40px"
  onclick="document.getElementById('log').textContent='clicked '+event.clientX+','+event.clientY;
           fetch('/slow-api.json').then(r=>r.text()).then(t=>document.getElementById('api').textContent=t)">Pay now</button>
<input id="card" style="position:absolute;left:20px;top:80px;width:200px;height:30px" placeholder="Card number">
<div id="log" style="position:absolute;left:20px;top:130px">idle</div>
<div id="api" style="position:absolute;left:20px;top:160px">none</div>
<div id="secret" style="display:none">hidden</div>
</body></html>"#;

/// The top page: the frame at (50, 100), its border 0.
fn outer(frame_port: &str) -> String {
    format!(
        "<!doctype html><html><body style=\"margin:0\"><h1 style=\"margin:0;height:40px\">Checkout</h1>\
         <iframe id=\"f\" src=\"http://localhost:{frame_port}/inner.html\" \
         style=\"position:absolute;left:50px;top:100px;width:400px;height:300px;border:0\"></iframe>\
         </body></html>"
    )
}

/// The frame's server, the top page's server and a driver on the top page,
/// with the frame loaded.
async fn setup() -> velocity_web::WebDriver {
    let inner = serve(&[("/inner.html", INNER), ("/slow-api.json", "{\"ok\":1}")]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let page = outer(&port);
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    d
}

/// Finds `s` (waiting for the frame to load, up to 5 s).
async fn find_in_frame(d: &velocity_web::WebDriver, s: &Selector) -> velocity_common::Element {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match d.find_element("web", s).await {
            Ok(e) => return e,
            Err(e) if Instant::now() > deadline => panic!("{s}: never found: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn text_in_frame(d: &velocity_web::WebDriver, s: &str) -> String {
    let el = find_in_frame(d, &css(s)).await;
    d.get_element_text("web", &el).await.unwrap()
}

/// W4: an element inside a cross-origin iframe is found (by CSS and by
/// text), with its box in the top page's coordinates; `assertVisible`
/// sees it, and a hidden one isn't visible. Before, the frame was an
/// opaque box: no selector reached into it.
#[tokio::test]
async fn elements_in_a_cross_origin_frame_are_found_and_visible() {
    require_chrome!();
    let d = setup().await;
    let pay = find_in_frame(&d, &css("#pay")).await;
    assert!(pay.platform_id.starts_with('@'), "{}", pay.platform_id);
    // (20, 20) in the frame, the frame at (50, 100).
    assert_eq!((pay.bounds.x, pay.bounds.y), (70, 120), "{:?}", pay.bounds);
    assert_eq!((pay.bounds.width, pay.bounds.height), (100, 40));
    let by_text = find_in_frame(&d, &Selector::Text("Pay now".into())).await;
    assert_eq!(by_text.bounds, pay.bounds);
    assert!(d.is_element_visible("web", &pay).await.unwrap());
    let all = d.find_elements("web", &css("input")).await.unwrap();
    assert_eq!(all.len(), 1, "the frame's input, through findAll");
    assert_eq!(all[0].hint.as_deref(), Some("Card number"));
    let secret = d.find_element("web", &css("#secret")).await.unwrap();
    assert!(!d.is_element_visible("web", &secret).await.unwrap());
    d.cleanup().await;
}

/// W4: a tap on it lands on it: the click point is translated to the top
/// page and Chrome routes it into the frame (the frame logs where, in its
/// own coordinates: the button's centre).
#[tokio::test]
async fn a_tap_in_a_cross_origin_frame_lands_on_its_target() {
    require_chrome!();
    let d = setup().await;
    let pay = find_in_frame(&d, &css("#pay")).await;
    d.tap("web", &pay).await.unwrap();
    let mut got = String::new();
    for _ in 0..40 {
        got = text_in_frame(&d, "#log").await;
        if got != "idle" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(got, "clicked 70,40", "the click's point in the frame");
    d.cleanup().await;
}

/// W4: `inputText` into a field in a cross-origin frame types into it,
/// and the focus check runs in the frame (it is editable, not "unknown").
#[tokio::test]
async fn input_text_into_a_cross_origin_frame() {
    require_chrome!();
    let d = setup().await;
    let card = find_in_frame(&d, &css("#card")).await;
    d.input_text("web", &card, "4242 4242").await.unwrap();
    assert_eq!(text_in_frame(&d, "#card").await, "4242 4242");
    // Appended at the end, as on the page.
    let card = find_in_frame(&d, &css("#card")).await;
    d.input_text("web", &card, " 42").await.unwrap();
    assert_eq!(text_in_frame(&d, "#card").await, "4242 4242 42");
    assert!(
        d.element_text_matches("web", &card, "4242 4242 42")
            .await
            .unwrap()
            .0
    );
    d.cleanup().await;
}

/// W4: the frame's own requests count for the network-idle wait: its
/// 3 s request keeps the page busy, and its end is seen.
#[tokio::test]
async fn a_cross_origin_frames_requests_keep_the_network_busy() {
    require_chrome!();
    let d = setup().await;
    let pay = find_in_frame(&d, &css("#pay")).await;
    // The frame's load is over.
    let start = Instant::now();
    while d.network_busy("web").await && start.elapsed() < Duration::from_secs(6) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!d.network_busy("web").await, "busy before the tap");
    d.tap("web", &pay).await.unwrap();
    assert!(
        common::eventually(|| async { d.network_busy("web").await }).await,
        "the frame's request was not seen"
    );
    let start = Instant::now();
    while d.network_busy("web").await && start.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let waited = start.elapsed();
    assert!(
        waited > Duration::from_millis(1500) && waited < Duration::from_millis(4500),
        "busy for {waited:?}, not about the request's 3 s"
    );
    assert_eq!(text_in_frame(&d, "#api").await, "{\"ok\":1}");
    d.cleanup().await;
}

/// The top page with a cookie banner over the lower part of the frame:
/// the frame's "Pay now" button (page y 120..160) is under it.
fn outer_with_banner(frame_port: &str) -> String {
    format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <iframe id=\"f\" src=\"http://localhost:{frame_port}/inner.html\" \
         style=\"position:absolute;left:50px;top:100px;width:400px;height:300px;border:0\"></iframe>\
         <div id=\"cookies\" style=\"position:fixed;left:0;right:0;top:110px;height:80px;\
         background:#333;color:#fff;z-index:10\">We use cookies</div>\
         </body></html>"
    )
}

/// review-i2 M1: a cookie banner on the top page covers the frame's
/// button. The tap fails "obscured by <div>", as it does for an element
/// of the page itself, and the frame never sees a click. Before, the
/// frame's own hit test (which can't see the page) passed it and the
/// click went to the banner: the step passed and nothing was paid.
#[tokio::test]
async fn a_tap_on_a_frame_element_under_a_page_banner_fails() {
    require_chrome!();
    let inner = serve(&[("/inner.html", INNER), ("/slow-api.json", "{\"ok\":1}")]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let page = outer_with_banner(&port);
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    let pay = find_in_frame(&d, &css("#pay")).await;
    let err = d
        .tap("web", &pay)
        .await
        .expect_err("the banner takes the click");
    assert!(err.to_string().contains("obscured by <div>"), "{err}");
    assert_eq!(
        text_in_frame(&d, "#log").await,
        "idle",
        "no click reached it"
    );
    // An element of the frame outside the banner is still tapped.
    let card = find_in_frame(&d, &css("#card")).await;
    d.tap("web", &card).await.unwrap();
    d.cleanup().await;
}

/// A frame that hangs (an endless loop) a moment after it loads.
const HUNG: &str = r#"<!doctype html><html><body><button id="b">In the frame</button>
<script>setTimeout(function () { for (;;) {} }, 300);</script></body></html>"#;

/// review-i2 M2: a cross-origin frame whose renderer never answers. A
/// lookup that falls back to the frames is bounded (about 2 s for all of
/// them), and once the frame has timed out it is left out: the next
/// lookups don't wait on it. Before, each lookup waited 4 x 10 s on it.
#[tokio::test]
async fn a_hung_frame_does_not_stall_lookups() {
    require_chrome!();
    let inner = serve(&[("/hung.html", HUNG)]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let page = format!(
        "<!doctype html><html><body><h1 id=\"t\">Top</h1>\
         <iframe src=\"http://localhost:{port}/hung.html\"></iframe></body></html>"
    );
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let missing = css("#nowhere");
    let lookup = |limit: Duration| {
        let d = &d;
        let missing = &missing;
        async move {
            let start = Instant::now();
            let r = tokio::time::timeout(limit, d.find_element("web", missing)).await;
            (r.is_ok(), start.elapsed())
        }
    };
    let (done, first) = lookup(Duration::from_secs(8)).await;
    assert!(done, "the first lookup hung on the frame");
    assert!(
        first < Duration::from_secs(4),
        "first lookup took {first:?}"
    );
    let (done, second) = lookup(Duration::from_secs(8)).await;
    assert!(
        done && second < Duration::from_millis(1000),
        "second lookup took {second:?}"
    );
    // The page itself still answers.
    assert!(d.find_element("web", &css("#t")).await.is_ok());
    d.cleanup().await;
}

/// review-i2 m1: two card frames. After the focus moved to frame B, a
/// click on A's field that doesn't take the focus (its mousedown is
/// cancelled) leaves the keys going to B. `inputText` into A is refused
/// and nothing lands in B's field. (Chrome itself resets a blurred
/// frame's `activeElement` to <body>, so this also held before the
/// `document.hasFocus()` guard in `caretToEnd`; the guard covers a
/// browser or page that keeps it.)
#[tokio::test]
async fn typing_into_a_frame_that_lost_the_focus_is_refused() {
    require_chrome!();
    const A: &str = r#"<!doctype html><html><body style="margin:0">
<input id="a" style="width:300px;height:40px"
  onmousedown="window.n=(window.n||0)+1; if (window.n>1) event.preventDefault()"></body></html>"#;
    const B: &str = r#"<!doctype html><html><body style="margin:0">
<input id="b" style="width:300px;height:40px"></body></html>"#;
    let a = serve(&[("/a.html", A)]);
    let b = serve(&[("/b.html", B)]);
    let (pa, pb) = (
        a.rsplit(':').next().unwrap().to_string(),
        b.rsplit(':').next().unwrap().to_string(),
    );
    let page = format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <iframe src=\"http://localhost:{pa}/a.html\" style=\"display:block;width:320px;height:60px;border:0\"></iframe>\
         <iframe src=\"http://localhost:{pb}/b.html\" style=\"display:block;width:320px;height:60px;border:0\"></iframe>\
         </body></html>"
    );
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    let fa = find_in_frame(&d, &css("#a")).await;
    d.input_text("web", &fa, "4242").await.unwrap();
    let fb = find_in_frame(&d, &css("#b")).await;
    d.input_text("web", &fb, "12/30").await.unwrap();
    let fa = find_in_frame(&d, &css("#a")).await;
    let err = d
        .input_text("web", &fa, "99")
        .await
        .expect_err("the focus stayed in frame B");
    assert!(err.to_string().contains("focus"), "{err}");
    assert_eq!(
        text_in_frame(&d, "#b").await,
        "12/30",
        "typed into the wrong frame"
    );
    assert_eq!(text_in_frame(&d, "#a").await, "4242");
    d.cleanup().await;
}

/// A frame that blocks its renderer for `ms` (a long task), 600 ms after
/// its "Block" button is clicked; "Payment failed" is shown in it.
fn blocking_frame(ms: u64) -> String {
    format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <button id=\"block\" style=\"width:120px;height:40px\" \
         onclick=\"setTimeout(function () {{ var t = Date.now(); while (Date.now() - t < {ms}) {{}} }}, 600)\">Block</button>\
         <div id=\"msg\">Payment failed</div></body></html>"
    )
}

/// The top page with `frame` (HTML) in a cross-origin <iframe>, launched;
/// the frame's library installed (its "Block" button found).
async fn with_frame(frame: &str) -> velocity_web::WebDriver {
    let inner = serve(&[("/f.html", frame)]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let page = format!(
        "<!doctype html><html><body><h1 id=\"t\">Top</h1>\
         <iframe src=\"http://localhost:{port}/f.html\" style=\"width:400px;height:200px\"></iframe></body></html>"
    );
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    find_in_frame(&d, &css("#block")).await;
    d
}

/// Clicks the frame's "Block" button and waits until its renderer is in
/// the long task.
async fn block(d: &velocity_web::WebDriver) {
    let b = find_in_frame(d, &css("#block")).await;
    d.tap("web", &b).await.unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
}

/// review-fi2 F1: one lookup that runs into a frame's long task (3 s,
/// over its 2 s share) is slowness, not a hang: once the task is over,
/// the frame is searched again and its element found. Before, that one
/// overrun left the frame out of every later lookup until it navigated
/// (a Stripe or captcha frame never does): "not found" with the element
/// on screen.
#[tokio::test]
async fn a_frame_slow_once_is_still_searched() {
    require_chrome!();
    let d = with_frame(&blocking_frame(3000)).await;
    block(&d).await;
    let _ = d.find_element("web", &css("#nowhere")).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let found = d.find_element("web", &css("#msg")).await;
    assert!(found.is_ok(), "the frame was left out: {:?}", found.err());
    d.cleanup().await;
}

/// review-fi2 F1: while a frame doesn't answer, a lookup that finds
/// nothing elsewhere is unknown, not "not found": the error names the
/// frame, and `assertNotVisible` (which passes only on `ElementNotFound`)
/// keeps reading instead of passing while the frame shows "Payment
/// failed". Before, it was `ElementNotFound`.
#[tokio::test]
async fn a_miss_while_a_frame_does_not_answer_is_not_not_found() {
    require_chrome!();
    let d = with_frame(&blocking_frame(3500)).await;
    block(&d).await;
    let err = d
        .find_element("web", &css("#msg"))
        .await
        .expect_err("the frame didn't answer");
    assert!(
        !matches!(err, velocity_common::VelocityError::ElementNotFound { .. }),
        "a skipped frame read as not found: {err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("frame") && text.contains("localhost"),
        "{text}"
    );
    // Once the frame answers again, it is there.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(d.find_element("web", &css("#msg")).await.is_ok());
    d.cleanup().await;
}

/// review-fi2 F1: a frame that hangs for good after it loaded
/// (`for(;;){}`) costs each lookup at most its share, and after
/// three overruns in a row it is left out: lookups are quick again,
/// and stay quick when the mark expires (a ping, not a lookup).
#[tokio::test]
async fn a_frame_that_hangs_after_loading_stops_costing_lookups() {
    require_chrome!();
    const HANGS: &str = r#"<!doctype html><html><body>
<button id="block" style="width:120px;height:40px"
  onclick="setTimeout(function () { for (;;) {} }, 600)">Block</button></body></html>"#;
    let d = with_frame(HANGS).await;
    block(&d).await;
    let missing = css("#nowhere");
    let mut took = Vec::new();
    for _ in 0..6 {
        let start = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(8), d.find_element("web", &missing)).await;
        assert!(r.is_ok(), "a lookup hung on the frame");
        took.push(start.elapsed());
    }
    assert!(took.iter().all(|t| *t < Duration::from_secs(3)), "{took:?}");
    assert!(
        took[3..].iter().all(|t| *t < Duration::from_millis(1000)),
        "still waiting on the hung frame: {took:?}"
    );
    tokio::time::sleep(Duration::from_millis(3200)).await;
    let start = Instant::now();
    let _ = d.find_element("web", &missing).await;
    assert!(
        start.elapsed() < Duration::from_millis(1000),
        "the re-check took {:?}",
        start.elapsed()
    );
    assert!(d.find_element("web", &css("#t")).await.is_ok());
    d.cleanup().await;
}

/// review-fi2 F3: a tap on an <iframe> inside a cross-origin frame. The
/// click is dispatched in the inner frame's document, so the outer
/// frame's window never sees it; that is not a missed click. Before, the
/// tap failed "the click never reached the frame" after three tries.
#[tokio::test]
async fn a_tap_on_an_iframe_inside_a_cross_origin_frame() {
    require_chrome!();
    const NESTED: &str = r#"<!doctype html><html><body style="margin:0">
<div id="log">idle</div>
<iframe id="inner" src="/leaf.html" style="display:block;width:200px;height:100px;border:0"></iframe>
</body></html>"#;
    const LEAF: &str = r#"<!doctype html><html><body style="margin:0;height:100px"
  onclick="parent.document.getElementById('log').textContent='leaf clicked'">Leaf</body></html>"#;
    let inner = serve(&[("/outer-frame.html", NESTED), ("/leaf.html", LEAF)]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let page = format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <iframe src=\"http://localhost:{port}/outer-frame.html\" \
         style=\"position:absolute;left:30px;top:40px;width:400px;height:300px;border:0\"></iframe></body></html>"
    );
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    let target = find_in_frame(&d, &css("#inner")).await;
    assert!(
        target.platform_id.starts_with('@'),
        "{}",
        target.platform_id
    );
    d.tap("web", &target).await.unwrap();
    assert_eq!(text_in_frame(&d, "#log").await, "leaf clicked");
    d.cleanup().await;
}

/// Serves a document whose head arrives at once and whose end never does
/// (the connection is held for 8 s): the frame commits, and its
/// document request stays open. Returns the port.
fn serve_streaming() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let head = "<!doctype html><html><body><p id=\"part\">part</p>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: 100000\r\nConnection: close\r\n\r\n{head}{}",
                    " ".repeat(2048)
                );
                let _ = stream.flush();
                std::thread::sleep(Duration::from_secs(8));
            });
        }
    });
    port
}

/// review-fi2 F7: a cross-origin frame removed while its document is
/// still loading (an ad frame dropped by an SPA route change). Its
/// document request, counted on the page's session while the frames are
/// attached, ends with it: the page goes idle long before the document
/// would have ended (8 s) or `networkIdleMaxMs`.
#[tokio::test]
async fn a_frame_removed_while_loading_does_not_keep_the_network_busy() {
    require_chrome!();
    let port = serve_streaming();
    let page = format!(
        "<!doctype html><html><body><h1 id=\"t\">Top</h1>\
         <script>window.addEventListener('load', function () {{ setTimeout(function () {{\
         var f = document.createElement('iframe'); f.id = 'ad'; f.src = 'http://localhost:{port}/doc.html';\
         document.body.appendChild(f); }}, 100); }});</script>\
         <button id=\"drop\" onclick=\"document.getElementById('ad').remove()\">Drop</button>\
         </body></html>"
    );
    let top = serve(&[("/outer.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{top}/outer.html"), false)
        .await
        .unwrap();
    // The frame committed (its partial document is searchable) and its
    // document is still loading.
    find_in_frame(&d, &css("#part")).await;
    assert!(
        d.network_busy("web").await,
        "the frame's document isn't counted"
    );
    let drop = d.find_element("web", &css("#drop")).await.unwrap();
    d.tap("web", &drop).await.unwrap();
    let start = Instant::now();
    let idle_at = loop {
        if !d.network_busy("web").await {
            break start.elapsed();
        }
        assert!(start.elapsed() < Duration::from_secs(12), "never idle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        idle_at < Duration::from_millis(1500),
        "busy for {idle_at:?} after the frame went: its document kept the page busy"
    );
    d.cleanup().await;
}
