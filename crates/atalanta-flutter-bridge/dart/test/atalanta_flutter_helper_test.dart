import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:atalanta_flutter_helper/atalanta_flutter_helper.dart';

List<Map<String, dynamic>> nodesOf(Map<String, dynamic> json) =>
    (json['nodes'] as List).cast<Map<String, dynamic>>();

Map<String, dynamic>? byText(List<Map<String, dynamic>> nodes, String text) {
  for (final n in nodes) {
    if (n['text'] == text || n['label'] == text) return n;
  }
  return null;
}

Future<List<Map<String, dynamic>>> semantics(
    AtalantaBridgeServer s, WidgetTester tester) async {
  final r = await s.handleCommand({'cmd': 'get_hierarchy', 'mode': 'semantics'}, tester: tester);
  expect(r['status'], 'ok');
  return nodesOf(r['data'] as Map<String, dynamic>);
}

Future<List<Map<String, dynamic>>> widgets(
    AtalantaBridgeServer s, WidgetTester tester) async {
  final r = await s.handleCommand({'cmd': 'get_hierarchy'}, tester: tester);
  expect(r['status'], 'ok');
  return nodesOf(r['data'] as Map<String, dynamic>);
}

Map<String, dynamic> targetOf(Map<String, dynamic> n) {
  final b = n['bounds'] as Map<String, dynamic>;
  return {
    'node': n['node'],
    if (n['id'] != null) 'id': n['id'],
    'x': (b['x'] as int) + (b['width'] as int) ~/ 2,
    'y': (b['y'] as int) + (b['height'] as int) ~/ 2,
  };
}

AtalantaBridgeServer serverFor(Widget Function() app, {Future<void> Function()? onReset}) =>
    AtalantaBridgeServer(port: 0, appBuilder: app, onReset: onReset);

