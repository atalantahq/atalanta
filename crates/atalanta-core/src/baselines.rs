// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Where `assertScreenshot` baselines live, and reviewing the comparisons
//! that failed (U1).
//!
//! A baseline is
//! `<suite dir>/atalanta-baselines/<platform>[-headless]/<W>x<H>/<file>`
//! ([`baseline_path`]; `W`x`H` is the device's full capture). A failed
//! comparison leaves `<stem>-expected.png`, `<stem>-actual.png` and
//! `<stem>-diff.png` in `<results dir>/diffs`, plus a `<stem>-review.json`
//! record naming the baseline the step compared against, so a reviewer
//! (`atalanta baselines`, the inspector's `/review` page) can replace it
//! with the actual frame without re-resolving the suite's layout.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The folder under a suite's directory that holds its baselines.
pub const BASELINES_DIR: &str = "atalanta-baselines";

/// Suffix of the record a failed comparison leaves next to its images.
const RECORD_SUFFIX: &str = "-review.json";

/// The baseline file for `assertScreenshot(name)`: the name itself when it
/// is already a portable file name (`[A-Za-z0-9_-]`, ≤ 100 bytes), so
/// existing baselines keep their file; otherwise the cleaned name plus a
/// hash of the original, so `cart total` and `cart/total` never share a
/// file (VEL-201).
pub fn baseline_file(name: &str) -> String {
    const MAX: usize = 100;
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean == name && !clean.is_empty() && clean.len() <= MAX {
        return clean + ".png";
    }
    // FNV-1a: stable across runs and Rust versions.
    let hash = name.bytes().fold(0x811c9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x01000193)
    });
    format!("{}-{hash:08x}.png", &clean[..clean.len().min(MAX)])
}

/// The baselines folder for a platform: `ios`, `android`, `web`, or the
/// same with `-headless` (headless renders never meet device baselines, B1).
pub fn baseline_space(platform: &str, headless: bool) -> String {
    if headless {
        format!("{platform}-headless")
    } else {
        platform.to_string()
    }
}

/// Where `assertScreenshot(name)` writes its baseline for a `w`x`h`
/// capture: `<suite_dir>/atalanta-baselines/<space>/<W>x<H>/<file>`.
pub fn baseline_path(suite_dir: &Path, space: &str, (w, h): (u32, u32), name: &str) -> PathBuf {
    suite_dir
        .join(BASELINES_DIR)
        .join(space)
        .join(format!("{w}x{h}"))
        .join(baseline_file(name))
}

/// What a failed comparison records for review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRecord {
    /// The `assertScreenshot` name.
    pub name: String,
    /// The baseline accepting replaces: the one at this capture size.
    pub baseline: PathBuf,
    /// The step's failure ("97.12% similar; largest change …").
    #[serde(default)]
    pub detail: String,
    /// The content of `baseline` when the comparison failed:
    /// `sha256:<hex>`, or [`ABSENT`] when there was no such file. Filled by
    /// [`record_failure`]. An accept is stale only when the baseline it
    /// would replace no longer has this content; records without it
    /// (written before it existed) fall back to comparing mtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_hash: Option<String>,
}

/// [`ReviewRecord::baseline_hash`] of a baseline that didn't exist.
pub const ABSENT: &str = "absent";

/// `sha256:<hex>` of the file at `path`, or [`ABSENT`] when there is none.
/// The content, not the mtime: a checkout, clone or `unzip` changes mtimes
/// without changing a baseline (review-fi56 MAJOR-2).
pub fn content_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest = Sha256::digest(&bytes);
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            format!("sha256:{hex}")
        }
        Err(_) => ABSENT.to_string(),
    }
}

fn record_path(diffs_dir: &Path, file: &str) -> PathBuf {
    let stem = file.strip_suffix(".png").unwrap_or(file);
    diffs_dir.join(format!("{stem}{RECORD_SUFFIX}"))
}

/// Record a failed comparison of baseline file `file` (whose images are in
/// `diffs_dir`). The baseline path is stored absolute, so a review from
/// another directory still finds it, with the baseline's content hash at
/// this moment (unless the caller gave one).
pub fn record_failure(diffs_dir: &Path, file: &str, record: &ReviewRecord) -> std::io::Result<()> {
    let mut record = record.clone();
    if let Ok(abs) = std::path::absolute(&record.baseline) {
        record.baseline = abs;
    }
    if record.baseline_hash.is_none() {
        record.baseline_hash = Some(content_hash(&record.baseline));
    }
    std::fs::create_dir_all(diffs_dir)?;
    let json = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
    crate::visual::write_atomic(&record_path(diffs_dir, file), &json)
}

