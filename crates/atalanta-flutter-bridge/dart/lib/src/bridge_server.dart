import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/gestures.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart' show JSONMethodCodec, MethodCall;
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'hierarchy_extractor.dart';
import 'screenshot_capture.dart';

/// Wire protocol version, returned by `init`. 2 = request ids, flat
/// hierarchies, targeted actions and error codes.
const int kAtalantaBridgeProtocol = 2;

/// TCP bridge server that accepts commands from Atalanta's Rust bridge.
///
/// Start it from a `flutter test` entry point and let it run for the whole
/// session:
///
/// ```dart
/// testWidgets('atalanta', (tester) =>
///     AtalantaBridgeServer(port: 19600, appBuilder: () => const MyApp()).run(tester),
///     timeout: Timeout.none);
/// ```
///
/// Do not wrap [run] in `tester.runAsync`: it does its own socket I/O inside
/// `runAsync` and handles every command, one at a time, in the test body, where
/// `pump` and virtual time work.
class AtalantaBridgeServer {
  final int port;
  final Widget Function() appBuilder;

  /// Called on `init` with `clear: true` (launchApp with clearState), after
  /// the old app is unmounted and before the new one is built. Reset fakes,
  /// service locators or mocked storage here.
  final FutureOr<void> Function()? onReset;

  /// Virtual-time cap for settling after an action. An endless animation
  /// (a spinner) never settles; the command still answers after this long.
  final Duration settleTimeout;

  ServerSocket? _server;
  WidgetTester? _tester;
  HierarchySnapshot? _snapshot;
  int _generation = 0;
  bool _viewResetRegistered = false;
  bool _stopped = false;

  final List<_Request> _inbox = <_Request>[];
  Completer<void>? _arrived;

  AtalantaBridgeServer({
    required this.port,
    required this.appBuilder,
    this.onReset,
    this.settleTimeout = const Duration(seconds: 2),
  });

  /// The bound port (useful with `port: 0`), once [run] has started.
  int? get boundPort => _server?.port;

  /// Run the bridge until a `shutdown` command or [stop]. Call this from the
  /// body of a `testWidgets`, not from inside `tester.runAsync`.
  Future<void> run(WidgetTester tester) async {
    _tester = tester;
    final semantics = tester.ensureSemantics();
    try {
      final server = (await tester.runAsync(() async {
        final s = await ServerSocket.bind(InternetAddress.loopbackIPv4, port);
        // Listen in the real zone: callbacks only enqueue, never pump.
        s.listen(_accept, onError: (Object _) {});
        return s;
      }))!;
      _server = server;
      // ignore: avoid_print
      print('[atalanta] Bridge server listening on port ${server.port}');

      while (!_stopped) {
        final req = await tester.runAsync(_next);
        if (req == null) break;
        final reply = await _dispatch(req.line);
        await tester.runAsync(() async {
          try {
            req.socket.writeln(jsonEncode(reply));
            await req.socket.flush();
          } catch (_) {
            // The client went away; the next command comes on a new connection.
          }
        });
      }
    } finally {
      await tester.runAsync(() async => _server?.close());
      semantics.dispose();
    }
  }

  /// Deprecated name for [run].
  @Deprecated('Use run(tester) from a testWidgets body, without runAsync')
  Future<void> start(WidgetTester tester) => run(tester);

  void _accept(Socket socket) {
    socket
        .cast<List<int>>()
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen(
          (line) => _enqueue(_Request(line, socket)),
          onError: (Object _) => socket.destroy(),
          cancelOnError: true,
        );
  }

  void _enqueue(_Request req) {
    _inbox.add(req);
    final arrived = _arrived;
    _arrived = null;
    arrived?.complete();
  }

  Future<_Request?> _next() async {
    while (_inbox.isEmpty) {
      if (_stopped) return null;
      _arrived = Completer<void>();
      await _arrived!.future;
    }
    return _inbox.removeAt(0);
  }

  /// Parse and run one command line; always returns a reply map.
  Future<Map<String, dynamic>> _dispatch(String line) async {
    Object? id;
    try {
      final cmd = jsonDecode(line) as Map<String, dynamic>;
      id = cmd['id'];
      final reply = await handleCommand(cmd);
      return {...reply, if (id != null) 'id': id};
    } catch (e) {
      return {
        'status': 'error',
        'message': e.toString(),
        if (id != null) 'id': id,
      };
    }
  }

