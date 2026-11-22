//! Where a login's secrets come from besides the environment (row 1.5,
//! AI-021 / API-079): the OS keychain, and one-time codes from a TOTP
//! secret. Both are written in YAML like any variable, never as values:
//!
//! - `${keychain:NAME}`: the password stored under service `velocity`
//!   (`VELOCITY_KEYCHAIN_SERVICE`), account `NAME`. macOS reads it with
//!   `security find-generic-password`, Linux with `secret-tool lookup`
//!   (libsecret). `VELOCITY_KEYCHAIN_CMD` names another program, called as
//!   `<cmd> <service> <account>`, printing the secret (a 1Password or
//!   Vault wrapper, a CI secret store).
//! - `${totp:NAME}`: the current 6-digit RFC 6238 code (SHA-1, 30 s) for
//!   the base32 secret in `NAME` (a variable, or `keychain:NAME`), worked
//!   out when the step runs, not when the suite loads, so it is fresh.
//!
//! Every value from either source is registered with [`crate::redact`], so
//! it is masked in every report, trace, event and MCP reply.

use std::process::Command;

/// The keychain service Velocity's secrets live under.
fn service() -> String {
    crate::env::var("VELOCITY_KEYCHAIN_SERVICE").unwrap_or_else(|_| "velocity".into())
}

/// The secret stored for `account`, or why there is none.
pub fn keychain(account: &str) -> Result<String, String> {
    let service = service();
    let (program, args): (String, Vec<String>) = match crate::env::var("VELOCITY_KEYCHAIN_CMD") {
        Ok(cmd) if !cmd.trim().is_empty() => (cmd, vec![service.clone(), account.into()]),
        _ if cfg!(target_os = "macos") => (
            "security".into(),
            ["find-generic-password", "-s", &service, "-a", account, "-w"]
                .map(String::from)
                .to_vec(),
        ),
        _ => (
            "secret-tool".into(),
            ["lookup", "service", &service, "account", account]
                .map(String::from)
                .to_vec(),
        ),
    };
    let (ok, stdout) = run_with_deadline(&program, &args, crate::process::default_timeout())
        .map_err(|e| format!("keychain: {e}"))?;
    let value = String::from_utf8_lossy(&stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if !ok || value.is_empty() {
        return Err(format!(
            "keychain: no secret for account {account:?} under service {service:?} \
             (store it with: security add-generic-password -s {service} -a {account} -w, \
             or secret-tool store --label velocity service {service} account {account})"
        ));
    }
    crate::redact::register_secret(&value);
    Ok(value)
}

/// Run `program` with `args` and no stdin, and give back whether it
/// succeeded and what it printed. A keychain command can wait for ever: macOS
/// `security` blocks on a locked keychain's unlock prompt (nobody to answer it
/// in CI or over SSH), and a password-manager wrapper on a login. So the
/// command is killed once `timeout` passes and the secret reported missing,
/// instead of the whole run hanging while the suite loads (V5).
fn run_with_deadline(
    program: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<(bool, Vec<u8>), String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run {program}: {e}"))?;
    let mut pipe = child.stdout.take();
    // Read on a thread: a command that fills the pipe must not block here.
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_end(&mut out);
        }
        out
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                // The reader is left to end on its own: a grandchild that
                // inherited the pipe could keep it open.
                return Err(format!(
                    "{program} gave no answer within {:.0}s (a locked keychain waiting for a \
                     prompt?); VELOCITY_COMMAND_TIMEOUT_MS sets how long to wait",
                    timeout.as_secs_f32()
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => return Err(format!("waiting for {program}: {e}")),
        }
    };
    let out = reader.join().unwrap_or_default();
    Ok((status.success(), out))
}

/// RFC 4648 base32 (the alphabet authenticator apps use), case and spaces
/// ignored, padding optional.
fn base32(input: &str) -> Option<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = Vec::new();
    for c in input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=' && *c != '-')
    {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => return None,
        };
        bits = (bits << 5) | v;
        n += 5;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The RFC 6238 code for `secret` (base32) at Unix time `now`: HMAC-SHA1,
/// a 30 s step, 6 digits.
pub fn totp_at(secret: &str, now: u64) -> Result<String, String> {
    let key = base32(secret).ok_or("totp: the secret is not base32 (A-Z, 2-7)")?;
    let counter = (now / 30).to_be_bytes();
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key);
    let mac = ring::hmac::sign(&k, &counter);
    let h = mac.as_ref();
    let off = (h[h.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    Ok(format!("{:06}", bin % 1_000_000))
}

/// The current code for `secret`; both are masked from here on.
pub fn totp_now(secret: &str) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::redact::register_secret(secret);
    let code = totp_at(secret, now)?;
    crate::redact::register_secret(&code);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1, with its 20-byte ASCII key
    // "12345678901234567890" (base32 below), truncated to 6 digits.
    #[test]
    fn rfc_6238_test_vectors() {
        let key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        for (t, want) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(totp_at(key, t).unwrap(), want, "t={t}");
        }
        // Lower case, spaces and padding as authenticator apps show them.
        assert_eq!(
            totp_at("gezd gnbv gy3t qojq gezd gnbv gy3t qojq====", 59).unwrap(),
            "287082"
        );
        assert!(totp_at("not base32!", 59).is_err());
    }

    /// The tests that install settings share the process-wide settings.
    #[cfg(unix)]
    static SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn a_custom_keychain_command_is_called_with_service_and_account() {
        let _serial = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cmd = dir.path().join("fake-keychain");
        std::fs::write(
            &cmd,
            "#!/bin/sh\n[ \"$1\" = velocity ] && [ \"$2\" = LOGIN_PASSWORD ] && echo 'hunter2-from-keychain' && exit 0\nexit 44\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cmd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = crate::env::settings_snapshot();
        env.insert(
            "VELOCITY_KEYCHAIN_CMD".into(),
            cmd.to_string_lossy().into_owned(),
        );
        crate::env::set_settings(env);
        assert_eq!(keychain("LOGIN_PASSWORD").unwrap(), "hunter2-from-keychain");
        assert!(crate::redact::registered_secrets().contains(&"hunter2-from-keychain".to_string()));
        let e = keychain("NOPE").unwrap_err();
        assert!(e.contains("no secret for account \"NOPE\""), "{e}");
        let mut env = crate::env::settings_snapshot();
        env.remove("VELOCITY_KEYCHAIN_CMD");
        crate::env::set_settings(env);
    }

    // V5: the keychain command had no deadline, so a `security` waiting on a
    // locked keychain's prompt (or a wrapper waiting on a login) hung the
    // run as the suite loaded.
    #[cfg(unix)]
    #[test]
    fn a_keychain_command_that_never_answers_is_killed_at_the_deadline() {
        let _serial = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cmd = dir.path().join("hung-keychain");
        std::fs::write(&cmd, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cmd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = crate::env::settings_snapshot();
        let mut env = before.clone();
        env.insert(
            "VELOCITY_KEYCHAIN_CMD".into(),
            cmd.to_string_lossy().into_owned(),
        );
        env.insert("VELOCITY_COMMAND_TIMEOUT_MS".into(), "300".into());
        crate::env::set_settings(env);
        let started = std::time::Instant::now();
        let err = keychain("HUNG").unwrap_err();
        crate::env::set_settings(before);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{err}"
        );
        assert!(err.contains("gave no answer within"), "{err}");
    }
}
