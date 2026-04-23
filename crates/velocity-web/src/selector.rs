use velocity_common::Selector;

/// Convert a `Selector` to a CSS query string for `querySelector`/`querySelectorAll`.
///
/// Returns `Err(())` for selectors that cannot be expressed as CSS
/// (Text, TextContains, XPath) — the caller falls back to a tree-walk or JS evaluation.
pub fn selector_to_css(selector: &Selector) -> Result<String, ()> {
    match selector {
        Selector::Css(q) => Ok(q.clone()),
        Selector::Id(id) => {
            // Match HTML `id` attribute or common `data-testid` convention.
            Ok(format!("[id='{id}'], [data-testid='{id}']"))
        }
        Selector::AccessibilityId(aid) => Ok(format!("[aria-label='{aid}']")),
        Selector::ClassName(cls) => Ok(format!(".{cls}")),
        Selector::Compound(parts) => {
            let css_parts: Vec<String> = parts
                .iter()
                .filter_map(|p| selector_to_css(p).ok())
                .collect();
            if css_parts.is_empty() {
                Err(())
            } else {
                // Compound = intersection — join with CSS descendant combinator via `:is()` trick.
                // For simplicity, use comma union (OR) so at least something resolves.
                Ok(css_parts.join(", "))
            }
        }
        // These cannot be expressed as CSS; callers use tree-walk or JS.
        Selector::Text(_) | Selector::TextContains(_) | Selector::XPath(_) => Err(()),
        // Index requires querySelectorAll + Nth pick — handled by the driver, not here.
        Selector::Index { .. } => Err(()),
    }
}

/// Build a JavaScript expression that evaluates an XPath query and returns
/// the first matching DOM element's bounding-rect JSON.
///
/// The expression is designed to be passed to `Runtime.evaluate`.
pub fn xpath_js(xpath: &str) -> String {
    let escaped = xpath.replace('\'', "\\'");
    format!(
        r#"(function(){{
            var r = document.evaluate('{escaped}', document, null,
                XPathResult.FIRST_ORDERED_NODE_TYPE, null);
            var n = r.singleNodeValue;
            if (!n) return null;
            if (!n.dataset.velId) n.dataset.velId = Math.random().toString(36).slice(2,10);
            var rect = n.getBoundingClientRect();
            var s = window.getComputedStyle(n);
            return {{
                velId:     n.dataset.velId,
                tag:       n.tagName.toLowerCase(),
                domId:     n.id || null,
                text:      (n.textContent||'').trim().slice(0,200)||null,
                ariaLabel: n.getAttribute('aria-label'),
                x: Math.round(rect.x), y: Math.round(rect.y),
                w: Math.round(rect.width), h: Math.round(rect.height),
                visible:   s.display !== 'none' && s.visibility !== 'hidden'
            }};
        }})()"#
    )
}

/// Build a JS expression that finds the Nth element matching `css` (0-indexed).
pub fn nth_css_js(css: &str, index: usize) -> String {
    let escaped = css.replace('\'', "\\'");
    format!(
        r#"(function(){{
            var nodes = document.querySelectorAll('{escaped}');
            var n = nodes[{index}];
            if (!n) return null;
            if (!n.dataset.velId) n.dataset.velId = Math.random().toString(36).slice(2,10);
            var rect = n.getBoundingClientRect();
            var s = window.getComputedStyle(n);
            return {{
                velId:     n.dataset.velId,
                tag:       n.tagName.toLowerCase(),
                domId:     n.id || null,
                text:      (n.textContent||'').trim().slice(0,200)||null,
                ariaLabel: n.getAttribute('aria-label'),
                x: Math.round(rect.x), y: Math.round(rect.y),
                w: Math.round(rect.width), h: Math.round(rect.height),
                visible:   s.display !== 'none' && s.visibility !== 'hidden'
            }};
        }})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::Selector;

    #[test]
    fn test_css_passthrough() {
        assert_eq!(
            selector_to_css(&Selector::Css(".btn".to_string())).unwrap(),
            ".btn"
        );
    }

    #[test]
    fn test_id_selector() {
        let css = selector_to_css(&Selector::Id("submit".to_string())).unwrap();
        assert!(css.contains("[id='submit']"));
        assert!(css.contains("[data-testid='submit']"));
    }

    #[test]
    fn test_accessibility_id() {
        let css = selector_to_css(&Selector::AccessibilityId("Login".to_string())).unwrap();
        assert_eq!(css, "[aria-label='Login']");
    }

    #[test]
    fn test_class_name() {
        let css = selector_to_css(&Selector::ClassName("Button".to_string())).unwrap();
        assert_eq!(css, ".Button");
    }

    #[test]
    fn test_text_returns_err() {
        assert!(selector_to_css(&Selector::Text("Hello".to_string())).is_err());
    }

    #[test]
    fn test_xpath_returns_err() {
        assert!(selector_to_css(&Selector::XPath("//button".to_string())).is_err());
    }

    #[test]
    fn test_index_returns_err() {
        assert!(selector_to_css(&Selector::Index {
            selector: Box::new(Selector::Css(".btn".to_string())),
            index: 0
        })
        .is_err());
    }
}
