//! FlutterDriver against a fake Dart bridge on 127.0.0.1.

use std::sync::{Arc, Mutex};

use atalanta_common::{AtalantaError, Element, Platform, PlatformDriver, Rect, Selector};
use atalanta_flutter_bridge::{FlutterBridgeConfig, FlutterDriver, HierarchyMode};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

type Log = Arc<Mutex<Vec<Value>>>;

/// A fake bridge: `reply(cmd)` gives the reply body; the request id is echoed
/// back unless `echo_ids` is false (an older server).
async fn serve<F>(echo_ids: bool, reply: F) -> (u16, Log)
where
    F: Fn(&Value) -> Value + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let reply = Arc::new(reply);
    let log2 = log.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let (reply, log) = (reply.clone(), log2.clone());
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push(cmd.clone());
                    let mut body = reply(&cmd);
                    if echo_ids {
                        body["id"] = cmd["id"].clone();
                    }
                    let _ = w.write_all(format!("{body}\n").as_bytes()).await;
                }
            });
        }
    });
    (port, log)
}

fn ok() -> Value {
    json!({"status": "ok"})
}

fn nested(depth: usize, label: &str) -> Value {
    let mut node = json!({"id": "leaf", "label": label, "text": label, "type": "Button",
        "bounds": {"x": 10, "y": 10, "width": 100, "height": 40}, "children": []});
    for i in 0..depth {
        node = json!({"id": format!("n{i}"), "type": "View",
            "bounds": {"x": 0, "y": 0, "width": 1080, "height": 2340}, "children": [node]});
    }
    node
}

fn login_screen() -> Value {
    json!({"format": "flat", "nodes": [
        {"node": 0, "parent": null, "type": "Root", "bounds": {"x": 0, "y": 0, "width": 800, "height": 600}},
        {"node": 4, "parent": 0, "id": "email", "type": "TextField", "bounds": {"x": 0, "y": 0, "width": 800, "height": 56}},
        {"node": 6, "parent": 0, "id": "password", "type": "TextField", "bounds": {"x": 0, "y": 96, "width": 800, "height": 56}},
        {"node": 7, "parent": 0, "label": "Log in", "text": "Log in", "type": "Button", "bounds": {"x": 10, "y": 160, "width": 100, "height": 40}},
        {"node": 8, "parent": 0, "label": "Gone", "text": "Gone", "type": "Button", "enabled": false, "bounds": {"x": 10, "y": 210, "width": 100, "height": 40}}
    ]})
}

fn hierarchy_reply(h: Value) -> impl Fn(&Value) -> Value {
    move |cmd| match cmd["cmd"].as_str() {
        Some("get_hierarchy") => json!({"status": "ok", "data": h}),
        _ => ok(),
    }
}

async fn driver(port: u16) -> FlutterDriver {
    let d = FlutterDriver::new(
        Platform::Android,
        FlutterBridgeConfig {
            port,
            ..Default::default()
        },
    );
    d.prepare("flutter-0").await.unwrap();
    d
}

fn sent(log: &Log, from: usize) -> Vec<Value> {
    log.lock().unwrap()[from..].to_vec()
}

fn cmds(log: &Log) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .map(|c| c["cmd"].as_str().unwrap().to_string())
        .collect()
}

// VEL-023: a realistic Element tree is ~200 levels deep, past serde_json's
// recursion limit; it must still parse (older servers send it nested).
#[tokio::test]
async fn a_200_level_nested_tree_parses() {
    let (port, _) = serve(true, hierarchy_reply(nested(200, "Deep"))).await;
    let d = driver(port).await;
    let found = d
        .find_element("flutter-0", &Selector::Text("Deep".into()))
        .await
        .unwrap();
    assert_eq!(found.platform_id, "leaf");
}

#[tokio::test]
async fn flat_hierarchies_rebuild_the_tree() {
    let (port, _) = serve(true, hierarchy_reply(login_screen())).await;
    let d = driver(port).await;
    let root = d.get_hierarchy("flutter-0").await.unwrap();
    assert_eq!(root.children.len(), 4);
    let pw = d
        .find_element("flutter-0", &Selector::Id("password".into()))
        .await
        .unwrap();
    assert_eq!(pw.bounds.y, 96);
    let gone = d
        .find_element("flutter-0", &Selector::Text("Gone".into()))
        .await
        .unwrap();
    assert!(!gone.enabled);
}

