// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Mobile-web device profiles for `web.device` (D-W3): Playwright's whole
//! device descriptor list, each a viewport, a device pixel ratio, a user
//! agent, and whether the page is laid out as mobile and takes touch. They
//! emulate the device's responsive layout in Chrome; an iPhone profile is
//! not Safari.
//!
//! The list is Playwright's `deviceDescriptorsSource.json`, kept as data in
//! `data/playwright-devices.json` (Apache-2.0, Copyright (c) Microsoft
//! Corporation; see `NOTICE` and `data/playwright-LICENSE`). Refreshing it
//! is a copy of that file from a newer `playwright-core`.
//!
//! The nine names Atalanta knew before (W1) still work: all but `iPad` are
//! Playwright names too, and `iPad` is an alias of `iPad (gen 7)`, the
//! 810x1080 iPad it was.

use std::sync::OnceLock;

/// One device descriptor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebDevice {
    /// The name `web.device` takes (matched ignoring case and spaces).
    pub name: &'static str,
    /// Viewport in CSS pixels.
    pub width: u32,
    pub height: u32,
    /// `window.devicePixelRatio`.
    pub dpr: f64,
    pub user_agent: &'static str,
    /// Laid out as a phone: the meta viewport applies, scrollbars overlay.
    pub mobile: bool,
    /// Touch events, `(pointer: coarse)`, `maxTouchPoints > 0`; taps are
    /// sent as touches.
    pub touch: bool,
    /// The browser Playwright pairs the descriptor with (`chromium`,
    /// `webkit`, `firefox`). Informational: Atalanta drives Chrome.
    pub browser: &'static str,
}

/// Playwright's descriptor list, verbatim.
const PLAYWRIGHT_DEVICES: &str = include_str!("../data/playwright-devices.json");

/// Names Atalanta used before W1 that Playwright doesn't have, and the
/// Playwright descriptor each one is.
pub const ALIASES: &[(&str, &str)] = &[("iPad", "iPad (gen 7)")];

/// The nine names `web.device` documented before the full list (W1).
pub const LEGACY_NAMES: [&str; 9] = [
    "iPhone 15",
    "iPhone 15 Pro Max",
    "iPhone SE",
    "Pixel 7",
    "Pixel 5",
    "Galaxy S24",
    "iPad",
    "iPad Mini",
    "iPad Pro 11",
];

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw<'a> {
    user_agent: &'a str,
    viewport: Size,
    device_scale_factor: f64,
    is_mobile: bool,
    has_touch: bool,
    default_browser_type: &'a str,
}

#[derive(serde::Deserialize)]
struct Size {
    width: u32,
    height: u32,
}

/// Every device `web.device` knows, in Playwright's order, then the
/// aliases.
pub fn web_devices() -> &'static [WebDevice] {
    static DEVICES: OnceLock<Vec<WebDevice>> = OnceLock::new();
    DEVICES.get_or_init(|| {
        // Borrowed from the embedded text: the names and user agents carry
        // no JSON escapes (a test checks the whole list parses).
        let parsed: Vec<(&'static str, Raw<'static>)> =
            serde_json::from_str::<ordered::Ordered>(PLAYWRIGHT_DEVICES)
                .expect("playwright-devices.json parses")
                .0;
        let mut out: Vec<WebDevice> = Vec::with_capacity(parsed.len() + ALIASES.len());
        for (name, r) in parsed {
            out.push(WebDevice {
                name,
                width: r.viewport.width,
                height: r.viewport.height,
                dpr: r.device_scale_factor,
                user_agent: r.user_agent,
                mobile: r.is_mobile,
                touch: r.has_touch,
                browser: r.default_browser_type,
            });
        }
        for (alias, target) in ALIASES {
            if let Some(d) = out.iter().find(|d| d.name == *target).copied() {
                out.push(WebDevice { name: alias, ..d });
            }
        }
        out
    })
}

/// The JSON object's entries in file order, borrowing from the embedded
/// text (serde_json's `Map` sorts its keys).
mod ordered {
    use super::Raw;
    use serde::de::{MapAccess, Visitor};

    pub struct Ordered<'a>(pub Vec<(&'a str, Raw<'a>)>);

    impl<'de> serde::Deserialize<'de> for Ordered<'de> {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Ordered<'de>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a map of device descriptors")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered<'de>, A::Error> {
                    let mut out = Vec::new();
                    while let Some(entry) = m.next_entry::<&'de str, Raw<'de>>()? {
                        out.push(entry);
                    }
                    Ok(Ordered(out))
                }
            }
            d.deserialize_map(V)
        }
    }
}

