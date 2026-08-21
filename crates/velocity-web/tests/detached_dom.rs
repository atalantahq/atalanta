//! Review MINOR-8: elements Velocity has looked at don't outlive the page's
//! own use of them. Its own binary: it runs Chrome with `--expose-gc`.

mod common;

use common::{css, eventually, serve};
use velocity_common::{PlatformDriver, Selector};

/// The id registry holds elements through `WeakRef`s only, so rows the page
/// removes are collected and their ids dropped. In round 1 they stayed
/// reachable for the life of the page.
#[tokio::test]
async fn walked_elements_can_be_collected() {
    require_chrome!();
    let rows: String = (0..300)
        .map(|i| format!("<p class=\"r\">row {i}</p>"))
        .collect();
    let page = format!("<!doctype html><html><body><div id=\"list\">{rows}</div></body></html>");
    let base = serve(&[("/g.html", &page)]);
    let d = velocity_web::WebDriver::new(velocity_web::WebDriverConfig {
        chrome_path: common::chrome_path(),
        extra_args: vec!["--no-sandbox".into(), "--js-flags=--expose-gc".into()],
        ..Default::default()
    });
    d.prepare("web").await.unwrap();
    d.launch_app("web", &format!("{base}/g.html"), false)
        .await
        .unwrap();
    // Every path that registers elements.
    d.get_hierarchy("web").await.unwrap();
    assert_eq!(d.find_elements("web", &css(".r")).await.unwrap().len(), 300);
    let seven = d
        .find_element("web", &Selector::Text("row 7".into()))
        .await
        .unwrap();
    assert!(d.is_element_visible("web", &seven).await.unwrap());
    let live = d.debug_live_ids().await.unwrap();
    assert!(live >= 300, "{live}");

    d.debug_eval_main("document.getElementById('list').remove()")
        .await
        .unwrap();
    let collected = eventually(|| async {
        d.debug_eval_main("gc()").await.unwrap();
        d.debug_live_ids().await.unwrap() < 50
    })
    .await;
    assert!(
        collected,
        "detached rows are still registered: {}",
        d.debug_live_ids().await.unwrap()
    );
    d.cleanup().await;
}
