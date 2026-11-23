//! `type_text {secret: NAME}` types the environment value, never returns
//! it, and the export writes `${NAME}`. Its own test binary (review 07 M7):
//! it sets process environment variables, which would race every other
//! test's environment reads in the library's test binary.

use std::sync::Arc;

use serde_json::{json, Value};
use velocity_common::{Platform, PlatformDriver};
use velocity_mcp::McpServer;

const VALUE: &str = "hunter2-correct-horse";

/// The first string under `key` anywhere in `v` (the tool result wraps
/// its JSON in a text block).
fn find_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => m
            .get(key)
            .and_then(|s| s.as_str().map(String::from))
            .or_else(|| m.values().find_map(|x| find_str(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_str(x, key)),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|p| p.is_object())
            .and_then(|p| find_str(&p, key)),
        _ => None,
    }
}

#[tokio::test]
async fn secret_by_name_is_typed_not_echoed_and_exported_as_env() {
    // Only test in this binary: nothing else reads these meanwhile.
    std::env::set_var("VELOCITY_MCP_TEST_LOGIN_PW", VALUE);
    std::env::set_var("VELOCITY_MCP_TEST_AWS_SECRET_KEY", "exfil-me-123");
    // Review MAJOR 4: only listed names; anything else is refused.
    std::env::set_var("VELOCITY_SECRETS", "VELOCITY_MCP_TEST_LOGIN_PW");
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("main.xml");
    std::fs::write(
        &layout,
        r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
            android:layout_width="match_parent" android:layout_height="match_parent"
            android:orientation="vertical">
          <EditText android:id="@+id/pw" android:layout_width="200dp" android:layout_height="48dp"/>
        </LinearLayout>"#,
    )
    .unwrap();
    let config = velocity_headless::HeadlessConfig {
        app_path: Some(layout.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
        Platform::Android,
        config,
    ));
    driver.launch_app("headless", "app", false).await.unwrap();
    let mut s = McpServer::new(driver.clone(), "headless".into(), None);
    let typed = s
        .call(
            "type_text",
            json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_LOGIN_PW" }),
        )
        .await
        .unwrap();
    assert!(typed.get("isError").is_none(), "{typed}");
    assert!(!typed.to_string().contains(VALUE), "{typed}");
    assert!(typed.to_string().contains("VELOCITY_MCP_TEST_LOGIN_PW"));
    // The value reached the field.
    let tree = driver.get_hierarchy("headless").await.unwrap();
    let field = velocity_common::selector_match::find_in_tree(
        &tree,
        &velocity_common::Selector::Id("pw".into()),
        None,
    )
    .unwrap();
    assert_eq!(field.text.as_deref(), Some(VALUE));
    // A later result that reads the field back is masked.
    let got = s
        .call("get_element", json!({ "selector": { "id": "pw" } }))
        .await
        .unwrap();
    assert!(!got.to_string().contains(VALUE), "{got}");
    let exported = s
        .call(
            "export_session_flow",
            json!({ "app_id": "app", "name": "t" }),
        )
        .await
        .unwrap();
    let yaml = find_str(&exported, "yaml").unwrap_or_else(|| panic!("{exported}"));
    assert!(yaml.contains("${VELOCITY_MCP_TEST_LOGIN_PW}"), "{yaml}");
    assert!(!yaml.contains(VALUE));
    let suite = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
    velocity_core::validate_suite(&suite).unwrap();
    // Unset, both, or a bad name: a clear error that names no value.
    for args in [
        json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_UNSET_X" }),
        json!({ "selector": { "id": "pw" }, "secret": "A", "text": "b" }),
        json!({ "selector": { "id": "pw" }, "secret": "$(id)" }),
        json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_AWS_SECRET_KEY" }),
    ] {
        let r = s.call("type_text", args).await.unwrap();
        assert_eq!(r["isError"], true, "{r}");
        assert!(!r.to_string().contains("exfil-me-123"), "{r}");
    }
    let r = s
        .call(
            "type_text",
            json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_AWS_SECRET_KEY" }),
        )
        .await
        .unwrap();
    assert!(r.to_string().contains("VELOCITY_SECRETS"), "{r}");
    let tree = driver.get_hierarchy("headless").await.unwrap();
    assert!(!format!("{tree:?}").contains("exfil-me-123"), "not typed");
}
