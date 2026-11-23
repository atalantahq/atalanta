// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::path::PathBuf;

/// Classification of errors for automatic retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    // Transient — auto-retry with backoff
    ConnectionLost,
    SessionExpired,
    ElementStale,
    DeviceNotReady,

    // Permanent — fail fast
    ElementNotFound,
    InvalidSelector,
    AssertionFailed,
    PlatformNotSupported,
    ConfigError,

    // Ambiguous — retry once, then escalate
    Timeout,
    Unknown,

    /// The device's circuit breaker is open: never retried (that is what the
    /// breaker is for), and a device failure, not the app's.
    DeviceUnresponsive,
}

impl ErrorKind {
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::ConnectionLost | Self::SessionExpired | Self::ElementStale | Self::DeviceNotReady
        )
    }

    pub fn is_permanent(self) -> bool {
        matches!(
            self,
            Self::ElementNotFound
                | Self::InvalidSelector
                | Self::AssertionFailed
                | Self::PlatformNotSupported
                | Self::ConfigError
        )
    }

    pub fn max_retries(self) -> u32 {
        match self {
            _ if self.is_transient() => 3,
            Self::Timeout | Self::Unknown => 1,
            _ => 0,
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum AtalantaError {
    // Configuration Errors (exit code 2)
    #[error("{}", yaml_parse_message(file, *line, *col, message))]
    YamlParse {
        file: PathBuf,
        line: usize,
        col: usize,
        message: String,
    },

    #[error("Unknown flow reference '{flow_id}' in test '{test_name}'")]
    UnknownFlowRef { flow_id: String, test_name: String },

    #[error("Invalid selector in step {step_index} of '{test_name}': {reason}")]
    InvalidSelector {
        test_name: String,
        step_index: usize,
        reason: String,
    },

    #[error("Missing required environment variable(s): {}", vars.join(", "))]
    MissingEnvVars { vars: Vec<String> },

    #[error("Configuration error: {0}")]
    Config(String),

    /// The step or driver call isn't supported on this platform (a
    /// `PlatformDriver` default): exit 2 like a config error, but kind
    /// `PlatformNotSupported`, so an agent is told "unsupported", not
    /// "fix your arguments" (review 02 H4).
    #[error("{0}")]
    Unsupported(String),

    /// An agent's element ref (`@e12` from `snapshot`, `e3` from
    /// `list_elements`) no longer names a current element: the agent must
    /// take a new snapshot. Exit code 1, never retried by the executor.
    #[error("{0}")]
    StaleRef(String),

    /// A step the executor ran on an agent's behalf (an MCP step tool)
    /// failed: the executor's message and the kind of the error behind it,
    /// so the agent is told "not found" or "crashed", not "bad arguments".
    #[error("{message}")]
    StepFailed {
        kind: ErrorKind,
        crashed: bool,
        message: String,
    },

    // Device Errors (exit code 3)
    #[error("Device '{id}' not found. Available: {}", available.join(", "))]
    DeviceNotFound { id: String, available: Vec<String> },

    #[error("Failed to boot device '{id}': {reason}")]
    DeviceBootFailed { id: String, reason: String },

    /// The lock screen needs a PIN, pattern or password: nothing launched
    /// behind it can be seen, and Atalanta won't enter a credential.
    #[error("Device '{id}' is locked with a PIN, pattern or password; unlock it (or set the lock screen to None/Swipe) and run again")]
    DeviceLocked { id: String },

    #[error("WDA health check failed after {timeout_s}s on device '{device_id}'")]
    WdaUnhealthy { device_id: String, timeout_s: u64 },

    /// The iOS runner can't be started on the device: its port is held by
    /// a process Atalanta won't stop, or it failed to come up several
    /// times in a row (a crash loop). Relaunching won't help: fail fast.
    #[error("The iOS runner can't start on device '{device_id}': {detail}")]
    RunnerFailing { device_id: String, detail: String },

    #[error("WDA session lost during test '{test_name}' (restart attempt {attempt}/{max})")]
    WdaSessionLost {
        test_name: String,
        attempt: u32,
        max: u32,
    },

    /// WebDriverAgent didn't accept the connection (crashed or not started).
    /// The request never reached it, so it is safe to replay after a restart.
    #[error(
        "WebDriverAgent at {url}{} is not reachable: {detail}",
        on_device(device_id)
    )]
    WdaUnreachable {
        /// The device's UDID; empty when the client wasn't told it.
        device_id: String,
        url: String,
        detail: String,
    },

    /// The connection to WebDriverAgent broke mid-request (reset, closed,
    /// timed out): the request may have run, so an action is not replayed.
    #[error(
        "Lost the connection to WebDriverAgent at {url}{}: {detail}",
        on_device(device_id)
    )]
    WdaLost {
        /// The device's UDID; empty when the client wasn't told it.
        device_id: String,
        url: String,
        detail: String,
    },

    /// Too many driver failures in a row on this device; calls fail fast
    /// until the breaker's cool-down passes.
    #[error("Circuit breaker is open for device '{device_id}' — driver unresponsive")]
    CircuitOpen { device_id: String },

    /// The DevTools connection to the web driver's Chrome is gone (Chrome
    /// crashed or was killed). Transient: the session is restarted.
    #[error("Chrome DevTools connection lost: {detail}")]
    BrowserConnectionLost { detail: String },

    /// The connection to an in-app native sync probe broke or went quiet
    /// (reset, closed, no reply in time). Transient: the next settle
    /// reconnects, and callers fall back to tree polling meanwhile.
    #[error("Native sync probe on port {port}: {detail}")]
    SyncProbeLost { port: u16, detail: String },

    #[error("ADB connection lost to device '{device_id}': {reason}")]
    AdbConnectionLost { device_id: String, reason: String },

    /// A device-side command ran and failed (non-zero exit, error output).
    /// The connection is fine, so this is not retried as a lost connection.
    /// The command line and its output are redacted when shown: they can
    /// hold a typed password or a token.
    #[error(
        "Command failed on device '{device_id}' ({}): {}",
        crate::redact::redact(command),
        crate::redact::redact(detail)
    )]
    CommandFailed {
        device_id: String,
        command: String,
        detail: String,
    },

    /// The device's screenshots kept coming back as something other than
    /// an image (a truncated transfer): the device failed, not the app
    /// (exit 3, and the test counts as an infrastructure failure).
    #[error("Screen capture failed on device '{device_id}': {detail}")]
    ScreenCaptureFailed { device_id: String, detail: String },

    /// An external service the run depends on (e.g. the model API for
    /// natural-language steps) stayed unavailable through its retries: a
    /// transient infrastructure condition, not a test failure.
    #[error("{service} is unavailable: {detail}")]
    ServiceUnavailable { service: String, detail: String },

    // Test Failures (exit code 1)
    #[error("Element not found: {selector} after {timeout_ms}ms")]
    ElementNotFound {
        selector: String,
        timeout_ms: u64,
        screenshot: Option<PathBuf>,
        hierarchy_snapshot: Option<String>,
    },

    /// The target was found but refused the action: a disabled web control,
    /// or one that stayed covered (VEL-274: this read "Element not found").
    #[error("Element {selector} is not actionable: {reason}")]
    NotActionable { selector: String, reason: String },

    /// The app under test crashed during the step (Android's crash log).
    #[error("App {app_id} crashed: {detail}")]
    AppCrashed { app_id: String, detail: String },

    #[error("Assertion failed: expected {expected:?}, got {actual:?}")]
    AssertionFailed {
        expected: String,
        actual: String,
        selector: String,
        screenshot: Option<PathBuf>,
    },

    /// An `http` step's response failed its `expect:` checks: every failed
    /// check, and the start of the body.
    #[error("{}", http_check_message(request, *status, failures, body))]
    HttpCheckFailed {
        request: String,
        status: u16,
        failures: Vec<String>,
        body: String,
    },

    /// An `http` step got no response (refused, DNS, TLS, timeout). A test
    /// failure, not the device's: the API under test didn't answer.
    #[error("{request}: no response: {detail}")]
    HttpUnreachable { request: String, detail: String },

    #[error(
        "Sync timeout: UI did not stabilize within {timeout_ms}ms \
         ({stable_count}/{required} stable frames)"
    )]
    SyncTimeout {
        timeout_ms: u64,
        stable_count: u32,
        required: u32,
    },

    /// `step_index` counts from 0; the message counts from 1, like the
    /// failure reports.
    #[error("Step timeout: step {} in '{test_name}' exceeded {timeout_ms}ms", step_index + 1)]
    StepTimeout {
        test_name: String,
        step_index: usize,
        timeout_ms: u64,
    },

    // Timeout (exit code 4)
    #[error("Suite timeout: exceeded {timeout_ms}ms ({completed}/{total} tests completed)")]
    SuiteTimeout {
        timeout_ms: u64,
        completed: usize,
        total: usize,
    },

    // Internal Errors (exit code 5)
    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// An error from one step, with where that step is written:
    /// `tests/app.yaml:42: tap { text: "Pay" }: Element not found ...`.
    /// Exit code and retry class are the inner error's.
    #[error("{pos}: {step}: {inner}")]
    AtSource {
        pos: crate::SourcePos,
        step: String,
        inner: Box<AtalantaError>,
    },
}

