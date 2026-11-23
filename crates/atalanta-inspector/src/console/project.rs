//! The project folder: which suites the Run panel offers, and path
//! confinement for everything the console reads from disk.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

/// Folders never searched for suites.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "atalanta-results",
    "atalanta-baselines",
    "build",
    "dist",
    "Pods",
    "DerivedData",
];
/// How deep under the project folder suites are looked for.
const MAX_DEPTH: usize = 6;
/// At most this many are listed.
const MAX_SUITES: usize = 500;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Suite {
    /// Relative to the project folder, `/`-separated.
    pub path: String,
    /// Its tests' names (from a quick header scan; empty if it didn't parse).
    pub tests: Vec<String>,
}

/// `rel` as a path inside `base` that exists: relative, normal components
/// only, and still inside `base` once symlinks are resolved. `None`
/// otherwise (traversal, absolute paths, a symlink out).
pub fn confine(base: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains(['\0', '\\']) {
        return None;
    }
    let p = Path::new(rel);
    if !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let base = base.canonicalize().ok()?;
    let full = base.join(p).canonicalize().ok()?;
    full.starts_with(&base).then_some(full)
}

fn is_suite_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.ends_with(".yaml") || lower.ends_with(".yml"))
        && !lower.starts_with('.')
        && !matches!(
            lower.as_str(),
            "atalanta-quarantine.yaml" | "atalanta-impact.yaml" | "pubspec.yaml"
        )
}

/// Whether a YAML file looks like a Atalanta suite: a top-level `tests:`.
fn looks_like_suite(text: &str) -> bool {
    text.lines().any(|l| l.trim_end() == "tests:")
}

/// The suites under `base`, sorted by path.
pub fn find_suites(base: &Path) -> Vec<Suite> {
    let mut out = Vec::new();
    let Ok(root) = base.canonicalize() else {
        return out;
    };
    let mut stack = vec![(root.clone(), 0usize)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // Symlinks are not followed: they could lead out of the project.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth + 1 < MAX_DEPTH
                    && !name.starts_with('.')
                    && !SKIP_DIRS.contains(&name.as_str())
                {
                    stack.push((e.path(), depth + 1));
                }
            } else if ft.is_file() && is_suite_name(&name) {
                let path = e.path();
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if !looks_like_suite(&text) {
                    continue;
                }
                let tests = atalanta_core::streaming::parse_headers(&text)
                    .map(|h| h.into_iter().map(|h| h.name).collect())
                    .unwrap_or_default();
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push(Suite { path: rel, tests });
                if out.len() >= MAX_SUITES {
                    break 'walk;
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// `GET /api/project/suites`.
pub async fn suites(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let base = state.console.project_dir.clone();
    let list = tokio::task::spawn_blocking(move || find_suites(&base))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "projectDir": state.console.project_dir.display().to_string(),
        "suites": list,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "vel-console-{tag}-{}-{}",
            std::process::id(),
            crate::security::new_token()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn paths_stay_inside_the_project() {
        let base = tmp("confine");
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::write(base.join("a/s.yaml"), "tests:\n").unwrap();
        let outside = tmp("outside");
        std::fs::write(outside.join("secret.yaml"), "x").unwrap();
        assert!(confine(&base, "a/s.yaml").is_some());
        for bad in [
            "../x.yaml",
            "/etc/passwd",
            "a/../../x",
            "./a/s.yaml",
            "",
            "a\\s.yaml",
            "missing.yaml",
        ] {
            assert!(confine(&base, bad).is_none(), "{bad}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();
            assert!(confine(&base, "link/secret.yaml").is_none());
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn suites_are_found_and_junk_is_skipped() {
        let base = tmp("suites");
        std::fs::create_dir_all(base.join("flows/sub")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/x")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let suite = "appId: a\ntests:\n  - name: login\n    steps:\n      - launchApp\n";
        std::fs::write(base.join("atalanta.yaml"), suite).unwrap();
        std::fs::write(base.join("flows/sub/b.yml"), suite).unwrap();
        std::fs::write(base.join("node_modules/x/c.yaml"), suite).unwrap();
        std::fs::write(base.join(".git/d.yaml"), suite).unwrap();
        std::fs::write(base.join("other.yaml"), "key: value\n").unwrap();
        let found = find_suites(&base);
        let paths: Vec<_> = found.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["flows/sub/b.yml", "atalanta.yaml"]);
        assert_eq!(found[1].tests, ["login"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// V2: the cap holds across folders (it used to end only the folder
    /// being read, so every further folder added one more suite).
    #[test]
    fn the_suite_list_stops_at_the_cap() {
        let base = tmp("cap");
        let suite = "tests:\n  - name: t\n    steps:\n      - launchApp\n";
        for i in 0..MAX_SUITES + 20 {
            let d = base.join(format!("d{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s.yaml"), suite).unwrap();
        }
        assert_eq!(find_suites(&base).len(), MAX_SUITES);
        let _ = std::fs::remove_dir_all(&base);
    }
}
