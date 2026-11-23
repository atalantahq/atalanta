// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The host's parsers for tree-server reply lines (PONG, SETTEXT,
//! ENCODERS, WINDOWS). The server runs on the device; a squatting or
//! compromised one must not be able to crash the host with a reply.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    atalanta_android::tree_server::fuzzing::parse_reply_lines(&text);
});
