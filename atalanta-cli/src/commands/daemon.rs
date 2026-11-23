// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta daemon`: keep devices warm between runs.
//!
//! The daemon holds one driver per platform for its whole life, so what a
//! run would set up and tear down stays up: the Android tree server and
//! animation guard, iOS WDA connections and sessions, the planner cache.
//! `atalanta run --daemon …` sends its arguments, working directory and
//! environment over `~/.atalanta/daemon.sock`; the daemon runs them and
//! streams the output back. The client's environment is installed with
//! `atalanta_common::env::install` for the run, never with `set_var`: the
//! daemon's other threads read the process environment meanwhile.
//!
//! It also serves the agent commands for the web (`atalanta tap @e3 -p
//! web`, `Request::Agent`): Chrome dies with the process that launched it,
//! so a browser that lasts from one command to the next has to live here.
//! Those commands start a daemon themselves when none answers
//! ([`spawn_background`]).
//!
//! Runs are served one at a time, so for each run the daemon points its
//! stdout and stderr at the client's socket — output, colours and reports
//! look exactly like a local run, with no changes to the runner's printing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use atalanta_common::{Platform, ResilientDriver};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

const EXIT_MARKER: &str = "\u{0}ATALANTA-EXIT ";
/// Sent by the daemon when it starts serving a request, so a client can
/// tell "queued behind another run" from "running, nothing printed yet".
const START_MARKER: &str = "\u{0}ATALANTA-START";
/// How long a client waits for the daemon to start its request before it
/// says it is queued.
const QUEUED_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground (start it in the background with `&`
    /// or a process manager)
    Start,
    /// Ask a running daemon to stop
    Stop,
    /// Whether a daemon is running
    Status,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Request {
    Run {
        cwd: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        color: bool,
    },
    /// One agent command (`tap`, `snapshot`, …), argv after `atalanta`.
    Agent {
        cwd: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        color: bool,
        #[serde(default)]
        stdin: Option<String>,
    },
    Stop,
    Ping,
}

/// `run` arguments, parsed on the daemon side.
#[derive(Parser)]
#[command(name = "atalanta run", no_binary_name = true)]
struct RunOnly {
    #[command(flatten)]
    args: super::run::RunArgs,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static POOL: OnceLock<Mutex<HashMap<String, Arc<ResilientDriver>>>> = OnceLock::new();

/// Inside the daemon, reuse its driver for `key`, else keep `fresh`.
/// Outside, always `fresh`. Build `key` with [`pool_key`]: a driver is only
/// reused by a run that would have built the very same one. A driver of
/// the same kind with another configuration (another `web:` window size)
/// is cleaned up and dropped, so the pool doesn't grow a warm Chrome per
/// configuration for the daemon's life.
pub async fn share(key: String, fresh: Arc<ResilientDriver>) -> Arc<ResilientDriver> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return fresh;
    }
    let (driver, evicted) = pool_insert(
        &mut POOL
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
        key,
        fresh,
    );
    for d in evicted {
        atalanta_common::PlatformDriver::cleanup(&*d).await;
    }
    driver
}

/// The pooled driver for `key` (inserting `fresh`), and the drivers of the
/// same kind under another configuration, now removed.
fn pool_insert<D>(
    pool: &mut HashMap<String, Arc<D>>,
    key: String,
    fresh: Arc<D>,
) -> (Arc<D>, Vec<Arc<D>>) {
    if let Some(d) = pool.get(&key) {
        return (d.clone(), Vec::new());
    }
    let kind = pool_kind(&key).to_string();
    let stale: Vec<String> = pool
        .keys()
        .filter(|k| pool_kind(k) == kind)
        .cloned()
        .collect();
    let evicted = stale.iter().filter_map(|k| pool.remove(k)).collect();
    pool.insert(key, fresh.clone());
    (fresh, evicted)
}

/// `platform/mode/framework` of a [`pool_key`].
fn pool_kind(key: &str) -> &str {
    let mut slashes = key.match_indices('/').map(|(i, _)| i);
    match slashes.nth(2) {
        Some(i) => &key[..i],
        None => key,
    }
}

/// The stop handle of the run the daemon is serving, so a client that
/// disconnects stops its run instead of leaving it running unseen.
static RUN_STOP: Mutex<Option<atalanta_common::StopHandle>> = Mutex::new(None);

