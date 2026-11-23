//! Every complete suite example in the README must parse and validate. The
//! Quick Start once didn't — the first thing a new user copies.

use velocity_core::{parse_suite_from_str, validate_suite};

#[test]
fn readme_suite_examples_parse_and_validate() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("README.md");
    let mut checked = 0;
    for block in readme.split("```yaml\n").skip(1) {
        let yaml = block.split("```").next().unwrap_or("");
        // Only whole suites; fragments (a config excerpt, a `tests:` snippet)
        // are illustrations.
        if !(yaml.contains("appId:") && yaml.contains("tests:")) {
            continue;
        }
        let suite = parse_suite_from_str(yaml)
            .unwrap_or_else(|e| panic!("README example failed to parse: {e}\n---\n{yaml}"));
        validate_suite(&suite)
            .unwrap_or_else(|e| panic!("README example failed validation: {e}\n---\n{yaml}"));
        assert_steps_point_at_their_lines(&suite, yaml);
        checked += 1;
    }
    assert!(checked >= 1, "expected at least the Quick Start example");
}

/// The same for every whole suite in `docs/*.md` (U2), and the DSL guide
/// must have at least one, so the docs' examples can't rot.
#[test]
fn docs_suite_examples_parse_and_validate() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("docs/").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for block in text.split("```yaml\n").skip(1) {
            let yaml = block.split("```").next().unwrap_or("");
            if !(yaml.contains("appId:") && yaml.contains("tests:")) {
                continue;
            }
            let suite = parse_suite_from_str(yaml).unwrap_or_else(|e| {
                panic!(
                    "{}: example failed to parse: {e}\n---\n{yaml}",
                    path.display()
                )
            });
            validate_suite(&suite).unwrap_or_else(|e| {
                panic!(
                    "{}: example failed validation: {e}\n---\n{yaml}",
                    path.display()
                )
            });
            checked += 1;
        }
    }
    assert!(checked >= 2, "expected DSL.md's examples, found {checked}");
}

#[test]
fn every_repo_suite_has_a_line_for_every_test_and_step() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let mut files = vec![
        format!("{root}/tests/smoke/android-settings.yaml"),
        format!("{root}/tests/smoke/ios-settings.yaml"),
        format!("{root}/examples/web-example/velocity.yaml"),
    ];
    for dir in ["tests", "tests/fixtures"] {
        if let Ok(entries) = std::fs::read_dir(format!("{root}/{dir}")) {
            files.extend(
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
                    .map(|p| p.to_string_lossy().into_owned()),
            );
        }
    }
    let mut checked = 0;
    for file in files {
        let Ok((suite, tests)) = velocity_core::parse_suite_with_positions(&file) else {
            continue; // fixtures that are meant not to parse
        };
        let yaml = std::fs::read_to_string(&file).unwrap();
        assert_steps_point_at_their_lines(&suite, &yaml);
        for t in &suite.tests {
            let pos = &tests[&t.name];
            assert_eq!(pos.file, file);
            let line = yaml.lines().nth(pos.line - 1).unwrap();
            assert!(
                line.contains(&t.name) || line.contains("name"),
                "{file}:{}: {line}",
                pos.line
            );
        }
        checked += 1;
    }
    assert!(checked >= 2, "expected the smoke and web suites at least");
}

/// Every step knows its line, and that line is where the step is written:
/// it holds the step's action key (or its plain-English text).
fn assert_steps_point_at_their_lines(suite: &velocity_common::TestSuite, yaml: &str) {
    let lines: Vec<&str> = yaml.lines().collect();
    let steps = suite
        .tests
        .iter()
        .flat_map(|t| &t.steps)
        .chain(suite.flows.iter().flat_map(|f| &f.steps));
    for step in steps {
        let pos = step
            .source
            .as_ref()
            .unwrap_or_else(|| panic!("step without a position: {:?}\n---\n{yaml}", step.action));
        let line = lines[pos.line - 1].trim_start();
        assert!(
            line.starts_with("- ") || line.starts_with('{'),
            "line {} is not a step: {line:?}\n---\n{yaml}",
            pos.line
        );
    }
}

/// Every Velocity suite under `tests/` and `examples/` parses, validates,
/// and uses no key Velocity ignores. Maestro inputs for the migrator
/// (`maestro_compat`) are not Velocity suites.
#[test]
fn every_repo_suite_parses_validates_and_has_no_unknown_keys() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for p in entries.flatten().map(|e| e.path()) {
            if p.is_dir() {
                if !p.ends_with("maestro_compat") {
                    walk(&p, out);
                }
            } else if p.extension().is_some_and(|x| x == "yaml" || x == "yml") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let mut files = Vec::new();
    walk(&root.join("tests"), &mut files);
    walk(&root.join("examples"), &mut files);
    let mut checked = 0;
    for file in &files {
        let path = file.to_string_lossy();
        let yaml = std::fs::read_to_string(file).unwrap();
        if !(yaml.contains("appId:") && yaml.contains("tests:")) {
            continue;
        }
        let suite = velocity_core::parse_suite(&path)
            .unwrap_or_else(|e| panic!("{path} failed to parse: {e}"));
        validate_suite(&suite).unwrap_or_else(|e| panic!("{path} failed validation: {e}"));
        let unknown = velocity_core::config_keys::unknown_keys(&yaml);
        assert!(unknown.is_empty(), "{path}: {unknown:#?}");
        checked += 1;
    }
    assert!(
        files.iter().any(|f| f.ends_with("android-settings.yaml")) && checked >= 5,
        "expected the smoke suites and the fixtures, checked {checked}"
    );
}
