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
        VelocityError::Config(format!(
            "failed to create screenshot directory {}: {e}",
            dir_path.display()
        ))
    })?;

    let sanitized_name = unique_filename(test_name);
    let filename = format!("{sanitized_name}_step_{step_index}.png");
    let file_path = dir_path.join(&filename);

    std::fs::write(&file_path, png_bytes).map_err(|e| {
        VelocityError::Config(format!(
            "failed to write screenshot {}: {e}",
            file_path.display()
        ))
    })?;

    Ok(file_path)
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