/// The client of the run being served went away. Sticky for the run: a
/// client that left before the run had a stop handle (while it parsed,
/// bootstrapped WDA or launched Chrome) was forgotten, and so were the
/// later suites of a folder run, each with its own handle (M-5). Reset by
/// [`run_for_client`].
static CLIENT_GONE: AtomicBool = AtomicBool::new(false);

const GONE_REASON: &str = "stopped: the client disconnected";

/// A run is starting (inside the daemon: remember how to stop it, and stop
/// it at once if its client has already gone).
pub fn run_started(stop: &atalanta_common::StopHandle) {
    if active() {
        let mut slot = RUN_STOP.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(stop.clone());
        if CLIENT_GONE.load(Ordering::SeqCst) {
            stop.stop(GONE_REASON);
        }
    }
}

/// Inside the daemon: whether the client of the current run has gone, so
/// a folder run stops between suites.
pub fn client_disconnected() -> bool {
    active() && CLIENT_GONE.load(Ordering::SeqCst)
}

fn client_gone() {
    // Under the lock, so a `run_started` either sees the flag or has
    // stored the handle this stops.
    let slot = RUN_STOP.lock().unwrap_or_else(|e| e.into_inner());
    CLIENT_GONE.store(true, Ordering::SeqCst);
    if let Some(stop) = slot.as_ref() {
        stop.stop(GONE_REASON);
    }
}

/// How long a connection may take to send its request line.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The daemon pool key for a run's driver: platform, driver mode,
/// framework and the driver's configuration. Keyed by platform alone, the
/// first run decided the driver for every later one, so a device-mode run
/// after a `--driver headless` run passed on the headless driver with no
/// device at all.
pub fn pool_key(
    platform: Platform,
    mode: atalanta_common::DriverMode,
    framework: atalanta_common::Framework,
    config: &impl Serialize,
) -> String {
    let config = serde_json::to_string(config).unwrap_or_default();
    format!("{platform}/{mode:?}/{framework:?}/{config}")
}

/// Whether this process is the daemon (drivers outlive runs).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

pub fn socket_path() -> PathBuf {
    atalanta_common::env::var("ATALANTA_DAEMON_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            atalanta_common::host::home_dir()
                .join(".atalanta")
                .join("daemon.sock")
        })
}

pub async fn execute(args: DaemonArgs) -> anyhow::Result<i32> {
    match args.command {
        DaemonCommand::Start => serve().await,
        DaemonCommand::Stop => match send(&Request::Stop) {
            Ok(_) => {
                println!("=> daemon stopped");
                Ok(0)
            }
            Err(e) => {
                println!("=> {}", no_daemon(&e));
                Ok(1)
            }
        },
        DaemonCommand::Status => match send(&Request::Ping) {
            Ok(_) => {
                println!("=> daemon running at {}", socket_path().display());
                Ok(0)
            }
            Err(e) => {
                println!("=> {}", no_daemon(&e));
                Ok(1)
            }
        },
    }
}

/// Why a request got no answer: a busy daemon, or none.
fn no_daemon(e: &anyhow::Error) -> String {
    let busy = e.to_string();
    if busy.contains("busy") {
        busy
    } else {
        "no daemon running".to_string()
    }
}

/// The uid of the process at the other end of a connected Unix socket.
fn peer_uid(fd: std::os::fd::RawFd) -> Option<u32> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is a socket this process owns; uid/gid are valid
        // out-pointers.
        let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        (r == 0).then_some(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writing and `len` is its size.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (r == 0).then_some(cred.uid)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "linux"
    )))]
    {
        let _ = fd;
        None
    }
}

/// Whether the other end of `fd` runs as this user. A run sends the whole
/// environment (API keys) and the daemon runs whatever argv it gets, so
/// both ends check: the socket's 0600 mode is set only after `bind`, and a
/// socket path in a shared directory (or the old HOME-less
/// `/tmp/.atalanta/daemon.sock`) could be bound first by another user
/// (audit 2026-11 FS-3).
fn peer_is_us(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: geteuid has no preconditions and can't fail.
    let me = unsafe { libc::geteuid() };
    peer_uid(fd) == Some(me)
}

/// Connect to the daemon socket, refusing one another user is serving.
fn connect_own(path: &std::path::Path) -> anyhow::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::AsRawFd;
    let s = std::os::unix::net::UnixStream::connect(path)?;
    if !peer_is_us(s.as_raw_fd()) {
        anyhow::bail!(
            "the daemon socket {} is served by another user; refusing to send it this \
             environment (remove it, or set ATALANTA_DAEMON_SOCKET to a path of your own)",
            path.display()
        );
    }
    Ok(s)
}