// VEL-024: text entry names its field: node id, element id and centre.
#[tokio::test]
async fn input_and_clear_text_name_their_field() {
    let (port, log) = serve(true, hierarchy_reply(login_screen())).await;
    let d = driver(port).await;
    let pw = d
        .find_element("flutter-0", &Selector::Id("password".into()))
        .await
        .unwrap();
    let before = log.lock().unwrap().len();
    d.input_text("flutter-0", &pw, "hunter2").await.unwrap();
    d.clear_text("flutter-0", &pw).await.unwrap();
    let sent = sent(&log, before);
    assert_eq!(sent.len(), 2, "no extra pump after an action: {sent:?}");
    assert_eq!(sent[0]["cmd"], "input_text");
    assert_eq!(sent[0]["text"], "hunter2");
    assert_eq!(
        sent[0]["target"],
        json!({"node": 6, "id": "password", "x": 400, "y": 124})
    );
    assert_eq!(sent[1]["cmd"], "clear_text");
    assert_eq!(sent[1]["target"]["node"], 6);

    // An element from elsewhere (no node known) still sends its id and point.
    let other = Element {
        hint: None,
        platform_id: "promo".into(),
        label: None,
        text: None,
        element_type: "TextField".into(),
        bounds: Rect {
            x: 0,
            y: 400,
            width: 300,
            height: 50,
        },
        enabled: true,
        visible: true,
        children: vec![],
        ..Default::default()
    };
    d.input_text("flutter-0", &other, "x").await.unwrap();
    let last = log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["target"], json!({"id": "promo", "x": 150, "y": 425}));
}

#[tokio::test]
async fn taps_carry_the_target_and_the_legacy_point() {
    let (port, log) = serve(true, hierarchy_reply(login_screen())).await;
    let d = driver(port).await;
    let b = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    d.tap("flutter-0", &b).await.unwrap();
    let tap = log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(tap["x"], 60);
    assert_eq!(tap["y"], 180);
    assert_eq!(tap["target"]["node"], 7);
}

// VEL-135 (Flutter side): an error reply fails the action, classified as a
// command that ran and failed, with the bridge's code.
#[tokio::test]
async fn error_replies_fail_every_action() {
    let (port, _) = serve(true, |cmd| match cmd["cmd"].as_str() {
        Some("init") | Some("get_hierarchy") => {
            json!({"status": "ok", "data": login_screen()})
        }
        _ => json!({"status": "error", "code": "not_hittable", "message": "covered"}),
    })
    .await;
    let d = driver(port).await;
    let b = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    let results = [
        d.tap("flutter-0", &b).await,
        d.double_tap("flutter-0", &b).await,
        d.long_press("flutter-0", &b, 500).await,
        d.input_text("flutter-0", &b, "x").await,
        d.clear_text("flutter-0", &b).await,
        d.swipe_coords("flutter-0", (1, 1), (2, 2)).await,
        d.press_key("flutter-0", atalanta_common::Key::Home).await,
    ];
    for r in results {
        match r {
            Err(AtalantaError::CommandFailed { detail, .. }) => {
                assert!(detail.starts_with("not_hittable"), "{detail}")
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }
}

// VEL-280: the action settles on the Dart side; a failing follow-up pump can
// no longer turn a tap that happened into a failure.
#[tokio::test]
async fn no_follow_up_pump_after_actions() {
    let (port, log) = serve(true, |cmd| match cmd["cmd"].as_str() {
        Some("pump_frames") => json!({"status": "error", "message": "pump failed"}),
        Some("get_hierarchy") => json!({"status": "ok", "data": login_screen()}),
        _ => ok(),
    })
    .await;
    let d = driver(port).await;
    let b = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    d.tap("flutter-0", &b).await.unwrap();
    d.swipe("flutter-0", atalanta_common::Direction::Up)
        .await
        .unwrap();
    assert!(!cmds(&log).contains(&"pump_frames".to_string()));
}

// VEL-140: stopApp leaves the bridge serving, so launchApp works after it,
// and cleanup never shuts down a bridge the driver did not start.
#[tokio::test]
async fn stop_app_then_launch_app_works_and_nothing_is_shut_down() {
    let (port, log) = serve(true, |_| ok()).await;
    let d = driver(port).await;
    d.stop_app("flutter-0", "app").await.unwrap();
    d.launch_app("flutter-0", "app", false).await.unwrap();
    d.cleanup().await;
    d.prepare("flutter-0").await.unwrap();
    let cmds = cmds(&log);
    assert!(!cmds.contains(&"shutdown".to_string()), "{cmds:?}");
    assert_eq!(cmds, ["init", "reset", "init", "init"]);
}

// VEL-141: launchApp with clearState re-mounts the app with its state reset.
#[tokio::test]
async fn launch_app_clear_state_sends_init_with_clear() {
    let (port, log) = serve(true, |_| ok()).await;
    let d = driver(port).await;
    let before = log.lock().unwrap().len();
    d.launch_app("flutter-0", "app", true).await.unwrap();
    let sent = sent(&log, before);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["cmd"], "init");
    assert_eq!(sent[0]["clear"], true);
}

#[tokio::test]
async fn requests_carry_ids_and_old_servers_without_ids_still_work() {
    let (port, log) = serve(false, hierarchy_reply(login_screen())).await;
    let d = driver(port).await;
    d.get_hierarchy("flutter-0").await.unwrap();
    let ids: Vec<u64> = log
        .lock()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [1, 2]);
}

