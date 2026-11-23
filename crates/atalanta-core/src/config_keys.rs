//! Keys in a suite's `config:` that Atalanta doesn't read.
//!
//! serde skips unknown keys, so a misspelt setting (`app_pth:`) would
//! silently do nothing. This names them. The known keys come from serde
//! itself (each struct's field list, aliases included), so they cannot
//! drift from the structs the way a hand-kept list would.

use atalanta_common::test_types as t;
use serde::de::{self, Deserialize, Deserializer, Visitor};

/// `config.<path>: …` for each key no setting reads, with the closest
/// known key when one is near.
pub fn unknown_config_keys(yaml: &str) -> Vec<String> {
    let Ok(root) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return Vec::new();
    };
    let Some(config) = root.get("config").and_then(|c| c.as_mapping()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    check(config, "config", fields::<t::SuiteConfig>(), &mut out);
    for (key, value) in config {
        let (Some(key), Some(section)) = (key.as_str(), value.as_mapping()) else {
            continue;
        };
        let Some(known) = section_fields(key) else {
            continue;
        };
        check(section, &format!("config.{key}"), known, &mut out);
    }
    out
}

/// The keys the parser reads in `config.<section>` (aliases included),
/// for the sections that are structs; `None` for any other key.
pub fn section_fields(section: &str) -> Option<&'static [&'static str]> {
    Some(match section {
        "sync" => fields::<t::SyncConfig>(),
        "retry" => fields::<t::RetryConfig>(),
        "artifacts" => fields::<t::ArtifactsConfig>(),
        "dialog" => fields::<t::DialogConfig>(),
        "healing" => fields::<t::HealingConfig>(),
        "performance" => fields::<t::PerformanceConfig>(),
        "reset" => fields::<t::ResetConfig>(),
        "headless" => fields::<t::HeadlessYamlConfig>(),
        "react_native" | "reactNative" => fields::<t::ReactNativeYamlConfig>(),
        "flutter" => fields::<t::FlutterYamlConfig>(),
        "web" => fields::<t::WebConfig>(),
        "nl" => fields::<t::NlConfig>(),
        "ai" => fields::<t::AiConfig>(),
        "exec" => fields::<atalanta_common::exec::ExecConfig>(),
        _ => return None,
    })
}

fn check(map: &serde_yaml::Mapping, path: &str, known: &[&str], out: &mut Vec<String>) {
    check_as(map, path, known, "a setting", out)
}

fn check_as(
    map: &serde_yaml::Mapping,
    path: &str,
    known: &[&str],
    what: &str,
    out: &mut Vec<String>,
) {
    if known.is_empty() {
        return;
    }
    for key in map.keys().filter_map(|k| k.as_str()) {
        if known.contains(&key) {
            continue;
        }
        if key == "<<" {
            out.push(format!(
                "{path}.<<: YAML merge keys aren't supported; the merged keys are ignored (write them out)"
            ));
            continue;
        }
        out.push(match closest(key, known) {
            Some(k) => format!("{path}.{key}: not {what} (ignored); did you mean `{k}`?"),
            None => format!("{path}.{key}: not {what} (ignored)"),
        });
    }
}

/// The known name closest to `key` (case-insensitive edit distance of at
/// most 3, and less than the key's own length so `x` suggests nothing).
pub(crate) fn closest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let lower = key.to_lowercase();
    known
        .iter()
        .map(|k| (distance(&lower, &k.to_lowercase()), *k))
        .filter(|(d, _)| *d <= 3 && *d < lower.chars().count())
        .min()
        .map(|(_, k)| k)
}

/// Every key nothing reads: the suite's own keys, each test's and flow's
/// (VEL-193: `tag:`, `dependOn:` or a YAML merge `<<:` were dropped without
/// a word), then `config:`'s (see [`unknown_config_keys`]).
pub fn unknown_keys(yaml: &str) -> Vec<String> {
    let mut out = unknown_suite_keys(yaml);
    out.extend(unknown_config_keys(yaml));
    out.extend(renamed_config_keys(yaml));
    out
}

