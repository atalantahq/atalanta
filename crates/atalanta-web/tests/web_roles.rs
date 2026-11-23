//! S1/S2: `role:` + `name:` and state filters on the web, computed in the
//! page by Playwright's rules, against a real headless Chrome.

mod common;

use atalanta_common::{PlatformDriver, Relation, Selector};
use common::{driver, serve};

const PAGE: &str = r#"<!doctype html><html><body>
<h2 id="h">Payment</h2>
<p id="label">Pay</p>
<a id="help" href="/help">Pay</a>
<button id="cancel">Cancel</button>
<button id="pay">Pay</button>
<div id="custom" role="button" aria-label="Close dialog">x</div>
<label for="email">Email</label><input id="email" type="email">
<input id="q" type="search" placeholder="Search">
<input id="vol" type="range">
<input id="agree" type="checkbox"><label for="agree">Agree</label>
<input id="news" type="checkbox" checked><label for="news">News</label>
<button id="next-off" disabled>Next</button>
<button id="next-on">Next</button>
<div role="tablist"><div id="t1" role="tab" aria-selected="true">Home</div><div id="t2" role="tab" aria-selected="false">Away</div></div>
<ul><li id="li1">One</li></ul>
<img id="logo" src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" alt="Logo" width="10" height="10">
<img id="spacer" src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" alt="" width="10" height="10">
</body></html>"#;

fn and(parts: Vec<Selector>) -> Selector {
    Selector::Compound(parts)
}

fn role(r: &str) -> Selector {
    Selector::Role(r.into())
}

fn name(n: &str) -> Selector {
    Selector::Name(n.into())
}

async fn dom_id(d: &atalanta_web::WebDriver, s: &Selector) -> Option<String> {
    let el = d.find_element("web", s).await.ok()?;
    el.dom_id
}

#[tokio::test]
async fn role_and_name_find_what_playwright_would() {
    require_chrome!();
    let base = serve(&[("/r.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/r.html"), false)
        .await
        .unwrap();
    let id = |s: Selector| {
        let d = &d;
        async move { dom_id(d, &s).await }
    };
    // Three "Pay"s; the role tells them apart.
    assert_eq!(
        id(and(vec![role("button"), name("Pay")])).await.as_deref(),
        Some("pay")
    );
    assert_eq!(
        id(and(vec![role("link"), name("Pay")])).await.as_deref(),
        Some("help")
    );
    assert_eq!(
        id(and(vec![role("paragraph"), Selector::Text("Pay".into())]))
            .await
            .as_deref(),
        Some("label")
    );
    assert_eq!(id(role("heading")).await.as_deref(), Some("h"));
    // An explicit role, named by aria-label.
    assert_eq!(
        id(and(vec![role("button"), name("Close dialog")]))
            .await
            .as_deref(),
        Some("custom")
    );
    // Implicit roles of inputs; a field is named by its <label>.
    assert_eq!(
        id(and(vec![role("textbox"), name("Email")]))
            .await
            .as_deref(),
        Some("email")
    );
    assert_eq!(id(role("searchbox")).await.as_deref(), Some("q"));
    assert_eq!(id(role("slider")).await.as_deref(), Some("vol"));
    assert_eq!(id(role("listitem")).await.as_deref(), Some("li1"));
    assert_eq!(id(role("img")).await.as_deref(), Some("logo"));
    // `name:` falls back to ignoring case, as `text:` does.
    assert_eq!(
        id(and(vec![role("button"), name("pay")])).await.as_deref(),
        Some("pay")
    );
    // No such role here.
    assert!(d.find_element("web", &role("dialog")).await.is_err());
    // The capture carries the computed role.
    let el = d
        .find_element("web", &and(vec![role("link"), name("Pay")]))
        .await
        .unwrap();
    assert_eq!(el.role.as_deref(), Some("link"));
    // A role inside a relation and an index.
    let right_of = Selector::Relative {
        target: Box::new(role("button")),
        relation: Relation::RightOf,
        anchor: Box::new(Selector::Text("Cancel".into())),
    };
    assert_eq!(id(right_of).await.as_deref(), Some("pay"));
    let second = Selector::Index {
        selector: Box::new(role("checkbox")),
        index: 1,
    };
    assert_eq!(id(second).await.as_deref(), Some("news"));
    d.cleanup().await;
}

#[tokio::test]
async fn state_filters_narrow_on_the_web() {
    require_chrome!();
    let base = serve(&[("/s.html", PAGE)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/s.html"), false)
        .await
        .unwrap();
    let id = |s: Selector| {
        let d = &d;
        async move { dom_id(d, &s).await }
    };
    assert_eq!(
        id(and(vec![role("checkbox"), Selector::Checked(true)]))
            .await
            .as_deref(),
        Some("news")
    );
    assert_eq!(
        id(and(vec![role("checkbox"), Selector::Checked(false)]))
            .await
            .as_deref(),
        Some("agree")
    );
    assert_eq!(
        id(and(vec![
            Selector::Text("Next".into()),
            Selector::Enabled(true)
        ]))
        .await
        .as_deref(),
        Some("next-on")
    );
    assert_eq!(
        id(and(vec![
            Selector::Text("Next".into()),
            Selector::Enabled(false)
        ]))
        .await
        .as_deref(),
        Some("next-off")
    );
    assert_eq!(
        id(and(vec![role("tab"), Selector::Selected(true)]))
            .await
            .as_deref(),
        Some("t1")
    );
    let tab = d
        .find_element("web", &and(vec![role("tab"), Selector::Selected(true)]))
        .await
        .unwrap();
    assert_eq!(
        tab.selected,
        Some(true),
        "the capture carries aria-selected"
    );
    // Focus moves with a tap into the field.
    let email = d
        .find_element("web", &and(vec![role("textbox"), name("Email")]))
        .await
        .unwrap();
    assert!(d
        .find_element("web", &and(vec![role("textbox"), Selector::Focused(true)]))
        .await
        .is_err());
    d.tap("web", &email).await.unwrap();
    assert_eq!(
        id(and(vec![role("textbox"), Selector::Focused(true)]))
            .await
            .as_deref(),
        Some("email")
    );
    d.cleanup().await;
}
