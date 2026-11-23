//! Completion notifiers (`atalanta mcp --notify …`): told once when a task
//! ends, with the task as JSON.
//!
//! - `webhook=<url>`: POST the task JSON (http or https).
//! - `script=<path>`: run the program with the task file's path as its one
//!   argument and `ATALANTA_TASK_ID` / `ATALANTA_TASK_STATE` set. Its output
//!   goes to the server's stderr, never to stdout (the protocol).
//! - `jsonl=<path>`: append the task JSON as one line.
//! - `desktop`: a desktop notification (`osascript` on macOS, `notify-send`
//!   on Linux).
//!
//! Only the person starting the server picks notifiers: an agent can't add
//! a script or a webhook through a tool call. No text is ever spliced into a
//! shell or a script: everything is passed as separate arguments.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// One way to say a task ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Notifier {
    Webhook(String),
    Script(PathBuf),
    Jsonl(PathBuf),
    Desktop,
}

impl std::str::FromStr for Notifier {
    type Err = String;

    /// `webhook=<url>`, `script=<path>`, `jsonl=<path>` or `desktop`.
    fn from_str(s: &str) -> Result<Self, String> {
        let (kind, value) = match s.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (s.trim(), None),
        };
        let need = |v: Option<&'_ str>| -> Result<String, String> {
            v.filter(|v| !v.is_empty())
                .map(String::from)
                .ok_or_else(|| format!("--notify {kind} needs a value: {kind}=<…>"))
        };
        match kind {
            "webhook" => {
                let url = need(value)?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!("--notify webhook: not an http(s) URL: {url}"));
                }
                Ok(Self::Webhook(url))
            }
            "script" => Ok(Self::Script(PathBuf::from(need(value)?))),
            "jsonl" => Ok(Self::Jsonl(PathBuf::from(need(value)?))),
            "desktop" if value.is_none() => Ok(Self::Desktop),
            _ => Err(format!(
                "unknown --notify '{s}': use webhook=<url>, script=<path>, jsonl=<path> or desktop"
            )),
        }
    }
}

impl Notifier {
    /// Tell this notifier that `task` (written at `task_file`) ended.
    /// Failures are logged to stderr, never raised: a broken notifier must
    /// not lose the task's result.
    pub async fn notify(&self, task: &Value, task_file: &Path) {
        if let Err(e) = self.try_notify(task, task_file).await {
            tracing::warn!("notifier {self:?} failed: {e}");
        }
    }

    async fn try_notify(&self, task: &Value, task_file: &Path) -> Result<(), String> {
        let id = task["id"].as_str().unwrap_or("");
        let state = task["state"].as_str().unwrap_or("");
        match self {
            Self::Webhook(url) => {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|e| e.to_string())?;
                let resp = client
                    .post(url)
                    .json(task)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                Ok(())
            }
            Self::Script(path) => {
                let mut cmd = tokio::process::Command::new(path);
                cmd.arg(task_file)
                    .env("ATALANTA_TASK_ID", id)
                    .env("ATALANTA_TASK_STATE", state)
                    .stdin(std::process::Stdio::null())
                    // Never our stdout: it carries the protocol.
                    .stdout(stderr_stdio())
                    .stderr(std::process::Stdio::inherit());
                let out = atalanta_common::process::output_with_timeout(
                    &mut cmd,
                    Duration::from_secs(60),
                )
                .await
                .map_err(|e| e.to_string())?;
                if !out.status.success() {
                    return Err(format!("{} exited with {}", path.display(), out.status));
                }
                Ok(())
            }
            Self::Jsonl(path) => {
                use std::io::Write;
                let mut line = task.to_string();
                line.push('\n');
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .and_then(|mut f| f.write_all(line.as_bytes()))
                    .map_err(|e| format!("{}: {e}", path.display()))
            }
            Self::Desktop => {
                let title = format!("Atalanta task {id}");
                let body = desktop_body(task);
                let mut cmd =
                    desktop_command(&title, &body).ok_or("no desktop notifier on this platform")?;
                cmd.stdin(std::process::Stdio::null());
                let out = atalanta_common::process::output_with_timeout(
                    &mut cmd,
                    Duration::from_secs(10),
                )
                .await
                .map_err(|e| e.to_string())?;
                if !out.status.success() {
                    return Err(String::from_utf8_lossy(&out.stderr).into_owned());
                }
                Ok(())
            }
        }
    }
}

