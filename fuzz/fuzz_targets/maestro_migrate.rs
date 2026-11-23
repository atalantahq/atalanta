// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The Maestro converter (`atalanta migrate maestro`): reads a flow file
//! someone else wrote, converts it, and parses the result as a suite.
//! It works on files, so each input is written to a private temp folder.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use atalanta_migrate::MaestroMigrator;

fn dir() -> &'static tempfile::TempDir {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("temp dir"))
}

fuzz_target!(|data: &[u8]| {
    let dir = dir().path();
    let input = dir.join("flow.yaml");
    let output = dir.join("out.yaml");
    if std::fs::write(&input, data).is_err() {
        return;
    }
    let _ = MaestroMigrator::new().migrate_file(
        input.to_str().expect("utf-8 temp path"),
        output.to_str().expect("utf-8 temp path"),
    );
});
