//! iOS simulator failure evidence: `simctl io recordVideo` for the video
//! and `log stream` for the app's log, both started with the test.
//!
//! `recordVideo` writes the file as it records and finalizes it on SIGINT,
//! so a test's video is stopped at its end and kept only when it failed (a
//! passing test's is stopped and deleted in the background). There is no
//! ring buffer: the whole test is recorded. Real devices have neither
//! (`devicectl` offers no screen recording): unsupported for now.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, warn};
use velocity_common::evidence::{
    self, Attachment, AttachmentKind, Evidence, EvidenceOut, EvidenceRequest, EvidenceSession,
    LogBuffer, LogLine,
};

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Recording {
    child: Child,
    file: PathBuf,
    /// When `recordVideo` said "Recording started".
    started: Arc<Mutex<Option<Instant>>>,
}

/// One simulator test's capture.
pub struct SimEvidence {
    req: EvidenceRequest,
    video: Option<Recording>,
    log: Arc<tokio::sync::Mutex<Option<Child>>>,
    lines: LogBuffer,
    log_task: Option<tokio::task::JoinHandle<()>>,
}

/// The executable name in `simctl appinfo`'s output
/// (`CFBundleExecutable = Name;`).
pub fn executable_from_appinfo(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let v = l.trim().strip_prefix("CFBundleExecutable")?.trim_start();
        let v = v.strip_prefix('=')?.trim().trim_end_matches(';').trim();
        let v = v.trim_matches('"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// A `log stream --style compact` line's level: `Df` debug, `I` info,
/// `E` error, `Fa` fault; `' '` for headers and continuation lines.
pub fn compact_level(line: &str) -> char {
    let mut it = line.split_whitespace();
    let (Some(date), Some(_time), Some(ty)) = (it.next(), it.next(), it.next()) else {
        return ' ';
    };
    if date.len() != 10 || !date.as_bytes()[0].is_ascii_digit() {
        return ' ';
    }
    match ty.chars().next() {
        Some('D') => 'D',
        Some('I') => 'I',
        Some('E') => 'E',
        Some('F') => 'F',
        Some('A') => 'I',
        _ => ' ',
    }
}

/// `log stream`'s own preamble, not the app's.
fn is_preamble(line: &str) -> bool {
    line.starts_with("Filtering the log data")
        || line.starts_with("Timestamp ")
        || line.starts_with("getpwuid_r")
}

/// The predicate for the app's process, with its name quoted for
/// NSPredicate (a name can't break out of the string).
pub fn predicate(executable: &str) -> String {
    let quoted = executable.replace('\\', "\\\\").replace('"', "\\\"");
    format!("process == \"{quoted}\"")
}

impl SimEvidence {
    pub async fn start(xcrun: &str, udid: &str, req: &EvidenceRequest) -> Option<Self> {
        if !req.video.on() && !req.logs.on() {
            return None;
        }
        let video = if req.video.on() {
            match start_recording(xcrun, udid) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!(udid, "simulator video not recorded: {e}");
                    None
                }
            }
        } else {
            None
        };
        let lines = LogBuffer::default();
        let log = Arc::new(tokio::sync::Mutex::new(None));
        let log_task = req.logs.on().then(|| {
            let (xcrun, udid, app, slot, buf, started) = (
                xcrun.to_string(),
                udid.to_string(),
                req.app_id.clone(),
                log.clone(),
                lines.clone(),
                req.started,
            );
            tokio::spawn(async move {
                // The app's executable name, for the predicate: one simctl call.
                // Bounded, and killed with the task: a wedged simctl would
                // otherwise outlive the test that aborted this task.
                let mut appinfo = Command::new(&xcrun);
                appinfo.args(["simctl", "appinfo", &udid, &app]);
                let info = velocity_common::process::output_with_timeout(
                    &mut appinfo,
                    velocity_common::process::default_timeout(),
                )
                .await;
                let exe = info
                    .ok()
                    .and_then(|o| executable_from_appinfo(&String::from_utf8_lossy(&o.stdout)));
                let Some(exe) = exe else {
                    debug!(udid, "no executable name for {app}: no app log");
                    return;
                };
                let child = Command::new(&xcrun)
                    .args([
                        "simctl", "spawn", &udid, "log", "stream", "--style", "compact",
                    ])
                    .args(["--level", "info", "--predicate", &predicate(&exe)])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn();
                let mut child = match child {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(udid, "log stream not started: {e}");
                        return;
                    }
                };
                let Some(out) = child.stdout.take() else {
                    return;
                };
                *slot.lock().await = Some(child);
                let mut r = BufReader::new(out).lines();
                while let Ok(Some(line)) = r.next_line().await {
                    if is_preamble(&line) {
                        continue;
                    }
                    buf.push(LogLine {
                        t: started.elapsed().as_millis() as i64,
                        level: compact_level(&line),
                        text: line,
                    });
                }
            })
        });
        Some(Self {
            req: req.clone(),
            video,
            log,
            lines,
            log_task,
        })
    }
}

