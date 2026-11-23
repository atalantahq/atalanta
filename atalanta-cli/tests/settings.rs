// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta settings` shows the value each setting's variable has now: a
//! secret's must not be among them (its help says it is masked).

use std::process::Command;

fn settings(filter: &str, env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atalanta"));
    cmd.args(["--color", "never", "settings", filter]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_secret_setting_is_listed_without_its_value() {
    let text = settings(
        "runnerToken",
        &[("ATALANTA_IOS_RUNNER_TOKEN", "tok-0123456789abcdef-sekret")],
    );
    assert!(text.contains("ios.runnerToken"), "{text}");
    assert!(!text.contains("0123456789abcdef"), "{text}");
    assert!(text.contains("(now set)"), "it says it is set: {text}");
}

#[test]
fn the_list_of_secret_names_is_still_shown() {
    let text = settings("secrets", &[("ATALANTA_SECRETS", "MY_PIN,MY_TOKEN")]);
    assert!(text.contains("(now MY_PIN,MY_TOKEN)"), "{text}");
}

#[test]
fn an_ordinary_setting_still_shows_its_value() {
    let text = settings(
        "typingFrequency",
        &[("ATALANTA_IOS_TYPING_FREQUENCY", "240")],
    );
    assert!(text.contains("(now 240)"), "{text}");
}
