// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The suite YAML parser and validator: what `atalanta run` / `validate`
//! do with a suite file, and what the MCP server does with an agent's
//! inline `yaml`. Must never panic or hang, whatever the input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(yaml) = std::str::from_utf8(data) else {
        return;
    };
    // The streaming header scan (`--list`, sharding) reads the same text.
    let _ = atalanta_core::parse_headers(yaml);
    if let Ok(suite) = atalanta_core::parser::parse_suite_from_str(yaml) {
        let _ = atalanta_core::validate_suite(&suite);
    }
});
