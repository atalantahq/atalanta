//! Paths an agent passes to a tool, kept inside the places a tool may use.
//!
//! A model chooses these arguments, and a model reads untrusted screen text,
//! so `run_task`'s `out` (a file the explore child writes) and `script` (a
//! file it reads) must not reach `~/.ssh/authorized_keys`, a shell profile
//! or anything else outside the project: an absolute path, `..` and a
//! symlink out are all refused. The same rule as `describe_failure`'s
//! read-only artifacts check (VEL-S16): a working directory of `/` or
//! `$HOME` allows nothing.

use std::path::{Component, Path, PathBuf};

/// The canonical path for `asked` (relative to `cwd`) when it lies under one
/// of `roots`, after symlinks are resolved. A file that must exist is
/// canonicalized whole; one that will be written needs an existing parent
/// under a root, and may not itself be a symlink.
pub fn confined(
    what: &str,
    asked: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
    roots: &[&Path],
    must_exist: bool,
) -> Result<PathBuf, String> {
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let home = home.and_then(canon);
    let broad = |r: &PathBuf| r.parent().is_none() || Some(r) == home.as_ref();
    let allowed: Vec<PathBuf> = cwd
        .into_iter()
        .chain(roots.iter().copied())
        .filter_map(canon)
        .filter(|r| !broad(r))
        .collect();
    let refuse = || {
        format!(
            "{what} '{asked}' is outside the working directory and the task folder (a working \
             directory of / or $HOME doesn't count); give a path inside the project"
        )
    };
    let path = Path::new(asked);
    let full = match cwd {
        Some(c) if path.is_relative() => c.join(path),
        _ => path.to_path_buf(),
    };
    let resolved = if must_exist {
        canon(&full).ok_or_else(|| format!("{what} '{asked}': no such file"))?
    } else {
        let name = match full.components().next_back() {
            Some(Component::Normal(n)) => n.to_os_string(),
            _ => return Err(refuse()),
        };
        if std::fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{what} '{asked}' is a symlink; give the real file"));
        }
        let parent = full
            .parent()
            .and_then(canon)
            .ok_or_else(|| format!("{what} '{asked}': its folder doesn't exist"))?;
        parent.join(name)
    };
    if allowed.iter().any(|r| resolved.starts_with(r)) {
        Ok(resolved)
    } else {
        Err(refuse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_inside_the_project_or_task_folder_pass_and_others_are_refused() {
        let project = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = project.path();
        std::fs::create_dir(cwd.join("tests")).unwrap();
        std::fs::write(cwd.join("plan.json"), "{}").unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        let roots = [task.path()];
        let ok = |asked: &str, exist: bool| confined("out", asked, Some(cwd), None, &roots, exist);

        let w = ok("tests/login.yaml", false).unwrap();
        assert!(w.ends_with("tests/login.yaml"), "{w:?}");
        assert!(ok("plan.json", true).is_ok());
        let in_task = task.path().join("explore.yaml");
        assert!(ok(in_task.to_str().unwrap(), false).is_ok());

        // Absolute paths out, `..`, a missing folder, `.` itself.
        let abs = outside.path().join("x.yaml");
        assert!(ok(abs.to_str().unwrap(), false).is_err());
        assert!(ok("../escape.yaml", false).is_err());
        assert!(ok("tests/../../escape.yaml", false).is_err());
        assert!(ok("nope/x.yaml", false)
            .unwrap_err()
            .contains("doesn't exist"));
        assert!(ok("tests/..", false).is_err());
        let secret = outside.path().join("secret");
        assert!(ok(secret.to_str().unwrap(), true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_refused() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = project.path();
        let target = outside.path().join("authorized_keys");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, cwd.join("out.yaml")).unwrap();
        std::os::unix::fs::symlink(outside.path(), cwd.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(&target, cwd.join("plan.json")).unwrap();
        let ok = |asked: &str, exist: bool| confined("out", asked, Some(cwd), None, &[], exist);
        assert!(ok("out.yaml", false).unwrap_err().contains("symlink"));
        assert!(ok("linkdir/x.yaml", false).is_err());
        assert!(ok("plan.json", true).is_err());
    }

    #[test]
    fn a_working_directory_of_root_or_home_allows_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("x.json"), "{}").unwrap();
        let e = confined(
            "script",
            "x.json",
            Some(home.path()),
            Some(home.path()),
            &[],
            true,
        );
        assert!(e.is_err());
        assert!(confined("out", "etc/x", Some(Path::new("/")), None, &[], false).is_err());
    }
}