#[tokio::test]
async fn the_semantics_hierarchy_is_the_default_and_widgets_opts_out() {
    // D-15: semantics unless `hierarchy: widgets` (or the env var) says so.
    let (port, log) = serve(true, hierarchy_reply(login_screen())).await;
    if std::env::var_os("ATALANTA_FLUTTER_HIERARCHY").is_none() {
        let d = driver(port).await;
        d.get_hierarchy("flutter-0").await.unwrap();
        assert_eq!(log.lock().unwrap()[1]["mode"], "semantics");
    }

    let w = FlutterDriver::new(
        Platform::Android,
        FlutterBridgeConfig {
            port,
            hierarchy: Some(HierarchyMode::Widgets),
            ..Default::default()
        },
    );
    w.prepare("flutter-0").await.unwrap();
    w.get_hierarchy("flutter-0").await.unwrap();
    assert!(log.lock().unwrap().last().unwrap().get("mode").is_none());
}

// Review MINOR 14: a node is sent with its snapshot's generation, and a
// hierarchy Rust could not parse (but Dart already stored) drops the old
// node ids instead of sending numbers that now name other elements.
#[tokio::test]
async fn targets_carry_the_snapshot_generation_and_a_bad_snapshot_drops_node_ids() {
    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let f = fetches.clone();
    let (port, log) = serve(true, move |cmd| match cmd["cmd"].as_str() {
        Some("get_hierarchy") => {
            if f.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let mut h = login_screen();
                h["generation"] = json!(5);
                json!({"status": "ok", "data": h})
            } else {
                json!({"status": "ok", "data": {"format": "flat", "nodes": "garbage"}})
            }
        }
        _ => ok(),
    })
    .await;
    let d = driver(port).await;
    let button = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    d.tap("flutter-0", &button).await.unwrap();
    let tap = sent(&log, 0)
        .into_iter()
        .rfind(|c| c["cmd"] == "tap")
        .unwrap();
    assert_eq!(tap["target"]["node"], 7);
    assert_eq!(tap["target"]["generation"], 5);

    assert!(d.get_hierarchy("flutter-0").await.is_err());
    d.tap("flutter-0", &button).await.unwrap();
    let tap = sent(&log, 0)
        .into_iter()
        .rfind(|c| c["cmd"] == "tap")
        .unwrap();
    assert!(tap["target"].get("node").is_none(), "{tap}");
    assert!(tap["target"].get("generation").is_none(), "{tap}");
    assert_eq!(tap["target"]["x"], 60);
}

