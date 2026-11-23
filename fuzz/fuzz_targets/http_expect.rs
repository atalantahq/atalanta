//! The `http` step: JSONPath expressions in `expect.json` / `capture`, and
//! the step itself as written in a suite (its matchers and their checks).
#![no_main]

use libfuzzer_sys::fuzz_target;
use atalanta_api::path::Path;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // The first line as a JSONPath, run against a fixed document.
    let first = text.lines().next().unwrap_or("");
    if let Ok(path) = Path::parse(first) {
        let doc = serde_json::json!({
            "a": [1, {"b": "c"}, null, true, 2.5],
            "items": [{"sku": "A1", "qty": 2}, {"sku": "B2", "qty": 1}],
            "": {"": []}
        });
        let _ = path.select(&doc);
    }
    // The whole input as an `http:` step.
    if let Ok(req) = serde_yaml::from_str::<atalanta_common::http::HttpRequest>(text) {
        let _ = atalanta_api::validate(&req);
    }
});