impl AtalantaError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::AtSource { inner, .. } => inner.exit_code(),
            Self::StepFailed { kind, .. } => match kind {
                ErrorKind::ConfigError | ErrorKind::InvalidSelector => 2,
                ErrorKind::ConnectionLost
                | ErrorKind::SessionExpired
                | ErrorKind::DeviceNotReady
                | ErrorKind::DeviceUnresponsive => 3,
                _ => 1,
            },
            Self::ElementNotFound { .. }
            | Self::NotActionable { .. }
            | Self::AssertionFailed { .. }
            | Self::AppCrashed { .. }
            | Self::HttpCheckFailed { .. }
            | Self::HttpUnreachable { .. }
            | Self::SyncTimeout { .. }
            | Self::StepTimeout { .. }
            | Self::StaleRef(_) => 1,

            Self::YamlParse { .. }
            | Self::UnknownFlowRef { .. }
            | Self::InvalidSelector { .. }
            | Self::MissingEnvVars { .. }
            | Self::Config(_)
            | Self::Unsupported(_) => 2,

            Self::DeviceNotFound { .. }
            | Self::DeviceBootFailed { .. }
            | Self::DeviceLocked { .. }
            | Self::WdaUnhealthy { .. }
            | Self::RunnerFailing { .. }
            | Self::WdaSessionLost { .. }
            | Self::WdaUnreachable { .. }
            | Self::WdaLost { .. }
            | Self::SyncProbeLost { .. }
            | Self::AdbConnectionLost { .. }
            | Self::BrowserConnectionLost { .. }
            | Self::CircuitOpen { .. }
            | Self::CommandFailed { .. }
            | Self::ScreenCaptureFailed { .. }
            | Self::ServiceUnavailable { .. } => 3,

            Self::SuiteTimeout { .. } => 4,

            Self::Internal(_) => 5,
        }
    }
}

