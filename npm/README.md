# @atalantahq/cli

[Atalanta](https://github.com/atalantahq/atalanta) is a fast, reliable mobile UI testing framework: YAML tests for Android, iOS and the web, an inspector, an MCP server for AI agents, and `atalanta explore` to turn a goal into a test.

```sh
npx @atalantahq/cli --version
npm install --save-dev @atalantahq/cli   # then: npx atalanta run tests/
```

This package is a small launcher. npm installs the prebuilt binary for your machine from one of its optional dependencies:

| Platform | Package |
|---|---|
| macOS, Apple silicon | `@atalantahq/cli-darwin-arm64` |
| macOS, Intel | `@atalantahq/cli-darwin-x64` |
| Linux x64 (static) | `@atalantahq/cli-linux-x64` |
| Linux arm64 (static) | `@atalantahq/cli-linux-arm64` |

Windows is not supported. If your install skips optional dependencies (`--omit=optional`), the launcher says so; `ATALANTA_BINARY_PATH=/path/to/atalanta` points it at any binary.

Other ways to install: the [install script](https://github.com/atalantahq/atalanta#install), Homebrew (`brew install atalantahq/tap/atalanta`) or `cargo install atalanta-cli`.

Licensed under Apache-2.0.
