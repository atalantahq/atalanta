//! The agent commands: drive a device one shell command at a time.
//!
//! `velocity snapshot`, `velocity tap @e7`, `velocity type @e3 "…"`, … are
//! for coding agents that run shell commands (and for people at a
//! terminal). Each command runs one MCP tool in process (`McpServer::call`),
//! so leases, refs, the recording, secret masking and the read-only rules
//! are exactly the MCP server's. Between commands the session (its device,
//! refs and recording) is saved under `~/.velocity/agent/`, keyed by
//! `--session` or by the git worktree, so `@e7` from one command means the
//! same element in the next and `velocity export` writes everything done
//! since the session started as a test.
//!
//! An action waits for the screen to settle and prints what changed (a
//! delta snapshot), so its refs are current without another snapshot.
//! Failures print a stable code, whether a retry is safe and what to do
//! next (`velocity_mcp::agent_error`); `--json` prints one envelope
//! instead: `{"v":1,"ok":…,"command":…,"session":…,"device":…,"data"|"error":…}`.
//! Exit status: 0 ok, 1 a failure the reply describes, 2 a malformed command
//! line (clap's).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Subcommand};
use serde_json::{json, Value};
use velocity_common::{DeviceState, Platform, PlatformDriver};
use velocity_mcp::{skills, McpServer};

/// Flags every agent command takes.
#[derive(Args, Clone, Debug, Default)]
pub struct Common {
    /// Device id (default: the session's device, else the first booted one)
    #[arg(short, long)]
    device: Option<String>,

    /// android, ios or web (default: from the device id or the session).
    /// Web commands run in `velocity daemon`, started if none is running,
    /// so the browser outlives each command
    #[arg(short, long)]
    platform: Option<String>,

    /// Session name: refs and the recording belong to it (default: this git
    /// worktree, else this directory). Also VELOCITY_SESSION
    #[arg(long)]
    session: Option<String>,

    /// Print one JSON object: {"v":1,"ok":…,"data"|"error":…}
    #[arg(long)]
    json: bool,

    /// A suite file: its appId is the default for launch, stop and export
    #[arg(short = 'c', long)]
    config: Option<String>,

