// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use serde::{Deserialize, Serialize};

/// Configuration for the headless rendering driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessConfig {
    /// Viewport width in pixels.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Viewport height in pixels.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Device pixel density (1.0 = mdpi, 2.0 = xhdpi, 3.0 = xxhdpi).
    #[serde(default = "default_density")]
    pub density: f32,
    /// Path to APK (Android) or .app bundle (iOS).
    #[serde(default)]
    pub app_path: Option<String>,
    /// Initial layout name (e.g., "@layout/activity_main" or "Main.xib").
    #[serde(default)]
    pub initial_layout: Option<String>,
    /// Text size multiplier, like the system font-size setting (D-47).
    #[serde(default = "default_font_scale")]
    pub font_scale: f32,
}

fn default_font_scale() -> f32 {
    1.0
}

/// The longest side the surface may have (no device screen is near it).
const MAX_SIDE_PX: u64 = 16_384;
/// The most pixels the surface may hold: 100 million is 400 MB of RGBA.
const MAX_PIXELS: u64 = 100_000_000;

impl HeadlessConfig {
    /// The config a suite's `headless:` block asks for.
    /// `devicePixelRatio` (D-46) is the same thing as `density` here; both
    /// set to different values, or a non-positive ratio or `fontScale`, is
    /// a config error.
    pub fn from_yaml(
        yaml: Option<&atalanta_common::HeadlessYamlConfig>,
    ) -> atalanta_common::Result<Self> {
        use atalanta_common::AtalantaError;
        let mut c = Self::default();
        let Some(hc) = yaml else {
            return Ok(c);
        };
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if let Some(w) = hc.width {
            c.width = w;
        }
        if let Some(h) = hc.height {
            c.height = h;
        }
        // The surface is `width x height` RGBA pixels, allocated and filled
        // for every session: a zero side can't be created, and an absurd one
        // would try to allocate gigabytes.
        let (w, h) = (u64::from(c.width), u64::from(c.height));
        if !(1..=MAX_SIDE_PX).contains(&w) || !(1..=MAX_SIDE_PX).contains(&h) || w * h > MAX_PIXELS
        {
            return Err(AtalantaError::Config(format!(
                "headless.width and headless.height must each be 1 to {MAX_SIDE_PX} \
                 pixels and together at most {MAX_PIXELS} pixels, got {}x{}",
                c.width, c.height
            )));
        }
        match (hc.density, hc.device_pixel_ratio) {
            (Some(d), Some(r)) if (f64::from(d) - r).abs() > 1e-6 => {
                return Err(AtalantaError::Config(format!(
                    "headless.density ({d}) and headless.devicePixelRatio ({r}) disagree; \
                     set one of them"
                )));
            }
            (Some(d), _) if !positive(f64::from(d)) => {
                return Err(AtalantaError::Config(format!(
                    "headless.density must be a positive number, got {d}"
                )));
            }
            (Some(d), _) => c.density = d,
            (None, Some(r)) if !positive(r) => {
                return Err(AtalantaError::Config(format!(
                    "headless.devicePixelRatio must be a positive number, got {r}"
                )));
            }
            (None, Some(r)) => c.density = r as f32,
            (None, None) => {}
        }
        if let Some(s) = hc.font_scale {
            if !positive(f64::from(s)) {
                return Err(AtalantaError::Config(format!(
                    "headless.fontScale must be a positive number, got {s}"
                )));
            }
            c.font_scale = s;
        }
        c.app_path = hc.app_path.clone();
        c.initial_layout = hc.initial_layout.clone();
        Ok(c)
    }
}

fn default_width() -> u32 {
    1080
}
fn default_height() -> u32 {
    1920
}
fn default_density() -> f32 {
    2.0
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            density: default_density(),
            app_path: None,
            initial_layout: None,
            font_scale: default_font_scale(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::HeadlessYamlConfig;

    #[test]
    fn device_pixel_ratio_and_font_scale_come_from_the_suite() {
        // D-46 / D-47.
        let y = HeadlessYamlConfig {
            device_pixel_ratio: Some(3.0),
            font_scale: Some(1.3),
            ..Default::default()
        };
        let c = HeadlessConfig::from_yaml(Some(&y)).unwrap();
        assert_eq!(c.density, 3.0);
        assert_eq!(c.font_scale, 1.3);
        let both = HeadlessYamlConfig {
            density: Some(2.0),
            device_pixel_ratio: Some(3.0),
            ..Default::default()
        };
        assert!(HeadlessConfig::from_yaml(Some(&both)).is_err());
        let same = HeadlessYamlConfig {
            density: Some(3.0),
            ..both.clone()
        };
        assert_eq!(HeadlessConfig::from_yaml(Some(&same)).unwrap().density, 3.0);
        let zero = HeadlessYamlConfig {
            font_scale: Some(0.0),
            ..Default::default()
        };
        assert!(HeadlessConfig::from_yaml(Some(&zero)).is_err());
        assert_eq!(HeadlessConfig::from_yaml(None).unwrap().font_scale, 1.0);
    }

    #[test]
    fn a_zero_or_absurd_screen_size_is_a_config_error() {
        for (w, h) in [
            (0, 100),
            (100, 0),
            (100_000, 100_000),
            (16_385, 10),
            (50_000, 3_000),
        ] {
            let y = HeadlessYamlConfig {
                width: Some(w),
                height: Some(h),
                ..Default::default()
            };
            assert!(
                matches!(
                    HeadlessConfig::from_yaml(Some(&y)),
                    Err(atalanta_common::AtalantaError::Config(_))
                ),
                "{w}x{h}"
            );
        }
        let ok = HeadlessYamlConfig {
            width: Some(1280),
            height: Some(800),
            ..Default::default()
        };
        assert!(HeadlessConfig::from_yaml(Some(&ok)).is_ok());
    }
}
