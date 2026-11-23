//! Machine-readable failures for agents: a stable `code`, whether the call
//! may simply be repeated (`retry`), and the next thing to do (`next`).
//!
//! An agent reading prose has to guess whether a failed tap reached the
//! device. These fields say it: `safe` means nothing happened that repeating
//! could double (a read, or an action refused before it touched the
//! device); `observe_first` means the effect is unknown, so look at the
//! screen before repeating; `no` means repeating the same call fails the
//! same way, so change it (a new snapshot, other arguments).
//!
//! MCP results carry them as `structuredContent.error`; `atalanta agent`
//! prints them in its JSON envelope. Codes are part of the agent interface:
//! add new ones, never rename one.

use atalanta_common::{AtalantaError, ErrorKind};
use serde_json::{json, Value};

/// Whether a failed call can be sent again as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Nothing happened that repeating could double.
    Safe,
    /// The effect is unknown: observe the screen, then decide.
    ObserveFirst,
    /// Repeating it fails the same way: change the call.
    No,
}

impl Retry {
    pub fn as_str(self) -> &'static str {
        match self {
            Retry::Safe => "safe",
            Retry::ObserveFirst => "observe_first",
            Retry::No => "no",
        }
    }
}

/// A classified failure.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: &'static str,
    pub retry: Retry,
    pub next: Option<&'static str>,
    pub message: String,
    /// The call changed the device before it failed (a scroll that never
    /// reached its target still scrolled).
    pub moved: bool,
}

impl Failure {
    pub fn new(
        code: &'static str,
        retry: Retry,
        next: Option<&'static str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            retry,
            next,
            message: message.into(),
            moved: false,
        }
    }

    /// Adjust for the tool that failed. For a tool that only reads, an
    /// unknown effect is no effect: it may be repeated. A scroll that never
    /// found its target scrolled all the same.
    pub fn for_tool(mut self, tool: &str, read_only: bool) -> Self {
        if read_only && self.retry == Retry::ObserveFirst {
            self.retry = Retry::Safe;
        }
        if tool == "scroll" && self.code == "NOT_FOUND" {
            self.moved = true;
            self.next = Some("the screen scrolled: take a snapshot before deciding what next");
        }
        self
    }

    /// Whether the call reached the device: `yes` (it changed the screen
    /// before failing), `no` (refused before it did, or only read), or
    /// `unknown`.
    pub fn delivered(&self) -> &'static str {
        if self.moved {
            return "yes";
        }
        match (self.code, self.retry) {
            ("APP_CRASHED", _) | (_, Retry::ObserveFirst) => "unknown",
            _ => "no",
        }
    }

    /// `{code, message, retry, delivered, next?}`; the message masked like
    /// every other output.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "code": self.code,
            "message": atalanta_common::redact::redact(&self.message),
            "retry": self.retry.as_str(),
            "delivered": self.delivered(),
        });
        if let Some(n) = self.next {
            v["next"] = n.into();
        }
        v
    }
}

pub const NEXT_SNAPSHOT: &str = "take a new snapshot and use a current ref";
const NEXT_LOOK: &str = "take a snapshot; scroll if the element may be off screen";
const NEXT_OBSERVE: &str = "take a snapshot to see whether it happened before repeating it";
const NEXT_ARGS: &str = "fix the arguments; the message says which";
const NEXT_DOCTOR: &str = "run `atalanta doctor` (MCP: diagnose)";
const NEXT_WAIT: &str = "wait for the element to be enabled and still, then repeat";

