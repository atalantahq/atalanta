// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `screenshots.freezeStatusBar` on Android (D-V4): SystemUI demo mode, so
//! the status bar shows the same clock, battery, signal and no
//! notifications in every capture, and can be compared instead of masked.
//! AOSP: packages/SystemUI/docs/demo_mode.md. Exited again on cleanup.

/// The demo-mode broadcast for one command, with its extras.
fn demo(command: &str, extras: &str) -> String {
    let extras = if extras.is_empty() {
        String::new()
    } else {
        format!(" {extras}")
    };
    format!("am broadcast -a com.android.systemui.demo -e command {command}{extras}")
}

/// What enters demo mode: allow it, enter, then pin each status-bar item.
pub fn enter_commands() -> Vec<String> {
    vec![
        "settings put global sysui_demo_allowed 1".into(),
        demo("enter", ""),
        demo("clock", "-e hhmm 1200"),
        demo("battery", "-e level 100 -e plugged false"),
        demo("network", "-e wifi show -e level 4"),
        demo("network", "-e mobile show -e datatype none -e level 4"),
        demo("notifications", "-e visible false"),
    ]
}

/// Read back after entering (review-d5 m-1): `am broadcast` exits 0
/// whether or not SystemUI acts on it, so success of the shell alone says
/// nothing. First the setting that allows demo mode, then
/// `sysui_tuner_demo_on`, which SystemUI's `DemoModeController` (Android
/// 12+) writes itself when it enters demo mode (and clears on exit), so it
/// shows that SystemUI acted on `enter`, not only that the shell could
/// write a setting (review-fixes #8).
pub const VERIFY: &str = "settings get global sysui_demo_allowed";
pub const VERIFY_ON: &str = "settings get global sysui_tuner_demo_on";

/// What the read-back says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// SystemUI says it is in demo mode.
    Entered,
    /// Allowed, but this SystemUI never writes `sysui_tuner_demo_on`
    /// (`null`, before Android 12): the allowed setting is all there is
    /// to read, so it counts (the known limit).
    Unknown,
    /// Allowed, but SystemUI isn't in demo mode yet: `enter` can arrive
    /// before SystemUI has seen `sysui_demo_allowed` and be ignored.
    NotYet,
    /// Demo mode isn't allowed: the setting didn't stick.
    Refused,
}

/// The verdict from the output of the enter commands followed by
/// [`VERIFY`] and [`VERIFY_ON`] (their two last lines).
pub fn verdict(output: &str) -> Verdict {
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev();
    let on = lines.next().unwrap_or_default();
    let allowed = lines.next().unwrap_or_default();
    if allowed != "1" {
        return Verdict::Refused;
    }
    match on {
        "1" => Verdict::Entered,
        "null" => Verdict::Unknown,
        _ => Verdict::NotYet,
    }
}

/// The broadcasts again, after a pause that lets SystemUI's observer see
/// `sysui_demo_allowed`, then the read-back: the one retry for
/// [`Verdict::NotYet`].
pub fn retry_commands() -> Vec<String> {
    let mut v = vec!["sleep 0.3".to_string()];
    v.extend(enter_commands().into_iter().skip(1));
    v.push(VERIFY.into());
    v.push(VERIFY_ON.into());
    v
}

/// What leaves it: SystemUI shows the real status bar again.
pub fn exit_commands() -> Vec<String> {
    vec![demo("exit", "")]
}

#[cfg(test)]
mod tests {
    use crate::adb::Adb;
    use crate::driver::AndroidDriver;
    use crate::test_support::FakeAdb;
    use atalanta_common::PlatformDriver;