    /// Drive a headless render of this Android layout XML (tests and demos)
    #[arg(long, hide = true)]
    headless_layout: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Agent: what is on screen, one line per element with a ref (@e7)
    Snapshot {
        /// Only what changed since the last snapshot
        #[arg(long)]
        delta: bool,
        /// Only elements inside this one (a ref: a dialog, a list)
        #[arg(long, value_name = "REF")]
        within: Option<String>,
        /// Only lines containing this text (any case)
        #[arg(long, value_name = "TEXT")]
        find: Option<String>,
        /// At most this many lines (the header then says it is incomplete)
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Add each element's bounds
        #[arg(long)]
        bounds: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Agent: tap an element (a ref like @e7, "text", id=…, or JSON)
    Tap {
        /// The element: a ref from `velocity snapshot` (@e7), visible text
        /// ("Sign in"), a field=value selector (id=login_button,
        /// accessibilityId=Submit), or a JSON selector ('{"text":"OK","index":1}')
        #[arg(value_name = "TARGET")]
        target: String,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: type into an element (text, --secret NAME, or --stdin)
    Type {
        /// The element: a ref from `velocity snapshot` (@e7), visible text
        /// ("Sign in"), a field=value selector (id=login_button,
        /// accessibilityId=Submit), or a JSON selector ('{"text":"OK","index":1}')
        #[arg(value_name = "TARGET")]
        target: String,
        /// The text (or use --secret / --stdin)
        text: Option<String>,
        /// Type the value of this environment variable (listed in
        /// VELOCITY_SECRETS); masked everywhere, exported as ${NAME}
        #[arg(long, value_name = "NAME", conflicts_with_all = ["text", "stdin"])]
        secret: Option<String>,
        /// Read the text from stdin, so it is not on the command line
        #[arg(long, conflicts_with = "text")]
        stdin: bool,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: press a key: back, home, enter, tab, escape, backspace, …
    Press {
        key: String,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: swipe the finger up, down, left or right
    Swipe {
        direction: String,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: scroll the content (down shows what is below), optionally
    /// until an element is visible
    Scroll {
        #[arg(default_value = "down")]
        direction: String,
        /// Keep scrolling until this target is on screen
        #[arg(long, value_name = "TARGET")]
        until: Option<String>,
        /// Give up after this many scrolls (with --until)
        #[arg(long, value_name = "N")]
        max: Option<u64>,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: launch an app (default: the suite's appId)
    Launch {
        app_id: Option<String>,
        /// Clear its data first
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: stop an app (default: the suite's appId)
    Stop {
        app_id: Option<String>,
        #[command(flatten)]
        act: ActFlags,
    },
    /// Agent: check an element is visible (or, with --not, that it is not)
    Assert {
        /// The element: a ref from `velocity snapshot` (@e7), visible text
        /// ("Sign in"), a field=value selector (id=login_button,
        /// accessibilityId=Submit), or a JSON selector ('{"text":"OK","index":1}')
        #[arg(value_name = "TARGET")]
        target: String,
        #[arg(long)]
        not: bool,
        /// How long to wait. Takes a unit (5s, 500ms); a bare number is
        /// milliseconds
        #[arg(long, value_name = "DURATION", value_parser = super::duration_arg::bare_millis)]
        timeout: Option<std::time::Duration>,
        #[command(flatten)]
        common: Common,
    },
    /// Agent: wait for an element to appear
    Wait {
        /// The element: a ref from `velocity snapshot` (@e7), visible text
        /// ("Sign in"), a field=value selector (id=login_button,
        /// accessibilityId=Submit), or a JSON selector ('{"text":"OK","index":1}')
        #[arg(value_name = "TARGET")]
        target: String,
        /// How long to wait. Takes a unit (5s, 500ms); a bare number is
        /// milliseconds
        #[arg(long, value_name = "DURATION", value_parser = super::duration_arg::bare_millis)]
        timeout: Option<std::time::Duration>,
        #[command(flatten)]
        common: Common,
    },
    /// Agent: save a screenshot and print its path
    Screenshot {
        #[command(flatten)]
        common: Common,
    },
    /// Agent: write this session's actions as a test (with an assertion
    /// after each screen change)
    Export {
        /// Write the YAML here instead of printing it
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// The test's name
        #[arg(long)]
        name: Option<String>,
        /// The app id (default: the suite's, else the launched app)
        #[arg(long)]
        app_id: Option<String>,
        /// Start a new recording afterwards
        #[arg(long)]
        clear: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Agent: show the session (device, refs, recorded steps); `reset`
    /// starts a new one, `list` shows them all
    Session {
        #[arg(value_parser = ["show", "reset", "list"], default_value = "show")]
        action: String,
        #[command(flatten)]
        common: Common,
    },
    /// Agent guides that match this binary: `velocity skills get cli`
    Skills {
        #[arg(value_parser = ["list", "get", "stub"], default_value = "list")]
        action: String,
        /// The guide to print (with `get`)
        name: Option<String>,
    },
}

/// Flags of the commands that act.
#[derive(Args, Clone, Debug, Default)]
pub struct ActFlags {
    /// Don't wait for the screen to settle or print what changed
    #[arg(long)]
    no_settle: bool,
    #[command(flatten)]
    common: Common,
}

/// One tool call: its name, arguments, and whether it may change the screen.
struct Call {
    command: &'static str,
    tool: &'static str,
    args: Value,
    acts: bool,
    settle: bool,
}

/// In the daemon: the client's stdin, for `type --stdin` (the daemon's own
/// stdin is not the agent's).
static FORWARDED_STDIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The daemon's browser for web agent commands: one Chrome for the
/// daemon's life, so a page opened by `launch` is still there for the next
/// `tap`. Kept apart from `velocity run --daemon`'s pool, whose drivers are
/// replaced when a suite's `web:` configuration differs.
static WEB_DRIVER: tokio::sync::Mutex<Option<Arc<dyn PlatformDriver>>> =
    tokio::sync::Mutex::const_new(None);

/// Close the daemon's browser (the daemon is stopping).
pub async fn cleanup_daemon_drivers() {
    if let Some(d) = WEB_DRIVER.lock().await.take() {
        d.cleanup().await;
    }
}

/// Run one agent command inside the daemon: `argv` as typed after
/// `velocity`, and the client's stdin when it used `--stdin`.
pub async fn execute_in_daemon(argv: &[String], stdin: Option<String>) -> i32 {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "velocity", no_binary_name = true)]
    struct AgentOnly {
        #[command(subcommand)]
        cmd: AgentCommand,
    }
    let parsed = match AgentOnly::try_parse_from(argv) {
        Ok(p) => p,
        Err(e) => {
            println!("{e}");
            return 2;
        }
    };
    *FORWARDED_STDIN.lock().unwrap_or_else(|e| e.into_inner()) = stdin;
    let code = match Box::pin(execute(parsed.cmd)).await {
        Ok(code) => code,
        Err(e) => {
            println!("error: {}", crate::redacted(&e));
            super::exit_code_for(&e)
        }
    };
    *FORWARDED_STDIN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    code
}

/// Whether this command drives the web: `-p web`, a `web` device id, or a
/// session that already drives the web. Read without the session's lock:
/// the daemon takes it.
fn targets_web(common: &Common) -> bool {
    if common.headless_layout.is_some() {
        return false;
    }
    if let Some(p) = common.platform.as_deref() {
        return p == "web";
    }
    if let Some(d) = common.device.as_deref() {
        return d == "web" || d.starts_with("web-");
    }
    let path = sessions_dir().join(format!("{}.json", session_key(common)));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| v["platform"] == "web")
}

/// Hand this command to `velocity daemon`, starting one if none answers.
fn forward_to_daemon(stdin: Option<String>) -> anyhow::Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let argv = crate::without_color_flag(argv);
    if let Some(code) = super::daemon::client_agent(argv.clone(), stdin.clone())? {
        return Ok(code);
    }
    super::daemon::spawn_background()?;
    match super::daemon::client_agent(argv, stdin)? {
        Some(code) => Ok(code),
        None => anyhow::bail!(
            "started `velocity daemon` but it does not answer; see ~/.velocity/daemon.log"
        ),
    }
}

/// The flags of the command, for the routing decision.
fn common_of(cmd: &AgentCommand) -> Option<&Common> {
    Some(match cmd {
        AgentCommand::Skills { .. } => return None,
        AgentCommand::Session { common, .. }
        | AgentCommand::Snapshot { common, .. }
        | AgentCommand::Assert { common, .. }
        | AgentCommand::Wait { common, .. }
        | AgentCommand::Screenshot { common }
        | AgentCommand::Export { common, .. } => common,
        AgentCommand::Tap { act, .. }
        | AgentCommand::Type { act, .. }
        | AgentCommand::Press { act, .. }
        | AgentCommand::Swipe { act, .. }
        | AgentCommand::Scroll { act, .. }
        | AgentCommand::Launch { act, .. }
        | AgentCommand::Stop { act, .. } => &act.common,
    })
}

pub async fn execute(cmd: AgentCommand) -> anyhow::Result<i32> {
    if !super::daemon::active() {
        if let Some(common) = common_of(&cmd).filter(|c| targets_web(c)) {
            if let Some(p) = common.platform.as_deref() {
                parse_platform(p)?;
            }
            let stdin = match &cmd {
                AgentCommand::Type { stdin: true, .. } => {
                    let mut t = String::new();
                    std::io::stdin().read_to_string(&mut t)?;
                    Some(t)
                }
                _ => None,
            };
            return forward_to_daemon(stdin);
        }
    }
    let (call, common) = match cmd {
        AgentCommand::Skills { action, name } => {
            return Ok(skills_command(&action, name.as_deref()))
        }
        AgentCommand::Session { action, common } => return session_command(&action, &common),
        AgentCommand::Snapshot {
            delta,
            within,
            find,
            limit,
            bounds,
            common,
        } => {
            let mut args = json!({ "mode": if delta { "delta" } else { "full" } });
            if let Some(w) = within {
                args["within"] = w.into();
            }
            if let Some(f) = find {
                args["find"] = f.into();
            }
            if let Some(l) = limit {
                args["limit"] = l.into();
            }
            if bounds {
                args["bounds"] = true.into();
            }
            (call("snapshot", "snapshot", args, false, false), common)
        }
        AgentCommand::Tap { target, act } => (
            call("tap", "tap", target_args(&target)?, true, !act.no_settle),
            act.common,
        ),
        AgentCommand::Type {
            target,
            text,
            secret,
            stdin,
            act,
        } => {
            let mut args = target_args(&target)?;
            match (text, secret, stdin) {
                (Some(t), None, false) => args["text"] = t.into(),
                (None, Some(name), false) => args["secret"] = name.into(),
                (None, None, true) => {
                    let t = match FORWARDED_STDIN
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        Some(t) => t,
                        None => {
                            let mut t = String::new();
                            std::io::stdin().read_to_string(&mut t)?;
                            t
                        }
                    };
                    args["text"] = t.trim_end_matches(['\r', '\n']).to_string().into();
                }
                _ => usage_bail!("give the text, --secret NAME or --stdin"),
            }
            (
                call("type", "type_text", args, true, !act.no_settle),
                act.common,
            )
        }
        AgentCommand::Press { key, act } => (
            call(
                "press",
                "press_key",
                json!({ "key": key }),
                true,
                !act.no_settle,
            ),
            act.common,
        ),
        AgentCommand::Swipe { direction, act } => (
            call(
                "swipe",
                "swipe",
                json!({ "direction": direction }),
                true,
                !act.no_settle,
            ),
            act.common,
        ),
        AgentCommand::Scroll {
            direction,
            until,
            max,
            act,
        } => {
            let mut args = json!({ "direction": direction });
            if let Some(u) = until {
                let t = target_args(&u)?;
                if t.get("ref").is_some() {
                    usage_bail!("--until takes text, id=… or JSON, not a ref: the element is not on screen yet");
                }
                args["until_visible"] = t["selector"].clone();
            }
            if let Some(m) = max {
                args["max_scrolls"] = m.into();
            }
            (
                call("scroll", "scroll", args, true, !act.no_settle),
                act.common,
            )
        }
        AgentCommand::Launch { app_id, clear, act } => {
            let mut args = json!({ "clear_state": clear });
            if let Some(a) = app_id {
                args["app_id"] = a.into();
            }
            (
                call("launch", "launch_app", args, true, !act.no_settle),
                act.common,
            )
        }
        AgentCommand::Stop { app_id, act } => {
            let mut args = json!({});
            if let Some(a) = app_id {
                args["app_id"] = a.into();
            }
            (
                call("stop", "stop_app", args, true, !act.no_settle),
                act.common,
            )
        }
        AgentCommand::Assert {
            target,
            not,
            timeout,
            common,
        } => {
            let mut args = target_args(&target)?;
            if let Some(t) = timeout {
                args["timeout_ms"] = super::duration_arg::as_millis_u64(t).into();
            }
            let tool = if not {
                "assert_not_visible"
            } else {
                "assert_visible"
            };
            (call("assert", tool, args, false, false), common)
        }
        AgentCommand::Wait {
            target,
            timeout,
            common,
        } => {
            let mut args = target_args(&target)?;
            if let Some(t) = timeout {
                args["timeout_ms"] = super::duration_arg::as_millis_u64(t).into();
            }
            (call("wait", "wait_for_element", args, false, false), common)
        }
        AgentCommand::Screenshot { common } => (
            call("screenshot", "screenshot", json!({}), false, false),
            common,
        ),
        AgentCommand::Export {
            out,
            name,
            app_id,
            clear,
            common,
        } => {
            let mut args = json!({ "clear": clear });
            if let Some(n) = name {
                args["name"] = n.into();
            }
            if let Some(a) = app_id {
                args["app_id"] = a.into();
            }
            let reply = run_call(
                call("export", "export_session_flow", args, false, false),
                &common,
            )
            .await?;
            return Ok(finish_export(reply, out.as_deref(), common.json));
        }
    };
    let reply = run_call(call, &common).await?;
    Ok(print_reply(&reply, common.json))
}

fn call(command: &'static str, tool: &'static str, args: Value, acts: bool, settle: bool) -> Call {
    Call {
        command,
        tool,
        args,
        acts,
        settle,
    }
}

/// A target as tool arguments: `@e7` (a ref), `key=value` (one selector
/// key, as the DSL writes it: `id=login`, `textContains=net`), a JSON
/// selector map, else exact text.
fn target_args(target: &str) -> anyhow::Result<Value> {
    let t = target.trim();
    if t.is_empty() {
        usage_bail!(
            "the target is empty: give a ref from `velocity snapshot` (@e7), visible text, \
             id=…, or a JSON selector"
        );
    }
    if t.starts_with("@e") {
        return Ok(json!({ "ref": t }));
    }
    if t.starts_with('{') {
        let v: Value = serde_json::from_str(t).map_err(|e| {
            usage_err!("target JSON: {e} (quote it for the shell: '{{\"text\":\"OK\"}}')")
        })?;
        return Ok(json!({ "selector": v }));
    }
    if let Some((k, v)) = t.split_once('=') {
        if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Ok(json!({ "selector": { k: v.trim() } }));
        }
    }
    Ok(json!({ "selector": { "text": t } }))
}

/// What one command did, ready to print.
struct Reply {
    command: &'static str,
    session: String,
    device: String,
    /// The tool's result (`data`) or its failure (`error`).
    outcome: Result<Value, Value>,
    /// After an action: the settle and the delta snapshot.
    settle: Option<Value>,
    screen: Option<String>,
}

async fn run_call(call: Call, common: &Common) -> anyhow::Result<Reply> {
    let mut session = match Session::open_waiting(common).await? {
        Ok(s) => s,
        Err(busy) => {
            return Ok(Reply {
                command: call.command,
                session: busy.key.clone(),
                device: String::new(),
                outcome: Err(busy.failure()),
                settle: None,
                screen: None,
            })
        }
    };
    let (platform, device) = pick_device(common, &session).await?;
    if platform == Platform::Ios {
        keep_runner_between_commands();
    }
    let driver = build_driver(common, platform, &device).await?;
    let mut server = McpServer::new(driver, device.clone(), common.config.clone())
        .with_platform(platform)
        .with_leases(common.headless_layout.is_none());
    server.load_state(&session.state["server"]);

    // One lease for the whole command: another agent on this device waits
    // (or gets DEVICE_BUSY) instead of acting between this action and the
    // delta that reports it.
    let _lease = match server.hold_lease().await {
        Ok(lease) => lease,
        Err(busy) => {
            // Bind the session to the device it chose all the same: a retry
            // without `-p`/`--device` must go to this device, not to the
            // first booted one on another platform.
            if session.state["device"].is_null() {
                session.state["device"] = device.clone().into();
                session.state["platform"] = serde_json::to_value(platform)?;
                session.save()?;
            }
            return Ok(Reply {
                command: call.command,
                session: session.key.clone(),
                device,
                outcome: outcome_of(&busy),
                settle: None,
                screen: None,
            });
        }
    };
    let result = server.call(call.tool, call.args).await?;
    let outcome = outcome_of(&result);
    let (mut settle, mut screen) = (None, None);
    if call.acts && call.settle && outcome.is_ok() {
        settle = Some(server.settle().await);
        // An empty screen is read while the next app attaches its tree (back
        // into a Flutter app on a real tablet read "0 elements"): it is
        // almost never where an action ends, so look again, briefly.
        for attempt in 0..4 {
            let delta = server.call("snapshot", json!({ "mode": "delta" })).await?;
            let text = match outcome_of(&delta) {
                Ok(Value::String(s)) => s,
                _ => break,
            };
            let empty = text
                .lines()
                .next()
                .is_some_and(|h| h.contains(" 0 elements"));
            screen = Some(text);
            if !empty || attempt == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            server.settle().await;
        }
    }
    session.state = json!({
        "device": device,
        "platform": platform,
        "server": server.save_state(),
    });
    session.save()?;
    Ok(Reply {
        command: call.command,
        session: session.key.clone(),
        device,
        outcome,
        settle,
        screen,
    })
}

/// Leave WebDriverAgent running when this command exits, so the next one
/// reattaches instead of relaunching it (about 2 s a command on a
/// simulator). `VELOCITY_KEEP_WDA=0` set by the user still wins.
fn keep_runner_between_commands() {
    if velocity_common::env::var("VELOCITY_KEEP_WDA").is_err() {
        let mut settings = velocity_common::env::settings_snapshot();
        settings.insert("VELOCITY_KEEP_WDA".into(), "1".into());
        velocity_common::env::set_settings(settings);
    }
}

/// The tool's data, or its failure as `{code, message, retry, delivered, next}`.
fn outcome_of(result: &Value) -> Result<Value, Value> {
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(result
            .pointer("/structuredContent/error")
            .cloned()
            .unwrap_or_else(
                || json!({ "code": "ERROR", "message": text, "retry": "no", "delivered": "no" }),
            ));
    }
    Ok(match serde_json::from_str::<Value>(&text) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => v,
        _ => Value::String(text),
    })
}

/// 0 ok; 3 when the device (or the session holding it) is the problem
/// (`DEVICE_*`, `SESSION_BUSY`), so CI retries it like any infrastructure
/// failure (docs/CI.md); else 1.
fn exit_code_of(outcome: &Result<Value, Value>) -> i32 {
    match outcome {
        Ok(_) => 0,
        Err(e) => {
            let code = e["code"].as_str().unwrap_or("");
            if code.starts_with("DEVICE_") || code == "SESSION_BUSY" {
                3
            } else {
                1
            }
        }
    }
}

fn print_reply(reply: &Reply, as_json: bool) -> i32 {
    let mut out = std::io::stdout().lock();
    let code = exit_code_of(&reply.outcome);
    if as_json {
        let mut v = json!({
            "v": 1,
            "ok": reply.outcome.is_ok(),
            "command": reply.command,
            "session": reply.session,
            "device": reply.device,
        });
        match &reply.outcome {
            Ok(d) => v["data"] = d.clone(),
            Err(e) => v["error"] = e.clone(),
        }
        if let Some(s) = &reply.settle {
            v["settle"] = s.clone();
        }
        if let Some(s) = &reply.screen {
            v["screen"] = s.clone().into();
        }
        let _ = writeln!(out, "{v}");
        return code;
    }
    match &reply.outcome {
        Ok(Value::String(s)) => {
            let _ = writeln!(out, "{s}");
        }
        Ok(d) => {
            let _ = writeln!(out, "{}", summary(reply.command, d));
        }
        Err(e) => {
            let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
            let _ = writeln!(out, "error {}: {}", s("code"), s("message"));
            let mut tail = format!("retry: {}", s("retry"));
            if !s("next").is_empty() {
                tail.push_str(&format!("; next: {}", s("next")));
            }
            let _ = writeln!(out, "{tail}");
        }
    }
    if let Some(screen) = &reply.screen {
        let _ = writeln!(out, "{screen}");
    }
    code
}

/// One line for an action's result: `ok tap text="Continue"`.
fn summary(command: &str, data: &Value) -> String {
    let s = |k: &str| data.get(k).and_then(Value::as_str);
    if let Some(path) = s("path") {
        return format!("ok {command} {path}");
    }
    match s("selector") {
        Some(sel) => format!("ok {command} {sel}"),
        None => format!("ok {command} {data}"),
    }
}

fn finish_export(mut reply: Reply, out: Option<&std::path::Path>, as_json: bool) -> i32 {
    let (Ok(data), Some(path)) = (reply.outcome.clone(), out) else {
        if let (Ok(data), false) = (&reply.outcome, as_json) {
            // The YAML itself, ready to redirect into a file.
            print!("{}", data.get("yaml").and_then(Value::as_str).unwrap_or(""));
            return 0;
        }
        return print_reply(&reply, as_json);
    };
    let yaml = data.get("yaml").and_then(Value::as_str).unwrap_or("");
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let written = std::fs::write(path, yaml);
    reply.outcome = match written {
        Ok(()) => Ok(json!({
            "path": path.display().to_string(),
            "steps": data.get("steps").cloned().unwrap_or(Value::Null),
            "assertions": data.get("assertions").cloned().unwrap_or(Value::Null),
        })),
        Err(e) => Err(json!({
            "code": "INVALID_ARGS", "message": format!("can't write {}: {e}", path.display()),
            "retry": "no", "delivered": "no",
        })),
    };
    print_reply(&reply, as_json)
}

// ---- Sessions -------------------------------------------------------------

/// A session's saved state, locked for the command's life.
struct Session {
    key: String,
    path: PathBuf,
    state: Value,
    _lock: std::fs::File,
}

fn sessions_dir() -> PathBuf {
    let root = velocity_common::env::var("VELOCITY_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            velocity_common::host::home_dir()
                .join(".velocity")
                .join("agent")
        });
    root.join("sessions")
}

/// `--session`, else `wt-<hash of the git worktree, else the directory>`.
fn session_key(common: &Common) -> String {
    let from_env = velocity_common::env::var("VELOCITY_SESSION").ok();
    if let Some(name) = common
        .session
        .as_deref()
        .or(from_env.as_deref())
        .filter(|s| !s.trim().is_empty())
    {
        return name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
    }
    let place = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.display().to_string())
        })
        .unwrap_or_default();
    let hash = place.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("wt-{:012x}", hash & 0xffff_ffff_ffff)
}