fn send(req: &Request) -> anyhow::Result<String> {
    let mut s = connect_own(&socket_path())?;
    // A daemon busy with a run answers after it: don't hang on it.
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    s.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    s.write_all(serde_json::to_string(req)?.as_bytes())?;
    s.write_all(b"\n")?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            anyhow::anyhow!("the daemon is busy (a run is in progress)")
        } else {
            e.into()
        }
    })?;
    Ok(out)
}

/// The client's environment as the daemon is sent it. `std::env::vars()`
/// panics on a variable that is not UTF-8 (a damaged locale, a Latin-1
/// path), and this runs in `main` before its panic guard; such a variable
/// is left out, as no `${NAME}` can name it.
fn client_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// Client side of `atalanta run --daemon`. `Ok(None)`: no daemon is
/// running; the caller runs locally.
pub fn client_run(argv: Vec<String>) -> anyhow::Result<Option<i32>> {
    // All of it: suites read their own `${VARS}`, and the socket is the
    // owner's only (0600).
    let req = Request::Run {
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        argv,
        env: client_env(),
        // The client's decision (`--color`, NO_COLOR, its terminal), C4.
        color: atalanta_common::color::enabled(),
    };
    client_stream(&req)
}

/// Client side of a web agent command. `Ok(None)`: no daemon is running.
pub fn client_agent(argv: Vec<String>, stdin: Option<String>) -> anyhow::Result<Option<i32>> {
    let req = Request::Agent {
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        argv,
        // Secrets (`type --secret NAME`) and ATALANTA_SESSION are read
        // from the client's environment.
        env: client_env(),
        color: atalanta_common::color::enabled(),
        stdin,
    };
    client_stream(&req)
}

/// Start `atalanta daemon start` detached from this process (its own
/// process group, output in `~/.atalanta/daemon.log`) and wait until it
/// answers, for at most 10 s.
pub fn spawn_background() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let path = socket_path();
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let log = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("daemon.log"))?
    };
    let log_path = dir.join("daemon.log");
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["daemon", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    eprintln!("=> started atalanta daemon (stop it with `atalanta daemon stop`)");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < until {
        if send(&Request::Ping).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Another client's daemon may have won the race for the socket
            // (and may be busy with a run already: it queues ours).
            if legacy_daemon_present()
                || matches!(take_daemon_lock(&path), Ok(Err(Taken::Locked(_))))
            {
                return Ok(());
            }
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last = log.lines().last().unwrap_or("").to_string();
            anyhow::bail!("atalanta daemon exited at start ({status}): {last}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!(
        "atalanta daemon did not start within 10 s; see {}",
        log_path.display()
    )
}

/// Send `req` and copy the daemon's output to stdout until its exit code.
fn client_stream(req: &Request) -> anyhow::Result<Option<i32>> {
    let path = socket_path();
    let Ok(raw) = std::os::unix::net::UnixStream::connect(&path) else {
        return Ok(None);
    };
    {
        use std::os::fd::AsRawFd;
        if !peer_is_us(raw.as_raw_fd()) {
            anyhow::bail!(
                "the daemon socket {} is served by another user; refusing to send it this \
                 environment (remove it, or set ATALANTA_DAEMON_SOCKET to a path of your own)",
                path.display()
            );
        }
    }
    let mut s = raw;
    s.write_all(serde_json::to_string(req)?.as_bytes())?;
    s.write_all(b"\n")?;
    // The daemon serves one request at a time: say so when ours waits
    // instead of sitting silent until the other run ends.
    let started = Arc::new(AtomicBool::new(false));
    {
        let started = started.clone();
        std::thread::spawn(move || {
            std::thread::sleep(QUEUED_AFTER);
            if !started.load(Ordering::SeqCst) {
                eprintln!(
                    "=> queued: the daemon is busy with another run; this one starts when it \
                     finishes (Ctrl-C to give up, or run without --daemon)"
                );
            }
        });
    }
    let mut reader = BufReader::new(s);
    let mut stdout = std::io::stdout();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        // Any byte means the daemon got to us (an older daemon sends no
        // start marker, only output).
        started.store(true, Ordering::SeqCst);
        if n == 0 {
            // Infrastructure, not a bug in the run: exit 3 so pipelines that
            // retry on device/infra failures retry this too.
            eprintln!(
                "error: the daemon closed the connection before the run finished \
                 (it crashed or was stopped); see {} and run again",
                socket_path().with_file_name("daemon.log").display()
            );
            return Ok(Some(3));
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(code) = text.strip_prefix(EXIT_MARKER) {
            return Ok(Some(code.trim().parse().unwrap_or(5)));
        }
        if text.trim_end() == START_MARKER {
            continue;
        }
        stdout.write_all(&line)?;
        stdout.flush()?;
    }
}

/// The single-instance lock next to the socket: `daemon.sock.lock`.
fn lock_path(socket: &std::path::Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Why `daemon start` must not take over: a daemon already holds the
/// lock, or (an older daemon without the lock) answers or is busy.
#[derive(Debug)]
enum Taken {
    /// The lock is held; the pid it records, when readable.
    Locked(Option<u32>),
    /// A daemon answered a ping, or timed out on it because it is mid-run.
    Answering,
}

/// Take the daemon's single-instance lock for the process's whole life
/// (an advisory `flock`, released by the kernel when the daemon exits, so a
/// crashed daemon never leaves a stale lock). The pid in the file is for
/// the error message only, never trusted for exclusion.
fn take_daemon_lock(socket: &std::path::Path) -> anyhow::Result<Result<std::fs::File, Taken>> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let path = lock_path(socket);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        // Never through a symlink at the lock's name (audit 2026-11 FS-3).
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!("{} is not a plain lock file; remove it", path.display());
    }
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            writeln!(file, "{}", std::process::id())?;
            Ok(Ok(file))
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let pid = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| t.trim().parse().ok());
            Ok(Err(Taken::Locked(pid)))
        }
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::anyhow!(
            "can't lock {}: {e} (set ATALANTA_DAEMON_SOCKET to a path on a local disk)",
            path.display()
        )),
    }
}

