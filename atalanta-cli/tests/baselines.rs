// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta baselines review|accept|reject` (U1) over a results dir laid
//! out the way a failed `assertScreenshot` leaves it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 1x1 RGBA PNGs: black and white.
const BLACK: [u8; 70] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 96, 96, 96, 248, 15, 0, 1,
    4, 1, 0, 95, 229, 195, 75, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const WHITE: [u8; 68] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9, 251, 3,
    253, 251, 94, 107, 43, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "vbase-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One failed comparison per (results subfolder, name); returns the
/// baseline paths.
fn fail(root: &Path, shots: &[(&str, &str)]) -> Vec<PathBuf> {
    shots
        .iter()
        .map(|(sub, name)| {
            let diffs = root.join("atalanta-results").join(sub).join("diffs");
            std::fs::create_dir_all(&diffs).unwrap();
            let baseline = root.join(format!("suite/atalanta-baselines/android/1x1/{name}.png"));
            std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
            // A failing run never rewrites the baseline: write it once, so
            // it isn't newer than an earlier shard's record (fi56).
            if !baseline.exists() {
                std::fs::write(&baseline, BLACK).unwrap();
            }
            for (kind, png) in [
                ("expected", &BLACK[..]),
                ("actual", &WHITE),
                ("diff", &WHITE),
            ] {
                std::fs::write(diffs.join(format!("{name}-{kind}.png")), png).unwrap();
            }
            atalanta_core::baselines::record_failure(
                &diffs,
                &format!("{name}.png"),
                &atalanta_core::baselines::ReviewRecord {
                    name: name.to_string(),
                    baseline: baseline.clone(),
                    detail: "93.10% similar; largest change 40 px".into(),
                    baseline_hash: None,
                },
            )
            .unwrap();
            baseline
        })
        .collect()
}

fn atalanta(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atalanta"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn review_lists_and_accept_writes_the_actual_screen() {
    let root = tmp("accept");
    let b = fail(&root, &[("", "home"), ("", "cart"), ("shard-2", "home")]);

    let o = atalanta(&root, &["baselines", "review"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("3 failed screenshot comparison(s)"), "{out}");
    for want in [
        "diffs/home",
        "diffs/cart",
        "shard-2/diffs/home",
        "93.10% similar",
        "home-diff.png",
        "atalanta-baselines/android/1x1/home.png",
    ] {
        assert!(out.contains(want), "{want} missing:\n{out}");
    }
    let o = atalanta(&root, &["baselines", "review", "--json"]);
    let list: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 3);

    // A name accepts every comparison under it.
    let o = atalanta(&root, &["baselines", "accept", "home"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(out.matches("accepted; wrote").count(), 2, "{out}");
    assert_eq!(std::fs::read(&b[0]).unwrap(), WHITE);
    assert_eq!(std::fs::read(&b[1]).unwrap(), BLACK, "cart untouched");

    let o = atalanta(&root, &["baselines", "review"]);
    let out = text(&o);
    assert!(out.contains("1 failed screenshot comparison(s)"), "{out}");
    assert!(!out.contains("diffs/home"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reject_keeps_the_baseline_and_unknown_names_exit_2() {
    let root = tmp("reject");
    let b = fail(&root, &[("", "home"), ("", "cart")]);

    let o = atalanta(&root, &["baselines", "accept", "nope"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}");
    assert!(out.contains("cart") && out.contains("home"), "{out}");

    let o = atalanta(&root, &["baselines", "reject", "all"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert_eq!(std::fs::read(&b[0]).unwrap(), BLACK);
    assert_eq!(std::fs::read(&b[1]).unwrap(), BLACK);
    let o = atalanta(&root, &["baselines", "review"]);
    assert!(text(&o).contains("No failed screenshot comparisons"));
    // `all` with nothing pending is fine.
    let o = atalanta(&root, &["baselines", "accept", "all"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));

    // --results-dir points elsewhere.
    let other = tmp("reject-other");
    let o = atalanta(
        &root,
        &[
            "baselines",
            "review",
            "--results-dir",
            other.to_str().unwrap(),
        ],
    );
    assert!(text(&o).contains("No failed screenshot comparisons"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&other);
}

/// Artifacts from a CI runner name the runner's absolute baseline path;
/// `--baselines-dir` accepts into this checkout at the same layout.
#[test]
fn accept_into_a_local_checkout_with_baselines_dir() {
    let root = tmp("ci");
    let b = fail(&root, &[("", "home")]);
    let local = root.join("checkout/atalanta-baselines");
    // The checkout holds the same baseline CI compared against (its
    // content, not its mtime, is what counts: review-fi56 MAJOR-2).
    std::fs::create_dir_all(local.join("android/1x1")).unwrap();
    std::fs::write(local.join("android/1x1/home.png"), BLACK).unwrap();
    let o = atalanta(
        &root,
        &[
            "baselines",
            "accept",
            "home",
            "--baselines-dir",
            local.to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert_eq!(
        std::fs::read(local.join("android/1x1/home.png")).unwrap(),
        WHITE
    );
    assert_eq!(
        std::fs::read(&b[0]).unwrap(),
        BLACK,
        "recorded path untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// fi56 MAJOR 1: a baseline re-recorded after the failure was recorded is
/// skipped by `accept all` with a warning (exit 0), not overwritten. Only a
/// change of content counts: `cart` merely touched (a checkout) is accepted.
#[test]
fn accept_all_skips_a_baseline_newer_than_its_record() {
    let root = tmp("stale");
    let b = fail(&root, &[("", "home"), ("", "cart")]);
    let rerecorded = [&BLACK[..], b"re-recorded"].concat();
    std::fs::write(&b[0], &rerecorded).unwrap();
    for f in &b {
        std::fs::File::options()
            .write(true)
            .open(f)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
    }
    let o = atalanta(&root, &["baselines", "accept", "all"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(
        out.contains("warning:") && out.contains("changed after"),
        "{out}"
    );
    assert_eq!(std::fs::read(&b[0]).unwrap(), rerecorded, "home kept");
    assert_eq!(std::fs::read(&b[1]).unwrap(), WHITE, "cart accepted");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn review_serve_takes_baselines_dir() {
    let o = atalanta(
        &std::env::temp_dir(),
        &[
            "baselines",
            "review",
            "--serve",
            "--baselines-dir",
            "x",
            "--help",
        ],
    );
    assert!(text(&o).contains("--baselines-dir"), "{}", text(&o));
    // Without --serve it is refused (it only matters to the page's Accept).
    let o = atalanta(
        &std::env::temp_dir(),
        &["baselines", "review", "--baselines-dir", "x"],
    );
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}
