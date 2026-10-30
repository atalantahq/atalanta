use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tracing::debug;
use velocity_common::{PlatformDriver, Result, VelocityError};

use crate::recording::{Recorder, RECORDED_TOOLS};
use crate::session::McpSession;
use crate::snapshot::Snapshots;
use crate::tasks::TaskManager;
use crate::tool_registry::ToolDefinition;
use crate::tools::{device, failure, flow, inspector, interaction, query, steps};

#[derive(Debug)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

pub struct McpServer {
    driver: Arc<dyn PlatformDriver>,
    session: McpSession,
    /// `--read-only`: only tools that don't change the device are listed
    /// and callable.
    read_only: bool,
    /// The session's successful device actions (and the assertions decided
    /// for the screens they led to), for `export_session_flow`.
    recorder: Recorder,
    /// Element refs from the latest `list_elements`.
    refs: crate::refs::Refs,
    /// `snapshot` state: session-stable `@eN` refs and the last snapshot.
    snapshots: Snapshots,
    /// Background tasks (`run_task`).
    tasks: Arc<TaskManager>,
    /// Take the device's cross-process lease for each device tool call.
    leases: bool,
    /// The `velocity` binary `run_task` starts (default: this executable).
    velocity_bin: Option<std::path::PathBuf>,
    /// The device's platform, when the server was told it.
    platform: Option<velocity_common::Platform>,
}

/// Tools that only look: no taps, no typing, no runs.
const READ_ONLY_TOOLS: &[&str] = &[
    "list_devices",
    "screenshot",
    "snapshot",
    "list_elements",
    "get_element",
    "assert_visible",
    "assert_not_visible",
    "get_screen_summary",
    "wait_for_element",
    "list_flows",
    "list_tests",
    "describe_failure",
    "generate_test_skeleton",
    "export_session_flow",
    "task_status",
    "inspect_trace",
];

/// Tools whose runs can reset app data (`clearState`), install or wipe.
const DESTRUCTIVE_TOOLS: &[&str] = &["run_flow", "run_test", "run_task", "launch_app"];

/// Tools that never touch the device, so they take no device lease.
const NO_DEVICE_TOOLS: &[&str] = &[
    "list_devices",
    "list_flows",
    "list_tests",
    "describe_failure",
    "open_inspector",
    "close_inspector",
    "run_task",
    "task_status",
    "task_stop",
    "task_inject",
    "inspect_trace",
    "diagnose",
];

/// Common names models use for these tools.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("click", "tap"),
    ("press", "tap"),
    ("type", "type_text"),
    ("input_text", "type_text"),
    ("enter_text", "type_text"),
    ("launch", "launch_app"),
    ("open_app", "launch_app"),
    ("close_app", "stop_app"),
    ("kill_app", "stop_app"),
    ("scroll_until_visible", "scroll"),
    ("doctor", "diagnose"),
    ("trace", "inspect_trace"),
    ("run_suite", "run_task"),
    ("key", "press_key"),
    ("take_screenshot", "screenshot"),
    ("screen_summary", "get_screen_summary"),
    ("summary", "get_screen_summary"),
    ("elements", "list_elements"),
    ("find_element", "get_element"),
    ("wait", "wait_for_element"),
    ("devices", "list_devices"),
    ("export_flow", "export_session_flow"),
];

/// MCP tool annotations (spec 2025-03-26+): clients use them to decide
/// what an agent may call without asking.
fn annotations(name: &str) -> Value {
    let read_only = READ_ONLY_TOOLS.contains(&name);
    serde_json::json!({
        "readOnlyHint": read_only,
        "destructiveHint": DESTRUCTIVE_TOOLS.contains(&name),
        "idempotentHint": read_only,
        "openWorldHint": false,
    })
}

/// `"Type-Text"`, `"typeText"`, `"click"` → the tool's real name; an
/// unknown name gets the closest one as a hint.
/// Legacy MCP revisions (the `initialize` handshake) this server answers
/// in, newest first.
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The current, stateless MCP revision (D-33): no `initialize`; each
/// request names its version in `_meta`, and `server/discover` lists them.
const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// `_meta` key a 2026-07-28 request names its protocol version under.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// `UnsupportedProtocolVersionError` (2026-07-28).
const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// How long a client may cache `server/discover` and `tools/list` (the
/// tool set only changes with the binary and `--read-only`).
const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// Usage guidance for agents, returned by `server/discover`.
const INSTRUCTIONS: &str = "Velocity drives a mobile device (iOS simulator/device, Android \
emulator/device) or a browser. Read the screen with snapshot (one line per element, each with a \
ref like @e12; mode \"delta\" after an action shows only what changed), not screenshots. tap, \
type_text, assert_visible, assert_not_visible, get_element and wait_for_element take {\"ref\": \
\"@e12\"}; a stale ref is an error that says so: snapshot again. Use screenshot only for what \
the snapshot does not show. Type passwords with type_text {secret: \"ENV_VAR\"}. \
export_session_flow turns the session into a Velocity YAML suite with assertions; run a suite \
with run_task and poll task_status; run_task with a goal (and app_id) explores instead: an agent \
writes a deterministic YAML test and replays it with no model (steer it with task_inject; review \
summary.explore.yaml, then run it). Debug failures with describe_failure and inspect_trace. \
diagnose checks the machine and devices first.";

/// Every revision this server speaks, newest first (`server/discover`,
/// `-32022` data).
fn supported_versions() -> Vec<&'static str> {
    std::iter::once(MODERN_PROTOCOL_VERSION)
        .chain(SUPPORTED_PROTOCOL_VERSIONS)
        .collect()
}

fn server_info() -> Value {
    serde_json::json!({
        "name": "velocity-mcp",
        "version": env!("CARGO_PKG_VERSION")
    })
}

fn resolve_tool_name(raw: &str, known: &[String]) -> std::result::Result<String, String> {
    let mut snake = String::new();
    for (i, c) in raw.trim().chars().enumerate() {
        if c.is_uppercase() && i > 0 && !snake.ends_with('_') {
            snake.push('_');
        }
        let c = c.to_ascii_lowercase();
        snake.push(if c == '-' || c == ' ' || c == '.' {
            '_'
        } else {
            c
        });
    }
    if known.contains(&snake) {
        return Ok(snake);
    }
    if let Some((_, real)) = TOOL_ALIASES.iter().find(|(a, _)| *a == snake) {
        return Ok((*real).to_string());
    }
    let closest = known
        .iter()
        .min_by_key(|k| strsim_distance(k, &snake))
        .cloned()
        .unwrap_or_default();
    Err(format!("Unknown tool: {raw} (did you mean `{closest}`?)"))
}

/// Levenshtein distance, for the "did you mean" hint.
fn strsim_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The recorded actions as a runnable suite (tests).
#[cfg(test)]
fn session_flow_yaml(app_id: &str, name: &str, recorded: &Recorder) -> (String, usize) {
    recorded.suite_yaml(app_id, name)
}

fn ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    }
}

fn error(id: Value, code: i32, message: String) -> JsonRpcResponse {
    error_with_data(id, code, message, None)
}

fn error_with_data(id: Value, code: i32, message: String, data: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data,
        }),
    }
}

/// A 2026-07-28 result: `resultType` (required there) and the answering
/// revision in `_meta`, merged into whatever `_meta` the result already has
/// (D-33). The server's identity is not added: its `_meta` key was a guess
/// (review-d7 m6); `serverInfo` stays in `initialize` and `server/discover`.
fn modern_result(mut result: Value) -> Value {
    if let Value::Object(map) = &mut result {
        map.entry("resultType")
            .or_insert_with(|| Value::String("complete".to_string()));
        let meta = map
            .entry("_meta")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Value::Object(meta) = meta {
            meta.insert(
                META_PROTOCOL_VERSION.to_string(),
                Value::String(MODERN_PROTOCOL_VERSION.to_string()),
            );
        }
    }
    result
}

/// Parse one line as a JSON-RPC message: `Ok(None)` for nothing to do,
/// `Err` with the reply for a malformed one. Not JSON is a parse error
/// (-32700); JSON that isn't a valid request is -32600 (VEL-243), as are a
/// batch and an `"id": null`, which used to run the tool with no reply
/// (VEL-086).
fn parse_request(line: &str) -> std::result::Result<Option<JsonRpcRequest>, Box<JsonRpcResponse>> {
    let invalid =
        |id: Value, why: &str| Box::new(error(id, -32600, format!("Invalid Request: {why}")));
    let value: Value = serde_json::from_str(line)
        .map_err(|e| Box::new(error(Value::Null, -32700, format!("Parse error: {e}"))))?;
    let obj = match &value {
        Value::Object(o) => o,
        Value::Array(_) => return Err(invalid(Value::Null, "batches are not supported")),
        _ => return Err(invalid(Value::Null, "expected an object")),
    };
    let id = match obj.get("id") {
        None => None,
        Some(id @ (Value::String(_) | Value::Number(_))) => Some(id.clone()),
        Some(Value::Null) => return Err(invalid(Value::Null, "id must not be null")),
        Some(_) => return Err(invalid(Value::Null, "id must be a string or a number")),
    };
    let reply_id = id.clone().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid(reply_id, "jsonrpc must be \"2.0\""));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response from the client (no method) needs nothing.
        return if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
            Ok(None)
        } else {
            Err(invalid(reply_id, "method must be a string"))
        };
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if !matches!(params, Value::Null | Value::Object(_) | Value::Array(_)) {
        return Err(Box::new(error(
            reply_id,
            -32602,
            "Invalid params: params must be an object".to_string(),
        )));
    }
    Ok(Some(JsonRpcRequest {
        id,
        method: method.to_string(),
        params,
    }))
}

/// The id of a request line, if it has one.
fn request_id(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("id")
        .filter(|id| id.is_string() || id.is_number())
        .cloned()
}

/// What a message that arrives while a request runs asks for.
enum SideChannel {
    /// `ping`: answered at once.
    Ping(Value),
    /// `notifications/cancelled` for this request id.
    Cancel(Value),
    /// Anything else: queued until the running request finishes.
    Other,
}

fn is_tool_call(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .is_ok_and(|v| v.get("method").and_then(Value::as_str) == Some("tools/call"))
}

fn side_channel(line: &str) -> SideChannel {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return SideChannel::Other;
    };
    match v.get("method").and_then(Value::as_str) {
        Some("ping") => match v.get("id") {
            Some(id) if id.is_string() || id.is_number() => SideChannel::Ping(id.clone()),
            _ => SideChannel::Other,
        },
        Some("notifications/cancelled") => match v.pointer("/params/requestId") {
            Some(id) => SideChannel::Cancel(id.clone()),
            None => SideChannel::Other,
        },
        _ => SideChannel::Other,
    }
}

/// Longest line read from stdin; the rest of a longer one is discarded and
/// answered with a parse error.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Lines buffered ahead of the running request, in the reader's channel and
/// again in the serve loop's queue. Past that, stdin is not read until a
/// request finishes: backpressure on the pipe instead of unbounded memory.
const MAX_QUEUED: usize = 64;