/// Another command of the same session held its lock past the wait.
struct SessionBusy {
    key: String,
    holder: String,
}

impl SessionBusy {
    fn failure(&self) -> Value {
        json!({
            "code": "SESSION_BUSY",
            "message": format!(
                "session {} is busy: another command of it ({}) is still running",
                self.key, self.holder
            ),
            "retry": "safe",
            "delivered": "no",
            "next": "wait for that command to finish, or use another --session",
        })
    }
}

impl Session {
    /// [`Session::try_open`], waiting up to `VELOCITY_LEASE_WAIT_MS`
    /// (default 10 s, like the device lease) for another command of the
    /// session to finish. A blocking lock here held a runtime thread, and a
    /// hung command held every later one with no reply (review 12 #11).
    async fn open_waiting(common: &Common) -> anyhow::Result<Result<Self, SessionBusy>> {
        let wait = velocity_common::env::var("VELOCITY_LEASE_WAIT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(10_000);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait);
        loop {
            match Self::try_open(common)? {
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                other => return Ok(other),
            }
        }
    }

    /// The session, or who holds it. The lock is exclusive: two commands
    /// of one session take turns (the device lease alone would let them
    /// overwrite each other's refs).
    fn try_open(common: &Common) -> anyhow::Result<Result<Self, SessionBusy>> {
        use std::io::{Read, Seek, Write};
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let key = session_key(common);
        let mut lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(format!("{key}.lock")))?;
        match lock.try_lock() {
            Ok(()) => {
                // For the busy message only; never trusted for exclusion.
                let _ = lock.set_len(0);
                let _ = lock.seek(std::io::SeekFrom::Start(0));
                let _ = writeln!(lock, "pid={}", std::process::id());
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = lock.read_to_string(&mut holder);
                let holder = match holder.trim() {
                    "" => "another process".to_string(),
                    h => h.to_string(),
                };
                return Ok(Err(SessionBusy { key, holder }));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let path = dir.join(format!("{key}.json"));
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        Ok(Ok(Self {
            key,
            path,
            state,
            _lock: lock,
        }))
    }

    fn save(&self) -> anyhow::Result<()> {
        let tmp = self
            .path
            .with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&self.state)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn session_command(action: &str, common: &Common) -> anyhow::Result<i32> {
    if action == "list" {
        let mut rows = Vec::new();
        if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") {
                    let state: Value = std::fs::read_to_string(&p)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default();
                    rows.push(json!({
                        "session": p.file_stem().map(|s| s.to_string_lossy().into_owned()),
                        "device": state["device"],
                        "platform": state["platform"],
                    }));
                }
            }
        }
        if common.json {
            println!(
                "{}",
                json!({ "v": 1, "ok": true, "command": "session", "data": rows })
            );
        } else if rows.is_empty() {
            println!("no sessions");
        } else {
            for r in rows {
                println!(
                    "{}  {} ({})",
                    r["session"].as_str().unwrap_or(""),
                    r["device"].as_str().unwrap_or("-"),
                    r["platform"].as_str().unwrap_or("-")
                );
            }
        }
        return Ok(0);
    }
    let session = match Session::try_open(common)? {
        Ok(s) => s,
        Err(busy) => anyhow::bail!("{}", busy.failure()["message"].as_str().unwrap_or("busy")),
    };
    let data = if action == "reset" {
        let _ = std::fs::remove_file(&session.path);
        json!({ "session": session.key, "reset": true })
    } else {
        let steps = session
            .state
            .pointer("/server/recorder/steps")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let refs = session
            .state
            .pointer("/server/snapshots/refs")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        json!({
            "session": session.key,
            "device": session.state["device"],
            "platform": session.state["platform"],
            "refs": refs,
            "recorded_steps": steps,
        })
    };
    if common.json {
        println!(
            "{}",
            json!({ "v": 1, "ok": true, "command": "session", "data": data })
        );
    } else if action == "reset" {
        println!("session {} reset", session.key);
    } else {
        println!(
            "session {}: device {} ({}), {} refs, {} recorded steps",
            session.key,
            data["device"].as_str().unwrap_or("not chosen yet"),
            data["platform"].as_str().unwrap_or("-"),
            data["refs"],
            data["recorded_steps"],
        );
    }
    Ok(0)
}

