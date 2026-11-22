//! The agent commands (`velocity snapshot`, `tap`, `type`, `export`, …),
//! one process per command as an agent's shell runs them, against a
//! headless render of an Android layout. Refs and the recording must carry
//! over between processes; failures must print a code and a retry rule.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const FORM: &str = r#"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
  android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
  <TextView android:layout_width="200dp" android:layout_height="48dp" android:text="Sign in"/>
  <EditText android:id="@+id/email" android:layout_width="200dp" android:layout_height="48dp"/>
  <Button android:id="@+id/go" android:layout_width="200dp" android:layout_height="48dp" android:text="Continue"/>
</LinearLayout>"#;

fn velocity(dir: &Path, args: &[&str]) -> (i32, String) {
    let layout = dir.join("form.xml");
    let out = Command::new(env!("CARGO_BIN_EXE_velocity"))
        .args(args)
        .args([
            "--headless-layout",
            layout.to_str().unwrap(),
            "--session",
            "t",
        ])
        .env("VELOCITY_AGENT_DIR", dir.join("state"))
        .env_remove("VELOCITY_SESSION")
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn json(dir: &Path, args: &[&str]) -> (i32, Value) {
    let mut a = args.to_vec();
    a.push("--json");
    let (code, out) = velocity(dir, &a);
    let v = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    (code, v)
}

#[test]
fn an_agent_drives_by_ref_across_processes_and_exports_a_test() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("form.xml"), FORM).unwrap();
    let d = dir.path();

    let (code, shot) = velocity(d, &["snapshot"]);
    assert_eq!(code, 0, "{shot}");
    let email = shot
        .lines()
        .find(|l| l.contains("#email"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();

    // A ref from the previous process, and what changed comes back.
    let (code, typed) = json(d, &["type", &email, "qa@velocity.dev"]);
    assert_eq!(code, 0, "{typed}");
    assert_eq!(typed["ok"], true);
    assert_eq!(typed["v"], 1);
    assert_eq!(typed["session"], "t");
    assert!(
        typed["screen"]
            .as_str()
            .unwrap()
            .contains("qa@velocity.dev"),
        "{typed}"
    );
    assert!(typed["settle"]["settled"].is_boolean(), "{typed}");

    let (code, tapped) = velocity(d, &["tap", "id=go"]);
    assert_eq!(code, 0, "{tapped}");
    assert!(tapped.starts_with("ok tap"), "{tapped}");

    // A stale ref: a code, a retry rule, a next step, exit 1.
    let (code, stale) = json(d, &["tap", "@e99"]);
    assert_eq!(code, 1);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["error"]["code"], "STALE_REF");
    assert_eq!(stale["error"]["retry"], "no");
    assert_eq!(stale["error"]["delivered"], "no");
    let (code, text) = velocity(d, &["tap", "@e99"]);
    assert_eq!(code, 1);
    assert!(
        text.starts_with("error STALE_REF:") && text.contains("retry: no"),
        "{text}"
    );

    let (_, found) = velocity(d, &["snapshot", "--find", "CONT"]);
    assert!(
        found.contains("1 shown") && found.contains("Continue"),
        "{found}"
    );

    let (code, session) = json(d, &["session"]);
    assert_eq!(code, 0);
    assert_eq!(session["data"]["recorded_steps"], 2, "{session}");

    let out = d.join("velocity/login.yaml");
    let (code, exported) = json(
        d,
        &[
            "export",
            "--app-id",
            "com.example",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{exported}");
    assert_eq!(exported["data"]["steps"], 2, "{exported}");
    let yaml = std::fs::read_to_string(&out).unwrap();
    assert!(!yaml.contains("@e"), "refs are never exported: {yaml}");
    assert!(
        yaml.contains("qa@velocity.dev") && yaml.contains("id: go"),
        "{yaml}"
    );

    let (code, _) = velocity(d, &["session", "reset"]);
    assert_eq!(code, 0);
    let (_, fresh) = json(d, &["tap", &email]);
    assert_eq!(
        fresh["error"]["code"], "STALE_REF",
        "a reset session has no refs: {fresh}"
    );
}

/// Visible text that starts `@e` ("@everyone") is text, not a ref.
#[test]
fn text_starting_with_at_e_is_tapped_as_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("form.xml"),
        FORM.replace("Sign in", "@everyone"),
    )
    .unwrap();
    let (code, out) = velocity(dir.path(), &["tap", "@everyone"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.starts_with("ok tap"), "{out}");
}

/// Sessions hold screen text and typed values: owner-only files.
#[cfg(unix)]
#[test]
fn session_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("form.xml"), FORM).unwrap();
    let (code, out) = velocity(dir.path(), &["snapshot"]);
    assert_eq!(code, 0, "{out}");
    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let sessions = dir.path().join("state/sessions");
    assert_eq!(mode(sessions.clone()), 0o700);
    assert_eq!(mode(sessions.join("t.json")), 0o600);
}

