//! The live view: change-driven pushes, versioned snapshots and selector
//! generation by node key (research P-5; VEL-116, VEL-260).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use velocity_common::types::{DeviceInfo, Direction, Element, Key, Rect, Selector};
use velocity_common::{PlatformDriver, Result};
use velocity_inspector::routes::selector::{generate, GenerateRequest};
use velocity_inspector::state::AppState;
use velocity_inspector::ws::{poll, Sent};

struct Live {
    fail: Mutex<bool>,
    tree: Mutex<Element>,
    png: Mutex<Vec<u8>>,
    dumps: Mutex<usize>,
}

fn node(id: &str, text: &str, y: i32, children: Vec<Element>) -> Element {
    Element {
        hint: None,
        platform_id: id.into(),
        label: None,
        text: (!text.is_empty()).then(|| text.to_string()),
        element_type: "Button".into(),
        bounds: Rect {
            x: 0,
            y,
            width: 100,
            height: 10,
        },
        enabled: true,
        visible: true,
        children,
        ..Default::default()
    }
}

fn screen() -> Element {
    let mut root = node(
        "",
        "",
        0,
        vec![node("", "Save", 0, vec![]), node("", "Save", 20, vec![])],
    );
    root.bounds.height = 1000;
    root
}

fn live_with_driver() -> (Arc<AppState>, Arc<Live>) {
    let driver = Arc::new(Live {
        fail: Mutex::new(false),
        tree: Mutex::new(screen()),
        png: Mutex::new(vec![1, 2, 3]),
        dumps: Mutex::new(0),
    });
    let state = Arc::new(AppState::new(driver.clone(), Some("d".into()), None));
    (state, driver)
}

fn live() -> Arc<AppState> {
    live_with_driver().0
}

fn kinds(msgs: &[serde_json::Value]) -> Vec<String> {
    msgs.iter()
        .map(|m| m["type"].as_str().unwrap().to_string())
        .collect()
}

#[async_trait::async_trait]
impl PlatformDriver for Live {
    async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }
    async fn boot_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn shutdown_device(&self, _: &str) -> Result<()> {
        Ok(())
    }
    async fn install_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
        Ok(self.tree.lock().unwrap().clone())
    }
    async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
        Ok(vec![])
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        *self.dumps.lock().unwrap() += 1;
        if *self.fail.lock().unwrap() {
            return Err(velocity_common::VelocityError::Internal(anyhow::anyhow!(
                "device went away"
            )));
        }
        Ok(self.tree.lock().unwrap().clone())
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
        Ok(())
    }
    async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
        Ok(())
    }
    async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
        Ok(())
    }
    async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
        Ok(())
    }
    async fn press_key(&self, _: &str, _: Key) -> Result<()> {
        Ok(())
    }
    async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
        Ok(self.png.lock().unwrap().clone())
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((100, 100))
    }
    async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
        Ok(String::new())
    }
    async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
        Ok(true)
    }
}

#[tokio::test]
async fn hierarchy_and_screenshot_are_sent_only_on_change() {
    let (state, driver) = live_with_driver();
    let mut sent = Sent::default();
    let (first, changed) = poll(&state, "d", &mut sent).await;
    assert!(changed);
    assert_eq!(kinds(&first), ["screenshot", "hierarchy"]);
    for _ in 0..5 {
        let (msgs, changed) = poll(&state, "d", &mut sent).await;
        assert!(msgs.is_empty(), "{msgs:?}");
        assert!(!changed);
    }
    // The screen changes: only the hierarchy, under a new version.
    let v1 = first[1]["version"].as_u64().unwrap();
    driver.tree.lock().unwrap().children[0].text = Some("Saved".into());
    let (msgs, changed) = poll(&state, "d", &mut sent).await;
    assert!(changed);
    assert_eq!(kinds(&msgs), ["hierarchy"]);
    assert!(msgs[0]["version"].as_u64().unwrap() > v1);
    // A new frame: only the screenshot, by content hash.
    *driver.png.lock().unwrap() = vec![9];
    let (msgs, _) = poll(&state, "d", &mut sent).await;
    assert_eq!(kinds(&msgs), ["screenshot"]);
    assert!(msgs[0]["url"].as_str().unwrap().contains("?v="));
}