fn skills_command(action: &str, name: Option<&str>) -> i32 {
    match (action, name) {
        ("stub", _) => {
            print!("{}", skills::STUB);
            0
        }
        ("get", Some(n)) => match skills::get(n) {
            Some(body) => {
                print!("{body}");
                0
            }
            None => {
                eprintln!("no guide named {n:?}; `velocity skills list` shows them");
                2
            }
        },
        ("get", None) => {
            eprintln!("name a guide: `velocity skills get cli`");
            2
        }
        _ => {
            for (n, summary, _) in skills::SKILLS {
                println!("{n:<6} {summary}");
            }
            println!("\nPrint one with `velocity skills get <name>`.");
            0
        }
    }
}

// ---- Devices --------------------------------------------------------------

fn parse_platform(p: &str) -> anyhow::Result<Platform> {
    Ok(match p {
        "android" => Platform::Android,
        "ios" => Platform::Ios,
        "web" => Platform::Web,
        other => usage_bail!("unknown platform '{other}': use android, ios or web"),
    })
}

/// `--device`, else the session's, else the first booted device (Android
/// first, then iOS, or only `--platform`'s).
async fn pick_device(common: &Common, session: &Session) -> anyhow::Result<(Platform, String)> {
    if common.headless_layout.is_some() {
        return Ok((Platform::Android, "headless".into()));
    }
    let wanted = common.platform.as_deref().map(parse_platform).transpose()?;
    let saved_device = session.state["device"].as_str().map(String::from);
    let saved_platform = session.state["platform"]
        .as_str()
        .and_then(|p| parse_platform(p).ok());
    if wanted == Some(Platform::Web) && common.device.is_none() {
        return Ok((Platform::Web, "web".into()));
    }
    if let Some(d) = common.device.clone() {
        let p = wanted.unwrap_or_else(|| {
            if d == "web" || d.starts_with("web-") {
                Platform::Web
            } else {
                Platform::of_device_id(&d)
            }
        });
        return Ok((p, d));
    }
    if let (Some(d), Some(p)) = (saved_device, saved_platform) {
        if wanted.is_none_or(|w| w == p) {
            return Ok((p, d));
        }
    }
    let order: Vec<Platform> = match wanted {
        Some(p) => vec![p],
        None => vec![Platform::Android, Platform::Ios],
    };
    let mut seen = Vec::new();
    for p in order {
        if p == Platform::Ios && !cfg!(target_os = "macos") {
            continue;
        }
        let driver = super::create_driver(p);
        if let Ok(devices) = driver.list_devices().await {
            if let Some(d) = devices.iter().find(|d| d.state == DeviceState::Booted) {
                return Ok((p, d.id.clone()));
            }
            seen.extend(devices.into_iter().map(|d| d.id));
        }
    }
    Err(super::no_device(
        "No booted device found. Boot one (velocity device boot <name>) or pass --device",
        seen,
    ))
}

