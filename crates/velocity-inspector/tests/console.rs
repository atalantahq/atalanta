//! The console's routes over real HTTP and WebSockets on 127.0.0.1: auth on
//! every new route and upgrade, path confinement, device leases, the H.264
//! mirror framing from a fake video stream, the frames fallback, suite runs
//! with live events, and the explore seam.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use velocity_android::video::VideoStream;
use velocity_common::types::{DeviceInfo, Direction, Element, Key, Rect, Selector};
use velocity_common::{PlatformDriver, Result};
use velocity_inspector::console::diagnostics::DiagResult;
use velocity_inspector::console::tasks::{
    ExploreChild, ExploreFiles, ExploreLauncher, ExploreSpec, RunRequest, SuiteLauncher,
};
use velocity_inspector::console::{
    ConsoleHooks, Diagnostics, LeaseFail, Leases, LiveSource, NoDiagnostics, NoLive,
};
use velocity_inspector::InspectorServer;

// ── A fake device ───────────────────────────────────────────────────────

fn png() -> Vec<u8> {
    let mut p = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    p.extend_from_slice(&100u32.to_be_bytes());
    p.extend_from_slice(&200u32.to_be_bytes());
    p.extend_from_slice(b"rest");
    p
}

struct Fake {
    taps: std::sync::Mutex<Vec<(i32, i32)>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            taps: Default::default(),
        })
    }
}

fn el() -> Element {
    Element {
        text: Some("Pay".into()),
        element_type: "Button".into(),
        bounds: Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        },
        enabled: true,
        visible: true,
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl PlatformDriver for Fake {
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
        Ok(el())
    }
    async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
        Ok(vec![el()])
    }
    async fn get_hierarchy(&self, _: &str) -> Result<Element> {
        Ok(el())
    }
    async fn tap(&self, _: &str, _: &Element) -> Result<()> {
        Ok(())
    }
    async fn tap_xy(&self, _: &str, x: i32, y: i32) -> Result<()> {
        self.taps.lock().unwrap().push((x, y));
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
        Ok(png())
    }
    async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
        Ok((100, 200))
    }
    async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
        Ok(String::new())
    }
    async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
        Ok(true)
    }
}

/// `busy-dev` is held by another process; everything else is free.
struct FakeLeases;
impl Leases for FakeLeases {
    fn try_lock(
        &self,
        id: &str,
    ) -> std::result::Result<Box<dyn std::any::Any + Send + Sync>, LeaseFail> {
        if id == "busy-dev" {
            Err(LeaseFail::Held("pid=4242 cmd=run".into()))
        } else {
            Ok(Box::new(()))
        }
    }
}

/// A suite run that is a shell script: it writes the live events, prints
/// a line, and leaves a report and a trace (no new executable is written,
/// so the host's scanner never holds it).
struct ShellLauncher;
impl SuiteLauncher for ShellLauncher {
    fn command(
        &self,
        suite: &Path,
        req: &RunRequest,
        out_dir: &Path,
        events_file: &Path,
    ) -> tokio::process::Command {
        let script = r#"
E="$VELOCITY_EVENTS_FILE"
echo '{"event":"testStarted","test":"login","device":"'"$3"'"}' >> "$E"
echo '{"event":"stepStarted","test":"login","index":0,"step":"launchApp"}' >> "$E"
printf '\033[32m✓\033[0m running %s\n' "$1"
sleep 0.3
echo '{"event":"stepFinished","test":"login","index":0,"step":"launchApp","status":"passed","ms":12}' >> "$E"
echo '{"event":"testFinished","test":"login","status":"passed","ms":20}' >> "$E"
mkdir -p "$2/traces/login"
echo '<html>trace</html>' > "$2/traces/login/trace.html"
printf '0123456789' > "$2/traces/login/video.mp4"
echo '{"total":1,"passed":1,"failed":0,"skipped":0}' > "$2/results.json"
exit 0
"#;
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c")
            .arg(script)
            .arg("sh")
            .arg(suite)
            .arg(out_dir)
            .arg(&req.device_id)
            .env("VELOCITY_EVENTS_FILE", events_file);
        c
    }
}

