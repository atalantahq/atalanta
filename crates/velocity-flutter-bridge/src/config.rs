use serde::{Deserialize, Serialize};

/// Configuration for the Flutter headless bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlutterBridgeConfig {
    /// Path to the Flutter project root (containing pubspec.yaml).
    pub project_path: String,
    /// Dart target file (default: lib/main.dart).
    #[serde(default = "default_target")]
    pub target: String,
    /// Screen width, in physical px when `device_pixel_ratio` is set.
    #[serde(default = "default_width")]
    pub width: u32,
    /// Screen height, in physical px when `device_pixel_ratio` is set.
    #[serde(default = "default_height")]
    pub height: u32,
    /// Size the test surface as `width` x `height` physical px at this
    /// ratio (1080x2340 at 2.625 is a 411x891 logical phone). Unset: the
    /// surface stays as `flutter test` makes it (800x600 logical), and the
    /// driver uses whatever size the bridge reports.
    #[serde(default)]
    pub device_pixel_ratio: Option<f64>,
    /// `assertVisible` also requires a tap at the element's centre to reach
    /// it (Patrol's and flutter_driver's rule), so a widget under an overlay
    /// is not visible. Unset: `VELOCITY_FLUTTER_HIT_VISIBILITY=1`, else off.
    #[serde(default)]
    pub hit_test_visibility: Option<bool>,
    /// Directory for golden file baselines.
    #[serde(default)]
    pub golden_dir: Option<String>,
    /// TCP port for communication with the Dart test process.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Which tree the bridge exports. Unset: `VELOCITY_FLUTTER_HIERARCHY`,
    /// else `semantics` (D-15).
    #[serde(default)]
    pub hierarchy: Option<HierarchyMode>,
    /// Deadline for one bridge command (write and reply), in seconds.
    #[serde(default = "default_reply_timeout_secs")]
    pub reply_timeout_secs: u64,
}

/// The tree `get_hierarchy` exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyMode {
    /// Every on-stage widget (`text:` matches `Text` widgets, `id:` a
    /// `ValueKey`). Deprecated opt-out: one YAML can answer differently
    /// here than on a device (VEL-S53).
    Widgets,
    /// The semantics tree, as TalkBack/VoiceOver and the device drivers see
    /// it: merged labels, hidden nodes left out, real enabled state. The
    /// default (D-15).
    Semantics,
}

impl std::str::FromStr for HierarchyMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "semantics" => Ok(Self::Semantics),
            "widgets" => Ok(Self::Widgets),
            other => Err(format!(
                "flutter.hierarchy: expected `semantics` or `widgets`, got `{other}`"
            )),
        }
    }
}

/// The oldest Flutter the headless bridge supports (D-41): `Semantics(
/// identifier:)`, which `id:` matches in the default semantics tree,
/// arrived in 3.19. On an older SDK the app compiles but no `id:` ever
/// matches.
pub const MIN_FLUTTER: (u32, u32) = (3, 19);

/// The warning `hierarchy: widgets` gives (D-15).
pub const WIDGETS_DEPRECATED: &str = "flutter hierarchy `widgets` is deprecated: headless Flutter \
     now matches the semantics tree by default, as the device drivers do. Remove \
     `hierarchy: widgets` (and VELOCITY_FLUTTER_HIERARCHY=widgets) and select by \
     Semantics labels or `Semantics(identifier:)`";

impl FlutterBridgeConfig {
    pub fn hit_test_visibility(&self) -> bool {
        self.hit_test_visibility.unwrap_or_else(|| {
            velocity_common::env::var("VELOCITY_FLUTTER_HIT_VISIBILITY").is_ok_and(|v| v == "1")
        })
    }

    pub fn hierarchy_mode(&self) -> HierarchyMode {
        if let Some(mode) = self.hierarchy {
            return mode;
        }
        match velocity_common::env::var("VELOCITY_FLUTTER_HIERARCHY").as_deref() {
            Ok("widgets") => HierarchyMode::Widgets,
            _ => HierarchyMode::Semantics,
        }
    }

