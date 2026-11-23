//! Regressions found in the November 2026 review of `atalanta-migrate`,
//! through the public API: each input is a Maestro flow a hostile or just
//! unusual repository could hold.

use std::fs;
use std::path::PathBuf;

use atalanta_migrate::{FileMigrationResult, MaestroMigrator, Severity};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vel-mig-review-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Migrate `files[0]` (with the others beside it); the result and the
/// output YAML ("" when none was written).
fn migrate(name: &str, files: &[(&str, &str)]) -> (FileMigrationResult, String) {
    let d = scratch(name);
    for (f, body) in files {
        fs::write(d.join(f), body).unwrap();
    }
    let out = d.join("out.atalanta.yaml");
    let r = MaestroMigrator::new()
        .migrate_file(
            &d.join(files[0].0).to_string_lossy(),
            &out.to_string_lossy(),
        )
        .unwrap();
    let yaml = fs::read_to_string(&out).unwrap_or_default();
    let _ = fs::remove_dir_all(&d);
    (r, yaml)
}

fn warned(r: &FileMigrationResult, needle: &str) -> bool {
    r.issues
        .iter()
        .any(|i| i.severity != Severity::Info && i.message.contains(needle))
}

#[test]
fn nested_repeats_are_bounded() {
    // 100 x 100 x 100 steps from nine lines: a million steps, 30 s, and a
    // file `atalanta run` refuses (100 000 steps at most).
    let flow = "appId: x\n---\n- repeat:\n    times: 100\n    commands:\n      - repeat:\n          times: 100\n          commands:\n            - repeat:\n                times: 100\n                commands:\n                  - tapOn: a\n";
    let (r, _) = migrate("repeat3", &[("a.yaml", flow)]);
    assert!(r.steps_migrated <= 100_000, "{} steps", r.steps_migrated);
    assert!(warned(&r, "unrolled steps"), "{:?}", r.issues);
}

#[test]
fn a_flow_chain_atalanta_run_refuses_is_not_a_success() {
    let n = 40;
    let mut files: Vec<(String, String)> = Vec::new();
    for i in 0..n {
        let body = if i + 1 < n {
            format!("appId: x\n---\n- runFlow: f{}.yaml\n", i + 1)
        } else {
            "appId: x\n---\n- launchApp\n".to_string()
        };
        files.push((format!("f{i}.yaml"), body));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (r, yaml) = migrate("chain", &refs);
    assert!(!r.success, "{:?}", r.issues);
    assert!(
        yaml.is_empty(),
        "nothing is written for a suite that can't run"
    );
    assert!(
        r.issues.iter().any(|i| i.message.contains("deeper than")),
        "{:?}",
        r.issues
    );
}

#[test]
fn an_app_id_the_device_shell_would_run_is_not_converted() {
    let flow = "appId: \"com.x; touch /sdcard/hdr\"\n---\n- setPermissions:\n    permissions:\n      camera: allow\n- setPermissions:\n    appId: \"com.y;touch /sdcard/p\"\n    permissions:\n      camera: allow\n";
    let (r, yaml) = migrate("inject", &[("a.yaml", flow)]);
    assert!(!yaml.contains("pm\n"), "{yaml}");
    assert!(!yaml.contains("touch /sdcard/p"), "{yaml}");
    assert!(!yaml.contains("grant\n      - com.x;"), "{yaml}");
    assert_eq!(
        r.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning && i.message.contains("skipped"))
            .count(),
        2,
        "{:?}",
        r.issues
    );
}

#[test]
fn a_media_file_name_is_quoted_for_the_device_shell() {
    let flow = "appId: x\n---\n- addMedia:\n    files:\n      - \"a b;reboot.png\"\n";
    let (_, yaml) = migrate("media", &[("a.yaml", flow)]);
    assert!(
        yaml.contains("'file:///sdcard/Pictures/a b;reboot.png'"),
        "{yaml}"
    );
}

#[test]
fn an_anchor_that_cant_convert_skips_the_step() {
    let flow = "appId: x\n---\n- tapOn:\n    text: Delete\n    below:\n      index: 1\n- swipe:\n    direction: up\n    from:\n      index: 1\n- tapOn: ok\n";
    let (r, yaml) = migrate("anchor", &[("a.yaml", flow)]);
    assert_eq!(r.steps_migrated, 1, "{yaml}");
    assert!(!yaml.contains("Delete"), "{yaml}");
    assert!(!yaml.contains("swipe"), "{yaml}");
}

#[test]
fn a_numeric_selector_text_is_kept_and_a_list_skips_the_step() {
    let flow = "appId: x\n---\n- tapOn:\n    text: 123\n    id: foo\n- tapOn:\n    text: [a, b]\n    id: foo\n";
    let (r, yaml) = migrate("textnum", &[("a.yaml", flow)]);
    assert!(yaml.contains("'123'"), "{yaml}");
    assert_eq!(r.steps_migrated, 1, "{yaml}");
    assert!(warned(&r, "isn't a string"), "{:?}", r.issues);
}