/// Read `input` line by line on its own thread. Bytes that aren't UTF-8 are
/// replaced rather than ending the server (VEL-084), and a line is bounded,
/// as is the number of lines read ahead.
fn spawn_line_reader<R: BufRead + Send + 'static>(
    mut input: R,
) -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx) = tokio::sync::mpsc::channel(MAX_QUEUED);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_bounded_line(&mut input, &mut buf, MAX_LINE_BYTES) {
                Ok(None) => break,
                Ok(Some(overlong)) => {
                    let line = if overlong {
                        // Not JSON: the server replies -32700.
                        "\u{0}line too long".to_string()
                    } else {
                        String::from_utf8_lossy(&buf).into_owned()
                    };
                    if tx.blocking_send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// One `\n`-terminated line into `buf`, at most `max` bytes kept.
/// `Ok(None)` at end of input; `Ok(Some(true))` when the line was longer.
fn read_bounded_line<R: BufRead>(
    input: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<Option<bool>> {
    let mut overlong = false;
    let mut read_any = false;
    loop {
        let chunk = match input.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(read_any.then_some(overlong));
        }
        read_any = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let room = max.saturating_sub(buf.len());
        if take > room {
            overlong = true;
        }
        buf.extend_from_slice(&chunk[..take.min(room)]);
        input.consume(take);
        if done {
            return Ok(Some(overlong));
        }
    }
}

impl McpServer {
    pub fn new(
        driver: Arc<dyn PlatformDriver>,
        device_id: String,
        config_path: Option<String>,
    ) -> Self {
        Self {
            driver,
            session: McpSession::new(Some(device_id), config_path),
            read_only: false,
            recorder: Recorder::default(),
            refs: crate::refs::Refs::default(),
            snapshots: Snapshots::default(),
            tasks: Arc::new(TaskManager::new(TaskManager::default_root(), Vec::new())),
            leases: false,
            velocity_bin: None,
            platform: None,
        }
    }

    /// Run background tasks with `tasks` (its folder and notifiers).
    pub fn with_tasks(mut self, tasks: TaskManager) -> Self {
        self.tasks = Arc::new(tasks);
        self
    }

    /// Take the device's cross-process lease for each tool call that drives
    /// it (`velocity mcp` does; tests and embedders may not want it).
    pub fn with_leases(mut self, on: bool) -> Self {
        self.leases = on;
        self
    }

    /// The `velocity` binary `run_task` runs (default: this executable).
    pub fn with_velocity_bin(mut self, bin: std::path::PathBuf) -> Self {
        self.velocity_bin = Some(bin);
        self
    }

    /// The device's platform (else learned from the device list).
    pub fn with_platform(mut self, platform: velocity_common::Platform) -> Self {
        self.platform = Some(platform);
        self.refs.platform = platform;
        self.snapshots.platform = Some(platform);
        self.recorder.platform = Some(platform);
        self
    }

    /// Serve only the tools that don't change the device (`--read-only`).
    pub fn read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }

    /// Initialize structured logging to stderr (critical: stdout is for MCP protocol only).
    pub fn init_logging() {
        use tracing_subscriber::{fmt, EnvFilter};
        let _ = fmt::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
            )
            .with_target(false)
            .try_init();
    }

    pub async fn run_stdio(&mut self) -> Result<()> {
        let rx = spawn_line_reader(io::BufReader::new(io::stdin()));
        let served = self.serve(rx, &mut io::stdout()).await;
        self.shutdown().await;
        served
    }

    /// Stop every background task and wait for its reports.
    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    /// Serve messages from `rx` until it closes. Stdin is read on its own
    /// thread, so while one request runs, `ping` is answered at once and a
    /// `notifications/cancelled` for it stops it (VEL-085); other requests
    /// wait their turn, as tools share the device and the session.
    ///
    /// End of input is the client shutting down (MCP stdio transport): a
    /// `tools/call` still queued then is answered with an error, not run
    /// against the device with nobody waiting for it.
    ///
    /// Cancel safety: a cancelled call's future is dropped at its next await,
    /// so a device action already sent is not undone, and a recording step
    /// is only added once its action returned.
    async fn serve(
        &mut self,
        mut rx: tokio::sync::mpsc::Receiver<String>,
        out: &mut dyn Write,
    ) -> Result<()> {
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut open = true;
        loop {
            let line = match queue.pop_front() {
                Some(l) => l,
                None if open => match rx.recv().await {
                    Some(l) => l,
                    None => break,
                },
                None => break,
            };
            let id = request_id(&line);
            if !open && is_tool_call(&line) {
                if let Some(id) = id {
                    let msg = "not run: the client closed its input".to_string();
                    write_response(out, &error(id, -32000, msg))?;
                }
                continue;
            }
            let call = self.handle_line(&line);
            tokio::pin!(call);
            let resp = loop {
                tokio::select! {
                    // The running request first: replies stay in order
                    // unless it really is still running.
                    biased;
                    resp = &mut call => break resp,
                    next = rx.recv(), if open && queue.len() < MAX_QUEUED => match next {
                        None => open = false,
                        Some(next) => match side_channel(&next) {
                            SideChannel::Ping(ping_id) => write_response(out, &ok(ping_id, serde_json::json!({})))?,
                            SideChannel::Cancel(c) if id.as_ref() == Some(&c) => {
                                debug!("request {c} cancelled");
                                break None;
                            }
                            // Not started yet: drop it from the queue.
                            SideChannel::Cancel(c) => {
                                queue.retain(|l| request_id(l).as_ref() != Some(&c))
                            }
                            SideChannel::Other => queue.push_back(next),
                        },
                    },
                }
            };
            if let Some(resp) = resp {
                write_response(out, &resp)?;
            }
        }
        Ok(())
    }

    /// One JSON-RPC message in, the response to write (none for blank lines
    /// and notifications).
    async fn handle_line(&mut self, line: &str) -> Option<JsonRpcResponse> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        debug!("Received: {line}");
        let request = match parse_request(line) {
            Ok(Some(req)) => req,
            Ok(None) => return None,
            Err(resp) => return Some(*resp),
        };
        // A notification (no id, e.g. `notifications/initialized`) gets no
        // reply: answering it breaks strict clients. Only notifications are
        // acted on; a request without an id is never run (VEL-086).
        let Some(id) = request.id.clone() else {
            if request.method.starts_with("notifications/") {
                let _ = self.dispatch(&request).await;
            }
            return None;
        };
        // D-33: `_meta` must be an object; a version it names must be one
        // this server speaks (-32022 with the list), except on
        // `server/discover`, which is how a client learns the list.
        let meta = request.params.get("_meta");
        if meta.is_some_and(|m| !m.is_object()) {
            return Some(error(
                id,
                -32602,
                "Invalid params: _meta must be an object".to_string(),
            ));
        }
        let requested = meta.and_then(|m| m.get(META_PROTOCOL_VERSION));
        if let Some(requested) = requested {
            let known = requested
                .as_str()
                .is_some_and(|v| supported_versions().contains(&v));
            if !known && request.method != "server/discover" {
                return Some(error_with_data(
                    id,
                    UNSUPPORTED_PROTOCOL_VERSION,
                    format!("Unsupported protocol version: {requested}"),
                    Some(serde_json::json!({
                        "supported": supported_versions(),
                        "requested": requested,
                    })),
                ));
            }
        }
        let modern = request.method == "server/discover"
            || requested.and_then(Value::as_str) == Some(MODERN_PROTOCOL_VERSION);
        const METHODS: [&str; 5] = [
            "initialize",
            "server/discover",
            "tools/list",
            "tools/call",
            "ping",
        ];
        if !METHODS.contains(&request.method.as_str())
            && !request.method.starts_with("notifications/")
        {
            return Some(error(
                id,
                -32601,
                format!("Method not found: {}", request.method),
            ));
        }
        if request.method == "tools/call"
            && !request.params.get("name").is_some_and(Value::is_string)
        {
            return Some(error(
                id,
                -32602,
                "Invalid params: tools/call needs a string `name`".to_string(),
            ));
        }
        // An unknown tool is a protocol error, -32602 (MCP tools spec), with
        // the "did you mean" hint (P-9).
        if request.method == "tools/call" {
            let raw = request.params["name"].as_str().unwrap_or_default();
            let known: Vec<String> = self
                .tool_definitions()
                .into_iter()
                .map(|d| d.name)
                .collect();
            if let Err(msg) = resolve_tool_name(raw, &known) {
                return Some(error(id, -32602, msg));
            }
        }
        Some(match self.dispatch(&request).await {
            Ok(mut result) if modern => {
                if request.method == "tools/list" {
                    result["ttlMs"] = CACHE_TTL_MS.into();
                    result["cacheScope"] = "private".into();
                }
                ok(id, modern_result(result))
            }
            Ok(result) => ok(id, result),
            Err(e) => error(id, -32000, e.to_string()),
        })
    }

    async fn dispatch(&mut self, request: &JsonRpcRequest) -> Result<Value> {
        match request.method.as_str() {
            "initialize" => self.handle_initialize(&request.params),
            "server/discover" => Ok(self.handle_discover()),
            "tools/list" => self.handle_tools_list(),
            "ping" => Ok(serde_json::json!({})),
            m if m.starts_with("notifications/") => Ok(Value::Null),
            "tools/call" => self.handle_tools_call(&request.params).await,
            other => Err(VelocityError::Internal(anyhow::anyhow!(
                "Unknown method: {other}"
            ))),
        }
    }

    /// Answers with the client's protocol version when it is one this server
    /// speaks, else the newest one (P-9; the version-negotiation rule).
    fn handle_initialize(&self, params: &Value) -> Result<Value> {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
        Ok(serde_json::json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": server_info()
        }))
    }

    /// `server/discover` (2026-07-28): what this server speaks and offers,
    /// answered whatever era the client is in.
    fn handle_discover(&self) -> Value {
        serde_json::json!({
            "supportedVersions": supported_versions(),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": server_info(),
            "instructions": INSTRUCTIONS,
            "ttlMs": CACHE_TTL_MS,
            "cacheScope": "private",
        })
    }

    /// MCP's shape is `{ "tools": [...] }`; a bare array lists nothing in
    /// spec-following clients.
    fn handle_tools_list(&self) -> Result<Value> {
        let mut tools = Vec::new();
        for def in self.tool_definitions() {
            if self.read_only && !READ_ONLY_TOOLS.contains(&def.name.as_str()) {
                continue;
            }
            let mut v = serde_json::to_value(&def).map_err(|e| {
                VelocityError::Internal(anyhow::anyhow!("Serialization error: {e}"))
            })?;
            v["annotations"] = annotations(&def.name);
            tools.push(v);
        }
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn handle_tools_call(&mut self, params: &Value) -> Result<Value> {
        let raw_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VelocityError::Config("Missing tool name in params".to_string()))?;
        let known: Vec<String> = self
            .tool_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        let resolved = match resolve_tool_name(raw_name, &known) {
            Ok(n) => n,
            Err(msg) => {
                return Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": msg }],
                    "isError": true
                }))
            }
        };
        let name = resolved.as_str();
        if self.read_only && !READ_ONLY_TOOLS.contains(&name) {
            return Ok(serde_json::json!({
                "content": [{ "type": "text", "text": format!(
                    "`{name}` changes the device; this server was started with --read-only") }],
                "isError": true
            }));
        }

        let mut arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));

        debug!("Calling tool: {name}");

        let device_id = self
            .session
            .resolve_device_id(&*self.driver)
            .await
            .unwrap_or_default();
        let config_path = self.session.config_path.clone();

        // One device, one driver at a time across processes: hold its lease
        // for the call (released when this function returns).
        let _lease = if self.leases && !NO_DEVICE_TOOLS.contains(&name) && !device_id.is_empty() {
            match self.take_lease(&device_id).await {
                Ok(l) => Some(l),
                Err(msg) => return Ok(tool_error(&msg)),
            }
        } else {
            None
        };

        // A ref (`@e12` from snapshot, `e3` from list_elements) becomes the
        // selector it stands for, built in code (also what is recorded), or
        // an error the agent sees.
        if let Err(e) = self.resolve_refs(&device_id, &mut arguments).await {
            return Ok(tool_error(&e.to_string()));
        }

        // The screen the last recorded action led to decides its assertion;
        // read it now, before this action changes it.
        if RECORDED_TOOLS.contains(&name) || name == "export_session_flow" {
            self.observe_screen(&device_id).await;
        }

        let result = match name {
            "list_devices" => {
                let devices = self.session.get_devices(&*self.driver).await?;
                Ok(serde_json::to_value(devices).unwrap_or(Value::Null))
            }
            "screenshot" => device::screenshot(&self.driver, &device_id, &arguments).await,
            "snapshot" => self.snapshot(&device_id, &arguments).await,
            "tap" => interaction::tap(&self.driver, &device_id, &arguments).await,
            "type_text" => interaction::type_text(&self.driver, &device_id, &arguments).await,
            "swipe" => interaction::swipe(&self.driver, &device_id, &arguments).await,
            "scroll" => steps::scroll(&self.driver, &device_id, &config_path, &arguments).await,
            "launch_app" => {
                steps::launch_app(&self.driver, &device_id, &config_path, &arguments).await
            }
            "stop_app" => steps::stop_app(&self.driver, &device_id, &config_path, &arguments).await,
            "press_key" => {
                let platform = self.device_platform(&device_id).await;
                interaction::press_key(&self.driver, &device_id, platform, &arguments).await
            }
            "list_elements" => {
                // Refs follow the device's platform rules (web ids are per load).
                if let Some(p) = self.device_platform(&device_id).await {
                    self.refs.platform = p;
                }
                query::list_elements(&self.driver, &device_id, &arguments, &mut self.refs).await
            }
            "get_element" => query::get_element(&self.driver, &device_id, &arguments).await,
            "assert_visible" => query::assert_visible(&self.driver, &device_id, &arguments).await,
            "assert_not_visible" => {
                steps::assert_not_visible(&self.driver, &device_id, &config_path, &arguments).await
            }
            "get_screen_summary" => {
                query::get_screen_summary(&self.driver, &device_id, &arguments).await
            }
            "wait_for_element" => {
                query::wait_for_element(&self.driver, &device_id, &arguments).await
            }
            "list_flows" => flow::list_flows(&config_path, &arguments),
            "run_flow" => flow::run_flow(&self.driver, &device_id, &config_path, &arguments).await,
            "list_tests" => flow::list_tests(&config_path, &arguments),
            "run_test" => flow::run_test(&self.driver, &device_id, &config_path, &arguments).await,
            "generate_test_skeleton" => {
                let screen = self.driver.get_hierarchy(&device_id).await.ok();
                let rect = crate::refs::screen_rect(&self.driver, &device_id).await;
                let platform = self.device_platform(&device_id).await;
                flow::generate_test_skeleton(
                    &config_path,
                    &arguments,
                    flow::SkeletonSource {
                        recorder: Some(&self.recorder),
                        screen: screen.as_ref(),
                        screen_rect: rect,
                        platform,
                    },
                )
            }
            "describe_failure" => {
                if self.read_only {
                    failure::check_read_only_artifacts_dir(&config_path, &arguments)
                        .and_then(|checked| failure::describe_failure(&config_path, &checked))
                } else {
                    failure::describe_failure(&config_path, &arguments)
                }
            }
            "open_inspector" => {
                inspector::open_inspector(&self.driver, &device_id, &mut self.session, &arguments)
                    .await
            }
            "close_inspector" => inspector::close_inspector(&mut self.session, &arguments).await,
            "export_session_flow" => {
                let app_id = arguments
                    .get("app_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        self.session
                            .config_path
                            .as_deref()
                            .and_then(|p| velocity_core::parse_suite(p).ok())
                            .map(|s| s.app_id)
                    })
                    .or_else(|| self.recorder.launched_app().map(String::from))
                    .unwrap_or_else(|| "com.example.app".to_string());
                let test_name = arguments
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("recorded session");
                let (yaml, steps) = self.recorder.suite_yaml(&app_id, test_name);
                let assertions = self
                    .recorder
                    .steps
                    .iter()
                    .filter(|r| r.assertion.is_some())
                    .count();
                if arguments
                    .get("clear")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    self.recorder.clear();
                }
                Ok(serde_json::json!({ "steps": steps, "assertions": assertions, "yaml": yaml }))
            }
            "run_task" => self.run_task(&device_id, &arguments).await,
            "task_status" => self.task_status(&arguments).await,
            "task_stop" => self.task_stop(&arguments).await,
            "task_inject" => self.task_inject(&arguments),
            "diagnose" => self.diagnose(&device_id, &arguments).await,
            "inspect_trace" => self.inspect_trace(&config_path, &arguments),
            other => Err(VelocityError::Internal(anyhow::anyhow!(
                "Unknown tool: {other}"
            ))),
        };

        // Refs belong to one snapshot: whatever changed the screen (even a
        // failed tap may have) retires them (review MAJOR-2).
        if crate::refs::SCREEN_CHANGING_TOOLS.contains(&name) {
            self.refs.expire(name);
        }
        if result.is_ok() && RECORDED_TOOLS.contains(&name) {
            self.recorder.record(name, &arguments);
        }
        // A failing tool is a result with `isError`, so the model sees the
        // message; JSON-RPC errors are for protocol failures.
        Ok(match result {
            // Masked like reports: a page can show tokens, a failure can
            // quote a password the agent typed.
            Ok(mut value) => {
                // A screenshot is an image block, not base64 in the text (the
                // model would read it as tokens), and only the text is masked:
                // redaction would corrupt the PNG (P-2, NB-3).
                let image = match name {
                    "screenshot" | "inspect_trace" => {
                        value.as_object_mut().and_then(|o| o.remove("data"))
                    }
                    _ => None,
                };
                velocity_common::redact::redact_value(&mut value);
                // Text results (a text snapshot) go out as they are, not as a
                // JSON string full of escapes.
                let text = match &value {
                    Value::String(s) => s.clone(),
                    v => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                };
                let mut content = vec![serde_json::json!({ "type": "text", "text": text })];
                if let Some(data) = image {
                    content.push(serde_json::json!({
                        "type": "image",
                        "data": data,
                        "mimeType": "image/png"
                    }));
                }
                serde_json::json!({ "content": content })
            }
            Err(e) => tool_error(&e.to_string()),
        })
    }

    /// The device's platform: the one the server was given, else from the
    /// device list.
    async fn device_platform(&mut self, device_id: &str) -> Option<velocity_common::Platform> {
        if self.platform.is_some() {
            return self.platform;
        }
        let devices = self.session.get_devices(&*self.driver).await.ok()?;
        devices
            .iter()
            .find(|d| d.id == device_id)
            .or(devices.first().filter(|_| devices.len() == 1))
            .map(|d| d.platform)
    }

    /// The device's lease, waiting a few seconds for a holder to finish;
    /// else why not.
    async fn take_lease(
        &self,
        device_id: &str,
    ) -> std::result::Result<velocity_runner::lease::DeviceLock, String> {
        if let Some(task) = self.tasks.running_on(device_id) {
            return Err(format!(
                "{device_id} is busy running task {task}: wait for it (task_status) or stop it (task_stop)"
            ));
        }
        let deadline = std::time::Instant::now() + LEASE_WAIT;
        loop {
            match velocity_runner::lease::try_lock(device_id) {
                Ok(l) => return Ok(l),
                Err(velocity_runner::lease::LeaseError::Held(holder))
                    if std::time::Instant::now() < deadline =>
                {
                    debug!("{device_id} leased by {holder}; waiting");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(velocity_runner::lease::LeaseError::Held(holder)) => {
                    return Err(format!(
                        "{device_id} is in use by another Velocity process ({holder}); try again when it is done"
                    ))
                }
                Err(e) => return Err(format!("{device_id}: {e}")),
            }
        }
    }

    /// Turn `ref` / `selector.ref` / a `"@e12"` selector into the selector
    /// it stands for.
    async fn resolve_refs(&mut self, device_id: &str, args: &mut Value) -> Result<()> {
        // `{"ref": "…"}` at the top level and `"selector": "@e12"` both mean
        // `{"selector": {"ref": …}}`.
        if let Some(r) = args.as_object_mut().and_then(|o| o.remove("ref")) {
            if args.get("selector").is_some() {
                return Err(VelocityError::Config(
                    "give 'ref' or 'selector', not both".into(),
                ));
            }
            args["selector"] = serde_json::json!({ "ref": r });
        }
        if let Some(r) = args
            .get("selector")
            .and_then(Value::as_str)
            .map(String::from)
        {
            args["selector"] = serde_json::json!({ "ref": r });
        }
        let Some(r) = args.pointer("/selector/ref").cloned() else {
            return Ok(());
        };
        let tree = self.driver.get_hierarchy(device_id).await?;
        let screen = crate::refs::screen_rect(&self.driver, device_id).await;
        self.recorder.observe(&tree, screen.as_ref());
        match r.as_str() {
            Some(r) if crate::snapshot::is_snapshot_ref(r) => {
                if let Some(p) = self.device_platform(device_id).await {
                    self.snapshots.platform = Some(p);
                }
                let selector = self.snapshots.resolve(r, &tree, screen.as_ref())?;
                args["selector"] = serde_json::to_value(selector)
                    .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
                Ok(())
            }
            _ => self.refs.resolve_args(args, &tree, screen.as_ref()),
        }
    }

    /// Read the screen for the recording, when it needs one (best-effort).
    async fn observe_screen(&mut self, device_id: &str) {
        if !self.recorder.wants_tree() {
            return;
        }
        if let Some(p) = self.device_platform(device_id).await {
            self.recorder.platform = Some(p);
        }
        if let Ok(tree) = self.driver.get_hierarchy(device_id).await {
            let screen = crate::refs::screen_rect(&self.driver, device_id).await;
            self.recorder.observe(&tree, screen.as_ref());
        }
    }

    async fn snapshot(&mut self, device_id: &str, args: &Value) -> Result<Value> {
        let opts = crate::snapshot::Options::from_args(args)?;
        let tree = self.driver.get_hierarchy(device_id).await?;
        let screen = crate::refs::screen_rect(&self.driver, device_id).await;
        if let Some(p) = self.device_platform(device_id).await {
            self.snapshots.platform = Some(p);
            self.recorder.platform = Some(p);
        }
        self.recorder.observe(&tree, screen.as_ref());
        Ok(self.snapshots.take(&tree, screen.as_ref(), opts))
    }

    /// The `velocity` binary tasks run (default: this executable).
    fn program(&self) -> Result<std::path::PathBuf> {
        match &self.velocity_bin {
            Some(p) => Ok(p.clone()),
            None => std::env::current_exe().map_err(|e| {
                VelocityError::Internal(anyhow::anyhow!("finding the velocity binary: {e}"))
            }),
        }
    }

    async fn run_task(&mut self, device_id: &str, args: &Value) -> Result<Value> {
        use crate::tasks::process::{Child, RunRequest, SuiteRun};
        if args.get("goal").is_some() {
            return self.explore_task(device_id, args).await;
        }
        let device = args
            .get("device")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| device_id.to_string());
        if let Some(task) = self.tasks.running_on(&device) {
            return Err(VelocityError::Config(format!(
                "{device} is already running task {task}; wait for it or task_stop it"
            )));
        }
        let (id, dir) = self.tasks.new_task_dir()?;
        let (suite_path, suite) = match (
            args.get("yaml").and_then(Value::as_str),
            args.get("suite").and_then(Value::as_str),
        ) {
            (Some(_), Some(_)) => {
                return Err(VelocityError::Config(
                    "give 'suite' (a path) or 'yaml' (the suite itself), not both".into(),
                ))
            }
            (Some(yaml), None) => {
                let suite = velocity_core::parser::parse_suite_from_str(yaml)?;
                velocity_core::validate_suite(&suite)?;
                let path = dir.join("suite.yaml");
                std::fs::write(&path, yaml)
                    .map_err(|e| VelocityError::Config(format!("{}: {e}", path.display())))?;
                (path, suite)
            }
            (None, path) => {
                let path = path
                    .map(String::from)
                    .or_else(|| self.session.config_path.clone())
                    .ok_or_else(|| {
                        VelocityError::Config(
                            "run_task needs 'suite' (a path) or 'yaml' (no suite is loaded)".into(),
                        )
                    })?;
                let suite = velocity_core::parse_suite(&path)?;
                (std::path::PathBuf::from(path), suite)
            }
        };
        let tags: Vec<String> = match args.get("tags") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect(),
            Some(Value::String(s)) => s.split(',').map(|t| t.trim().to_string()).collect(),
            _ => Vec::new(),
        };
        let filter = args.get("filter").and_then(Value::as_str).map(String::from);
        let verify = args.get("verify").and_then(Value::as_bool).unwrap_or(false);
        let retries = args
            .get("retries")
            .and_then(Value::as_u64)
            .map(|r| r.min(5) as u32)
            .or(verify.then_some(1));
        let platform = match self.platform {
            Some(p) => Some(p),
            None => self.device_platform(&device).await,
        };
        let req = RunRequest {
            suite: suite_path.clone(),
            device: Some(device.clone()),
            platform,
            filter: filter.clone(),
            tags: tags.clone(),
            retries,
            step_screenshots: args
                .get("step_screenshots")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            trace_always: true,
        };
        let artifacts = dir.join("artifacts");
        let log = dir.join("run.log");
        let body = SuiteRun {
            child: Child {
                program: self.program()?,
                args: req.args(&artifacts),
                cwd: std::env::current_dir().ok(),
                log: log.clone(),
            },
            artifacts: artifacts.clone(),
        };
        let meta = crate::tasks::TaskMeta {
            target: suite_path.to_string_lossy().into_owned(),
            device: Some(device),
            dir,
            log,
            artifacts: artifacts.clone(),
            tests_total: (filter.is_none() && tags.is_empty()).then_some(suite.tests.len()),
        };
        // The device is the child's now: refs from this process are stale.
        self.refs.expire("run_task");
        let info = self.tasks.spawn(id, meta, Box::new(body));
        Ok(serde_json::json!({
            "task_id": info.id,
            "state": info.state,
            "dir": info.dir,
            "log": info.log,
            "trace_dir": artifacts.join("traces"),
            "next": "poll task_status {task_id} (wait_ms makes it wait for a change); inspect_trace {task_id} reads its traces",
        }))
    }

    /// `run_task` with a `goal`: a `velocity explore` child that writes a
    /// test, replays it and reports where it is.
    async fn explore_task(&mut self, device_id: &str, args: &Value) -> Result<Value> {
        use crate::tasks::process::{Child, ExploreRequest, ExploreRun};
        let bad = |m: String| VelocityError::Config(m);
        let goal = str_arg(args, "goal")?.trim().to_string();
        if goal.is_empty() {
            return Err(bad("'goal' is empty: say what to do on the app".into()));
        }
        for k in ["suite", "yaml", "filter", "tags"] {
            if args.get(k).is_some() {
                return Err(bad(format!(
                    "'{k}' is for suite runs; a 'goal' explores (give one or the other)"
                )));
            }
        }
        if args.get("model").is_some() && args.get("script").is_some() {
            return Err(bad("give 'model' or 'script', not both".into()));
        }
        let device = args
            .get("device")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| device_id.to_string());
        if let Some(task) = self.tasks.running_on(&device) {
            return Err(bad(format!(
                "{device} is already running task {task}; wait for it or task_stop it"
            )));
        }
        // The app: given, else the loaded suite's.
        let app_id = match args.get("app_id").and_then(Value::as_str) {
            Some(a) => a.to_string(),
            None => self
                .session
                .config_path
                .as_deref()
                .and_then(|p| velocity_core::parse_suite(p).ok())
                .map(|s| s.app_id)
                .filter(|a| !a.is_empty())
                .ok_or_else(|| bad("explore needs 'app_id' (bundle id / package)".into()))?,
        };
        let names = |k: &str| -> Vec<String> {
            match args.get(k) {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect(),
                Some(Value::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            }
        };
        let data: Vec<(String, String)> = match args.get("data") {
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str().map_or_else(|| v.to_string(), String::from),
                    )
                })
                .collect(),
            None => Vec::new(),
            Some(_) => return Err(bad("'data' is an object of NAME: value".into())),
        };
        let verify = match args.get("verify") {
            Some(Value::String(v)) => Some(v.clone()),
            Some(Value::Bool(false)) => Some("off".into()),
            Some(Value::Bool(true)) | None => None,
            Some(_) => return Err(bad("'verify' is off, final, checkpoints or strict".into())),
        };
        let platform = match args.get("platform").and_then(Value::as_str) {
            Some("android") => Some(velocity_common::Platform::Android),
            Some("ios") => Some(velocity_common::Platform::Ios),
            Some("web") => Some(velocity_common::Platform::Web),
            Some(p) => return Err(bad(format!("unknown platform '{p}': android, ios or web"))),
            None => match self.platform {
                Some(p) => Some(p),
                None => self.device_platform(&device).await,
            },
        };
        let cwd = std::env::current_dir().ok();
        let (id, dir) = self.tasks.new_task_dir()?;
        let out = match args.get("out").and_then(Value::as_str) {
            Some(o) => cwd.clone().unwrap_or_default().join(o),
            None => dir.join("explore.yaml"),
        };
        let req = ExploreRequest {
            goal: goal.clone(),
            app_id,
            device: Some(device.clone()),
            platform,
            model: args.get("model").and_then(Value::as_str).map(String::from),
            script: args
                .get("script")
                .and_then(Value::as_str)
                .map(|p| cwd.clone().unwrap_or_default().join(p)),
            data,
            secrets: names("secret"),
            max_steps: args
                .get("max_steps")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            verify,
            replay: args
                .get("replay")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            out: out.clone(),
        };
        let inject = dir.join("inject.txt");
        let log = dir.join("run.log");
        let body = ExploreRun {
            child: Child {
                program: self.program()?,
                args: req.args(&inject),
                cwd,
                log: log.clone(),
            },
            inject,
        };
        let meta = crate::tasks::TaskMeta {
            target: goal,
            device: Some(device),
            dir: dir.clone(),
            log,
            artifacts: dir,
            tests_total: None,
        };
        self.refs.expire("run_task");
        let info = self.tasks.spawn(id, meta, Box::new(body));
        Ok(serde_json::json!({
            "task_id": info.id,
            "kind": info.kind,
            "state": info.state,
            "dir": info.dir,
            "log": info.log,
            "out": out,
            "next": "poll task_status {task_id} (wait_ms waits for the next turn); steer it with task_inject; when it passes, review the YAML in summary.explore.yaml and run it with run_task suite=<that path>",
        }))
    }

    async fn task_status(&self, args: &Value) -> Result<Value> {
        let Some(id) = args.get("task_id").and_then(Value::as_str) else {
            let tasks = self.tasks.list();
            return Ok(serde_json::json!({
                "count": tasks.len(),
                "tasks": tasks.iter().map(|t| serde_json::json!({
                    "task_id": t.id, "state": t.state, "target": t.target, "device": t.device,
                })).collect::<Vec<_>>(),
            }));
        };
        let before = self.tasks.status(id)?;
        let wait = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(30_000);
        let info = if wait > 0 && !before.state.is_done() {
            // Wait for the task to end or its step to move on.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait);
            loop {
                let now = self.tasks.status(id)?;
                if now.state.is_done()
                    || now.progress != before.progress
                    || tokio::time::Instant::now() >= deadline
                {
                    break now;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        } else {
            before
        };
        serde_json::to_value(&info).map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))
    }

    async fn task_stop(&self, args: &Value) -> Result<Value> {
        let id = str_arg(args, "task_id")?;
        self.tasks.stop(id)?;
        let wait = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(15_000);
        let info = self
            .tasks
            .wait(id, std::time::Duration::from_millis(wait.min(60_000)))
            .await?;
        serde_json::to_value(&info).map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))
    }

    fn task_inject(&self, args: &Value) -> Result<Value> {
        let id = str_arg(args, "task_id")?;
        let text = str_arg(args, "instruction")?;
        let info = self.tasks.inject(id, text)?;
        Ok(serde_json::json!({
            "task_id": info.id,
            "queued": true,
            "instructions": info.instructions.len(),
        }))
    }

    async fn diagnose(&mut self, device_id: &str, args: &Value) -> Result<Value> {
        use velocity_doctor::{apply_safe_fixes, run_checks, FixKind, Options, Status};
        let attempt_fix = args
            .get("attempt_fix")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if attempt_fix && self.read_only {
            return Err(VelocityError::Config(
                "attempt_fix changes the machine; this server was started with --read-only".into(),
            ));
        }
        let probe_device = args
            .get("probe_device")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let platform = match args.get("platform").and_then(Value::as_str) {
            Some("android") => Some(velocity_common::Platform::Android),
            Some("ios") => Some(velocity_common::Platform::Ios),
            Some("web") => Some(velocity_common::Platform::Web),
            Some(other) => {
                return Err(VelocityError::Config(format!(
                    "unknown platform '{other}': android, ios or web"
                )))
            }
            None => self.device_platform(device_id).await,
        };
        // The same checks as `velocity doctor`; `probe_device` is its `--deep`
        // (tree-server handshake, which takes the device's lease only when free).
        let opts = Options {
            deep: probe_device,
            platforms: platform.into_iter().collect(),
            ..Options::default()
        };
        let mut report = run_checks(&opts).await;
        let mut fixes_applied = Vec::new();
        if attempt_fix && report.has_safe_fixes() {
            for outcome in apply_safe_fixes(&report, &opts).await {
                fixes_applied.push(outcome.message.clone());
            }
            report = run_checks(&opts).await;
        }
        let mut next_steps = Vec::new();
        for status in [Status::Fail, Status::Warn] {
            for check in report.checks.iter().filter(|c| c.status == status) {
                let Some(fix) = &check.fix else { continue };
                let line = match &fix.kind {
                    FixKind::Run(cmd) => format!("Run: {cmd}"),
                    FixKind::Guidance(text) => format!("Guidance: {text}"),
                };
                if !next_steps.contains(&line) {
                    next_steps.push(line);
                }
            }
        }
        let mut value = serde_json::to_value(&report)
            .map_err(|e| VelocityError::Internal(anyhow::anyhow!("{e}")))?;
        value["next_steps"] = next_steps.into();
        value["fixes_applied"] = fixes_applied.into();
        Ok(value)
    }

    fn inspect_trace(&self, config_path: &Option<String>, args: &Value) -> Result<Value> {
        let artifacts = match args.get("task_id").and_then(Value::as_str) {
            Some(id) => self.tasks.status(id)?.artifacts,
            None => {
                let args = if self.read_only {
                    failure::check_read_only_artifacts_dir(config_path, args)?
                } else {
                    args.clone()
                };
                match args.get("artifacts_dir").and_then(Value::as_str) {
                    Some(d) => std::path::PathBuf::from(d),
                    None => std::path::PathBuf::from(
                        config_path
                            .as_deref()
                            .and_then(|p| velocity_core::parse_suite(p).ok())
                            .map(|s| s.config.artifacts.output_dir)
                            .unwrap_or_else(|| "./velocity-results".into()),
                    ),
                }
            }
        };
        let out = crate::trace_view::inspect(&artifacts, args)?;
        let mut value = out.value;
        if let Some(png) = out.png {
            use base64::Engine;
            value["data"] = base64::engine::general_purpose::STANDARD.encode(png).into();
        }
        Ok(value)
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.base_tool_definitions();
        defs.extend(extra_tool_definitions());
        // Every tool that takes a selector also takes a ref instead.
        for d in &mut defs {
            if d.input_schema.pointer("/properties/selector").is_none() {
                continue;
            }
            d.input_schema["properties"]["ref"] = serde_json::json!({
                "type": "string",
                "description": "Instead of selector: a ref from the latest snapshot (\"@e12\") or list_elements (\"e3\")"
            });
            if let Some(req) = d
                .input_schema
                .get_mut("required")
                .and_then(Value::as_array_mut)
            {
                req.retain(|r| r != "selector");
            }
        }
        defs
    }

    fn base_tool_definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "list_devices".to_string(),
                description: "List available devices (simulators/emulators)".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "screenshot".to_string(),
                description: "Take a screenshot; returned as a PNG image".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "tap".to_string(),
                description: "Tap on an element matching the selector".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "object",
                            "description": "Selector to find the element (e.g. {\"text\": \"Login\"}, {\"id\": \"btn_submit\"}, {\"role\": \"button\", \"name\": \"Pay\"}, {\"text\": \"Remember me\", \"checked\": false}, or {\"ref\": \"e3\"} from list_elements)"
                        }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "type_text".to_string(),
                description: "Type text into an element matching the selector. For a password or token, pass `secret` (the name of an environment variable the server has) instead of `text`: the server types its value, never returns it, and export_session_flow writes `${NAME}`".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "object",
                            "description": "Selector to find the element, or {\"ref\": \"eN\"} from list_elements"
                        },
                        "text": {
                            "type": "string",
                            "description": "Text to type"
                        },
                        "secret": {
                            "type": "string",
                            "description": "Instead of text: the name of an environment variable whose value is typed (e.g. \"LOGIN_PASSWORD\"); it must be listed in VELOCITY_SECRETS; the value is never shown"
                        }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "swipe".to_string(),
                description: "Swipe in a direction on the screen".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "direction": {
                            "type": "string",
                            "enum": ["up", "down", "left", "right"],
                            "description": "Swipe direction"
                        }
                    },
                    "required": ["direction"]
                }),
            },
            ToolDefinition {
                name: "press_key".to_string(),
                description: "Press a hardware key. Not every platform has every key: iOS has no escape, delete or arrow keys (escape presses back there, with a note); the web has no volume keys".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "enum": ["back", "home", "enter", "volumeUp", "volumeDown", "backspace", "tab", "escape", "delete", "arrowUp", "arrowDown", "arrowLeft", "arrowRight"],
                            "description": "Key to press"
                        }
                    },
                    "required": ["key"]
                }),
            },
            ToolDefinition {
                name: "list_elements".to_string(),
                description: "List accessibility tree elements, optionally filtered. Each visible, on-screen element gets a `ref` (e.g. \"e3\") that tap/type_text/get_element/assert_visible accept as {\"ref\": \"e3\"}; refs expire with the next list_elements and after any tool that changes the screen (tap, type_text, swipe, press_key, run_flow, run_test)".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "description": "Optional text filter to match against element labels and text"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["json", "text"],
                            "description": "\"text\": one compact line per visible, on-screen element that has a ref (fewer tokens). Default \"json\" lists every element"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "get_element".to_string(),
                description: "Find a specific element by selector".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "object",
                            "description": "Selector to find the element, or {\"ref\": \"eN\"} from list_elements"
                        }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "assert_visible".to_string(),
                description: "Assert that an element matching the selector is visible".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "object",
                            "description": "Selector to check visibility for"
                        }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "get_screen_summary".to_string(),
                description: "Get a concise, LLM-friendly summary of what's currently on screen. Returns screen title, visible text, interactive elements, and navigation state. Much more token-efficient than list_elements.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "wait_for_element".to_string(),
                description: "Wait for an element matching the selector to become visible. Blocks until found or timeout. More efficient than polling list_elements.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": {
                            "type": "object",
                            "description": "Selector to wait for (e.g. {\"text\": \"Welcome\"} or {\"id\": \"home_screen\"})"
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Maximum wait time in milliseconds (default: 10000, max: 30000)"
                        },
                        "poll_interval_ms": {
                            "type": "integer",
                            "description": "Polling interval in milliseconds (default: 500, min: 100)"
                        }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "list_flows".to_string(),
                description: "List available flows from the test config".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "run_flow".to_string(),
                description: "Execute a named flow by ID".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Flow ID to execute"
                        }
                    },
                    "required": ["id"]
                }),
            },
            ToolDefinition {
                name: "list_tests".to_string(),
                description: "List available tests from the test config".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "run_test".to_string(),
                description: "Execute a named test".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Test name to execute"
                        }
                    },
                    "required": ["name"]
                }),
            },
            ToolDefinition {
                name: "describe_failure".to_string(),
                description: "Explain the last failed test from its trace: the failing step, its selector, its file:line in the suite YAML, the error, and the elements on the last screen closest to what the selector asked for (scored like self-healing). Use it to fix the test.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "test": {
                            "type": "string",
                            "description": "Test name; defaults to the most recently failed test"
                        },
                        "artifacts_dir": {
                            "type": "string",
                            "description": "Artifacts directory holding traces/ (default: the suite's artifacts.outputDir, ./velocity-results)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "How many closest elements to return (default 5)"
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "generate_test_skeleton".to_string(),
                description: "A Velocity test from this session: the recorded actions (with their assertions), else a launch plus an assertion on the current screen and its elements as comments. `description` names it and goes into a comment".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "Natural language description of the test to generate"
                        },
                        "app_id": {
                            "type": "string",
                            "description": "Application bundle/package ID (e.g. com.mycompany.myapp). Defaults to the loaded suite's appId, else com.example.app."
                        }
                    },
                    "required": ["description"]
                }),
            },
            ToolDefinition {
                name: "export_session_flow".to_string(),
                description: "Turn this session's successful actions (tap, type_text, scroll, swipe, press_key, launch_app, stop_app, asserts) into a Velocity test (YAML) to save and rerun. After each action that changed the screen it adds an assertVisible on a stable element of the new screen; typed secrets are written as ${NAME}".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Test name" },
                        "app_id": { "type": "string", "description": "App id (default: the loaded suite's)" },
                        "clear": { "type": "boolean", "description": "Start a new recording afterwards" }
                    }
                }),
            },
            ToolDefinition {
                name: "open_inspector".to_string(),
                description: "Launch the visual Inspector web UI for debugging. Shares the same device connection. Returns the URL to open. Idempotent — calling again returns the existing URL.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "port": {
                            "type": "integer",
                            "description": "Port to run the inspector on (default: 9876). Auto-increments if busy."
                        }
                    }
                }),
            },
            ToolDefinition {
                name: "close_inspector".to_string(),
                description: "Stop the Inspector web UI if running.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        ]
    }
}