/// `passed: 3 passed, 0 failed`.
fn desktop_body(task: &Value) -> String {
    let state = task["state"].as_str().unwrap_or("ended");
    match task.get("summary").filter(|s| s.is_object()) {
        Some(s) => format!(
            "{state}: {} passed, {} failed, {} flaky",
            s["passed"], s["failed"], s["flaky"]
        ),
        None => state.to_string(),
    }
}

/// The notification command, with title and body as arguments: the
/// AppleScript reads them from `argv`, so no text becomes script.
fn desktop_command(title: &str, body: &str) -> Option<tokio::process::Command> {
    match std::env::consts::OS {
        "macos" => {
            let mut c = tokio::process::Command::new("osascript");
            c.args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                title,
                body,
            ]);
            Some(c)
        }
        "linux" => {
            let mut c = tokio::process::Command::new("notify-send");
            c.args(["--app-name=Atalanta", "--", title, body]);
            Some(c)
        }
        _ => None,
    }
}

/// The server's stderr, to hand a child as its stdout.
pub(crate) fn stderr_stdio() -> std::process::Stdio {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        if let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() {
            return std::process::Stdio::from(fd);
        }
    }
    std::process::Stdio::null()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifiers_parse() {
        assert_eq!(
            "webhook=https://x.example/h".parse::<Notifier>().unwrap(),
            Notifier::Webhook("https://x.example/h".into())
        );
        assert_eq!("desktop".parse::<Notifier>().unwrap(), Notifier::Desktop);
        assert_eq!(
            "jsonl=/tmp/a.jsonl".parse::<Notifier>().unwrap(),
            Notifier::Jsonl("/tmp/a.jsonl".into())
        );
        assert!("webhook=file:///etc/passwd".parse::<Notifier>().is_err());
        assert!("script=".parse::<Notifier>().is_err());
        assert!("email=a@b".parse::<Notifier>().is_err());
    }

    #[test]
    fn desktop_text_is_passed_as_arguments_not_script() {
        let evil = "\" & do shell script \"touch /tmp/pwned\" & \"";
        let cmd = desktop_command("Atalanta task t1", evil);
        if let Some(cmd) = cmd {
            let args: Vec<String> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(args.last().map(String::as_str), Some(evil));
            assert!(
                args.iter().filter(|a| a.contains("touch")).count() == 1,
                "{args:?}"
            );
        }
    }

    #[tokio::test]
    async fn jsonl_and_script_notifiers_run() {
        let dir = tempfile::tempdir().unwrap();
        let task = serde_json::json!({ "id": "t1", "state": "passed" });
        let file = dir.path().join("task.json");
        std::fs::write(&file, task.to_string()).unwrap();
        let jsonl = dir.path().join("events.jsonl");
        Notifier::Jsonl(jsonl.clone()).notify(&task, &file).await;
        Notifier::Jsonl(jsonl.clone()).notify(&task, &file).await;
        let lines = std::fs::read_to_string(&jsonl).unwrap();
        assert_eq!(lines.lines().count(), 2);

        #[cfg(unix)]
        {
            let script = crate::test_util::script(
                "notify-hook",
                "#!/bin/sh\necho \"$ATALANTA_TASK_ID $ATALANTA_TASK_STATE $1\" > \"$1.out\"\necho on-stdout\n",
            );
            let out = dir.path().join("task.json.out");
            Notifier::Script(script).notify(&task, &file).await;
            let got = std::fs::read_to_string(&out).unwrap();
            assert_eq!(got.trim(), format!("t1 passed {}", file.display()));
        }
    }
}