/// The code, retry and next step for `e`.
pub fn classify(e: &AtalantaError) -> Failure {
    let message = e.to_string();
    let inner = match e {
        AtalantaError::AtSource { inner, .. } => inner.as_ref(),
        other => other,
    };
    let (code, retry, next) = match inner {
        AtalantaError::StaleRef(_) => ("STALE_REF", Retry::No, Some(NEXT_SNAPSHOT)),
        AtalantaError::ElementNotFound { .. } => ("NOT_FOUND", Retry::No, Some(NEXT_LOOK)),
        AtalantaError::NotActionable { .. } => ("NOT_ACTIONABLE", Retry::Safe, Some(NEXT_WAIT)),
        AtalantaError::AssertionFailed { .. } | AtalantaError::HttpCheckFailed { .. } => {
            ("ASSERTION_FAILED", Retry::No, None)
        }
        AtalantaError::AppCrashed { .. } => (
            "APP_CRASHED",
            Retry::No,
            Some("relaunch the app; the crash log path is in the message"),
        ),
        AtalantaError::YamlParse { .. }
        | AtalantaError::UnknownFlowRef { .. }
        | AtalantaError::InvalidSelector { .. }
        | AtalantaError::MissingEnvVars { .. }
        | AtalantaError::Config(_) => ("INVALID_ARGS", Retry::No, Some(NEXT_ARGS)),
        AtalantaError::DeviceNotFound { .. } => (
            "DEVICE_NOT_FOUND",
            Retry::No,
            Some("list devices (`atalanta devices`) and pass one with --device"),
        ),
        AtalantaError::DeviceLocked { .. } => {
            ("DEVICE_LOCKED", Retry::No, Some("unlock the device"))
        }
        AtalantaError::CircuitOpen { .. } | AtalantaError::RunnerFailing { .. } => {
            ("DEVICE_UNRESPONSIVE", Retry::No, Some(NEXT_DOCTOR))
        }
        AtalantaError::SyncTimeout { .. }
        | AtalantaError::StepTimeout { .. }
        | AtalantaError::SuiteTimeout { .. } => {
            ("TIMEOUT", Retry::ObserveFirst, Some(NEXT_OBSERVE))
        }
        AtalantaError::HttpUnreachable { .. } => ("HTTP_UNREACHABLE", Retry::Safe, None),
        AtalantaError::CommandFailed { .. } => {
            ("COMMAND_FAILED", Retry::ObserveFirst, Some(NEXT_OBSERVE))
        }
        AtalantaError::Internal(_) => ("INTERNAL", Retry::No, None),
        AtalantaError::StepFailed { crashed: true, .. } => (
            "APP_CRASHED",
            Retry::No,
            Some("relaunch the app; the crash log path is in the message"),
        ),
        AtalantaError::StepFailed { kind, .. } => match kind {
            ErrorKind::ElementNotFound => ("NOT_FOUND", Retry::No, Some(NEXT_LOOK)),
            ErrorKind::AssertionFailed => ("ASSERTION_FAILED", Retry::No, None),
            ErrorKind::InvalidSelector | ErrorKind::ConfigError => {
                ("INVALID_ARGS", Retry::No, Some(NEXT_ARGS))
            }
            ErrorKind::ElementStale => ("NOT_ACTIONABLE", Retry::ObserveFirst, Some(NEXT_OBSERVE)),
            ErrorKind::Timeout => ("TIMEOUT", Retry::ObserveFirst, Some(NEXT_OBSERVE)),
            ErrorKind::ConnectionLost | ErrorKind::SessionExpired => {
                ("DEVICE_LOST", Retry::ObserveFirst, Some(NEXT_OBSERVE))
            }
            ErrorKind::DeviceNotReady => ("DEVICE_NOT_READY", Retry::Safe, Some(NEXT_DOCTOR)),
            ErrorKind::DeviceUnresponsive => ("DEVICE_UNRESPONSIVE", Retry::No, Some(NEXT_DOCTOR)),
            ErrorKind::PlatformNotSupported => ("UNSUPPORTED", Retry::No, None),
            // Unknown, and any kind added later: the action may have run.
            _ => ("DEVICE_ERROR", Retry::ObserveFirst, Some(NEXT_OBSERVE)),
        },
        other => match other.kind() {
            ErrorKind::ConnectionLost | ErrorKind::SessionExpired => {
                ("DEVICE_LOST", Retry::ObserveFirst, Some(NEXT_OBSERVE))
            }
            ErrorKind::DeviceNotReady => ("DEVICE_NOT_READY", Retry::Safe, Some(NEXT_DOCTOR)),
            ErrorKind::DeviceUnresponsive => ("DEVICE_UNRESPONSIVE", Retry::No, Some(NEXT_DOCTOR)),
            ErrorKind::PlatformNotSupported => ("UNSUPPORTED", Retry::No, None),
            _ => ("DEVICE_ERROR", Retry::ObserveFirst, Some(NEXT_OBSERVE)),
        },
    };
    Failure::new(code, retry, next, message)
}