fn def(name: &str, description: &str, input_schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// The tools added for snapshots, app control, tasks, diagnostics and
/// traces.
fn extra_tool_definitions() -> Vec<ToolDefinition> {
    let selector = serde_json::json!({
        "type": "object",
        "description": "Selector (e.g. {\"id\": \"login\"}, {\"text\": \"Sign in\"})"
    });
    vec![
        def(
            "snapshot",
            "The screen as compact text: one line per visible element (role, \"text\", label, #id, [state]) with a ref like @e12 that tap/type_text/assert_visible/… accept as {\"ref\": \"@e12\"}. Refs stay the same across snapshots while the element does. mode \"delta\" returns only what was added (+), removed (-) or changed (~) since the last snapshot: use it after every action. Far fewer tokens than list_elements or screenshots.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["full", "delta"], "description": "full (default) or delta: only the changes since the last snapshot" },
                    "format": { "type": "string", "enum": ["text", "json"], "description": "text (default, fewest tokens) or json" },
                    "bounds": { "type": "boolean", "description": "Add each element's bounds @(x,y wxh)" }
                }
            }),
        ),
        def(
            "launch_app",
            "Launch the app (app_id, default the loaded suite's) as a suite's launchApp step does; clear_state wipes its data first",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "app_id": { "type": "string", "description": "Package / bundle id (default: the loaded suite's appId)" },
                    "clear_state": { "type": "boolean", "description": "Wipe the app's data first" }
                }
            }),
        ),
        def(
            "stop_app",
            "Stop the app (app_id, default the loaded suite's)",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "app_id": { "type": "string", "description": "Package / bundle id (default: the loaded suite's appId)" }
                }
            }),
        ),
        def(
            "scroll",
            "Scroll the content (direction: down shows what is below; default down). With until_visible, keep scrolling until that element is on screen (up to max_scrolls), as scrollUntilVisible does",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Where the content goes (default down)" },
                    "until_visible": selector,
                    "max_scrolls": { "type": "integer", "description": "With until_visible: at most this many scrolls (default 10)" }
                }
            }),
        ),
        def(
            "assert_not_visible",
            "Assert that no element matching the selector is visible, waiting up to timeout_ms (default 5000) for it to go",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "selector": selector,
                    "timeout_ms": { "type": "integer", "description": "How long it may take to disappear (default 5000, max 30000)" }
                },
                "required": ["selector"]
            }),
        ),
        def(
            "run_task",
            "Run a suite, or explore a goal, in the background and return at once with a task_id, its folder and log. With suite/yaml: a velocity run. With goal (+ app_id): velocity explore, where an agent reaches the goal on the device, writes a deterministic YAML test and replays it with no model; steer it with task_inject. Poll task_status for progress (step i/n; explore: turn, op, target) and the machine-readable summary (explore: the YAML path, replays, model calls and tokens); stop it with task_stop. The device is busy for other tools until it ends",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "suite": { "type": "string", "description": "Suite YAML path (default: the loaded suite)" },
                    "yaml": { "type": "string", "description": "Instead of suite: the suite's YAML itself" },
                    "filter": { "type": "string", "description": "Only tests whose name matches this glob" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Only tests with these tags" },
                    "device": { "type": "string", "description": "Device id (default: the server's device)" },
                    "retries": { "type": "integer", "description": "Retries for a failing test" },
                    "verify": { "type": ["boolean", "string"], "description": "Suite runs (boolean): re-run a failing test once, so a flaky one is reported as flaky, not failed. Explore (string): the AI check of the goal, off, final (default), checkpoints or strict" },
                    "step_screenshots": { "type": "boolean", "description": "Keep a screenshot per step for failing tests (default true)" },
                    "goal": { "type": "string", "description": "Explore instead of running a suite: what to do, in plain English (\"open Network & internet\")" },
                    "app_id": { "type": "string", "description": "Explore: the app (bundle id / package; default: the loaded suite's)" },
                    "platform": { "type": "string", "enum": ["android", "ios", "web"], "description": "Explore: the platform (default: the device's)" },
                    "model": { "type": "string", "description": "Explore: the goal model as provider:model (default: the configured provider's)" },
                    "script": { "type": "string", "description": "Explore: a fixed plan (JSON, docs/EXPLORE.md) instead of a model" },
                    "data": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Explore: values the agent may type, by NAME (written as ${NAME})" },
                    "secret": { "type": "array", "items": { "type": "string" }, "description": "Explore: names of secrets the agent may type, read from the server's environment; never sent to a model or written" },
                    "max_steps": { "type": "integer", "description": "Explore: planning turns at most (default 40)" },
                    "replay": { "type": "integer", "description": "Explore: replays of the written test, with no model (default 1)" },
                    "out": { "type": "string", "description": "Explore: where to write the test (default: explore.yaml in the task folder)" }
                }
            }),
        ),
        def(
            "task_status",
            "A task's state (queued, running, passed, failed, stopped, error), progress (test, step i/n) and, once done, its summary {passed, failed, flaky, skipped, failures: [{test, step, error, screenshot, trace}]}. Without task_id, lists the session's tasks. wait_ms waits (up to 30 s) for the task to move on",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "wait_ms": { "type": "integer", "description": "Wait up to this long for the step to change or the task to end" }
                }
            }),
        ),
        def(
            "task_stop",
            "Stop a running task; it writes its reports and ends as stopped. Waits up to wait_ms (default 15000) for that",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "wait_ms": { "type": "integer" }
                },
                "required": ["task_id"]
            }),
        ),
        def(
            "task_inject",
            "Give a running explore task (run_task with a goal) an instruction it takes in at its next planning turn (\"use the search box\"). Suite runs take none",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "instruction": { "type": "string" }
                },
                "required": ["task_id", "instruction"]
            }),
        ),
        def(
            "diagnose",
            "Check that this machine can run Velocity: adb and its server, Android devices (booted, authorized) and the on-device tree server, Xcode, booted simulators, the iOS runner build, device leases, AI keys (names only), disk and memory. Returns verdict ready|degraded|blocked and ordered next_steps (\"Run: …\" commands, \"Guidance: …\"). attempt_fix makes only safe fixes (restart the adb server, clear stale lease records, re-push the tree server) and lists them",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "attempt_fix": { "type": "boolean", "description": "Make the safe fixes" },
                    "probe_device": { "type": "boolean", "description": "Read the device's screen to check its automation server (default true)" },
                    "platform": { "type": "string", "enum": ["android", "ios", "web"], "description": "The platform to check (default: the device's)" }
                }
            }),
        ),
        def(
            "inspect_trace",
            "Read a run's trace: view summary (one line per step), search (steps whose label, selector, error or screen text contain query), step_details (selector, matched element, nearest elements, timing, error) or step_screenshot (the screen the step acted on, with a red ring at the tap point). From a task (task_id) or an artifacts_dir",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "view": { "type": "string", "enum": ["summary", "search", "step_details", "step_screenshot"] },
                    "task_id": { "type": "string", "description": "A run_task task" },
                    "artifacts_dir": { "type": "string", "description": "Instead of task_id: the artifacts folder (default: the suite's, ./velocity-results)" },
                    "test": { "type": "string", "description": "Test name (default: the newest trace)" },
                    "step": { "type": "integer", "description": "1-based step number, as in the summary" },
                    "query": { "type": "string", "description": "For search" }
                }
            }),
        ),
    ]
}

