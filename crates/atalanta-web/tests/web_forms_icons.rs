//! Review 10 #8, #10 and #11: icon buttons are named from what they hold,
//! not their utility classes; the hit-target check blocks touch events;
//! file inputs take files and an `<option>` of a closed drop-down can be
//! tapped. Against a real headless Chrome (see `common`).

mod common;

use atalanta_common::{AtalantaError, PlatformDriver, Selector};
use common::{driver, find, serve, wait_text};

const ICONS: &str = r##"<!doctype html><html><body>
<p id="del"><button class="p-2 rounded hover:bg-gray-100 text-gray-500"><svg aria-hidden="true" width="16" height="16"><title>Delete</title><rect width="16" height="16"/></svg></button></p>
<p id="close"><button class="Button_root__x3F9a"><svg width="16" height="16"><title>Close</title><rect width="16" height="16"/></svg></button></p>
<p id="trash"><button class="btn px-3 py-1"><i class="fa fa-trash" style="display:inline-block;width:12px;height:12px"></i></button></p>
<p id="use"><button class="css-1x2y3z"><svg width="16" height="16"><use href="#icon-settings"></use></svg></button></p>
<p id="star"><span class="sc-bdfBwQ star md:w-4" style="display:inline-block;width:16px;height:16px;cursor:pointer"></span></p>
</body></html>"##;

/// Review 10 #8: an icon button's hint came from its own class words, so
/// a Tailwind or CSS-module button read "p 2 rounded hover:bg gray 100"
/// or "Button root x3F9a"; the name one level down (an svg <title>, an
/// icon font's class, a sprite's fragment) was never looked at.
#[tokio::test]
async fn icon_buttons_are_named_from_what_they_hold() {
    require_chrome!();
    let base = serve(&[("/i.html", ICONS)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/i.html"), false)
        .await
        .unwrap();
    for (id, want) in [
        ("#del", "Delete"),
        ("#close", "Close"),
        ("#trash", "trash"),
        ("#use", "settings"),
        ("#star", "star"),
    ] {
        // Icons have no id of their own: those are on their wrappers.
        let el = find(&d, &format!("{id} > *")).await;
        assert_eq!(el.hint.as_deref(), Some(want), "{id}");
    }
    // An svg <title> outside aria-hidden names its button for screen
    // readers, so `accessibilityId:` finds it.
    let close = d
        .find_element("web", &Selector::AccessibilityId("Close".into()))
        .await
        .unwrap();
    assert_eq!(close.element_type, "button");
    assert_eq!(close.hint.as_deref(), Some("Close"));
    d.cleanup().await;
}

const FORM: &str = r##"<!doctype html><html><body>
<label for="up">Upload</label><input id="up" type="file">
<input id="many" type="file" multiple>
<select id="city"><option value="paris">Paris</option><option value="lyon">Lyon</option>
<option value="nice" disabled>Nice</option></select>
<div id="out">idle</div>
<script>
document.getElementById('up').addEventListener('change', function (e) {
  document.getElementById('out').textContent = 'file:' + e.target.files[0].name;
});
document.getElementById('city').addEventListener('change', function (e) {
  document.getElementById('out').textContent = 'city:' + e.target.value;
});
</script></body></html>"##;

/// Review 10 #11: `inputText` on a file input sets its files (it failed
/// as "takes no text"), a missing file is a config error, and a tap on an
/// `<option>` of a closed drop-down chooses it (it failed as "zero size").
#[tokio::test]
async fn file_inputs_and_options() {
    require_chrome!();
    let dir = std::env::temp_dir().join(format!("atalanta-upload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("report.csv");
    std::fs::write(&file, "a,b\n1,2\n").unwrap();
    let base = serve(&[("/f.html", FORM)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/f.html"), false)
        .await
        .unwrap();

    // Through its label, as `tap: {text: Upload}` would find it.
    let label = find(&d, "label").await;
    d.input_text("web", &label, file.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        wait_text(&d, "#out", "file:report.csv").await,
        "file:report.csv"
    );

    let up = find(&d, "#up").await;
    let missing = d
        .input_text("web", &up, dir.join("nope.csv").to_str().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(missing, AtalantaError::Config(_)), "{missing}");
    let two = format!("{}\n{}", file.display(), file.display());
    let err = d.input_text("web", &up, &two).await.unwrap_err();
    assert!(err.to_string().contains("one file"), "{err}");
    let many = find(&d, "#many").await;
    d.input_text("web", &many, &two).await.unwrap();
    assert_eq!(
        d.debug_eval_main("document.getElementById('many').files.length")
            .await
            .unwrap(),
        2
    );

    let lyon = find(&d, "option[value=lyon]").await;
    d.tap("web", &lyon).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "city:lyon").await, "city:lyon");
    let nice = find(&d, "option[value=nice]").await;
    let err = d.tap("web", &nice).await.unwrap_err();
    assert!(err.to_string().contains("disabled"), "{err}");
    d.cleanup().await;
    let _ = std::fs::remove_dir_all(&dir);
}

const TOUCH: &str = r##"<!doctype html><html><head><meta name="viewport" content="width=device-width"></head>
<body style="margin:0">
<button id="b" style="position:absolute;top:100px;left:20px;width:200px;height:60px">Buy</button>
<div id="ov" style="position:absolute;top:300px;left:20px;width:200px;height:60px;background:#f00"></div>
<script>
window.ovTouches = 0;
document.getElementById('ov').addEventListener('touchstart', function () { window.ovTouches++; }, {passive:true});
document.getElementById('ov').addEventListener('touchend', function () { window.ovTouches++; }, {passive:true});
</script></body></html>"##;

/// Review 10 #10: on a touch profile, a tap the hit-target check blocks
/// must not reach the intruder's touch handlers (FastClick, Swiper): the
/// interceptor listened for pointer and mouse events only.
#[tokio::test]
async fn the_interceptor_blocks_touch_events() {
    require_chrome!();
    let base = serve(&[("/t.html", TOUCH)]);
    let mut config = atalanta_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    };
    config.apply_device(atalanta_common::web_devices::lookup("Pixel 7").unwrap());
    let d = atalanta_web::WebDriver::new(config);
    d.prepare("web").await.unwrap();
    d.launch_app("web", &format!("{base}/t.html"), false)
        .await
        .unwrap();
    let b = find(&d, "#b").await;
    let ov = find(&d, "#ov").await;
    // Armed for the button, the touch lands on the overlay.
    let armed = d
        .debug_call_el("arm", &b.platform_id, vec![])
        .await
        .unwrap();
    assert_eq!(armed, true);
    let (x, y) = (
        ov.bounds.x + ov.bounds.width / 2,
        ov.bounds.y + ov.bounds.height / 2,
    );
    d.tap_xy("web", x, y).await.unwrap();
    let verdict = d
        .debug_call_el("disarm", &b.platform_id, vec![])
        .await
        .unwrap();
    assert!(
        verdict["blocked"]
            .as_str()
            .unwrap_or_default()
            .contains("div"),
        "{verdict}"
    );
    // The touch was dispatched before `tap_xy` returned: its handlers
    // have run, or never will.
    assert_eq!(
        d.debug_eval_main("window.ovTouches").await.unwrap(),
        0,
        "the overlay's touch handlers ran"
    );
    d.cleanup().await;
}
