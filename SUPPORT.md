# Support

## Before you ask

1. Run `atalanta doctor`. It checks adb, Xcode, devices, Chrome, AI keys and your suites, and names the fix for most first-run problems.
2. Read [Troubleshooting](docs/GUIDE.md#troubleshooting) in the guide.
3. Check [docs/STATUS.md](docs/STATUS.md). It lists what has been verified and on what, so you can tell a bug from a platform that has not been run yet.

## Where to go

| You have | Go to |
|---|---|
| A bug, or behaviour that differs from the docs | [GitHub Issues](https://github.com/atalantahq/atalanta/issues), using the bug report form |
| A feature request | [GitHub Issues](https://github.com/atalantahq/atalanta/issues), using the feature request form |
| A security vulnerability | [SECURITY.md](SECURITY.md). Do not open a public issue |
| A conduct concern | [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) |
| A change you want to contribute | [CONTRIBUTING.md](CONTRIBUTING.md) |

## What to include in a bug report

- The Atalanta version (`atalanta version`) and how you installed it.
- The host OS, and the device, emulator, simulator or browser with its OS version.
- The command you ran and its exit code.
- The smallest suite that shows the problem.
- For a failed test, the failure message and, if you can share it, the `trace.html` from `atalanta-results/traces/<test>/`. Secrets are masked in traces, but check the screenshots and logs before you attach them: they show whatever was on the screen.

## What to expect

Atalanta is an open-source project in pre-release. Issues are answered as maintainer time allows, and there is no guaranteed response time. The one exception is security reports, which have the response targets in [SECURITY.md](SECURITY.md). Supported versions are listed there too.
