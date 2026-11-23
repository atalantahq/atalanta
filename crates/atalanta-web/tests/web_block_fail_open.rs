// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `web.block`/mock rules must not fail open: if request interception can't
//! start, a web launch with such rules fails instead of letting blocked
//! requests reach the real network. Gated on Chrome like the other web tests
//! (see `tests/common/mod.rs`).

mod common;

use atalanta_common::{MockRule, PlatformDriver};
use atalanta_web::{WebDriver, WebDriverConfig};
use common::chrome_path;

fn block_rule() -> MockRule {
    MockRule {
        url: "blocked.test".into(),
        block: true,
        ..Default::default()
    }
}

/// `ATALANTA_WEB_TEST_FAIL_FETCH=1` makes `sync_fetch` refuse `Fetch.enable`.
/// With a block rule configured, `prepare` must fail with an error naming
/// interception. With no rules it must still succeed. Both cases run in one
/// test so the env var can't race other tests in this binary.
#[tokio::test]
async fn interception_failure_refuses_block_rules() {
    require_chrome!();

    // With a block rule: the launch must fail.
    std::env::set_var("ATALANTA_WEB_TEST_FAIL_FETCH", "1");
    let d = WebDriver::new(WebDriverConfig {
        mocks: vec![block_rule()],
        chrome_path: chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    });
    let err = d.prepare("web").await.unwrap_err();
    assert!(
        err.to_string()
            .contains("request interception could not start"),
        "{err}"
    );
    assert!(
        matches!(err, atalanta_common::AtalantaError::DeviceBootFailed { .. }),
        "{err}"
    );
    d.cleanup().await;

    // With no rules (the hook still set): Fetch is never turned on, so
    // nothing can fail and the launch succeeds.
    let d2 = WebDriver::new(WebDriverConfig {
        mocks: vec![],
        chrome_path: chrome_path(),
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    });
    d2.prepare("web")
        .await
        .expect("Chrome starts without rules");
    d2.cleanup().await;
    std::env::remove_var("ATALANTA_WEB_TEST_FAIL_FETCH");
}