/// How long a tool call waits for another process's lease on its device.
const LEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A tool result that is an error the model reads (masked).
fn tool_error(msg: &str) -> Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": velocity_common::redact::redact(msg) }],
        "isError": true
    })
}

/// A required string argument.
fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| VelocityError::Config(format!("Missing '{key}' argument")))
}

fn write_response(stdout: &mut dyn Write, resp: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(resp)
        .map_err(|e| VelocityError::Internal(anyhow::anyhow!("Serialization error: {e}")))?;
    debug!("Sending: {json}");
    writeln!(stdout, "{json}")
        .map_err(|e| VelocityError::Internal(anyhow::anyhow!("Failed to write stdout: {e}")))?;
    stdout
        .flush()
        .map_err(|e| VelocityError::Internal(anyhow::anyhow!("Failed to flush stdout: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn server() -> McpServer {
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            Default::default(),
        ));
        McpServer::new(driver, "headless".into(), None)
    }

    /// U2: docs/MCP.md has a section for every tool, and says "Read-only"
    /// exactly for the read-only ones, so the doc can't drift from the code.
    #[tokio::test]
    async fn the_mcp_doc_lists_every_tool() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/MCP.md"),
        )
        .unwrap();
        let s = server().await;
        for t in s.tool_definitions() {
            let head = format!("### `{}`\n", t.name);
            let at = doc
                .find(&head)
                .unwrap_or_else(|| panic!("docs/MCP.md has no section for {}", t.name));
            let rest = &doc[at + head.len()..];
            let section = &rest[..rest.find("\n### ").unwrap_or(rest.len())];
            assert_eq!(
                section.contains("Read-only."),
                READ_ONLY_TOOLS.contains(&t.name.as_str()),
                "docs/MCP.md: {} read-only marking",
                t.name
            );
        }
    }

    fn result(r: &JsonRpcResponse) -> Value {
        serde_json::to_value(r).unwrap()["result"].clone()
    }

    #[tokio::test]
    async fn speaks_the_mcp_shapes_clients_expect() {
        let mut s = server().await;
        let list = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let tools = result(&list)["tools"].as_array().cloned().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "describe_failure"));
        assert!(tools.iter().all(|t| t.get("inputSchema").is_some()));

        // Notifications get no reply.
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none());

        // An unknown tool is a protocol error, -32602 (P-9).
        let bad = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope"}}"#,
            )
            .await
            .unwrap();
        let bad = serde_json::to_value(&bad).unwrap();
        assert_eq!(bad["error"]["code"], -32602, "{bad}");
        assert!(bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool: nope"));
        let ping = s
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)
            .await
            .unwrap();
        assert!(ping.error.is_none());
        let unknown = s
            .handle_line(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#)
            .await
            .unwrap();
        assert_eq!(unknown.error.map(|e| e.code), Some(-32601));
    }

    fn code(r: Option<JsonRpcResponse>) -> Option<i32> {
        r.and_then(|r| r.error).map(|e| e.code)
    }

    // VEL-086 / VEL-243: malformed requests get the right codes, and a
    // tool never runs without an id to answer.
    #[tokio::test]
    async fn malformed_requests_get_the_right_error_codes() {
        let mut s = server().await;
        let tap =
            r#""method":"tools/call","params":{"name":"press_key","arguments":{"key":"back"}}"#;
        let null_id = format!(r#"{{"jsonrpc":"2.0","id":null,{tap}}}"#);
        assert_eq!(code(s.handle_line(&null_id).await), Some(-32600));
        let no_id = format!(r#"{{"jsonrpc":"2.0",{tap}}}"#);
        assert!(s.handle_line(&no_id).await.is_none());
        assert!(s.recorder.is_empty(), "no tool ran: {:?}", s.recorder.steps);
        assert_eq!(
            code(
                s.handle_line(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#)
                    .await
            ),
            Some(-32600)
        );
        assert_eq!(
            code(s.handle_line(r#"{"jsonrpc":"2.0","id":1}"#).await),
            Some(-32600)
        );
        assert_eq!(
            code(s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"#).await),
            Some(-32700)
        );
        assert_eq!(
            code(
                s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#)
                    .await
            ),
            Some(-32602)
        );
    }

    fn lines(out: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(out)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // VEL-084: invalid UTF-8 and an overlong line don't end the server.
    #[tokio::test]
    async fn bad_bytes_and_long_lines_are_answered_not_fatal() {
        let mut s = server().await;
        let mut input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\xfe\"}\n".to_vec();
        input.extend(b"\xc3\x28garbage\n");
        input.extend(vec![b'x'; 64]);
        input.extend(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
        let (tx, rx) = tokio::sync::mpsc::channel(MAX_QUEUED);
        let mut cursor = std::io::Cursor::new(input);
        let mut buf = Vec::new();
        while let Some(long) = read_bounded_line(&mut cursor, &mut buf, 56).unwrap() {
            let line = if long {
                "\u{0}line too long".to_string()
            } else {
                String::from_utf8_lossy(&buf).into_owned()
            };
            tx.try_send(line).unwrap();
            buf.clear();
        }
        drop(tx);
        let mut out = Vec::new();
        s.serve(rx, &mut out).await.unwrap();
        let replies = lines(&out);
        assert_eq!(replies.len(), 4, "{replies:?}");
        assert_eq!(replies[1]["error"]["code"], -32700);
        assert_eq!(replies[2]["error"]["code"], -32700, "overlong line");
        assert_eq!(replies[3]["id"], 2);
        assert!(replies[3].get("result").is_some());
    }

    fn wait_call(id: u64, timeout_ms: u64) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"wait_for_element","arguments":{{"selector":{{"text":"never-there"}},"timeout_ms":{timeout_ms}}}}}}}"#
        )
    }

    // VEL-085: ping is answered while a long call runs, and a cancel stops it.
    #[tokio::test]
    async fn ping_and_cancel_are_not_blocked_by_a_running_call() {
        let mut s = server().await;
        // An empty screen, so the wait polls until its timeout.
        s.driver.launch_app("headless", "app", false).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(MAX_QUEUED);
        tx.try_send(wait_call(1, 1_500)).unwrap();
        tx.try_send(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#.into())
            .unwrap();
        tx.try_send(wait_call(3, 30_000)).unwrap();
        tx.try_send(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}}"#
                .into(),
        )
        .unwrap();
        drop(tx);
        let mut out = Vec::new();
        let start = std::time::Instant::now();
        s.serve(rx, &mut out).await.unwrap();
        let ids: Vec<Value> = lines(&out).iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            [serde_json::json!(2), serde_json::json!(1)],
            "ping first; the cancelled call gets no reply"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "cancel stopped the 30 s wait"
        );
    }

    // Review test note: the cancel targets the request that is running.
    #[tokio::test]
    async fn cancelling_the_running_call_stops_it() {
        let mut s = server().await;
        s.driver.launch_app("headless", "app", false).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(MAX_QUEUED);
        tx.try_send(wait_call(1, 30_000)).unwrap();
        let start = std::time::Instant::now();
        let feeder = async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send(
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#
                    .into(),
            )
            .await
            .unwrap();
            tx.send(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#.into())
                .await
                .unwrap();
        };
        let mut out = Vec::new();
        let (served, ()) = tokio::join!(s.serve(rx, &mut out), feeder);
        served.unwrap();
        let ids: Vec<Value> = lines(&out).iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            [serde_json::json!(2)],
            "the cancelled call has no reply"
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    // Review MINOR 8: after stdin EOF a queued tools/call is answered, not run.
    #[tokio::test]
    async fn queued_calls_are_not_run_after_input_closes() {
        let mut s = server().await;
        s.driver.launch_app("headless", "app", false).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(MAX_QUEUED);
        tx.try_send(wait_call(1, 400)).unwrap();
        tx.try_send(wait_call(2, 30_000)).unwrap();
        tx.try_send(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#.into())
            .unwrap();
        drop(tx);
        let mut out = Vec::new();
        let start = std::time::Instant::now();
        s.serve(rx, &mut out).await.unwrap();
        let replies = lines(&out);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "the queued 30 s wait never ran"
        );
        let two = replies
            .iter()
            .find(|r| r["id"] == 2)
            .expect("id 2 answered");
        assert!(
            two["error"]["message"].as_str().unwrap().contains("closed"),
            "{two}"
        );
        assert!(replies
            .iter()
            .any(|r| r["id"] == 3 && r.get("result").is_some()));
    }

    // mcp-secret: `type_text {secret: NAME}` types the env value, never
    // returns it, and the export writes `${NAME}`.
    #[tokio::test]
    async fn secret_by_name_is_typed_not_echoed_and_exported_as_env() {
        const VALUE: &str = "hunter2-correct-horse";
        std::env::set_var("VELOCITY_MCP_TEST_LOGIN_PW", VALUE);
        std::env::set_var("VELOCITY_MCP_TEST_AWS_SECRET_KEY", "exfil-me-123");
        // Review MAJOR 4: only listed names; anything else is refused.
        std::env::set_var("VELOCITY_SECRETS", "VELOCITY_MCP_TEST_LOGIN_PW");
        let dir = std::env::temp_dir().join(format!("velmcp-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let layout = dir.join("main.xml");
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
            velocity_common::Platform::Android,
            config,
        ));
        driver.launch_app("headless", "app", false).await.unwrap();
        let mut s = McpServer::new(driver.clone(), "headless".into(), None);
        let call = |name: &str, args: Value| serde_json::json!({ "name": name, "arguments": args });
        let typed = s
            .handle_tools_call(&call(
                "type_text",
                serde_json::json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_LOGIN_PW" }),
            ))
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
            .handle_tools_call(&call(
                "get_element",
                serde_json::json!({ "selector": { "id": "pw" } }),
            ))
            .await
            .unwrap();
        assert!(!got.to_string().contains(VALUE), "{got}");
        let (yaml, n) = session_flow_yaml("app", "t", &s.recorder);
        assert_eq!(n, 1);
        assert!(yaml.contains("${VELOCITY_MCP_TEST_LOGIN_PW}"), "{yaml}");
        assert!(!yaml.contains(VALUE));
        let suite = velocity_core::parser::parse_suite_from_str(&yaml).unwrap();
        velocity_core::validate_suite(&suite).unwrap();
        // Unset, both, or a bad name: a clear error that names no value.
        for args in [
            serde_json::json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_UNSET_X" }),
            serde_json::json!({ "selector": { "id": "pw" }, "secret": "A", "text": "b" }),
            serde_json::json!({ "selector": { "id": "pw" }, "secret": "$(id)" }),
            serde_json::json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_AWS_SECRET_KEY" }),
        ] {
            let r = s.handle_tools_call(&call("type_text", args)).await.unwrap();
            assert_eq!(r["isError"], true, "{r}");
            assert!(!r.to_string().contains("exfil-me-123"), "{r}");
        }
        let r = s
            .handle_tools_call(&call(
                "type_text",
                serde_json::json!({ "selector": { "id": "pw" }, "secret": "VELOCITY_MCP_TEST_AWS_SECRET_KEY" }),
            ))
            .await
            .unwrap();
        assert!(r.to_string().contains("VELOCITY_SECRETS"), "{r}");
        let tree = driver.get_hierarchy("headless").await.unwrap();
        assert!(!format!("{tree:?}").contains("exfil-me-123"), "not typed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // P8: list_elements hands out refs; a ref is resolved to a selector built
    // in code (and that selector, not the ref, is what gets exported).
    #[tokio::test]
    async fn element_refs_resolve_record_and_expire() {
        let dir = std::env::temp_dir().join(format!("velmcp-refs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let layout = dir.join("main.xml");
        std::fs::write(
            &layout,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
                android:layout_width="match_parent" android:layout_height="match_parent"
                android:orientation="vertical">
              <Button android:id="@+id/ok" android:layout_width="200dp" android:layout_height="48dp" android:text="OK"/>
              <Button android:layout_width="200dp" android:layout_height="48dp" android:text="Buy"/>
              <Button android:layout_width="200dp" android:layout_height="48dp" android:text="Buy"/>
            </LinearLayout>"#,
        )
        .unwrap();
        let config = velocity_headless::HeadlessConfig {
            app_path: Some(layout.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            config,
        ));
        driver.launch_app("headless", "app", false).await.unwrap();
        let mut s = McpServer::new(driver, "headless".into(), None);
        let call = |name: &str, args: Value| serde_json::json!({ "name": name, "arguments": args });
        let text = |v: &Value| -> Value {
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap_or(Value::Null)
        };
        let listed = s
            .handle_tools_call(&call(
                "list_elements",
                serde_json::json!({ "filter": "Buy" }),
            ))
            .await
            .unwrap();
        let listed = text(&listed);
        let second = listed["elements"][1]["ref"].as_str().unwrap().to_string();
        let tapped = s
            .handle_tools_call(&call(
                "tap",
                serde_json::json!({ "selector": { "ref": second } }),
            ))
            .await
            .unwrap();
        assert!(tapped.get("isError").is_none(), "{tapped}");
        let (yaml, n) = session_flow_yaml("app", "t", &s.recorder);
        assert_eq!(n, 1);
        // Headless gives id-less nodes a synthetic, positional id; the ref
        // records the indexed text instead (review m2), never the ref.
        assert!(yaml.contains("- tap:") && !yaml.contains("ref"), "{yaml}");
        assert!(
            !yaml.contains("node_"),
            "a positional id was exported: {yaml}"
        );
        let recorded: velocity_common::Selector =
            serde_json::from_value(s.recorder.steps[0].args["selector"].clone()).unwrap();
        let tree = s.driver.get_hierarchy("headless").await.unwrap();
        let hit = velocity_common::selector_match::find_in_tree(&tree, &recorded, None).unwrap();
        assert_eq!(
            hit.platform_id, listed["elements"][1]["platform_id"],
            "{yaml}"
        );

        // A new listing retires the old refs.
        s.handle_tools_call(&call("list_elements", serde_json::json!({})))
            .await
            .unwrap();
        let old = s
            .handle_tools_call(&call(
                "tap",
                serde_json::json!({ "selector": { "ref": second } }),
            ))
            .await
            .unwrap();
        assert_eq!(old["isError"], true, "{old}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Review MAJOR-2: identical "Add to cart" rows (same resource-id, as in
    // any list); after a scroll the old ref must be reported stale, not tap
    // whichever row is now second.
    #[tokio::test]
    async fn a_ref_is_stale_after_a_scroll() {
        let dir = std::env::temp_dir().join(format!("velmcp-twins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let layout = dir.join("main.xml");
        let rows: String = (0..30)
            .map(|_| {
                r#"<Button android:id="@+id/add" android:layout_width="match_parent" android:layout_height="100dp" android:text="Add to cart"/>"#
            })
            .collect();
        std::fs::write(
            &layout,
            format!(
                r#"<ScrollView xmlns:android="http://schemas.android.com/apk/res/android"
                    android:layout_width="match_parent" android:layout_height="match_parent">
                  <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content"
                      android:orientation="vertical">{rows}</LinearLayout>
                </ScrollView>"#
            ),
        )
        .unwrap();
        let config = velocity_headless::HeadlessConfig {
            app_path: Some(layout.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            config,
        ));
        driver.launch_app("headless", "app", false).await.unwrap();
        let mut s = McpServer::new(driver, "headless".into(), None);
        let call = |name: &str, args: Value| serde_json::json!({ "name": name, "arguments": args });
        let listed = s
            .handle_tools_call(&call(
                "list_elements",
                serde_json::json!({ "filter": "Add to cart" }),
            ))
            .await
            .unwrap();
        let listed: Value =
            serde_json::from_str(listed["content"][0]["text"].as_str().unwrap()).unwrap();
        let second = listed["elements"][1]["ref"].as_str().unwrap().to_string();
        let swiped = s
            .handle_tools_call(&call("swipe", serde_json::json!({ "direction": "up" })))
            .await
            .unwrap();
        assert!(swiped.get("isError").is_none(), "{swiped}");
        let tapped = s
            .handle_tools_call(&call(
                "tap",
                serde_json::json!({ "selector": { "ref": second } }),
            ))
            .await
            .unwrap();
        assert_eq!(tapped["isError"], true, "{tapped}");
        let msg = tapped["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("stale"), "{msg}");
        assert!(s.recorder.steps.iter().all(|r| r.tool != "tap"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `Cursor` that shares how far it has been consumed.
    struct Counted {
        inner: std::io::Cursor<Vec<u8>>,
        pos: Arc<std::sync::atomic::AtomicU64>,
    }

    impl std::io::Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = std::io::Read::read(&mut self.inner, buf)?;
            self.pos
                .store(self.inner.position(), std::sync::atomic::Ordering::SeqCst);
            Ok(n)
        }
    }

    impl BufRead for Counted {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.inner.fill_buf()
        }
        fn consume(&mut self, amt: usize) {
            self.inner.consume(amt);
            self.pos
                .store(self.inner.position(), std::sync::atomic::Ordering::SeqCst);
        }
    }

    // Review MINOR 9: the reader stops reading ahead once MAX_QUEUED lines
    // wait. (The old check, `rx.len() <= MAX_QUEUED`, held by construction
    // for a bounded channel: it is how far the reader got that matters.)
    #[tokio::test]
    async fn the_reader_does_not_read_unboundedly_ahead() {
        let input: Vec<u8> = (0..1_000)
            .flat_map(|i| {
                format!("{{\"jsonrpc\":\"2.0\",\"id\":{i},\"method\":\"ping\"}}\n").into_bytes()
            })
            .collect();
        let total = input.len() as u64;
        let pos = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let rx = spawn_line_reader(Counted {
            inner: std::io::Cursor::new(input),
            pos: pos.clone(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let read = pos.load(std::sync::atomic::Ordering::SeqCst);
        // At most the queue plus the line in hand: well under half.
        assert!(
            read < total / 2,
            "the reader consumed {read} of {total} bytes with {} queued",
            rx.len()
        );
    }

    async fn launched_server(tag: &str) -> McpServer {
        let dir = std::env::temp_dir().join(format!("velmcp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let layout = dir.join("main.xml");
        std::fs::write(
            &layout,
            r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
                android:layout_width="match_parent" android:layout_height="match_parent"
                android:orientation="vertical">
              <Button android:id="@+id/ok" android:layout_width="200dp" android:layout_height="48dp" android:text="OK"/>
            </LinearLayout>"#,
        )
        .unwrap();
        let config = velocity_headless::HeadlessConfig {
            app_path: Some(layout.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            config,
        ));
        driver.launch_app("headless", "app", false).await.unwrap();
        McpServer::new(driver, "headless".into(), None)
    }

    // P-2 / NB-3: the screenshot is an image block, and redaction (which runs
    // over text) never touches its data.
    #[tokio::test]
    async fn screenshot_is_image_content_and_not_redacted() {
        use base64::Engine;
        let mut s = launched_server("shot").await;
        let png = s.driver.screenshot("headless").await.unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // A "secret" that occurs inside the image's base64.
        velocity_common::redact::register_secret(&b64[40..56]);
        let r = s
            .handle_tools_call(&serde_json::json!({ "name": "screenshot", "arguments": {} }))
            .await
            .unwrap();
        let content = r["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "{r}");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");
        assert_eq!(content[1]["data"].as_str().unwrap(), b64);
    }

    #[tokio::test]
    async fn screenshot_text_block_has_no_base64() {
        let mut s = launched_server("shot-text").await;
        let r = s
            .handle_tools_call(&serde_json::json!({ "name": "screenshot", "arguments": {} }))
            .await
            .unwrap();
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < 200, "{text}");
        assert!(!text.contains("\"data\""), "{text}");
    }

    // P-7: the compact listing is one line per element with a ref.
    #[tokio::test]
    async fn list_elements_text_format_is_one_line_per_ref() {
        let mut s = launched_server("compact").await;
        let r = s
            .handle_tools_call(&serde_json::json!({
                "name": "list_elements",
                "arguments": { "format": "text" }
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        let listing = v["elements"].as_str().unwrap();
        assert_eq!(v["count"], 2, "{listing}");
        let line = listing.lines().nth(1).unwrap();
        assert!(line.starts_with("e2 Button \"OK\" id=ok @("), "{listing}");
        // S1: the role `role:` matches is listed.
        assert!(line.ends_with(" role=button"), "{listing}");
        // The ref works.
        let tapped = s
            .handle_tools_call(&serde_json::json!({
                "name": "tap",
                "arguments": { "selector": { "ref": "e2" } }
            }))
            .await
            .unwrap();
        assert!(tapped.get("isError").is_none(), "{tapped}");
    }

    // VEL-S16: read-only mode reads artifacts only under cwd / outputDir.
    #[tokio::test]
    async fn read_only_describe_failure_stays_under_the_artifacts_dirs() {
        let mut s = server().await.read_only(true);
        let call = |dir: &str| serde_json::json!({ "name": "describe_failure", "arguments": { "artifacts_dir": dir } });
        let r = s.handle_tools_call(&call("/etc")).await.unwrap();
        assert_eq!(r["isError"], true, "{r}");
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("outside"),
            "{r}"
        );
        // Under the working directory it is allowed (and finds no trace).
        let r = s.handle_tools_call(&call(".")).await.unwrap();
        assert!(
            !r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("outside"),
            "{r}"
        );
    }

    /// D-33: `server/discover` lists every revision, the tools capability,
    /// instructions and cache hints, in either era.
    #[tokio::test]
    async fn server_discover_lists_versions_and_instructions() {
        let mut s = server().await;
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#)
            .await
            .unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("error").is_none(), "{v}");
        let d = &v["result"];
        assert_eq!(d["supportedVersions"][0], "2026-07-28", "{d}");
        assert!(d["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&"2024-11-05".into()));
        assert!(d["capabilities"]["tools"].is_object(), "{d}");
        assert!(d["instructions"].as_str().unwrap().contains("snapshot"));
        assert!(d["ttlMs"].as_u64().unwrap() > 0);
        assert_eq!(d["cacheScope"], "private");
        assert_eq!(d["resultType"], "complete");
        // serverInfo where the spec surely has it (discover's own field),
        // not under a guessed `_meta` key (review-d7 m6).
        assert_eq!(d["serverInfo"]["name"], "velocity-mcp");
        assert!(
            !d["_meta"]
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k.contains("serverInfo")),
            "{d}"
        );
        // Asked with a version it doesn't speak, discover still answers.
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2099-01-01"}}}"#)
            .await
            .unwrap();
        assert!(r.error.is_none());
    }

    /// D-33: a 2026-07-28 request (no `initialize`) gets `resultType` and
    /// the server's `_meta`; its own `_meta` result keys are kept; tools/list
    /// carries cache hints.
    #[tokio::test]
    async fn discover_then_modern_call() {
        let mut s = server().await;
        let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"progressToken":7}"#;
        let list = s
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{meta}}}}}"#
            ))
            .await
            .unwrap();
        let l = result(&list);
        assert!(!l["tools"].as_array().unwrap().is_empty(), "{l}");
        assert_eq!(l["resultType"], "complete");
        assert_eq!(l["cacheScope"], "private");
        assert!(l["ttlMs"].as_u64().is_some());
        assert_eq!(l["_meta"][META_PROTOCOL_VERSION], "2026-07-28");
        assert_eq!(
            l["_meta"].as_object().unwrap().len(),
            1,
            "only the sourced protocolVersion key: {l}"
        );
        let call = s
            .handle_line(&format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"list_devices","arguments":{{}},{meta}}}}}"#
            ))
            .await
            .unwrap();
        let c = result(&call);
        assert!(c["content"].is_array(), "{c}");
        assert_eq!(c["resultType"], "complete");
        // A result's own `_meta` survives the server's additions.
        let merged = modern_result(serde_json::json!({ "_meta": { "x.example/k": 1 } }));
        assert_eq!(merged["_meta"]["x.example/k"], 1);
        assert_eq!(merged["_meta"][META_PROTOCOL_VERSION], "2026-07-28");
        // Legacy requests are unchanged: no resultType, no _meta.
        let legacy = s
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .await
            .unwrap();
        let l = result(&legacy);
        assert!(
            l.get("resultType").is_none() && l.get("_meta").is_none(),
            "{l}"
        );
    }

    /// D-33: a version the server doesn't speak is -32022 with the list.
    #[tokio::test]
    async fn unsupported_meta_version_is_32022() {
        let mut s = server().await;
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01"}}}"#)
            .await
            .unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["error"]["code"], -32022, "{v}");
        assert_eq!(v["error"]["data"]["requested"], "2030-01-01");
        assert_eq!(v["error"]["data"]["supported"][0], "2026-07-28");
        assert_eq!(v["id"], 5);
        // A legacy revision in `_meta` is accepted.
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":6,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-06-18"}}}"#)
            .await
            .unwrap();
        assert!(r.error.is_none());
        // `_meta` that isn't an object is invalid params.
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"_meta":"x"}}"#)
            .await
            .unwrap();
        assert_eq!(r.error.map(|e| e.code), Some(-32602));
    }

    /// The legacy handshake still works alongside the modern era.
    #[tokio::test]
    async fn legacy_initialize_still_works() {
        let mut s = server().await;
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28"}}"#)
            .await
            .unwrap();
        // 2026-07-28 has no initialize: the newest handshake revision.
        assert_eq!(result(&r)["protocolVersion"], "2025-11-25");
        assert_eq!(result(&r)["serverInfo"]["name"], "velocity-mcp");
    }

    // P-9: the client's version is echoed when supported, else the newest.
    #[tokio::test]
    async fn initialize_echoes_supported_version() {
        let mut s = server().await;
        for (asked, want) in [
            ("2025-06-18", "2025-06-18"),
            ("2024-11-05", "2024-11-05"),
            ("2025-11-25", "2025-11-25"),
            ("1999-01-01", "2025-11-25"),
        ] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{asked}"}}}}"#
            );
            let r = s.handle_line(&line).await.unwrap();
            assert_eq!(result(&r)["protocolVersion"], want, "{asked}");
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params() {
        let mut s = server().await;
        let r = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"tapp"}}"#,
            )
            .await
            .unwrap();
        let r = serde_json::to_value(&r).unwrap();
        assert_eq!(r["error"]["code"], -32602, "{r}");
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("did you mean `tap`"));
        // An alias still resolves.
        let r = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"take_screenshot"}}"#,
            )
            .await
            .unwrap();
        assert!(serde_json::to_value(&r).unwrap().get("error").is_none());
    }
}

