// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Decisions run d2: text matching (D-10, D-11, tc-case), visibility (D-1)
//! and the accessible name (D-W2) on the web, against a real headless
//! Chrome (see `common`).

mod common;

use atalanta_common::{PlatformDriver, Selector};
use common::{driver, serve};

const CASES: &str = r#"<!doctype html><html><body>
<button id="del">DELETE</button>
<button id="all">Delete all</button>
<p id="row1">Add</p><p id="row2">Add</p><p id="row3">ADD</p>
</body></html>"#;

/// D-10: `text:` falls back to ignoring case when nothing matches exactly,
/// as on the devices; an exact match wins, and an `index:` takes its tier
/// from its own selector.
#[tokio::test]
async fn text_falls_back_to_ignoring_case_like_the_devices() {
    require_chrome!();
    let base = serve(&[("/c.html", CASES)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    let id_of = |e: atalanta_common::Element| e.dom_id.unwrap_or_default();
    let del = d
        .find_element("web", &Selector::Text("Delete".into()))
        .await
        .unwrap();
    assert_eq!(id_of(del), "del");
    // An exact "Add" exists: the folded "ADD" isn't counted.
    let adds = d
        .find_elements("web", &Selector::Text("Add".into()))
        .await
        .unwrap();
    assert_eq!(adds.len(), 2);
    let third = d
        .find_elements(
            "web",
            &Selector::Index {
                selector: Box::new(Selector::Text("Add".into())),
                index: 2,
            },
        )
        .await
        .unwrap();
    assert!(third.is_empty(), "{third:?}");
    d.cleanup().await;
}

const REGEX: &str = r#"<!doctype html><html><body>
<div id="list"><p data-testid="row_1">Item 1 of 3</p><p data-testid="row_2">Item 2 of 3</p></div>
<input id="email" placeholder="Email address">
<p id="hidden" style="display:none">Item 9 of 9</p>
</body></html>"#;

/// D-11: `textMatches` and `idMatches` resolve through the tree with the
/// devices' regex semantics (whole match, ignoring case), hidden elements
/// excluded, `index:` in reading order.
#[tokio::test]
async fn regex_selectors_resolve_on_the_web() {
    require_chrome!();
    let base = serve(&[("/r.html", REGEX)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/r.html"), false)
        .await
        .unwrap();
    let all = d
        .find_elements("web", &Selector::TextMatches(r"item \d+ of \d+".into()))
        .await
        .unwrap();
    let texts: Vec<_> = all.iter().filter_map(|e| e.text.clone()).collect();
    assert_eq!(texts, ["Item 1 of 3", "Item 2 of 3"]);
    let second = d
        .find_element(
            "web",
            &Selector::Index {
                selector: Box::new(Selector::IdMatches(r"ROW_\d".into())),
                index: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(second.text.as_deref(), Some("Item 2 of 3"));
    let field = d
        .find_element("web", &Selector::TextMatches("email.*".into()))
        .await
        .unwrap();
    assert_eq!(field.element_type, "input", "the placeholder is the hint");
    assert!(d
        .find_element("web", &Selector::TextMatches("Item".into()))
        .await
        .is_err());
    d.cleanup().await;
}

/// tc-case: `textContains` ignores case on the web too.
#[tokio::test]
async fn text_contains_ignores_case() {
    require_chrome!();
    let base = serve(&[("/c.html", CASES)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/c.html"), false)
        .await
        .unwrap();
    let found = d
        .find_element("web", &Selector::TextContains("delete ALL".into()))
        .await
        .unwrap();
    assert_eq!(found.dom_id.as_deref(), Some("all"));
    d.cleanup().await;
}

/// D-6 / R5: the executor counts a web selector's matches from the page
/// (`find_elements`), keeping the visible ones: a twin inside a closed
/// `<details>` must read as hidden there.
#[tokio::test]
async fn a_twin_in_a_closed_details_reads_as_hidden() {
    require_chrome!();
    let page = r#"<!doctype html><html><body>
<button>Save</button>
<details><summary>More</summary><button>Save</button></details>
</body></html>"#;
    let base = serve(&[("/d.html", page)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/d.html"), false)
        .await
        .unwrap();
    let all = d
        .find_elements("web", &Selector::Text("Save".into()))
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let shown = all
        .iter()
        .filter(|e| e.visible && !e.bounds.is_empty())
        .count();
    assert_eq!(shown, 1, "{all:?}");
    d.cleanup().await;
}

const NAMES: &str = r##"<!doctype html><html><body>
<button id="close" aria-label="Close dialog">X</button>
<span id="lbl">Shipping address</span><input id="ship" aria-labelledby="lbl">
<label for="email">Email</label><input id="email">
<img id="logo" alt="Company logo" src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" width="10" height="10">
<a id="help" href="#help" title="Help center"><svg width="10" height="10"></svg></a>
<button id="save"><span>Save</span> draft</button>
<div id="plain">Save draft</div>
</body></html>"##;

/// D-W2: `accessibilityId` on the web is the accessible name: aria-label as
/// before, and aria-labelledby, a <label>, alt, title and a button's text.
#[tokio::test]
async fn accessibility_id_is_the_accessible_name() {
    require_chrome!();
    let base = serve(&[("/n.html", NAMES)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/n.html"), false)
        .await
        .unwrap();
    for (name, id) in [
        ("Close dialog", "close"),
        ("Shipping address", "ship"),
        ("Email", "email"),
        ("Company logo", "logo"),
        ("Help center", "help"),
        ("Save draft", "save"),
    ] {
        let el = d
            .find_element("web", &Selector::AccessibilityId(name.into()))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(el.dom_id.as_deref(), Some(id), "{name}");
    }
    // A plain <div> isn't named by its content.
    let all = d
        .find_elements("web", &Selector::AccessibilityId("Save draft".into()))
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    d.cleanup().await;
}

const ARIA_FIRST: &str = r##"<!doctype html><html><body>
<button id="early">Submit</button>
<input id="sub" type="submit">
<div id="footer" role="button" aria-label="Submit" tabindex="0">Go</div>
</body></html>"##;

/// Review MAJOR 7 (D-W2 "aria-label first"): an earlier element whose
/// computed name is the same never takes over from the aria-label match
/// the old `[aria-label=…]` rule found.
#[tokio::test]
async fn accessibility_id_prefers_an_aria_label_match() {
    require_chrome!();
    let base = serve(&[("/a.html", ARIA_FIRST)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/a.html"), false)
        .await
        .unwrap();
    let el = d
        .find_element("web", &Selector::AccessibilityId("Submit".into()))
        .await
        .unwrap();
    assert_eq!(el.label.as_deref(), Some("Submit"), "{el:?}");
    let all = d
        .find_elements("web", &Selector::AccessibilityId("Submit".into()))
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    d.cleanup().await;
}

const HIDDEN_LABEL: &str = r##"<!doctype html><html><body>
<div id="dialog" style="display:none"><button id="x" aria-label="Close">&times;</button></div>
<button id="shown">Close</button>
</body></html>"##;

/// review-fixes #4: the aria-label priority applies among visible matches:
/// a closed dialog's hidden `aria-label="Close"` doesn't hide the visible
/// `<button>Close</button>` (before: the lookup resolved to the hidden one).
#[tokio::test]
async fn a_hidden_aria_label_match_never_hides_a_visible_named_one() {
    require_chrome!();
    let base = serve(&[("/h.html", HIDDEN_LABEL)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/h.html"), false)
        .await
        .unwrap();
    let close = Selector::AccessibilityId("Close".into());
    let el = d.find_element("web", &close).await.unwrap();
    assert_eq!(el.dom_id.as_deref(), Some("shown"), "{el:?}");
    assert!(el.visible && !el.bounds.is_empty(), "{el:?}");
    // Once the dialog opens, its aria-label button wins again.
    d.debug_eval_main("document.getElementById('dialog').style.display='block'")
        .await
        .unwrap();
    let el = d.find_element("web", &close).await.unwrap();
    assert_eq!(el.label.as_deref(), Some("Close"), "{el:?}");
    d.cleanup().await;
}

const BELOW_FOLD: &str = r##"<!doctype html><html><body style="margin:0">
<button id="top">Buy</button>
<div style="height:3000px"></div>
<button id="low">Buy</button>
</body></html>"##;

/// Review MAJOR 8: the lookup counts its on-screen matches in the same page
/// call, so the executor's ambiguity check needs no second lookup, and a
/// twin below the fold isn't "on screen".
#[tokio::test]
async fn the_lookup_counts_on_screen_matches() {
    require_chrome!();
    let base = serve(&[("/b.html", BELOW_FOLD)]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/b.html"), false)
        .await
        .unwrap();
    let buy = Selector::Text("Buy".into());
    d.find_element("web", &buy).await.unwrap();
    assert_eq!(d.last_match_count("web", &buy), Some(1));
    // Both on screen once the page is short.
    d.debug_eval_main("document.querySelector('div').style.height='10px'")
        .await
        .unwrap();
    d.find_element("web", &buy).await.unwrap();
    assert_eq!(d.last_match_count("web", &buy), Some(2));
    assert_eq!(
        d.last_match_count("web", &Selector::Text("Other".into())),
        None
    );
    d.cleanup().await;
}

/// Review MINOR 10: `textMatches` is a whole match, and the element tree
/// cut text at 200 characters, so a long paragraph never matched.
#[tokio::test]
async fn text_matches_sees_the_whole_text() {
    require_chrome!();
    let long = format!(
        "<!doctype html><html><body><p id=\"terms\">By continuing you agree to {} and our Privacy Policy.</p></body></html>",
        "the terms of service ".repeat(15)
    );
    let base = serve(&[("/l.html", long.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/l.html"), false)
        .await
        .unwrap();
    d.find_element(
        "web",
        &Selector::TextMatches(r"By continuing.*Privacy Policy\.".into()),
    )
    .await
    .unwrap();
    d.cleanup().await;
}