fn surface_reply(cmd: &Value) -> Value {
    match cmd["cmd"].as_str() {
        Some("init") => json!({"status": "ok", "data": {"protocol": 2, "settled": true,
                                "size": {"width": 800, "height": 600}, "dpr": 3.0}}),
        Some("get_hierarchy") => json!({"status": "ok", "data": {"format": "flat", "nodes": [
            {"node": 0, "parent": null, "type": "Root", "bounds": {"x": 0, "y": 0, "width": 800, "height": 600}},
            {"node": 1, "parent": 0, "text": "Item", "type": "Text", "bounds": {"x": 0, "y": 1000, "width": 100, "height": 40}},
            {"node": 2, "parent": 0, "text": "Item", "type": "Text", "bounds": {"x": 0, "y": 100, "width": 100, "height": 40}}
        ]}}),
        _ => ok(),
    }
}

// VEL-S39: the driver uses the surface the bridge reports, so screen size,
// swipes and the on-screen filter match the real test surface; the surface
// itself is resized only when a pixel ratio is configured.
#[tokio::test]
async fn the_reported_surface_is_the_screen() {
    let (port, log) = serve(true, surface_reply).await;
    let d = driver(port).await;
    let init = sent(&log, 0)
        .into_iter()
        .find(|c| c["cmd"] == "init")
        .unwrap();
    assert!(init.get("dpr").is_none(), "{init}");
    assert_eq!(d.screen_size("flutter-0").await.unwrap(), (800, 600));
    let found = d
        .find_elements("flutter-0", &Selector::Text("Item".into()))
        .await
        .unwrap();
    assert_eq!(
        found.len(),
        1,
        "the item below the 600 px surface is off screen"
    );
    assert_eq!(found[0].bounds.y, 100);
    let before = log.lock().unwrap().len();
    d.swipe("flutter-0", atalanta_common::Direction::Up)
        .await
        .unwrap();
    let swipe = sent(&log, before).pop().unwrap();
    assert_eq!(swipe["from_x"], 400);
    assert!(swipe["from_y"].as_i64().unwrap() < 600, "{swipe}");
}

#[tokio::test]
async fn a_configured_pixel_ratio_is_sent_and_old_bridges_keep_the_configured_size() {
    let (port, log) = serve(true, |_| ok()).await;
    let d = FlutterDriver::new(
        Platform::Android,
        FlutterBridgeConfig {
            port,
            device_pixel_ratio: Some(2.625),
            ..Default::default()
        },
    );
    d.prepare("flutter-0").await.unwrap();
    let init = sent(&log, 0)
        .into_iter()
        .find(|c| c["cmd"] == "init")
        .unwrap();
    assert_eq!(init["dpr"], 2.625);
    assert_eq!(init["width"], 1080);
    assert_eq!(init["height"], 2340);
    // This fake reports no size, like an older bridge.
    assert_eq!(d.screen_size("flutter-0").await.unwrap(), (1080, 2340));
}

// P11: hit-test visibility is opt-in; with it, a covered element is not
// visible.
#[tokio::test]
async fn hit_test_visibility_is_opt_in() {
    let reply = |cmd: &Value| match cmd["cmd"].as_str() {
        Some("hit_test") => json!({"status": "ok", "data": {"hittable": false}}),
        _ => hierarchy_reply(login_screen())(cmd),
    };
    let (port, log) = serve(true, reply).await;
    let d = driver(port).await;
    let button = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    assert!(d.is_element_visible("flutter-0", &button).await.unwrap());
    assert!(!cmds(&log).contains(&"hit_test".to_string()));

    let d = FlutterDriver::new(
        Platform::Android,
        FlutterBridgeConfig {
            port,
            hit_test_visibility: Some(true),
            ..Default::default()
        },
    );
    d.prepare("flutter-0").await.unwrap();
    let button = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    assert!(!d.is_element_visible("flutter-0", &button).await.unwrap());
    let hit = sent(&log, 0)
        .into_iter()
        .find(|c| c["cmd"] == "hit_test")
        .unwrap();
    assert_eq!(hit["target"]["node"], 7);
}

