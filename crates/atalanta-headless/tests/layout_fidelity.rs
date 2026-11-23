// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Layout fidelity against Android semantics (VEL-103, VEL-106, VEL-107, VEL-108, VEL-109).

use std::sync::Arc;

use atalanta_common::Element;
use atalanta_headless::android::resources::ResourceTable;
use atalanta_headless::android::AndroidInflater;
use atalanta_headless::session::HeadlessSession;
use atalanta_headless::text::TextMeasurer;
use atalanta_headless::HeadlessConfig;

fn render(xml: &str, density: f32, width: u32, height: u32) -> Element {
    let config = HeadlessConfig {
        width,
        height,
        density,
        ..Default::default()
    };
    let inflater = AndroidInflater::new(ResourceTable::empty().with_density(density));
    let tree = inflater.inflate_xml(xml).expect("inflate");
    let mut session = HeadlessSession::new(config, Arc::new(TextMeasurer::new())).unwrap();
    session.set_render_tree(tree).expect("layout");
    session.get_hierarchy().unwrap().clone()
}

fn by_id<'a>(root: &'a Element, id: &str) -> &'a Element {
    fn walk<'a>(e: &'a Element, want: &str) -> Option<&'a Element> {
        if e.platform_id == want {
            return Some(e);
        }
        e.children.iter().find_map(|c| walk(c, want))
    }
    walk(root, &format!("headless:id/{id}")).unwrap_or_else(|| panic!("no {id}"))
}

const NS: &str = r#"xmlns:android="http://schemas.android.com/apk/res/android""#;

#[test]
fn long_text_wraps_and_pushes_sibling() {
    let long = "This is a long sentence that cannot possibly fit on one line of a narrow screen";
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <TextView android:id="@+id/body" android:layout_width="match_parent" android:layout_height="wrap_content" android:textSize="10dp" android:text="{long}"/>
            <TextView android:id="@+id/next" android:layout_width="match_parent" android:layout_height="wrap_content" android:textSize="10dp" android:text="Next"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 2.0, 300, 1000);
    let body = by_id(&root, "body");
    let next = by_id(&root, "next");
    // One unbounded line would be about 20px high; wrapped at 300px it takes several.
    assert!(
        body.bounds.height > 40,
        "wrapped text should be several lines high: {:?}",
        body.bounds
    );
    assert!(body.bounds.width <= 300, "{:?}", body.bounds);
    assert!(
        next.bounds.y >= body.bounds.y + body.bounds.height,
        "sibling overlaps wrapped text: body {:?} next {:?}",
        body.bounds,
        next.bounds
    );
}

#[test]
fn linear_layout_children_keep_fixed_dp_and_overflow() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <View android:id="@+id/header" android:layout_width="match_parent" android:layout_height="48dp"/>
            <View android:id="@+id/spacer" android:layout_width="match_parent" android:layout_height="3000dp"/>
            <Button android:id="@+id/footer" android:layout_width="match_parent" android:layout_height="48dp" android:text="Footer"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 3.0, 1080, 1920);
    assert_eq!(by_id(&root, "header").bounds.height, 144);
    assert_eq!(by_id(&root, "spacer").bounds.height, 9000);
    let footer = by_id(&root, "footer");
    assert_eq!(footer.bounds.height, 144);
    assert!(footer.bounds.y > 1920, "{:?}", footer.bounds);
    // Laid out below the fold: in the hierarchy, but not visible.
    assert!(!footer.visible);
    assert!(by_id(&root, "header").visible);
}

// Review MAJOR-3: a match_parent body under a toolbar gets the space that is left,
// as on Android (EXACTLY(parent - used)), so a bottom-anchored button stays on screen.
#[test]
fn match_parent_body_under_a_toolbar_keeps_its_bottom_button_on_screen() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <View android:id="@+id/toolbar" android:layout_width="match_parent" android:layout_height="56dp"/>
            <FrameLayout android:id="@+id/body" android:layout_width="match_parent" android:layout_height="match_parent" android:gravity="bottom">
                <Button android:id="@+id/save" android:layout_width="match_parent" android:layout_height="48dp" android:text="Save"/>
            </FrameLayout>
        </LinearLayout>"#
    );
    let root = render(&xml, 2.0, 1080, 1920);
    let body = by_id(&root, "body");
    assert_eq!(body.bounds.y, 112, "{:?}", body.bounds);
    assert_eq!(
        body.bounds.y + body.bounds.height,
        1920,
        "{:?}",
        body.bounds
    );
    let save = by_id(&root, "save");
    assert_eq!(
        save.bounds.y + save.bounds.height,
        1920,
        "{:?}",
        save.bounds
    );
    assert!(save.visible);
}

