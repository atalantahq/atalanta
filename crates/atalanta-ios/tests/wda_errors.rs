//! What a WDA answer means (P1): garbled bodies, W3C error codes, broken
//! connections and lost sessions, against a fake WDA on 127.0.0.1.
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use atalanta_common::{AtalantaError, ErrorKind, PlatformDriver, Selector};
use atalanta_ios::wda::WdaClient;
use common::*;

async fn client_for(
    handler: impl Fn(&Req) -> Resp + Send + Sync + 'static,
) -> (FakeWda, WdaClient) {
    let fake = FakeWda::start(handler).await;
    let mut c = WdaClient::with_timeout(&fake.url, Duration::from_millis(500));
    c.set_session_id("S".into());
    (fake, c)
}

#[tokio::test]
async fn garbled_source_is_an_error_not_an_empty_screen() {
    // VEL-145: a 200 whose body isn't JSON read as "" (an empty screen).
    let (_f, c) = client_for(|_| Resp::Raw(200, "{not json".into())).await;
    let e = c.get_source().await.unwrap_err();
    assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
    assert!(
        e.to_string().contains("HTTP 200") && e.to_string().contains("{not json"),
        "{e}"
    );
    assert_ne!(e.kind(), ErrorKind::ElementNotFound);
}

#[tokio::test]
async fn a_source_that_is_not_text_is_an_error() {
    let (_f, c) = client_for(|_| ok("{}")).await;
    assert!(c.get_source().await.is_err());
}

#[tokio::test]
async fn http_500_with_a_non_json_body_keeps_status_and_body() {
    // VEL-282: the detail was "Unknown WDA error".
    let (_f, c) = client_for(|_| Resp::Raw(500, "<html>Internal boom</html>".into())).await;
    let e = c.tap_at(1.0, 2.0).await.unwrap_err().to_string();
    assert!(e.contains("500") && e.contains("Internal boom"), "{e}");
}

#[tokio::test]
async fn only_no_such_element_is_element_not_found() {
    // VEL-144: every failed lookup was ElementNotFound.
    let (_f, c) = client_for(|r| {
        if r.body.contains("gone") {
            w3c(404, "no such element", "not there")
        } else if r.body.contains("lost") {
            w3c(404, "invalid session id", "no such session")
        } else {
            Resp::Raw(500, "oops".into())
        }
    })
    .await;
    let gone = c
        .find_element("accessibility id", "gone")
        .await
        .unwrap_err();
    assert!(
        matches!(gone, AtalantaError::ElementNotFound { .. }),
        "{gone:?}"
    );
    let lost = c
        .find_element("accessibility id", "lost")
        .await
        .unwrap_err();
    assert!(
        matches!(lost, AtalantaError::WdaSessionLost { .. }),
        "{lost:?}"
    );
    let broken = c.find_element("accessibility id", "x").await.unwrap_err();
    assert!(
        !matches!(broken, AtalantaError::ElementNotFound { .. }),
        "{broken:?}"
    );
}

#[tokio::test]
async fn a_read_timeout_is_a_lost_connection() {
    // IOS-N3: a hung read was a plain command failure; WDA was never restarted.
    let (_f, c) = client_for(|_| Resp::Hang(Duration::from_secs(3))).await;
    let read = c.get_source().await.unwrap_err();
    assert!(
        matches!(read, AtalantaError::WdaUnreachable { .. }),
        "{read:?}"
    );
    let tap = c.tap_at(1.0, 2.0).await.unwrap_err();
    assert!(matches!(tap, AtalantaError::WdaLost { .. }), "{tap:?}");
    assert_eq!(tap.kind(), ErrorKind::ConnectionLost);
}

#[tokio::test]
async fn a_connection_closed_without_an_answer_is_lost() {
    // VEL-025: what the usbmux forward does when the device's WDA is gone.
    let (_f, c) = client_for(|_| Resp::Close).await;
    let read = c.get_source().await.unwrap_err();
    assert!(
        matches!(read, AtalantaError::WdaUnreachable { .. }),
        "{read:?}"
    );
    let tap = c.tap_at(1.0, 2.0).await.unwrap_err();
    assert!(matches!(tap, AtalantaError::WdaLost { .. }), "{tap:?}");
}

/// A WDA that holds one session; `/status` names it, and a request under
/// any other id gets W3C `invalid session id`.
struct Sessions {
    valid: Mutex<Option<String>>,
    made: Mutex<u32>,
}

