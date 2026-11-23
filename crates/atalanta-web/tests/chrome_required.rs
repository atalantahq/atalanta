//! Review 07 M6: on CI (or with `ATALANTA_TEST_STRICT=1`) the Chrome
//! tests must run. Without Chrome they used to report PASS, so a runner
//! that lost Chrome made the web crate green and empty. This test fails
//! there even if every `require_chrome!` were bypassed.

mod common;

#[test]
fn chrome_is_present_where_it_is_required() {
    if common::chrome_required() {
        assert!(
            common::chrome_available(),
            "no Chrome binary on a CI server (or with ATALANTA_TEST_STRICT=1): \
             install Chrome, set CHROME_PATH, or ATALANTA_TEST_WEB=0 to skip the web tests"
        );
    }
}
