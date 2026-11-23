//! Ctrl-C / SIGTERM / SIGHUP for the whole process, armed once and kept
//! armed.
//!
//! Each run used to install its own handler for the run only: once tokio
//! had replaced the default handlers, a signal outside that window (writing
//! reports, cleaning up devices, the daemon between runs) was swallowed and
//! a hung cleanup could only be SIGKILLed. Now one watcher counts signals
//! for the life of the process:
//! - the first one asks the run (or the daemon) to stop and clean up;
//! - a second one, or any signal once cleanup has started, SIGKILLs the
//!   process groups of the commands still running and exits at once.
//!
//! The exit code is the shell's `128 + signal` (D-20): 130 for Ctrl-C, 143
//! for SIGTERM (how CI and Kubernetes cancel a job), 129 for SIGHUP. A
//! second Ctrl-C within [`DEBOUNCE`] of the first is ignored: `npx` and
//! `yarn` forward one Ctrl-C twice, which skipped cleanup and the reports.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// A signal: its name as printed, and its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    pub name: &'static str,
    pub number: i32,
}

impl Signal {
    pub const INT: Signal = Signal {
        name: "Ctrl-C",
        number: 2,
    };
    pub const TERM: Signal = Signal {
        name: "SIGTERM",
        number: 15,
    };
    pub const HUP: Signal = Signal {
        name: "SIGHUP",
        number: 1,
    };

    /// `128 + n`, as a shell reports a process killed by it.
    pub fn exit_code(self) -> i32 {
        128 + self.number
    }

    /// The signal's own name (`SIGINT`, not `Ctrl-C`), for reports.
    pub fn sig_name(self) -> &'static str {
        match self.number {
            2 => "SIGINT",
            15 => "SIGTERM",
            1 => "SIGHUP",
            _ => self.name,
        }
    }
}

/// Signals received so far, and the first one (the one that stopped the run).
static STATE: OnceLock<watch::Sender<(u32, Option<Signal>)>> = OnceLock::new();
static CLEANING_UP: AtomicBool = AtomicBool::new(false);

/// A Ctrl-C this soon after the previous one is the same Ctrl-C forwarded
/// twice (npm, yarn), not a request to skip cleanup.
pub const DEBOUNCE: Duration = Duration::from_secs(1);

/// Tells a Ctrl-C forwarded twice from a second, real one.
#[derive(Default)]
struct Debounce {
    /// The last Ctrl-C taken as a real one.
    accepted: Option<Instant>,
}

impl Debounce {
    /// Whether a Ctrl-C at `now` is a real one. The window runs from the
    /// last *accepted* press: measured from every press, it slid forward
    /// on each ignored one, so pressing Ctrl-C every half second at a hung
    /// cleanup never escalated (review d4 m1).
    fn accept(&mut self, now: Instant) -> bool {
        if self
            .accepted
            .is_some_and(|t| now.saturating_duration_since(t) < DEBOUNCE)
        {
            return false;
        }
        self.accepted = Some(now);
        true
    }
}

/// The exit code after a signal: `128 + n` of the first signal received
/// (130 when none was, e.g. a daemon client that went away).
pub fn exit_code() -> i32 {
    first().map_or(Signal::INT.exit_code(), Signal::exit_code)
}

/// The first signal received, if any.
pub fn first() -> Option<Signal> {
    STATE.get().and_then(|s| s.borrow().1)
}

/// Start the watcher (once per process; later calls do nothing). Needs a
/// tokio runtime.
pub fn install() {
    STATE.get_or_init(|| {
        let (tx, _) = watch::channel((0, None));
        let state = tx.clone();
        tokio::spawn(async move {
            let mut next = Listener::new();
            let mut debounce = Debounce::default();
            loop {
                let signal = next.recv().await;
                if signal == Signal::INT && !debounce.accept(Instant::now()) {
                    continue;
                }
                let mut count = 0;
                state.send_modify(|s| {
                    s.0 += 1;
                    s.1.get_or_insert(signal);
                    count = s.0;
                });
                if count >= 2 || CLEANING_UP.load(Ordering::SeqCst) {
                    eprintln!(
                        "\n=> {} again — killing running commands and exiting without finishing cleanup",
                        signal.name
                    );
                    // `process::exit` runs no destructors: the commands'
                    // group guards never fire, so kill the groups here (and
                    // the registered pids, such as the web driver's Chrome).
                    atalanta_common::process::kill_all_groups();
                    // `process::exit` skips the `::stop-commands::` guard's
                    // drop: without this the runner ignored every workflow
                    // command for the rest of the step (m-1). A cancelled
                    // GitHub job sends SIGINT then SIGTERM, so this path is
                    // the common one there.
                    atalanta_runner::reporter::github::resume_commands();
                    std::process::exit(signal.exit_code());
                }
            }
        });
        tx
    });
}