#[cfg(test)]
mod t2_tests {
    use super::*;

    async fn launched(xml_children: &str) -> (McpServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("main.xml");
        std::fs::write(
            &layout,
            format!(
                r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
                android:layout_width="match_parent" android:layout_height="match_parent"
                android:orientation="vertical">{xml_children}</LinearLayout>"#
            ),
        )
        .unwrap();
        let config = velocity_headless::HeadlessConfig {
            app_path: Some(layout.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            config,
        ));
        driver.launch_app("headless", "app", false).await.unwrap();
        let s = McpServer::new(driver, "headless".into(), None)
            .with_platform(velocity_common::Platform::Android)
            .with_tasks(TaskManager::new(dir.path().join("tasks"), vec![]));
        (s, dir)
    }

    async fn call(s: &mut McpServer, name: &str, args: Value) -> Value {
        s.handle_tools_call(&serde_json::json!({ "name": name, "arguments": args }))
            .await
            .unwrap()
    }

    fn text(v: &Value) -> String {
        v["content"][0]["text"].as_str().unwrap().to_string()
    }

    const FORM: &str = r#"
      <TextView android:layout_width="200dp" android:layout_height="48dp" android:text="Sign in"/>
      <EditText android:id="@+id/email" android:layout_width="200dp" android:layout_height="48dp"/>
      <Button android:id="@+id/go" android:layout_width="200dp" android:layout_height="48dp" android:text="Continue"/>
    "#;