fn session_wda(
    s: Arc<Sessions>,
    then: impl Fn(&Req) -> Resp + Send + Sync + 'static,
) -> impl Fn(&Req) -> Resp + Send + Sync + 'static {
    move |r: &Req| {
        let valid = s.valid.lock().unwrap().clone();
        if r.path == "/status" {
            let sid = valid
                .map(|v| format!(r#","sessionId":"{v}""#))
                .unwrap_or_default();
            return Resp::Json(200, format!(r#"{{"value":{{"ready":true}}{sid}}}"#));
        }
        if r.method == "POST" && r.path == "/session" {
            let mut n = s.made.lock().unwrap();
            *n += 1;
            let id = format!("S{}", *n + 1);
            *s.valid.lock().unwrap() = Some(id.clone());
            return ok(&format!(r#"{{"sessionId":"{id}","capabilities":{{}}}}"#));
        }
        if let Some(rest) = r.path.strip_prefix("/session/") {
            let id = rest.split('/').next().unwrap_or("");
            if Some(id) != valid.as_deref() {
                return w3c(404, "invalid session id", "Session does not exist");
            }
        }
        // No system alert. (Not `ATALANTA_IOS_SYSTEM_ALERTS=0`: setting the
        // env while other test threads read it is a data race on macOS.)
        if r.path.ends_with("/alert/text") {
            return w3c(404, "no such alert", "none");
        }
        then(r)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_session_is_re_created_and_the_read_retried() {
    // VEL-143: a session WDA no longer knows was never re-created.
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let s2 = s.clone();
    let reads = Arc::new(Mutex::new(0));
    let fake = FakeWda::start(session_wda(s.clone(), move |r| {
        if r.path.ends_with("/source") {
            let mut n = reads.lock().unwrap();
            *n += 1;
            if *n == 1 {
                // WDA restarted under us: the session is gone.
                *s2.valid.lock().unwrap() = None;
                return w3c(404, "invalid session id", "Session does not exist");
            }
            return source(&app(&node("Button", "Pay", 10, 100, 100, 40)));
        }
        ok("null")
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    let tree = d
        .get_hierarchy(SIM)
        .await
        .expect("read after a new session");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(*s.made.lock().unwrap(), 1, "one new session");
    assert_eq!(fake.count("GET", "/source"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tap_whose_connection_broke_is_not_replayed() {
    // P1/P8: the tap may have landed; it must not be sent twice.
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let fake = FakeWda::start(session_wda(s, |r| {
        if r.path.ends_with("/actions") {
            return Resp::Close;
        }
        if r.path.ends_with("/source") {
            return source(&app(&node("Button", "Pay", 10, 100, 100, 40)));
        }
        ok("null")
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    let el = d
        .find_element(SIM, &Selector::Text("Pay".into()))
        .await
        .unwrap();
    let e = d.tap(SIM, &el).await.unwrap_err();
    assert!(matches!(e, AtalantaError::WdaLost { .. }), "{e:?}");
    assert_eq!(fake.count("POST", "/actions"), 1, "tap sent once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_displayed_error_does_not_drop_the_element() {
    // VEL-336: `/displayed` failing read as "hidden", so the element was
    // dropped and assertNotVisible passed.
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let fake = FakeWda::start(session_wda(s, |r| {
        if r.path.ends_with("/source") {
            return source(&app(""));
        }
        if r.path.ends_with("/elements") {
            return ok(r#"[{"ELEMENT":"E1"}]"#);
        }
        if r.path.ends_with("/displayed") {
            return Resp::Raw(500, "boom".into());
        }
        ok("null")
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    let r = d.find_elements(SIM, &Selector::Id("deep".into())).await;
    assert!(r.is_err(), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_index_or_relation_missing_on_screen_is_not_found_without_asking_wda() {
    // VEL-154: a missing relative target became a config error (exit 2);
    // VEL-155: an index past the on-screen matches returned a WDA element
    // with zero bounds, which then "passed".
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let fake = FakeWda::start(session_wda(s, |r| {
        if r.path.ends_with("/source") {
            return source(&app(&[
                node("StaticText", "Row", 0, 100, 400, 40),
                node("StaticText", "Row", 0, 150, 400, 40),
            ]
            .concat()));
        }
        if r.path.ends_with("/elements") {
            return ok(r#"[{"ELEMENT":"E1"},{"ELEMENT":"E2"},{"ELEMENT":"E3"}]"#);
        }
        if r.path.ends_with("/displayed") {
            return ok("true");
        }
        ok(r#""Row""#)
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    let idx = Selector::Index {
        selector: Box::new(Selector::Text("Row".into())),
        index: 2,
    };
    let e = d.find_element(SIM, &idx).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ElementNotFound, "{e:?}");
    let rel = Selector::Relative {
        target: Box::new(Selector::Text("Edit".into())),
        relation: atalanta_common::Relation::Below,
        anchor: Box::new(Selector::Text("Row".into())),
    };
    let e = d.find_element(SIM, &rel).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ElementNotFound, "{e:?}");
    assert_eq!(fake.count("POST", "/elements"), 0, "no WDA fallback");
}

#[tokio::test]
async fn a_slow_read_on_a_live_wda_is_busy_not_unreachable() {
    // Review M1: a read that timed out was WdaUnreachable (ConnectionLost),
    // so the driver re-created the session and relaunched the app, and the
    // resilience layer did it again up to three times.
    // Its main thread (`/wda/screen`) still answers: busy, not dead.
    let (_f, c) = client_for(|r| {
        if r.path == "/status" {
            return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
        }
        if r.path == "/wda/screen" {
            return ok(r#"{"statusBarSize":{"width":402,"height":62}}"#);
        }
        Resp::Hang(Duration::from_secs(2))
    })
    .await;
    let e = c.get_source().await.unwrap_err();
    assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
    assert!(e.to_string().contains("busy"), "{e}");
    assert_ne!(e.kind(), ErrorKind::ConnectionLost);
}

/// A runner whose `/status` answers (AtalantaRunner serves it off the main
/// thread) while every main-thread route hangs.
fn wedged(r: &Req) -> Resp {
    if r.path == "/status" {
        return Resp::Json(200, r#"{"value":{"ready":true}}"#.into());
    }
    Resp::Hang(Duration::from_secs(5))
}

#[tokio::test]
async fn a_wedged_runner_is_unreachable_not_busy() {
    // Review MAJOR-1: `/status` answering made a hung runner "busy"
    // forever, so it was never restarted.
    let (_f, c) = client_for(wedged).await;
    let e = c.get_source().await.unwrap_err();
    assert!(matches!(e, AtalantaError::WdaUnreachable { .. }), "{e:?}");
}

#[tokio::test]
async fn busy_is_capped_then_unreachable() {
    // Review MAJOR-1 (b): even with the main thread answering, reads that
    // keep timing out end in WdaUnreachable so the driver recovers.
    let (_f, c) = client_for(|r| {
        if r.path == "/wda/screen" || r.path == "/status" {
            return ok("{}");
        }
        Resp::Hang(Duration::from_secs(2))
    })
    .await;
    for _ in 0..2 {
        let e = c.get_source().await.unwrap_err();
        assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
    }
    let e = c.get_source().await.unwrap_err();
    assert!(matches!(e, AtalantaError::WdaUnreachable { .. }), "{e:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_runner_whose_session_cannot_be_renewed_is_restarted() {
    // Review MAJOR-1 (c): recover() saw `/status` alive, failed to renew
    // the session through the wedged main thread, and returned false
    // without trying the restart branch. Here the second session request
    // (the restart's) un-wedges the fake.
    let healed = Arc::new(Mutex::new(false));
    let posts = Arc::new(Mutex::new(0));
    let h = healed.clone();
    let fake = FakeWda::start(move |r| {
        if *h.lock().unwrap() {
            if r.method == "POST" && r.path == "/session" {
                return ok(r#"{"sessionId":"S2","capabilities":{}}"#);
            }
            if r.path.ends_with("/source") {
                return source(&app(&node("Button", "Pay", 10, 100, 100, 40)));
            }
            if r.path.ends_with("/alert/text") {
                return w3c(404, "no such alert", "none");
            }
            return ok("null");
        }
        if r.path == "/status" {
            return Resp::Json(200, r#"{"value":{"ready":true},"sessionId":"S1"}"#.into());
        }
        if r.method == "POST" && r.path == "/session" {
            let mut n = posts.lock().unwrap();
            *n += 1;
            if *n >= 2 {
                *h.lock().unwrap() = true;
                return ok(r#"{"sessionId":"S2","capabilities":{}}"#);
            }
        }
        Resp::Hang(Duration::from_secs(5))
    })
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url)
        .with_wda_timeout(Duration::from_millis(500));
    let tree = d
        .get_hierarchy(SIM)
        .await
        .expect("read after the runner was restarted");
    assert_eq!(tree.children.len(), 1);
    assert!(*healed.lock().unwrap());
}

#[tokio::test]
async fn an_element_gone_while_checking_is_not_displayed_not_an_error() {
    // Review minor 1: a stale element mid-wait failed the whole wait.
    let (_f, c) = client_for(|r| {
        if r.path.ends_with("/E1/displayed") {
            return w3c(404, "stale element reference", "gone");
        }
        if r.path.ends_with("/E2/displayed") {
            return w3c(404, "no such element", "gone");
        }
        Resp::Raw(500, "boom".into())
    })
    .await;
    assert!(!c.is_displayed("E1").await.unwrap());
    assert!(!c.is_displayed("E2").await.unwrap());
    assert!(c.is_displayed("E3").await.is_err(), "VEL-336 still holds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_taken_by_another_client_is_not_adopted() {
    // Review minor 5: renewal adopted the session `/status` named, which
    // was the other client's; each renewal then invalidated the other's.
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let s2 = s.clone();
    let reads = Arc::new(Mutex::new(0));
    let fake = FakeWda::start(session_wda(s.clone(), move |r| {
        if r.path.contains("alert") {
            return w3c(404, "no such alert", "none");
        }
        if r.path.ends_with("/source") {
            let mut n = reads.lock().unwrap();
            *n += 1;
            if *n == 1 {
                *s2.valid.lock().unwrap() = Some("OTHER".into());
                return w3c(404, "invalid session id", "Session does not exist");
            }
            return source(&app(&node("Button", "Pay", 10, 100, 100, 40)));
        }
        ok("null")
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    d.get_hierarchy(SIM).await.expect("read in a new session");
    assert_eq!(*s.made.lock().unwrap(), 1, "a fresh session, not OTHER");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_text_retried_in_a_new_session_does_not_tap_again() {
    // VEL-296 / review minor 4: the tap and the typing were one replayed
    // closure, so a session lost while typing tapped the field twice.
    let s = Arc::new(Sessions {
        valid: Mutex::new(Some("S1".into())),
        made: Mutex::new(0),
    });
    let s2 = s.clone();
    let typed = Arc::new(Mutex::new(0));
    let fake = FakeWda::start(session_wda(s.clone(), move |r| {
        if r.path.contains("alert") {
            return w3c(404, "no such alert", "none");
        }
        if r.path.ends_with("/source") {
            return source(&app(&node("TextField", "Email", 20, 100, 362, 44)));
        }
        if r.path.ends_with("/wda/keys") {
            let mut n = typed.lock().unwrap();
            *n += 1;
            if *n == 1 {
                *s2.valid.lock().unwrap() = None;
                return w3c(404, "invalid session id", "Session does not exist");
            }
        }
        ok("null")
    }))
    .await;
    let d = atalanta_ios::IosDriver::with_wda_url(&fake.url);
    let field = d
        .find_element(SIM, &Selector::Text("Email".into()))
        .await
        .unwrap();
    d.input_text(SIM, &field, "hi").await.unwrap();
    assert_eq!(fake.count("POST", "/actions"), 1, "tapped once");
    assert_eq!(fake.count("POST", "/wda/keys"), 2, "typing retried");
}

#[tokio::test]
async fn a_session_answer_without_an_id_is_not_a_config_error() {
    // VEL-146: a bad WDA payload exited 2 as a config error.
    let (_f, mut c) = client_for(|_| ok("{}")).await;
    let e = c.create_session("").await.unwrap_err();
    assert_ne!(e.kind(), ErrorKind::ConfigError, "{e:?}");
}

#[tokio::test]
async fn an_xctest_focus_issue_from_the_runner_is_an_assertion_failure() {
    // P1 / N1: AtalantaRunner now answers 500 with the XCTest issues an
    // action recorded. Typing with nothing focused fails the step at once,
    // rather than being retried as a flaky command (or passing as a 200).
    let (_f, c) = client_for(|_| {
        w3c(
            500,
            "unknown error",
            "Failed to synthesize event: Neither element nor any descendant has keyboard focus.",
        )
    })
    .await;
    let e = c.type_text("abc").await.unwrap_err();
    assert!(matches!(e, AtalantaError::AssertionFailed { .. }), "{e:?}");
    assert!(e.to_string().contains("keyboard focus"), "{e}");
    assert_eq!(e.kind(), ErrorKind::AssertionFailed);
    // Any other recorded issue stays a plain command failure.
    let (_f, c) = client_for(|_| w3c(500, "unknown error", "Failed to tap: boom")).await;
    let e = c.tap_at(1.0, 2.0).await.unwrap_err();
    assert!(matches!(e, AtalantaError::CommandFailed { .. }), "{e:?}");
}
