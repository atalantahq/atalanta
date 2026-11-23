// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Versioned hierarchy snapshots with stable node keys (research P-5).
//!
//! The WebSocket sends a device's hierarchy only when it changed, as a
//! snapshot with a version and a `nodeKey` (the tree path, `0/3/1`) on each
//! node. The page refers to an element by `(version, nodeKey)`, and the server
//! resolves it against the snapshot it sent, so the client never posts a whole
//! tree back (VEL-116: no 2 MB body or nesting limits on selector generation).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use atalanta_common::types::Element;
use serde_json::Value;

/// One device's hierarchy as the page last received it.
pub struct Snapshot {
    pub version: u64,
    pub device_id: String,
    pub tree: Element,
    pub hash: u64,
}

impl Snapshot {
    /// The element at `key` (a path such as `0/3/1`; `0` is the root).
    pub fn resolve(&self, key: &str) -> Option<&Element> {
        let mut parts = key.split('/');
        if parts.next()? != "0" {
            return None;
        }
        let mut node = &self.tree;
        for part in parts {
            node = node.children.get(part.parse::<usize>().ok()?)?;
        }
        Some(node)
    }

    /// The `hierarchy` message: the tree with a `nodeKey` on every node.
    pub fn message(&self) -> Value {
        let mut root = serde_json::to_value(&self.tree).unwrap_or(Value::Null);
        add_node_keys(&mut root);
        serde_json::json!({
            "type": "hierarchy",
            "version": self.version,
            "root": root,
        })
    }
}

/// A content hash of `tree`, to tell whether a fresh dump changed anything.
pub fn tree_hash(tree: &Element) -> u64 {
    let mut h = DefaultHasher::new();
    serde_json::to_string(tree).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// A content hash of screenshot bytes.
pub fn bytes_hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Add `nodeKey` (the tree path) to each node of a serialised element tree.
/// Iterative, so a deep tree can't overflow the stack here.
fn add_node_keys(root: &mut Value) {
    let mut stack: Vec<(&mut Value, String)> = vec![(root, "0".to_string())];
    while let Some((node, key)) = stack.pop() {
        let Some(obj) = node.as_object_mut() else {
            continue;
        };
        obj.insert("nodeKey".into(), Value::String(key.clone()));
        if let Some(Value::Array(children)) = obj.get_mut("children") {
            for (i, child) in children.iter_mut().enumerate() {
                stack.push((child, format!("{key}/{i}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::types::Rect;

    fn node(text: &str, children: Vec<Element>) -> Element {
        Element {
            hint: None,
            platform_id: String::new(),
            label: None,
            text: Some(text.into()),
            element_type: "View".into(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            enabled: true,
            visible: true,
            children,
            ..Default::default()
        }
    }

    #[test]
    fn node_keys_resolve_to_the_node_they_were_sent_on() {
        let tree = node(
            "root",
            vec![node("a", vec![]), node("b", vec![node("c", vec![])])],
        );
        let snap = Snapshot {
            version: 7,
            device_id: "d".into(),
            hash: tree_hash(&tree),
            tree,
        };
        let msg = snap.message();
        assert_eq!(msg["version"], 7);
        let c = &msg["root"]["children"][1]["children"][0];
        assert_eq!(c["nodeKey"], "0/1/0");
        let key = c["nodeKey"].as_str().unwrap();
        assert_eq!(snap.resolve(key).unwrap().text.as_deref(), Some("c"));
        assert!(snap.resolve("0/5").is_none());
        assert!(snap.resolve("1").is_none());
        assert!(snap.resolve("0/x").is_none());
    }

    #[test]
    fn hash_changes_only_with_content() {
        let a = node("a", vec![]);
        assert_eq!(tree_hash(&a), tree_hash(&a.clone()));
        assert_ne!(tree_hash(&a), tree_hash(&node("b", vec![])));
    }
}