    #[tokio::test]
    async fn snapshot_refs_drive_tools_and_delta_shows_changes() {
        let (mut s, _dir) = launched(FORM).await;
        let full = text(&call(&mut s, "snapshot", serde_json::json!({})).await);
        assert!(
            full.starts_with("# "),
            "raw text, not a JSON string: {full}"
        );
        let email = full
            .lines()
            .find(|l| l.contains("#email"))
            .and_then(|l| l.split_whitespace().next())
            .unwrap()
            .to_string();
        let typed = call(
            &mut s,
            "type_text",
            serde_json::json!({ "ref": email, "text": "qa@velocity.dev" }),
        )
        .await;
        assert!(typed.get("isError").is_none(), "{typed}");
        let delta = text(&call(&mut s, "snapshot", serde_json::json!({ "mode": "delta" })).await);
        assert!(
            delta.contains("~ ") && delta.contains("qa@velocity.dev"),
            "{delta}"
        );
        assert!(
            !delta.contains("Continue"),
            "unchanged lines are left out: {delta}"
        );
        // Selector as a bare ref string works too; an unknown ref is an error.
        let tapped = call(&mut s, "tap", serde_json::json!({ "selector": "@e3" })).await;
        assert!(tapped.get("isError").is_none(), "{tapped}");
        let bad = call(&mut s, "tap", serde_json::json!({ "ref": "@e99" })).await;
        assert_eq!(bad["isError"], true);
        assert!(text(&bad).contains("snapshot"), "{bad}");
        let out = call(
            &mut s,
            "export_session_flow",
            serde_json::json!({ "app_id": "com.app" }),
        )
        .await;
        let v: Value = serde_json::from_str(&text(&out)).unwrap();
        let yaml = v["yaml"].as_str().unwrap();
        assert!(!yaml.contains("@e"), "refs are never exported: {yaml}");
        let suite = velocity_core::parser::parse_suite_from_str(yaml).unwrap();
        velocity_core::validate_suite(&suite).unwrap();
        assert_eq!(v["steps"], 2, "{yaml}");
    }

