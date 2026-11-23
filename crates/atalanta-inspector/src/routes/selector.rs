use std::sync::Arc;

use atalanta_common::types::{Element, Selector};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::selector_gen;
use crate::state::AppState;
use crate::yaml_gen;

/// Either `{nodeKey, version}` (an element of a snapshot the live view sent,
/// resolved on the server) or the older `{element, tree?}` body.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateRequest {
    #[serde(default)]
    pub node_key: Option<String>,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub element: Option<Element>,
    /// The screen the element is on. With it the selector is checked for
    /// uniqueness (scoped or indexed when needed); without, it's a guess.
    #[serde(default)]
    pub tree: Option<Element>,
}

#[derive(Serialize)]
pub struct Candidate {
    /// The DSL's flat form (`{"text": "Pay", "index": 1}`), review 02 M5.
    #[serde(with = "atalanta_core::selector_wire")]
    pub selector: Selector,
    pub yaml: String,
    #[serde(rename = "matchCount")]
    pub match_count: usize,
}

#[derive(Serialize)]
pub struct GenerateResponse {
    #[serde(with = "atalanta_core::selector_wire")]
    pub selector: Selector,
    pub yaml_tap: String,
    pub yaml_assert: String,
    pub yaml_input: String,
    /// Only for `{nodeKey, version}` or `{element, tree}` requests.
    pub candidates: Vec<Candidate>,
}

pub async fn generate(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GenerateRequest>,
) -> Result<Json<GenerateResponse>, (StatusCode, String)> {
    let snapshot;
    let (tree, element): (Option<&Element>, &Element) = match (&req.node_key, req.version) {
        (Some(key), Some(version)) => {
            snapshot = state.snapshot(version).await.ok_or((
                StatusCode::CONFLICT,
                format!("hierarchy version {version} is no longer current; refresh"),
            ))?;
            let element = snapshot.resolve(key).ok_or((
                StatusCode::NOT_FOUND,
                format!("no node {key} in version {version}"),
            ))?;
            (Some(&snapshot.tree), element)
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "nodeKey and version go together".into(),
            ))
        }
        (None, None) => match &req.element {
            Some(element) => (req.tree.as_ref(), element),
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "send {nodeKey, version} or {element}".into(),
                ))
            }
        },
    };

    let selector = match tree {
        Some(tree) => selector_gen::generate_selector_in(tree, element),
        None => selector_gen::generate_selector(element),
    };
    let candidates = tree
        .map(|tree| {
            selector_gen::candidates_with_counts(tree, element, &selector)
                .into_iter()
                .map(|(selector, match_count)| Candidate {
                    yaml: yaml_gen::tap_yaml(&selector),
                    selector,
                    match_count,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(GenerateResponse {
        yaml_tap: yaml_gen::tap_yaml(&selector),
        yaml_assert: yaml_gen::assert_visible_yaml(&selector),
        yaml_input: yaml_gen::type_text_yaml(&selector, ""),
        selector,
        candidates,
    }))
}
