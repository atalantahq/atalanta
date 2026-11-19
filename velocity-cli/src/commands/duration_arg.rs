//! Duration flags (`--timeout`, `--lease-wait`) with one rule for every
//! subcommand.
//!
//! A value with a unit suffix means what it says: `500ms`, `30s`, `10m`,
//! `1h`, `1.5s` (one number, one unit: `2m30s` is rejected). A
//! bare number keeps the unit that flag has always had, so existing scripts
//! keep working: milliseconds on `run --timeout`, `doctor --timeout` and the
//! agent commands' `--timeout`; seconds on `explore --timeout` and
//! `explore --lease-wait`. A bare number that is almost certainly in the
//! other unit (a `run --timeout` under one second, an `explore --timeout`
//! over a day) gets a warning naming the suffixed form.

use std::time::Duration;

/// What a bare number means for a flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BareUnit {
    Millis,
    Secs,
}

/// Parse `text` as a duration, a bare number being in `bare`. `Ok` carries
/// the duration and, for a bare number that looks like the other unit, a
/// warning to print.
pub fn parse(text: &str, bare: BareUnit) -> Result<(Duration, Option<String>), String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("empty duration: give a number with a unit, like 30s or 500ms".into());
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = num.parse().map_err(|_| {
        format!("invalid duration {text:?}: expected a number with a unit, like 30s or 500ms")
    })?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid duration {text:?}: must not be negative"));
    }
    let secs = match unit.trim().to_ascii_lowercase().as_str() {
        "" => {
            let secs = match bare {
                BareUnit::Millis => value / 1000.0,
                BareUnit::Secs => value,
            };
            let d = Duration::from_secs_f64(secs);
            return Ok((d, bare_warning(t, value, bare)));
        }
        "ms" => value / 1000.0,
        "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value * 60.0,
        "h" => value * 3600.0,
        other => {
            return Err(format!(
                "invalid duration {text:?}: unknown unit {other:?} (use ms, s, m or h)"
            ))
        }
    };
    Ok((Duration::from_secs_f64(secs), None))
}

fn bare_warning(text: &str, value: f64, bare: BareUnit) -> Option<String> {
    match bare {
        BareUnit::Millis if value > 0.0 && value < 1000.0 => Some(format!(
            "warning: the bare duration {text} means {text} milliseconds here; write {text}s if you meant seconds"
        )),
        BareUnit::Secs if value >= 86_400.0 => Some(format!(
            "warning: the bare duration {text} means {text} seconds here (over a day); write {text}ms if you meant milliseconds"
        )),
        _ => None,
    }
}

fn parse_and_warn(text: &str, bare: BareUnit) -> Result<Duration, String> {
    let (d, warning) = parse(text, bare)?;
    if let Some(w) = warning {
        eprintln!("{w}");
    }
    Ok(d)
}

/// clap `value_parser` for a duration whose bare number is milliseconds.
pub fn bare_millis(text: &str) -> Result<Duration, String> {
    parse_and_warn(text, BareUnit::Millis)
}

/// clap `value_parser` for a duration whose bare number is seconds.
pub fn bare_secs(text: &str) -> Result<Duration, String> {
    parse_and_warn(text, BareUnit::Secs)
}

/// Whole milliseconds, saturating.
pub fn as_millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(text: &str, bare: BareUnit) -> u64 {
        as_millis_u64(parse(text, bare).unwrap().0)
    }

    #[test]
    fn suffixes_mean_the_same_on_every_flag() {
        for bare in [BareUnit::Millis, BareUnit::Secs] {
            assert_eq!(ms("500ms", bare), 500);
            assert_eq!(ms("30s", bare), 30_000);
            assert_eq!(ms("1.5s", bare), 1_500);
            assert_eq!(ms("10m", bare), 600_000);
            assert_eq!(ms("1h", bare), 3_600_000);
            assert_eq!(ms(" 2S ", bare), 2_000);
        }
    }

    #[test]
    fn a_bare_number_keeps_the_flag_s_historical_unit() {
        assert_eq!(ms("20000", BareUnit::Millis), 20_000);
        assert_eq!(ms("600", BareUnit::Secs), 600_000);
    }

    #[test]
    fn a_bare_number_in_the_wrong_unit_warns() {
        assert!(parse("600", BareUnit::Millis).unwrap().1.is_some());
        assert!(parse("1800000", BareUnit::Secs).unwrap().1.is_some());
        assert!(parse("20000", BareUnit::Millis).unwrap().1.is_none());
        assert!(parse("600", BareUnit::Secs).unwrap().1.is_none());
        assert!(parse("600ms", BareUnit::Millis).unwrap().1.is_none());
    }

    #[test]
    fn junk_is_rejected_with_the_accepted_forms() {
        for bad in ["", "abc", "10d", "-5s", "s"] {
            let e = parse(bad, BareUnit::Millis).unwrap_err();
            assert!(e.contains("ms") || e.contains("negative"), "{bad}: {e}");
        }
    }
}