    #[tokio::test]
    async fn freeze_status_bar_enters_demo_mode_once_and_cleanup_exits_it() {
        let fake = FakeAdb::new("demo-mode");
        fake.stub("settings", r#"if [ "$1" = get ]; then echo 1; fi"#);
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(d.freeze_status_bar("emulator-5554").await);
        assert!(d.freeze_status_bar("emulator-5554").await, "still frozen");
        assert_eq!(
            fake.calls("settings"),
            [
                "SETTINGS:[put][global][sysui_demo_allowed][1]",
                "SETTINGS:[get][global][sysui_demo_allowed]",
                "SETTINGS:[get][global][sysui_tuner_demo_on]"
            ]
        );
        let am = fake.calls("am");
        assert_eq!(
            am,
            [
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][enter]",
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][clock][-e][hhmm][1200]",
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][battery][-e][level][100][-e][plugged][false]",
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][network][-e][wifi][show][-e][level][4]",
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][network][-e][mobile][show][-e][datatype][none][-e][level][4]",
                "AM:[broadcast][-a][com.android.systemui.demo][-e][command][notifications][-e][visible][false]",
            ]
        );
        d.cleanup().await;
        assert_eq!(
            fake.calls("am").last().unwrap(),
            "AM:[broadcast][-a][com.android.systemui.demo][-e][command][exit]"
        );
    }

    #[tokio::test]
    async fn a_device_that_refuses_demo_mode_keeps_its_mask() {
        let fake = FakeAdb::new("demo-mode-refused");
        fake.stub("settings", "exit 1");
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(!d.freeze_status_bar("emulator-5554").await);
        d.cleanup().await;
        assert!(fake.calls("am").is_empty(), "nothing to exit");
    }

    /// review-d5 m-4: the end of a run (also a daemon's, which keeps the
    /// driver) leaves demo mode; the next run's freeze enters it again.
    #[tokio::test]
    async fn unfreeze_status_bar_exits_demo_mode_and_a_later_freeze_reenters() {
        let fake = FakeAdb::new("demo-mode-thaw");
        fake.stub("settings", r#"if [ "$1" = get ]; then echo 1; fi"#);
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(d.freeze_status_bar("emulator-5554").await);
        d.unfreeze_status_bar().await;
        assert_eq!(
            fake.calls("am").last().unwrap(),
            "AM:[broadcast][-a][com.android.systemui.demo][-e][command][exit]"
        );
        assert!(d.freeze_status_bar("emulator-5554").await);
        let enters = fake
            .calls("am")
            .iter()
            .filter(|c| c.ends_with("[command][enter]"))
            .count();
        assert_eq!(enters, 2);
    }

    /// review-d5 m-1: every command exits 0 but the setting doesn't read
    /// back as 1 (a build whose SystemUI ignores demo mode): not frozen,
    /// so the status bar stays masked.
    #[tokio::test]
    async fn demo_mode_that_does_not_read_back_is_not_a_freeze() {
        let fake = FakeAdb::new("demo-mode-noop");
        fake.stub("settings", r#"if [ "$1" = get ]; then echo null; fi"#);
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(!d.freeze_status_bar("emulator-5554").await);
    }

    /// review-fixes #8: the shell can always write `sysui_demo_allowed`, so
    /// reading it back proved nothing. SystemUI's own `sysui_tuner_demo_on`
    /// staying 0 means `enter` was ignored: the broadcasts are re-sent
    /// once, and a SystemUI that still isn't in demo mode keeps its mask.
    #[tokio::test]
    async fn demo_mode_that_systemui_never_entered_keeps_the_mask() {
        let fake = FakeAdb::new("demo-mode-ignored");
        fake.stub(
            "settings",
            r#"case "$1 $3" in "get sysui_demo_allowed") echo 1;; "get sysui_tuner_demo_on") echo 0;; esac"#,
        );
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(!d.freeze_status_bar("emulator-5554").await);
        let enters = fake
            .calls("am")
            .iter()
            .filter(|c| c.ends_with("[command][enter]"))
            .count();
        assert_eq!(enters, 2, "one retry of the broadcasts");
        assert!(fake
            .calls("am")
            .last()
            .unwrap()
            .ends_with("[command][exit]"));
    }

    /// The race the read-back exists for: SystemUI saw the setting late and
    /// ignored the first `enter`; the retry takes.
    #[tokio::test]
    async fn a_late_systemui_is_entered_on_the_retry() {
        let fake = FakeAdb::new("demo-mode-late");
        let marker = std::env::temp_dir().join(format!("vel-demo-late-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        fake.stub(
            "settings",
            &format!(
                r#"case "$1 $3" in "get sysui_demo_allowed") echo 1;; "get sysui_tuner_demo_on") if [ -e '{m}' ]; then echo 1; else touch '{m}'; echo 0; fi;; esac"#,
                m = marker.display()
            ),
        );
        let d = AndroidDriver::for_tests(Box::new(Adb::with_path(fake.path())));
        assert!(d.freeze_status_bar("emulator-5554").await);
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn an_old_systemui_that_never_writes_the_flag_counts_as_entered() {
        use super::{verdict, Verdict};
        assert_eq!(verdict("1\nnull\n"), Verdict::Unknown);
        assert_eq!(verdict("1\n1"), Verdict::Entered);
        assert_eq!(verdict("1\n0"), Verdict::NotYet);
        assert_eq!(verdict("0\n1"), Verdict::Refused);
        assert_eq!(verdict(""), Verdict::Refused);
    }
}
