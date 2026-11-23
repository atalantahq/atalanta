// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! API testing for Atalanta: the `http` step.
//!
//! A step sends one request and checks the response: its status, headers,
//! JSONPath (RFC 9535) values and body, and how long it took. Every check
//! runs, and a failure lists each one that failed with the start of the
//! body. `capture:` takes values from the response for `${NAME}` in the
//! test's later steps, UI steps included: seed an order through the API,
//! then find it on screen. A test's steps share a cookie jar.
//!
//! The step types are in `atalanta_common::http` (the parser, schema and
//! executor share them); this crate sends them ([`HttpSession`]), checks
//! them before a run ([`validate()`]), and stands in for the device when a
//! suite has nothing else ([`NoDeviceDriver`]). [`openapi`] writes a suite
//! from an OpenAPI 3 document (`atalanta api scaffold`).
#![forbid(unsafe_code)]

pub mod check;
pub mod client;
pub mod guard;
pub mod no_device;
pub mod openapi;
pub mod path;
pub mod validate;

pub use client::{resolve_url, HttpSession, Passed, DEFAULT_RETRY_INTERVAL_MS, DEFAULT_TIMEOUT_MS};
pub use no_device::NoDeviceDriver;
pub use validate::{is_var_name, validate};
