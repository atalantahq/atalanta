# AtalantaRunner

Atalanta's own iOS runner, an alternative to WebDriverAgent. It is an XCUITest
bundle that serves the part of WebDriverAgent's HTTP API that Atalanta uses
(see `AtalantaRunnerUITests/AtalantaRunner.swift`). Read this if you want to
use it or change it.

## Use

Set `ATALANTA_IOS_RUNNER=atalanta`. Atalanta builds the runner (about 5 s, once
per Xcode build) and launches it the same way it launches WebDriverAgent.

## Change

The project, scheme and source are embedded in the Atalanta binary
(`crates/atalanta-ios/src/runner_src.rs`). After editing, regenerate the project
with `xcodegen generate` (spec: `project.yml`) and rebuild Atalanta.

## Limits

Only public XCTest APIs are used, except one: XCTest's wait-for-idle is turned
into a no-op (as WebDriverAgent does), because Atalanta runs its own settle.
`ATALANTA_RUNNER_WAIT_FOR_IDLE=1` keeps XCTest's waits.
