//! Velocity's web driver: a `PlatformDriver` for Chrome over the DevTools
//! Protocol (`chromiumoxide`), with cross-origin frames on their own
//! connection, request mocking and network-idle sync, and evidence capture.
//! Internal to the `velocity` CLI (`platform: web`); its API has no
//! stability promise.
#![forbid(unsafe_code)]

pub mod cdp;
pub mod dom;
pub mod driver;
pub mod evidence;
pub mod frames;
pub mod live;
pub mod network;
pub mod protocol;
pub mod selector;

pub use driver::{ParallelMode, WebDriver, WebDriverConfig};
