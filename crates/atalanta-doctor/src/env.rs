//! Where checks read variables and find tools, and how they run them.
//!
//! [`Env::process`] reads the process environment through
//! `velocity_common::env` (so a daemon's installed client environment and
//! `--set` settings count). [`Env::from_vars`] reads a fixed map instead and
//! runs every tool with exactly that environment: tests point `PATH` at
//! fake `adb` / `xcrun` / `xcodebuild` scripts without touching the process
//! environment, which other tests share.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct Env {
    vars: Option<Arc<HashMap<String, String>>>,
}

/// How a command went.
#[derive(Debug)]
pub enum Ran {
    /// It ran (whatever its exit status) in this long.
    Done(Output, Duration),
    /// The program isn't there.
    NotFound,
    /// It didn't finish in time and was killed.
    TimedOut(Duration),
    /// It couldn't be started.
    Error(String),
}

impl Ran {
    /// Stdout of a successful run.
    pub fn stdout(&self) -> Option<String> {
        match self {
            Ran::Done(o, _) if o.status.success() => {
                Some(String::from_utf8_lossy(&o.stdout).into_owned())
            }
            _ => None,
        }
    }

    /// Stdout and stderr together, whatever the status (`None` if it never ran).
    pub fn text(&self) -> Option<String> {
        match self {
            Ran::Done(o, _) => Some(format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )),
            _ => None,
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Ran::Done(o, _) if o.status.success())
    }

    pub fn elapsed(&self) -> Option<Duration> {
        match self {
            Ran::Done(_, d) => Some(*d),
            _ => None,
        }
    }

    /// Why it didn't succeed, in a few words.
    pub fn why(&self) -> String {
        match self {
            Ran::Done(o, _) => {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let msg = if err.is_empty() { out } else { err };
                let msg: String = msg.lines().take(3).collect::<Vec<_>>().join(" | ");
                match o.status.code() {
                    Some(c) if msg.is_empty() => format!("exit {c}"),
                    Some(c) => format!("exit {c}: {msg}"),
                    None => format!("killed: {msg}"),
                }
            }
            Ran::NotFound => "not found".into(),
            Ran::TimedOut(d) => format!("no answer in {:.0} s", d.as_secs_f32()),
            Ran::Error(e) => e.clone(),
        }
    }
}

impl Env {
    /// The process environment (through `velocity_common::env`).
    pub fn process() -> Self {
        Self { vars: None }
    }

    /// Exactly these variables, for lookups and for the tools run.
    pub fn from_vars(vars: HashMap<String, String>) -> Self {
        Self {
            vars: Some(Arc::new(vars)),
        }
    }

    /// A variable, `None` when unset or empty.
    pub fn var(&self, key: &str) -> Option<String> {
        let v = match &self.vars {
            Some(m) => m.get(key).cloned(),
            None => velocity_common::env::var(key).ok(),
        };
        v.filter(|s| !s.trim().is_empty())
    }

    pub fn is_set(&self, key: &str) -> bool {
        self.var(key).is_some()
    }

    /// `$HOME`; when it is unset, the private fallback Velocity itself uses
    /// (`velocity_common::host::home_dir`), never the shared `/tmp`.
    pub fn home(&self) -> PathBuf {
        match self.var("HOME") {
            Some(h) => PathBuf::from(h),
            None => velocity_common::host::home_dir(),
        }
    }

    /// `name` on `PATH` (an executable file), like `which`.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            let p = PathBuf::from(name);
            return is_executable(&p).then_some(p);
        }
        let path = self.var("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| is_executable(p))
    }

    /// Run `program args…` with a deadline, killing it (and its process
    /// group) when it passes.
    pub async fn run(&self, program: &Path, args: &[&str], timeout: Duration) -> Ran {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(vars) = &self.vars {
            cmd.env_clear().envs(vars.iter());
        }
        let started = Instant::now();
        let mut result = velocity_common::process::output_with_timeout(&mut cmd, timeout).await;
        // ETXTBSY: the program was just written and another thread's fork
        // still holds it open for writing until that child execs (a script
        // a test wrote a moment ago). It clears within milliseconds.
        for _ in 0..20 {
            match &result {
                Err(e) if e.raw_os_error() == Some(26) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    result = velocity_common::process::output_with_timeout(&mut cmd, timeout).await;
                }
                _ => break,
            }
        }
        match result {
            Ok(out) => Ran::Done(out, started.elapsed()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ran::NotFound,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ran::TimedOut(timeout),
            Err(e) => Ran::Error(e.to_string()),
        }
    }
}