  /// Run one command against the tester. Exposed for tests.
  @visibleForTesting
  Future<Map<String, dynamic>> handleCommand(Map<String, dynamic> cmd,
      {WidgetTester? tester}) async {
    if (tester != null) _tester = tester;
    final t = _tester!;

    switch (cmd['cmd'] as String?) {
      case 'init':
        _applySurface(t, cmd);
        // Unmount first so every State is disposed: a launch starts fresh.
        await t.pumpWidget(const SizedBox.shrink());
        if (cmd['clear'] == true) await onReset?.call();
        await t.pumpWidget(appBuilder());
        final settled = await _settle(t);
        _snapshot = null;
        final view = t.view;
        final logical = view.physicalSize / view.devicePixelRatio;
        return _ok({
          'protocol': kAtalantaBridgeProtocol,
          'settled': settled,
          // The surface actions and hierarchies use (logical px).
          'size': {'width': logical.width.round(), 'height': logical.height.round()},
          'dpr': view.devicePixelRatio,
        });

      case 'reset':
        await t.pumpWidget(const SizedBox.shrink());
        _snapshot = null;
        return _ok();

      case 'get_hierarchy':
        final mode = cmd['mode'] as String? ?? 'widgets';
        final snap = mode == 'semantics'
            ? SemanticsExtractor.extract(t)
            : HierarchyExtractor.extractFlat(t);
        snap.generation = ++_generation;
        _snapshot = snap;
        return _ok({...snap.json, 'generation': snap.generation});

      case 'screenshot':
        final png = await t.runAsync(() => ScreenshotCapture.capture(t));
        if (png == null || png.isEmpty) {
          return _error('screenshot_failed', 'no frame to capture');
        }
        return _ok(base64Encode(png));

      case 'hit_test':
        // Whether a tap at the target would reach it (visible = hit-testable).
        final target = _resolve(t, cmd);
        return _ok({'hittable': _hitCheck(t, target) == null});

      case 'tap':
        final target = _resolve(t, cmd);
        final blocked = _hitCheck(t, target);
        if (blocked != null) return blocked;
        await t.tapAt(target.point);
        return _ok({'settled': await _settle(t)});

      case 'double_tap':
        final target = _resolve(t, cmd);
        final blocked = _hitCheck(t, target);
        if (blocked != null) return blocked;
        await t.tapAt(target.point);
        await t.pump(kDoubleTapMinTime + const Duration(milliseconds: 10));
        await t.tapAt(target.point);
        return _ok({'settled': await _settle(t)});

      case 'long_press':
        final target = _resolve(t, cmd);
        final blocked = _hitCheck(t, target);
        if (blocked != null) return blocked;
        final ms = (cmd['duration_ms'] as num?)?.toInt() ?? 0;
        final hold = Duration(milliseconds: ms) > kLongPressTimeout
            ? Duration(milliseconds: ms)
            : kLongPressTimeout + const Duration(milliseconds: 100);
        final gesture = await t.startGesture(target.point);
        await t.pump(hold);
        await gesture.up();
        return _ok({'settled': await _settle(t)});

      case 'input_text':
      case 'clear_text':
        final text = cmd['cmd'] == 'clear_text' ? '' : cmd['text'] as String;
        if (!_hasTarget(cmd)) {
          // No target (an old client): type into the focused field, like
          // flutter_driver's enter_text. Never guess a field.
          if (t.binding.focusedEditable == null) {
            return _error('no_focused_field',
                'no text field is focused and the command names no target');
          }
          t.testTextInput.enterText(text);
          return _ok({'settled': await _settle(t)});
        }
        final target = _resolve(t, cmd);
        final field = _editableFor(t, target);
        if (field == null) {
          return _error('no_text_field',
              'no text field at (${target.point.dx}, ${target.point.dy})');
        }
        await t.enterText(find.byElementPredicate((e) => identical(e, field)), text);
        return _ok({'settled': await _settle(t)});

      case 'swipe':
        final from = Offset((cmd['from_x'] as num).toDouble(), (cmd['from_y'] as num).toDouble());
        final to = Offset((cmd['to_x'] as num).toDouble(), (cmd['to_y'] as num).toDouble());
        // Drag from the given point, so the scrollable under it moves.
        await t.dragFrom(from, to - from);
        return _ok({'settled': await _settle(t)});

      case 'pump_frames':
        final count = (cmd['count'] as num?)?.toInt() ?? 1;
        for (var i = 0; i < count; i++) {
          await t.pump(const Duration(milliseconds: 16));
        }
        return _ok();

      case 'settle':
        return _ok({'settled': await _settle(t)});

      case 'press_key':
        switch (cmd['key'] as String?) {
          case 'back':
            // What Android's system back sends.
            await t.binding.defaultBinaryMessenger.handlePlatformMessage(
              'flutter/navigation',
              const JSONMethodCodec().encodeMethodCall(const MethodCall('popRoute')),
              (_) {},
            );
            return _ok({'settled': await _settle(t)});
          case 'enter':
            await t.testTextInput.receiveAction(TextInputAction.done);
            return _ok({'settled': await _settle(t)});
          default:
            return _error('unsupported', 'key ${cmd['key']} is not supported by the Flutter bridge');
        }

      case 'shutdown':
        stop();
        return _ok();

      default:
        return _error('unknown_command', 'Unknown command: ${cmd['cmd']}');
    }
  }