    /// The bridge config a suite asks for: `headless.width/height/
    /// devicePixelRatio` and the `flutter:` block. Returns it with the
    /// warnings to print, or a config error: an unknown `hierarchy`, a
    /// non-positive ratio, or a project on a Flutter SDK older than
    /// [`MIN_FLUTTER`] (D-41).
    pub fn from_suite(
        suite: &velocity_common::SuiteConfig,
    ) -> velocity_common::Result<(Self, Vec<String>)> {
        use velocity_common::VelocityError;
        let mut config = Self::default();
        let mut warnings = Vec::new();
        if let Some(hc) = &suite.headless {
            if let Some(w) = hc.width {
                config.width = w;
            }
            if let Some(h) = hc.height {
                config.height = h;
            }
            if let Some(dpr) = hc.device_pixel_ratio {
                if !(dpr.is_finite() && dpr > 0.0) {
                    return Err(VelocityError::Config(format!(
                        "headless.devicePixelRatio must be a positive number, got {dpr}"
                    )));
                }
                config.device_pixel_ratio = Some(dpr);
            }
            if hc.font_scale.is_some() {
                warnings.push(
                    "headless.fontScale applies to native headless renders only; \
                     the Flutter bridge ignores it"
                        .into(),
                );
            }
        }
        if let Some(fc) = &suite.flutter {
            if let Some(pp) = &fc.project_path {
                config.project_path = pp.clone();
            }
            if let Some(t) = &fc.target {
                config.target = t.clone();
            }
            if let Some(gd) = &fc.golden_dir {
                config.golden_dir = Some(gd.clone());
            }
            if let Some(p) = fc.port {
                config.port = p;
            }
            if let Some(h) = &fc.hierarchy {
                config.hierarchy = Some(h.parse().map_err(VelocityError::Config)?);
            }
        }
        if config.hierarchy_mode() == HierarchyMode::Widgets {
            warnings.push(WIDGETS_DEPRECATED.into());
        }
        let project = std::path::Path::new(&config.project_path);
        if let Some(version) = detect_flutter_version(project) {
            check_flutter_floor(&version)?;
        }
        Ok((config, warnings))
    }
}

