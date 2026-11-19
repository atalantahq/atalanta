use std::path::PathBuf;

use velocity_common::{Result, VelocityError};

/// Save a PNG screenshot to disk with a sanitized filename.
///
/// The file is written to `{dir}/{sanitized_test_name}_step_{step_index}.png`.
/// Directories are created as needed.
pub fn save_screenshot(
    png_bytes: &[u8],
    dir: &str,
    test_name: &str,
    step_index: usize,
) -> Result<PathBuf> {
    let dir_path = PathBuf::from(dir);
    std::fs::create_dir_all(&dir_path).map_err(|e| {
        VelocityError::io(
            format!(
                "failed to create screenshot directory {}",
                dir_path.display()
            ),
            e,
        )
    })?;

    let sanitized_name = unique_filename(test_name);
    let filename = format!("{sanitized_name}_step_{step_index}.png");
    let file_path = dir_path.join(&filename);

    std::fs::write(&file_path, png_bytes).map_err(|e| {
        VelocityError::io(
            format!("failed to write screenshot {}", file_path.display()),
            e,
        )
    })?;

    Ok(file_path)
}

/// A warning when the disk holding `dir` (or its nearest existing parent)
/// has less free space than `devices up` requires (3 GB,
/// `VELOCITY_MIN_FREE_DISK_GB`): a long run with video fills it, and the
/// first thing to fail is the report write, after every test ran.
pub fn low_disk_warning(dir: &std::path::Path) -> Option<String> {
    let mut at = dir;
    while !at.exists() {
        at = at.parent()?;
    }
    let free = velocity_common::host::free_disk(at)?;
    let (_, _, min_disk) = velocity_common::host::limits();
    (free < min_disk).then(|| {
        let gb = velocity_common::host::GB as f64;
        format!(
            "only {:.1} GB free where the artifacts go ({}), under the {:.1} GB bar: evidence \
             (video, traces) may fill it before the reports are written. Free some space, or cap \
             old evidence with VELOCITY_ARTIFACTS_MAX_BYTES",
            free as f64 / gb,
            at.display(),
            min_disk as f64 / gb
        )
    })
}

/// `VELOCITY_ARTIFACTS_MAX_BYTES`: the most evidence the artifacts dir may
/// keep (`500M`, `2G`, or bytes). `None`: no cap.
pub fn max_evidence_bytes() -> Option<u64> {
    velocity_common::env::var("VELOCITY_ARTIFACTS_MAX_BYTES")
        .ok()
        .and_then(|v| parse_bytes(&v))
}

fn parse_bytes(text: &str) -> Option<u64> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().trim_end_matches('B') {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// What [`prune_evidence`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub removed: usize,
    pub freed: u64,
}

/// Keep the evidence under `dir` (each test's `traces/<test>/` folder and
/// each file in `screenshots/`) within `max_bytes` by removing the oldest
/// first. Nothing written since `keep_since` (this run's evidence, which
/// its reports link to) is removed, even when that alone is over the cap.
pub fn prune_evidence(
    dir: &std::path::Path,
    max_bytes: u64,
    keep_since: std::time::SystemTime,
) -> Pruned {
    let mut items: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for sub in ["traces", "screenshots"] {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let (size, newest) = tree_size(&path, &meta);
            items.push((newest, size, path));
        }
    }
    let mut total: u64 = items.iter().map(|(_, s, _)| s).sum();
    items.sort_by_key(|a| a.0);
    let mut out = Pruned::default();
    for (modified, size, path) in items {
        if total <= max_bytes || modified >= keep_since {
            break;
        }
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if gone.is_ok() {
            total = total.saturating_sub(size);
            out.removed += 1;
            out.freed += size;
        }
    }
    out
}