// Actions settle on the Dart side and nothing moves between commands, so the
// sync engine needn't poll out its reaction window: settled at once, and
// "reacted" only when the action changed the tree.
#[tokio::test]
async fn settles_at_once_and_reports_whether_the_screen_changed() {
    let taps = Arc::new(Mutex::new(0));
    let taps2 = taps.clone();
    let (port, _log) = serve(true, move |cmd| match cmd["cmd"].as_str() {
        Some("tap") => {
            *taps2.lock().unwrap() += 1;
            ok()
        }
        Some("get_hierarchy") => {
            // The first tap changes the screen; the second does nothing.
            let label = if *taps2.lock().unwrap() == 0 {
                "Log in"
            } else {
                "Welcome"
            };
            json!({"status": "ok", "data": nested(1, label)})
        }
        _ => ok(),
    })
    .await;
    let d = driver(port).await;
    let first = d
        .settle_on_device("flutter-0", 300, 100, 5000)
        .await
        .unwrap();
    assert!(first.settled);
    let b = d
        .find_element("flutter-0", &Selector::Text("Log in".into()))
        .await
        .unwrap();
    d.tap("flutter-0", &b).await.unwrap();
    let changed = d
        .settle_on_device("flutter-0", 300, 100, 5000)
        .await
        .unwrap();
    assert!(changed.settled && changed.reacted, "{changed:?}");
    d.tap("flutter-0", &b).await.unwrap();
    let same = d
        .settle_on_device("flutter-0", 300, 100, 5000)
        .await
        .unwrap();
    assert!(same.settled && !same.reacted, "{same:?}");
    assert_eq!(*taps.lock().unwrap(), 2);
}

// Review (November 2026): the bridge answers `unsupported` for a key it
// can't send; that is a platform limit (exit 2, never retried), not a device
// command that failed (exit 3).
#[tokio::test]
async fn a_key_the_bridge_cannot_send_is_unsupported() {
    let (port, _) = serve(true, |cmd| match cmd["cmd"].as_str() {
        Some("press_key") => json!({"status": "error", "code": "unsupported",
            "message": "key home is not supported by the Flutter bridge"}),
        _ => ok(),
    })
    .await;
    let d = driver(port).await;
    let err = d
        .press_key("flutter-0", atalanta_common::Key::Home)
        .await
        .unwrap_err();
    assert!(matches!(err, AtalantaError::Unsupported(_)), "{err:?}");
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("home"), "{err}");
    assert!(!err.kind().is_transient());
}

// An older Dart package lacks `reset`/`hit_test`: say what to update.
#[tokio::test]
async fn a_bridge_without_a_command_says_to_update_the_package() {
    let (port, _) = serve(true, |cmd| match cmd["cmd"].as_str() {
        Some("reset") => json!({"status": "error", "code": "unknown_command",
            "message": "Unknown command: reset"}),
        _ => ok(),
    })
    .await;
    let d = driver(port).await;
    let err = d.stop_app("flutter-0", "app").await.unwrap_err();
    assert!(matches!(err, AtalantaError::Config(_)), "{err:?}");
    assert!(err.to_string().contains("atalanta_flutter_helper"), "{err}");
}

// A command the bridge never answers times out, drops the connection, and
// the next command reconnects (the Dart side keeps the app mounted).
#[tokio::test]
async fn a_timed_out_command_reconnects_for_the_next_one() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = hang.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let h = h.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd: Value = serde_json::from_str(&line).unwrap();
                    if cmd["cmd"] == "get_hierarchy" && h.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        continue; // never answers
                    }
                    let mut body = match cmd["cmd"].as_str() {
                        Some("get_hierarchy") => json!({"status": "ok", "data": login_screen()}),
                        _ => ok(),
                    };
                    body["id"] = cmd["id"].clone();
                    let _ = w.write_all(format!("{body}\n").as_bytes()).await;
                }
            });
        }
    });
    let d = FlutterDriver::new(
        Platform::Android,
        FlutterBridgeConfig {
            port,
            reply_timeout_secs: 1,
            ..Default::default()
        },
    );
    d.prepare("flutter-0").await.unwrap();
    hang.store(true, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    let err = d.get_hierarchy("flutter-0").await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        d.health_check().await,
        atalanta_common::HealthStatus::Unhealthy
    );
    hang.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        d.get_hierarchy("flutter-0").await.unwrap().children.len(),
        4
    );
}