  /// Pump until no frame is scheduled, for at most [settleTimeout] of virtual
  /// time. Never throws: an endless animation just reports `false`.
  Future<bool> _settle(WidgetTester t) async {
    const step = Duration(milliseconds: 16);
    await t.pump();
    var elapsed = Duration.zero;
    while (t.binding.hasScheduledFrame) {
      if (elapsed >= settleTimeout) return false;
      await t.pump(step);
      elapsed += step;
    }
    return true;
  }

  /// With `dpr` (and `width`/`height` in physical px), size the test surface
  /// like the configured screen. Without it the surface stays as it is.
  void _applySurface(WidgetTester t, Map<String, dynamic> cmd) {
    final dpr = (cmd['dpr'] as num?)?.toDouble();
    final w = (cmd['width'] as num?)?.toDouble();
    final h = (cmd['height'] as num?)?.toDouble();
    if (dpr == null || dpr <= 0 || w == null || h == null || w <= 0 || h <= 0) return;
    t.view.physicalSize = Size(w, h);
    t.view.devicePixelRatio = dpr;
    if (!_viewResetRegistered) {
      _viewResetRegistered = true;
      addTearDown(t.view.reset);
    }
  }

  static bool _hasTarget(Map<String, dynamic> cmd) =>
      cmd['target'] is Map || cmd['x'] is num;

  /// Where an action goes: the node from the last hierarchy, else a widget
  /// or semantics id, else the point.
  _Target _resolve(WidgetTester t, Map<String, dynamic> cmd) {
    final raw = cmd['target'];
    final target = raw is Map ? raw.cast<String, dynamic>() : cmd;
    final point = Offset(
      (target['x'] as num? ?? cmd['x'] as num? ?? 0).toDouble(),
      (target['y'] as num? ?? cmd['y'] as num? ?? 0).toDouble(),
    );
    final node = (target['node'] as num?)?.toInt();
    final generation = (target['generation'] as num?)?.toInt();
    final snap = _snapshot;
    // A node numbered by another snapshot may name a different element now.
    if (node != null && snap != null && (generation == null || generation == snap.generation)) {
      final hit = snap.resolve(node);
      final rect = hit?.currentRect;
      if (hit != null && rect != null && rect.isFinite && !rect.isEmpty) {
        return _Target(rect.center, hit.renderObject, hit.element);
      }
    }
    final id = target['id'] as String?;
    if (id != null && id.isNotEmpty) {
      final hit = HierarchyExtractor.findById(t, id);
      if (hit != null && hit.rect.isFinite && !hit.rect.isEmpty) {
        // Keep the caller's point when it lies on the element.
        final p = hit.rect.contains(point) ? point : hit.rect.center;
        return _Target(p, hit.renderObject, hit.element);
      }
    }
    return _Target(point, null, null);
  }

