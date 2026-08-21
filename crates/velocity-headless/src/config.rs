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

impl HeadlessConfig {
    /// The config a suite's `headless:` block asks for.
    /// `devicePixelRatio` (D-46) is the same thing as `density` here; both
    /// set to different values, or a non-positive ratio or `fontScale`, is
    /// a config error.
    pub fn from_yaml(
        yaml: Option<&velocity_common::HeadlessYamlConfig>,
    ) -> velocity_common::Result<Self> {
        use velocity_common::VelocityError;
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
        match (hc.density, hc.device_pixel_ratio) {
            (Some(d), Some(r)) if (f64::from(d) - r).abs() > 1e-6 => {
                return Err(VelocityError::Config(format!(
                    "headless.density ({d}) and headless.devicePixelRatio ({r}) disagree; \
                     set one of them"
                )));
            }
            (Some(d), _) if !positive(f64::from(d)) => {
                return Err(VelocityError::Config(format!(
                    "headless.density must be a positive number, got {d}"
                )));
            }
            (Some(d), _) => c.density = d,
            (None, Some(r)) if !positive(r) => {
                return Err(VelocityError::Config(format!(
                    "headless.devicePixelRatio must be a positive number, got {r}"
                )));
            }
            (None, Some(r)) => c.density = r as f32,
            (None, None) => {}
        }
        if let Some(s) = hc.font_scale {
            if !positive(f64::from(s)) {
                return Err(VelocityError::Config(format!(
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
    use velocity_common::HeadlessYamlConfig;

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
}