/// Whether a daemon without the lock (an older version) is on the socket.
/// A ping that times out is a daemon busy with a run, not an absent one.
fn legacy_daemon_present() -> bool {
    match send(&Request::Ping) {
        Ok(_) => true,
        Err(e) => e.to_string().contains("busy"),
    }
}

fn already_running(path: &std::path::Path, taken: &Taken) -> anyhow::Error {
    let who = match taken {
        Taken::Locked(Some(pid)) => format!(" (pid {pid})"),
        _ => String::new(),
    };
    anyhow::anyhow!(
        "a daemon is already running at {}{who}. \
         Use it with `atalanta run --daemon`, check it with `atalanta daemon status`, \
         or stop it with `atalanta daemon stop` first",
        path.display()
    )
}

async fn serve() -> anyhow::Result<i32> {
    use std::os::unix::fs::PermissionsExt;
    let path = socket_path();
    // Held until the daemon exits: a second `daemon start` fails here
    // instead of unlinking the socket of a daemon busy with a run.
    let _instance = match take_daemon_lock(&path)? {
        Ok(lock) => lock,
        Err(taken) => return Err(already_running(&path, &taken)),
    };
    if legacy_daemon_present() {
        return Err(already_running(&path, &Taken::Answering));
    }
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    // Runs carry the client's environment (API keys): owner only.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    ACTIVE.store(true, Ordering::Relaxed);
    crate::signals::install();
    let seen = crate::signals::count();
    eprintln!("=> atalanta daemon listening on {}", path.display());

    loop {
        // SIGTERM / Ctrl-C stop the daemon through the normal cleanup
        // below, between runs or after the current one (VEL-031).
        if crate::signals::count() > seen {
            break;
        }
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                // One bad connection never ends the daemon (VEL-030).
                Err(e) => {
                    eprintln!("=> daemon: accept failed: {e}");
                    continue;
                }
            },
            signal = crate::signals::after(seen) => {
                eprintln!("=> {signal} received — stopping the daemon (again to skip cleanup)");
                break;
            }
        };
        match serve_one(stream).await {
            Ok(Served::Stop) => break,
            Ok(Served::Done) => {}
            Err(e) => eprintln!("=> daemon: {e}"),
        }
    }

    crate::signals::cleaning_up();
    let drivers: Vec<Arc<ResilientDriver>> = POOL
        .get()
        .map(|p| {
            p.lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for d in drivers {
        atalanta_common::PlatformDriver::cleanup(&*d).await;
    }
    super::agent::cleanup_daemon_drivers().await;
    let _ = std::fs::remove_file(&path);
    Ok(0)
}

enum Served {
    Done,
    Stop,
}

/// Answer one connection. Its request line must come within
/// [`REQUEST_TIMEOUT`]: an idle client held the single accept loop, and so
/// every later `status`, `stop` and run, forever (VEL-029). A line that
/// isn't a request (not JSON, not UTF-8) is answered and dropped.
async fn serve_one(stream: tokio::net::UnixStream) -> anyhow::Result<Served> {
    use std::os::fd::AsRawFd;
    use tokio::io::AsyncBufReadExt;
    if !peer_is_us(stream.as_raw_fd()) {
        anyhow::bail!("refused a connection from another user");
    }
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = Vec::new();
    match tokio::time::timeout(REQUEST_TIMEOUT, reader.read_until(b'\n', &mut line)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => anyhow::bail!("reading a request: {e}"),
        Err(_) => anyhow::bail!("a client sent no request within {REQUEST_TIMEOUT:?}"),
    }
    let mut stream = reader.into_inner().into_std()?;
    stream.set_nonblocking(false)?;
    match serde_json::from_slice::<Request>(&line) {
        Ok(Request::Stop) => {
            // A `daemon stop` that waited for a run, timed out and told its
            // user the daemon was busy has closed its end: stopping now
            // would contradict what it said (m-3).
            if !peer_alive(&stream) {
                eprintln!("=> daemon: a stop request whose client gave up; ignored");
                return Ok(Served::Done);
            }
            let _ = stream.write_all(b"bye\n");
            return Ok(Served::Stop);
        }
        Ok(Request::Ping) => {
            let _ = stream.write_all(b"pong\n");
        }
        Ok(Request::Run {
            cwd,
            argv,
            env,
            color,
        }) => {
            let _ = stream.write_all(format!("{START_MARKER}\n").as_bytes());
            let code = run_for_client(&stream, &cwd, Job::Run(&argv), &env, color).await;
            let _ = stream.write_all(format!("{EXIT_MARKER}{code}\n").as_bytes());
        }
        Ok(Request::Agent {
            cwd,
            argv,
            env,
            color,
            stdin,
        }) => {
            let _ = stream.write_all(format!("{START_MARKER}\n").as_bytes());
            let code = run_for_client(&stream, &cwd, Job::Agent(&argv, stdin), &env, color).await;
            let _ = stream.write_all(format!("{EXIT_MARKER}{code}\n").as_bytes());
        }
        Err(e) => {
            let _ = stream.write_all(format!("bad request: {e}\n").as_bytes());
        }
    }
    Ok(Served::Done)
}

/// Whether the peer of `stream` is still connected: a zero-byte read
/// (EOF) or an error means it closed its end. Clients send nothing after
/// their request line, so pending data means it is there.
fn peer_alive(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut b = [0u8; 1];
    // SAFETY: a non-blocking peek into a 1-byte buffer on a valid fd.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            b.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n > 0 {
        return true;
    }
    if n == 0 {
        return false;
    }
    let e = std::io::Error::last_os_error();
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// What a client asked the daemon to do.
enum Job<'a> {
    Run(&'a [String]),
    /// An agent command, with the client's stdin for `type --stdin`.
    Agent(&'a [String], Option<String>),
}

/// Run one client's `atalanta run` (or agent command), with this process's
/// stdout/stderr, working directory and Atalanta environment pointed at
/// the client.
async fn run_for_client(
    stream: &std::os::unix::net::UnixStream,
    cwd: &str,
    job: Job<'_>,
    env: &[(String, String)],
    color: bool,
) -> i32 {
    let saved_cwd = std::env::current_dir().ok();
    let daemon_color = atalanta_common::color::enabled();
    CLIENT_GONE.store(false, Ordering::SeqCst);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Output goes through a pipe that a thread copies to the client. When
    // the client goes away, the copy stops and the run is stopped; the
    // run's own prints never fail. Pointed straight at the socket, the
    // first print after a disconnect hit EPIPE and panicked the daemon,
    // with no device cleanup (VEL-028).
    let Some((pipe_write, pump)) = pump_to(stream) else {
        return 5;
    };
    // SAFETY: plain fd duplication; the saved fds are restored below and
    // closed exactly once.
    let (out, err) = unsafe { (libc::dup(1), libc::dup(2)) };
    if out < 0 || err < 0 {
        // Out of descriptors: redirecting now would leave 1 and 2 on the
        // pipe for good, and the pump waiting for an EOF that never comes
        // (audit 2026-11). Close what we have; the pump sees EOF and ends.
        // SAFETY: each fd closed here is one this function opened, once.
        unsafe {
            if out >= 0 {
                libc::close(out);
            }
            if err >= 0 {
                libc::close(err);
            }
            libc::close(pipe_write);
        }
        drop(pump);
        eprintln!("=> daemon: out of file descriptors; refused a client run");
        return 5;
    }
    // SAFETY: `pipe_write` is the pipe's write end, ours until closed here;
    // 1 and 2 stay open throughout (now copies of it).
    unsafe {
        libc::dup2(pipe_write, 1);
        libc::dup2(pipe_write, 2);
        libc::close(pipe_write);
    }
    let code = match std::env::set_current_dir(cwd) {
        Err(e) => {
            println!("error: working directory {cwd}: {e}");
            2
        }
        Ok(()) => {
            atalanta_common::env::install(env.iter().cloned().collect());
            colored::control::set_override(color);
            atalanta_common::color::set(color);
            match job {
                // Its own task: a panic ends the command, not the daemon.
                Job::Agent(argv, stdin) => {
                    let argv = argv.to_vec();
                    match tokio::spawn(async move {
                        super::agent::execute_in_daemon(&argv, stdin).await
                    })
                    .await
                    {
                        Ok(code) => code,
                        Err(e) => {
                            println!("error: internal error: the command stopped: {e}");
                            5
                        }
                    }
                }
                Job::Run(argv) => run_parsed(argv).await,
            }
        }
    };
    finish_client_io(out, err, pump, saved_cwd, daemon_color).await;
    code
}

/// Parse and run one `atalanta run`.
async fn run_parsed(argv: &[String]) -> i32 {
    match RunOnly::try_parse_from(argv) {
        // Its own task: a panic in the run ends the run (exit 5),
        // not the daemon.
        Ok(parsed) => match tokio::spawn(super::run::execute(parsed.args)).await {
            Ok(Ok(code)) => code,
            Ok(Err(e)) => {
                println!("error: {}", crate::redacted(&e));
                super::exit_code_for(&e)
            }
            Err(e) => {
                println!("error: internal error: the run stopped: {e}");
                5
            }
        },
        Err(e) => {
            println!("{e}");
            2
        }
    }
}

/// Undo [`run_for_client`]'s redirection and environment.
async fn finish_client_io(
    out: libc::c_int,
    err: libc::c_int,
    pump: std::thread::JoinHandle<()>,
    saved_cwd: Option<PathBuf>,
    daemon_color: bool,
) {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: `out` and `err` are the fds saved above, restored onto 1 and 2
    // and closed once, here.
    unsafe {
        libc::dup2(out, 1);
        libc::dup2(err, 2);
        libc::close(out);
        libc::close(err);
    }
    // The pipe's last write end is gone: the copy finishes (bounded: its
    // writes to the client time out). Off the runtime's workers.
    let _ = tokio::task::spawn_blocking(move || pump.join()).await;
    *RUN_STOP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    colored::control::unset_override();
    atalanta_common::color::set(daemon_color);
    atalanta_common::env::clear();
    atalanta_common::env::set_settings(Default::default());
    if let Some(c) = saved_cwd {
        let _ = std::env::set_current_dir(c);
    }
}

/// How long one write to the client may block before the client counts
/// as gone: a client that stops reading without closing (a suspended
/// terminal) blocked the pump, and the daemon with it, forever (m-2).
const CLIENT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A pipe whose read end a thread copies to `stream` until EOF: (write
/// end, the thread). The thread also watches the socket, so a client that
/// disconnects while the run prints nothing (parsing, WDA bootstrap, a
/// long wait) stops the run at once; after that it keeps draining the
/// pipe, discarding.
fn pump_to(
    stream: &std::os::unix::net::UnixStream,
) -> Option<(std::os::fd::RawFd, std::thread::JoinHandle<()>)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut client = stream.try_clone().ok()?;
    client.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT)).ok()?;
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills both fds on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // Close-on-exec, both ends: every child the run spawns (the pooled
    // Chrome, an adb server, xcodebuild) inherited them and held the pipe
    // open for its life (m-1). macOS has no pipe2. `dup2` onto 1 and 2
    // clears the flag on those copies, as it should.
    for fd in fds {
        // SAFETY: fcntl on fds we just created.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    // SAFETY: the read end is ours alone from here on.
    let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut connected = true;
        loop {
            if connected {
                // Output to copy, or the client hanging up.
                let mut polls = [
                    libc::pollfd {
                        fd: read.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: client.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: two valid pollfds.
                let n = unsafe { libc::poll(polls.as_mut_ptr(), 2, -1) };
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                } else if polls[1].revents != 0 && !peer_alive(&client) {
                    connected = false;
                    client_gone();
                    continue;
                } else if polls[0].revents == 0 {
                    // Bytes from the client (it sends none): drop them.
                    let _ = client.read(&mut buf);
                    continue;
                }
            }
            let n = match read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if connected && client.write_all(&buf[..n]).is_err() {
                connected = false;
                client_gone();
            }
        }
    });
    Some((fds[1], thread))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atalanta_common::{DriverMode, Framework};

    /// Audit 2026-11 FS-3: both ends check who is on the other side.
    #[test]
    fn the_peer_of_a_socket_pair_is_this_user() {
        use std::os::fd::AsRawFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(peer_is_us(a.as_raw_fd()));
        assert_eq!(peer_uid(-1), None);
    }

    /// Audit 2026-11 FS-3: a symlink at the lock's name is refused, and
    /// its target left alone.
    #[test]
    fn a_symlinked_daemon_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let socket = dir.path().join("daemon.sock");
        std::os::unix::fs::symlink(&victim, lock_path(&socket)).unwrap();
        assert!(take_daemon_lock(&socket).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }

    // A second `daemon start` must fail on the lock, whatever the first
    // daemon is doing (a ping to a daemon mid-run times out).
    #[test]
    fn a_second_daemon_cannot_take_the_lock_while_the_first_holds_it() {
        let dir = std::env::temp_dir().join(format!("atalanta-daemon-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = dir.join("daemon.sock");
        let first = take_daemon_lock(&socket)
            .unwrap()
            .expect("first daemon locks");
        match take_daemon_lock(&socket).unwrap() {
            Err(Taken::Locked(pid)) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("second daemon got {other:?}"),
        }
        let msg = already_running(&socket, &Taken::Locked(Some(42))).to_string();
        assert!(
            msg.contains("pid 42") && msg.contains("atalanta daemon stop"),
            "{msg}"
        );
        drop(first);
        // Another test spawning a child at this moment can hold a forked
        // copy of the descriptor (and so the flock) until its exec closes
        // it: give that window a moment rather than fail on it.
        let released = (0..50).any(|_| {
            let ok = take_daemon_lock(&socket).unwrap().is_ok();
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            ok
        });
        assert!(released, "released on exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // VEL-002: a device-mode run must not get the headless driver an
    // earlier run left in the pool (nor one built from another config).
    // R-m3: another configuration of the same driver kind replaces the
    // old driver (cleaned up) instead of growing the pool for good.
    #[test]
    fn a_new_config_of_the_same_driver_kind_evicts_the_old_one() {
        let key = |p, m, c: serde_json::Value| pool_key(p, m, Framework::Native, &c);
        let web_a = key(
            Platform::Web,
            DriverMode::Device,
            serde_json::json!({"width": 1}),
        );
        let web_b = key(
            Platform::Web,
            DriverMode::Device,
            serde_json::json!({"width": 2}),
        );
        let ios = key(Platform::Ios, DriverMode::Device, serde_json::Value::Null);
        let mut pool: HashMap<String, Arc<u8>> = HashMap::new();
        let (a, ev) = pool_insert(&mut pool, web_a.clone(), Arc::new(1));
        assert!(ev.is_empty());
        let (again, ev) = pool_insert(&mut pool, web_a.clone(), Arc::new(9));
        assert!(Arc::ptr_eq(&a, &again) && ev.is_empty());
        let _ = pool_insert(&mut pool, ios.clone(), Arc::new(3));
        let (_, ev) = pool_insert(&mut pool, web_b.clone(), Arc::new(2));
        assert_eq!(ev.len(), 1);
        assert!(Arc::ptr_eq(&ev[0], &a));
        let mut keys: Vec<_> = pool.keys().cloned().collect();
        keys.sort();
        let mut want = vec![web_b, ios];
        want.sort();
        assert_eq!(keys, want);
    }

    // m-3: a `daemon stop` that gave up (busy) has closed its end; the
    // daemon checks before stopping.
    /// VEL-030: a request line that isn't UTF-8 (or JSON) is answered and
    /// dropped; it used to end the daemon.
    #[tokio::test]
    async fn a_malformed_request_is_answered_and_the_daemon_serves_on() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for bad in [&b"\xff\xfe{}\n"[..], b"not json\n"] {
            let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
            client.write_all(bad).await.unwrap();
            let served = serve_one(server).await;
            assert!(matches!(served, Ok(Served::Done)), "{bad:?}");
            let mut reply = String::new();
            client.read_to_string(&mut reply).await.unwrap();
            assert!(reply.starts_with("bad request"), "{reply}");
        }
        // And the next request is served.
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        client.write_all(b"{\"kind\":\"ping\"}\n").await.unwrap();
        assert!(matches!(serve_one(server).await, Ok(Served::Done)));
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "pong\n");
    }

    /// VEL-029: a client that connects and says nothing is dropped after
    /// REQUEST_TIMEOUT (5 s, real time) instead of holding the accept loop
    /// for good.
    #[tokio::test]
    async fn an_idle_client_is_dropped_after_the_request_timeout() {
        let (server, _client) = tokio::net::UnixStream::pair().unwrap();
        let served = tokio::time::timeout(REQUEST_TIMEOUT * 2, serve_one(server))
            .await
            .expect("serve_one returned");
        let e = served.err().expect("an idle client is an error");
        assert!(e.to_string().contains("no request"), "{e}");
    }

    #[test]
    fn a_closed_peer_is_told_apart_from_a_quiet_one() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(peer_alive(&a), "connected and quiet");
        drop(b);
        assert!(!peer_alive(&a), "the peer closed");
    }

    // m-1: the output pipe is close-on-exec, so children the run spawns
    // don't hold it open; and the pump ends when the write end closes.
    #[test]
    fn the_output_pipe_is_close_on_exec() {
        let (a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (write, pump) = pump_to(&a).unwrap();
        // SAFETY: F_GETFD on an fd we own.
        let flags = unsafe { libc::fcntl(write, libc::F_GETFD) };
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "write end inherited by children"
        );
        // SAFETY: writing to and closing the fd we own.
        unsafe {
            libc::write(write, b"hi\n".as_ptr().cast(), 3);
            libc::close(write);
        }
        pump.join().unwrap();
        drop(a);
        let mut out = String::new();
        b.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn the_pool_key_tells_driver_modes_and_configs_apart() {
        let cfg = serde_json::json!({ "width": 400 });
        let key = |m, c: &serde_json::Value| pool_key(Platform::Android, m, Framework::Native, c);
        let headless = key(DriverMode::Headless, &cfg);
        assert_ne!(headless, key(DriverMode::Device, &cfg));
        assert_ne!(
            headless,
            key(DriverMode::Headless, &serde_json::json!({ "width": 800 }))
        );
        assert_eq!(headless, key(DriverMode::Headless, &cfg));
        assert_ne!(
            headless,
            pool_key(
                Platform::Android,
                DriverMode::Headless,
                Framework::Flutter,
                &cfg
            )
        );
    }

    #[test]
    fn an_agent_request_round_trips_and_old_lines_have_no_stdin() {
        let req = Request::Agent {
            cwd: "/w".into(),
            argv: vec!["tap".into(), "@e3".into(), "-p".into(), "web".into()],
            env: vec![("ATALANTA_SESSION".into(), "a".into())],
            color: false,
            stdin: Some("typed text\n".into()),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains(r#""kind":"agent""#), "{line}");
        match serde_json::from_str::<Request>(&line).unwrap() {
            Request::Agent {
                cwd,
                argv,
                env,
                color,
                stdin,
            } => {
                assert_eq!(cwd, "/w");
                assert_eq!(argv, ["tap", "@e3", "-p", "web"]);
                assert_eq!(env, [("ATALANTA_SESSION".to_string(), "a".to_string())]);
                assert!(!color);
                assert_eq!(stdin.as_deref(), Some("typed text\n"));
            }
            _ => panic!("not an agent request: {line}"),
        }
        let old = r#"{"kind":"agent","cwd":"/w","argv":["snapshot"],"env":[],"color":true}"#;
        match serde_json::from_str::<Request>(old).unwrap() {
            Request::Agent { stdin, argv, .. } => {
                assert_eq!(stdin, None);
                assert_eq!(argv, ["snapshot"]);
            }
            _ => panic!("not an agent request"),
        }
    }
}