fn start_recording(xcrun: &str, udid: &str) -> std::io::Result<Recording> {
    let file = std::env::temp_dir().join(format!(
        "velocity-sim-{}-{}.mp4",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut child = Command::new(xcrun)
        .args([
            "simctl",
            "io",
            udid,
            "recordVideo",
            "--codec",
            "h264",
            "--force",
        ])
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let started = Arc::new(Mutex::new(None));
    if let Some(err) = child.stderr.take() {
        let s = started.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(err).lines();
            while let Ok(Some(line)) = r.next_line().await {
                if line.contains("Recording started") {
                    *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                }
            }
        });
    }
    Ok(Recording {
        child,
        file,
        started,
    })
}

/// SIGINT, which makes `recordVideo` write the file out, then wait.
async fn stop_recording(rec: &mut Recording) -> bool {
    if let Some(pid) = rec.child.id() {
        // SAFETY: plain kill(2) on our own child's pid.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGINT);
        }
    }
    match tokio::time::timeout(Duration::from_secs(15), rec.child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = rec.child.start_kill();
            false
        }
    }
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        let _ = std::fs::remove_file(from);
    }
    Ok(())
}

#[async_trait::async_trait]
impl EvidenceSession for SimEvidence {
    async fn checkpoint(&mut self, out: EvidenceOut<'_>) -> Evidence {
        // The recording finalizes only when stopped: logs only.
        let mut ev = Evidence::default();
        if out.keep_logs {
            write_logs(&self.lines, out, &mut ev);
        }
        ev
    }

    async fn finish(mut self: Box<Self>, out: EvidenceOut<'_>) -> Evidence {
        let mut ev = Evidence::default();
        if let Some(mut rec) = self.video.take() {
            if out.keep_video {
                let ok = stop_recording(&mut rec).await;
                let dest = out.file("video", "mp4");
                let has_file = std::fs::metadata(&rec.file).is_ok_and(|m| m.len() > 0);
                if ok && has_file {
                    match move_file(&rec.file, &dest) {
                        Ok(()) => {
                            let started = *rec.started.lock().unwrap_or_else(|e| e.into_inner());
                            ev.video_offset_ms = started.map(|t| {
                                t.saturating_duration_since(self.req.started).as_millis() as i64
                            });
                            ev.attachments
                                .push(Attachment::new(AttachmentKind::Video, dest));
                        }
                        Err(e) => warn!("simulator video not kept: {e}"),
                    }
                } else {
                    warn!("simulator video was not finalized");
                    let _ = std::fs::remove_file(&rec.file);
                }
            } else {
                // A pass: stop and delete off the test's path.
                tokio::spawn(async move {
                    stop_recording(&mut rec).await;
                    let _ = std::fs::remove_file(&rec.file);
                });
            }
        }
        if out.keep_logs {
            // Lines still in flight.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if let Some(t) = self.log_task.take() {
            t.abort();
        }
        if let Some(mut c) = self.log.lock().await.take() {
            let _ = c.start_kill();
        }
        if out.keep_logs {
            write_logs(&self.lines, out, &mut ev);
        }
        ev
    }
}

