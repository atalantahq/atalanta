# Security Policy

## Supported versions

Velocity is pre-1.0. Security fixes go into the latest minor release line only.

| Version | Supported |
| ------- | --------- |
| 0.2.x   | Yes       |
| < 0.2   | No        |

Fixes ship as a new patch release (for example 0.2.3). Upgrade to the latest
0.2.x to get them. When 0.3.0 is released, 0.2.x keeps getting fixes for
critical issues for 30 days and then stops.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a
security problem.**

Report it privately through GitHub's private vulnerability reporting:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Fill in the advisory form.

Only the maintainers can see the report. Please include:

- the Velocity version (`velocity --version`) and how you installed it
  (release archive, Homebrew, npm, `cargo install`, source build);
- the platform: host OS, and the device, simulator, emulator or browser involved;
- the component, for example the `velocity inspect` console, the MCP server,
  the Android tree or video server, the iOS runner, the web driver, AI
  provider calls, or report and artifact output;
- steps to reproduce, or a proof of concept, and the impact you expect
  (for example "a web page I visit can tap on my emulator");
- whether you plan to disclose it publicly, and when.

If you can't use GitHub's reporting form, open an issue that asks for a
private contact channel. Don't include any details of the problem in it.

## What to expect

These are targets for a project maintained by volunteers, not guarantees:

| Step | Target |
| ---- | ------ |
| Acknowledge the report | within 3 business days |
| First assessment (accepted or declined, with a severity) | within 10 business days |
| Fix released for a critical or high-severity issue | within 30 days of the assessment |
| Fix released for a medium or low-severity issue | in the next scheduled release, normally within 90 days |

We will keep you informed while we work on it. We will agree a disclosure
date with you, publish a GitHub Security Advisory (with a CVE where
appropriate) when the fix ships, and credit you unless you'd rather not be
named.

## Scope

In scope: code in this repository and the artifacts built from it, which are
the `velocity` binary, the release archives and installers, the embedded
Android tree and video server (`velocity-tree.jar`), the iOS VelocityRunner,
the in-app sync probes and the Flutter bridge. Examples:

- a way for a web page, another user on the same machine, or a host on the
  network to drive a device, read the screen, run commands, or read or
  write files through any server Velocity starts. These are the inspector
  console, the tree, video and sync servers, the iOS runner, and their
  `adb`/usbmuxd port forwards;
- bypassing the inspector's token, `Host` or `Origin` checks;
- path traversal or overwriting files outside the project, flows or results
  directories;
- secrets (API keys, `${VAR}` values marked secret) showing up unredacted in
  reports, traces, logs, MCP replies or requests to AI providers;
- model output that turns into a selector, a typed value or a command
  without the checks described in `docs/SECURITY.md`;
- a vulnerable dependency that is reachable in practice.

Out of scope:

- what the system under test does. Velocity drives the app you point it at,
  with the access the device grants (`adb` shell, XCTest, Chrome DevTools).
  Someone who can already run `adb`, `xcrun` or Velocity commands as you
  already has that control;
- risks the documentation describes as accepted, such as binding the
  inspector to a non-loopback address with `--host`, or a cabled iPhone's
  runner listening on Wi-Fi without `ios.runnerToken`. A way around the
  protection those modes still have *is* in scope;
- what AI providers do with the data you choose to send them (see
  `docs/AI-PROVIDERS.md`);
- third-party tools Velocity calls but does not ship, such as `adb`,
  Xcode, WebDriverAgent, Chrome and the Android emulator. Please report
  those upstream;
- denial of service that needs local access or a test suite you wrote
  yourself.

`docs/SECURITY.md` describes the threat model for each network-facing part
of Velocity, the defences it has, and the risks that remain.