/// `config.<section>` keys that were renamed: (section, old names, new
/// name, why). The old names are still read (serde aliases), but an alias
/// cannot warn, so [`renamed_config_keys`] does.
pub const RENAMED: &[(&str, &[&str], &str, &str)] = &[(
    "performance",
    &["heapGrowthThresholdPct", "heap_growth_threshold_pct"],
    "pssGrowthThresholdPct",
    "it compares the test's peak total PSS with its saved baseline, not heap growth",
)];

/// Whether `key` in `config.<section>` is an old name from [`RENAMED`].
pub fn is_renamed(section: &str, key: &str) -> bool {
    RENAMED
        .iter()
        .any(|(s, old, _, _)| *s == section && old.contains(&key))
}

/// A warning for each old key name from [`RENAMED`] in the suite's
/// `config:`; it still works.
pub fn renamed_config_keys(yaml: &str) -> Vec<String> {
    let Ok(root) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return Vec::new();
    };
    let Some(config) = root.get("config").and_then(|c| c.as_mapping()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (section, old, new, why) in RENAMED {
        let Some(map) = config.get(*section).and_then(|v| v.as_mapping()) else {
            continue;
        };
        for key in map.keys().filter_map(|k| k.as_str()) {
            if old.contains(&key) {
                out.push(format!(
                    "config.{section}.{key}: renamed to `{new}` ({why}); the old name still works for now"
                ));
            }
        }
    }
    out
}

/// Unknown keys at the suite, test and flow level.
pub fn unknown_suite_keys(yaml: &str) -> Vec<String> {
    let Ok(root) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return Vec::new();
    };
    let Some(map) = root.as_mapping() else {
        return Vec::new();
    };
    let [suite, test, flow] = crate::parser::suite_keys();
    let mut out = Vec::new();
    check_as(map, "suite", suite, "a suite key", &mut out);
    for (section, known, what) in [("tests", test, "a test key"), ("flows", flow, "a flow key")] {
        let Some(items) = map.get(section).and_then(|v| v.as_sequence()) else {
            continue;
        };
        for (i, item) in items.iter().enumerate() {
            let Some(item) = item.as_mapping() else {
                continue;
            };
            let label = item
                .get("name")
                .or_else(|| item.get("id"))
                .and_then(|n| n.as_str())
                .map(|n| format!("{section}[{i}] ({n:?})"))
                .unwrap_or_else(|| format!("{section}[{i}]"));
            check_as(item, &label, known, what, &mut out);
        }
    }
    out
}

/// The keys serde accepts for `T` (its field names and aliases), read by
/// handing `T::deserialize` a deserializer that only records them.
pub(crate) fn fields<'de, T: Deserialize<'de>>() -> &'static [&'static str] {
    struct Probe<'a>(&'a mut &'static [&'static str]);
    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = fields;
            Err(de::Error::custom("fields recorded"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
    let mut fields: &'static [&'static str] = &[];
    let _ = T::deserialize(Probe(&mut fields));
    fields
}

/// The names serde accepts for the unit enum `T`, aliases included, read
/// the same way as [`fields`]; for the JSON schema's enums (VEL-208).
pub(crate) fn variants<'de, T: Deserialize<'de>>() -> &'static [&'static str] {
    struct Probe<'a>(&'a mut &'static [&'static str]);
    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not an enum"))
        }
        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(de::Error::custom("variants recorded"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }
    }
    let mut variants: &'static [&'static str] = &[];
    let _ = T::deserialize(Probe(&mut variants));
    variants
}