async fn build_driver(
    common: &Common,
    platform: Platform,
    device: &str,
) -> anyhow::Result<Arc<dyn PlatformDriver>> {
    if let Some(layout) = &common.headless_layout {
        let config = velocity_headless::HeadlessConfig {
            app_path: Some(layout.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let driver: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            Platform::Android,
            config,
        ));
        driver.launch_app(device, "app", false).await?;
        return Ok(driver);
    }
    if platform == Platform::Web {
        // Only in the daemon (`execute` forwards web commands there).
        let mut slot = WEB_DRIVER.lock().await;
        if let Some(d) = slot.as_ref() {
            return Ok(d.clone());
        }
        let d: Arc<dyn PlatformDriver> = Arc::from(super::create_driver(Platform::Web));
        *slot = Some(d.clone());
        return Ok(d);
    }
    Ok(Arc::from(super::create_driver(platform)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_become_refs_selectors_or_text() {
        assert_eq!(target_args("@e7").unwrap(), json!({ "ref": "@e7" }));
        assert_eq!(
            target_args("id=login_button").unwrap(),
            json!({ "selector": { "id": "login_button" } })
        );
        assert_eq!(
            target_args(r#"{"role":"button","name":"Pay"}"#).unwrap(),
            json!({ "selector": { "role": "button", "name": "Pay" } })
        );
        assert_eq!(
            target_args("Total = 3").unwrap(),
            json!({ "selector": { "text": "Total = 3" } }),
            "text with a spaced '=' is text"
        );
        assert_eq!(
            target_args("Log in").unwrap(),
            json!({ "selector": { "text": "Log in" } })
        );
        assert!(target_args("  ").is_err());
    }

    #[test]
    fn named_sessions_are_file_safe() {
        let c = Common {
            session: Some("agent 1/../x".into()),
            ..Default::default()
        };
        assert_eq!(session_key(&c), "agent_1____x");
        let default = session_key(&Common::default());
        assert!(
            default.starts_with("wt-") && default.len() == 15,
            "{default}"
        );
    }

    #[test]
    fn failures_without_a_code_still_get_one() {
        let r = json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true });
        let e = outcome_of(&r).unwrap_err();
        assert_eq!(e["code"], "ERROR");
        let ok = json!({ "content": [{ "type": "text", "text": "# 3 elements" }] });
        assert_eq!(
            outcome_of(&ok).unwrap(),
            Value::String("# 3 elements".into())
        );
    }

    #[test]
    fn web_is_a_platform_and_unknown_ones_are_usage_errors() {
        assert_eq!(parse_platform("web").unwrap(), Platform::Web);
        assert_eq!(parse_platform("android").unwrap(), Platform::Android);
        assert_eq!(parse_platform("ios").unwrap(), Platform::Ios);
        let e = parse_platform("tvos").unwrap_err();
        assert_eq!(super::super::exit_code_for(&e), 2, "{e}");
        assert!(e.to_string().contains("android, ios or web"), "{e}");
    }

    /// One test for every `targets_web` case: the session-file case
    /// installs an environment (VELOCITY_AGENT_DIR), which is process-wide.
    #[test]
    fn web_commands_are_recognised_by_flag_device_or_session() {
        let with = |platform: Option<&str>, device: Option<&str>| Common {
            platform: platform.map(String::from),
            device: device.map(String::from),
            session: Some("web-routing-test".into()),
            ..Default::default()
        };
        assert!(targets_web(&with(Some("web"), None)));
        assert!(!targets_web(&with(Some("android"), Some("web"))), "-p wins");
        assert!(targets_web(&with(None, Some("web"))));
        assert!(targets_web(&with(None, Some("web-1"))));
        assert!(!targets_web(&with(None, Some("emulator-5554"))));
        let headless = Common {
            platform: Some("web".into()),
            headless_layout: Some("form.xml".into()),
            ..Default::default()
        };
        assert!(
            !targets_web(&headless),
            "a headless layout never goes to the daemon"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
        env.insert(
            "VELOCITY_AGENT_DIR".into(),
            dir.path().to_string_lossy().into_owned(),
        );
        velocity_common::env::install(env);
        let plain = with(None, None);
        let missing = targets_web(&plain);
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("web-routing-test.json"),
            r#"{"device":"web","platform":"web"}"#,
        )
        .unwrap();
        let saved_web = targets_web(&plain);
        std::fs::write(
            sessions.join("web-routing-test.json"),
            r#"{"device":"emulator-5554","platform":"android"}"#,
        )
        .unwrap();
        let saved_android = targets_web(&plain);
        velocity_common::env::clear();
        assert!(!missing, "no session file: not the web");
        assert!(
            saved_web,
            "a session that drives the web keeps going to the daemon"
        );
        assert!(!saved_android);
    }
}
