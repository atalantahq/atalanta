// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

/// Parsed XIB document containing a view hierarchy.
#[derive(Debug, Clone)]
pub struct XibDocument {
    pub objects: Vec<XibObject>,
    pub connections: Vec<XibConnection>,
}

/// An object in the XIB (UIView, UILabel, etc.).
#[derive(Debug, Clone)]
pub struct XibObject {
    pub class: String,
    pub id: String,
    pub properties: HashMap<String, String>,
    pub subviews: Vec<XibObject>,
    pub constraints: Vec<XibConstraint>,
    /// Frame rectangle if specified.
    pub frame: Option<XibFrame>,
}

/// A frame rectangle.
#[derive(Debug, Clone, Copy)]
pub struct XibFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A layout constraint from the XIB.
#[derive(Debug, Clone)]
pub struct XibConstraint {
    pub id: String,
    pub first_item: Option<String>,
    pub first_attribute: String,
    pub second_item: Option<String>,
    pub second_attribute: Option<String>,
    pub relation: String,
    pub constant: f32,
    pub multiplier: f32,
}

/// An outlet/action connection.
#[derive(Debug, Clone)]
pub struct XibConnection {
    pub kind: String,
    pub property: String,
    pub destination: String,
}

/// Parser for XIB (XML Interface Builder) files.
pub struct XibParser;