/// `velocity explore --json` as a shell script: an observed and a planned
/// turn, waits for an injected line (echoed as an `injected` event), a
/// replay's test and step events, then the YAML and the JSON result. Its
/// argv goes to `argv.txt` for the test to check.
struct ShellExplore;
impl ExploreLauncher for ShellExplore {
    fn command(&self, spec: &ExploreSpec, files: &ExploreFiles) -> tokio::process::Command {
        let script = r#"
E="$VELOCITY_EVENTS_FILE"
echo "$@" > "$(dirname "$1")/argv.txt"
echo '{"event":"observed","atMs":5,"turn":1,"title":"Settings","elements":20}' >> "$E"
echo '{"event":"planned","atMs":9,"turn":1,"op":"tap","element":3,"target":"Network & internet","reason":"open it"}' >> "$E"
i=0
while [ $i -lt 100 ] && ! grep -q . "$2"; do sleep 0.1; i=$((i+1)); done
echo "{\"event\":\"injected\",\"turn\":2,\"instruction\":\"$(head -n 1 "$2")\"}" >> "$E"
echo '{"event":"testStarted","test":"open network","steps":2}' >> "$E"
echo '{"event":"stepStarted","test":"open network","index":0,"total":2,"step":"launchApp"}' >> "$E"
echo '{"event":"stepFinished","test":"open network","index":0,"total":2,"step":"launchApp","status":"passed","ms":800}' >> "$E"
echo '{"event":"replayRun","run":1,"passed":true,"ms":1300}' >> "$E"
echo '{"event":"finished","outcome":"success","turns":2}' >> "$E"
printf 'appId: com.android.settings\n' > "$1"
printf '{"report":{"outcome":"success","reason":"the page is open","turns":2,"steps":[{}],"modelCalls":0,"inputTokens":0,"outputTokens":0},"yaml":"appId: com.android.settings\\ntests: []\\n","replay":{"runs":[{"passed":true,"ms":1300,"steps":[]}],"modelCalls":0},"repair":null,"files":["%s"]}' "$1"
"#;
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c")
            .arg(script)
            .arg("sh")
            .arg(&files.out)
            .arg(&files.inject)
            .arg(&spec.goal)
            .arg(&spec.app_id)
            .arg(spec.script.as_deref().unwrap_or(Path::new("-")))
            .env("VELOCITY_EVENTS_FILE", &files.events);
        c
    }
}

/// A doctor that always reports one warning.
struct FakeDoctor;
#[async_trait::async_trait]
impl Diagnostics for FakeDoctor {
    async fn run(&self) -> DiagResult {
        DiagResult::Report(serde_json::json!({
            "verdict": "degraded",
            "checks": [{"id": "android.adb", "area": "android", "status": "warn",
                        "summary": "adb 35 found", "fix": {"kind": "run", "value": "adb start-server"}}]
        }))
    }
}