/// Forget an earlier failure of `file` against one of `baselines` (its
/// images are now something else, such as an unstable screen's frames, or
/// that baseline passed, so it can't be accepted). Records are named by
/// file only, so one output dir can hold `home.png`'s record for another
/// size folder or platform: that one is kept (review-fi56 m1). A record
/// that can't be read is removed.
pub fn clear_record(diffs_dir: &Path, file: &str, baselines: &[&Path]) {
    let path = record_path(diffs_dir, file);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let ours = match serde_json::from_slice::<ReviewRecord>(&bytes) {
        Ok(record) => {
            let recorded = canonical_path(&record.baseline);
            baselines.iter().any(|b| canonical_path(b) == recorded)
        }
        Err(_) => true,
    };
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// `path` spelled one way for one file (review-fy2 m3): absolute, with
/// symlinks and `..` resolved as the OS would (`a/link/..` is the link
/// target's parent). A path that doesn't exist (yet) is resolved up to its
/// longest existing ancestor, and the rest appended with `.` and `..` worked
/// out lexically.
fn canonical_path(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c;
    }
    let mut out = PathBuf::new();
    for part in abs.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                // The `..` of a symlink is its target's parent.
                if let Ok(c) = std::fs::canonicalize(&out) {
                    out = c;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    let mut head = out.as_path();
    let mut tail = Vec::new();
    while let (Some(name), Some(parent)) = (head.file_name(), head.parent()) {
        tail.push(name);
        head = parent;
        if let Ok(mut c) = std::fs::canonicalize(head) {
            c.extend(tail.iter().rev());
            return c;
        }
    }
    out
}

/// A failed comparison waiting for review.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingBaseline {
    /// Stable id: the images' stem relative to the results dir, with `/`
    /// separators (`diffs/home`).
    pub id: String,
    /// The `assertScreenshot` name.
    pub name: String,
    pub expected: PathBuf,
    pub actual: PathBuf,
    pub diff: Option<PathBuf>,
    /// The baseline accepting writes.
    pub baseline: PathBuf,
    pub detail: String,
    /// The baseline's content when the comparison failed (see
    /// [`ReviewRecord::baseline_hash`]); `None` for an older record.
    #[serde(skip)]
    pub baseline_hash: Option<String>,
    record: PathBuf,
}

/// A baseline path a record may name: an absolute `.png` path under a
/// `atalanta-baselines` folder, with no `..`. A results dir is plain files
/// anyone can write, so accepting never writes anywhere else (and
/// [`accept_into`] checks the real, symlink-resolved target too).
fn plausible_baseline(p: &Path) -> bool {
    p.is_absolute()
        && p.extension().is_some_and(|e| e == "png")
        && p.components().all(|c| c != Component::ParentDir)
        && p.parent()
            .is_some_and(|d| d.components().any(|c| c.as_os_str() == BASELINES_DIR))
}

