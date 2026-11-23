// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! XML entities, XIB colours and button titles (VEL-104, VEL-105).

use atalanta_headless::android::resources::ResourceTable;
use atalanta_headless::android::AndroidInflater;
use atalanta_headless::ios::resources::IosResourceLoader;
use atalanta_headless::ios::storyboard::StoryboardParser;
use atalanta_headless::ios::{IosInflater, XibParser};
use atalanta_headless::render_tree::Dimension;
use atalanta_headless::RenderNode;

fn find<'a>(n: &'a RenderNode, pred: &dyn Fn(&RenderNode) -> bool) -> Option<&'a RenderNode> {
    if pred(n) {
        return Some(n);
    }
    n.children.iter().find_map(|c| find(c, pred))
}

#[test]
fn xml_entities_in_android_text() {
    let xml = r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" android:layout_width="match_parent" android:layout_height="match_parent">
        <TextView android:id="@+id/t" android:text="Terms &amp; Conditions &lt;3 &quot;now&quot;"/>
    </LinearLayout>"#;
    let tree = AndroidInflater::new(ResourceTable::empty())
        .inflate_xml(xml)
        .unwrap();
    let t = find(&tree, &|n| n.id.as_deref() == Some("t")).unwrap();
    assert_eq!(t.text.as_deref(), Some(r#"Terms & Conditions <3 "now""#));
}

#[test]
fn xml_entities_in_xib_attributes() {
    let xib = r#"<document><objects><view id="root"><subviews>
        <label id="l" text="Tom &amp; Jerry"/>
    </subviews></view></objects></document>"#;
    let doc = XibParser::parse(xib).unwrap();
    let tree = IosInflater::new(IosResourceLoader::empty())
        .inflate(&doc)
        .unwrap();
    let l = find(&tree, &|n| n.id.as_deref() == Some("l")).unwrap();
    assert_eq!(l.text.as_deref(), Some("Tom & Jerry"));
}

#[test]
fn storyboard_text_and_entities() {
    let sb = r#"<document initialViewController="vc"><scenes><scene sceneID="s1"><objects>
        <viewController id="vc"><view key="view" id="root"><subviews>
            <label id="a" text="Tom &amp; Jerry"/>
            <label id="b"><string key="text">Line one &amp; two</string></label>
        </subviews></view></viewController>
    </objects></scene></scenes></document>"#;
    let doc = StoryboardParser::parse(sb).unwrap();
    let tree = IosInflater::new(IosResourceLoader::empty())
        .inflate(&doc)
        .unwrap();
    let a = find(&tree, &|n| n.id.as_deref() == Some("a")).expect("label a");
    assert_eq!(a.text.as_deref(), Some("Tom & Jerry"));
    let b = find(&tree, &|n| n.id.as_deref() == Some("b")).expect("text view b");
    assert_eq!(b.text.as_deref(), Some("Line one & two"));
}

#[test]
fn xib_white_and_system_colours() {
    let xib = r#"<document><objects><view id="root">
        <color key="backgroundColor" white="1" alpha="1" colorSpace="custom" customColorSpace="genericGamma22GrayColorSpace"/>
        <subviews>
            <view id="grey"><color key="backgroundColor" systemColor="systemGray5Color"/></view>
            <view id="half"><color key="backgroundColor" white="0.5" alpha="0.5" colorSpace="calibratedWhite"/></view>
        </subviews>
    </view></objects></document>"#;
    let doc = XibParser::parse(xib).unwrap();
    let root = &doc.objects[0];
    assert_eq!(
        root.properties
            .get("color_backgroundColor")
            .map(String::as_str),
        Some("#ffffffff")
    );
    assert_eq!(
        root.subviews[0]
            .properties
            .get("color_backgroundColor")
            .map(String::as_str),
        Some("#FFE5E5EA")
    );
    assert_eq!(
        root.subviews[1]
            .properties
            .get("color_backgroundColor")
            .map(String::as_str),
        Some("#80808080")
    );
}

#[test]
fn xib_button_title_comes_from_its_normal_state() {
    let xib = r#"<document><objects><view id="root"><subviews>
        <button id="login" accessibilityIdentifier="login">
            <state key="normal" title="Log In"/>
            <state key="highlighted" title="Logging"/>
        </button>
    </subviews></view></objects></document>"#;
    let doc = XibParser::parse(xib).unwrap();
    let tree = IosInflater::new(IosResourceLoader::empty())
        .inflate(&doc)
        .unwrap();
    let b = find(&tree, &|n| n.id.as_deref() == Some("login")).unwrap();
    assert_eq!(b.text.as_deref(), Some("Log In"));
}

/// VEL-S23: Interface Builder stores a subview's constraints on the
/// superview; they were ignored, and the superview took the subview's width.
#[test]
fn xib_constraints_stored_on_the_superview_apply_to_the_subview() {
    let xib = r#"<document><objects><view id="root">
        <rect key="frame" x="0" y="0" width="390" height="844"/>
        <subviews>
            <label id="l" text="Hello"/>
        </subviews>
        <constraints>
            <constraint firstItem="l" firstAttribute="width" constant="120" id="c1"/>
            <constraint firstItem="l" firstAttribute="leading" secondItem="root" secondAttribute="leading" constant="16" id="c2"/>
            <constraint firstAttribute="trailing" secondItem="l" secondAttribute="trailing" constant="20" id="c3"/>
        </constraints>
    </view></objects></document>"#;
    let doc = XibParser::parse(xib).unwrap();
    let tree = IosInflater::new(IosResourceLoader::empty())
        .inflate(&doc)
        .unwrap();
    let l = find(&tree, &|n| n.id.as_deref() == Some("l")).unwrap();
    assert_eq!(l.style.width, Dimension::length(120.0));
    assert_eq!(l.style.margin.left, 16.0);
    assert_eq!(l.style.margin.right, 20.0, "superview-first trailing pin");
    assert_eq!(
        tree.style.width,
        Dimension::length(390.0),
        "root keeps its frame"
    );
    assert_eq!(tree.style.margin.right, 0.0);
}