/// Signals received so far.
pub fn count() -> u32 {
    STATE.get().map_or(0, |s| s.borrow().0)
}

/// Resolves with the signal's name once more than `seen` signals have
/// arrived (pass [`count`] from before the work started).
pub async fn after(seen: u32) -> &'static str {
    install();
    let mut rx = STATE.get().expect("installed").subscribe();
    let name = rx
        .wait_for(|s| s.0 > seen)
        .await
        .map(|s| s.1.map_or("a signal", |sig| sig.name));
    match name {
        Ok(name) => name,
        Err(_) => std::future::pending().await,
    }
}

/// From now on any signal exits at once: cleanup is under way, and a
/// signal now means it hangs.
pub fn cleaning_up() {
    CLEANING_UP.store(true, Ordering::SeqCst);
}

/// A run (one suite) is starting: the previous one's cleanup is over, so a
/// signal again asks for a clean stop. Without this, every suite after the
/// first in a folder run exited at once on Ctrl-C, skipping cleanup.
pub fn run_starting() {
    CLEANING_UP.store(false, Ordering::SeqCst);
}

/// Ctrl-C, and (Unix) SIGTERM and SIGHUP, registered once and held: a
/// `ctrl_c()` made anew on each wait could miss a Ctrl-C that arrived in
/// between, so the "again to skip cleanup" press needed a third (R-m6).
struct Listener {
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl Listener {
    fn new() -> Self {
        #[cfg(unix)]
        use tokio::signal::unix::{signal, SignalKind};
        Self {
            #[cfg(unix)]
            int: signal(SignalKind::interrupt()).ok(),
            #[cfg(unix)]
            term: signal(SignalKind::terminate()).ok(),
            #[cfg(unix)]
            hup: signal(SignalKind::hangup()).ok(),
        }
    }

    /// The next signal.
    async fn recv(&mut self) -> Signal {
        #[cfg(unix)]
        if let (Some(int), Some(term), Some(hup)) = (&mut self.int, &mut self.term, &mut self.hup) {
            return tokio::select! {
                _ = int.recv() => Signal::INT,
                _ = term.recv() => Signal::TERM,
                _ = hup.recv() => Signal::HUP,
            };
        }
        let _ = tokio::signal::ctrl_c().await;
        Signal::INT
    }
}

#[cfg(test)]
mod tests {
    use super::{Debounce, Signal, DEBOUNCE};
    use std::time::{Duration, Instant};

    #[test]
    fn repeated_presses_escalate_once_the_window_from_the_first_ends() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut d = Debounce::default();
        assert!(d.accept(at(0)));
        // The forwarded echo, then presses every 400 ms.
        assert!(!d.accept(at(10)));
        assert!(!d.accept(at(400)));
        assert!(!d.accept(at(800)));
        // 1 s after the accepted press: a real second Ctrl-C, though the
        // last ignored one was only 400 ms ago.
        assert!(d.accept(at(DEBOUNCE.as_millis() as u64 + 200)));
    }

    #[cfg(unix)]
    #[test]
    fn signal_numbers_match_the_platform() {
        assert_eq!(Signal::INT.number, libc::SIGINT);
        assert_eq!(Signal::TERM.number, libc::SIGTERM);
        assert_eq!(Signal::HUP.number, libc::SIGHUP);
        assert_eq!(Signal::TERM.exit_code(), 143);
        assert_eq!(Signal::HUP.exit_code(), 129);
        assert_eq!(Signal::INT.exit_code(), 130);
    }
}
