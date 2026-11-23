// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Atalanta Headless Renderer
//!
//! CPU-only rendering engine for Android XML layouts and iOS XIB/Storyboard files.
//! Enables layout assertions in ~50ms without emulators or simulators.
//!
//! # Architecture
//!
//! Two-layer design:
//! 1. **Core Layer** (platform-agnostic): Taffy flexbox layout + tiny-skia pixel rendering
//! 2. **Platform Adapters**: Android AXML parser + iOS XIB parser → RenderTree
//!
//! # Usage
//!
//! ```rust,ignore
//! use atalanta_headless::HeadlessDriver;
//! use atalanta_common::Platform;
//!
//! let driver = HeadlessDriver::new(Platform::Android, HeadlessConfig::default());
//! ```
#![forbid(unsafe_code)]

// Core rendering layer
pub mod element_map;
pub mod layout;
pub mod render_tree;
pub mod scroll;
pub mod surface;
pub mod text;
pub mod unrendered;

// Platform adapters
pub mod android;
pub mod ios;

// Driver integration
pub mod config;
pub mod driver;
pub mod session;

// Public re-exports
pub use config::HeadlessConfig;
pub use driver::HeadlessDriver;
pub use render_tree::{Color, ComputedLayout, EdgeSizes, NodeStyle, RenderNode};
pub use surface::SoftwareSurface;