impl AtalantaError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::AtSource { inner, .. } => inner.kind(),
            Self::AdbConnectionLost { .. }
            | Self::WdaUnreachable { .. }
            | Self::WdaLost { .. }
            | Self::SyncProbeLost { .. }
            | Self::BrowserConnectionLost { .. }
            | Self::ServiceUnavailable { .. } => ErrorKind::ConnectionLost,
            // Ambiguous: a read may be retried once; actions never are.
            Self::CommandFailed { .. } => ErrorKind::Unknown,
            Self::WdaSessionLost { .. } => ErrorKind::SessionExpired,
            Self::CircuitOpen { .. } => ErrorKind::DeviceUnresponsive,
            // Another launch would fail the same way (or crash-loop).
            Self::RunnerFailing { .. } => ErrorKind::DeviceUnresponsive,
            Self::WdaUnhealthy { .. } => ErrorKind::DeviceNotReady,
            Self::DeviceBootFailed { .. } => ErrorKind::DeviceNotReady,
            Self::DeviceNotFound { .. } => ErrorKind::DeviceNotReady,
            Self::ScreenCaptureFailed { .. } => ErrorKind::DeviceNotReady,
            // Retrying can't unlock it: fail at once.
            Self::DeviceLocked { .. } => ErrorKind::ConfigError,

            Self::ElementNotFound { .. } => ErrorKind::ElementNotFound,
            Self::InvalidSelector { .. } => ErrorKind::InvalidSelector,
            Self::AssertionFailed { .. } => ErrorKind::AssertionFailed,
            Self::NotActionable { .. } => ErrorKind::AssertionFailed,
            // A test failure (exit 1), never retried within the step.
            Self::AppCrashed { .. } => ErrorKind::AssertionFailed,
            Self::HttpCheckFailed { .. } => ErrorKind::AssertionFailed,
            // Down or slow, can't tell; never the device's fault, so an
            // `optional` step forgives it and no device is quarantined. The
            // step's own `retry:` is what sends it again.
            Self::HttpUnreachable { .. } => ErrorKind::Unknown,

            Self::SyncTimeout { .. } | Self::StepTimeout { .. } => ErrorKind::Timeout,
            Self::SuiteTimeout { .. } => ErrorKind::Timeout,

            Self::YamlParse { .. }
            | Self::UnknownFlowRef { .. }
            | Self::MissingEnvVars { .. }
            | Self::Config(_) => ErrorKind::ConfigError,

            Self::Unsupported(_) => ErrorKind::PlatformNotSupported,

            Self::StaleRef(_) => ErrorKind::ElementStale,
            Self::StepFailed { kind, .. } => *kind,

            Self::Internal(_) => ErrorKind::Unknown,
        }
    }

    pub fn is_transient(&self) -> bool {
        self.kind().is_transient()
    }

    /// Atalanta couldn't read or write its own files (reports, baselines,
    /// healed mappings, history) or serialize its own data: an internal
    /// error (exit 5), never `Config` (exit 2), which tells the user their
    /// YAML or flags are wrong (review 03 M2).
    pub fn io(context: impl std::fmt::Display, e: impl std::fmt::Display) -> Self {
        Self::Internal(anyhow::anyhow!("{context}: {e}"))
    }
}