/// A fake Android video stream: whatever the test writes to the device
/// side of a pipe.
struct FakeVideo {
    stream: Arc<VideoStream>,
}
#[async_trait::async_trait]
impl LiveSource for FakeVideo {
    async fn h264(&self, _: &str) -> Option<std::result::Result<Arc<VideoStream>, String>> {
        Some(Ok(self.stream.clone()))
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "vel-console-{tag}-{}-{}",
        std::process::id(),
        velocity_inspector::security::new_token()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Srv {
    port: u16,
    token: String,
    project: PathBuf,
    fake: Arc<Fake>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

async fn start_with(live: Arc<dyn LiveSource>, explore: bool) -> Srv {
    let project = tmp_dir("project");
    std::fs::write(
        project.join("velocity.yaml"),
        "appId: a\ntests:\n  - name: login\n    steps:\n      - launchApp\n",
    )
    .unwrap();
    std::fs::write(project.join("plan.json"), "{\"steps\":[]}").unwrap();
    let listener = InspectorServer::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fake = Fake::new();
    let hooks = ConsoleHooks {
        live,
        leases: Arc::new(FakeLeases),
        diagnostics: if explore {
            Arc::new(FakeDoctor)
        } else {
            Arc::new(NoDiagnostics)
        },
        explore: if explore {
            Arc::new(ExploreChild {
                launcher: Arc::new(ShellExplore),
            })
        } else {
            Arc::new(velocity_inspector::console::ExploreUnavailable)
        },
        launcher: Arc::new(ShellLauncher),
        runs_dir: project.join("velocity-results/console"),
        project_dir: project.clone(),
    };
    let srv = InspectorServer::new(fake.clone(), Some("dev".into()), None).with_console(hooks);
    let token = srv.token().to_string();
    let task = tokio::spawn(async move {
        let _ = srv.start_with_listener(listener).await;
    });
    Srv {
        port,
        token,
        project,
        fake,
        task,
    }
}

async fn start() -> Srv {
    start_with(Arc::new(NoLive), false).await
}

struct Resp {
    code: u16,
    head: String,
    body: String,
}

async fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await;
    let txt = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = txt.split_once("\r\n\r\n").unwrap_or((&txt, ""));
    Resp {
        code: head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0),
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

impl Srv {
    async fn api(&self, method: &str, path: &str, body: &str) -> Resp {
        let host = format!("127.0.0.1:{}", self.port);
        send(
            self.port,
            method,
            path,
            &[("Host", &host), ("X-Velocity-Token", &self.token)],
            body,
        )
        .await
    }

    async fn ws(&self, path: &str) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
        let url = format!(
            "ws://127.0.0.1:{}{path}{}token={}",
            self.port,
            if path.contains('?') { "&" } else { "?" },
            self.token
        );
        let req = url.into_client_request().unwrap();
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        tokio_tungstenite::client_async(req, tcp).await.unwrap().0
    }
}

async fn next_msg(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Message {
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a message in time")
        .expect("socket open")
        .expect("no socket error")
}

async fn next_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> serde_json::Value {
    match next_msg(ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text, got {other:?}"),
    }
}

async fn next_bin(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Vec<u8> {
    match next_msg(ws).await {
        Message::Binary(b) => b.to_vec(),
        other => panic!("expected binary, got {other:?}"),
    }
}

/// Every route the console added, GET and POST.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/devices/current"),
    ("GET", "/api/devices/dev/control"),
    ("POST", "/api/devices/dev/control"),
    ("DELETE", "/api/devices/dev/control"),
    ("POST", "/api/devices/dev/input"),
    ("GET", "/api/project/suites"),
    ("GET", "/api/diagnostics"),
    ("GET", "/api/tasks"),
    ("POST", "/api/tasks/run"),
    ("POST", "/api/tasks/explore"),
    ("GET", "/api/tasks/1"),
    ("POST", "/api/tasks/1/inject"),
    ("POST", "/api/tasks/1/cancel"),
    ("POST", "/api/tasks/1/save"),
    ("GET", "/api/tasks/1/files"),
    ("GET", "/api/tasks/1/files/trace.html"),
    ("GET", "/api/mirror/dev"),
    ("GET", "/api/tasks/1/events"),
];

#[tokio::test]
async fn every_console_route_and_upgrade_needs_the_token_host_and_origin() {
    let s = start().await;
    let host = format!("127.0.0.1:{}", s.port);
    let upgrade = [
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];
    for (method, path) in ROUTES {
        let mut base = vec![("Host", host.as_str())];
        if path.contains("mirror") || path.ends_with("events") {
            base.extend_from_slice(&upgrade);
        }
        // No token.
        let r = send(s.port, method, path, &base, "{}").await;
        assert_eq!(r.code, 401, "{method} {path} without a token: {}", r.body);
        // A foreign page, even with the token.
        let mut h = base.clone();
        h.push(("X-Velocity-Token", &s.token));
        h.push(("Origin", "https://evil.example"));
        let r = send(s.port, method, path, &h, "{}").await;
        assert_eq!(r.code, 403, "{method} {path} from another origin");
        assert!(
            !r.head.contains("access-control-allow-origin"),
            "{}",
            r.head
        );
        // Cross-site fetch metadata without an Origin.
        let mut h = base.clone();
        h.push(("X-Velocity-Token", &s.token));
        h.push(("Sec-Fetch-Site", "cross-site"));
        let r = send(s.port, method, path, &h, "{}").await;
        assert_eq!(r.code, 403, "{method} {path} cross-site");
        // DNS rebinding: a foreign Host.
        let mut h: Vec<(&str, &str)> = base.iter().filter(|(k, _)| *k != "Host").cloned().collect();
        h.push(("Host", "evil.example"));
        h.push(("X-Velocity-Token", &s.token));
        let r = send(s.port, method, path, &h, "{}").await;
        assert_eq!(r.code, 403, "{method} {path} with a foreign Host");
    }
    // Nothing was driven by any of that.
    assert!(s.fake.taps.lock().unwrap().is_empty());
}

#[tokio::test]
async fn input_takes_the_lease_and_a_busy_device_says_who_has_it() {
    let s = start().await;
    // A free device: the first input takes the lease.
    let r = s.api("GET", "/api/devices/dev/control", "").await;
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["mine"], false);
    assert_eq!(v["screen"]["width"], 100);
    let r = s
        .api(
            "POST",
            "/api/devices/dev/input",
            r#"{"kind":"tap","x":40,"y":80}"#,
        )
        .await;
    assert_eq!(r.code, 200, "{}", r.body);
    assert_eq!(*s.fake.taps.lock().unwrap(), vec![(40, 80)]);
    let v: serde_json::Value =
        serde_json::from_str(&s.api("GET", "/api/devices/dev/control", "").await.body).unwrap();
    assert_eq!(v["mine"], true);
    let r = s.api("DELETE", "/api/devices/dev/control", "").await;
    assert!(r.body.contains("\"released\":true"), "{}", r.body);

    // Held by another process: input and the toolbar's actions are 409
    // with the holder; watching still works.
    for (path, body) in [
        (
            "/api/devices/busy-dev/input",
            r#"{"kind":"key","key":"back"}"#,
        ),
        ("/api/devices/busy-dev/tap", r#"{"coordinates":[1,2]}"#),
        ("/api/devices/busy-dev/press-key", r#"{"key":"home"}"#),
        ("/api/devices/busy-dev/control", ""),
    ] {
        let r = s.api("POST", path, body).await;
        assert_eq!(r.code, 409, "{path}: {}", r.body);
        assert!(
            r.body.contains("busy: pid=4242 cmd=run"),
            "{path}: {}",
            r.body
        );
    }
    let v: serde_json::Value =
        serde_json::from_str(&s.api("GET", "/api/devices/busy-dev/control", "").await.body)
            .unwrap();
    assert_eq!(v["busy"], "pid=4242 cmd=run");
    let r = s.api("GET", "/api/devices/busy-dev/hierarchy", "").await;
    assert_eq!(r.code, 200);
    // Bad input is a 4xx, not a device call.
    let r = s
        .api("POST", "/api/devices/dev/input", r#"{"kind":"warp"}"#)
        .await;
    assert!(r.code == 400 || r.code == 422, "{}", r.code);
}

#[tokio::test]
async fn the_mirror_frames_h264_from_the_video_stream() {
    // The device side of the stream: we write packets, and read the 'K'
    // (key frame request) a subscriber sends.
    let (host_read, mut dev_write) = tokio::io::duplex(1 << 20);
    let (mut dev_read, host_write) = tokio::io::duplex(1024);
    let stream = Arc::new(VideoStream::from_parts(
        "emulator-5554",
        host_read,
        host_write,
        Duration::from_secs(5),
        None,
        None,
        "adb",
    ));
    let s = start_with(Arc::new(FakeVideo { stream }), false).await;
    let mut ws = s.ws("/api/mirror/emulator-5554").await;
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["mode"], "h264");
    // Subscribing asked the encoder for a key frame.
    let mut k = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), dev_read.read_exact(&mut k))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&k, b"K");

    let session = |w: u32, h: u32| {
        let mut b = (1u32 << 31).to_be_bytes().to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b
    };
    let media = |flags: u64, pts: u64, data: &[u8]| {
        let mut b = (flags | pts).to_be_bytes().to_vec();
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    };
    let sps = [0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE];
    let mut wire = session(584, 1280);
    wire.extend(media(1 << 62, 0, &sps));
    wire.extend(media(0, 1000, &[0, 0, 0, 1, 0x41, 1])); // delta before any key: dropped
    wire.extend(media(1 << 61, 2000, &[0, 0, 0, 1, 0x65, 2])); // key
    wire.extend(media(0, 3000, &[0, 0, 0, 1, 0x41, 3])); // delta
    dev_write.write_all(&wire).await.unwrap();

    let v = next_json(&mut ws).await;
    assert_eq!(
        (v["type"].as_str(), v["width"].as_u64()),
        (Some("session"), Some(584))
    );
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "config");
    assert_eq!(v["codec"], "avc1.42c01f");
    let b = next_bin(&mut ws).await;
    assert_eq!(b[0], 1, "config kind");
    assert_eq!(&b[18..], &sps);
    let b = next_bin(&mut ws).await;
    assert_eq!((b[0], b[1]), (2, 1), "the key frame comes first");
    assert_eq!(u64::from_be_bytes(b[2..10].try_into().unwrap()), 2000);
    let recv_us = u64::from_be_bytes(b[10..18].try_into().unwrap());
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    assert!(
        now_us >= recv_us && now_us - recv_us < 5_000_000,
        "{recv_us} vs {now_us}"
    );
    assert_eq!(&b[18..], &[0, 0, 0, 1, 0x65, 2]);
    let b = next_bin(&mut ws).await;
    assert_eq!((b[0], b[1]), (2, 0));
    assert_eq!(u64::from_be_bytes(b[2..10].try_into().unwrap()), 3000);

