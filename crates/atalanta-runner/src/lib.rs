// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

/// `println!` for lines that carry ANSI colour escapes: they are stripped
/// when colour is off (`--color never`, `NO_COLOR`, not a terminal; see
/// `atalanta_common::color`, C4).
macro_rules! cprintln {
    () => { println!() };
    ($($arg:tt)*) => {
        println!("{}", atalanta_common::color::paint(&format!($($arg)*)))
    };
}

pub mod artifacts;
pub mod completeness;
pub mod copies;
pub mod farm;
pub mod flake_cause;
pub mod history;
pub mod hunt;
pub mod impact;
pub mod incidents;
pub mod install;
pub mod lease;
pub mod live;
pub mod nl;
pub mod parallel;
pub mod quarantine;
pub mod reporter;
pub mod result_cache;
pub mod results_check;
mod retry;
pub mod runner;
pub mod scheduler;
pub mod shared_history;

pub use artifacts::save_screenshot;
pub use farm::DeviceFarm;
pub use history::TestHistory;
pub use parallel::ParallelRunner;
pub use reporter::html::write_html;
pub use reporter::json::write_json;
pub use reporter::junit::write_junit;
pub use runner::SuiteRunner;
#[allow(deprecated)]
pub use scheduler::shard_tests;
pub use scheduler::{
    filter_by_name, filter_by_tags, order_failed_first, order_longest_first, shard_by_count,
};
