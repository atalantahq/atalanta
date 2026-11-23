use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use zip::ZipArchive;

use super::axml::AxmlParser;
use super::resources::ResourceTable;

/// Loads and extracts content from Android APK files.
pub struct ApkLoader {
    /// Parsed resource table from resources.arsc.
    pub resources: ResourceTable,
    /// Raw layout XML files keyed by name (e.g., "activity_main").
    pub layouts: HashMap<String, Vec<u8>>,
    /// The app's package name from AndroidManifest.xml.
    pub package_name: Option<String>,
}

impl ApkLoader {
    /// Load an APK file and extract layouts and resources.
    pub fn from_path(path: &Path) -> Result<Self, ApkError> {
        let file = File::open(path).map_err(|e| ApkError::IoError(e.to_string()))?;
        let mut archive = ZipArchive::new(file).map_err(|e| ApkError::ZipError(e.to_string()))?;

        let mut layouts = HashMap::new();
        let mut resources = ResourceTable::empty();
        let mut package_name = None;

        // Collect file names first to avoid borrow issues
        let file_names: Vec<String> = (0..archive.len())
            .filter_map(|i| {
                archive
                    .by_index(i)
                    .ok()
                    .and_then(|f| f.name().ok().map(|n| n.to_string()))
            })
            .collect();

        let mut budget = MAX_APK_TOTAL_BYTES;
        for name in &file_names {
            // Extract resources.arsc
            if name == "resources.arsc" {
                let mut entry = archive
                    .by_name(name)
                    .map_err(|e| ApkError::ZipError(e.to_string()))?;
                let data = read_capped(&mut entry, &mut budget)?;
                resources = ResourceTable::parse(&data).unwrap_or_else(|_| ResourceTable::empty());
            }

            // Extract layout XMLs from res/layout/ directories
            if name.starts_with("res/layout") && name.ends_with(".xml") {
                // `res/layout/` is the default; `res/layout-land/`, `-v26`,
                // `-sw600dp`... are variants. Zip order is not a priority, so a
                // variant never replaces the default whichever comes first.
                let is_default = name.starts_with("res/layout/");
                let layout_name = Path::new(name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                if !is_default && layouts.contains_key(&layout_name) {
                    continue;
                }

                let mut entry = archive
                    .by_name(name)
                    .map_err(|e| ApkError::ZipError(e.to_string()))?;
                let data = read_capped(&mut entry, &mut budget)?;

                // Layout name from the path: res/layout/activity_main.xml -> activity_main
                layouts.insert(layout_name, data);
            }

            // Extract package name from AndroidManifest.xml
            if name == "AndroidManifest.xml" {
                let mut entry = archive
                    .by_name(name)
                    .map_err(|e| ApkError::ZipError(e.to_string()))?;
                let data = read_capped(&mut entry, &mut budget)?;

                if let Ok(xml) = AxmlParser::parse(&data) {
                    package_name = xml.get_attribute("manifest", "package");
                }
            }
        }

        Ok(Self {
            resources,
            layouts,
            package_name,
        })
    }

    /// Get a parsed layout XML by name.
    pub fn get_layout(&self, name: &str) -> Option<&Vec<u8>> {
        // Strip @layout/ prefix if present
        let clean_name = name.strip_prefix("@layout/").unwrap_or(name);
        self.layouts.get(clean_name)
    }

    /// List all available layout names.
    pub fn layout_names(&self) -> Vec<&str> {
        self.layouts.keys().map(|s| s.as_str()).collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApkError {
    #[error("I/O error: {0}")]
    IoError(String),
    #[error("ZIP error: {0}")]
    ZipError(String),
    #[error("Layout not found: {0}")]
    LayoutNotFound(String),
}

/// Largest one APK entry read (`resources.arsc`, a layout, the manifest).
const MAX_APK_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Most bytes read from one APK in all. A zip entry can claim, and inflate
/// to, gigabytes from a few kilobytes (audit 2026-11 HEADLESS-1).
const MAX_APK_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// `entry`'s bytes, refusing one over [`MAX_APK_ENTRY_BYTES`] or past what
/// is left of `budget`.
fn read_capped(entry: &mut impl Read, budget: &mut u64) -> Result<Vec<u8>, ApkError> {
    let limit = MAX_APK_ENTRY_BYTES.min(*budget);
    let mut data = Vec::new();
    entry
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| ApkError::IoError(e.to_string()))?;
    if data.len() as u64 > limit {
        return Err(ApkError::IoError(format!(
            "an APK entry inflates past {limit} bytes; refusing it (zip bomb?)"
        )));
    }
    *budget -= data.len() as u64;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_that_inflates_past_the_cap_is_refused() {
        let mut budget = MAX_APK_TOTAL_BYTES;
        let mut bomb = std::io::repeat(0).take(MAX_APK_ENTRY_BYTES + 1);
        assert!(read_capped(&mut bomb, &mut budget).is_err());
        let mut small = std::io::repeat(7).take(10);
        assert_eq!(read_capped(&mut small, &mut budget).unwrap().len(), 10);
        assert_eq!(budget, MAX_APK_TOTAL_BYTES - 10);
        let mut tight = 5;
        let mut over = std::io::repeat(0).take(6);
        assert!(read_capped(&mut over, &mut tight).is_err());
    }

    #[test]
    fn a_layout_variant_never_replaces_the_default() {
        use std::io::Write;
        for order in [
            ["res/layout-land/main.xml", "res/layout/main.xml"],
            ["res/layout/main.xml", "res/layout-land/main.xml"],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.apk");
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            for name in order {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(if name.contains("land") {
                    b"land"
                } else {
                    b"default"
                })
                .unwrap();
            }
            zip.finish().unwrap();
            let loader = ApkLoader::from_path(&path).unwrap();
            assert_eq!(loader.get_layout("main").unwrap(), b"default", "{order:?}");
        }
    }
}