/// Failures the server decides itself, not from a [`AtalantaError`].
pub fn read_only(message: impl Into<String>) -> Failure {
    Failure::new("READ_ONLY", Retry::No, None, message)
}

pub fn not_in_profile(message: impl Into<String>) -> Failure {
    Failure::new("NOT_IN_PROFILE", Retry::No, None, message)
}

pub fn unknown_tool(message: impl Into<String>) -> Failure {
    Failure::new("UNKNOWN_TOOL", Retry::No, None, message)
}

pub fn device_busy(message: impl Into<String>) -> Failure {
    Failure::new(
        "DEVICE_BUSY",
        Retry::Safe,
        Some("another process holds the device; repeat when it is done"),
        message,
    )
}

pub fn deadline(message: impl Into<String>) -> Failure {
    Failure::new("TIMEOUT", Retry::ObserveFirst, Some(NEXT_OBSERVE), message)
}

pub fn session_rebuilt(message: impl Into<String>) -> Failure {
    Failure::new(
        "DEVICE_LOST",
        Retry::ObserveFirst,
        Some(NEXT_OBSERVE),
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_refs_say_take_a_snapshot_and_never_retry() {
        let f = classify(&AtalantaError::StaleRef("ref @e3 is stale".into()));
        assert_eq!((f.code, f.retry), ("STALE_REF", Retry::No));
        assert_eq!(f.next, Some(NEXT_SNAPSHOT));
    }

    #[test]
    fn a_lost_device_is_observe_first_for_actions_and_safe_for_reads() {
        let e = AtalantaError::AdbConnectionLost {
            device_id: "emulator-5554".into(),
            reason: "closed".into(),
        };
        let f = classify(&e);
        assert_eq!((f.code, f.retry), ("DEVICE_LOST", Retry::ObserveFirst));
        assert_eq!(f.clone().for_tool("snapshot", true).retry, Retry::Safe);
        assert_eq!(f.for_tool("tap", false).retry, Retry::ObserveFirst);
    }

    #[test]
    fn a_failed_step_keeps_the_kind_of_its_error() {
        let step = |kind, crashed| AtalantaError::StepFailed {
            kind,
            crashed,
            message: "Element not found: text=\"Privacy\" (12 scrolls Down)".into(),
        };
        assert_eq!(
            classify(&step(ErrorKind::ElementNotFound, false)).code,
            "NOT_FOUND"
        );
        assert_eq!(
            classify(&step(ErrorKind::AssertionFailed, false)).code,
            "ASSERTION_FAILED"
        );
        assert_eq!(
            classify(&step(ErrorKind::Unknown, true)).code,
            "APP_CRASHED"
        );
        let scrolled = classify(&step(ErrorKind::ElementNotFound, false)).for_tool("scroll", false);
        assert_eq!(scrolled.delivered(), "yes");
        assert_eq!(
            classify(&step(ErrorKind::ElementNotFound, false))
                .for_tool("tap", false)
                .delivered(),
            "no"
        );
    }

    #[test]
    fn config_errors_are_invalid_args() {
        let f = classify(&AtalantaError::Config(
            "'mode' must be one of full, delta".into(),
        ));
        assert_eq!((f.code, f.retry), ("INVALID_ARGS", Retry::No));
    }

    #[test]
    fn json_is_masked_and_carries_every_field() {
        let f = device_busy("held by pid 42");
        let v = f.to_json();
        assert_eq!(v["code"], "DEVICE_BUSY");
        assert_eq!(v["retry"], "safe");
        assert_eq!(v["delivered"], "no");
        assert!(v["next"].is_string());
        assert_eq!(v["message"], "held by pid 42");
    }
}
