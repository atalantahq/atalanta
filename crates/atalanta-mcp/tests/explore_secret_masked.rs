// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! An explore task's `secret` is typed by the `atalanta explore` child, which
//! reads the value from its own environment. This server must mask the value
//! in what it returns too (a later `snapshot` of the field the child typed it
//! into), as it does for `type_text {secret}`. Its own test binary: it sets
//! process environment variables.

use std::sync::Arc;

use atalanta_common::{Platform, PlatformDriver};
use atalanta_mcp::tasks::TaskManager;
use atalanta_mcp::McpServer;
use serde_json::json;

const VALUE: &str = "hunter2-explore-horse-staple";

#[cfg(unix)]
#[tokio::test]
async fn an_explore_secret_is_masked_in_this_servers_output() {
    std::env::set_var("VEL_MCP_EXPLORE_PW", VALUE);
    std::env::set_var("ATALANTA_SECRETS", "VEL_MCP_EXPLORE_PW");
    let dir = tempfile::tempdir().unwrap();
    let driver: Arc<dyn PlatformDriver> = Arc::new(atalanta_headless::HeadlessDriver::new(
        Platform::Android,
        Default::default(),
    ));
    let mut server = McpServer::new(driver, "headless".into(), None)
        .with_platform(Platform::Android)
        // The child exits at once: only the start of the task is under test.
        .with_atalanta_bin("/usr/bin/true".into())
        .with_tasks(TaskManager::new(dir.path().join("tasks"), vec![]));

    // Nothing has read the value in this process yet.
    let line = format!("Password field now shows {VALUE}");
    assert_eq!(atalanta_common::redact::redact(&line), line);

    let started = server
        .call(
            "run_task",
            json!({ "goal": "log in", "app_id": "com.app", "secret": ["VEL_MCP_EXPLORE_PW"] }),
        )
        .await
        .unwrap();
    assert_ne!(started["isError"], true, "{started}");

    let masked = atalanta_common::redact::redact(&line);
    assert!(!masked.contains(VALUE), "{masked}");
    server.shutdown().await;
}
