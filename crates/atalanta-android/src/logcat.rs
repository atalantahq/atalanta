// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! A test's logcat: started at the test's start with `logcat -T <device
//! time>`, so only that test's lines come, read in the background, and
//! written on failure as the whole device log and the app's own lines (its
//! pids, plus the crash buffer). Crashes and ANRs of the app are spotted
//! while the lines stream in, for the failure message.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use atalanta_common::evidence::{LogBuffer, LogLine};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// The device's wall clock minus the host's, in milliseconds.
pub async fn clock_offset_ms(adb_path: &str, device_id: &str) -> Option<i64> {
    let before = SystemTime::now();
    let out = crate::tree_server::run_adb(adb_path, device_id, &["shell", "date +%s%3N"])
        .await
        .ok()?;
    let after = SystemTime::now();
    parse_offset(out.trim(), before, after)
}

/// `device_ms` (`date +%s%3N`) against the middle of the host's round trip.
fn parse_offset(device_ms: &str, before: SystemTime, after: SystemTime) -> Option<i64> {
    let device: i64 = device_ms.parse().ok()?;
    let ms = |t: SystemTime| {
        t.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    };
    // A toybox without %3N prints "...%3N" or seconds only: not a time.
    if device < 1_000_000_000_000 {
        return None;
    }
    Some(device - (ms(before) + ms(after)) / 2)
}

/// `logcat -T`'s argument for host time `at` on a device whose clock is
/// `offset_ms` ahead: epoch seconds with milliseconds. A little earlier
/// than asked (`SLACK_MS`), so a line written as the test starts isn't
/// lost to the clock estimate.
pub fn since_arg(at: SystemTime, offset_ms: i64) -> String {
    let host = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let t = (host + offset_ms - SLACK_MS).max(0);
    format!("{}.{:03}", t / 1000, t % 1000)
}

const SLACK_MS: i64 = 100;

/// Most crash/ANR notes one test keeps.
const MAX_NOTES: usize = 20;

/// One `-v threadtime` line: `MM-DD HH:MM:SS.mmm  PID  TID L TAG: msg`.
#[derive(Debug, PartialEq)]
pub struct Parsed<'a> {
    pub pid: u32,
    pub level: char,
    pub tag: &'a str,
    pub msg: &'a str,
}

pub fn parse_threadtime(line: &str) -> Option<Parsed<'_>> {
    let mut rest = line;
    let mut fields = Vec::with_capacity(5);
    for _ in 0..5 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let pid = fields[2].parse().ok()?;
    let level = fields[4].chars().next()?;
    let rest = rest.trim_start();
    let (tag, msg) = match rest.find(": ") {
        Some(i) => (rest[..i].trim_end(), &rest[i + 2..]),
        None => (rest.trim_end_matches(':').trim_end(), ""),
    };
    Some(Parsed {
        pid,
        level,
        tag,
        msg,
    })
}

/// What the stream showed about the app so far.
#[derive(Debug, Default)]
pub struct Watch {
    package: String,
    pub pids: HashSet<u32>,
    /// The app's crashes and ANRs, one line each.
    pub notes: Vec<String>,
    /// The buffer the last divider named (`crash`, `main`, …).
    buffer: String,
    /// A Java crash of the app whose exception line comes next.
    fatal: Option<u32>,
    /// An ANR whose reason line comes next.
    anr: bool,
}

/// The longest note kept: the exception line, not its stack.
const NOTE_MAX: usize = 300;

impl Watch {
    pub fn new(package: &str) -> Self {
        Self {
            package: package.to_string(),
            ..Default::default()
        }
    }

    fn is_app_process(&self, name: &str) -> bool {
        name == self.package
            || name
                .strip_prefix(self.package.as_str())
                .is_some_and(|r| r.starts_with(':'))
    }

