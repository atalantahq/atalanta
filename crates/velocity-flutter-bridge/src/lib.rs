//! Velocity's Flutter driver: a `PlatformDriver` that talks to a Dart
//! bridge running the app under `flutter test` with a `WidgetTester` (the
//! package in `dart/`), so Flutter widgets are tested headless, without a
//! device. Internal to the `velocity` CLI (`framework: flutter`); its API
//! has no stability promise.

pub mod config;
pub mod driver;
pub mod process;
pub mod protocol;

pub use config::{FlutterBridgeConfig, HierarchyMode};
pub use driver::FlutterDriver;