/// Edit distance, for "did you mean".
pub(crate) fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_memory_threshold_key_warns_and_the_new_one_does_not() {
        let old = "config:\n  performance:\n    heapGrowthThresholdPct: 20\ntests: []\n";
        let w = unknown_keys(old);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("renamed to `pssGrowthThresholdPct`"), "{w:?}");
        let new = "config:\n  performance:\n    pssGrowthThresholdPct: 20\ntests: []\n";
        assert!(unknown_keys(new).is_empty());
        // Both names still set the threshold.
        for yaml in [old, new] {
            let c: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let p: t::PerformanceConfig =
                serde_yaml::from_value(c["config"]["performance"].clone()).unwrap();
            assert_eq!(p.pss_growth_threshold_pct, 20);
        }
    }

    #[test]
    fn misspelt_and_unknown_settings_are_named() {
        let yaml = "appId: a\nconfig:\n  platform: android\n  headless:\n    app_path: x.xml\n    appPth: y\n  sync:\n    timeout_ms: 5\n    timeoutMS: 5\n  artifacts:\n    output_dir: r\n  colour: blue\ntests: []\n";
        let found = unknown_config_keys(yaml);
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].starts_with("config.colour"), "{found:#?}");
        assert!(
            found
                .iter()
                .any(|f| f.contains("config.headless.appPth") && f.contains("`appPath`")),
            "{found:#?}"
        );
        assert!(
            found.iter().any(|f| f.contains("config.sync.timeoutMS")),
            "{found:#?}"
        );
    }

    #[test]
    fn enum_variants_include_aliases() {
        let v = variants::<atalanta_common::config::Framework>();
        assert!(v.contains(&"react_native") && v.contains(&"rn"), "{v:?}");
    }

    #[test]
    fn every_config_section_reports_its_fields() {
        assert!(fields::<t::SuiteConfig>().contains(&"reactNative"));
        assert!(fields::<t::HeadlessYamlConfig>().contains(&"appPath"));
        assert!(fields::<t::HeadlessYamlConfig>().contains(&"app_path"));
        assert!(!fields::<t::WebConfig>().is_empty());
    }

    #[test]
    fn the_dead_headless_baseline_dir_still_parses_and_is_named() {
        // VT-11 / VEL-253: `headless.baselineDir` was parsed and never read.
        let yaml = "appId: a\nconfig:\n  headless:\n    baselineDir: b\ntests: []\n";
        assert!(crate::parser::parse_suite_from_str(yaml).is_ok());
        let found = unknown_config_keys(yaml);
        assert!(
            found
                .iter()
                .any(|f| f.contains("config.headless.baselineDir")),
            "{found:?}"
        );
    }

    #[test]
    fn unknown_suite_test_and_flow_keys_are_named() {
        // VEL-193: these were ignored without a word.
        let yaml = r#"
appId: a
defaults: &d
  tags: [smoke]
descripton: x
flows:
  - id: login
    step: []
    steps: [goBack]
tests:
  - name: t
    <<: *d
    tag: [smoke]
    dependOn: [u]
    timeoutMs: 5
    steps: [goBack]
  - name: u
    sideEffects: true
    depends_on: []
    steps: [goBack]
"#;
        assert!(crate::parser::parse_suite_from_str(yaml).is_ok());
        let found = unknown_suite_keys(yaml);
        let has = |a: &str, b: &str| found.iter().any(|f| f.contains(a) && f.contains(b));
        assert!(has("suite.defaults", "not a suite key"), "{found:#?}");
        assert!(has("suite.descripton", "ignored"), "{found:#?}");
        assert!(has("flows[0] (\"login\").step", "`steps`"), "{found:#?}");
        assert!(has("tests[0] (\"t\").<<", "merge"), "{found:#?}");
        assert!(has("tests[0] (\"t\").tag:", "`tags`"), "{found:#?}");
        assert!(
            has("tests[0] (\"t\").dependOn", "`dependsOn`"),
            "{found:#?}"
        );
        assert!(
            has("tests[0] (\"t\").timeoutMs", "not a test key"),
            "{found:#?}"
        );
        assert_eq!(found.len(), 7, "{found:#?}");
        // `unknown_keys` reports these and the config's.
        assert_eq!(unknown_keys(yaml).len(), 7);
    }
}
