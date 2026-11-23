//! Atalanta's MCP server: JSON-RPC over stdio that gives AI agents the
//! device tools (look at the screen, act, assert), suite runs and
//! `atalanta explore` sessions as background tasks, and `atalanta doctor`'s
//! diagnostics. Internal to the `atalanta` CLI (`atalanta mcp`); its API has
//! no stability promise.

pub mod agent_error;
mod authored;
pub mod install;
pub mod profile;
mod recording;
mod refs;
mod server;
pub mod session;
pub mod skills;
mod snapshot;
pub mod tasks;
#[cfg(test)]
mod test_util;
pub mod tool_registry;
pub mod tools;
mod trace_view;

pub use server::McpServer;