  /// A not_hittable error when something else is on top of the target.
  Map<String, dynamic>? _hitCheck(WidgetTester t, _Target target) {
    final ro = target.renderObject;
    if (ro == null || !ro.attached) return null;
    // A target that takes no hits there itself (a transparent wrapper) can't
    // be judged: tap the point as before.
    if (ro is RenderBox && ro.hasSize) {
      final own = BoxHitTestResult();
      if (!ro.hitTest(own, position: ro.globalToLocal(target.point))) return null;
    }
    final result = HitTestResult();
    final view = t.view;
    t.binding.hitTestInView(result, target.point, view.viewId);
    final onTop = result.path.any((entry) => identical(entry.target, ro));
    if (onTop) return null;
    return _error('not_hittable',
        'the target at (${target.point.dx}, ${target.point.dy}) is covered by another widget or not hit-testable');
  }

  /// The EditableText under the target: inside the target widget, else the
  /// field whose box contains the point (the smallest, if nested), else the
  /// field the target labels.
  Element? _editableFor(WidgetTester t, _Target target) {
    final el = target.element;
    if (el != null && el.mounted) {
      final inside = find
          .descendant(
            of: find.byElementPredicate((e) => identical(e, el)),
            matching: find.byType(EditableText),
            matchRoot: true,
          )
          .evaluate();
      if (inside.isNotEmpty) return inside.first;
    }
    Element? best;
    var bestArea = double.infinity;
    for (final e in find.byType(EditableText).evaluate()) {
      final ro = e.renderObject;
      if (ro is! RenderBox || !ro.hasSize) continue;
      final rect = ro.localToGlobal(Offset.zero) & ro.size;
      if (!rect.contains(target.point)) continue;
      final area = rect.width * rect.height;
      if (area < bestArea) {
        best = e;
        bestArea = area;
      }
    }
    if (best != null) return best;
    // A field's label (a sibling of its EditableText, above the input box
    // once it floats): the field of the nearest enclosing text field widget
    // (TextField, TextFormField, InputDecorator, CupertinoTextField). The
    // walk stops there: a heading or button next to a one-field form is no
    // label, so climbing on to the form would type into an unrelated field
    // (review m7).
    if (el != null && el.mounted) {
      Element? field;
      el.visitAncestorElements((a) {
        if (_isTextFieldWidget(a.widget)) {
          field = a;
          return false;
        }
        return true;
      });
      final f = field;
      if (f != null) {
        final owned = <Element>[];
        void visit(Element e) {
          if (owned.length > 1) return;
          if (e.widget is EditableText) {
            owned.add(e);
            return;
          }
          e.visitChildren(visit);
        }
        f.visitChildren(visit);
        if (owned.length == 1) return owned.single;
      }
    }
    // A semantics node for a TextField can be a little larger than its
    // EditableText (decoration, padding): fall back to the field under it.
    final ro = target.renderObject;
    if (ro != null) {
      for (final e in find.byType(EditableText).evaluate()) {
        RenderObject? r = e.renderObject;
        while (r != null) {
          if (identical(r, ro)) return e;
          r = r.parent;
        }
      }
    }
    return null;
  }

  static Map<String, dynamic> _ok([Object? data]) =>
      {'status': 'ok', if (data != null) 'data': data};

  static Map<String, dynamic> _error(String code, String message) =>
      {'status': 'error', 'code': code, 'message': message};

  void stop() {
    _stopped = true;
    _server?.close();
    final arrived = _arrived;
    _arrived = null;
    arrived?.complete();
  }
}

class _Request {
  final String line;
  final Socket socket;
  _Request(this.line, this.socket);
}

class _Target {
  final Offset point;
  final RenderObject? renderObject;
  final Element? element;
  _Target(this.point, this.renderObject, this.element);
}

/// Widgets that own exactly one input box and its label (review m7).
/// Matched by name for the Material and Cupertino ones, so the bridge does
/// not import those libraries.
bool _isTextFieldWidget(Widget w) {
  switch (w.runtimeType.toString()) {
    case 'TextField':
    case 'TextFormField':
    case 'InputDecorator':
    case 'CupertinoTextField':
    case 'CupertinoTextFormFieldRow':
      return true;
  }
  return false;
}