impl XibParser {
    /// Parse a XIB XML string into a document.
    pub fn parse(xml: &str) -> Result<XibDocument, XibError> {
        let mut reader = Reader::from_str(xml);
        let mut objects = Vec::new();
        let connections = Vec::new();
        let mut stack: Vec<XibObject> = Vec::new();
        let mut current_string_key: Option<String> = None;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    let attrs = Self::parse_attributes(&e);

                    match tag.as_str() {
                        "view" | "label" | "imageView" | "stackView" | "scrollView" | "button"
                        | "textField" | "switch" | "slider" | "tableView" => {
                            // Dropped recursively: refuse a nesting that would
                            // overflow the stack before it is built.
                            if stack.len() >= crate::android::inflate::MAX_VIEW_DEPTH {
                                return Err(XibError::TooDeep(
                                    crate::android::inflate::MAX_VIEW_DEPTH,
                                ));
                            }
                            let obj = Self::create_object(&tag, &attrs);
                            stack.push(obj);
                        }
                        "constraint" => {
                            let constraint = Self::parse_constraint(&attrs);
                            if let Some(parent) = stack.last_mut() {
                                parent.constraints.push(constraint);
                            }
                        }
                        "rect" if attrs.contains_key("key") => {
                            if attrs.get("key").map(|k| k == "frame").unwrap_or(false) {
                                let frame = XibFrame {
                                    x: attrs.get("x").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                                    y: attrs.get("y").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                                    width: attrs
                                        .get("width")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0.0),
                                    height: attrs
                                        .get("height")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0.0),
                                };
                                if let Some(parent) = stack.last_mut() {
                                    parent.frame = Some(frame);
                                }
                            }
                        }
                        "color" if attrs.contains_key("key") => {
                            let key = attrs.get("key").unwrap().clone();
                            if let Some(parent) = stack.last_mut() {
                                if let Some(hex) = Self::color_to_hex(&attrs) {
                                    parent.properties.insert(format!("color_{}", key), hex);
                                }
                            }
                        }
                        // A button's title lives in `<state key="normal" title=…>` (VEL-105).
                        "state" if attrs.get("key").map(String::as_str) == Some("normal") => {
                            if let (Some(parent), Some(title)) =
                                (stack.last_mut(), attrs.get("title"))
                            {
                                parent
                                    .properties
                                    .entry("title".to_string())
                                    .or_insert_with(|| title.clone());
                            }
                        }
                        "fontDescription" => {
                            if let Some(parent) = stack.last_mut() {
                                if let Some(size) = attrs.get("pointSize") {
                                    parent
                                        .properties
                                        .insert("fontSize".to_string(), size.clone());
                                }
                                if let Some(name) = attrs.get("name") {
                                    parent
                                        .properties
                                        .insert("fontName".to_string(), name.clone());
                                }
                                for key in ["type", "weight"] {
                                    if let Some(v) = attrs.get(key) {
                                        parent.properties.insert(format!("font_{key}"), v.clone());
                                    }
                                }
                            }
                        }
                        "textInputTraits" => {
                            if let (Some(parent), Some(v)) =
                                (stack.last_mut(), attrs.get("secureTextEntry"))
                            {
                                parent
                                    .properties
                                    .insert("secureTextEntry".to_string(), v.clone());
                            }
                        }
                        "string" if attrs.contains_key("key") => {
                            // Text content will be handled via text content parsing
                            let key = attrs.get("key").unwrap().clone();
                            if let Some(parent) = stack.last_mut() {
                                // Will be filled by text content
                                parent.properties.insert(key, String::new());
                            }
                            current_string_key = Some(attrs.get("key").unwrap().clone());
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    let attrs = Self::parse_attributes(&e);

                    match tag.as_str() {
                        "view" | "label" | "imageView" | "stackView" | "scrollView" | "button"
                        | "textField" | "switch" | "slider" | "tableView" => {
                            let obj = Self::create_object(&tag, &attrs);
                            if let Some(parent) = stack.last_mut() {
                                parent.subviews.push(obj);
                            } else {
                                objects.push(obj);
                            }
                        }
                        "constraint" => {
                            let constraint = Self::parse_constraint(&attrs);
                            if let Some(parent) = stack.last_mut() {
                                parent.constraints.push(constraint);
                            }
                        }
                        "rect" if attrs.contains_key("key") => {
                            if attrs.get("key").map(|k| k == "frame").unwrap_or(false) {
                                let frame = XibFrame {
                                    x: attrs.get("x").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                                    y: attrs.get("y").and_then(|v| v.parse().ok()).unwrap_or(0.0),
                                    width: attrs
                                        .get("width")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0.0),
                                    height: attrs
                                        .get("height")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0.0),
                                };
                                if let Some(parent) = stack.last_mut() {
                                    parent.frame = Some(frame);
                                }
                            }
                        }
                        "color" if attrs.contains_key("key") => {
                            let key = attrs.get("key").unwrap().clone();
                            if let Some(parent) = stack.last_mut() {
                                if let Some(hex) = Self::color_to_hex(&attrs) {
                                    parent.properties.insert(format!("color_{}", key), hex);
                                }
                            }
                        }
                        // A button's title lives in `<state key="normal" title=…>` (VEL-105).
                        "state" if attrs.get("key").map(String::as_str) == Some("normal") => {
                            if let (Some(parent), Some(title)) =
                                (stack.last_mut(), attrs.get("title"))
                            {
                                parent
                                    .properties
                                    .entry("title".to_string())
                                    .or_insert_with(|| title.clone());
                            }
                        }
                        "fontDescription" => {
                            if let Some(parent) = stack.last_mut() {
                                if let Some(size) = attrs.get("pointSize") {
                                    parent
                                        .properties
                                        .insert("fontSize".to_string(), size.clone());
                                }
                                if let Some(name) = attrs.get("name") {
                                    parent
                                        .properties
                                        .insert("fontName".to_string(), name.clone());
                                }
                                for key in ["type", "weight"] {
                                    if let Some(v) = attrs.get(key) {
                                        parent.properties.insert(format!("font_{key}"), v.clone());
                                    }
                                }
                            }
                        }
                        "textInputTraits" => {
                            if let (Some(parent), Some(v)) =
                                (stack.last_mut(), attrs.get("secureTextEntry"))
                            {
                                parent
                                    .properties
                                    .insert("secureTextEntry".to_string(), v.clone());
                            }
                        }
                        "string" if attrs.contains_key("key") => {
                            let key = attrs.get("key").unwrap().clone();
                            if let Some(parent) = stack.last_mut() {
                                parent.properties.insert(key, String::new());
                            }
                        }
                        _ => {}
                    }
                }
                // Since quick-xml 0.38 a text with an entity in it comes as
                // pieces (`Line one `, `&amp;`, ` two`): append each one.
                Ok(Event::Text(e)) => {
                    if let (Some(parent), Some(key)) =
                        (stack.last_mut(), current_string_key.as_ref())
                    {
                        let text = e
                            .decode()
                            .map(|t| t.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(e.as_ref()).to_string());
                        if !text.is_empty() {
                            parent
                                .properties
                                .entry(key.clone())
                                .or_default()
                                .push_str(&text);
                        }
                    }
                }
                Ok(Event::GeneralRef(e)) => {
                    if let (Some(parent), Some(key)) =
                        (stack.last_mut(), current_string_key.as_ref())
                    {
                        let name = String::from_utf8_lossy(e.as_ref()).to_string();
                        let resolved = match e.resolve_char_ref() {
                            Ok(Some(c)) => c.to_string(),
                            _ => quick_xml::escape::resolve_predefined_entity(&name)
                                .map_or_else(|| format!("&{name};"), str::to_string),
                        };
                        parent
                            .properties
                            .entry(key.clone())
                            .or_default()
                            .push_str(&resolved);
                    }
                }
                Ok(Event::End(e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();

                    match tag.as_str() {
                        "string" => {
                            current_string_key = None;
                        }
                        "view" | "label" | "imageView" | "stackView" | "scrollView" | "button"
                        | "textField" | "switch" | "slider" | "tableView" => {
                            if let Some(obj) = stack.pop() {
                                if let Some(parent) = stack.last_mut() {
                                    parent.subviews.push(obj);
                                } else {
                                    objects.push(obj);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(XibError::ParseError(e.to_string())),
                _ => {}
            }
        }

        Ok(XibDocument {
            objects,
            connections,
        })
    }

    fn parse_attributes(event: &quick_xml::events::BytesStart) -> HashMap<String, String> {
        let mut attrs = HashMap::new();
        for attr in event.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            // `Terms &amp; Conditions` is the text `Terms & Conditions` (VEL-104).
            let val = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).to_string());
            attrs.insert(key, val);
        }
        attrs
    }

    fn create_object(tag: &str, attrs: &HashMap<String, String>) -> XibObject {
        let class = match tag {
            "view" => "UIView",
            "label" => "UILabel",
            "imageView" => "UIImageView",
            "stackView" => "UIStackView",
            "scrollView" => "UIScrollView",
            "button" => "UIButton",
            "textField" => "UITextField",
            "switch" => "UISwitch",
            "slider" => "UISlider",
            "tableView" => "UITableView",
            _ => tag,
        };

        let mut properties = HashMap::new();

        // Copy relevant attributes
        for (key, val) in attrs {
            match key.as_str() {
                "text"
                | "placeholder"
                | "title"
                | "numberOfLines"
                | "textAlignment"
                | "contentMode"
                | "axis"
                | "distribution"
                | "alignment"
                | "spacing"
                | "hidden"
                | "userInteractionEnabled"
                | "opaque"
                | "alpha"
                | "translatesAutoresizingMaskIntoConstraints"
                | "accessibilityIdentifier"
                | "accessibilityLabel" => {
                    properties.insert(key.clone(), val.clone());
                }
                _ => {}
            }
        }

        XibObject {
            class: class.to_string(),
            id: attrs.get("id").cloned().unwrap_or_default(),
            properties,
            subviews: Vec::new(),
            constraints: Vec::new(),
            frame: None,
        }
    }

    fn parse_constraint(attrs: &HashMap<String, String>) -> XibConstraint {
        XibConstraint {
            id: attrs.get("id").cloned().unwrap_or_default(),
            first_item: attrs.get("firstItem").cloned(),
            first_attribute: attrs.get("firstAttribute").cloned().unwrap_or_default(),
            second_item: attrs.get("secondItem").cloned(),
            second_attribute: attrs.get("secondAttribute").cloned(),
            relation: attrs
                .get("relation")
                .cloned()
                .unwrap_or_else(|| "equal".to_string()),
            constant: attrs
                .get("constant")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            multiplier: attrs
                .get("multiplier")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),
        }
    }

    fn color_to_hex(attrs: &HashMap<String, String>) -> Option<String> {
        // Handle named system colors
        if let Some(name) = attrs.get("systemColor") {
            return Some(system_color_hex(name).to_string());
        }

        // Grayscale colours (`colorSpace="calibratedWhite"`), e.g. `white="1"` (VEL-105).
        if let Some(white) = attrs.get("white").and_then(|v| v.parse::<f32>().ok()) {
            let w = (white.clamp(0.0, 1.0) * 255.0).round() as u8;
            let a = attrs
                .get("alpha")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(1.0);
            let a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            return Some(format!("#{:02x}{:02x}{:02x}{:02x}", a, w, w, w));
        }

        // Handle custom RGBA colors
        if let (Some(r), Some(g), Some(b)) =
            (attrs.get("red"), attrs.get("green"), attrs.get("blue"))
        {
            let r = (r.parse::<f32>().unwrap_or(0.0) * 255.0) as u8;
            let g = (g.parse::<f32>().unwrap_or(0.0) * 255.0) as u8;
            let b = (b.parse::<f32>().unwrap_or(0.0) * 255.0) as u8;
            let a = attrs
                .get("alpha")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(1.0);
            let a = (a * 255.0) as u8;
            return Some(format!("#{:02x}{:02x}{:02x}{:02x}", a, r, g, b));
        }

        attrs
            .get("customColorSpace")
            .map(|_| "#FF000000".to_string())
    }
}

