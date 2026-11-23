//! One colour switch for everything Velocity prints (C4, VEL-174).
//!
//! The CLI printed through `colored` (which only looked at `NO_COLOR`,
//! `CLICOLOR` and a TTY on its own), the runner wrote raw ANSI escapes
//! whatever the terminal, and log lines on stderr had their own rule. Now
//! one decision, made once per process (per run under `velocity daemon`),
//! is read by all of them:
//!
//! 1. `--color always|never` wins;
//! 2. `NO_COLOR` set to anything but the empty string: off (no-color.org);
//! 3. `CLICOLOR_FORCE` set and not `0`: on;
//! 4. `CLICOLOR=0`, or `TERM=dumb`: off;
//! 5. otherwise on when the stream is a terminal.
//!
//! Output that carries escapes of its own goes through [`paint`], which
//! strips them when colour is off.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};

/// `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl std::str::FromStr for ColorChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "always" | "yes" | "on" => Ok(Self::Always),
            "never" | "no" | "off" => Ok(Self::Never),
            other => Err(format!("--color {other}: expected auto, always or never")),
        }
    }
}

const UNSET: u8 = 0;
const ON: u8 = 1;
const OFF: u8 = 2;

/// The decision for stdout, once made.
static STDOUT: AtomicU8 = AtomicU8::new(UNSET);

/// Whether to colour a stream, by the rules in the module docs. `var`
/// reads an environment variable; `tty` says whether the stream is a
/// terminal.
pub fn decide(choice: ColorChoice, var: impl Fn(&str) -> Option<String>, tty: bool) -> bool {
    match choice {
        ColorChoice::Always => return true,
        ColorChoice::Never => return false,
        ColorChoice::Auto => {}
    }
    if var("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if var("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if var("CLICOLOR").is_some_and(|v| v == "0") || var("TERM").is_some_and(|v| v == "dumb") {
        return false;
    }
    tty
}

/// [`decide`] with the process environment (or the one a daemon run
/// installed, see [`crate::env`]).
pub fn resolve(choice: ColorChoice, tty: bool) -> bool {
    decide(choice, |k| crate::env::var(k).ok(), tty)
}

/// Decide for stdout from `choice` and remember it: what [`enabled`] and
/// [`paint`] return from now on. Returns the decision.
pub fn init(choice: ColorChoice) -> bool {
    use std::io::IsTerminal;
    let on = resolve(choice, std::io::stdout().is_terminal());
    set(on);
    on
}

/// Set the decision outright (`velocity daemon` uses the client's).
pub fn set(on: bool) {
    STDOUT.store(if on { ON } else { OFF }, Ordering::SeqCst);
}

/// Whether stdout is coloured. Before [`init`], decided from the
/// environment and the terminal, as `--color auto` would.
pub fn enabled() -> bool {
    match STDOUT.load(Ordering::SeqCst) {
        ON => true,
        OFF => false,
        _ => init(ColorChoice::Auto),
    }
}

/// `s` as it should be printed: unchanged with colour on, without its ANSI
/// colour escapes (`ESC [ … m`) with colour off.
pub fn paint(s: &str) -> Cow<'_, str> {
    if enabled() {
        Cow::Borrowed(s)
    } else {
        strip(s)
    }
}

/// `s` without ANSI SGR escapes (`ESC [ <digits;…> m`).
pub fn strip(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let params = tail
            .strip_prefix('[')
            .map(|t| t.find(|c: char| !(c.is_ascii_digit() || c == ';')));
        match params {
            Some(Some(n)) if tail[1 + n..].starts_with('m') => rest = &tail[n + 2..],
            // Not a colour escape: kept as it is.
            _ => {
                out.push('\x1b');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_flag_wins_then_no_color_then_clicolor_then_the_terminal() {
        use ColorChoice::*;
        assert!(decide(Always, env(&[("NO_COLOR", "1")]), false));
        assert!(!decide(Never, env(&[("CLICOLOR_FORCE", "1")]), true));
        assert!(!decide(Auto, env(&[("NO_COLOR", "1")]), true));
        // An empty NO_COLOR is unset (no-color.org).
        assert!(decide(Auto, env(&[("NO_COLOR", "")]), true));
        assert!(!decide(
            Auto,
            env(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]),
            true
        ));
        assert!(decide(Auto, env(&[("CLICOLOR_FORCE", "1")]), false));
        assert!(!decide(Auto, env(&[("CLICOLOR_FORCE", "0")]), false));
        assert!(!decide(Auto, env(&[("CLICOLOR", "0")]), true));
        assert!(!decide(Auto, env(&[("TERM", "dumb")]), true));
        assert!(decide(Auto, env(&[("CLICOLOR", "1")]), true));
        assert!(!decide(Auto, env(&[]), false));
        assert!(decide(Auto, env(&[]), true));
    }

    #[test]
    fn choices_parse() {
        assert_eq!("never".parse::<ColorChoice>(), Ok(ColorChoice::Never));
        assert_eq!("ALWAYS".parse::<ColorChoice>(), Ok(ColorChoice::Always));
        assert_eq!("auto".parse::<ColorChoice>(), Ok(ColorChoice::Auto));
        assert!("sometimes".parse::<ColorChoice>().is_err());
    }

    #[test]
    fn escapes_are_stripped_and_nothing_else() {
        assert_eq!(strip("  \x1b[33m⚠ slow\x1b[0m done"), "  ⚠ slow done");
        assert_eq!(strip("\x1b[1;31mred\x1b[m"), "red");
        assert_eq!(strip("plain"), "plain");
        assert!(matches!(strip("plain"), Cow::Borrowed(_)));
        // Not a colour escape: left alone.
        assert_eq!(strip("a\x1b[2Jb\x1b"), "a\x1b[2Jb\x1b");
    }
}
