// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta inspect` on a port in use: a config error (exit 2) before any
//! device work, and never a "running at" line (VEL-171, VEL-173).
#![cfg(unix)]

use std::process::Command;

#[test]
fn a_port_in_use_fails_before_running_is_printed() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    // Device tools that fail if anything reaches them.
    let bin = std::env::temp_dir().join(format!("vinspect-{}", std::process::id()));
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["adb", "xcrun"] {
        let p = bin.join(tool);
        let log = bin.join("calls.log");
        std::fs::write(
            &p,
            format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n", log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_atalanta"))
        .args([
            "inspect",
            "--platform",
            "android",
            "--device",
            "emulator-5554",
        ])
        .args(["--port", &port.to_string()])
        .env("PATH", path)
        .env("ATALANTA_ADB_PATH", bin.join("adb"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(!stderr.contains("running at") && !stdout.contains("running at"));
    assert!(stderr.contains(&port.to_string()), "{stderr}");
    let calls = std::fs::read_to_string(bin.join("calls.log")).unwrap_or_default();
    assert!(
        calls.is_empty(),
        "device tools ran before the bind: {calls}"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&bin);
}