fn write_logs(lines: &LogBuffer, out: EvidenceOut<'_>, ev: &mut Evidence) {
    let all = lines.snapshot();
    let path = out.file("app", "log");
    match evidence::write_log(&path, &all) {
        Ok(()) => ev
            .attachments
            .push(Attachment::new(AttachmentKind::AppLog, path)),
        Err(e) => warn!("app log not written: {e}"),
    }
    let keep = all.len().saturating_sub(20_000);
    ev.log_lines = evidence::redacted(all.into_iter().skip(keep).collect());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_executable_name() {
        let out = "{\n    CFBundleDisplayName = Settings;\n    CFBundleExecutable = Preferences;\n    CFBundleIdentifier = \"com.apple.Preferences\";\n}";
        assert_eq!(executable_from_appinfo(out).as_deref(), Some("Preferences"));
        let quoted = "    CFBundleExecutable = \"My App\";";
        assert_eq!(executable_from_appinfo(quoted).as_deref(), Some("My App"));
        assert_eq!(executable_from_appinfo("nothing"), None);
    }

    #[test]
    fn compact_levels() {
        assert_eq!(
            compact_level("2026-10-25 00:41:57.026 Df SpringBoard[38114:63a0e] hi"),
            'D'
        );
        assert_eq!(
            compact_level("2026-10-25 00:41:57.026 E  App[1:2] boom"),
            'E'
        );
        assert_eq!(
            compact_level("2026-10-25 00:41:57.026 Fa App[1:2] fault"),
            'F'
        );
        assert_eq!(compact_level("\tcontinued"), ' ');
        assert_eq!(
            compact_level("Timestamp               Ty Process[PID:TID]"),
            ' '
        );
        assert!(is_preamble("Filtering the log data using \"x\""));
    }

    /// `simctl appinfo` ran with no deadline and no kill: a wedged one kept
    /// running after the test ended and aborted the task waiting on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wedged_appinfo_does_not_outlive_the_test() {
        let dir = std::env::temp_dir().join(format!("velocity-appinfo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (stub, pidfile) = (dir.join("xcrun"), dir.join("pid"));
        crate::write_stub(
            &stub,
            &format!("echo $$ > '{}'\nexec sleep 300", pidfile.display()),
        );
        let req = EvidenceRequest {
            test: "t".into(),
            app_id: "com.example.app".into(),
            video: velocity_common::evidence::CaptureMode::Off,
            video_seconds: 0,
            logs: velocity_common::evidence::CaptureMode::Always,
            started: Instant::now(),
        };
        let session = SimEvidence::start(stub.to_str().unwrap(), "UDID", &req)
            .await
            .expect("logs are on");
        let mut pid = None;
        for _ in 0..100 {
            pid = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok());
            if pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = pid.expect("the stub ran");
        Box::new(session)
            .finish(EvidenceOut {
                dir: &dir,
                suffix: "",
                keep_video: false,
                keep_logs: false,
            })
            .await;
        // Gone, or a zombie waiting to be reaped.
        let running = || {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|o| {
                    let stat = String::from_utf8_lossy(&o.stdout);
                    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
                })
                .unwrap_or(false)
        };
        for _ in 0..60 {
            if !running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let alive = running();
        if alive {
            // Don't leave the stub behind when the assertion fails.
            // SAFETY: plain kill(2) on the stub this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!alive, "simctl appinfo (pid {pid}) is still running");
    }

    #[test]
    fn predicate_quotes_the_name() {
        assert_eq!(predicate("App"), "process == \"App\"");
        assert_eq!(predicate("a\"b"), "process == \"a\\\"b\"");
    }
}
