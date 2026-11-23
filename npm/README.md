# @atalantahq/cli

This package installs the `atalanta` command-line tool from npm, for projects that already use Node. [Atalanta](https://github.com/atalantahq/atalanta) runs YAML UI tests on Android emulators, iOS simulators and Chrome. It also provides a console (`atalanta inspect`), an MCP server for AI agents, and `atalanta explore`, which turns a goal into a test. It is pre-release: this package has not been published yet.

```sh
npx @atalantahq/cli --version
npm install --save-dev @atalantahq/cli   # then: npx atalanta run tests/
```

This package is only a launcher. npm installs the prebuilt binary for your machine from one of its optional dependencies:

| Platform | Package |
|---|---|
| macOS, Apple silicon | `@atalantahq/cli-darwin-arm64` |
| macOS, Intel | `@atalantahq/cli-darwin-x64` |
| Linux x64 (static) | `@atalantahq/cli-linux-x64` |
| Linux arm64 (static) | `@atalantahq/cli-linux-arm64` |

Windows is not supported. If your install skips optional dependencies (`--omit=optional`), the launcher says so. `ATALANTA_BINARY_PATH=/path/to/atalanta` points it at any binary.

Other ways to install: the [install script](https://github.com/atalantahq/atalanta#install), Homebrew (`brew install atalantahq/tap/atalanta`) or `cargo install atalanta-cli`.

Licence: AGPL-3.0-only.
