//! Review MINOR 10: focus that stays inside a cross-origin frame after the
//! click of an `inputText` is focus the click didn't move, against a real
//! headless Chrome and a two-origin page.

mod common;

use common::{driver, find, serve};
use velocity_common::PlatformDriver;

const CARD_FIELD: &str = r#"<!doctype html><html><body style="margin:0">
<input id="n" style="width:300px;height:60px"></body></html>"#;

#[tokio::test]
async fn focus_left_in_a_card_frame_fails_the_next_input_text() {
    require_chrome!();
    let inner = serve(&[("/card.html", CARD_FIELD)]);
    let port = inner.rsplit(':').next().unwrap();
    // `localhost` vs the outer page's 127.0.0.1: another origin. "Name" is
    // a custom widget that ignores clicks and takes no focus.
    let page = format!(
        "<!doctype html><html><body style=\"margin:0\"><iframe id=\"card\" \
         src=\"http://localhost:{port}/card.html\" style=\"width:320px;height:80px;border:0\"></iframe>\
         <div id=\"name\" onmousedown=\"event.preventDefault()\" style=\"width:300px;height:40px\">Name</div></body></html>"
    );
    let outer = serve(&[("/p.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{outer}/p.html"), false)
        .await
        .unwrap();
    assert!(
        common::wait_found(&d, "#n").await,
        "the card frame never loaded"
    );
    // Into the card: the frame takes the focus, unknown inside, allowed.
    let card = find(&d, "#card").await;
    d.input_text("web", &card, "4242").await.unwrap();
    // Into "Name": the focus stays in the card frame; typing would go into
    // the card number. Before the fix this passed.
    let name = find(&d, "#name").await;
    let err = d
        .input_text("web", &name, "Ada")
        .await
        .expect_err("focus stayed in the card frame");
    assert!(err.to_string().contains("iframe"), "{err}");
    // Into the card again: the target is the frame itself, allowed.
    d.input_text("web", &card, "4242").await.unwrap();
    d.cleanup().await;
}

/// review-i1-i3 M2: a Stripe-like card widget, targeted by its container
/// on the page (`div#card-element`) whose field lives in a cross-origin
/// <iframe> filling it. The second `inputText` finds the focus still in
/// that frame, which is the target's own: typing is allowed. Before, it
/// failed "the focus is on <iframe>".
#[tokio::test]
async fn two_input_texts_into_a_card_widget_container() {
    require_chrome!();
    let inner = serve(&[("/card.html", CARD_FIELD)]);
    let port = inner.rsplit(':').next().unwrap();
    let page = format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <div id=\"card-element\" style=\"width:320px;height:80px;padding:4px;border:1px solid #ccc\">\
         <iframe src=\"http://localhost:{port}/card.html\" style=\"width:320px;height:80px;border:0\"></iframe>\
         </div></body></html>"
    );
    let outer = serve(&[("/p.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{outer}/p.html"), false)
        .await
        .unwrap();
    assert!(
        common::wait_found(&d, "#n").await,
        "the card frame never loaded"
    );
    let card = find(&d, "#card-element").await;
    d.input_text("web", &card, "4242").await.unwrap();
    let card = find(&d, "#card-element").await;
    d.input_text("web", &card, " 4242").await.unwrap();
    let field = find(&d, "#n").await;
    assert!(field.platform_id.starts_with('@'), "{}", field.platform_id);
    assert_eq!(
        d.get_element_text("web", &field).await.unwrap(),
        "4242 4242"
    );
    d.cleanup().await;
}

/// review-fi2 F6: a container that cancels its mousedown and holds a card
/// frame in its top part, the focus left in that frame by an earlier
/// step. `inputText` on the container clicks its centre, beside the
/// frame: the focus doesn't move, and typing would go into the card
/// field. It is refused. Before, holding the <iframe> was enough.
#[tokio::test]
async fn a_container_clicked_beside_its_frame_does_not_take_the_frames_focus() {
    require_chrome!();
    let inner = serve(&[("/card.html", CARD_FIELD)]);
    let port = inner.rsplit(':').next().unwrap();
    let page = format!(
        "<!doctype html><html><body style=\"margin:0\">\
         <div id=\"wrap\" onmousedown=\"event.preventDefault()\" style=\"width:320px;height:260px\">\
         <iframe id=\"card\" src=\"http://localhost:{port}/card.html\" style=\"display:block;width:320px;height:80px;border:0\"></iframe>\
         </div></body></html>"
    );
    let outer = serve(&[("/p.html", page.as_str())]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{outer}/p.html"), false)
        .await
        .unwrap();
    assert!(
        common::wait_found(&d, "#n").await,
        "the card frame never loaded"
    );
    let card = find(&d, "#card").await;
    d.input_text("web", &card, "4242").await.unwrap();
    let wrap = find(&d, "#wrap").await;
    let err = d
        .input_text("web", &wrap, "99")
        .await
        .expect_err("the click beside the frame moved no focus");
    assert!(err.to_string().contains("iframe"), "{err}");
    let field = find(&d, "#n").await;
    assert_eq!(d.get_element_text("web", &field).await.unwrap(), "4242");
    d.cleanup().await;
}