    /// Look at one line; returns whether it belongs to the app's log.
    pub fn see(&mut self, line: &str) -> bool {
        if let Some(b) = line
            .strip_prefix("--------- beginning of ")
            .or_else(|| line.strip_prefix("--------- switch to "))
        {
            self.buffer = b.trim().to_string();
            return false;
        }
        let Some(p) = parse_threadtime(line) else {
            return false;
        };
        let mut mine = self.pids.contains(&p.pid) || self.buffer == "crash";
        match p.tag {
            "ActivityManager" | "ActivityTaskManager" => {
                // "Start proc 4242:com.app/u0a190 for …"
                if let Some(rest) = p.msg.split("Start proc ").nth(1) {
                    if let Some((pid, name)) = rest.split_once(':') {
                        let name = name.split(['/', ' ']).next().unwrap_or("");
                        if self.is_app_process(name) {
                            if let Ok(pid) = pid.trim().parse() {
                                self.pids.insert(pid);
                                mine = true;
                            }
                        }
                    }
                }
                if let Some(rest) = p.msg.strip_prefix("ANR in ") {
                    let name = rest.split([' ', '(']).next().unwrap_or("");
                    if self.is_app_process(name) {
                        self.note(format!("ANR in {}", rest.trim()));
                        self.anr = true;
                        mine = true;
                    }
                } else if self.anr {
                    if let Some(reason) = p.msg.trim().strip_prefix("Reason: ") {
                        if let Some(last) = self.notes.last_mut() {
                            last.push_str(&format!(": {reason}"));
                            cut_note(last);
                        }
                        self.anr = false;
                    }
                }
                if p.msg.contains(&self.package) {
                    mine = true;
                }
            }
            "AndroidRuntime" => {
                // "FATAL EXCEPTION: main", "Process: com.app, PID: 4242",
                // then the exception.
                if let Some(rest) = p.msg.strip_prefix("Process: ") {
                    let (name, pid) = rest.split_once(", PID: ").unwrap_or((rest, ""));
                    if self.is_app_process(name.trim()) {
                        let pid = pid.trim().parse().unwrap_or(p.pid);
                        self.pids.insert(pid);
                        self.fatal = Some(pid);
                        mine = true;
                    }
                } else if let Some(pid) = self.fatal {
                    if p.pid == pid && !p.msg.starts_with("FATAL EXCEPTION") {
                        self.note(format!("crash (pid {pid}): {}", p.msg.trim()));
                        self.fatal = None;
                    }
                }
            }
            // "Fatal signal 11 (SIGSEGV), code 1 … pid 4242 (com.app)"
            "libc" | "DEBUG" | "crash_dump"
                if p.msg.starts_with("Fatal signal")
                    && (self.pids.contains(&p.pid)
                        || p.msg.contains(&format!("({})", self.package))) =>
            {
                self.note(format!("native crash: {}", p.msg.trim()));
                mine = true;
            }
            _ => {}
        }
        mine
    }

    fn note(&mut self, mut s: String) {
        cut_note(&mut s);
        // Capped: any app can log lines that look like the app's crashes
        // (logcat tags are not authenticated), and each distinct one was
        // kept (audit 2026-11 AND-3).
        if self.notes.len() < MAX_NOTES && !self.notes.contains(&s) {
            self.notes.push(s);
        }
    }
}

