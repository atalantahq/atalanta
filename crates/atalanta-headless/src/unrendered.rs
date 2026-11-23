//! D-47: headless is an approximate smoke renderer. It lays out and paints
//! boxes, colours, text, opacity and corner radius; attributes it parses
//! but can't draw (images, shadows, transforms, custom fonts...) produce one
//! warning per attribute, instead of a render that silently differs from
//! the device.

use std::collections::{BTreeSet, HashMap};

/// Android view attributes headless reads past without drawing.
const ANDROID: &[&str] = &[
    "elevation",
    "translationZ",
    "translationX",
    "translationY",
    "rotation",
    "rotationX",
    "rotationY",
    "scaleX",
    "scaleY",
    "src",
    "srcCompat",
    "tint",
    "backgroundTint",
    "foreground",
    "drawableLeft",
    "drawableStart",
    "drawableRight",
    "drawableEnd",
    "drawableTop",
    "drawableBottom",
    "shadowColor",
    "shadowRadius",
    "shadowDx",
    "shadowDy",
    "fontFamily",
    "letterSpacing",
    "textAllCaps",
];

/// The attributes of one Android view that headless won't draw, named as
/// in the layout (`android:elevation`). A `background` that isn't a colour
/// (a drawable, a gradient) and an italic `textStyle` count too.
pub fn android(attrs: &HashMap<String, String>, background_is_colour: bool) -> Vec<String> {
    let mut out: Vec<String> = ANDROID
        .iter()
        .filter(|a| attrs.contains_key(**a))
        .map(|a| format!("android:{a}"))
        .collect();
    if attrs.contains_key("background") && !background_is_colour {
        out.push("android:background (drawable)".into());
    }
    if attrs.get("textStyle").is_some_and(|v| {
        v.split('|').any(|f| f.trim() == "italic") || v.trim() == "2" || v.trim() == "3"
    }) {
        out.push("android:textStyle italic".into());
    }
    out
}

/// UIKit properties (as the XIB parser stores them) headless won't draw.
const IOS: &[(&str, &str)] = &[
    ("image", "image"),
    ("highlightedImage", "highlightedImage"),
    ("backgroundImage", "backgroundImage"),
    ("font_name", "custom font (fontDescription name)"),
    ("font_family", "custom font (fontDescription family)"),
    ("color_shadowColor", "shadowColor"),
];

/// The properties of one XIB object that headless won't draw.
pub fn ios(props: &HashMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = IOS
        .iter()
        .filter(|(k, _)| props.contains_key(*k))
        .map(|(_, name)| (*name).to_string())
        .collect();
    if props
        .get("font_type")
        .is_some_and(|t| t.to_ascii_lowercase().contains("italic"))
    {
        out.push("italic font".into());
    }
    out
}

/// The warning for an attribute headless can't draw.
pub fn message(attr: &str) -> String {
    format!(
        "headless is an approximate smoke renderer and doesn't draw {attr}: screenshots and \
         visual baselines from headless runs won't show it (warned once per attribute)"
    )
}

/// What one run has already warned about, so each attribute warns once.
#[derive(Default)]
pub struct Warned(pub(crate) std::sync::Mutex<BTreeSet<String>>);

impl Warned {
    /// The messages for the attributes in `seen` not warned about yet,
    /// each also logged.
    pub fn note(&self, seen: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut warned = self.0.lock().unwrap_or_else(|e| e.into_inner());
        seen.into_iter()
            .filter(|a| warned.insert(a.clone()))
            .map(|a| {
                let m = message(&a);
                tracing::warn!("{m}");
                m
            })
            .collect()
    }
}
