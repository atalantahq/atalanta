//! Where taps land (VEL-341, VEL-344), against a fake WDA on 127.0.0.1:
//! screen 402×874, keyboard from y 538, status bar 62 pt (from /wda/screen).
mod common;

use std::sync::{Arc, Mutex};

use common::*;
use velocity_common::{Key, PlatformDriver, Selector};

fn screen_xml() -> String {
    app(&[
        node("TextField", "Email", 20, 100, 362, 44),
        node("StaticText", "Top row", 0, -30, 402, 80),
        node("Button", "Submit", 20, 560, 362, 50),
        node("Button", "Half", 20, 500, 362, 80),
        r#"<XCUIElementTypeKeyboard type="XCUIElementTypeKeyboard" x="0" y="538" width="402" height="336" enabled="true"><XCUIElementTypeKey type="XCUIElementTypeKey" label="g" x="181" y="566" width="40" height="42" enabled="true"/></XCUIElementTypeKeyboard>"#.to_string(),
        node("StaticText", "Bottom row", 0, 850, 402, 100),
    ]
    .concat())
}

async fn fake(xml: String) -> FakeWda {
    fake_on(Arc::new(Mutex::new(xml))).await
}

/// A fake whose screen is whatever `xml` holds at the time of the read.
async fn fake_on(xml: Arc<Mutex<String>>) -> FakeWda {
    FakeWda::start(move |r| {
        let xml = xml.lock().unwrap().clone();
        if r.path == "/status" {
            return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S"}"#.into());
        }
        if r.path.ends_with("/source") {
            return source(&xml);
        }
        if r.path.ends_with("/wda/screen") {
            return ok(r#"{"statusBarSize":{"width":402,"height":62}}"#);
        }
        if r.path.ends_with("/window/size") {
            return ok(r#"{"width":402,"height":874}"#);
        }
        if r.path.contains("alert") {
            return w3c(404, "no such alert", "none");
        }
        ok("null")
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taps_never_land_on_the_keyboard_the_status_bar_or_off_screen() {
    let f = fake(screen_xml()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Submit".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &el).await.unwrap_err().to_string();
    assert!(e.contains("covered by the keyboard"), "{e}");
    // Review minor 2: `pressKey: back` navigates away on iOS.
    assert!(
        !e.contains("pressKey: back") && e.contains("dismiss the keyboard"),
        "{e}"
    );
    assert!(f.taps().is_empty(), "nothing tapped: {:?}", f.taps());
    // VEL-341: on screen only under the status bar, where a tap scrolls to
    // the top instead: refused loudly, with the opt-out named.
    let top = d
        .find_element(SIM, &Selector::Text("Top row".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &top).await.unwrap_err().to_string();
    assert!(e.contains("covered by the status bar"), "{e}");
    assert!(e.contains("VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off"), "{e}");
    assert!(f.taps().is_empty(), "nothing tapped: {:?}", f.taps());
    // Partly free: tapped on the free part.
    let half = d
        .find_element(SIM, &Selector::Text("Half".into()))
        .await
        .unwrap();
    d.tap(SIM, &half).await.unwrap();
    let (_, y) = f.taps()[0];
    assert!((500.0..538.0).contains(&y), "{y}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bottom_row_is_tapped_on_screen_when_nothing_covers_it() {
    let f = fake(app(&node("StaticText", "Bottom row", 0, 850, 402, 100))).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Bottom row".into()))
        .await
        .unwrap();
    d.tap(SIM, &el).await.unwrap();
    let (_, y) = f.taps()[0];
    assert!((850.0..874.0).contains(&y), "{y}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_never_presses_a_right_hand_nav_bar_button() {
    // VEL-344: a root screen whose bar holds only "Delete" on the right.
    let xml = app(&format!(
        r#"<XCUIElementTypeNavigationBar type="XCUIElementTypeNavigationBar" label="Inbox" x="0" y="62" width="402" height="44" enabled="true">{}</XCUIElementTypeNavigationBar>"#,
        node("Button", "Delete", 330, 66, 64, 36)
    ));
    let f = fake(xml).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    d.press_key(SIM, Key::Back).await.unwrap();
    assert_eq!(f.count("POST", "/click"), 0, "no element click");
    assert_eq!(f.count("POST", "/element"), 0, "no nav-bar button lookup");
    let (x, _) = f.taps()[0];
    assert!(x < 10.0, "edge swipe from the left, got x={x}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_drops_the_portrait_status_bar() {
    // Review M2: the portrait status bar (62 pt) was cached for the
    // session, and pushed taps in landscape below it.
    let xml = Arc::new(Mutex::new(app(&node("Button", "Tall", 0, 0, 402, 100))));
    let f = fake_on(xml.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Tall".into()))
        .await
        .unwrap();
    d.tap(SIM, &el).await.unwrap();
    assert!(
        f.taps()[0].1 >= 62.0,
        "portrait: below the bar, {:?}",
        f.taps()
    );
    *xml.lock().unwrap() = format!(
        r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="874" height="402" enabled="true">{}</XCUIElementTypeApplication>"#,
        node("Button", "Tall", 0, 0, 874, 100)
    );
    let el = d
        .find_element(SIM, &Selector::Text("Tall".into()))
        .await
        .unwrap();
    d.tap(SIM, &el).await.unwrap();
    assert_eq!(f.taps()[1], (437.0, 50.0), "landscape: the centre");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_field_reads_as_empty_not_as_its_label() {
    // VEL-354: a cleared field read as its label ("Email"), so
    // `assertText: ""` failed only on iOS.
    let f = fake(app(&[
        node("TextField", "Email", 20, 100, 362, 44),
        r#"<XCUIElementTypeTextField type="XCUIElementTypeTextField" label="Name" value="Ada" x="20" y="200" width="362" height="44" enabled="true"/>"#.to_string(),
        node("StaticText", "Welcome", 20, 300, 362, 44),
    ]
    .concat()))
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let text = |label: &'static str| {
        let d = &d;
        async move {
            let el = d
                .find_element(SIM, &Selector::Text(label.into()))
                .await
                .unwrap();
            d.get_element_text(SIM, &el).await.unwrap()
        }
    };
    assert_eq!(text("Email").await, "");
    assert_eq!(text("Name").await, "Ada");
    assert_eq!(
        text("Welcome").await,
        "Welcome",
        "static text keeps its label"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_opt_in_focus_check_refuses_to_type_into_a_label_or_an_unfocused_field() {
    // VEL-S46: typing went to whatever field already had focus.
    let f = fake(app(&[
        node("StaticText", "Email", 20, 60, 362, 30),
        node("TextField", "Name", 20, 100, 362, 44),
    ]
    .concat()))
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_focus_check(true);
    let label = d
        .find_element(SIM, &Selector::Text("Email".into()))
        .await
        .unwrap();
    let e = d
        .input_text(SIM, &label, "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("not a text field"), "{e}");
    assert!(f.taps().is_empty(), "not tapped");
    // A field, but no keyboard after the tap: no keys.
    let field = d
        .find_element(SIM, &Selector::Text("Name".into()))
        .await
        .unwrap();
    let e = d
        .input_text(SIM, &field, "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("did not take focus"), "{e}");
    assert_eq!(f.count("POST", "/wda/keys"), 0);
    // Off (the default): typed as before.
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    d.input_text(SIM, &field, "x").await.unwrap();
    assert_eq!(f.count("POST", "/wda/keys"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_focus_check_waits_for_a_keyboard_that_animates_in() {
    // Review m11: one read 100 ms after the tap missed a keyboard still
    // animating in (250-400 ms), so a good field was refused.
    let base = [
        node("TextField", "Name", 20, 100, 362, 44),
        node("StaticText", "Welcome", 20, 300, 362, 44),
    ]
    .concat();
    let xml = Arc::new(Mutex::new(app(&base)));
    let f = fake_on(xml.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_focus_check(true);
    let field = d
        .find_element(SIM, &Selector::Text("Name".into()))
        .await
        .unwrap();
    let log = f.log.clone();
    let keyboard = tokio::spawn(async move {
        // The keyboard appears 400 ms after the tap.
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/actions"))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        *xml.lock().unwrap() = app(&[base, node("Keyboard", "", 0, 500, 402, 300)].concat());
    });
    d.input_text(SIM, &field, "x").await.unwrap();
    keyboard.await.unwrap();
    assert_eq!(f.count("POST", "/wda/keys"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_ambiguous_selector_resolves_to_the_first_in_tree_order() {
    // N-1: iOS took the first match in reading order (the "Go" at y 100),
    // Android and every other driver the first in the tree (y 500); the
    // ambiguity note says "first in the tree". `index:` stays reading order.
    let f = fake(app(&[
        node("Button", "Go", 20, 500, 100, 40),
        node("Button", "Go", 20, 100, 100, 40),
    ]
    .concat()))
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Go".into()))
        .await
        .unwrap();
    assert_eq!(el.bounds.y, 500, "tree order");
    let first = d
        .find_element(
            SIM,
            &Selector::Index {
                selector: Box::new(Selector::Text("Go".into())),
                index: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.bounds.y, 100, "index: counts in reading order");
    let all = d
        .find_elements(SIM, &Selector::Text("Go".into()))
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
}

/// A field the runner reports as focused (VelocityRunner `focused="true"`).
fn focused_field(label: &str, y: i32) -> String {
    node("TextField", label, 20, y, 362, 44).replace("/>", r#" focused="true"/>"#)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_text_fails_when_the_runner_reports_focus_elsewhere() {
    // D-5 / VEL-S46: the "Email" label was tapped, the focus stayed on the
    // Name field, and the keys went into Name. By default now, with no
    // opt-in, no key is sent.
    let xml = Arc::new(Mutex::new(app(&[
        node("StaticText", "Email", 20, 300, 362, 30),
        focused_field("Name", 100),
    ]
    .concat())));
    let f = fake_on(xml.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let label = d
        .find_element(SIM, &Selector::Text("Email".into()))
        .await
        .unwrap();
    let e = d
        .input_text(SIM, &label, "x")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("focus stayed on TextField") && e.contains("Name"),
        "{e}"
    );
    assert_eq!(f.count("POST", "/wda/keys"), 0);
    // The field itself takes focus: typed.
    *xml.lock().unwrap() = app(&[
        node("StaticText", "Email", 20, 300, 362, 30),
        node("TextField", "Name", 20, 100, 362, 44),
        focused_field("Mail", 340),
    ]
    .concat());
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let field = d
        .find_element(SIM, &Selector::Text("Mail".into()))
        .await
        .unwrap();
    d.input_text(SIM, &field, "x").await.unwrap();
    assert_eq!(f.count("POST", "/wda/keys"), 1);
    // The escape hatch: no check.
    *xml.lock().unwrap() = app(&[
        node("StaticText", "Email", 20, 300, 362, 30),
        focused_field("Name", 100),
    ]
    .concat());
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_strict_focus(false);
    d.input_text(SIM, &label, "x").await.unwrap();
    assert_eq!(f.count("POST", "/wda/keys"), 2);
}

/// D-4: `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off` drops only the status bar
/// rule: the row under it is tapped, the keyboard still guards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_bar_rule_has_its_own_opt_out() {
    let f = fake(screen_xml()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_status_bar_guard(false);
    let top = d
        .find_element(SIM, &Selector::Text("Top row".into()))
        .await
        .unwrap();
    d.tap(SIM, &top).await.unwrap();
    let (_, y) = f.taps()[0];
    assert!((0.0..50.0).contains(&y), "tapped in the visible part: {y}");
    let submit = d
        .find_element(SIM, &Selector::Text("Submit".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &submit).await.unwrap_err().to_string();
    assert!(e.contains("covered by the keyboard"), "{e}");
}

/// D-9: backspace and tab are typed through WDA's keyboard; escape,
/// forward delete and the arrows (hardware-keyboard keys WDA can't press)
/// are a config error that reaches WDA not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_keys_are_typed_and_hardware_only_keys_refused() {
    let f = fake(app(&node("TextField", "Email", 20, 100, 362, 44))).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    d.press_key(SIM, Key::Backspace).await.unwrap();
    d.press_key(SIM, Key::Tab).await.unwrap();
    let typed: Vec<String> = f
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.ends_with("/wda/keys"))
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(typed.len(), 2, "{typed:?}");
    assert!(typed[0].contains(r#""\b""#), "{}", typed[0]);
    assert!(typed[1].contains(r#""\t""#), "{}", typed[1]);
    for key in [Key::Escape, Key::Delete, Key::ArrowUp] {
        let e = d.press_key(SIM, key.clone()).await.unwrap_err();
        assert!(
            matches!(e, velocity_common::VelocityError::Config(_)),
            "{key:?}: {e}"
        );
    }
    assert_eq!(f.count("POST", "/wda/keys"), 2, "nothing more was typed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_field_that_slides_into_the_nav_bar_on_focus_takes_the_keys() {
    // Review BLOCKER 2: UISearchController moves its field up into the
    // navigation bar when it takes focus; D-5 compared the focus with the
    // field's rectangle from before the tap and refused to type.
    let before = [
        node("SearchField", "Search", 20, 140, 362, 36),
        node("Cell", "Row", 0, 200, 402, 44),
    ]
    .concat();
    let xml = Arc::new(Mutex::new(app(&before)));
    let f = fake_on(xml.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_strict_focus(true);
    let field = d
        .find_element(SIM, &Selector::Text("Search".into()))
        .await
        .unwrap();
    let log = f.log.clone();
    let moved = tokio::spawn(async move {
        while !log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/actions"))
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        *xml.lock().unwrap() = app(&[
            r#"<XCUIElementTypeSearchField type="XCUIElementTypeSearchField" label="Search" x="20" y="50" width="362" height="36" enabled="true" hasFocus="true"/>"#,
            &node("Keyboard", "", 0, 500, 402, 300),
        ]
        .concat());
    });
    d.input_text(SIM, &field, "x").await.unwrap();
    moved.await.unwrap();
    assert_eq!(f.count("POST", "/wda/keys"), 1);
}

/// D-38: a fake runner whose `/velocity/hitTest` answers `hit` (`None`:
/// WebDriverAgent's 404 for a route it doesn't have).
async fn fake_hit(xml: String, hit: Arc<Mutex<Option<String>>>) -> FakeWda {
    FakeWda::start(move |r| {
        if r.path == "/status" {
            return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S"}"#.into());
        }
        if r.path.ends_with("/velocity/hitTest") {
            return match hit.lock().unwrap().clone() {
                Some(v) => ok(&v),
                None => w3c(404, "unknown command", "POST /velocity/hitTest"),
            };
        }
        if r.path.ends_with("/source") {
            return source(&xml);
        }
        if r.path.ends_with("/wda/screen") {
            return ok(r#"{"statusBarSize":{"width":402,"height":62}}"#);
        }
        if r.path.ends_with("/window/size") {
            return ok(r#"{"width":402,"height":874}"#);
        }
        if r.path.contains("alert") {
            return w3c(404, "no such alert", "none");
        }
        ok("null")
    })
    .await
}

fn hit(owner: &str, ty: &str, label: &str, frame: (i32, i32, i32, i32)) -> Option<String> {
    Some(format!(
        r#"{{"available":true,"owner":"{owner}","pid":42,"element":{{"type":"{ty}","label":"{label}","identifier":"","x":{},"y":{},"width":{},"height":{}}}}}"#,
        frame.0, frame.1, frame.2, frame.3
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_refuses_a_tap_something_else_is_drawn_over() {
    // "Pay" at 20,400 200×50, with a dimming view the frame rule can't see.
    let xml = app(&[
        node("Button", "Pay", 20, 400, 200, 50),
        node("Other", "Dimming", 0, 0, 402, 874),
    ]
    .concat());
    let answer = Arc::new(Mutex::new(hit("app", "Other", "Dimming", (0, 0, 402, 874))));
    let f = fake_hit(xml, answer.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let pay = d
        .find_element(SIM, &Selector::Text("Pay".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
    assert!(
        e.contains("would land on Other") && e.contains("Dimming") && e.contains("ios.verifyTap"),
        "{e}"
    );
    assert!(f.taps().is_empty(), "nothing tapped: {:?}", f.taps());

    // A system alert (SpringBoard) on top: refused too.
    *answer.lock().unwrap() = hit("springboard", "Alert", "Allow?", (40, 300, 322, 200));
    let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
    assert!(
        e.contains("belongs to the system (Alert") && e.contains("Allow?"),
        "{e}"
    );
    assert!(f.taps().is_empty());

    // The button itself (or its label) at the point: tapped.
    *answer.lock().unwrap() = hit("app", "StaticText", "Pay", (30, 410, 60, 30));
    d.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.taps().len(), 1);

    // WebDriverAgent has no hit test: the frame rule alone, as before.
    *answer.lock().unwrap() = None;
    d.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.taps().len(), 2);

    // ios.verifyTap off (the default): no hit test is asked for.
    let before = f.count("POST", "/velocity/hitTest");
    let plain = velocity_ios::IosDriver::with_wda_url(&f.url);
    *answer.lock().unwrap() = hit("app", "Other", "Dimming", (0, 0, 402, 874));
    plain.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.count("POST", "/velocity/hitTest"), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_lets_a_tap_under_a_hidden_status_bar_through() {
    // Only the part under the (reported) 62 pt status bar is on screen.
    let xml = app(&node("StaticText", "Top row", 0, -30, 402, 80));
    let answer = Arc::new(Mutex::new(hit(
        "springboard",
        "StatusBar",
        "",
        (0, 0, 402, 62),
    )));
    let f = fake_hit(xml, answer.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let top = d
        .find_element(SIM, &Selector::Text("Top row".into()))
        .await
        .unwrap();
    // SpringBoard's status bar really is there: refused as before.
    let e = d.tap(SIM, &top).await.unwrap_err().to_string();
    assert!(e.contains("covered by the status bar"), "{e}");
    assert!(f.taps().is_empty());
    // The app hides its bar and draws the row there: tapped.
    *answer.lock().unwrap() = hit("app", "StaticText", "Top row", (0, -30, 402, 80));
    d.tap(SIM, &top).await.unwrap();
    let (_, y) = f.taps()[0];
    assert!((0.0..50.0).contains(&y), "{y}");
}

/// One node of a fi56 hit-test answer: `path` in the app's snapshot and
/// `chain` (root first) as VelocityRunner sends them, plus `extra` fields.
fn hit_node(
    ty: &str,
    label: &str,
    f: (i32, i32, i32, i32),
    path: &[usize],
    chain: &[String],
    extra: &str,
) -> String {
    format!(
        r#"{{"type":"{ty}","label":"{label}","identifier":"","x":{},"y":{},"width":{},"height":{},"path":{path:?},"chain":[{}]{extra}}}"#,
        f.0,
        f.1,
        f.2,
        f.3,
        chain.join(",")
    )
}

fn chain_entry(ty: &str, label: &str, f: (i32, i32, i32, i32)) -> String {
    format!(
        r#"{{"type":"{ty}","label":"{label}","identifier":"","x":{},"y":{},"width":{},"height":{}}}"#,
        f.0, f.1, f.2, f.3
    )
}

const FULL: (i32, i32, i32, i32) = (0, 0, 402, 874);
const PAY: (i32, i32, i32, i32) = (20, 400, 200, 50);

/// The app root, a full-screen container holding "Pay", and a later
/// full-screen sibling `top` with the same frame and type as the container.
fn pay_under(top: &str) -> String {
    app(&format!(
        r#"<XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="0" width="402" height="874" enabled="true">{}</XCUIElementTypeOther>{top}"#,
        node("Button", "Pay", PAY.0, PAY.1, PAY.2, PAY.3)
    ))
}

/// Review MAJOR 2, false negative: a dimming view over "Pay" has the same
/// frame and type as Pay's own full-screen container, so matching the hit by
/// (frame, type) found the container and let the tap through. XCTest's hit
/// element, placed by its snapshot path, is the dimming view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_refuses_a_dimming_view_framed_like_the_targets_container() {
    let xml = pay_under(
        r#"<XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="0" width="402" height="874" enabled="true"/>"#,
    );
    let root = chain_entry("Application", "", FULL);
    let dim = chain_entry("Other", "", FULL);
    let overlay = hit_node("Other", "", FULL, &[1], &[root.clone(), dim.clone()], "");
    let answer = Arc::new(Mutex::new(Some(format!(
        r#"{{"available":true,"owner":"app","pid":42,"hit":{overlay},"element":{overlay}}}"#
    ))));
    let f = fake_hit(xml, answer.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let pay = d
        .find_element(SIM, &Selector::Text("Pay".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
    assert!(
        e.contains("would land on Other") && e.contains("ios.verifyTap"),
        "{e}"
    );
    assert!(f.taps().is_empty(), "nothing tapped: {:?}", f.taps());

    // XCTest's element is Pay itself (path [0, 0]): tapped.
    let container = chain_entry("Other", "", FULL);
    let button = hit_node(
        "Button",
        "Pay",
        PAY,
        &[0, 0],
        &[root.clone(), container, chain_entry("Button", "Pay", PAY)],
        "",
    );
    *answer.lock().unwrap() = Some(format!(
        r#"{{"available":true,"owner":"app","pid":42,"hit":{button},"element":{button}}}"#
    ));
    d.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.taps().len(), 1);
}

/// Review MAJOR 2, false positive: a later full-screen sibling that lets
/// touches through (a SwiftUI overlay layer, an RN toast host) is the
/// deepest node at the point, but XCTest's hit test returns Pay: tapped.
/// Without XCTest's element, a deepest node the runner reports with user
/// interaction off is skipped; one that is merely not an accessibility
/// element or not hittable is not (review-fi56 MAJOR-1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_lets_a_tap_through_a_pass_through_overlay() {
    let xml = pay_under(&node("Other", "Toasts", 0, 0, 402, 874));
    let root = chain_entry("Application", "", FULL);
    let host = |extra: &str| {
        hit_node(
            "Other",
            "Toasts",
            FULL,
            &[1],
            &[root.clone(), chain_entry("Other", "Toasts", FULL)],
            extra,
        )
    };
    let button = hit_node(
        "Button",
        "Pay",
        PAY,
        &[0, 0],
        &[
            root.clone(),
            chain_entry("Other", "", FULL),
            chain_entry("Button", "Pay", PAY),
        ],
        "",
    );
    let answer = Arc::new(Mutex::new(Some(format!(
        r#"{{"available":true,"owner":"app","pid":42,"hit":{button},"element":{}}}"#,
        host("")
    ))));
    let f = fake_hit(xml, answer.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let pay = d
        .find_element(SIM, &Selector::Text("Pay".into()))
        .await
        .unwrap();
    d.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.taps().len(), 1);

    // XCTest's element not placed in the snapshot; the overlay reported
    // with user interaction off: the touch goes through it.
    *answer.lock().unwrap() = Some(format!(
        r#"{{"available":true,"owner":"app","pid":42,"element":{}}}"#,
        host(r#","interactive":false"#)
    ));
    d.tap(SIM, &pay).await.unwrap();
    assert_eq!(f.taps().len(), 2);
    // Review-fi56 MAJOR-1: a plain UIView overlay (a dimming view) is not an
    // accessibility element and not hittable to XCTest, yet it swallows the
    // touch: refused, whichever of those flags the runner read.
    for flag in [
        r#","hittable":false"#,
        r#","accessible":false"#,
        r#","accessible":false,"hittable":false,"interactive":true"#,
    ] {
        *answer.lock().unwrap() = Some(format!(
            r#"{{"available":true,"owner":"app","pid":42,"element":{}}}"#,
            host(flag)
        ));
        let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
        assert!(
            e.contains("would land on Other") && e.contains("Toasts"),
            "{flag}: {e}"
        );
    }
    assert_eq!(f.taps().len(), 2);
    // The same overlay, taking touches: refused.
    *answer.lock().unwrap() = Some(format!(
        r#"{{"available":true,"owner":"app","pid":42,"element":{}}}"#,
        host(r#","hittable":true"#)
    ));
    let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
    assert!(
        e.contains("would land on Other") && e.contains("Toasts"),
        "{e}"
    );
    assert_eq!(f.taps().len(), 2);
}

/// DV-4 (iPhone 17 Pro simulator, iOS 26.0, 2026-10-25): a SwiftUI app keeps
/// an empty full-screen window after its content window. XCTest's element
/// for an alert's button couldn't be placed in the snapshot, so the deepest
/// node, that empty window's root, decided and the tap was refused although
/// it lands. An empty other window lets the tap through; one holding a
/// labelled overlay does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_looks_through_an_empty_window_over_the_app() {
    let window = |inner: &str| {
        format!(
            r#"<XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="402" height="874" enabled="true">{inner}</XCUIElementTypeWindow>"#
        )
    };
    let other = |label: &str, inner: &str| {
        format!(
            r#"<XCUIElementTypeOther type="XCUIElementTypeOther" label="{label}" x="0" y="0" width="402" height="874" enabled="true">{inner}</XCUIElementTypeOther>"#
        )
    };
    let content = window(&other(
        "",
        &node("Button", "Delete", PAY.0, PAY.1, PAY.2, PAY.3),
    ));
    let on_top = |label: &str| {
        let root = chain_entry("Application", "", FULL);
        let w = chain_entry("Window", "", FULL);
        let o = chain_entry("Other", label, FULL);
        let top = hit_node(
            "Other",
            label,
            FULL,
            &[1, 0, 0],
            &[root, w, o.clone(), o],
            "",
        );
        format!(r#"{{"available":true,"owner":"app","pid":42,"element":{top}}}"#)
    };
    let xml = app(&format!("{content}{}", window(&other("", &other("", "")))));
    let answer = Arc::new(Mutex::new(Some(on_top(""))));
    let f = fake_hit(xml, answer.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let delete = d
        .find_element(SIM, &Selector::Text("Delete".into()))
        .await
        .unwrap();
    d.tap(SIM, &delete).await.unwrap();
    assert_eq!(f.taps().len(), 1);

    let f = fake_hit(
        app(&format!(
            "{content}{}",
            window(&other("Dimming", &other("Dimming", "")))
        )),
        Arc::new(Mutex::new(Some(on_top("Dimming")))),
    )
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let delete = d
        .find_element(SIM, &Selector::Text("Delete".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &delete).await.unwrap_err().to_string();
    assert!(
        e.contains("would land on Other") && e.contains("Dimming"),
        "{e}"
    );
    assert!(f.taps().is_empty());
}

/// DV-5 (iPhone 17 Pro simulator, iOS 26.0, 2026-10-25): a WKWebView's page
/// is drawn by WebKit's WebContent process, so XCTest's hit test names
/// another pid at every point of it and a tap into the page was refused.
/// Inside a web view that is the app's page; elsewhere it is still refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_tap_takes_a_web_views_content_process_as_the_app() {
    let web = format!(
        r#"<XCUIElementTypeWebView type="XCUIElementTypeWebView" label="web" x="0" y="116" width="402" height="724" enabled="true">{}</XCUIElementTypeWebView>"#,
        node("TextField", "web_input", 24, 221, 238, 42)
    );
    let xml = app(&format!(
        "{web}{}",
        node("Button", "Pay", PAY.0, 860, PAY.2, 10)
    ));
    let answer = Arc::new(Mutex::new(Some(
        r#"{"available":true,"owner":"other","pid":77}"#.to_string(),
    )));
    let f = fake_hit(xml, answer).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url).with_verify_tap(true);
    let field = d
        .find_element(SIM, &Selector::Text("web_input".into()))
        .await
        .unwrap();
    d.tap(SIM, &field).await.unwrap();
    assert_eq!(f.taps().len(), 1);
    let pay = d
        .find_element(SIM, &Selector::Text("Pay".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &pay).await.unwrap_err().to_string();
    assert!(e.contains("another process draws"), "{e}");
    assert_eq!(f.taps().len(), 1);
}

fn row_switch(value: u8) -> String {
    app(&format!(
        r#"<XCUIElementTypeSwitch type="XCUIElementTypeSwitch" label="Outdoor Seating" value="{value}" x="0" y="300" width="402" height="44" enabled="true"/>"#
    ))
}

/// Review 09 #11: a SwiftUI Toggle row is tapped at its trailing end (the
/// knob in a left-to-right layout). In an RTL layout that is the label: the
/// value stays as it was, and the leading end is tapped once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_switch_that_did_not_flip_is_tapped_at_its_other_end() {
    let xml = Arc::new(Mutex::new(row_switch(0)));
    let f = fake_on(xml.clone()).await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Outdoor Seating".into()))
        .await
        .unwrap();
    d.tap(SIM, &el).await.unwrap();
    let taps = f.taps();
    assert_eq!(taps.len(), 2, "{taps:?}");
    assert!(taps[0].0 > 350.0, "trailing end first: {taps:?}");
    assert!(taps[1].0 < 50.0, "then the leading end: {taps:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_switch_that_flipped_is_tapped_once() {
    let xml = Arc::new(Mutex::new(row_switch(0)));
    let f = FakeWda::start({
        let xml = xml.clone();
        move |r| {
            if r.path.ends_with("/actions") {
                *xml.lock().unwrap() = row_switch(1);
            }
            let xml = xml.lock().unwrap().clone();
            if r.path == "/status" {
                return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S"}"#.into());
            }
            if r.path.ends_with("/source") {
                return source(&xml);
            }
            if r.path.ends_with("/wda/screen") {
                return ok(r#"{"statusBarSize":{"width":402,"height":62}}"#);
            }
            if r.path.ends_with("/window/size") {
                return ok(r#"{"width":402,"height":874}"#);
            }
            if r.path.contains("alert") {
                return w3c(404, "no such alert", "none");
            }
            ok("null")
        }
    })
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let el = d
        .find_element(SIM, &Selector::Text("Outdoor Seating".into()))
        .await
        .unwrap();
    d.tap(SIM, &el).await.unwrap();
    assert_eq!(f.taps().len(), 1, "{:?}", f.taps());
}

/// Review 09 #2: trees are read without `visible`, so a button behind a
/// sheet passed for shown. With a modal layer up, the lookup tree gets
/// WDA's `visible` flags (one more read), and the hidden match is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_a_sheet_the_lookup_tree_reads_visibility() {
    let screen = |behind_visible: &str| {
        app(&format!(
            r#"<XCUIElementTypeButton type="XCUIElementTypeButton" label="Delete" visible="{behind_visible}" x="20" y="700" width="362" height="44" enabled="true"/><XCUIElementTypeSheet type="XCUIElementTypeSheet" label="Options" x="0" y="400" width="402" height="474" enabled="true"><XCUIElementTypeButton type="XCUIElementTypeButton" label="Cancel" x="20" y="800" width="362" height="44" enabled="true"/></XCUIElementTypeSheet>"#
        ))
    };
    let (cheap, full) = (screen("true"), screen("false"));
    let f = FakeWda::start(move |r| {
        if r.path == "/status" {
            return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S"}"#.into());
        }
        if r.path.ends_with("/source") {
            // The cheap read leaves `visible` out: the parser takes every
            // node as shown, as with WDA's real reply.
            return if r.query.contains("visible") {
                source(&cheap.replace(r#" visible="true""#, ""))
            } else {
                source(&full)
            };
        }
        if r.path.ends_with("/wda/screen") {
            return ok(r#"{"statusBarSize":{"width":402,"height":62}}"#);
        }
        if r.path.ends_with("/window/size") {
            return ok(r#"{"width":402,"height":874}"#);
        }
        if r.path.contains("alert") {
            return w3c(404, "no such alert", "none");
        }
        ok("null")
    })
    .await;
    let d = velocity_ios::IosDriver::with_wda_url(&f.url);
    let tree = d.get_hierarchy_cached(SIM).await.unwrap();
    fn find<'a>(
        e: &'a velocity_common::Element,
        label: &str,
    ) -> Option<&'a velocity_common::Element> {
        if e.label.as_deref() == Some(label) {
            return Some(e);
        }
        e.children.iter().find_map(|c| find(c, label))
    }
    assert!(!find(&tree, "Delete").unwrap().visible, "behind the sheet");
    assert!(find(&tree, "Cancel").unwrap().visible, "in the sheet");
    let full_reads = f
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.ends_with("/source") && !r.query.contains("visible"))
        .count();
    assert_eq!(full_reads, 1);
    // The same tree again: no second visibility read.
    d.get_hierarchy_cached(SIM).await.unwrap();
    let again = f
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path.ends_with("/source") && !r.query.contains("visible"))
        .count();
    assert_eq!(again, 1);
}
