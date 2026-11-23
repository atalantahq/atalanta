//! Headless emulators for `atalanta devices up --android N`.
//!
//! Since emulator 28.0.16 several instances of one AVD can run at once when
//! they are started with `-read-only`: each gets a copy-on-write overlay
//! that is thrown away on exit. So a farm of N needs one AVD, not N, and
//! every instance starts from the same clean image. Each instance takes a
//! console/adb port pair `5554 + 2k` (serial `emulator-<port>`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use atalanta_common::process::{default_timeout, output_with_timeout};
use atalanta_common::{AtalantaError, Result};
use tokio::process::Command;
use tracing::debug;

/// First console port the emulator uses; instances take even ports above it.
pub const FIRST_PORT: u16 = 5554;
/// The emulator only accepts console ports 5554–5682.
pub const LAST_PORT: u16 = 5682;

/// The `emulator` binary: `ATALANTA_EMULATOR_PATH`, the SDK
/// (`ANDROID_HOME`, `ANDROID_SDK_ROOT`, Android Studio's default location),
/// else `emulator` on PATH.
pub fn emulator_path() -> PathBuf {
    if let Ok(p) = atalanta_common::env::var("ATALANTA_EMULATOR_PATH") {
        return PathBuf::from(p);
    }
    let home = atalanta_common::env::var("HOME").unwrap_or_default();
    let roots = [
        atalanta_common::env::var("ANDROID_HOME").ok(),
        atalanta_common::env::var("ANDROID_SDK_ROOT").ok(),
        Some(format!("{home}/Library/Android/sdk")),
        Some(format!("{home}/Android/Sdk")),
    ];
    roots
        .into_iter()
        .flatten()
        .map(|r| Path::new(&r).join("emulator").join("emulator"))
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("emulator"))
}

fn adb_path() -> String {
    atalanta_common::env::var("ATALANTA_ADB_PATH").unwrap_or_else(|_| "adb".to_string())
}