pub(crate) fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && std::fs::metadata(p)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scratch directory of fake tools for tests.
    use super::*;

    pub struct Tools {
        pub dir: tempfile::TempDir,
        pub vars: HashMap<String, String>,
    }

    impl Tools {
        /// `bin/` on PATH (plus /bin and /usr/bin for the shell
        /// builtins the scripts use), HOME in the scratch dir.
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("bin")).unwrap();
            std::fs::create_dir_all(dir.path().join("home")).unwrap();
            // Only the utilities the fake scripts use, linked into `sys/`:
            // with all of /usr/bin on PATH, a CI runner's real
            // `google-chrome` (or adb, xcrun …) was found before a fake.
            let sys = dir.path().join("sys");
            std::fs::create_dir_all(&sys).unwrap();
            for tool in [
                "sh", "[", "test", "echo", "printf", "cat", "cp", "mv", "rm", "mkdir", "touch",
                "ls", "ln", "chmod", "sleep", "env", "true", "false", "grep", "sed", "awk", "head",
                "tail", "cut", "tr", "wc", "sort", "uniq", "basename", "dirname", "date", "uname",
                "kill", "mktemp", "tee", "xargs", "expr",
            ] {
                if let Some(real) = ["/bin", "/usr/bin"]
                    .iter()
                    .map(|d| Path::new(d).join(tool))
                    .find(|p| p.exists())
                {
                    let _ = std::os::unix::fs::symlink(real, sys.join(tool));
                }
            }
            let mut vars = HashMap::new();
            vars.insert(
                "PATH".into(),
                format!("{}:{}", dir.path().join("bin").display(), sys.display()),
            );
            vars.insert(
                "HOME".into(),
                dir.path().join("home").to_string_lossy().into_owned(),
            );
            Self { dir, vars }
        }

        /// An executable `bin/<name>` running `body` under /bin/sh.
        pub fn tool(&self, name: &str, body: &str) -> PathBuf {
            let p = self.dir.path().join("bin").join(name);
            write_script(&p, body);
            p
        }

        pub fn set(&mut self, k: &str, v: &str) {
            self.vars.insert(k.into(), v.into());
        }

        pub fn env(&self) -> Env {
            Env::from_vars(self.vars.clone())
        }

        pub fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }
    }

    pub fn write_script(p: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        // Written to a temp name and renamed in: a script another test
        // forks while it's open for writing fails with ETXTBSY.
        let tmp = p.with_extension("tmp-write");
        std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, p).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Tools;
    use super::*;

    #[tokio::test]
    async fn tools_are_found_on_the_given_path_and_run_with_a_deadline() {
        let t = Tools::new();
        t.tool("hello", "echo hi \"$FOO\"");
        t.tool("slow", "sleep 5");
        let mut vars = t.vars.clone();
        vars.insert("FOO".into(), "there".into());
        let env = Env::from_vars(vars);
        let hello = env.which("hello").expect("found");
        assert_eq!(
            {
                let r = env.run(&hello, &[], Duration::from_secs(30)).await;
                r.stdout().unwrap_or_else(|| panic!("{r:?} {}", r.why()))
            }
            .trim(),
            "hi there"
        );
        assert!(env.which("nope").is_none());
        let started = Instant::now();
        let slow = env
            .run(&env.which("slow").unwrap(), &[], Duration::from_millis(200))
            .await;
        assert!(matches!(slow, Ran::TimedOut(_)), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            env.run(Path::new("/nonexistent/x"), &[], Duration::from_secs(1))
                .await,
            Ran::NotFound
        ));
    }

    /// With HOME unset the doctor looked in `/tmp/.velocity` (and wrote its
    /// writability probe into the shared `/tmp`), while Velocity uses a
    /// private `velocity-home-<uid>` (audit 2026-11 FS-1).
    #[test]
    fn an_unset_home_never_falls_back_to_the_shared_tmp() {
        let home = Env::from_vars(HashMap::new()).home();
        assert_ne!(home, Path::new("/tmp"));
        assert_ne!(home, std::env::temp_dir());
        assert!(home.is_absolute(), "{home:?}");
    }
}
