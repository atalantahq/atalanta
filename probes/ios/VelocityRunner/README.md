# VelocityRunner

Velocity's own iOS runner: an XCUITest bundle that serves the part of
WebDriverAgent's HTTP API Velocity uses (see `VelocityRunnerUITests/VelocityRunner.swift`).
Enable it with `VELOCITY_IOS_RUNNER=velocity`; Velocity builds it (≈5 s, once per
Xcode build) and launches it the same way it launches WebDriverAgent.

The project, scheme and source are embedded in the Velocity binary
(`crates/velocity-ios/src/runner_src.rs`). After editing, regenerate the project with
`xcodegen generate` (spec: `project.yml`) and rebuild Velocity.

Only public XCTest APIs are used, except one: XCTest's wait-for-idle is turned
into a no-op (as WebDriverAgent does), because Velocity runs its own settle.
`VELOCITY_RUNNER_WAIT_FOR_IDLE=1` keeps XCTest's waits.
