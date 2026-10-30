//! `velocity daemon`: keep devices warm between runs.
//!
//! The daemon holds one driver per platform for its whole life, so what a
//! run would set up and tear down stays up: the Android tree server and
//! animation guard, iOS WDA connections and sessions, the planner cache.
//! `velocity run --daemon …` sends its arguments, working directory and
//! environment over `~/.velocity/daemon.sock`; the daemon runs them and
//! streams the output back. The client's environment is installed with
//! `velocity_common::env::install` for the run, never with `set_var`: the
//! daemon's other threads read the process environment meanwhile.
//!
//! Runs are served one at a time, so for each run the daemon points its
//! stdout and stderr at the client's socket — output, colours and reports
//! look exactly like a local run, with no changes to the runner's printing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use velocity_common::{Platform, ResilientDriver};

const EXIT_MARKER: &str = "\u{0}VELOCITY-EXIT ";

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
    Stop,
    Ping,
}

/// `run` arguments, parsed on the daemon side.
#[derive(Parser)]
#[command(name = "velocity run", no_binary_name = true)]
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
        velocity_common::PlatformDriver::cleanup(&*d).await;
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
static RUN_STOP: Mutex<Option<velocity_common::StopHandle>> = Mutex::new(None);

/// The client of the run being served went away. Sticky for the run: a
/// client that left before the run had a stop handle (while it parsed,
/// bootstrapped WDA or launched Chrome) was forgotten, and so were the
/// later suites of a folder run, each with its own handle (M-5). Reset by
/// [`run_for_client`].
static CLIENT_GONE: AtomicBool = AtomicBool::new(false);

const GONE_REASON: &str = "stopped: the client disconnected";

/// A run is starting (inside the daemon: remember how to stop it, and stop
/// it at once if its client has already gone).
pub fn run_started(stop: &velocity_common::StopHandle) {
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
    mode: velocity_common::DriverMode,
    framework: velocity_common::Framework,
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
    velocity_common::env::var("VELOCITY_DAEMON_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(velocity_common::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".velocity")
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

fn send(req: &Request) -> anyhow::Result<String> {
    let mut s = std::os::unix::net::UnixStream::connect(socket_path())?;
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

/// Client side of `velocity run --daemon`. `Ok(None)`: no daemon is
/// running; the caller runs locally.
pub fn client_run(argv: Vec<String>) -> anyhow::Result<Option<i32>> {
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket_path()) else {
        return Ok(None);
    };
    // All of it: suites read their own `${VARS}`, and the socket is the
    // owner's only (0600).
    let env = std::env::vars().collect();
    let req = Request::Run {
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        argv,
        env,
        // The client's decision (`--color`, NO_COLOR, its terminal), C4.
        color: velocity_common::color::enabled(),
    };
    s.write_all(serde_json::to_string(&req)?.as_bytes())?;
    s.write_all(b"\n")?;
    let mut reader = BufReader::new(s);
    let mut stdout = std::io::stdout();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            anyhow::bail!("the daemon closed the connection before the run finished");
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(code) = text.strip_prefix(EXIT_MARKER) {
            return Ok(Some(code.trim().parse().unwrap_or(5)));
        }
        stdout.write_all(&line)?;
        stdout.flush()?;
    }
}

async fn serve() -> anyhow::Result<i32> {
    use std::os::unix::fs::PermissionsExt;
    let path = socket_path();
    if send(&Request::Ping).is_ok() {
        anyhow::bail!("a daemon is already running at {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    // Runs carry the client's environment (API keys): owner only.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    ACTIVE.store(true, Ordering::Relaxed);
    crate::signals::install();
    let seen = crate::signals::count();
    eprintln!("=> velocity daemon listening on {}", path.display());

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
        velocity_common::PlatformDriver::cleanup(&*d).await;
    }
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
    use tokio::io::AsyncBufReadExt;
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
            let code = run_for_client(&stream, &cwd, &argv, &env, color).await;
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

/// Run one client's `velocity run`, with this process's stdout/stderr,
/// working directory and Velocity environment pointed at the client.
async fn run_for_client(
    stream: &std::os::unix::net::UnixStream,
    cwd: &str,
    argv: &[String],
    env: &[(String, String)],
    color: bool,
) -> i32 {
    let saved_cwd = std::env::current_dir().ok();
    let daemon_color = velocity_common::color::enabled();
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
            velocity_common::env::install(env.iter().cloned().collect());
            colored::control::set_override(color);
            velocity_common::color::set(color);
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
    };
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
    velocity_common::color::set(daemon_color);
    velocity_common::env::clear();
    velocity_common::env::set_settings(Default::default());
    if let Some(c) = saved_cwd {
        let _ = std::env::set_current_dir(c);
    }
    code
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
    use velocity_common::{DriverMode, Framework};

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
}
