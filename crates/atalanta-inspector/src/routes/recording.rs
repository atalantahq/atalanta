// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::yaml_gen;

#[derive(Deserialize)]
pub struct SaveFlowRequest {
    pub name: String,
    pub app_id: String,
    pub steps: Vec<String>,
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct SaveFlowResponse {
    pub path: String,
    pub yaml: String,
}

type ApiError = (StatusCode, String);

fn bad(msg: String) -> ApiError {
    (StatusCode::BAD_REQUEST, msg)
}

/// Saves a recorded flow as a NEW `.yaml`/`.yml` file under the flows
/// directory. Existing files are never overwritten and symlinks are never
/// followed, so the endpoint can't be turned into an arbitrary file write.
pub async fn save_flow(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SaveFlowRequest>,
) -> Result<Json<SaveFlowResponse>, ApiError> {
    if req.steps.is_empty() {
        return Err(bad("No steps to save".to_string()));
    }
    // One step per line: a newline would let a step add its own YAML lines.
    if let Some(i) = req
        .steps
        .iter()
        .position(|s| s.contains(['\n', '\r']) || s.trim().is_empty())
    {
        return Err(bad(format!(
            "Step {} must be a single, non-empty line of YAML",
            i + 1
        )));
    }

    let yaml = yaml_gen::flow_yaml(&req.name, &req.app_id, &req.steps);
    check_flow_shape(&yaml, req.steps.len()).map_err(bad)?;

    let sanitized_name = req
        .name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized_name = if sanitized_name.is_empty() {
        "recorded_flow".to_string()
    } else {
        sanitized_name
    };
    let rel = req.path.unwrap_or_else(|| format!("{sanitized_name}.yaml"));
    let rel = validate_relative_path(&rel).map_err(bad)?;

    let base = match &state.flows_dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to resolve working directory: {e}"),
                )
            })?
            .join("flows"),
    };

    let final_path = create_new_file(&base, &rel, yaml.as_bytes())?;
    let output_path = final_path.display().to_string();

    tracing::info!(path = %output_path, steps = req.steps.len(), "saved recorded flow");

    Ok(Json(SaveFlowResponse {
        path: output_path,
        yaml,
    }))
}

/// The saved file must be exactly the suite we meant to write: the two
/// top-level keys and one step per recorded line.
fn check_flow_shape(yaml: &str, n_steps: usize) -> Result<(), String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| format!("Recorded steps are not valid YAML: {e}"))?;
    let map = doc
        .as_mapping()
        .ok_or_else(|| "Recorded flow is not a mapping".to_string())?;
    let keys_ok = map.len() == 2 && map.contains_key("appId") && map.contains_key("tests");
    let steps = doc
        .get("tests")
        .and_then(|t| t.get(0))
        .and_then(|t| t.get("steps"))
        .and_then(|s| s.as_sequence())
        .map(|s| s.len());
    if !keys_ok || steps != Some(n_steps) {
        return Err(format!(
            "Recorded steps don't form a {n_steps}-step flow (each step must be one YAML list item)"
        ));
    }
    Ok(())
}

/// A relative path made only of normal components, ending in `.yaml`/`.yml`.
fn validate_relative_path(path: &str) -> Result<PathBuf, String> {
    let invalid = || format!("Invalid output path: {path:?}");
    if path.is_empty() || path.contains(['\0', '\n', '\r', '\\']) {
        return Err(invalid());
    }
    let p = Path::new(path);
    if !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!(
            "Invalid output path {path:?}: must be relative, without '..' or '.'"
        ));
    }
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    if !ext_ok {
        return Err(format!(
            "Invalid output path {path:?}: flows are saved as .yaml files"
        ));
    }
    Ok(p.to_path_buf())
}

/// Create `base/rel` with `create_new`: intermediate directories are created
/// as needed but must be real directories (no symlinks), and the file itself
/// must not exist (`O_EXCL` also refuses a symlink in its place).
fn create_new_file(base: &Path, rel: &Path, contents: &[u8]) -> Result<PathBuf, ApiError> {
    let internal = |what: &str, p: &Path, e: std::io::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to {what} {}: {e}", p.display()),
        )
    };
    std::fs::create_dir_all(base).map_err(|e| internal("create", base, e))?;
    let mut dir = base
        .canonicalize()
        .map_err(|e| internal("resolve", base, e))?;

    let parts: Vec<_> = rel.components().collect();
    let (file, parents) = parts.split_last().expect("validated non-empty path");
    for part in parents {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(bad(format!(
                    "Invalid output path: {} is not a directory (symlinks are not followed)",
                    dir.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir).map_err(|e| internal("create", &dir, e))?;
            }
            Err(e) => return Err(internal("inspect", &dir, e)),
        }
    }
    let path = dir.join(file);

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                (
                    StatusCode::CONFLICT,
                    format!("{} already exists; choose another name", path.display()),
                )
            } else {
                internal("create", &path, e)
            }
        })?;
    f.write_all(contents)
        .map_err(|e| internal("write", &path, e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_relative_yaml_paths_are_accepted() {
        for ok in ["a.yaml", "sub/dir/flow.yml", "x.YAML"] {
            assert!(validate_relative_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Cargo.toml",
            "../x.yaml",
            "a/../../x.yaml",
            "./x.yaml",
            "/etc/x.yaml",
            "a\\..\\x.yaml",
            "a\nb.yaml",
            ".git/hooks/pre-commit",
        ] {
            assert!(validate_relative_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn flow_shape_rejects_steps_that_are_not_one_item() {
        let one = vec!["- tap: { text: \"a\" }".to_string()];
        let y = yaml_gen::flow_yaml("n", "a", &one);
        assert!(check_flow_shape(&y, 1).is_ok(), "{y}");
        // Two list items smuggled into one step line can't happen without a
        // newline, but a broken flow mapping must still be refused.
        let broken = vec!["- tap: { text: \"a\" ".to_string()];
        let y = yaml_gen::flow_yaml("n", "a", &broken);
        assert!(check_flow_shape(&y, 1).is_err(), "{y}");
    }

    #[test]
    fn existing_files_and_symlinks_are_never_written() {
        let tmp = std::env::temp_dir().join(format!(
            "vel-insp-save-{}-{}",
            std::process::id(),
            crate::security::new_token()
        ));
        let base = tmp.join("flows");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.yaml"), "ORIGINAL").unwrap();
        std::fs::write(base.join("exists.yaml"), "ORIGINAL").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("victim.yaml"), base.join("link.yaml"))
                .unwrap();
            std::os::unix::fs::symlink(&outside, base.join("dirlink")).unwrap();
            let e = create_new_file(&base, Path::new("link.yaml"), b"X").unwrap_err();
            assert_eq!(e.0, StatusCode::CONFLICT);
            let e = create_new_file(&base, Path::new("dirlink/new.yaml"), b"X").unwrap_err();
            assert_eq!(e.0, StatusCode::BAD_REQUEST);
            assert!(!outside.join("new.yaml").exists());
        }
        let e = create_new_file(&base, Path::new("exists.yaml"), b"X").unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.yaml")).unwrap(),
            "ORIGINAL"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("exists.yaml")).unwrap(),
            "ORIGINAL"
        );

        let p = create_new_file(&base, Path::new("nested/new.yaml"), b"X").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "X");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