// Review MAJOR-3: a long wrap_content TextView in a horizontal row is AT_MOST(remaining)
// on Android, so it wraps and leaves room for the fixed button beside it.
#[test]
fn long_wrap_content_text_in_a_row_wraps() {
    let long = "This is a long sentence that cannot possibly fit on one line of a narrow screen";
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="horizontal">
            <TextView android:id="@+id/text" android:layout_width="wrap_content" android:layout_height="wrap_content" android:textSize="10dp" android:text="{long}"/>
            <Button android:id="@+id/go" android:layout_width="60dp" android:layout_height="40dp" android:text="Go"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 2.0, 600, 1000);
    let text = by_id(&root, "text");
    let go = by_id(&root, "go");
    assert!(text.bounds.width <= 600 - 120, "{:?}", text.bounds);
    assert!(text.bounds.height > 40, "should wrap: {:?}", text.bounds);
    assert_eq!(go.bounds.width, 120, "{:?}", go.bounds);
    assert!(go.bounds.x + go.bounds.width <= 600, "{:?}", go.bounds);
    assert!(go.visible);
}

#[test]
fn scroll_view_content_is_not_squashed() {
    let xml = format!(
        r#"<ScrollView {NS} android:layout_width="match_parent" android:layout_height="match_parent">
            <LinearLayout android:id="@+id/content" android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">
                <View android:id="@+id/a" android:layout_width="match_parent" android:layout_height="600dp"/>
                <View android:id="@+id/b" android:layout_width="match_parent" android:layout_height="600dp"/>
            </LinearLayout>
        </ScrollView>"#
    );
    let root = render(&xml, 2.0, 1080, 1920);
    assert_eq!(by_id(&root, "content").bounds.height, 2400);
    assert_eq!(by_id(&root, "b").bounds.y, 1200);
}

#[test]
fn invisible_keeps_layout_space_and_gone_does_not() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <View android:id="@+id/ghost" android:layout_width="match_parent" android:layout_height="50dp" android:visibility="invisible">
            </View>
            <View android:id="@+id/after_ghost" android:layout_width="match_parent" android:layout_height="10dp"/>
            <View android:id="@+id/gone" android:layout_width="match_parent" android:layout_height="50dp" android:visibility="gone"/>
            <View android:id="@+id/after_gone" android:layout_width="match_parent" android:layout_height="10dp"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 2.0, 1080, 1920);
    let ghost = by_id(&root, "ghost");
    assert!(!ghost.visible);
    assert_eq!(ghost.bounds.height, 100);
    assert_eq!(by_id(&root, "after_ghost").bounds.y, 100);
    assert_eq!(by_id(&root, "after_gone").bounds.y, 120);
    assert!(!by_id(&root, "gone").visible);
}

#[test]
fn children_of_an_invisible_view_are_not_visible() {
    let xml = format!(
        r#"<LinearLayout {NS} android:id="@+id/box" android:layout_width="match_parent" android:layout_height="100dp" android:visibility="invisible">
            <Button android:id="@+id/inner" android:layout_width="match_parent" android:layout_height="40dp" android:text="Inner"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 2.0, 1080, 1920);
    assert!(!by_id(&root, "inner").visible);
}

#[test]
fn density_config_is_honoured() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent">
            <View android:id="@+id/box" android:layout_width="100dp" android:layout_height="100dp"/>
        </LinearLayout>"#
    );
    let root = render(&xml, 3.0, 1080, 1920);
    let b = by_id(&root, "box").bounds;
    assert_eq!((b.width, b.height), (300, 300));
}

#[test]
fn nesting_beyond_the_cap_is_an_error_not_a_crash() {
    let depth = atalanta_headless::android::inflate::MAX_VIEW_DEPTH + 10;
    let mut xml = String::new();
    for _ in 0..depth {
        xml.push_str(r#"<FrameLayout android:layout_width="match_parent" android:layout_height="match_parent">"#);
    }
    for _ in 0..depth {
        xml.push_str("</FrameLayout>");
    }
    let err = AndroidInflater::new(ResourceTable::empty())
        .inflate_xml(&xml)
        .expect_err("too deep");
    assert!(err.to_string().contains("deeper"), "{err}");
}
