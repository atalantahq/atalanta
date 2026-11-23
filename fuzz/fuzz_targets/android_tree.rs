//! The Android UI-tree dump parsers: `uiautomator dump` XML and the tree
//! server's DUMP reply, both produced on the device (an app controls the
//! text, content descriptions and resource ids in them).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(xml) = std::str::from_utf8(data) else {
        return;
    };
    let _ = atalanta_android::parser::parse_hierarchy(xml);
    let _ = atalanta_android::parser::parse_hierarchy_v2(xml);
});