/// UIKit's dynamic system colours in light mode, as `#AARRGGBB`.
fn system_color_hex(name: &str) -> &'static str {
    match name {
        "systemBackgroundColor"
        | "secondarySystemGroupedBackgroundColor"
        | "tertiarySystemBackgroundColor" => "#FFFFFFFF",
        "secondarySystemBackgroundColor"
        | "systemGroupedBackgroundColor"
        | "tertiarySystemGroupedBackgroundColor" => "#FFF2F2F7",
        "labelColor" | "darkTextColor" => "#FF000000",
        "secondaryLabelColor" => "#993C3C43",
        "tertiaryLabelColor" => "#4D3C3C43",
        "quaternaryLabelColor" => "#2E3C3C43",
        "placeholderTextColor" => "#4D3C3C43",
        "separatorColor" => "#4A3C3C43",
        "opaqueSeparatorColor" => "#FFC6C6C8",
        "linkColor" | "systemBlueColor" | "tintColor" => "#FF007AFF",
        "systemRedColor" => "#FFFF3B30",
        "systemGreenColor" => "#FF34C759",
        "systemOrangeColor" => "#FFFF9500",
        "systemYellowColor" => "#FFFFCC00",
        "systemPinkColor" => "#FFFF2D55",
        "systemPurpleColor" => "#FFAF52DE",
        "systemTealColor" => "#FF30B0C7",
        "systemIndigoColor" => "#FF5856D6",
        "systemGrayColor" => "#FF8E8E93",
        "systemGray2Color" => "#FFAEAEB2",
        "systemGray3Color" => "#FFC7C7CC",
        "systemGray4Color" => "#FFD1D1D6",
        "systemGray5Color" => "#FFE5E5EA",
        "systemGray6Color" => "#FFF2F2F7",
        "whiteColor" | "lightTextColor" => "#FFFFFFFF",
        "clearColor" => "#00000000",
        _ => "#FF000000",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum XibError {
    #[error("XIB parse error: {0}")]
    ParseError(String),
    #[error("No views found in XIB")]
    NoViews,
    #[error("XIB nests views deeper than {0} levels")]
    TooDeep(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_xib() {
        let xib = r#"<?xml version="1.0" encoding="UTF-8"?>
        <document type="com.apple.InterfaceBuilder3.CocoaTouch.XIB" version="3.0">
            <objects>
                <view id="root-view" userLabel="Root View">
                    <rect key="frame" x="0" y="0" width="375" height="812"/>
                    <subviews>
                        <label id="title-label" text="Hello World">
                            <rect key="frame" x="20" y="100" width="335" height="21"/>
                        </label>
                        <imageView id="hero-image" contentMode="scaleAspectFit">
                            <rect key="frame" x="20" y="130" width="335" height="200"/>
                        </imageView>
                    </subviews>
                </view>
            </objects>
        </document>"#;

        let doc = XibParser::parse(xib).unwrap();
        assert!(!doc.objects.is_empty());

        let root = &doc.objects[0];
        assert_eq!(root.class, "UIView");
        assert_eq!(root.subviews.len(), 2);

        let label = &root.subviews[0];
        assert_eq!(label.class, "UILabel");
        assert_eq!(label.properties.get("text").unwrap(), "Hello World");

        let image = &root.subviews[1];
        assert_eq!(image.class, "UIImageView");
    }

    #[test]
    fn test_nested_self_closing_subviews_are_preserved() {
        let xib = r#"<?xml version="1.0" encoding="UTF-8"?>
        <document type="com.apple.InterfaceBuilder3.CocoaTouch.XIB" version="3.0">
            <objects>
                <view id="root-view">
                    <subviews>
                        <view id="content-view">
                            <subviews>
                                <label id="title-label" text="Hello"/>
                                <button id="cta-button" title="Tap"/>
                            </subviews>
                        </view>
                    </subviews>
                </view>
            </objects>
        </document>"#;

        let doc = XibParser::parse(xib).unwrap();
        let root = &doc.objects[0];
        assert_eq!(root.subviews.len(), 1);

        let content = &root.subviews[0];
        assert_eq!(content.class, "UIView");
        assert_eq!(content.subviews.len(), 2);

        let label = &content.subviews[0];
        assert_eq!(label.class, "UILabel");
        assert_eq!(label.properties.get("text"), Some(&"Hello".to_string()));

        let button = &content.subviews[1];
        assert_eq!(button.class, "UIButton");
        assert_eq!(button.properties.get("title"), Some(&"Tap".to_string()));
    }

    #[test]
    fn xib_nested_past_the_limit_is_an_error_not_a_crash() {
        let n = 100_000;
        let xml = format!("{}{}", "<view>".repeat(n), "</view>".repeat(n));
        assert!(matches!(XibParser::parse(&xml), Err(XibError::TooDeep(_))));
    }
}