#[test]
fn a_float_typed_as_text_is_flagged() {
    let (r, _) = migrate("float", &[("a.yaml", "appId: x\n---\n- inputText: 1.50\n")]);
    assert!(
        warned(&r, "inputText: the number was read as 1.5"),
        "{:?}",
        r.issues
    );
    let (r, _) = migrate("zip", &[("a.yaml", "appId: x\n---\n- inputText: 02134\n")]);
    assert!(!warned(&r, "number was read"), "{:?}", r.issues);
}

#[test]
fn a_sub_flows_env_is_reported() {
    let (r, _) = migrate(
        "subenv",
        &[
            ("a.yaml", "appId: x\n---\n- runFlow: login.yaml\n"),
            (
                "login.yaml",
                "appId: x\nenv:\n  USER: bob\n---\n- inputText: ${USER}\n",
            ),
        ],
    );
    assert!(
        warned(&r, "login.yaml' isn't carried over"),
        "{:?}",
        r.issues
    );
}

#[test]
fn step_lines_survive_a_leading_separator_and_indented_lists() {
    // The header's `tags:` items sit at column 0 and the file opens with
    // `---`: the unknown command is on line 8.
    let flow = "---\nappId: x\ntags:\n- smoke\n- login\n---\n- launchApp\n- bogus\n";
    let (r, _) = migrate("lines1", &[("a.yaml", flow)]);
    let bogus = r
        .issues
        .iter()
        .find(|i| i.message.contains("bogus"))
        .unwrap();
    assert_eq!(bogus.line, 8);
    let flow = "appId: x\n---\n  - launchApp\n  - bogus\n";
    let (r, _) = migrate("lines2", &[("a.yaml", flow)]);
    let bogus = r
        .issues
        .iter()
        .find(|i| i.message.contains("bogus"))
        .unwrap();
    assert_eq!(bogus.line, 4);
}

#[test]
fn permissions_that_dont_convert_are_a_warning() {
    let flow = "appId: x\n---\n- setPermissions:\n    permissions:\n      all: allow\n      camera: allow\n";
    let (r, _) = migrate("perm", &[("a.yaml", flow)]);
    assert!(warned(&r, "not converted: all"), "{:?}", r.issues);
}

#[test]
fn a_flow_with_no_steps_is_not_a_clean_pass() {
    // Maestro's workspace `config.yaml`, an empty file.
    let (r, _) = migrate("cfg", &[("config.yaml", "flows:\n  - '*'\n")]);
    assert!(warned(&r, "migrated test is empty"), "{:?}", r.issues);
    let (r, _) = migrate("empty", &[("a.yaml", "")]);
    assert!(warned(&r, "migrated test is empty"), "{:?}", r.issues);
    // A runFlow of a file with nothing in it.
    let (r, _) = migrate(
        "emptysub",
        &[
            ("a.yaml", "appId: x\n---\n- runFlow: b.yaml\n"),
            ("b.yaml", "appId: x\n---\n- bogus\n"),
        ],
    );
    assert!(warned(&r, "does nothing"), "{:?}", r.issues);
}

#[test]
fn a_flow_without_an_app_id_is_flagged() {
    let (r, _) = migrate("noapp", &[("a.yaml", "---\n- launchApp\n")]);
    assert!(warned(&r, "placeholder"), "{:?}", r.issues);
}

#[test]
fn a_step_with_a_non_string_command_is_reported() {
    let (r, _) = migrate(
        "key",
        &[("a.yaml", "appId: x\n---\n- 5: foo\n- tapOn: ok\n")],
    );
    assert_eq!(r.steps_skipped, 1);
    assert!(warned(&r, "Unrecognised step"), "{:?}", r.issues);
}

#[test]
fn an_unreadable_flow_is_a_config_error() {
    let err = MaestroMigrator::new()
        .migrate_file("/nonexistent-dir/x.yaml", "/tmp/never-written.yaml")
        .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
}

#[test]
fn control_characters_in_a_report_are_escaped() {
    let flow = "appId: x\n---\n- \"\\e[2Jboom\"\n";
    let (r, _) = migrate("ansi", &[("a.yaml", flow)]);
    assert!(!r.issues.is_empty());
    for i in &r.issues {
        assert!(!i.message.chars().any(char::is_control), "{:?}", i.message);
        assert!(!i.maestro_construct.chars().any(char::is_control));
    }
}

#[test]
fn the_baseline_copy_hint_quotes_the_path() {
    let flow = "appId: x\n---\n- assertScreenshot: \"a'; rm -rf ~; echo '.png\"\n";
    let (r, _) = migrate("quote", &[("a.yaml", flow)]);
    let hint = r
        .issues
        .iter()
        .find(|i| i.message.starts_with("copy the reference image"))
        .unwrap();
    assert!(
        hint.message
            .contains("cp 'a'\\''; rm -rf ~; echo '\\''.png'"),
        "{}",
        hint.message
    );
}