    // A second viewer shares the stream (and asks for its own key frame).
    let mut ws2 = s.ws("/api/mirror/emulator-5554").await;
    assert_eq!(next_json(&mut ws2).await["mode"], "h264");
    // It starts from the parameter sets the stream already sent.
    assert_eq!(next_json(&mut ws2).await["type"], "session");
    assert_eq!(next_json(&mut ws2).await["type"], "config");
    assert_eq!(next_bin(&mut ws2).await[0], 1);
    // And the picture on screen now, from the ring buffer: the last key
    // frame and what followed it (a still screen sends nothing new).
    let b = next_bin(&mut ws2).await;
    assert_eq!((b[0], b[1]), (2, 1));
    assert_eq!(u64::from_be_bytes(b[2..10].try_into().unwrap()), 2000);
    assert_eq!(
        u64::from_be_bytes(b[10..18].try_into().unwrap()),
        0,
        "replayed: no latency"
    );
    let b = next_bin(&mut ws2).await;
    assert_eq!(u64::from_be_bytes(b[2..10].try_into().unwrap()), 3000);

    // Paused (tab hidden): nothing; resumed: deltas wait for a key frame.
    ws.send(Message::Text(r#"{"type":"pause"}"#.into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    dev_write
        .write_all(&media(0, 4000, &[0, 0, 0, 1, 0x41, 4]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    ws.send(Message::Text(r#"{"type":"resume"}"#.into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    dev_write
        .write_all(&media(0, 5000, &[0, 0, 0, 1, 0x41, 5]))
        .await
        .unwrap();
    dev_write
        .write_all(&media(1 << 61, 6000, &[0, 0, 0, 1, 0x65, 6]))
        .await
        .unwrap();
    let b = next_bin(&mut ws).await;
    assert_eq!(
        u64::from_be_bytes(b[2..10].try_into().unwrap()),
        6000,
        "paused and pre-key deltas are skipped"
    );

    // The stream ends: the mirror falls back to frames on the same socket.
    drop(dev_write);
    let v = next_json(&mut ws).await;
    assert_eq!(v["type"], "fallback", "{v}");
    let v = next_json(&mut ws).await;
    assert_eq!(
        (v["type"].as_str(), v["mode"].as_str()),
        (Some("hello"), Some("frames"))
    );
}

#[tokio::test]
async fn without_video_the_mirror_pushes_changed_screenshots() {
    let s = start().await;
    let mut ws = s.ws("/api/mirror/dev").await;
    let hello = next_json(&mut ws).await;
    assert_eq!(
        (hello["mode"].as_str(), hello["source"].as_str()),
        (Some("frames"), Some("screenshots"))
    );
    let v = next_json(&mut ws).await;
    assert_eq!(
        (v["type"].as_str(), v["width"].as_u64()),
        (Some("session"), Some(100))
    );
    let b = next_bin(&mut ws).await;
    assert_eq!(b[0], 4, "a PNG");
    assert_eq!(&b[18..], &png()[..]);
    // The same screen again is not resent.
    let again = tokio::time::timeout(Duration::from_millis(600), ws.next()).await;
    assert!(again.is_err(), "unchanged screen resent: {again:?}");
    // `?mode=frames` (no WebCodecs) never offers H.264.
    let mut ws2 = s.ws("/api/mirror/dev?mode=frames").await;
    assert_eq!(next_json(&mut ws2).await["mode"], "frames");
}

#[tokio::test]
async fn suites_run_with_live_steps_and_confined_artifacts() {
    let s = start().await;
    let r = s.api("GET", "/api/project/suites", "").await;
    assert!(r.body.contains("\"velocity.yaml\""), "{}", r.body);
    assert!(r.body.contains("\"login\""), "{}", r.body);

    // Outside the project, or not a suite: refused.
    let outside = tmp_dir("outside");
    std::fs::write(outside.join("x.yaml"), "tests:\n").unwrap();
    for suite in [
        "../x.yaml".to_string(),
        outside.join("x.yaml").display().to_string(),
        "missing.yaml".into(),
        "".into(),
    ] {
        let body = serde_json::json!({"suite": suite, "deviceId": "dev"}).to_string();
        let r = s.api("POST", "/api/tasks/run", &body).await;
        assert_eq!(r.code, 400, "{suite}: {}", r.body);
    }
    let r = s
        .api(
            "POST",
            "/api/tasks/run",
            r#"{"suite":"velocity.yaml","deviceId":"--help"}"#,
        )
        .await;
    assert_eq!(r.code, 400);

    let r = s
        .api(
            "POST",
            "/api/tasks/run",
            r#"{"suite":"velocity.yaml","deviceId":"dev"}"#,
        )
        .await;
    assert_eq!(r.code, 202, "{}", r.body);
    let id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["id"]
        .as_u64()
        .unwrap();

    // While it runs, the device is the task's.
    let mut ws = s.ws(&format!("/api/tasks/{id}/events")).await;
    let mut kinds = Vec::new();
    let mut saw_busy = false;
    loop {
        let m = tokio::time::timeout(Duration::from_secs(20), ws.next())
            .await
            .unwrap();
        let Some(Ok(Message::Text(t))) = m else { break };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        let kind = v["kind"].as_str().unwrap().to_string();
        if kind == "stepStarted" && !saw_busy {
            let r = s
                .api(
                    "POST",
                    "/api/devices/dev/input",
                    r#"{"kind":"tap","x":1,"y":1}"#,
                )
                .await;
            assert_eq!(r.code, 409, "{}", r.body);
            assert!(r.body.contains("console task"), "{}", r.body);
            saw_busy = true;
        }
        if kind == "log" {
            assert!(!t.contains("\\u001b"), "colours are stripped: {t}");
        }
        kinds.push((kind, v["data"].clone()));
    }
    let names: Vec<&str> = kinds.iter().map(|(k, _)| k.as_str()).collect();
    for want in [
        "testStarted",
        "stepStarted",
        "stepFinished",
        "testFinished",
        "summary",
        "log",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    let last = kinds.last().unwrap();
    assert_eq!(last.0, "status");
    assert_eq!(last.1["status"], "passed", "{:?}", last.1);
    assert!(saw_busy);

    // The device is free again.
    let r = s
        .api(
            "POST",
            "/api/devices/dev/input",
            r#"{"kind":"tap","x":1,"y":1}"#,
        )
        .await;
    assert_eq!(r.code, 200, "{}", r.body);

    // Artifacts: listed, served with a CSP, ranges for video seeking, and
    // confined to the run's folder.
    let r = s.api("GET", &format!("/api/tasks/{id}/files"), "").await;
    assert!(r.body.contains("traces/login/trace.html"), "{}", r.body);
    let r = s
        .api(
            "GET",
            &format!("/api/tasks/{id}/files/traces/login/trace.html"),
            "",
        )
        .await;
    assert_eq!(r.code, 200);
    assert!(r.head.contains("content-type: text/html"), "{}", r.head);
    assert!(
        r.head
            .contains("content-security-policy: default-src 'none'"),
        "{}",
        r.head
    );
    assert!(r.head.contains("connect-src 'none'"), "{}", r.head);
    assert!(r.head.contains("x-frame-options: deny"), "{}", r.head);
    let host = format!("127.0.0.1:{}", s.port);
    let r = send(
        s.port,
        "GET",
        &format!("/api/tasks/{id}/files/traces/login/video.mp4"),
        &[
            ("Host", &host),
            ("X-Velocity-Token", &s.token),
            ("Range", "bytes=2-4"),
        ],
        "",
    )
    .await;
    assert_eq!(r.code, 206, "{}", r.head);
    assert_eq!(r.body, "234");
    assert!(r.head.contains("content-range: bytes 2-4/10"), "{}", r.head);

    // Traversal and symlinks out of the folder are refused.
    let run_dir = std::fs::read_dir(s.project.join("velocity-results/console"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    std::fs::write(outside.join("secret.txt"), "SECRET").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, run_dir.join("link")).unwrap();
    for path in [
        "../velocity.yaml",
        "..%2F..%2Fvelocity.yaml",
        "%2Fetc%2Fpasswd",
        "link/secret.txt",
        "traces/../../../velocity.yaml",
    ] {
        let r = s
            .api("GET", &format!("/api/tasks/{id}/files/{path}"), "")
            .await;
        assert_eq!(r.code, 404, "{path}: {}", r.body);
        assert!(
            !r.body.contains("SECRET") && !r.body.contains("appId"),
            "{path}"
        );
    }
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn without_explore_or_doctor_the_routes_say_so() {
    let s = start().await;
    let r = s.api("GET", "/api/tasks", "").await;
    assert!(r.body.contains("\"exploreAvailable\":false"), "{}", r.body);
    let r = s
        .api(
            "POST",
            "/api/tasks/explore",
            r#"{"goal":"add a card","deviceId":"dev","appId":"a"}"#,
        )
        .await;
    assert_eq!(r.code, 501, "{}", r.body);
    assert!(r.body.contains("not available in this build"), "{}", r.body);
    let r = s.api("GET", "/api/diagnostics", "").await;
    assert_eq!(r.code, 501, "{}", r.body);
    assert!(r.body.contains("\"available\":false"), "{}", r.body);
}

#[tokio::test]
async fn diagnostics_pass_the_doctor_report_through() {
    let s = start_with(Arc::new(NoLive), true).await;
    let r = s.api("GET", "/api/diagnostics", "").await;
    assert_eq!(r.code, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["available"], true);
    assert_eq!(v["report"]["verdict"], "degraded");
    assert_eq!(v["report"]["checks"][0]["summary"], "adb 35 found");
}

#[tokio::test]
async fn explore_runs_a_child_streams_its_events_and_saves_the_test() {
    let s = start_with(Arc::new(NoLive), true).await;
    let r = s.api("GET", "/api/tasks", "").await;
    assert!(r.body.contains("\"exploreAvailable\":true"), "{}", r.body);
    // Refused: no app, a plan outside the project, a flag for a value.
    for (body, want) in [
        (r#"{"goal":"g","deviceId":"dev"}"#, "which app"),
        (
            r#"{"goal":"g","deviceId":"dev","appId":"a","script":"../plan.json"}"#,
            "not a plan file",
        ),
        (
            r#"{"goal":"g","deviceId":"dev","appId":"--help"}"#,
            "may not start",
        ),
        (
            r#"{"goal":"g","deviceId":"dev","appId":"a","script":"plan.json","model":"x:y"}"#,
            "not both",
        ),
    ] {
        let r = s.api("POST", "/api/tasks/explore", body).await;
        assert_eq!(r.code, 400, "{body}: {}", r.body);
        assert!(r.body.contains(want), "{body}: {}", r.body);
    }
    let r = s
        .api(
            "POST",
            "/api/tasks/explore",
            r#"{"goal":"open Network & internet","deviceId":"dev","appId":"com.android.settings","script":"plan.json"}"#,
        )
        .await;
    assert_eq!(r.code, 202, "{}", r.body);
    let id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["id"]
        .as_u64()
        .unwrap();
    // Saving before it has finished is refused.
    let r = s
        .api(
            "POST",
            &format!("/api/tasks/{id}/save"),
            r#"{"path":"flows/early.yaml"}"#,
        )
        .await;
    assert_eq!(r.code, 409, "{}", r.body);
    let r = s
        .api(
            "POST",
            &format!("/api/tasks/{id}/inject"),
            r#"{"text":"use search"}"#,
        )
        .await;
    assert_eq!(r.code, 200, "{}", r.body);
    let mut ws = s.ws(&format!("/api/tasks/{id}/events")).await;
    let mut explore = Vec::new();
    let mut kinds = Vec::new();
    let mut result = serde_json::Value::Null;
    let mut last_status = serde_json::Value::Null;
    while let Ok(Some(Ok(Message::Text(t)))) =
        tokio::time::timeout(Duration::from_secs(20), ws.next()).await
    {
        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
        let kind = v["kind"].as_str().unwrap().to_string();
        match kind.as_str() {
            "explore" => explore.push(v["data"].clone()),
            "result" => result = v["data"].clone(),
            "status" => last_status = v["data"].clone(),
            _ => {}
        }
        kinds.push(kind);
    }
    let events: Vec<&str> = explore.iter().filter_map(|e| e["event"].as_str()).collect();
    for want in ["observed", "planned", "injected", "replayRun", "finished"] {
        assert!(events.contains(&want), "{want} in {events:?}");
    }
    assert!(
        explore
            .iter()
            .any(|e| e["event"] == "injected" && e["instruction"] == "use search"),
        "the instruction reached the child: {explore:?}"
    );
    for want in ["stepStarted", "stepFinished"] {
        assert!(kinds.iter().any(|k| k == want), "the replay's {want}");
    }
    assert_eq!(last_status["status"], "passed", "{last_status}");
    assert_eq!(result["outcome"], "success", "{result}");
    assert_eq!(result["file"], "open-network-internet.yaml", "{result}");
    assert_eq!(result["replay"][0]["passed"], true);
    assert!(result["yaml"].as_str().unwrap().contains("appId"));
    let run_dir = std::fs::read_dir(s.project.join("velocity-results/console"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let argv = std::fs::read_to_string(run_dir.join("argv.txt")).unwrap();
    assert!(
        argv.contains("open Network & internet com.android.settings"),
        "{argv}"
    );
    assert!(argv.contains("plan.json"), "{argv}");

    // Save: inside the project only, new files unless asked.
    for bad in ["../x.yaml", "/tmp/x.yaml", "flows/x.txt", ""] {
        let body = serde_json::json!({ "path": bad }).to_string();
        let r = s.api("POST", &format!("/api/tasks/{id}/save"), &body).await;
        assert_eq!(r.code, 400, "{bad}: {}", r.body);
    }
    let r = s
        .api(
            "POST",
            &format!("/api/tasks/{id}/save"),
            r#"{"path":"flows/open-network.yaml"}"#,
        )
        .await;
    assert_eq!(r.code, 200, "{}", r.body);
    let saved = std::fs::read_to_string(s.project.join("flows/open-network.yaml")).unwrap();
    assert!(saved.starts_with("appId: com.android.settings"), "{saved}");
    let r = s
        .api(
            "POST",
            &format!("/api/tasks/{id}/save"),
            r#"{"path":"flows/open-network.yaml"}"#,
        )
        .await;
    assert_eq!(r.code, 409, "{}", r.body);
    #[cfg(unix)]
    {
        let outside = tmp_dir("save-outside");
        std::os::unix::fs::symlink(&outside, s.project.join("out")).unwrap();
        let r = s
            .api(
                "POST",
                &format!("/api/tasks/{id}/save"),
                r#"{"path":"out/x.yaml"}"#,
            )
            .await;
        assert_eq!(r.code, 400, "a symlink out of the project: {}", r.body);
        assert!(!outside.join("x.yaml").exists());
        let _ = std::fs::remove_dir_all(&outside);
    }
    // Finished: no more instructions.
    let r = s
        .api(
            "POST",
            &format!("/api/tasks/{id}/inject"),
            r#"{"text":"more"}"#,
        )
        .await;
    assert_eq!(r.code, 409);
}
