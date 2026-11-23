pub mod a11y;
pub mod actionability;
pub mod color;
pub mod config;
pub mod env;
pub mod error;
pub mod evidence;
pub mod exec;
pub mod fsutil;
pub mod host;
pub mod http;
pub mod install;
pub mod jsonline;
pub mod live_events;
pub mod natural;
pub mod process;
pub mod redact;
pub mod resilience;
pub mod result;
pub mod role;
pub mod secrets;
pub mod selector_build;
pub mod selector_match;
pub mod settings;
pub mod tap_guard;
pub mod test_types;
pub mod traits;
pub mod types;
pub mod web_devices;

pub use config::*;
pub use error::{AtalantaError, ErrorKind, Result};
pub use evidence::{
    Attachment, AttachmentKind, CaptureMode, Evidence, EvidenceOut, EvidenceRequest,
    EvidenceSession,
};
pub use http::{
    Capture, Checks, HttpConfig, HttpExpect, HttpRequest, HttpRetry, JsonType, Matcher, StatusCheck,
};
pub use install::InstallOutcome;
pub use resilience::{CircuitBreaker, ProbePermit, ResilientDriver, RetryPolicy};
pub use result::*;
pub use test_types::*;
pub use traits::{HealthStatus, Liveness, PlatformDriver, Touch};
pub use types::*;

/// Where `saveState` keeps app data: `~/.atalanta/state/<device>/<app>/<name>`
/// (`ATALANTA_STATE_DIR` overrides the root).
pub fn state_dir(device_id: &str, app_id: &str, name: &str) -> std::path::PathBuf {
    // One safe path component per name. `.` and `..` passed through and
    // escaped the state root, and names differing only in replaced
    // characters (`a:b`, `a/b`) shared a folder (VEL-330): those get a hash
    // of the full name.
    let clean = |s: &str| -> String {
        let plain: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if plain == s && !matches!(s, "" | "." | "..") {
            return plain;
        }
        let hash = s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{}-{hash:016x}", plain.replace('.', "_"))
    };
    let root = crate::env::var("ATALANTA_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| crate::host::home_dir().join(".atalanta").join("state"));
    root.join(clean(device_id))
        .join(clean(app_id))
        .join(clean(name))
}

#[cfg(test)]
mod state_dir_tests {
    use super::*;

    // VEL-330: `..` escaped the state root; `a:b` and `a/b` shared a folder.
    #[test]
    fn state_names_stay_inside_their_folder_and_apart() {
        let base = state_dir("d", "app", "x");
        let root = base.parent().unwrap();
        for name in ["..", ".", ""] {
            let p = state_dir("d", "app", name);
            assert_eq!(p.parent().unwrap(), root, "{name:?} -> {}", p.display());
            assert!(p.file_name().is_some_and(|f| f != ".." && f != "."));
        }
        assert_ne!(state_dir("d", "app", "a:b"), state_dir("d", "app", "a/b"));
        assert_ne!(state_dir("d", "app", "a:b"), state_dir("d", "app", "a_b"));
        assert_ne!(
            state_dir("..", "app", "x").parent(),
            state_dir(".", "app", "x").parent()
        );
        assert!(base.ends_with("d/app/x"), "plain names unchanged");
    }
}