async fn adb(args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(adb_path());
    cmd.args(args);
    let out = output_with_timeout(&mut cmd, timeout)
        .await
        .map_err(|e| AtalantaError::Config(format!("adb {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(AtalantaError::Config(format!(
            "adb {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// AVD names from `emulator -list-avds`.
pub async fn list_avds() -> Result<Vec<String>> {
    let mut cmd = Command::new(emulator_path());
    cmd.arg("-list-avds");
    let out = output_with_timeout(&mut cmd, default_timeout())
        .await
        .map_err(|e| {
            AtalantaError::Config(format!(
                "could not run {}: {e} (set ATALANTA_EMULATOR_PATH or ANDROID_HOME)",
                emulator_path().display()
            ))
        })?;
    Ok(parse_avds(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_avds(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        // The emulator prints INFO/WARNING lines to stdout on some versions.
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.contains('|'))
        .map(String::from)
        .collect()
}

/// Serials of the emulators adb sees (any state).
pub async fn running_serials() -> Vec<String> {
    adb(&["devices"], default_timeout())
        .await
        .map(|out| parse_emulator_serials(&out))
        .unwrap_or_default()
}

fn parse_emulator_serials(out: &str) -> Vec<String> {
    out.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .filter(|s| is_emulator_serial(s))
        .map(String::from)
        .collect()
}

/// `emulator-NNNN`: only these are ever started or stopped by Atalanta.
pub fn is_emulator_serial(serial: &str) -> bool {
    serial
        .strip_prefix("emulator-")
        .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The lowest console port at or above `from` whose port pair is free on
/// this host and that no emulator adb knows of uses.
pub fn free_port(from: u16, taken: &[String]) -> Option<u16> {
    let from = from.max(FIRST_PORT);
    // Overflowed for a base near u16::MAX (VEL-316); past LAST_PORT there
    // is nothing to find anyway.
    let from = from.checked_add((from - FIRST_PORT) % 2)?;
    (from..=LAST_PORT).step_by(2).find(|&p| {
        !taken.iter().any(|s| *s == format!("emulator-{p}")) && port_free(p) && port_free(p + 1)
    })
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Start a headless, read-only instance of `avd` on `port`. It runs in its
/// own process group, so Ctrl-C in the terminal that started it doesn't
/// take it down, and it outlives this process; output goes to `log`.
pub fn launch_read_only(avd: &str, port: u16, log: &Path) -> Result<std::process::Child> {
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = std::fs::File::create(log)
        .map_err(|e| AtalantaError::Config(format!("{}: {e}", log.display())))?;
    let err = out
        .try_clone()
        .map_err(|e| AtalantaError::Config(format!("{}: {e}", log.display())))?;
    let mut cmd = std::process::Command::new(emulator_path());
    cmd.args(["-avd", avd])
        .args(launch_flags(port))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    debug!(avd, port, "starting emulator");
    cmd.spawn().map_err(|e| AtalantaError::DeviceBootFailed {
        id: format!("emulator-{port}"),
        reason: format!("could not start {}: {e}", emulator_path().display()),
    })
}

fn launch_flags(port: u16) -> Vec<String> {
    [
        "-read-only",
        "-no-window",
        "-no-audio",
        "-no-boot-anim",
        "-port",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([port.to_string()])
    .collect()
}

/// Wait until `serial` has booted: `sys.boot_completed` is 1 and the
/// package manager answers (`pm path android`; the property flips a few
/// seconds before installs work). Fails early if `child` exits.
pub async fn wait_booted(
    serial: &str,
    mut child: Option<&mut std::process::Child>,
    timeout: Duration,
) -> Result<Duration> {
    let start = Instant::now();
    let fail = |reason: String| AtalantaError::DeviceBootFailed {
        id: serial.to_string(),
        reason,
    };
    loop {
        if let Some(c) = child.as_deref_mut() {
            if let Ok(Some(status)) = c.try_wait() {
                return Err(fail(format!("the emulator exited during boot ({status})")));
            }
        }
        let step = Duration::from_secs(10);
        let booted = adb(
            &["-s", serial, "shell", "getprop", "sys.boot_completed"],
            step,
        )
        .await
        .is_ok_and(|o| o.trim() == "1");
        if booted
            && adb(&["-s", serial, "shell", "pm", "path", "android"], step)
                .await
                .is_ok_and(|o| o.contains("package:"))
        {
            return Ok(start.elapsed());
        }
        if start.elapsed() > timeout {
            return Err(fail(format!("not booted after {}s", timeout.as_secs())));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The AVD an emulator runs (`adb emu avd name`), to make sure a serial
/// still belongs to the instance Atalanta started before stopping it.
pub async fn avd_name(serial: &str) -> Option<String> {
    let out = adb(
        &["-s", serial, "emu", "avd", "name"],
        Duration::from_secs(10),
    )
    .await
    .ok()?;
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "OK")
        .map(String::from)
}

/// Stop the emulator behind `serial` (`adb emu kill`).
pub async fn kill(serial: &str) -> Result<()> {
    if !is_emulator_serial(serial) {
        return Err(AtalantaError::Config(format!(
            "{serial} is not an emulator; refusing to stop it"
        )));
    }
    adb(&["-s", serial, "emu", "kill"], Duration::from_secs(20))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avd_list_skips_emulator_chatter() {
        let out = "INFO    | Storing crashdata in: /tmp/x\nzii_arm64\nPixel_8_API_34\n\n";
        assert_eq!(parse_avds(out), vec!["zii_arm64", "Pixel_8_API_34"]);
    }

    #[test]
    fn only_emulator_serials_count_as_emulators() {
        let out = "List of devices attached\nemulator-5554\tdevice\nR5CT10ABC1X\tdevice\n\
                   192.168.1.5:5555\tdevice\nemulator-5556\toffline\n";
        assert_eq!(
            parse_emulator_serials(out),
            vec!["emulator-5554", "emulator-5556"]
        );
        assert!(!is_emulator_serial("emulator-"));
        assert!(!is_emulator_serial("713-163-843"));
    }

    #[test]
    fn free_ports_are_even_and_skip_running_emulators() {
        let taken = vec!["emulator-5554".to_string(), "emulator-5556".to_string()];
        let p = free_port(FIRST_PORT, &taken).unwrap();
        assert!(p >= 5558 && p.is_multiple_of(2), "{p}");
        // An odd base rounds up to the next instance's port.
        assert!(free_port(5561, &[]).is_some_and(|p| p.is_multiple_of(2)));
        // VEL-316: no overflow near u16::MAX.
        assert_eq!(free_port(u16::MAX, &[]), None);
        assert_eq!(free_port(u16::MAX - 1, &[]), None);
    }

    #[test]
    fn instances_start_read_only_and_headless() {
        let flags = launch_flags(5556).join(" ");
        assert_eq!(
            flags,
            "-read-only -no-window -no-audio -no-boot-anim -port 5556"
        );
    }
}
