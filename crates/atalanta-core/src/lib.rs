// The suite JSON schema is one large `json!` literal (schema.rs).
#![recursion_limit = "256"]

pub mod a11y_run;
pub mod baselines;
pub mod config_keys;
pub mod dialog_handler;
pub mod env;
pub mod executor;
pub mod healing;
pub mod impact;
pub mod native_sync;
pub mod parser;
pub mod profiler;
pub mod reference;
pub mod resolver;
pub mod schema;
pub mod selector;
pub mod selector_lint;
pub mod selector_wire;
pub mod source_map;
pub mod streaming;
pub mod sync;
pub mod trace;
pub mod trace_html;
pub mod tree_diff;
pub mod validator;
pub mod visual;

pub use dialog_handler::DialogHandler;
pub use executor::TestExecutor;
pub use healing::{HealingConfig, SelectorHealer};
pub use native_sync::{NativeSyncClient, SyncManager};
pub use parser::{
    is_suite_file, parse_suite, parse_suite_from_str, parse_suite_with_positions, NOT_SUITES,
};
pub use profiler::ResourceProfiler;
pub use resolver::resolve_flows;
pub use selector::SelectorEngine;
pub use streaming::{parse_headers, ParserMode, TestHeader};
pub use sync::{AdaptiveSyncEngine, SmartPollingSyncEngine};
pub use tree_diff::TreeDiff;
pub use validator::{check_capabilities, validate_structure_at, validate_suite, validate_suite_at};

/// Whether the suite runs without a device: it has an `http` step, and
/// every step of its tests and flows is one that needs no screen (`http`,
/// `wait`, `setConfig`, `runFlow`). It may leave out `appId`.
pub fn http_only(suite: &atalanta_common::TestSuite) -> bool {
    let steps = suite
        .tests
        .iter()
        .flat_map(|t| &t.steps)
        .chain(suite.flows.iter().flat_map(|f| &f.steps));
    steps_need_no_device(steps)
}

/// [`http_only`] for tests with their flows inlined: the ones a run
/// selected. `atalanta run` gives them no device.
pub fn http_only_tests(tests: &[atalanta_common::TestCase]) -> bool {
    steps_need_no_device(tests.iter().flat_map(|t| &t.steps))
}

fn steps_need_no_device<'a>(steps: impl Iterator<Item = &'a atalanta_common::Step>) -> bool {
    use atalanta_common::Action;
    let mut any_http = false;
    for s in steps {
        match s.action {
            Action::Http(_) => any_http = true,
            Action::Wait { .. } | Action::SetConfig { .. } | Action::RunFlow { .. } => {}
            _ => return false,
        }
    }
    any_http
}
