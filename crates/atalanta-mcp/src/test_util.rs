//! Test helpers.

use std::path::PathBuf;

/// A shell script with `content`, written once per content under the temp
/// dir and reused: a security scanner on this kind of machine can hold a
/// freshly written executable for many seconds before it may run, so tests
/// that wrote one per run flaked. Run it through `/bin/sh`.
pub fn script(name: &str, content: &str) -> PathBuf {
    let hash = content.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let dir = std::env::temp_dir().join("atalanta-mcp-test-scripts");
    let path = dir.join(format!("{name}-{hash:016x}"));
    if !path.is_file() {
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(format!(".{name}-{hash:016x}-{}", std::process::id()));
        std::fs::write(&tmp, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = std::fs::rename(&tmp, &path);
    }
    path
}