#[tokio::test]
async fn selectors_come_from_the_node_key_with_every_candidate_counted() {
    let (state, driver) = live_with_driver();
    let (msgs, _) = poll(&state, "d", &mut Sent::default()).await;
    let hierarchy = &msgs[1];
    let version = hierarchy["version"].as_u64().unwrap();
    let second = &hierarchy["root"]["children"][1];
    assert_eq!(second["nodeKey"], "0/1");

    let Json(resp) = generate(
        State(state.clone()),
        Json(GenerateRequest {
            node_key: Some("0/1".into()),
            version: Some(version),
            element: None,
            tree: None,
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("{e:?}"));
    // Two "Save" buttons: the chosen selector must single out the second.
    assert!(
        matches!(&resp.selector, Selector::Index { index: 1, .. }),
        "{:?}",
        resp.selector
    );
    let text = resp
        .candidates
        .iter()
        .find(|c| c.selector == Selector::Text("Save".into()))
        .expect("text candidate listed");
    assert_eq!(text.match_count, 2);
    assert!(resp.yaml_input.starts_with("- inputText:"));

    // Review m13: a second tab (its own socket) polling the same screen
    // keeps the version, so neither tab gets 409 forever.
    let (other, _) = poll(&state, "d", &mut Sent::default()).await;
    assert_eq!(other[1]["version"].as_u64(), Some(version));
    let _ = generate(
        State(state.clone()),
        Json(GenerateRequest {
            node_key: Some("0/1".into()),
            version: Some(version),
            element: None,
            tree: None,
        }),
    )
    .await
    .unwrap_or_else(|e| panic!("the first tab's version still resolves: {e:?}"));

    // A version that's no longer current is a conflict, not a wrong answer.
    driver.tree.lock().unwrap().children[0].text = Some("Saved".into());
    let (_, _) = poll(&state, "d", &mut Sent::default()).await;
    let err = generate(
        State(state.clone()),
        Json(GenerateRequest {
            node_key: Some("0/1".into()),
            version: Some(version),
            element: None,
            tree: None,
        }),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn deep_and_huge_trees_need_no_round_trip() {
    // Deeper than serde_json's 128-level limit and with 20k nodes: the page
    // never posts the tree back, so neither limit applies (VEL-116).
    let mut deep = node("", "leaf", 0, vec![]);
    for _ in 0..500 {
        deep = node("", "", 0, vec![deep]);
    }
    let wide: Vec<Element> = (0..20_000)
        .map(|i| node("", &format!("row {i}"), i, vec![]))
        .collect();
    let mut root = node("", "", 0, vec![deep, node("", "", 0, wide)]);
    root.bounds.height = 100_000;
    let state = live();
    let version = state.publish_snapshot("d", root, 1).await.version;
    let key = format!("0/0{}", "/0".repeat(500));
    let Json(resp) = tokio::task::spawn(async move {
        generate(
            State(state),
            Json(GenerateRequest {
                node_key: Some(key),
                version: Some(version),
                element: None,
                tree: None,
            }),
        )
        .await
    })
    .await
    .unwrap()
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(resp.selector, Selector::Text("leaf".into()));
}

#[tokio::test]
async fn an_error_is_sent_once_and_cleared_when_it_goes() {
    let (state, driver) = live_with_driver();
    let mut sent = Sent::default();
    poll(&state, "d", &mut sent).await;
    *driver.fail.lock().unwrap() = true;
    let (msgs, _) = poll(&state, "d", &mut sent).await;
    assert_eq!(kinds(&msgs), ["error"]);
    for _ in 0..3 {
        let (msgs, _) = poll(&state, "d", &mut sent).await;
        assert!(msgs.is_empty(), "the same error again: {msgs:?}");
    }
    *driver.fail.lock().unwrap() = false;
    let (msgs, _) = poll(&state, "d", &mut sent).await;
    assert_eq!(kinds(&msgs), ["ok"]);
}

#[test]
fn device_ids_are_encoded_in_urls() {
    assert_eq!(
        velocity_inspector::ws::encode_path_segment("emulator-5554"),
        "emulator-5554"
    );
    assert_eq!(
        velocity_inspector::ws::encode_path_segment("a/b?c#d e"),
        "a%2Fb%3Fc%23d%20e"
    );
}
