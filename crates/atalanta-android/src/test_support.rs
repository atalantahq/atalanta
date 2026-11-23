//! A fake `adb` for tests: `adb shell <cmd>` runs `<cmd>` with the host's
//! `/bin/sh`, as the device's shell would, with device tools (`input`,
//! `am`, `pm`, `monkey`, `run-as`, `uiautomator`, …) replaced by scripts
//! that log their arguments. No device or emulator is touched.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// The token a fake device's tree server writes (protocol 13).
pub(crate) const FAKE_TREE_TOKEN: &str = "0123456789abcdef0123456789abcdef";

pub(crate) struct FakeAdb {
    pub dir: PathBuf,
    pub adb: PathBuf,
    log: PathBuf,
}

impl FakeAdb {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vel-fake-adb-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("device")).unwrap();
        let log = dir.join("adb.log");
        let adb = dir.join("adb");
        let script = format!(
            r#"#!/bin/bash
export LOG='{log}' BIN='{bin}' DEV='{dev}' ATALANTA_DUMP_DIR='{dev}'
echo "ARGV:$(printf '[%s]' "$@")" >> "$LOG"
while [ "$1" = "-s" ] || [ "$1" = "-H" ] || [ "$1" = "-P" ]; do shift 2; done
case "$1" in
  devices) echo "List of devices attached"; echo "emulator-5554 device product:x model:Fake"; exit 0;;
  shell|exec-out) shift; cmd="$*"; echo "DEVICE_SH:$cmd" >> "$LOG"
    cd "$DEV" && PATH="$BIN:$PATH" exec /bin/sh -c "$cmd";;
  emu) shift; echo "EMU:$*" >> "$LOG"; echo OK; exit 0;;
  *) [ -x "$BIN/adb-$1" ] && exec "$BIN/adb-$1" "$@"; exit 0;;
esac
"#,
            log = log.display(),
            bin = dir.join("bin").display(),
            dev = dir.join("device").display(),
        );
        write_exec(&adb, &script);
        let me = Self { dir, adb, log };
        for tool in [
            "input", "am", "monkey", "run-as", "dumpsys", "cmd", "logcat",
        ] {
            me.stub(tool, "");
        }
        // Every app is installed, as `pm path` says on a device.
        me.stub(
            "pm",
            "case \"$1\" in path) echo \"package:/data/app/$2/base.apk\";; esac",
        );
        me.stub("getprop", "echo 1");
        // The tree server's token file (protocol 13), next to `$DEV` (tests
        // expect the device directory to hold only what they put there).
        me.stub(
            "cat",
            "case \"$1\" in /data/local/tmp/atalanta-tree.token) exec /bin/cat \"$DEV/../atalanta-tree.token\";; esac; exec /bin/cat \"$@\"",
        );
        std::fs::write(
            me.dir.join("atalanta-tree.token"),
            format!("{FAKE_TREE_TOKEN}\n"),
        )
        .unwrap();
        me
    }

    /// Replace device tool `name` with a script that logs its arguments
    /// and then runs `body`.
    pub fn stub(&self, name: &str, body: &str) {
        let tag = name.to_uppercase().replace('-', "_");
        write_exec(
            &self.dir.join("bin").join(name),
            &format!(
                "#!/bin/sh\nprintf '{tag}:' >>\"$LOG\"; for a in \"$@\"; do printf '[%s]' \"$a\" >>\"$LOG\"; done; echo >>\"$LOG\"\n{body}\n"
            ),
        );
    }

    pub fn path(&self) -> String {
        self.adb.to_string_lossy().into_owned()
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Lines the tool `name` logged.
    pub fn calls(&self, name: &str) -> Vec<String> {
        let tag = format!("{}:", name.to_uppercase().replace('-', "_"));
        self.log()
            .lines()
            .filter(|l| l.starts_with(&tag))
            .map(String::from)
            .collect()
    }
}

impl Drop for FakeAdb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_exec(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The device `ATALANTA_TEST_ANDROID` names for the device-backed tests;
/// `None` skips them. With `ATALANTA_TEST_STRICT=1` (a device CI job) a
/// missing device fails the test instead: a skipped device test used to
/// report PASS (review 07 M6).
pub(crate) fn device_under_test() -> Option<String> {
    match std::env::var("ATALANTA_TEST_ANDROID") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            assert!(
                std::env::var("ATALANTA_TEST_STRICT").as_deref() != Ok("1"),
                "ATALANTA_TEST_STRICT=1 but ATALANTA_TEST_ANDROID names no device"
            );
            None
        }
    }
}

/// Everything `tracing` events say while `f` runs on this thread, one line
/// per event (`field=value` pairs), at every level. For checking what the
/// crate logs; build the runtime inside `f` so its tasks run here too.
pub(crate) fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    use std::fmt::Write;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};

    struct Lines(Arc<Mutex<String>>);
    struct Fields<'a>(&'a mut String);
    impl Visit for Fields<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }
    impl tracing::Subscriber for Lines {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut line = String::new();
            event.record(&mut Fields(&mut line));
            let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
            all.push_str(&line);
            all.push('\n');
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let sink = Arc::new(Mutex::new(String::new()));
    let out = tracing::subscriber::with_default(Lines(sink.clone()), f);
    let text = sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (out, text)
}
