// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::selector_match;
use atalanta_common::{Element, Rect, Selector};

/// Options controlling how selectors are matched.
pub struct MatchOptions {
    pub visible_only: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self { visible_only: true }
    }
}

// Matching lives in atalanta_common::selector_match so every platform
// resolves selectors the same way; these keep the Android call sites.

/// The element `selector` resolves to (innermost match; `index` counts by
/// screen position).
pub fn find_element<'a>(
    root: &'a Element,
    selector: &Selector,
    opts: &MatchOptions,
    screen: &Rect,
) -> Option<&'a Element> {
    selector_match::find_ref(root, selector, opts.visible_only.then_some(screen))
}

/// Every element `selector` matches, in on-screen order.
pub fn find_all_elements<'a>(
    root: &'a Element,
    selector: &Selector,
    opts: &MatchOptions,
    screen: &Rect,
) -> Vec<&'a Element> {
    selector_match::find_all_ref(root, selector, opts.visible_only.then_some(screen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::Rect;

    fn make_screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        }
    }

    fn make_button() -> Element {
        Element {
            hint: None,
            platform_id: "com.app:id/login_button".to_string(),
            label: Some("Login".to_string()),
            text: Some("Log In".to_string()),
            element_type: "Button".to_string(),
            bounds: Rect {
                x: 120,
                y: 640,
                width: 840,
                height: 96,
            },
            enabled: true,
            visible: true,
            children: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn test_find_by_id() {
        let button = make_button();
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "FrameLayout".to_string(),
            bounds: make_screen(),
            enabled: true,
            visible: true,
            children: vec![button],
            ..Default::default()
        };

        let opts = MatchOptions::default();
        let screen = make_screen();
        let selector = Selector::Id("login_button".to_string());
        let found = find_element(&root, &selector, &opts, &screen);
        assert!(found.is_some());
        assert_eq!(found.unwrap().text.as_deref(), Some("Log In"));
    }

    #[test]
    fn test_find_by_text() {
        let button = make_button();
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "FrameLayout".to_string(),
            bounds: make_screen(),
            enabled: true,
            visible: true,
            children: vec![button],
            ..Default::default()
        };

        let opts = MatchOptions::default();
        let screen = make_screen();
        let selector = Selector::Text("Log In".to_string());
        let found = find_element(&root, &selector, &opts, &screen);
        assert!(found.is_some());
    }

    #[test]
    fn test_invisible_element_skipped() {
        let mut button = make_button();
        button.visible = false;

        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "FrameLayout".to_string(),
            bounds: make_screen(),
            enabled: true,
            visible: true,
            children: vec![button],
            ..Default::default()
        };

        let opts = MatchOptions { visible_only: true };
        let screen = make_screen();
        let selector = Selector::Id("login_button".to_string());
        let found = find_element(&root, &selector, &opts, &screen);
        assert!(found.is_none());
    }
}