/// Cut `s` to [`NOTE_MAX`] bytes at a char boundary: an exception message
/// in CJK or with an emoji across the limit panicked the logcat reader
/// (`String::truncate` mid-char), and the crash evidence went with it.
fn cut_note(s: &mut String) {
    if s.len() <= NOTE_MAX {
        return;
    }
    let mut cut = NOTE_MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// `pidof <package>`'s pids (the app may be running from an earlier test).
pub fn parse_pidof(out: &str) -> Vec<u32> {
    out.split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// A running capture.
pub struct Logcat {
    child: Option<tokio::process::Child>,
    pub lines: LogBuffer,
    /// Indexes into `lines` of the app's own.
    app: Arc<Mutex<Vec<usize>>>,
    pub watch: Arc<Mutex<Watch>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Logcat {
    /// Start `logcat` from `since` (host time) on. `now_ms` stamps each
    /// line with the test time it arrived at.
    pub fn start(
        adb_path: &str,
        device_id: &str,
        package: &str,
        since: String,
        initial_pids: Vec<u32>,
        now_ms: impl Fn() -> i64 + Send + 'static,
    ) -> std::io::Result<Self> {
        let mut child = Command::new(adb_path)
            .args(crate::adb::target_args(device_id))
            .args([
                "logcat",
                "-v",
                "threadtime",
                "-D",
                "-b",
                "main,system,crash",
                "-T",
                &since,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let lines = LogBuffer::default();
        let app = Arc::new(Mutex::new(Vec::new()));
        let mut w = Watch::new(package);
        w.pids.extend(initial_pids);
        let watch = Arc::new(Mutex::new(w));
        let (buf, app2, watch2) = (lines.clone(), app.clone(), watch.clone());
        let reader = tokio::spawn(async move {
            let mut r = BufReader::new(stdout);
            let mut raw = Vec::with_capacity(256);
            let mut n = 0usize;
            loop {
                raw.clear();
                match r.read_until(b'\n', &mut raw).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&raw);
                let text = text.trim_end_matches(['\n', '\r']);
                let mine = watch2.lock().unwrap_or_else(|e| e.into_inner()).see(text);
                let level = parse_threadtime(text).map_or(' ', |p| p.level);
                buf.push(LogLine {
                    t: now_ms(),
                    level,
                    text: text.to_string(),
                });
                if mine {
                    app2.lock().unwrap_or_else(|e| e.into_inner()).push(n);
                }
                n += 1;
            }
        });
        Ok(Self {
            child: Some(child),
            lines,
            app,
            watch,
            reader: Some(reader),
        })
    }

    pub fn notes(&self) -> Vec<String> {
        self.watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .notes
            .clone()
    }

    /// The whole log and the app's lines so far.
    pub fn split(&self) -> (Vec<LogLine>, Vec<LogLine>) {
        let all = self.lines.snapshot();
        // Indexes count from the first line; dropped old lines shift them.
        let dropped = self.lines.dropped();
        let app: Vec<LogLine> = self
            .app
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|&i| i.checked_sub(dropped).and_then(|i| all.get(i).cloned()))
            .collect();
        (all, app)
    }

    /// Stop `logcat`, after giving lines in flight `drain` to arrive.
    pub async fn stop(&mut self, drain: Duration) {
        if !drain.is_zero() {
            tokio::time::sleep(drain).await;
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), c.wait()).await;
        }
        if let Some(r) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), r).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_time_for_logcat_t() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_792_848_427_250);
        // Device clock 2.5 s ahead: -T is device time minus the slack.
        assert_eq!(since_arg(at, 2500), "1792848429.650");
        assert_eq!(since_arg(at, 0), "1792848427.150");
        assert_eq!(since_arg(at, -1_792_848_427_250 - 5000), "0.000");
    }

    #[test]
    fn clock_offset_from_the_round_trip_middle() {
        let before = SystemTime::UNIX_EPOCH + Duration::from_millis(1_000_000_000_000);
        let after = before + Duration::from_millis(40);
        assert_eq!(parse_offset("1000000000520", before, after), Some(500));
        assert_eq!(
            parse_offset("1792848427", before, after),
            None,
            "seconds only"
        );
        assert_eq!(parse_offset("%3N", before, after), None);
    }

    #[test]
    fn parses_threadtime_lines() {
        let p = parse_threadtime("10-25 00:20:00.008  1372  1936 I AiAiEcho: Predicting[0]: x")
            .unwrap();
        assert_eq!(
            p,
            Parsed {
                pid: 1372,
                level: 'I',
                tag: "AiAiEcho",
                msg: "Predicting[0]: x"
            }
        );
        let p = parse_threadtime("10-25 00:20:00.008  1372  1936 E Tag with space: m").unwrap();
        assert_eq!(p.tag, "Tag with space");
        assert!(parse_threadtime("--------- beginning of main").is_none());
        assert!(parse_threadtime("").is_none());
    }

    #[test]
    fn finds_the_apps_crash_and_lines() {
        let mut w = Watch::new("com.example.app");
        let lines = [
            "--------- beginning of system",
            "10-25 00:20:00.000   500   600 I ActivityManager: Start proc 4242:com.example.app/u0a190 for top-activity {com.example.app/.Main}",
            "10-25 00:20:00.001   500   600 I ActivityManager: Start proc 4300:com.other/u0a191 for service",
            "--------- switch to main",
            "10-25 00:20:01.000  4242  4242 D MainActivity: hello",
            "10-25 00:20:01.000  4300  4300 D Other: not mine",
            "--------- switch to crash",
            "10-25 00:20:02.000  4242  4242 E AndroidRuntime: FATAL EXCEPTION: main",
            "10-25 00:20:02.000  4242  4242 E AndroidRuntime: Process: com.example.app, PID: 4242",
            "10-25 00:20:02.000  4242  4242 E AndroidRuntime: java.lang.IllegalStateException: boom",
            "10-25 00:20:02.000  4242  4242 E AndroidRuntime: \tat com.example.app.Main.onClick(Main.java:10)",
        ];
        let mine: Vec<bool> = lines.iter().map(|l| w.see(l)).collect();
        assert_eq!(
            mine,
            [false, true, false, false, true, false, false, true, true, true, true]
        );
        assert_eq!(
            w.notes,
            vec!["crash (pid 4242): java.lang.IllegalStateException: boom"]
        );
        assert!(w.pids.contains(&4242) && !w.pids.contains(&4300));
    }

    #[test]
    fn finds_anrs_and_native_crashes() {
        let mut w = Watch::new("com.example.app");
        w.see("10-25 00:20:00.000   500   600 E ActivityManager: ANR in com.example.app (com.example.app/.Main)");
        w.see("10-25 00:20:00.000   500   600 E ActivityManager: PID: 4242");
        w.see(
            "10-25 00:20:00.000   500   600 E ActivityManager: Reason: Input dispatching timed out",
        );
        w.see("10-25 00:20:00.000   500   600 E ActivityManager: ANR in com.example.app.other");
        w.see("10-25 00:20:03.000  4242  4250 F libc    : Fatal signal 11 (SIGSEGV), code 1, fault addr 0x0 in tid 4250 (RenderThread), pid 4242 (com.example.app)");
        assert_eq!(w.notes.len(), 2, "{:?}", w.notes);
        assert_eq!(
            w.notes[0],
            "ANR in com.example.app (com.example.app/.Main): Input dispatching timed out"
        );
        assert!(w.notes[1].starts_with("native crash: Fatal signal 11"));
        // Another app's process name only shares a prefix.
        assert!(!w.is_app_process("com.example.apple"));
        assert!(w.is_app_process("com.example.app:remote"));
    }

    #[test]
    fn long_notes_are_cut_between_chars() {
        let mut w = Watch::new("com.example.app");
        w.see(
            "10-25 00:20:00.000   500   600 E AndroidRuntime: Process: com.example.app, PID: 4242",
        );
        // 46 bytes before the 3-byte chars: one runs across byte 300.
        let msg = format!("java.lang.RuntimeException: {}", "错".repeat(100));
        w.see(&format!(
            "10-25 00:20:00.000  4242  4242 E AndroidRuntime: {msg}"
        ));
        w.see("10-25 00:20:01.000   500   600 E ActivityManager: ANR in com.example.app");
        // 25 bytes before the 4-byte chars: the same.
        let reason = format!("x{}", "🔥".repeat(80));
        w.see(&format!(
            "10-25 00:20:01.000   500   600 E ActivityManager: Reason: {reason}"
        ));
        assert_eq!(w.notes.len(), 2, "{:?}", w.notes);
        for n in &w.notes {
            assert!(
                n.len() <= NOTE_MAX && n.len() > NOTE_MAX - 4,
                "{} {n}",
                n.len()
            );
        }
        assert!(w.notes[0].starts_with("crash (pid 4242): java.lang.RuntimeException: 错"));
        assert!(w.notes[1].starts_with("ANR in com.example.app: x🔥"));
    }

    #[test]
    fn pidof_output() {
        assert_eq!(parse_pidof("4242 4300\n"), vec![4242, 4300]);
        assert!(parse_pidof("").is_empty());
    }
}
