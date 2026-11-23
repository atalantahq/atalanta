//! D-37: the session settings Velocity sends through
//! `POST /session/:id/appium/settings` right after the session starts,
//! against a fake WDA on 127.0.0.1. Its own test binary: the override
//! check installs settings for the whole process.
mod common;

use std::collections::HashMap;

use common::*;
use serde_json::Value;
use velocity_ios::wda::WdaClient;

async fn sent_settings(fake: &FakeWda) -> Value {
    let mut c = WdaClient::new(&fake.url);
    let sid = c.create_session("com.example.app").await.unwrap();
    assert_eq!(sid, "S1");
    let log = fake.log.lock().unwrap().clone();
    let req = log
        .iter()
        .rev()
        .find(|r| r.method == "POST" && r.path == "/session/S1/appium/settings")
        .unwrap_or_else(|| panic!("no settings request after the session started: {log:?}"));
    let body: Value = serde_json::from_str(&req.body).unwrap();
    body["settings"].clone()
}

#[tokio::test]
async fn the_session_bounds_the_accessibility_wait_and_the_snapshot_depth() {
    let fake = FakeWda::start(|r| {
        if r.method == "POST" && r.path == "/session" {
            return ok(r#"{"sessionId":"S1","capabilities":{}}"#);
        }
        ok("null")
    })
    .await;

    let s = sent_settings(&fake).await;
    // A WDA this process didn't build (version unknown) gets no deadline
    // by default: older WDA crashed WKWebView content with one (#1290).
    assert!(s.get("accessibilityDeadline").is_none(), "{s}");
    assert_eq!(s["snapshotMaxDepth"].as_u64(), Some(60), "{s}");
    // The settings that were already there are still sent.
    assert_eq!(s["waitForIdleTimeout"].as_u64(), Some(0), "{s}");
    assert_eq!(s["animationCoolOffTimeout"].as_u64(), Some(0), "{s}");
    assert_eq!(s["defaultActiveApplication"], "com.example.app", "{s}");

    // `ios.accessibilityDeadlineMs` / `ios.snapshotMaxDepth` change them.
    velocity_common::env::set_settings(HashMap::from([
        (
            "VELOCITY_IOS_ACCESSIBILITY_DEADLINE_MS".to_string(),
            "2500".to_string(),
        ),
        (
            "VELOCITY_IOS_SNAPSHOT_MAX_DEPTH".to_string(),
            "80".to_string(),
        ),
    ]));
    let s = sent_settings(&fake).await;
    assert_eq!(s["accessibilityDeadline"].as_f64(), Some(2.5), "{s}");
    assert_eq!(s["snapshotMaxDepth"].as_u64(), Some(80), "{s}");

    // 0 leaves WDA's own value alone.
    velocity_common::env::set_settings(HashMap::from([
        (
            "VELOCITY_IOS_ACCESSIBILITY_DEADLINE_MS".to_string(),
            "0".to_string(),
        ),
        (
            "VELOCITY_IOS_SNAPSHOT_MAX_DEPTH".to_string(),
            "0".to_string(),
        ),
    ]));
    let s = sent_settings(&fake).await;
    assert!(s.get("accessibilityDeadline").is_none(), "{s}");
    assert!(s.get("snapshotMaxDepth").is_none(), "{s}");
    velocity_common::env::set_settings(HashMap::new());
}
