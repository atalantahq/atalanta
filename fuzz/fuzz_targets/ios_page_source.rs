// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! The iOS page-source parser (WebDriverAgent / AtalantaRunner XML, whose
//! labels and values come from the app under test).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };
    let _ = atalanta_ios::parser::parse_ios_hierarchy(xml);
});
