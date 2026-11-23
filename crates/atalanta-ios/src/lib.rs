pub mod asc;
pub mod devicectl;
pub mod driver;
pub mod evidence;
pub mod hit;
pub mod install;
pub mod parser;
pub mod provision;
pub mod runner_src;
pub mod simctl;
pub mod usbmux;
pub mod wda;
pub mod wda_bootstrap;
pub mod wda_manager;

pub use driver::IosDriver;
pub use wda_bootstrap::WdaBootstrap;

/// Write an executable `#!/bin/sh` stub at `path` running `body`, and wait
/// until it can be run: a process another test forks while the file is
/// open for writing keeps it busy (ETXTBSY) until that child execs.
#[cfg(test)]
pub(crate) fn write_stub(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(
        path,
        format!("#!/bin/sh\n[ \"$1\" = --velocity-noop ] && exit 0\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..100 {
        match std::process::Command::new(path)
            .arg("--velocity-noop")
            .status()
        {
            Ok(_) => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    panic!("stub {} never became runnable", path.display());
}