class Login extends StatelessWidget {
  const Login({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
        home: Scaffold(
          body: Column(children: [
            const TextField(key: ValueKey('email'), decoration: InputDecoration(labelText: 'Email')),
            const SizedBox(height: 40),
            const TextField(
                key: ValueKey('password'),
                obscureText: true,
                decoration: InputDecoration(labelText: 'Password')),
            Semantics(identifier: 'login', child: ElevatedButton(onPressed: () {}, child: const Text('Log in'))),
            const ElevatedButton(onPressed: null, child: Text('Disabled')),
            const Text('Hello', key: ValueKey('greeting')),
          ]),
        ),
      );
}

class Counter extends StatefulWidget {
  const Counter({super.key});
  @override
  State<Counter> createState() => _CounterState();
}

class _CounterState extends State<Counter> {
  int n = 0;
  @override
  Widget build(BuildContext context) => MaterialApp(
        home: Scaffold(
          body: Center(child: TextButton(onPressed: () => setState(() => n++), child: Text('count $n'))),
        ),
      );
}

String editableText(WidgetTester tester, String key) => tester
    .widget<EditableText>(find.descendant(of: find.byKey(ValueKey(key)), matching: find.byType(EditableText)))
    .controller
    .text;

void main() {
  testWidgets('widgets mode is flat, on-stage only, one node per Text', (tester) async {
    final s = serverFor(() => const Login());
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await widgets(s, tester);
    expect(nodes.first['parent'], isNull);
    final hello = nodes.where((n) => n['text'] == 'Hello').toList();
    expect(hello, hasLength(1), reason: 'RichText under Text is folded in');
    expect(hello.single['id'], 'greeting');
    final disabled = nodes.where((n) => n['text'] == 'Disabled').single;
    expect(disabled['enabled'], isFalse, reason: 'a disabled button disables its subtree');
    final ids = nodes.map((n) => n['node']).toSet();
    for (final n in nodes.skip(1)) {
      expect(ids.contains(n['parent']), isTrue);
    }
  });

  testWidgets('widgets mode skips the route under a pushed one', (tester) async {
    final s = serverFor(() => MaterialApp(
          home: Builder(
            builder: (context) => Scaffold(
              body: TextButton(
                onPressed: () => Navigator.of(context)
                    .push(MaterialPageRoute<void>(builder: (_) => const Scaffold(body: Text('Second')))),
                child: const Text('Save'),
              ),
            ),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    final w = await widgets(s, tester);
    expect(byText(w, 'Second'), isNotNull);
    expect(byText(w, 'Save'), isNull);
    final sem = await semantics(s, tester);
    expect(byText(sem, 'Second'), isNotNull);
    expect(byText(sem, 'Save'), isNull);
  });

  testWidgets('semantics mode: ids, flags, obscured text, no hidden nodes', (tester) async {
    final s = serverFor(() => const Login());
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await semantics(s, tester);
    expect(nodes.where((n) => n['id'] == 'login'), hasLength(1));
    final login = byText(nodes, 'Log in')!;
    expect(login['type'], 'Button');
    expect(login['enabled'], isTrue);
    final disabled = byText(nodes, 'Disabled')!;
    expect(disabled['enabled'], isFalse);
    expect(nodes.where((n) => n['label'] == 'Log in'), hasLength(1));
    final email = nodes.firstWhere((n) => n['id'] == 'email');
    expect(email['type'], 'TextField');
    expect(nodes.firstWhere((n) => n['id'] == 'greeting')['text'], 'Hello');

    await s.handleCommand({'cmd': 'input_text', 'text': 'hunter2', 'target': targetOf(nodes.firstWhere((n) => n['id'] == 'password'))}, tester: tester);
    final after = await semantics(s, tester);
    final pw = after.firstWhere((n) => n['id'] == 'password');
    expect(pw['obscured'], isTrue);
    expect(jsonEncode(after).contains('hunter2'), isFalse, reason: 'obscured text never leaks');
  });

  testWidgets('semantics mode: a switch reports checked, a selected node selected (S2)', (tester) async {
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: Column(children: [
              Switch(value: true, onChanged: (_) {}),
              Semantics(selected: true, button: true, child: const Text('Home tab')),
              Semantics(selected: false, button: true, child: const Text('Away tab')),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await semantics(s, tester);
    final sw = nodes.firstWhere((n) => n['type'] == 'Switch');
    expect(sw['checked'], isTrue);
    expect(byText(nodes, 'Home tab')!['selected'], isTrue);
    expect(byText(nodes, 'Away tab')!.containsKey('selected'), isFalse);
  });

  testWidgets('semantics mode leaves out Opacity(0) and scrolled-away items', (tester) async {
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: Column(children: [
              const Opacity(opacity: 0, child: Text('ghost')),
              Expanded(child: ListView(children: [for (var i = 0; i < 60; i++) SizedBox(height: 50, child: Text('item $i'))])),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await semantics(s, tester);
    expect(byText(nodes, 'ghost'), isNull);
    expect(byText(nodes, 'item 0'), isNotNull);
    expect(byText(nodes, 'item 40'), isNull);
    // The widgets mode still sees it (it is on stage), but not as visible.
    final w = await widgets(s, tester);
    expect(w.firstWhere((n) => n['text'] == 'ghost')['visible'], isFalse);
  });

  testWidgets('input_text goes to the targeted field (VEL-024)', (tester) async {
    final s = serverFor(() => const Login());
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await semantics(s, tester);
    final email = nodes.firstWhere((n) => n['id'] == 'email');
    final pw = nodes.firstWhere((n) => n['id'] == 'password');
    expect((await s.handleCommand({'cmd': 'input_text', 'text': 'a@b.c', 'target': targetOf(email)}, tester: tester))['status'], 'ok');
    expect((await s.handleCommand({'cmd': 'input_text', 'text': 'hunter2', 'target': targetOf(pw)}, tester: tester))['status'], 'ok');
    expect(editableText(tester, 'email'), 'a@b.c');
    expect(editableText(tester, 'password'), 'hunter2');

    // A point alone (no node) still picks the field under it.
    final t = targetOf(pw)..remove('node')..remove('id');
    await s.handleCommand({'cmd': 'clear_text', 'target': t}, tester: tester);
    expect(editableText(tester, 'password'), '');
    expect(editableText(tester, 'email'), 'a@b.c');

    // Widget-mode nodes work as targets too.
    final w = await widgets(s, tester);
    final wEmail = w.firstWhere((n) => n['id'] == 'email');
    await s.handleCommand({'cmd': 'input_text', 'text': 'x@y.z', 'target': targetOf(wEmail)}, tester: tester);
    expect(editableText(tester, 'email'), 'x@y.z');

    // No target and nothing focused: an error, never "the first field".
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final r = await s.handleCommand({'cmd': 'input_text', 'text': 'zzz'}, tester: tester);
    expect(r['status'], 'error');
    expect(r['code'], 'no_focused_field');
  });

  testWidgets('clear and type into a filled, focused field selected by its floating label', (tester) async {
    final s = serverFor(() => const Login());
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    await tester.enterText(find.byKey(const ValueKey('email')), 'old@example.com');
    await tester.pumpAndSettle();
    // Widgets mode: the field's only text node is its InputDecorator label.
    final w = await widgets(s, tester);
    final label = w.firstWhere((n) => n['text'] == 'Email');
    final t = targetOf(label)..remove('id');
    final box = tester.getRect(find.descendant(
        of: find.byKey(const ValueKey('email')), matching: find.byType(EditableText)));
    expect(box.contains(Offset((t['x'] as int).toDouble(), (t['y'] as int).toDouble())), isFalse,
        reason: 'the floated label lies outside the input box');

    final cleared = await s.handleCommand({'cmd': 'clear_text', 'target': t}, tester: tester);
    expect(cleared['status'], 'ok', reason: '$cleared');
    expect(editableText(tester, 'email'), '');
    final w2 = await widgets(s, tester);
    final t2 = targetOf(w2.firstWhere((n) => n['text'] == 'Email'))..remove('id');
    final typed = await s.handleCommand({'cmd': 'input_text', 'text': 'new@example.com', 'target': t2}, tester: tester);
    expect(typed['status'], 'ok', reason: '$typed');
    expect(editableText(tester, 'email'), 'new@example.com');
    expect(editableText(tester, 'password'), '');
  });

  testWidgets('a heading next to a lone field is not its label', (tester) async {
    // Review m7: the label walk climbed to the first ancestor with any
    // EditableText (here the Column) and typed into the screen's only field.
    final s = serverFor(() => const MaterialApp(
          home: Scaffold(
            body: Column(children: [
              Text('Shipping notes'),
              TextField(key: ValueKey('notes')),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final w = await widgets(s, tester);
    final t = targetOf(w.firstWhere((n) => n['text'] == 'Shipping notes'))..remove('id');
    final typed = await s.handleCommand({'cmd': 'input_text', 'text': 'zzz', 'target': t}, tester: tester);
    expect(typed['status'], 'error', reason: '$typed');
    expect(typed['code'], 'no_text_field');
    expect(editableText(tester, 'notes'), '');
  });

  testWidgets('a node follows its element after a scroll; a node from another snapshot is ignored', (tester) async {
    final tapped = <int>[];
    final list = ScrollController();
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: ListView(controller: list, children: [
              for (var i = 0; i < 30; i++)
                SizedBox(height: 60, child: TextButton(onPressed: () => tapped.add(i), child: Text('row $i'))),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final r = await s.handleCommand({'cmd': 'get_hierarchy'}, tester: tester);
    final data = r['data'] as Map<String, dynamic>;
    final generation = data['generation'] as int;
    final row5 = nodesOf(data).firstWhere((n) => n['text'] == 'row 5');
    list.jumpTo(120);
    await tester.pump();
    // The snapshot's point is now on row 7; the node still means row 5.
    final ok = await s.handleCommand(
        {'cmd': 'tap', 'target': {...targetOf(row5), 'generation': generation}}, tester: tester);
    expect(ok['status'], 'ok', reason: '$ok');
    expect(tapped, [5]);

    // Same node number, another generation: fall back to the point (row 7).
    tapped.clear();
    await s.handleCommand(
        {'cmd': 'tap', 'target': {...targetOf(row5), 'generation': generation + 41}}, tester: tester);
    expect(tapped, [7]);
  });

  (int, int) pngSize(String b64) {
    final bytes = base64Decode(b64);
    int be(int at) => (bytes[at] << 24) | (bytes[at + 1] << 16) | (bytes[at + 2] << 8) | bytes[at + 3];
    return (be(16), be(20));
  }

  testWidgets('init sizes the surface only when asked; screenshots match the surface (VEL-S39)', (tester) async {
    final s = serverFor(() => const Login());
    // Default: the surface is left alone and reported as it is.
    final plain = await s.handleCommand({'cmd': 'init', 'width': 1080, 'height': 2340}, tester: tester);
    expect((plain['data'] as Map)['size'], {'width': 800, 'height': 600});
    final shot = await s.handleCommand({'cmd': 'screenshot'}, tester: tester);
    expect(shot['status'], 'ok');
    expect(pngSize(shot['data'] as String), (800, 600));

    // With a dpr: 1080x2340 physical at 2.625 is a 411x891 logical phone.
    final phone = await s.handleCommand(
        {'cmd': 'init', 'width': 1080, 'height': 2340, 'dpr': 2.625}, tester: tester);
    expect((phone['data'] as Map)['size'], {'width': 411, 'height': 891});
    expect(tester.view.physicalSize, const Size(1080, 2340));
    final sem = await semantics(s, tester);
    final root = sem.first['bounds'] as Map;
    expect([root['width'], root['height']], [411, 891]);
    final phoneShot = await s.handleCommand({'cmd': 'screenshot'}, tester: tester);
    // A fractional logical size: the image rounds up.
    final (w, h) = pngSize(phoneShot['data'] as String);
    expect([w, h], [inInclusiveRange(411, 412), inInclusiveRange(891, 892)]);
  });

  testWidgets('screenshots include overlays above the app (dialogs)', (tester) async {
    final s = serverFor(() => MaterialApp(
          home: Builder(
            builder: (context) => TextButton(
              onPressed: () => showDialog<void>(
                  context: context,
                  builder: (_) => const AlertDialog(content: Text('Sure?'))),
              child: const Text('Ask'),
            ),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final before = await s.handleCommand({'cmd': 'screenshot'}, tester: tester);
    await tester.tap(find.text('Ask'));
    await tester.pumpAndSettle();
    final after = await s.handleCommand({'cmd': 'screenshot'}, tester: tester);
    expect(after['status'], 'ok');
    expect(pngSize(after['data'] as String), (800, 600));
    expect(after['data'], isNot(before['data']), reason: 'the dialog and its scrim are captured');
  });

  testWidgets('hit_test: a covered widget is not hittable, an uncovered one is', (tester) async {
    var covered = true;
    late StateSetter setCovered;
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: StatefulBuilder(builder: (context, set) {
              setCovered = set;
              return Stack(children: [
                Center(child: ElevatedButton(onPressed: () {}, child: const Text('Under'))),
                if (covered) const Positioned.fill(child: ColoredBox(color: Color(0xFF000000))),
              ]);
            }),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    var nodes = await widgets(s, tester);
    var under = nodes.firstWhere((n) => n['type'] == 'ElevatedButton');
    var r = await s.handleCommand({'cmd': 'hit_test', 'target': targetOf(under)}, tester: tester);
    expect((r['data'] as Map)['hittable'], isFalse);
    setCovered(() => covered = false);
    await tester.pump();
    nodes = await widgets(s, tester);
    under = nodes.firstWhere((n) => n['type'] == 'ElevatedButton');
    r = await s.handleCommand({'cmd': 'hit_test', 'target': targetOf(under)}, tester: tester);
    expect((r['data'] as Map)['hittable'], isTrue);
  });

  testWidgets('a tap on a covered widget is not_hittable', (tester) async {
    var taps = 0;
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: Stack(children: [
              Center(child: ElevatedButton(onPressed: () => taps++, child: const Text('Under'))),
              const ModalBarrier(dismissible: false),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    final nodes = await widgets(s, tester);
    final under = nodes.firstWhere((n) => n['type'] == 'ElevatedButton');
    final r = await s.handleCommand({'cmd': 'tap', ...targetOf(under), 'target': targetOf(under)}, tester: tester);
    expect(r['status'], 'error');
    expect(r['code'], 'not_hittable');
    expect(taps, 0);
  });

  testWidgets('tap on a target works and init clear resets state (VEL-141)', (tester) async {
    var resets = 0;
    final s = serverFor(() => const Counter(), onReset: () async => resets++);
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    for (var i = 0; i < 3; i++) {
      final nodes = await semantics(s, tester);
      final b = nodes.firstWhere((n) => n['type'] == 'Button');
      final r = await s.handleCommand({'cmd': 'tap', 'target': targetOf(b)}, tester: tester);
      expect(r['status'], 'ok');
    }
    expect(find.text('count 3'), findsOneWidget);
    await s.handleCommand({'cmd': 'reset'}, tester: tester);
    expect(find.text('count 3'), findsNothing);
    await s.handleCommand({'cmd': 'init', 'clear': true}, tester: tester);
    expect(find.text('count 0'), findsOneWidget);
    expect(resets, 1);
  });

  testWidgets('settling is bounded: an endless spinner does not stall commands (VEL-S02)', (tester) async {
    final s = serverFor(() => const MaterialApp(home: Scaffold(body: Center(child: CircularProgressIndicator()))));
    final init = await s.handleCommand({'cmd': 'init'}, tester: tester);
    expect(init['status'], 'ok');
    expect((init['data'] as Map)['settled'], isFalse);
    final sw = Stopwatch()..start();
    for (var i = 0; i < 50; i++) {
      await s.handleCommand({'cmd': 'get_hierarchy', 'mode': 'semantics'}, tester: tester);
    }
    final tap = await s.handleCommand({'cmd': 'tap', 'x': 10, 'y': 10}, tester: tester);
    expect(tap['status'], 'ok');
    expect(sw.elapsed, lessThan(const Duration(seconds: 10)));
  });

  testWidgets('swipe drags the scrollable under its start point', (tester) async {
    final outer = ScrollController();
    final inner = ScrollController();
    final s = serverFor(() => MaterialApp(
          home: Scaffold(
            body: Row(children: [
              Expanded(child: ListView(controller: outer, children: [for (var i = 0; i < 50; i++) SizedBox(height: 50, child: Text('a$i'))])),
              Expanded(child: ListView(controller: inner, children: [for (var i = 0; i < 50; i++) SizedBox(height: 50, child: Text('b$i'))])),
            ]),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    await s.handleCommand({'cmd': 'swipe', 'from_x': 600, 'from_y': 400, 'to_x': 600, 'to_y': 100}, tester: tester);
    expect(outer.offset, 0);
    expect(inner.offset, greaterThan(0));
  });

  testWidgets('keys: back pops, unsupported keys are errors', (tester) async {
    final s = serverFor(() => MaterialApp(
          home: Builder(
            builder: (context) => TextButton(
              onPressed: () => Navigator.of(context)
                  .push(MaterialPageRoute<void>(builder: (_) => const Scaffold(body: Text('Second')))),
              child: const Text('Go'),
            ),
          ),
        ));
    await s.handleCommand({'cmd': 'init'}, tester: tester);
    await tester.tap(find.text('Go'));
    await tester.pumpAndSettle();
    expect(find.text('Second'), findsOneWidget);
    expect((await s.handleCommand({'cmd': 'press_key', 'key': 'back'}, tester: tester))['status'], 'ok');
    expect(find.text('Second'), findsNothing);
    final home = await s.handleCommand({'cmd': 'press_key', 'key': 'home'}, tester: tester);
    expect(home['status'], 'error');
    expect(home['code'], 'unsupported');
  });

  testWidgets('run() serves commands in order over TCP and echoes ids', (tester) async {
    final s = AtalantaBridgeServer(port: 0, appBuilder: () => const Counter());
    final replies = <Map<String, dynamic>>[];
    // The client lives in the real zone; run() pumps in the test body.
    late Future<void> client;
    await tester.runAsync(() async {
      client = () async {
        while (s.boundPort == null) {
          await Future<void>.delayed(const Duration(milliseconds: 5));
        }
        final a = await Socket.connect(InternetAddress.loopbackIPv4, s.boundPort!);
        final b = await Socket.connect(InternetAddress.loopbackIPv4, s.boundPort!);
        final linesA = a.cast<List<int>>().transform(utf8.decoder).transform(const LineSplitter()).asBroadcastStream();
        final linesB = b.cast<List<int>>().transform(utf8.decoder).transform(const LineSplitter()).asBroadcastStream();
        a.writeln(jsonEncode({'id': 1, 'cmd': 'init'}));
        replies.add(jsonDecode(await linesA.first) as Map<String, dynamic>);
        b.writeln(jsonEncode({'id': 7, 'cmd': 'get_hierarchy', 'mode': 'semantics'}));
        replies.add(jsonDecode(await linesB.first) as Map<String, dynamic>);
        a.writeln('not json');
        replies.add(jsonDecode(await linesA.first) as Map<String, dynamic>);
        a.writeln(jsonEncode({'id': 2, 'cmd': 'shutdown'}));
        replies.add(jsonDecode(await linesA.first) as Map<String, dynamic>);
        a.destroy();
        b.destroy();
      }();
    });
    await s.run(tester);
    await tester.runAsync(() => client);
    expect(replies, hasLength(4));
    expect(replies[0]['id'], 1);
    expect(replies[0]['status'], 'ok');
    expect(replies[1]['id'], 7);
    expect(replies[1]['status'], 'ok');
    expect(replies[2]['status'], 'error');
    expect(replies[3]['id'], 2);
  });
}