/// Bytes under `path` and the newest modification time of the files in it
/// (a folder's own time changes whenever an entry is added or removed, so
/// it counts only for an empty folder). Symlinks are counted as
/// themselves, never followed.
fn tree_size(path: &std::path::Path, meta: &std::fs::Metadata) -> (u64, std::time::SystemTime) {
    let own = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
    if !meta.is_dir() {
        return (meta.len(), own);
    }
    let mut newest = std::time::UNIX_EPOCH;
    let mut size = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                let (s, n) = tree_size(&e.path(), &m);
                size += s;
                newest = newest.max(n);
            }
        }
    }
    if newest == std::time::UNIX_EPOCH {
        newest = own;
    }
    (size, newest)
}

/// [`sanitize_filename`], plus a short hash of the full name when
/// characters other than spaces were replaced: "Löschen" and "L?schen", or
/// "a.b" and "a:b", wrote over each other's screenshots (VEL-S11). Plain
/// names keep their old file names.
fn unique_filename(name: &str) -> String {
    let clean = sanitize_filename(name);
    let lossy = name
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')));
    if !lossy {
        return clean;
    }
    let hash = name.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    format!("{clean}-{hash:08x}")
}

/// Replace characters that are problematic in filenames with underscores.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            ' ' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {

    #[test]
    fn evidence_sizes_take_units() {
        assert_eq!(parse_bytes("500M"), Some(500 << 20));
        assert_eq!(parse_bytes("2G"), Some(2 << 30));
        assert_eq!(parse_bytes("2gb"), Some(2 << 30));
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("lots"), None);
    }

    /// Review 12 #10: old evidence goes first, this run's never.
    #[test]
    fn pruning_removes_the_oldest_evidence_and_keeps_this_runs() {
        let dir = std::env::temp_dir().join(format!("vel-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |rel: &str, bytes: usize| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![0u8; bytes]).unwrap();
        };
        let age = |rel: &str, secs_ago: u64| {
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
            let f = std::fs::File::options()
                .write(true)
                .open(dir.join(rel))
                .unwrap();
            f.set_modified(t).unwrap();
        };
        write("traces/old/video.mp4", 1000);
        age("traces/old/video.mp4", 3000);
        write("screenshots/older.png", 500);
        age("screenshots/older.png", 2000);
        write("traces/mid/steps.json", 1000);
        age("traces/mid/steps.json", 1000);
        let run_start = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
        write("traces/now/video.mp4", 4000);
        let pruned = prune_evidence(&dir, 4500, run_start);
        assert_eq!(
            pruned,
            Pruned {
                removed: 3,
                freed: 2500
            }
        );
        assert!(
            dir.join("traces/now/video.mp4").exists(),
            "this run's stays"
        );
        assert!(!dir.join("traces/old").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    // VEL-S11: names that differ only in replaced characters collided.
    #[test]
    fn names_differing_in_replaced_characters_get_their_own_files() {
        let dir = std::env::temp_dir().join(format!("vel-shots-s11-{}", std::process::id()));
        let d = dir.to_str().unwrap();
        let a = save_screenshot(b"a", d, "Löschen", 1).unwrap();
        let b = save_screenshot(b"b", d, "L?schen", 1).unwrap();
        let c = save_screenshot(b"c", d, "a:b", 1).unwrap();
        let e = save_screenshot(b"e", d, "a/b", 1).unwrap();
        assert_ne!(a, b);
        assert_ne!(c, e);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sanitize_spaces() {
        assert_eq!(sanitize_filename("Login Flow Test"), "Login_Flow_Test");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize_filename("test/case:1<2>3"), "test_case_1_2_3");
    }

    #[test]
    fn test_sanitize_preserves_valid_chars() {
        assert_eq!(
            sanitize_filename("valid-name_123.test"),
            "valid-name_123.test"
        );
    }

    #[test]
    fn test_save_screenshot_roundtrip() {
        let dir = std::env::temp_dir().join("velocity_screenshot_test");
        let _ = std::fs::remove_dir_all(&dir);

        let fake_png = b"\x89PNG\r\n\x1a\nfakedata";
        let path = save_screenshot(fake_png, dir.to_str().unwrap(), "My Test Case", 3).unwrap();

        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "My_Test_Case_step_3.png");
        assert_eq!(std::fs::read(&path).unwrap(), fake_png);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