    #[tokio::test]
    async fn skeleton_comes_from_the_screen_when_nothing_was_recorded() {
        let (mut s, _dir) = launched(FORM).await;
        let out = call(
            &mut s,
            "generate_test_skeleton",
            serde_json::json!({ "description": "log in", "app_id": "com.app" }),
        )
        .await;
        let v: Value = serde_json::from_str(&text(&out)).unwrap();
        assert_eq!(v["source"], "screen", "{v}");
        let yaml = v["yaml"].as_str().unwrap();
        assert!(yaml.contains("#go"), "elements listed as comments: {yaml}");
        let suite = velocity_core::parser::parse_suite_from_str(yaml).unwrap();
        velocity_core::validate_suite(&suite).unwrap();
        assert!(suite.tests[0].steps.len() >= 2, "{yaml}");
    }

    #[tokio::test]
    async fn assert_not_visible_and_scroll_run_as_steps() {
        let (mut s, _dir) = launched(FORM).await;
        let gone = call(
            &mut s,
            "assert_not_visible",
            serde_json::json!({ "selector": { "text": "Nope" }, "app_id": "app" }),
        )
        .await;
        assert!(gone.get("isError").is_none(), "{gone}");
        let there = call(
            &mut s,
            "assert_not_visible",
            serde_json::json!({ "selector": { "text": "Continue" }, "timeout_ms": 300 }),
        )
        .await;
        assert_eq!(there["isError"], true, "{there}");
        let scrolled = call(
            &mut s,
            "scroll",
            serde_json::json!({ "until_visible": { "text": "Continue" } }),
        )
        .await;
        assert!(scrolled.get("isError").is_none(), "{scrolled}");
        assert!(s.recorder.steps.iter().any(|r| r.tool == "scroll"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_task_returns_at_once_and_status_tracks_it() {
        let (s, dir) = launched(FORM).await;
        let bin = crate::test_util::script(
            "fake-velocity-pass",
            "#!/bin/sh\nart=\"\"\nwhile [ $# -gt 0 ]; do [ \"$1\" = --artifacts-dir ] && art=\"$2\"; shift; done\nmkdir -p \"$art\"\necho '{\"event\":\"stepStarted\",\"test\":\"t\",\"index\":0,\"total\":2,\"step\":\"launchApp\"}' >> \"$VELOCITY_EVENTS_FILE\"\nsleep 1\nprintf '%s' '{\"total\":1,\"passed\":1,\"failed\":0,\"skipped\":0,\"retried\":0,\"duration\":{\"secs\":1,\"nanos\":0},\"tests\":[{\"test_name\":\"t\",\"status\":\"passed\",\"duration\":{\"secs\":1,\"nanos\":0},\"steps\":[],\"retries\":0,\"error_message\":null,\"screenshots\":[]}],\"shard_index\":null,\"shard_total\":null}' > \"$art/results.json\"\n",
        );
        let _ = dir;
        // The script is the binary; it is reused across runs (see test_util).
        let mut s = s.with_velocity_bin(bin);
        let yaml = "appId: com.app\ntests:\n  - name: t\n    steps:\n      - launchApp: {}\n";
        let started = std::time::Instant::now();
        let out = call(&mut s, "run_task", serde_json::json!({ "yaml": yaml })).await;
        assert!(out.get("isError").is_none(), "{out}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "non-blocking"
        );
        let v: Value = serde_json::from_str(&text(&out)).unwrap();
        let id = v["task_id"].as_str().unwrap().to_string();
        // The device is busy meanwhile.
        let again = call(&mut s, "run_task", serde_json::json!({ "yaml": yaml })).await;
        assert_eq!(again["isError"], true, "{again}");
        let done = call(
            &mut s,
            "task_status",
            serde_json::json!({ "task_id": id, "wait_ms": 100 }),
        )
        .await;
        let mid: Value = serde_json::from_str(&text(&done)).unwrap();
        assert!(
            mid["state"] == "running" || mid["state"] == "passed",
            "{mid}"
        );
        s.tasks
            .wait(&id, std::time::Duration::from_secs(90))
            .await
            .unwrap();
        let fin: Value = serde_json::from_str(&text(
            &call(&mut s, "task_status", serde_json::json!({ "task_id": id })).await,
        ))
        .unwrap();
        assert_eq!(
            fin["state"],
            "passed",
            "{fin}\nlog: {:?}\nps: {}",
            std::fs::read_to_string(fin["log"].as_str().unwrap()),
            String::from_utf8_lossy(
                &std::process::Command::new("ps")
                    .args(["-A", "-o", "pid,ppid,pgid,stat,etime,command"])
                    .output()
                    .unwrap()
                    .stdout
            )
            .lines()
            .filter(|l| l.contains("velocity") && (l.contains("sh") || l.contains("sleep")))
            .collect::<Vec<_>>()
            .join("\n")
        );
        assert_eq!(fin["summary"]["passed"], 1);
        let inj = call(
            &mut s,
            "task_inject",
            serde_json::json!({ "task_id": id, "instruction": "x" }),
        )
        .await;
        assert_eq!(inj["isError"], true);
        // An invalid inline suite is refused before anything runs.
        let bad = call(
            &mut s,
            "run_task",
            serde_json::json!({ "yaml": "tests: [" }),
        )
        .await;
        assert_eq!(bad["isError"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_task_with_a_goal_explores_and_takes_instructions() {
        let (s, _dir) = launched(FORM).await;
        // Records its argv, waits for one injected line, then reports.
        let bin = crate::test_util::script(
            "fake-velocity-explore-tool",
            "#!/bin/sh\nout=\"\"; inject=\"\"; all=\"$*\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --out) out=\"$2\"; shift;; --inject-file) inject=\"$2\"; shift;; esac; shift; done\necho \"$all\" > \"$out.argv\"\necho '{\"event\":\"planned\",\"turn\":1,\"op\":\"tap\",\"target\":\"Continue\"}' >> \"$VELOCITY_EVENTS_FILE\"\ni=0; while [ $i -lt 100 ] && ! grep -q . \"$inject\"; do sleep 0.1; i=$((i+1)); done\nprintf 'appId: com.app\\n' > \"$out\"\nprintf '{\"report\":{\"outcome\":\"success\",\"reason\":\"done\",\"turns\":1,\"steps\":[{}],\"modelCalls\":0},\"replay\":{\"runs\":[{\"passed\":true,\"ms\":5}],\"modelCalls\":0},\"files\":[\"%s\"]}' \"$out\"\n",
        );
        let mut s = s.with_velocity_bin(bin);
        let mixed = call(
            &mut s,
            "run_task",
            serde_json::json!({ "goal": "sign in", "yaml": "tests: []" }),
        )
        .await;
        assert_eq!(mixed["isError"], true, "{mixed}");
        let out = call(
            &mut s,
            "run_task",
            serde_json::json!({
                "goal": "sign in",
                "app_id": "com.app",
                "script": "plan.json",
                "data": { "EMAIL": "a@b.c" },
                "secret": ["PASSWORD"],
                "verify": "off",
                "max_steps": 5,
            }),
        )
        .await;
        assert!(out.get("isError").is_none(), "{out}");
        let v: Value = serde_json::from_str(&text(&out)).unwrap();
        let id = v["task_id"].as_str().unwrap().to_string();
        assert_eq!(v["kind"], "explore");
        let inj = call(
            &mut s,
            "task_inject",
            serde_json::json!({ "task_id": id, "instruction": "tap Continue" }),
        )
        .await;
        assert!(inj.get("isError").is_none(), "{inj}");
        s.tasks
            .wait(&id, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let fin: Value = serde_json::from_str(&text(
            &call(&mut s, "task_status", serde_json::json!({ "task_id": id })).await,
        ))
        .unwrap();
        assert_eq!(fin["state"], "passed", "{fin}");
        let yaml = fin["summary"]["explore"]["yaml"].as_str().unwrap();
        let argv = std::fs::read_to_string(format!("{yaml}.argv")).unwrap();
        for part in [
            "explore sign in --app-id com.app --json",
            "--data EMAIL=a@b.c",
            "--secret PASSWORD",
            "--verify off",
            "--max-steps 5",
            "--device headless",
        ] {
            assert!(argv.contains(part), "{part} in {argv}");
        }
        assert_eq!(fin["summary"]["explore"]["replay"]["passed"], 1);
        let inject = std::fs::read_to_string(
            std::path::Path::new(fin["dir"].as_str().unwrap()).join("inject.txt"),
        )
        .unwrap();
        assert_eq!(inject, "tap Continue\n");
    }

    #[tokio::test]
    async fn the_new_tools_are_listed_with_ref_targets() {
        let (s, _dir) = launched(FORM).await;
        let defs = s.tool_definitions();
        for name in [
            "snapshot",
            "launch_app",
            "stop_app",
            "scroll",
            "assert_not_visible",
            "run_task",
            "task_status",
            "task_stop",
            "task_inject",
            "diagnose",
            "inspect_trace",
        ] {
            assert!(defs.iter().any(|d| d.name == name), "{name}");
        }
        let tap = defs.iter().find(|d| d.name == "tap").unwrap();
        assert!(tap.input_schema["properties"]["ref"].is_object());
        assert!(!tap.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&"selector".into()));
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;

    fn known() -> Vec<String> {
        [
            "tap",
            "type_text",
            "get_screen_summary",
            "press_key",
            "list_elements",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn tool_names_are_resolved_leniently_with_a_hint_otherwise() {
        assert_eq!(
            resolve_tool_name("typeText", &known()).unwrap(),
            "type_text"
        );
        assert_eq!(
            resolve_tool_name("Type-Text", &known()).unwrap(),
            "type_text"
        );
        assert_eq!(resolve_tool_name("click", &known()).unwrap(), "tap");
        assert_eq!(
            resolve_tool_name("summary", &known()).unwrap(),
            "get_screen_summary"
        );
        let err = resolve_tool_name("list_elementz", &known()).unwrap_err();
        assert!(err.contains("list_elements"), "{err}");
    }

    #[test]
    fn annotations_mark_what_changes_the_device() {
        assert_eq!(annotations("screenshot")["readOnlyHint"], true);
        assert_eq!(annotations("tap")["readOnlyHint"], false);
        assert_eq!(annotations("tap")["destructiveHint"], false);
        assert_eq!(annotations("run_test")["destructiveHint"], true);
    }

    #[test]
    fn a_recorded_session_exports_as_a_suite_that_parses() {
        let calls = vec![
            (
                "tap".to_string(),
                serde_json::json!({ "selector": { "id": "nav_login" } }),
            ),
            (
                "type_text".to_string(),
                serde_json::json!({ "selector": { "id": "email" }, "text": "qa@velocity.dev" }),
            ),
            (
                "press_key".to_string(),
                serde_json::json!({ "key": "Return" }),
            ),
            (
                "swipe".to_string(),
                serde_json::json!({ "direction": "UP" }),
            ),
            ("tap".to_string(), serde_json::json!({ "bogus": 1 })),
        ];
        let mut rec = Recorder::default();
        for (tool, args) in &calls {
            rec.record(tool, args);
        }
        let (yaml, n) = session_flow_yaml("com.velocity.lab", "login by agent", &rec);
        assert_eq!(n, 4, "the unusable call is skipped");
        let suite = velocity_core::parser::parse_suite_from_str(&yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        velocity_core::validate_suite(&suite).unwrap();
        assert_eq!(suite.app_id, "com.velocity.lab");
        assert_eq!(suite.tests[0].steps.len(), 5, "launchApp + 4");
    }

    /// review-d6 m3: an agent's `escape` on iOS was back before D-9 and a
    /// config error after it. It presses back again, with a note.
    #[tokio::test]
    async fn escape_on_ios_presses_back_with_a_note() {
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Ios,
            Default::default(),
        ));
        let mut s = McpServer::new(driver, "headless".into(), None);
        let out = s
            .handle_tools_call(&serde_json::json!({
                "name": "press_key",
                "arguments": { "key": "esc" }
            }))
            .await
            .unwrap();
        assert!(out.get("isError").is_none(), "{out}");
        let text = out.to_string();
        assert!(text.contains("pressed back"), "{out}");
        assert!(text.contains("iOS has no Escape key"), "{out}");
    }
}
