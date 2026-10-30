# @velocity-test/cli

[Velocity](https://github.com/omkar-dev/velocity) is a fast, reliable mobile UI testing framework: YAML tests for Android, iOS and the web, an inspector, an MCP server for AI agents, and `velocity explore` to turn a goal into a test.

```sh
npx @velocity-test/cli --version
npm install --save-dev @velocity-test/cli   # then: npx velocity run tests/
```

This package is a small launcher. npm installs the prebuilt binary for your machine from one of its optional dependencies:

| Platform | Package |
|---|---|
| macOS, Apple silicon | `@velocity-test/cli-darwin-arm64` |
| macOS, Intel | `@velocity-test/cli-darwin-x64` |
| Linux x64 (static) | `@velocity-test/cli-linux-x64` |
| Linux arm64 (static) | `@velocity-test/cli-linux-arm64` |

Windows is not supported. If your install skips optional dependencies (`--omit=optional`), the launcher says so; `VELOCITY_BINARY_PATH=/path/to/velocity` points it at any binary.

Other ways to install: the [install script](https://github.com/omkar-dev/velocity#install), Homebrew (`brew install omkar-dev/velocity/velocity`) or `cargo install velocity-cli`.

Licensed under Apache-2.0.