fn key(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The descriptor called `name` (`"Pixel 7"`, `"pixel7"`, `"iphone 15"`,
/// `"iPhone 15 landscape"`).
pub fn web_device(name: &str) -> Option<&'static WebDevice> {
    let want = key(name);
    web_devices().iter().find(|d| key(d.name) == want)
}

/// The descriptor called `name`, or the config error that names the
/// closest known devices.
pub fn lookup(name: &str) -> crate::Result<&'static WebDevice> {
    web_device(name).ok_or_else(|| {
        let want = key(name);
        // The names sharing the first word (`iPhone …`, `Pixel …`), else
        // the classic nine: the whole list is ~200 names.
        let first = name
            .split_whitespace()
            .next()
            .map(|w| w.to_lowercase())
            .unwrap_or_default();
        let mut near: Vec<&str> = web_devices()
            .iter()
            .map(|d| d.name)
            .filter(|n| {
                !first.is_empty()
                    && n.to_lowercase().starts_with(&first)
                    && !n.ends_with("landscape")
            })
            .collect();
        if want.is_empty() {
            near.clear();
        }
        // The common ones too, so the usual choices are always named.
        for n in LEGACY_NAMES {
            if !near.contains(&n) {
                near.push(n);
            }
        }
        crate::AtalantaError::Config(format!(
            "web.device: unknown device {name:?}; known devices include: {} \
             ({} in all: Playwright's device list, plus \"<name> landscape\")",
            near.join(", "),
            web_devices().len()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_loosely_and_unknown_ones_list_the_known() {
        assert_eq!(web_device("Pixel 7").unwrap().width, 412);
        assert_eq!(web_device("pixel7").unwrap().name, "Pixel 7");
        assert_eq!(web_device(" iPhone  15 ").unwrap().dpr, 3.0);
        assert!(web_device("iPhone 15 Pro Max").unwrap().touch);
        let e = lookup("Nokia 3310").unwrap_err().to_string();
        assert!(
            e.contains("Nokia 3310") && e.contains("Nokia Lumia 520") && e.contains("in all"),
            "{e}"
        );
        let e = lookup("x").unwrap_err().to_string();
        assert!(e.contains("Pixel 7, ") && e.contains("iPad"), "{e}");
        assert!(matches!(lookup("x"), Err(crate::AtalantaError::Config(_))));
    }

    /// W1: the whole Playwright list, in its order, every entry sane.
    #[test]
    fn the_full_playwright_list_is_there() {
        let all = web_devices();
        assert!(all.len() > 150, "only {} devices", all.len());
        assert_eq!(all[0].name, "Blackberry PlayBook");
        for d in all {
            assert!(d.width > 0 && d.height > 0 && d.dpr > 0.0, "{d:?}");
            assert!(d.user_agent.starts_with("Mozilla/5.0"), "{d:?}");
            assert!(["chromium", "webkit", "firefox"].contains(&d.browser));
        }
        let pro = web_device("Pixel 9 Pro XL").unwrap();
        assert!(pro.mobile && pro.touch);
        let land = web_device("iPhone 15 landscape").unwrap();
        assert!(land.width > land.height);
        let desk = web_device("desktop chrome").unwrap();
        assert!(!desk.mobile && !desk.touch);
        assert_eq!((desk.width, desk.height), (1280, 720));
    }

    /// The nine names that existed before W1 still resolve, to the
    /// viewports they had.
    #[test]
    fn the_legacy_names_still_resolve() {
        let want: [(&str, u32, u32, f64); 9] = [
            ("iPhone 15", 393, 659, 3.0),
            ("iPhone 15 Pro Max", 430, 739, 3.0),
            ("iPhone SE", 320, 568, 2.0),
            ("Pixel 7", 412, 839, 2.625),
            ("Pixel 5", 393, 727, 2.75),
            ("Galaxy S24", 360, 780, 3.0),
            ("iPad", 810, 1080, 2.0),
            ("iPad Mini", 768, 1024, 2.0),
            ("iPad Pro 11", 834, 1194, 2.0),
        ];
        for (name, w, h, dpr) in want {
            assert!(LEGACY_NAMES.contains(&name));
            let d = lookup(name).unwrap();
            assert_eq!((d.width, d.height, d.dpr), (w, h, dpr), "{name}");
            assert!(d.mobile && d.touch, "{name}");
        }
        assert_eq!(web_device("ipad").unwrap().name, "iPad");
    }
}
