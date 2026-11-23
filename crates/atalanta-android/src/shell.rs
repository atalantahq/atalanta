//! Building command lines for the device shell.
//!
//! `adb shell a b c` joins its arguments with spaces and the device runs the
//! result with `sh -c`, so every argument is shell source. Anything that
//! comes from a test (an appId, typed text, a snapshot name) goes through
//! [`quote`] or [`cmd`] before it reaches `adb shell`.

use atalanta_common::{AtalantaError, Result};

/// `s` as one POSIX shell word: single quotes, with `'` written `'\''`.
/// Nothing inside single quotes is special (`$`, backticks, `\`, newlines).
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A command line whose words are `argv`, each quoted.
pub fn cmd(argv: &[&str]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Validate an Android application id (`com.example.app`): letters, digits
/// and `_`, in at least two dot-separated segments that each start with a
/// letter, at most 255 characters. Everything that puts an appId on a
/// device command line checks it first.
pub fn validate_app_id(app_id: &str) -> Result<()> {
    let segment_ok = |s: &str| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let ok = !app_id.is_empty()
        && app_id.len() <= 255
        && app_id.split('.').count() >= 2
        && app_id.split('.').all(segment_ok);
    if ok {
        Ok(())
    } else {
        Err(AtalantaError::Config(format!(
            "Invalid Android appId {app_id:?}: expected a package name like com.example.app \
             (letters, digits and _, dot-separated, each part starting with a letter)"
        )))
    }
}

/// Printed by [`double_tap_script`] on a device too old to double-tap.
pub const NO_DOUBLE_TAP: &str = "ATALANTA-NO-DOUBLE-TAP";

/// A device shell script that double-taps (x, y) in one `adb shell`, for
/// when the tree server is not available (VEL-S47). Two `adb shell input
/// tap` calls were two single taps: each one starts an adb process and,
/// before Android 12, an `input` JVM (300-800 ms), well past the 300 ms
/// double-tap window. From Android 12 `input` is `cmd input`, a binder call
/// of a few ms, so two in one shell land inside the window. Older devices
/// print [`NO_DOUBLE_TAP`] and tap nothing.
pub fn double_tap_script(x: i32, y: i32) -> String {
    format!(
        "if [ \"$(getprop ro.build.version.sdk)\" -ge 31 ] 2>/dev/null; \
         then input tap {x} {y} && input tap {x} {y}; else echo {NO_DOUBLE_TAP}; fi"
    )
}

/// The result of running [`double_tap_script`] on `device_id`.
pub fn double_tap_result(device_id: &str, output: &str) -> Result<()> {
    if output.contains(NO_DOUBLE_TAP) {
        return Err(AtalantaError::CommandFailed {
            device_id: device_id.to_string(),
            command: "input tap (double tap)".into(),
            detail: "a double tap needs the tree server before Android 12: two `adb shell \
                     input tap` calls are too slow to count as one double tap"
                .into(),
        });
    }
    Ok(())
}

/// A device shell script that types `text` with `input text`, for when the
/// tree server is not available.
///
/// `input text` turns `%s` into a space and has no escape for a literal
/// `%s`, so spaces are sent as `%s` and every `%` goes in an `input text`
/// of its own (where it can't start a `%s`). Each segment is quoted, so
/// nothing in `text` is shell code. A newline is the Enter key and a tab
/// the Tab key. That differs from the tree-server path, which inserts the
/// characters: here Enter in a single-line field fires its IME action
/// (submit or next), and Tab moves focus, so later text lands in the next
/// field. `input text` can only type ASCII; anything else, and other
/// control characters, is an error that names the tree server.
pub fn input_text_script(text: &str) -> Result<String> {
    if let Some(c) = text
        .chars()
        .find(|c| !c.is_ascii() || (c.is_ascii_control() && *c != '\n' && *c != '\t'))
    {
        return Err(AtalantaError::CommandFailed {
            device_id: String::new(),
            command: "input text".into(),
            detail: format!(
                "{c:?} can't be typed with `adb shell input text` (ASCII only); \
                 typing it needs Atalanta's tree server, which is not available on this device"
            ),
        });
    }
    let mut commands: Vec<String> = Vec::new();
    let mut segment = String::new();
    let flush = |segment: &mut String, commands: &mut Vec<String>| {
        if !segment.is_empty() {
            commands.push(format!("input text {}", quote(segment)));
            segment.clear();
        }
    };
    for c in text.chars() {
        match c {
            ' ' => segment.push_str("%s"),
            '%' => {
                flush(&mut segment, &mut commands);
                commands.push("input text '%'".into());
            }
            '\n' => {
                flush(&mut segment, &mut commands);
                commands.push("input keyevent 66".into());
            }
            '\t' => {
                flush(&mut segment, &mut commands);
                commands.push("input keyevent 61".into());
            }
            c => segment.push(c),
        }
    }
    flush(&mut segment, &mut commands);
    if commands.is_empty() {
        // Never an empty script: `adb shell ""` opens an interactive shell.
        return Ok("true".into());
    }
    Ok(commands.join(" && "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `script` with the host's `sh`, `input` replaced by a function
    /// that prints each argument on its own line.
    fn run_sh(script: &str) -> String {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "input() {{ for a in \"$@\"; do printf '<%s>\\n' \"$a\"; done; }}; {script}"
            ))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn quote_round_trips_through_sh() {
        let nasty = [
            "",
            "plain",
            "it's",
            "''",
            "$HOME `id` $(id) \\ \" ; & | < > * ? [a] {b} ~ # ! %",
            "line1\nline2\ttab",
            "'; touch /tmp/x; '",
        ];
        for s in nasty {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), s, "{s:?}");
        }
        assert_eq!(cmd(&["a b", "c"]), "'a b' 'c'");
    }

    #[test]
    fn input_text_script_is_inert() {
        let dir = std::env::temp_dir().join(format!("vel-shell-{}", std::process::id()));
        let marker = dir.join("PWNED");
        let text = format!(
            "price $HOME `touch {m}` $(touch {m}); rm -rf ~ & echo 'q' \"d\" \\ 100%s off",
            m = marker.display()
        );
        let script = input_text_script(&text).unwrap();
        let out = run_sh(&script);
        assert!(!marker.exists(), "text ran as shell code");
        // Joined back the way `input text` would type it (%s is a space).
        let typed: String = out
            .lines()
            .filter_map(|l| l.strip_prefix('<')?.strip_suffix('>'))
            .filter(|a| *a != "text")
            .map(|a| a.replace("%s", " "))
            .collect();
        // Equal only if the literal "%s" in the text was not typed as a space.
        assert_eq!(typed, text);
    }

    #[test]
    fn newline_and_tab_are_keys_not_commands() {
        let script = input_text_script("line1\nline2\tx").unwrap();
        assert_eq!(
            script,
            "input text 'line1' && input keyevent 66 && input text 'line2' && input keyevent 61 && input text 'x'"
        );
    }

    #[test]
    fn empty_text_is_a_no_op_script() {
        let script = input_text_script("").unwrap();
        assert!(!script.trim().is_empty());
        assert_eq!(run_sh(&script), "");
    }

    #[test]
    fn non_ascii_without_server_is_an_error() {
        for t in ["héllo", "😀", "a\u{7}b"] {
            let e = input_text_script(t).unwrap_err();
            assert!(e.to_string().contains("tree server"), "{e}");
        }
    }

    #[test]
    fn app_ids_are_validated() {
        for ok in ["com.example.app", "com.atalanta.lab", "a.b", "org.x_1.Y2"] {
            validate_app_id(ok).unwrap();
        }
        for bad in [
            "",
            "app",
            "com.example;reboot",
            "com.example app",
            "com.$(id)",
            ".com.x",
            "com..x",
            "1com.x",
            "com.x\n",
            "com.x'",
        ] {
            assert!(validate_app_id(bad).is_err(), "{bad:?} accepted");
        }
        assert!(validate_app_id(&format!("a.{}", "b".repeat(300))).is_err());
    }
}