/// Every failed comparison under `results_dir` (searched a few levels deep,
/// so per-device or per-suite subfolders count), sorted by id.
pub fn pending(results_dir: &Path) -> Vec<PendingBaseline> {
    let mut out = Vec::new();
    walk(results_dir, results_dir, 0, &mut out);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PendingBaseline>) {
    const MAX_DEPTH: usize = 6;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` doesn't follow symlinks: a link can't lead the walk
        // out of the results dir.
        let Ok(ty) = entry.file_type() else { continue };
        if ty.is_dir() {
            if depth < MAX_DEPTH && entry.file_name() != BASELINES_DIR {
                walk(root, &path, depth + 1, out);
            }
            continue;
        }
        if !ty.is_file() {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = file.strip_suffix(RECORD_SUFFIX) else {
            continue;
        };
        let Some(record) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<ReviewRecord>(&b).ok())
        else {
            continue;
        };
        if !plausible_baseline(&record.baseline) {
            tracing::warn!(
                record = %path.display(),
                baseline = %record.baseline.display(),
                "ignoring a review record whose baseline isn't under atalanta-baselines/"
            );
            continue;
        }
        let image = |kind: &str| dir.join(format!("{stem}-{kind}.png"));
        let (expected, actual, diff) = (image("expected"), image("actual"), image("diff"));
        if !expected.is_file() || !actual.is_file() {
            continue;
        }
        let rel = dir.strip_prefix(root).unwrap_or(Path::new(""));
        let mut id: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        id.push(stem.to_string());
        out.push(PendingBaseline {
            id: id.join("/"),
            name: record.name,
            expected,
            actual,
            diff: diff.is_file().then_some(diff),
            baseline: record.baseline,
            detail: record.detail,
            baseline_hash: record.baseline_hash,
            record: path,
        });
    }
}

/// The pending comparisons `which` names: `all`, an id, or an
/// `assertScreenshot` name (every device/size that failed under it).
pub fn select<'a>(pending: &'a [PendingBaseline], which: &str) -> Vec<&'a PendingBaseline> {
    pending
        .iter()
        .filter(|p| which == "all" || p.id == which || p.name == which)
        .collect()
}

/// Why [`accept_into`] didn't write a baseline.
#[derive(Debug, Clone, PartialEq)]
pub enum AcceptError {
    /// The baseline changed after the comparison was recorded (a later
    /// run re-recorded it, or it was edited by hand): the recorded frame
    /// is older than it, so it is skipped rather than written back.
    Stale(String),
    /// Anything else: an unreadable image, a target outside the baselines
    /// folder, a write that failed.
    Failed(String),
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Stale(m) | AcceptError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AcceptError {}

/// Accept `p`: its actual frame becomes the baseline, written atomically
/// (a parallel run never reads half a file), and it leaves the review
/// list. The images stay for the run's reports. Returns the baseline path.
pub fn accept(p: &PendingBaseline) -> Result<PathBuf, AcceptError> {
    accept_into(p, None)
}

/// The part of `baseline` after its last `atalanta-baselines` folder
/// (`ios/1179x2556/home.png`).
fn under_baselines_dir(baseline: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = baseline.components().collect();
    let at = parts.iter().rposition(|c| c.as_os_str() == BASELINES_DIR)?;
    Some(parts[at + 1..].iter().collect())
}

/// The `atalanta-baselines` folder `baseline` sits in (its path up to and
/// including the last such component).
fn baselines_root_of(baseline: &Path) -> Option<PathBuf> {
    let parts: Vec<Component> = baseline.components().collect();
    let at = parts.iter().rposition(|c| c.as_os_str() == BASELINES_DIR)?;
    Some(parts[..=at].iter().collect())
}

/// Create `target`'s parent folders and check that they resolve (symlinks
/// followed) inside `root`. A results dir from CI is plain files anyone
/// can write, and so is a checkout: a symlinked `atalanta-baselines/ios`
/// must not turn an accept into a write elsewhere.
fn checked_parent(root: &Path, target: &Path) -> Result<(), String> {
    let refuse = |why: String| Err(format!("refusing to write {}: {why}", target.display()));
    if !target.is_absolute() {
        return refuse("not an absolute path".into());
    }
    let rel = match target.strip_prefix(root) {
        Ok(r) if r.components().all(|c| matches!(c, Component::Normal(_))) => r,
        _ => return refuse(format!("not inside {}", root.display())),
    };
    if rel.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        return refuse(format!("not in a folder under {}", root.display()));
    }
    if std::fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        return refuse(format!("{} is a symlink", root.display()));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("can't create {}: {e}", root.display()))?;
    let real_root = root
        .canonicalize()
        .map_err(|e| format!("can't resolve {}: {e}", root.display()))?;
    // Walk down from the root: every existing folder must be a real one
    // (not a symlink) and resolve under the root; missing ones are made.
    let mut dir = root.to_path_buf();
    for part in rel.parent().into_iter().flat_map(|p| p.components()) {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                return refuse(format!("{} is a symlink", dir.display()))
            }
            Ok(m) if !m.is_dir() => return refuse(format!("{} isn't a folder", dir.display())),
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&dir)
                .or_else(|e| if dir.is_dir() { Ok(()) } else { Err(e) })
                .map_err(|e| format!("can't create {}: {e}", dir.display()))?,
        }
    }
    let real = dir
        .canonicalize()
        .map_err(|e| format!("can't resolve {}: {e}", dir.display()))?;
    if !real.starts_with(&real_root) {
        return refuse(format!(
            "{} resolves to {}, outside {}",
            dir.display(),
            real.display(),
            real_root.display()
        ));
    }
    Ok(())
}

/// Whether the baseline at `target` changed after `p` was recorded, so the
/// recorded frame is out of date. By content: the record's
/// [`ReviewRecord::baseline_hash`] against the target's now (with
/// `--baselines-dir`, the file in that folder), which holds across
/// machines and checkouts. A record without a hash (older Atalanta) falls
/// back to "the baseline's mtime is newer than the record's", with a
/// warning: that is only meaningful on the machine that ran the test.
fn changed_since_record(p: &PendingBaseline, target: &Path) -> bool {
    if let Some(recorded) = &p.baseline_hash {
        return content_hash(target) != *recorded;
    }
    tracing::warn!(
        record = %p.record.display(),
        "review record has no baseline hash (written by an older Atalanta): judging staleness \
         by modification times, which a checkout or unzip changes; re-run to record a hash"
    );
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    match (modified(target), modified(&p.record)) {
        (Some(baseline), Some(record)) => baseline > record,
        _ => false,
    }
}

/// [`accept`], but into the `atalanta-baselines` folder `root` when given,
/// at the same `<platform>/<W>x<H>/<file>` the run used. For artifacts
/// from another machine (a CI runner), whose absolute paths don't exist
/// here. Returns the path written.
///
/// The target must resolve inside `root` (or, without one, inside the
/// `atalanta-baselines` folder the record names) with no symlinked
/// folder on the way. A baseline whose content changed after the
/// comparison was recorded is skipped ([`AcceptError::Stale`]) and its
/// record kept.
pub fn accept_into(p: &PendingBaseline, root: Option<&Path>) -> Result<PathBuf, AcceptError> {
    let (root, target) = accept_target(p, root)?;
    if let Some(e) = stale(p, &target) {
        return Err(e);
    }
    write_accepted(p, &root, &target)
}

/// [`accept_into`] for several comparisons at once (`accept all`, or a
/// name that failed on several devices or shards). Whether a baseline
/// changed since a record is judged before anything is written, so two
/// records of one baseline behave as before (the later one wins) instead
/// of the first accept making the second look stale.
pub fn accept_batch<'a>(
    ps: &[&'a PendingBaseline],
    root: Option<&Path>,
) -> Vec<(&'a PendingBaseline, Result<PathBuf, AcceptError>)> {
    let checked: Vec<_> = ps
        .iter()
        .map(|p| {
            let t = accept_target(p, root);
            let s = t.as_ref().ok().and_then(|(_, target)| stale(p, target));
            (*p, t, s)
        })
        .collect();
    checked
        .into_iter()
        .map(|(p, t, s)| {
            let r = match (t, s) {
                (Err(e), _) => Err(e),
                (_, Some(e)) => Err(e),
                (Ok((root, target)), None) => write_accepted(p, &root, &target),
            };
            (p, r)
        })
        .collect()
}

fn stale(p: &PendingBaseline, target: &Path) -> Option<AcceptError> {
    changed_since_record(p, target).then(|| {
        AcceptError::Stale(format!(
            "skipped: {} changed after this comparison was recorded (a later run passed \
             on it or re-recorded it), so the recorded frame is out of date; re-run to \
             compare again",
            target.display()
        ))
    })
}

/// The baselines folder an accept of `p` must stay in, and the file it writes.
fn accept_target(
    p: &PendingBaseline,
    root: Option<&Path>,
) -> Result<(PathBuf, PathBuf), AcceptError> {
    let failed = AcceptError::Failed;
    let (root, target) = match root {
        None => {
            let root = baselines_root_of(&p.baseline).ok_or_else(|| {
                failed(format!(
                    "{} isn't under atalanta-baselines/",
                    p.baseline.display()
                ))
            })?;
            (root, p.baseline.clone())
        }
        Some(root) => {
            let root = std::path::absolute(root)
                .map_err(|e| failed(format!("can't resolve {}: {e}", root.display())))?;
            let under = under_baselines_dir(&p.baseline).ok_or_else(|| {
                failed(format!(
                    "{} isn't under atalanta-baselines/",
                    p.baseline.display()
                ))
            })?;
            let target = root.join(under);
            (root, target)
        }
    };
    Ok((root, target))
}

fn write_accepted(p: &PendingBaseline, root: &Path, target: &Path) -> Result<PathBuf, AcceptError> {
    let failed = AcceptError::Failed;
    let png = std::fs::read(&p.actual)
        .map_err(|e| failed(format!("can't read {}: {e}", p.actual.display())))?;
    if crate::visual::png_size(&png).is_none() {
        return Err(failed(format!("{} isn't a PNG image", p.actual.display())));
    }
    checked_parent(root, target).map_err(failed)?;
    crate::visual::write_atomic(target, &png)
        .map_err(|e| failed(format!("can't write {}: {e}", target.display())))?;
    let _ = std::fs::remove_file(&p.record);
    Ok(target.to_path_buf())
}

/// Reject `p`: the baseline stays as it is and `p` leaves the review list.
pub fn reject(p: &PendingBaseline) -> Result<(), String> {
    std::fs::remove_file(&p.record).map_err(|e| format!("can't remove {}: {e}", p.record.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, shade: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&vec![shade; (w * h * 4) as usize])
            .unwrap();
        wr.finish().unwrap();
        out
    }

    #[test]
    fn baseline_path_is_the_documented_layout() {
        assert_eq!(
            baseline_path(Path::new("suite"), "ios", (1179, 2556), "home"),
            Path::new("suite/atalanta-baselines/ios/1179x2556/home.png")
        );
        assert_eq!(baseline_space("android", true), "android-headless");
        assert_eq!(baseline_file("cart total"), baseline_file("cart total"));
        assert_ne!(baseline_file("cart total"), baseline_file("cart/total"));
    }

    #[test]
    fn a_recorded_failure_is_listed_accepted_and_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let results = tmp.path().join("results");
        let diffs = results.join("diffs");
        let baseline = tmp
            .path()
            .join("suite/atalanta-baselines/ios/10x10/home.png");
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, png(10, 10, 0)).unwrap();
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("home-expected.png"), png(10, 10, 0)).unwrap();
        std::fs::write(diffs.join("home-actual.png"), png(10, 10, 200)).unwrap();
        std::fs::write(diffs.join("home-diff.png"), png(10, 10, 9)).unwrap();
        // Images without a record (an older run) aren't listed.
        std::fs::write(diffs.join("old-expected.png"), png(1, 1, 0)).unwrap();
        std::fs::write(diffs.join("old-actual.png"), png(1, 1, 0)).unwrap();
        record_failure(
            &diffs,
            "home.png",
            &ReviewRecord {
                name: "home".into(),
                baseline: baseline.clone(),
                detail: "90% similar".into(),
                baseline_hash: None,
            },
        )
        .unwrap();

        let list = pending(&results);
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].id, "diffs/home");
        assert_eq!(list[0].name, "home");
        assert_eq!(list[0].detail, "90% similar");
        assert!(list[0].diff.is_some());
        assert_eq!(select(&list, "home").len(), 1);
        assert_eq!(select(&list, "all").len(), 1);
        assert!(select(&list, "nope").is_empty());

        let written = accept(&list[0]).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), png(10, 10, 200));
        assert!(pending(&results).is_empty());
        // The images stay for the report.
        assert!(diffs.join("home-actual.png").is_file());
    }

    #[test]
    fn a_record_naming_a_file_outside_atalanta_baselines_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let diffs = tmp.path().join("diffs");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("x-expected.png"), png(1, 1, 0)).unwrap();
        std::fs::write(diffs.join("x-actual.png"), png(1, 1, 0)).unwrap();
        for target in [
            tmp.path().join("victim.png"),
            tmp.path().join("atalanta-baselines/../victim.png"),
            tmp.path().join("atalanta-baselines/ios/1x1/x.sh"),
        ] {
            record_failure(
                &diffs,
                "x.png",
                &ReviewRecord {
                    name: "x".into(),
                    baseline: target.clone(),
                    detail: String::new(),
                    baseline_hash: None,
                },
            )
            .unwrap();
            assert!(pending(tmp.path()).is_empty(), "{}", target.display());
        }
    }

    #[test]
    fn accept_into_another_checkout_keeps_the_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let diffs = tmp.path().join("r/diffs");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("a-expected.png"), png(4, 4, 0)).unwrap();
        std::fs::write(diffs.join("a-actual.png"), png(4, 4, 1)).unwrap();
        // As a CI runner recorded it: a path that doesn't exist here.
        let rec = ReviewRecord {
            name: "a".into(),
            baseline: "/home/runner/work/app/suite/atalanta-baselines/ios/4x4/a.png".into(),
            detail: String::new(),
            baseline_hash: None,
        };
        record_failure(&diffs, "a.png", &rec).unwrap();
        let list = pending(&tmp.path().join("r"));
        let root = tmp.path().join("checkout/atalanta-baselines");
        let written = accept_into(&list[0], Some(&root)).unwrap();
        assert_eq!(written, root.join("ios/4x4/a.png"));
        assert_eq!(std::fs::read(&written).unwrap(), png(4, 4, 1));
    }

    #[test]
    fn reject_keeps_the_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let diffs = tmp.path().join("r/diffs");
        let baseline = tmp.path().join("atalanta-baselines/web/4x4/a.png");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("a-expected.png"), png(4, 4, 0)).unwrap();
        std::fs::write(diffs.join("a-actual.png"), png(4, 4, 1)).unwrap();
        let rec = ReviewRecord {
            name: "a".into(),
            baseline: baseline.clone(),
            detail: String::new(),
            baseline_hash: None,
        };
        record_failure(&diffs, "a.png", &rec).unwrap();
        let list = pending(&tmp.path().join("r"));
        reject(&list[0]).unwrap();
        assert!(!baseline.exists());
        assert!(pending(&tmp.path().join("r")).is_empty());
    }

    /// One recorded failure of `a.png` under `results/diffs`, naming
    /// `baseline`; returns its pending entry.
    fn recorded(results: &Path, baseline: &Path) -> PendingBaseline {
        let diffs = results.join("diffs");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("a-expected.png"), png(4, 4, 0)).unwrap();
        std::fs::write(diffs.join("a-actual.png"), png(4, 4, 1)).unwrap();
        let rec = ReviewRecord {
            name: "a".into(),
            baseline: baseline.to_path_buf(),
            detail: String::new(),
            baseline_hash: None,
        };
        record_failure(&diffs, "a.png", &rec).unwrap();
        let list = pending(results);
        assert_eq!(list.len(), 1, "{list:?}");
        list[0].clone()
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn a_baseline_newer_than_its_record_is_skipped() {
        // fi56 MAJOR 1: accepting an old record wrote its frame over a
        // baseline re-recorded since.
        let tmp = tempfile::tempdir().unwrap();
        let baseline = tmp.path().join("suite/atalanta-baselines/ios/4x4/a.png");
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, png(4, 4, 0)).unwrap();
        let p = recorded(&tmp.path().join("r"), &baseline);
        let now = std::time::SystemTime::now();
        set_mtime(&p.record, now - std::time::Duration::from_secs(60));
        // Re-recorded after the failure.
        std::fs::write(&baseline, png(4, 4, 7)).unwrap();
        set_mtime(&baseline, now);
        let e = accept(&p).unwrap_err();
        assert!(matches!(e, AcceptError::Stale(_)), "{e}");
        assert!(e.to_string().contains("changed after"), "{e}");
        assert_eq!(std::fs::read(&baseline).unwrap(), png(4, 4, 7));
        assert!(p.record.is_file(), "the record is kept");
        // Into another checkout, the target there is what's compared.
        let root = tmp.path().join("checkout/atalanta-baselines");
        let there = root.join("ios/4x4/a.png");
        std::fs::create_dir_all(there.parent().unwrap()).unwrap();
        std::fs::write(&there, png(4, 4, 9)).unwrap();
        set_mtime(&there, now);
        assert!(matches!(
            accept_into(&p, Some(&root)),
            Err(AcceptError::Stale(_))
        ));
        // The baseline as it was when the comparison failed is replaced,
        // whatever its mtime.
        std::fs::write(&baseline, png(4, 4, 0)).unwrap();
        set_mtime(&baseline, now);
        assert_eq!(accept(&p).unwrap(), baseline);
        assert_eq!(std::fs::read(&baseline).unwrap(), png(4, 4, 1));
    }

    /// review-fi56 MAJOR-2: CI artifacts. `unzip` keeps the records' CI-run
    /// mtimes and a fresh clone makes every baseline newer, yet nothing
    /// changed: accepted. A baseline changed upstream but written before
    /// the artifact was extracted (older than the record) is stale.
    #[test]
    fn staleness_is_by_content_not_mtime_across_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let ci = Path::new("/home/runner/work/app/atalanta-baselines/ios/4x4/a.png");
        // What CI compared against, hashed on CI.
        let p = {
            let diffs = tmp.path().join("artifact/diffs");
            std::fs::create_dir_all(&diffs).unwrap();
            std::fs::write(diffs.join("a-expected.png"), png(4, 4, 0)).unwrap();
            std::fs::write(diffs.join("a-actual.png"), png(4, 4, 1)).unwrap();
            let ci_file = tmp.path().join("ci.png");
            std::fs::write(&ci_file, png(4, 4, 0)).unwrap();
            let rec = ReviewRecord {
                name: "a".into(),
                baseline: ci.to_path_buf(),
                detail: String::new(),
                baseline_hash: Some(content_hash(&ci_file)),
            };
            record_failure(&diffs, "a.png", &rec).unwrap();
            pending(&tmp.path().join("artifact")).remove(0)
        };
        let now = std::time::SystemTime::now();
        let day = std::time::Duration::from_secs(86_400);
        set_mtime(&p.record, now - day); // unzip restored CI's time
        let root = tmp.path().join("checkout/atalanta-baselines");
        let here = root.join("ios/4x4/a.png");
        std::fs::create_dir_all(here.parent().unwrap()).unwrap();
        std::fs::write(&here, png(4, 4, 0)).unwrap();
        set_mtime(&here, now); // a fresh clone
        let saved = std::fs::read(&p.record).unwrap();
        let out = accept_batch(&[&p], Some(&root));
        assert!(out[0].1.is_ok(), "{:?}", out[0].1);
        assert_eq!(std::fs::read(&here).unwrap(), png(4, 4, 1));

        // Upstream changed it; the checkout is older than the record.
        std::fs::write(&p.record, &saved).unwrap();
        std::fs::write(&here, png(4, 4, 5)).unwrap();
        set_mtime(&here, now - 2 * day);
        set_mtime(&p.record, now);
        let e = accept_into(&p, Some(&root)).unwrap_err();
        assert!(matches!(e, AcceptError::Stale(_)), "{e}");
        assert_eq!(std::fs::read(&here).unwrap(), png(4, 4, 5));

        // No baseline at the time ("absent"), none now: accepted.
        let fresh = root.join("ios/4x4/b.png");
        let rec = PendingBaseline {
            baseline: ci.with_file_name("b.png"),
            baseline_hash: Some(ABSENT.into()),
            ..p.clone()
        };
        assert_eq!(accept_into(&rec, Some(&root)).unwrap(), fresh);
        // ...but one that appeared since is stale.
        std::fs::write(&p.record, &saved).unwrap();
        assert!(matches!(
            accept_into(&rec, Some(&root)),
            Err(AcceptError::Stale(_))
        ));
    }

    /// A record from before the hash (no `baseline_hash`) keeps the mtime rule.
    #[test]
    fn a_legacy_record_without_a_hash_falls_back_to_mtimes() {
        let tmp = tempfile::tempdir().unwrap();
        let baseline = tmp.path().join("suite/atalanta-baselines/ios/4x4/a.png");
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, png(4, 4, 0)).unwrap();
        let diffs = tmp.path().join("r/diffs");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("a-expected.png"), png(4, 4, 0)).unwrap();
        std::fs::write(diffs.join("a-actual.png"), png(4, 4, 1)).unwrap();
        let json = serde_json::json!({"name": "a", "baseline": baseline});
        std::fs::write(diffs.join("a-review.json"), json.to_string()).unwrap();
        let p = pending(&tmp.path().join("r")).remove(0);
        assert_eq!(p.baseline_hash, None);
        let now = std::time::SystemTime::now();
        set_mtime(&p.record, now - std::time::Duration::from_secs(60));
        set_mtime(&baseline, now);
        assert!(matches!(accept(&p), Err(AcceptError::Stale(_))));
        set_mtime(&baseline, now - std::time::Duration::from_secs(120));
        assert_eq!(accept(&p).unwrap(), baseline);
    }

    /// review-fi56 m1: clearing is by baseline path, not file name.
    #[test]
    fn clear_record_keeps_another_size_folders_record() {
        let tmp = tempfile::tempdir().unwrap();
        let small = tmp
            .path()
            .join("suite/atalanta-baselines/android/720x1280/home.png");
        let big = tmp
            .path()
            .join("suite/atalanta-baselines/android/1080x2400/home.png");
        let p = recorded(&tmp.path().join("r"), &small);
        let diffs = tmp.path().join("r/diffs");
        clear_record(&diffs, "home.png", &[&big]);
        clear_record(&diffs, "a.png", &[&big]);
        assert!(p.record.is_file(), "another size's record is kept");
        clear_record(&diffs, "a.png", &[&big, &small]);
        assert!(!p.record.exists());
    }

    /// review-fy2 m3: two spellings of one suite path (`app/../flows`, a
    /// symlinked folder) are one baseline, whether the file exists or not.
    #[test]
    fn clear_record_matches_another_spelling_of_the_same_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("app")).unwrap();
        let rel = "atalanta-baselines/ios/1x1/a.png";
        let plain = tmp.path().join("flows").join(rel);
        let dotted = tmp.path().join("app/../flows").join(rel);
        for exists in [false, true] {
            if exists {
                std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
                std::fs::write(&plain, png(1, 1, 0)).unwrap();
            }
            for (recorded_as, cleared_as) in [(&dotted, &plain), (&plain, &dotted)] {
                let diffs = tmp.path().join("r/diffs");
                let rec = ReviewRecord {
                    name: "a".into(),
                    baseline: recorded_as.clone(),
                    detail: String::new(),
                    baseline_hash: None,
                };
                record_failure(&diffs, "a.png", &rec).unwrap();
                let record = record_path(&diffs, "a.png");
                assert!(record.is_file());
                clear_record(&diffs, "a.png", &[cleared_as.as_path()]);
                assert!(
                    !record.exists(),
                    "recorded as {recorded_as:?}, cleared as {cleared_as:?} (exists: {exists})"
                );
            }
        }
        // A symlinked spelling of the suite folder.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.path().join("flows"), tmp.path().join("link")).unwrap();
            let linked = tmp.path().join("link").join(rel);
            let diffs = tmp.path().join("r/diffs");
            let rec = ReviewRecord {
                name: "a".into(),
                baseline: linked,
                detail: String::new(),
                baseline_hash: None,
            };
            record_failure(&diffs, "a.png", &rec).unwrap();
            clear_record(&diffs, "a.png", &[plain.as_path()]);
            assert!(
                !record_path(&diffs, "a.png").exists(),
                "a symlinked spelling"
            );
        }
        // Another baseline is still another one.
        let diffs = tmp.path().join("r/diffs");
        let rec = ReviewRecord {
            name: "a".into(),
            baseline: plain.clone(),
            detail: String::new(),
            baseline_hash: None,
        };
        record_failure(&diffs, "a.png", &rec).unwrap();
        let other = tmp.path().join("app/../other").join(rel);
        clear_record(&diffs, "a.png", &[other.as_path()]);
        assert!(record_path(&diffs, "a.png").is_file());
    }

    #[test]
    fn a_relative_baseline_path_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let diffs = tmp.path().join("diffs");
        std::fs::create_dir_all(&diffs).unwrap();
        std::fs::write(diffs.join("x-expected.png"), png(1, 1, 0)).unwrap();
        std::fs::write(diffs.join("x-actual.png"), png(1, 1, 0)).unwrap();
        // record_failure makes paths absolute; a crafted record needn't.
        let json = br#"{"name":"x","baseline":"atalanta-baselines/ios/1x1/x.png"}"#;
        std::fs::write(diffs.join("x-review.json"), json).unwrap();
        assert!(pending(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_under_the_baselines_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("Pictures");
        std::fs::create_dir_all(elsewhere.join("4x4")).unwrap();
        let root = tmp.path().join("checkout/atalanta-baselines");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("ios")).unwrap();
        let p = recorded(
            &tmp.path().join("r"),
            Path::new("/home/runner/work/app/atalanta-baselines/ios/4x4/a.png"),
        );
        let e = accept_into(&p, Some(&root)).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert!(!elsewhere.join("4x4/a.png").exists());
        // The recorded root itself being a link is refused too.
        let suite = tmp.path().join("suite");
        std::fs::create_dir_all(&suite).unwrap();
        std::os::unix::fs::symlink(&elsewhere, suite.join(BASELINES_DIR)).unwrap();
        let p = recorded(
            &tmp.path().join("r2"),
            &suite.join("atalanta-baselines/4x4/a.png"),
        );
        let e = accept(&p).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert!(!elsewhere.join("4x4/a.png").exists());
    }

    #[test]
    fn a_batch_judges_staleness_before_writing() {
        // Two shards failed on one baseline: accepting the first must not
        // make the second look newer-than-its-record.
        let tmp = tempfile::tempdir().unwrap();
        let baseline = tmp.path().join("suite/atalanta-baselines/ios/4x4/a.png");
        std::fs::create_dir_all(baseline.parent().unwrap()).unwrap();
        std::fs::write(&baseline, png(4, 4, 0)).unwrap();
        set_mtime(
            &baseline,
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        let a = recorded(&tmp.path().join("r1"), &baseline);
        let b = recorded(&tmp.path().join("r2"), &baseline);
        let out = accept_batch(&[&a, &b], None);
        assert!(out.iter().all(|(_, r)| r.is_ok()), "{out:?}");
    }
}