/// `session reset` on a session another command holds is the busy error
/// every other agent command gives (exit 3, "retry it"), not an internal
/// error (5).
#[test]
fn resetting_a_busy_session_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("form.xml"), FORM).unwrap();
    let sessions = dir.path().join("state/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(sessions.join("t.lock"))
        .unwrap();
    held.lock().unwrap();
    let (code, out) = json(dir.path(), &["session", "reset"]);
    assert_eq!(code, 3, "{out}");
    assert_eq!(out["error"]["code"], "SESSION_BUSY", "{out}");
    drop(held);
    let (code, _) = velocity(dir.path(), &["session", "reset"]);
    assert_eq!(code, 0, "free again");
}

#[test]
fn skills_print_the_guides_in_the_binary() {
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_velocity"))
            .args(args)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (code, list) = run(&["skills"]);
    assert_eq!(code, Some(0));
    assert!(list.contains("cli") && list.contains("mcp"), "{list}");
    let (code, cli) = run(&["skills", "get", "cli"]);
    assert_eq!(code, Some(0));
    assert!(
        cli.contains("velocity snapshot") && cli.contains("STALE_REF"),
        "{cli}"
    );
    let (code, stub) = run(&["skills", "stub"]);
    assert_eq!(code, Some(0));
    assert!(stub.starts_with("---\nname: velocity"), "{stub}");
    assert_eq!(run(&["skills", "get", "nope"]).0, Some(2));
}

/// Chrome or Chromium, where the web driver looks for it; `None` skips.
fn chrome() -> Option<std::path::PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .find(|p| p.exists())
}

/// Stops the test's daemon (and its Chrome) however the test ends.
struct DaemonGuard {
    socket: std::path::PathBuf,
    state: std::path::PathBuf,
}

impl DaemonGuard {
    fn velocity(&self, dir: &Path, args: &[&str]) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_velocity"))
            .args(args)
            .env("VELOCITY_DAEMON_SOCKET", &self.socket)
            .env("VELOCITY_AGENT_DIR", &self.state)
            .env_remove("VELOCITY_SESSION")
            .current_dir(dir)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_velocity"))
            .args(["daemon", "stop"])
            .env("VELOCITY_DAEMON_SOCKET", &self.socket)
            .output();
    }
}

const PAGE: &str = r#"<!doctype html><html><head><title>Login</title></head><body>
<h1>Sign in</h1>
<label for="email">Email</label><input id="email" aria-label="Email">
<button id="go" onclick="document.getElementById('out').textContent='Welcome '+document.getElementById('email').value">Continue</button>
<p id="out"></p></body></html>"#;

/// Web agent commands go through `velocity daemon` (started by the first
/// one), so the page a `launch` opened is still there for the next command.
#[test]
fn web_commands_keep_one_browser_through_the_daemon() {
    if chrome().is_none() {
        eprintln!("skipped: no Chrome or Chromium found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("index.html"), PAGE).unwrap();
    let url = format!("file://{}", d.join("index.html").display());
    // A Unix socket path must stay short (SUN_LEN): not under the temp dir.
    let guard = DaemonGuard {
        socket: std::path::PathBuf::from(format!("/tmp/vt-{}.sock", std::process::id())),
        state: d.join("state"),
    };
    let json_of = |args: &[&str]| -> (i32, Value) {
        let mut a = args.to_vec();
        a.extend(["--session", "t", "--json"]);
        let (code, out) = guard.velocity(d, &a);
        let line = out.lines().last().unwrap_or("").to_string();
        let v = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {out}"));
        (code, v)
    };

    let (code, launched) = json_of(&["launch", &url, "-p", "web"]);
    assert_eq!(code, 0, "{launched}");
    assert_eq!(launched["device"], "web", "{launched}");

    // No -p from here on: the session remembers it drives the web.
    let (code, typed) = json_of(&["type", "id=email", "qa@velocity.dev"]);
    assert_eq!(code, 0, "{typed}");
    let (code, tapped) = json_of(&["tap", "Continue"]);
    assert_eq!(code, 0, "{tapped}");
    assert!(
        tapped["screen"]
            .as_str()
            .unwrap_or("")
            .contains("Welcome qa@velocity.dev"),
        "the delta shows what the tap did: {tapped}"
    );
    let (code, seen) = json_of(&["assert", "Welcome qa@velocity.dev"]);
    assert_eq!(code, 0, "{seen}");

    let (code, stale) = json_of(&["tap", "@e99"]);
    assert_eq!(code, 1);
    assert_eq!(stale["error"]["code"], "STALE_REF", "{stale}");

    let (code, status) = guard.velocity(d, &["daemon", "status"]);
    assert_eq!(code, 0, "the first web command started a daemon: {status}");
}
