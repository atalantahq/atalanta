// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! D-1 on the web: visible for an assertion means rendered, a box, and on
//! screen (in the viewport, not clipped away by an overflow ancestor), as on
//! the devices. Its own test binary: it changes process-wide settings.

mod common;

use std::collections::HashMap;

use atalanta_common::{PlatformDriver, Selector};
use common::{driver, serve};

const PAGE: &str = r#"<!doctype html><html><body style="margin:0">
<button id="top">Top</button>
<div id="carousel" style="width:200px;height:50px;overflow:hidden;white-space:nowrap">
  <span id="slide1" style="display:inline-block;width:200px">One</span><span id="slide2" style="display:inline-block;width:200px">Two</span>
</div>
<div id="clear" style="opacity:0">Ghost</div>
<div style="position:relative;width:200px;height:40px;overflow:hidden">
  <div id="half" style="position:absolute;top:20px;height:40px">Half</div>
</div>
<div style="height:3000px"></div>
<button id="far">Far</button>
</body></html>"#;

fn css(q: &str) -> Selector {
    Selector::Css(q.into())
}

#[tokio::test]
async fn assertions_see_only_what_is_on_screen() {
    require_chrome!();
    let base = serve(&[("/v.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/v.html"), false)
        .await
        .unwrap();
    let visible = |q: &'static str| {
        let d = &d;
        async move {
            let el = d.find_element("web", &css(q)).await.unwrap();
            d.is_element_visible("web", &el).await.unwrap()
        }
    };
    assert!(visible("#top").await);
    // VEL-353: below the fold is not visible (it was, anywhere on the page).
    assert!(!visible("#far").await);
    // VEL-276: a carousel slide clipped by overflow:hidden is not visible.
    assert!(visible("#slide1").await);
    assert!(!visible("#slide2").await);
    // VEL-356: opacity 0 stays hidden.
    assert!(!visible("#clear").await);
    // Half of #half shows: visible at any pixel, not with minArea 0.6.
    assert!(visible("#half").await);
    let mut set = HashMap::new();
    set.insert(
        "ATALANTA_VISIBILITY_MIN_AREA".to_string(),
        "0.6".to_string(),
    );
    atalanta_common::env::set_settings(set);
    assert!(!visible("#half").await);
    // `web.visibility: page`: the old rule, anywhere on the page.
    let mut set = HashMap::new();
    set.insert("ATALANTA_WEB_VISIBILITY".to_string(), "page".to_string());
    atalanta_common::env::set_settings(set);
    assert!(visible("#far").await);
    assert!(visible("#slide2").await);
    assert!(!visible("#clear").await);
    atalanta_common::env::set_settings(HashMap::new());
    // An action still scrolls its target into view, and then it is on screen.
    let far = d.find_element("web", &css("#far")).await.unwrap();
    d.tap("web", &far).await.unwrap();
    assert!(visible("#far").await);
    d.cleanup().await;
}