/// ` (device 'X')`, or nothing when the id isn't known.
fn on_device(device_id: &str) -> String {
    if device_id.is_empty() {
        String::new()
    } else {
        format!(" (device '{device_id}')")
    }
}

pub type Result<T> = std::result::Result<T, AtalantaError>;

/// `POST https://x.dev/orders answered 500: 2 checks failed: …; body: …`.
fn http_check_message(request: &str, status: u16, failures: &[String], body: &str) -> String {
    let n = failures.len();
    let mut out = format!(
        "{request} answered {status}: {n} check{} failed: {}",
        if n == 1 { "" } else { "s" },
        failures.join("; ")
    );
    if !body.is_empty() {
        out.push_str("; body: ");
        out.push_str(body);
    }
    out
}

/// `tests/app.yaml:12:9: message` (the form editors and CI logs link), with
/// the parts that are unknown left out.
fn yaml_parse_message(file: &std::path::Path, line: usize, col: usize, message: &str) -> String {
    let file = file.display().to_string();
    match (file.is_empty(), line) {
        (true, 0) => format!("YAML parse error: {message}"),
        (true, _) => format!("line {line}:{col}: {message}"),
        (false, 0) => format!("{file}: {message}"),
        (false, _) => format!("{file}:{line}:{col}: {message}"),
    }
}