/// The Flutter SDK version the project was last built with, from what
/// `flutter pub get` leaves in `.dart_tool/`: `version` (the framework
/// version), else `package_config.json`'s `flutterVersion`. `None` when
/// neither says (never built, or the bridge runs elsewhere).
pub fn detect_flutter_version(project: &std::path::Path) -> Option<String> {
    let dart_tool = project.join(".dart_tool");
    if let Ok(v) = std::fs::read_to_string(dart_tool.join("version")) {
        let v = v.trim();
        if parse_version(v).is_some() {
            return Some(v.to_string());
        }
    }
    let config = std::fs::read_to_string(dart_tool.join("package_config.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&config).ok()?;
    let v = json.get("flutterVersion")?.as_str()?;
    parse_version(v).map(|_| v.to_string())
}

/// `major.minor` of a Flutter version; `None` when it is unknown: not a
/// version, or major 0 (`0.0.0-unknown`, what Flutter reports when it
/// can't read its git tags, e.g. a shallow clone; review-d5 M-3).
fn parse_version(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split(['.', '-', '+']);
    let major: u32 = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    (major > 0).then_some((major, minor))
}

/// A config error naming the upgrade when `version` is older than
/// [`MIN_FLUTTER`] (D-41). An unknown version (unparsable, or
/// `0.0.0-unknown`) skips the check: it never fails a run.
pub fn check_flutter_floor(version: &str) -> velocity_common::Result<()> {
    let parsed = parse_version(version);
    if parsed.is_none() {
        tracing::debug!(
            "Flutter version {version:?} is unknown; skipping the {}.{} floor check",
            MIN_FLUTTER.0,
            MIN_FLUTTER.1
        );
    }
    match parsed {
        Some(v) if v < MIN_FLUTTER => Err(velocity_common::VelocityError::Config(format!(
            "headless Flutter needs Flutter {}.{} or newer (Semantics identifiers, which `id:` \
             matches); this project was built with Flutter {version}. Upgrade with \
             `flutter upgrade` (or pin a {}.{}+ SDK in CI), then run `flutter pub get`",
            MIN_FLUTTER.0, MIN_FLUTTER.1, MIN_FLUTTER.0, MIN_FLUTTER.1
        ))),
        _ => Ok(()),
    }
}

fn default_target() -> String {
    "lib/main.dart".to_string()
}
fn default_width() -> u32 {
    1080
}
fn default_height() -> u32 {
    2340
}
fn default_port() -> u16 {
    19600
}
fn default_reply_timeout_secs() -> u64 {
    30
}

impl Default for FlutterBridgeConfig {
    fn default() -> Self {
        Self {
            project_path: ".".to_string(),
            target: default_target(),
            width: default_width(),
            height: default_height(),
            device_pixel_ratio: None,
            hit_test_visibility: None,
            golden_dir: None,
            port: default_port(),
            hierarchy: None,
            reply_timeout_secs: default_reply_timeout_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{FlutterYamlConfig, HeadlessYamlConfig, SuiteConfig};

    fn suite(project: &std::path::Path, hierarchy: Option<&str>) -> SuiteConfig {
        SuiteConfig {
            flutter: Some(FlutterYamlConfig {
                project_path: Some(project.display().to_string()),
                hierarchy: hierarchy.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn semantics_is_the_default_and_widgets_warns() {
        // D-15. Holds unless the environment opts out.
        if std::env::var_os("VELOCITY_FLUTTER_HIERARCHY").is_some() {
            return;
        }
        let dir = std::env::temp_dir();
        let (c, warnings) = FlutterBridgeConfig::from_suite(&suite(&dir, None)).unwrap();
        assert_eq!(c.hierarchy_mode(), HierarchyMode::Semantics);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            FlutterBridgeConfig::default().hierarchy_mode(),
            HierarchyMode::Semantics
        );

        let (c, warnings) = FlutterBridgeConfig::from_suite(&suite(&dir, Some("widgets"))).unwrap();
        assert_eq!(c.hierarchy_mode(), HierarchyMode::Widgets);
        assert!(
            warnings.iter().any(|w| w.contains("deprecated")),
            "{warnings:?}"
        );

        let err = FlutterBridgeConfig::from_suite(&suite(&dir, Some("tree"))).unwrap_err();
        assert!(matches!(err, velocity_common::VelocityError::Config(m) if m.contains("tree")));
    }

    #[test]
    fn a_flutter_older_than_3_19_is_a_config_error_naming_the_upgrade() {
        // D-41.
        let dir = tempfile_dir("old");
        std::fs::create_dir_all(dir.join(".dart_tool")).unwrap();
        std::fs::write(dir.join(".dart_tool/version"), "3.16.9\n").unwrap();
        let err = FlutterBridgeConfig::from_suite(&suite(&dir, None)).unwrap_err();
        let velocity_common::VelocityError::Config(m) = err else {
            panic!("{err:?}");
        };
        assert!(
            m.contains("3.19") && m.contains("3.16.9") && m.contains("flutter upgrade"),
            "{m}"
        );

        std::fs::write(dir.join(".dart_tool/version"), "3.19.0").unwrap();
        assert!(FlutterBridgeConfig::from_suite(&suite(&dir, None)).is_ok());

        // package_config.json's flutterVersion when there is no version file.
        std::fs::remove_file(dir.join(".dart_tool/version")).unwrap();
        std::fs::write(
            dir.join(".dart_tool/package_config.json"),
            r#"{"configVersion":2,"packages":[],"flutterVersion":"3.13.0"}"#,
        )
        .unwrap();
        assert!(FlutterBridgeConfig::from_suite(&suite(&dir, None)).is_err());
        assert_eq!(detect_flutter_version(&dir).as_deref(), Some("3.13.0"));
        // Nothing detected: no check.
        std::fs::remove_dir_all(dir.join(".dart_tool")).unwrap();
        assert!(FlutterBridgeConfig::from_suite(&suite(&dir, None)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// review-d5 M-3: an SDK that can't read its own version (a shallow
    /// clone reports `0.0.0-unknown`) is unknown, never "too old".
    #[test]
    fn an_unknown_flutter_version_skips_the_floor_check() {
        for v in ["0.0.0-unknown", "0.0.0", "0.5.1", "unknown", "", "3"] {
            assert!(check_flutter_floor(v).is_ok(), "{v:?}");
        }
        assert!(check_flutter_floor("3.16.9").is_err());
        let dir = tempfile_dir("unknown");
        std::fs::create_dir_all(dir.join(".dart_tool")).unwrap();
        std::fs::write(dir.join(".dart_tool/version"), "0.0.0-unknown\n").unwrap();
        std::fs::write(
            dir.join(".dart_tool/package_config.json"),
            r#"{"configVersion":2,"packages":[],"flutterVersion":"0.0.0-unknown"}"#,
        )
        .unwrap();
        assert_eq!(detect_flutter_version(&dir), None);
        assert!(FlutterBridgeConfig::from_suite(&suite(&dir, None)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_device_pixel_ratio_sizes_the_flutter_surface() {
        // D-46.
        let mut s = suite(&std::env::temp_dir(), None);
        s.headless = Some(HeadlessYamlConfig {
            width: Some(1080),
            height: Some(2340),
            device_pixel_ratio: Some(2.625),
            ..Default::default()
        });
        let (c, _) = FlutterBridgeConfig::from_suite(&s).unwrap();
        assert_eq!((c.width, c.height), (1080, 2340));
        assert_eq!(c.device_pixel_ratio, Some(2.625));
        s.headless.as_mut().unwrap().device_pixel_ratio = Some(0.0);
        assert!(FlutterBridgeConfig::from_suite(&s).is_err());
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vel-flutter-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
