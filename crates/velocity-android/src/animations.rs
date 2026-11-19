//! System animations off for the run, restored however the run ends.
//!
//! Window, transition and animator scales go to 0 the first time a device
//! launches an app, which shortens every screen change the settle has to
//! wait for. The restore can't depend on Velocity exiting cleanly (Ctrl-C,
//! a panic, `kill -9`), so it runs *on the device* (scrcpy's pattern): one
//! `adb shell` saves the old values, sets 0, then blocks reading its stdin.
//! However Velocity ends, that stdin closes, `read` returns, and the same
//! shell puts the old values back (an unset value is deleted again). `HUP`
//! is ignored so the shell outlives its adb session. Checked on an API 35
//! emulator with the host adb client killed with SIGKILL.
//!
//! `VELOCITY_KEEP_ANIMATIONS=1` leaves animations alone.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const OFF: &str = "VELOCITY_ANIM_OFF";
const RESTORED: &str = "VELOCITY_ANIM_RESTORED";

const SCRIPT: &str = concat!(
    "trap '' HUP; ",
    "k='window_animation_scale transition_animation_scale animator_duration_scale'; ",
    "for s in $k; do eval \"old_$s=\\\"$(settings get global $s)\\\"\"; done; ",
    "for s in $k; do settings put global $s 0; done; ",
    "echo VELOCITY_ANIM_OFF; ",
    "read _; ",
    "for s in $k; do eval \"v=\\$old_$s\"; ",
    "if [ \"$v\" = null ] || [ -z \"$v\" ]; then settings delete global $s >/dev/null; ",
    "else settings put global $s \"$v\"; fi; done; ",
    "echo VELOCITY_ANIM_RESTORED"
);

struct Held {
    child: Child,
    out: Lines<BufReader<ChildStdout>>,
}

/// One device's restore shell, behind that device's own lock.
type Slot = std::sync::Arc<Mutex<Option<Held>>>;

#[derive(Default)]
pub struct AnimationGuard {
    /// Per device: turning animations off waits up to 10 s for the device,
    /// and one shared lock made every other device wait too (VEL-315).
    held: std::sync::Mutex<HashMap<String, Slot>>,
    /// Never touch a device (a driver on a fake adb: the guard runs the
    /// real one).
    off: bool,
}

fn wanted() -> bool {
    !matches!(
        velocity_common::env::var("VELOCITY_KEEP_ANIMATIONS").as_deref(),
        Ok("1") | Ok("true")
    )
}

impl AnimationGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Turn animations off on `device_id` (once per run). Best-effort: a
    /// device where it fails just keeps its animations.
    /// A guard that leaves every device's animations alone (tests).
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self {
            off: true,
            ..Self::default()
        }
    }

    pub async fn disable(&self, device_id: &str) {
        if self.off || !wanted() {
            return;
        }
        let slot = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device_id.to_string())
            .or_default()
            .clone();
        let mut held = slot.lock().await;
        if held.is_some() {
            return;
        }
        let adb =
            velocity_common::env::var("VELOCITY_ADB_PATH").unwrap_or_else(|_| "adb".to_string());
        let spawned = Command::new(adb)
            .args(crate::adb::target_args(device_id))
            .args(["shell", SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                warn!(device_id, "could not turn animations off: {e}");
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let mut out = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(Some(line)) = out.next_line().await {
                if line.trim() == OFF {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if ready {
            debug!(device_id, "animations off for this run");
            *held = Some(Held { child, out });
        } else {
            warn!(
                device_id,
                "could not turn animations off; continuing with them on"
            );
        }
    }

    /// Put every device's animations back, waiting briefly for each.
    pub async fn restore_all(&self) {
        let slots: Vec<(String, Slot)> = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (device_id, slot) in slots {
            let Some(mut h) = slot.lock().await.take() else {
                continue;
            };
            // Closing stdin is the restore signal.
            drop(h.child.stdin.take());
            let done = tokio::time::timeout(Duration::from_secs(5), async {
                while let Ok(Some(line)) = h.out.next_line().await {
                    if line.trim() == RESTORED {
                        return true;
                    }
                }
                false
            })
            .await
            .unwrap_or(false);
            if done {
                debug!(device_id, "animations restored");
            } else {
                // The device-side shell still restores once it sees EOF.
                debug!(device_id, "animation restore not confirmed");
            }
            let _ = h.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VEL-315: one device's slow `disable` held the lock every other
    /// device needed.
    #[tokio::test]
    async fn a_slow_device_does_not_block_another() {
        let guard = AnimationGuard::new();
        let slow = guard
            .held
            .lock()
            .unwrap()
            .entry("slow".into())
            .or_default()
            .clone();
        // "slow" is mid-disable (its lock held).
        let _busy = slow.lock().await;
        let other = guard
            .held
            .lock()
            .unwrap()
            .entry("other".into())
            .or_default()
            .clone();
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), other.lock()).await;
        assert!(got.is_ok(), "another device waited on the slow one");
    }

    #[test]
    fn script_restores_what_it_saved() {
        // Same three keys saved, zeroed and restored; unset values deleted.
        for key in [
            "window_animation_scale",
            "transition_animation_scale",
            "animator_duration_scale",
        ] {
            assert!(SCRIPT.contains(key));
        }
        assert!(SCRIPT.starts_with("trap '' HUP"));
        assert!(SCRIPT.contains("read _"));
        assert!(SCRIPT.contains("settings delete global"));
        let off = SCRIPT.find(OFF).unwrap();
        let read = SCRIPT.find("read _").unwrap();
        let restored = SCRIPT.find(RESTORED).unwrap();
        assert!(off < read && read < restored);
    }

    /// Needs a device: `VELOCITY_TEST_ANDROID=<serial>`.
    #[tokio::test]
    async fn animations_come_back_after_the_run() {
        let Some(device) = crate::test_support::device_under_test() else {
            return;
        };
        let get = |key: &'static str| {
            let device = device.clone();
            async move {
                let out = Command::new("adb")
                    .args(["-s", &device, "shell", "settings", "get", "global", key])
                    .output()
                    .await
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };
        let before = get("window_animation_scale").await;
        let guard = AnimationGuard::new();
        guard.disable(&device).await;
        assert_eq!(get("window_animation_scale").await, "0");
        guard.restore_all().await;
        assert_eq!(get("window_animation_scale").await, before);
    }
}
