---
name: tree-server
description: Change Atalanta's embedded on-device code and ship it correctly — the Android tree server (probes/android/atalanta-tree-server: Server.java, VideoServer.java, build.sh, the committed atalanta-tree.jar, the protocol version that must agree in four places, scripts/check-tree-jar.sh) and the iOS AtalantaRunner (probes/ios/AtalantaRunner: Swift, project.yml, xcodegen, the committed .xcodeproj embedded by runner_src.rs). Use when editing anything under probes/, when the tree-jar or "Tree-server protocol agrees" CI check fails, when adding a tree-server command or reply field, or when the jar "does not reproduce".
---

# Embedded probes: the Android tree server and the iOS runner

Both are built outside cargo and shipped inside the binary: the Rust crate embeds a committed build product. A source edit without the rebuilt product changes nothing on a device, and CI rebuilds the product to prove it matches the source.

## Android tree server

Source: `probes/android/atalanta-tree-server/src/com/atalanta/tree/` (`Server.java`, `VideoServer.java`, `TagReads.java`). Host side: `crates/atalanta-android/src/tree_server.rs`, which embeds `crates/atalanta-android/assets/atalanta-tree.jar` with `include_bytes!`.

### After editing the Java

```bash
probes/android/atalanta-tree-server/build.sh    # javac, the host checks, d8, copies the jar into assets/
scripts/check-tree-jar.sh                       # rebuilds in a temp dir and compares with the committed jar
git add crates/atalanta-android/assets/atalanta-tree.jar
```

Commit the jar with the source change, in one commit.

The jar must rebuild byte for byte, so the toolchain is pinned in `toolchain.env` (`TREE_PLATFORM`, `TREE_BUILD_TOOLS`, `TREE_JDK`): d8 stamps its version into the dex and a different javac patch release emits different class files. Install exactly those (`sdkmanager "platforms;<TREE_PLATFORM>" "build-tools;<TREE_BUILD_TOOLS>"`, a JDK of the `TREE_JDK` version). `check-tree-jar.sh` exits 0 reproduced, 1 differs, 2 cannot check; with a different JDK it warns and usually exits 1. Exit 2 is "not checked", not a pass: say so, and let the `tree-jar` CI job decide. Changing a pin means rebuilding and committing the jar in the same change.

### When the wire protocol changes

A new command, a new field in a reply, a changed meaning: bump the protocol number, and bump it in all four places or CI fails.

| Where | What |
|---|---|
| `Server.java` | `static final String VERSION` and a "Protocol N: …" line in the class doc saying what changed |
| `crates/atalanta-android/src/tree_server.rs` | `const SERVER_VERSION` |
| `probes/android/atalanta-tree-server/test/com/atalanta/tree/HostChecks.java` | the `Server.VERSION.equals("N")` check (`build.sh` fails without it) |
| `CLAUDE.md` | "(protocol N: …" in the tree-server paragraph (`claude_md_names_this_protocol`, and the "Tree-server protocol agrees" step in `ci.yml`) |

Then, on the host side:

- **Record the protocol.** The doc comment above `OLDEST_VERSION` in `tree_server.rs` is the protocol history: add a sentence for the new number saying what changed and what a server without it does. `supported` decides which reported versions the host accepts, and the tests' stand-in servers still speak the old ones, so a host change must keep working when the new field or command is absent. Raising `OLDEST_VERSION` is a deliberate, changelogged decision.
- **Replies are untrusted input.** Parse a new reply defensively: no `unwrap`, no indexing, bounded lengths. Add it to `parse_reply_lines` in the `#[cfg(fuzzing)] mod fuzzing` so the `tree_server_lines` fuzz target covers it, and to `mangled_replies_never_panic`.
- **The handshake is not negotiable.** The per-start token never crosses the socket (mutual `HELLO`/`AUTH` HMAC; the server proves it holds the token first), and a line that is not a command closes the connection. A new command goes after authentication, like the others. `VideoServer` has its own process and the same kind of handshake.

### Test

```bash
cargo test -p atalanta-android                                       # fakes: scripted adb on a PATH
ATALANTA_TEST_ANDROID=<serial> cargo test -p atalanta-android        # also drives a real device or emulator
ATALANTA_TEST_STRICT=1 ATALANTA_TEST_ANDROID=<serial> cargo test -p atalanta-android   # a missing device fails
```

No CI job runs the device-backed tests (`dumps_from_a_real_device`, `records_from_a_real_device`, `animations_come_back_after_the_run`), so run them yourself on an emulator and say which API level. Then run `tests/smoke/android-settings.yaml` through `verify-e2e`. The Java `HostChecks` run on the host JVM inside `build.sh`: add checks there for pure logic (parsing, encoding), since they need no device.

## iOS: AtalantaRunner

Source: `probes/ios/AtalantaRunner` (`AtalantaRunnerUITests/AtalantaRunner.swift`, `project.yml`). `crates/atalanta-ios/src/runner_src.rs` embeds four files with `include_str!` — the Swift source and three files of the committed `AtalantaRunner.xcodeproj` — and Atalanta builds them on the user's Mac on first use (`ATALANTA_IOS_RUNNER=atalanta`), cached under `~/.atalanta/wda/build/` by a hash of those files.

After editing the Swift or `project.yml`:

```bash
cd probes/ios/AtalantaRunner
xcodegen generate
xcodebuild -project AtalantaRunner.xcodeproj -scheme AtalantaRunner \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build-for-testing
```

Commit the regenerated `.xcodeproj`. `project.yml` is not embedded, so a `project.yml` edit without `xcodegen generate` ships the old project, and nothing in CI compares the two. A new source file must also be added to `FILES` in `runner_src.rs`. The unit test there pins the strings in the Swift that the host relies on (routes, attributes the parser reads): the Swift cannot be built in `cargo test`, so that list is the contract. Add to it when the host starts depending on something new, and never edit it only to make it pass.

Then `cargo test -p atalanta-ios`, and `ATALANTA_IOS_RUNNER=atalanta target/release/atalanta run tests/smoke/ios-settings.yaml` on a simulator. The routes are WDA-compatible on purpose: a change in a reply's shape must be made in the WDA path too, or be gated on the runner.

## The sync probes

`probes/ios/AtalantaSyncProbe` (SwiftPM) and `probes/android/atalanta-sync-probe` (Gradle) are linked into the app under test, not embedded in Atalanta. Their host checks need no device: `probes/ios/AtalantaSyncProbe/HostChecks/run.sh` and `probes/android/atalanta-sync-probe/host-check.sh`.

## CI

`tree-jar` in `ci.yml` runs `scripts/check-tree-jar.sh` when the probe, the jar or the script changes, and is part of `CI OK`. The Probes workflow (`probes.yml`: the Gradle and xcodebuild builds of everything above) runs only on the `probes-ci` label or by hand: ask for the label on a PR that touches `probes/`.

## Report

What changed on the wire, the old and new protocol number, the result of `check-tree-jar.sh` (reproduced, differs, or not checked and why), and which device or simulator ran the device-backed tests and the smoke suite.
