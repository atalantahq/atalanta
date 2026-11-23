// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Only tells cargo to rebuild when the built frontend appears or changes
//! (review-d7 m1): rust-embed's release embedding tracks no file when
//! `frontend/dist` is missing, so building the UI afterwards left the
//! binary without it. This never runs npm (D-42).
fn main() {
    println!("cargo:rerun-if-changed=frontend/dist");
}
