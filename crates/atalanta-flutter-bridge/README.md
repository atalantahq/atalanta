# atalanta-flutter-bridge

Rust client for driving Flutter apps in headless mode via an in-app Dart bridge.

The bridge is a Dart package (`dart/`) that the app under test embeds; it
serves newline-delimited JSON over TCP on 127.0.0.1. The Rust client
(`src/process.rs`) only *connects* to a running bridge — it does not spawn
it, and so never shuts it down: `stopApp` unmounts the app and the next
`launchApp` mounts it again.

## Embedding the bridge

The bridge runs inside a `flutter_test` harness (it drives the app through a
`WidgetTester`). Add the package from `crates/atalanta-flutter-bridge/dart`
to the app's `pubspec.yaml` as a dev dependency (path dependency), then add a
test entry point:

```dart
import 'package:flutter_test/flutter_test.dart';
import 'package:atalanta_flutter_helper/atalanta_flutter_helper.dart';
import 'package:my_app/main.dart';

void main() {
  testWidgets('atalanta', (tester) => AtalantaBridgeServer(
        port: 19600,
        appBuilder: () => const MyApp(),
        // Optional: reset fakes and mocked storage on launchApp clearState.
        onReset: () {},
      ).run(tester), timeout: Timeout.none);
}
```

Start it with `flutter test test/atalanta_test.dart`, then run tests with:

```bash
atalanta run tests/ --driver headless --framework flutter
```

Call `run(tester)` from the test body, not inside `tester.runAsync`: it does
its socket I/O in `runAsync` itself and handles one command at a time in the
test body, where pumping and virtual time work. After each action it settles
for at most 2 s of virtual time (`settleTimeout`), so an endless animation
never stalls a command.

## Hierarchy

`get_hierarchy` returns a flat node list (`{"format":"flat","nodes":[…]}`,
each node with its `parent`), which Rust rebuilds without recursion.

- `semantics` (the default): the semantics tree, as TalkBack/VoiceOver and
  Atalanta's device drivers see it, so one YAML answers the same headless and
  on a device. Merged, hidden and invisible nodes are left out; `id` is the
  `Semantics(identifier:)` or a string `ValueKey`; `text` is the merged label
  (or a text field's value, masked when obscured). `text:` matches a button's
  merged label rather than the `Text` widget inside it.
- `widgets` (deprecated opt-out: `flutter: { hierarchy: widgets }` in the
  suite config or `ATALANTA_FLUTTER_HIERARCHY=widgets`; the run prints a
  warning): every on-stage widget — `text:` matches `Text` widgets, `id:`
  matches a `ValueKey`. Off-stage routes are left out, each `Text` appears
  once, and `IgnorePointer`/disabled buttons and `Opacity(0)`/`Visibility`
  hold for their whole subtree.

## Flutter SDK floor

The bridge needs **Flutter 3.19 or newer** (`Semantics(identifier:)`, which
`id:` matches, arrived in 3.19; on older SDKs an app compiles but no `id:`
ever matches). The Dart package's `pubspec.yaml` says `flutter: ">=3.19.0"`,
and a headless run whose project `.dart_tool/version` (or
`package_config.json` `flutterVersion`) names an older Flutter stops with a
configuration error (exit 2) asking for `flutter upgrade`. A version that
can't be read (`0.0.0-unknown`, which an SDK installed by a shallow
`git clone` reports, or anything unparsable) is unknown: the check is skipped
(logged at debug level), never failed. Device mode is not affected: it
doesn't use the Dart package.

Element actions name their target (node id from the last hierarchy, element
id, centre). Text goes to the text field under the target (or the field a label
belongs to, also once the label has floated); a tap on a target
that something else covers fails with `not_hittable`.
Each hierarchy carries a `generation`; a node from any other snapshot is
ignored, and a node's point follows its element if it has moved since.
Trees deeper than 512 levels keep their deeper nodes, hung off their
ancestor at depth 512.

## Screen size and screenshots

The driver uses the logical surface size the bridge reports on `init`
(800x600 under plain `flutter test`) for `screen_size`, swipes and the
on-screen filter. Set `headless: { devicePixelRatio: 2.625 }` in the suite
config (`device_pixel_ratio` in the bridge config) to size the surface as
`headless.width` x `headless.height` physical px at that ratio instead
(1080x2340 at 2.625 is a 411x891 logical phone). Unset keeps dpr 1. Screenshots capture the whole view
(dialogs and overlays included) at logical size, so their pixels match the
hierarchy's bounds.

## Visibility

`assertVisible` uses the hierarchy's `visible` flag. With
`hit_test_visibility: true` in the bridge config (or
`ATALANTA_FLUTTER_HIT_VISIBILITY=1`) it also requires a tap at the element's
centre to reach it, so a widget under an overlay is not visible. This is off
by default because it also makes `assertNotVisible` pass for a covered
widget.
