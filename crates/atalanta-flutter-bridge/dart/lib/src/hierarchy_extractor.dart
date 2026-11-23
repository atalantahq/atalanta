import 'package:flutter/material.dart' show ButtonStyleButton;
import 'package:flutter/rendering.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

/// One node of a hierarchy snapshot, kept so a later action can name it.
class NodeRef {
  /// Global rect in logical pixels (the coordinates `tapAt` takes), at
  /// snapshot time.
  final Rect rect;
  final RenderObject? renderObject;
  final Element? element;

  /// The render object's own rect at snapshot time, to follow it if it moves.
  final Rect? _roRect;
  NodeRef(this.rect, this.renderObject, this.element) : _roRect = _globalRect(renderObject);

  /// [rect], moved by however far the render object has moved since the
  /// snapshot (a scroll, a keyboard, an animation): an action never taps
  /// where the element used to be.
  Rect get currentRect {
    final then = _roRect;
    final now = _globalRect(renderObject);
    if (then == null || now == null) return rect;
    return rect.shift(now.topLeft - then.topLeft);
  }
}

/// A flat hierarchy: `json` goes on the wire as
/// `{"format":"flat","nodes":[{"node":n,"parent":p|null,...}]}`, parents
/// before children. Flat JSON is three levels deep however deep the UI is.
class HierarchySnapshot {
  final Map<String, dynamic> json;
  final Map<int, NodeRef> _nodes;
  HierarchySnapshot(this.json, this._nodes);

  /// Numbers the snapshots a server hands out; set by the server and sent as
  /// `generation`, so a client never acts on a node from an older one.
  int? generation;

  NodeRef? resolve(int node) {
    final ref = _nodes[node];
    if (ref == null) return null;
    final el = ref.element;
    if (el != null && !el.mounted) return null;
    final ro = ref.renderObject;
    if (ro != null && !ro.attached) return null;
    return ref;
  }
}

Map<String, dynamic> _bounds(Rect r) {
  int n(double v) => v.isFinite ? v.round() : 0;
  return {'x': n(r.left), 'y': n(r.top), 'width': n(r.width), 'height': n(r.height)};
}

Rect? _globalRect(RenderObject? ro) {
  if (ro is! RenderBox || !ro.hasSize || !ro.attached) return null;
  try {
    final rect = MatrixUtils.transformRect(ro.getTransformTo(null), Offset.zero & ro.size);
    return rect.isFinite ? rect : null;
  } catch (_) {
    return null;
  }
}

String? _keyId(Widget widget) {
  final key = widget.key;
  if (key is ValueKey) return key.value?.toString();
  return null;
}

/// The Element-tree export: every on-stage widget, as before, but flat.
///
/// Off-stage children (inactive routes, `Offstage`) are skipped, a `RichText`
/// directly under `Text` is folded into it, and `enabled` / `visible` hold
/// for a whole subtree (an `IgnorePointer` or `Opacity(0)` ancestor counts).
class HierarchyExtractor {
  static HierarchySnapshot extractFlat(WidgetTester tester) {
    final nodes = <Map<String, dynamic>>[];
    final refs = <int, NodeRef>{};
    final root = tester.binding.rootElement;
    if (root == null) {
      nodes.add({
        'node': 0,
        'parent': null,
        'type': 'Root',
        'bounds': _bounds(Rect.zero),
        'enabled': true,
        'visible': false,
      });
      return HierarchySnapshot({'format': 'flat', 'nodes': nodes}, refs);
    }

    // Explicit stack: no recursion, whatever the depth.
    final stack = <_Visit>[_Visit(root, null, true, true)];
    var next = 0;
    while (stack.isNotEmpty) {
      final v = stack.removeLast();
      final element = v.element;
      final widget = element.widget;

      // A Text's own RichText repeats its text: skip it, keep its children.
      final parentWidget = v.parentWidget;
      final folded = widget is RichText && parentWidget is Text;

      var enabled = v.enabled;
      var visible = v.visible;
      if (widget is AbsorbPointer && widget.absorbing) enabled = false;
      if (widget is IgnorePointer && widget.ignoring) enabled = false;
      if (widget is ButtonStyleButton && !widget.enabled) enabled = false;
      if (widget is Opacity && widget.opacity == 0) visible = false;
      if (widget is Visibility && !widget.visible) visible = false;

      int? me = v.parent;
      if (!folded) {
        me = next++;
        final ro = element.renderObject;
        final rect = _globalRect(ro) ?? Rect.zero;

        String? text;
        if (widget is Text) {
          text = widget.data ?? widget.textSpan?.toPlainText();
        } else if (widget is RichText) {
          text = widget.text.toPlainText();
        } else if (widget is EditableText) {
          text = widget.obscureText
              ? widget.obscuringCharacter * widget.controller.text.length
              : widget.controller.text;
        }
        String? label;
        if (widget is Semantics) label = widget.properties.label;

        nodes.add({
          'node': me,
          'parent': v.parent,
          'id': _keyId(widget),
          'label': label,
          'text': text,
          'type': widget.runtimeType.toString(),
          'bounds': _bounds(rect),
          'enabled': enabled,
          'visible': visible && rect.width > 0 && rect.height > 0,
          if (widget is EditableText) 'obscured': widget.obscureText,
        });
        refs[me] = NodeRef(rect, ro, element);
      }

      final children = <Element>[];
      element.debugVisitOnstageChildren(children.add);
      for (final child in children.reversed) {
        stack.add(_Visit(child, me, enabled, visible, folded ? parentWidget : widget));
      }
    }
    return HierarchySnapshot({'format': 'flat', 'nodes': nodes}, refs);
  }

  /// The on-stage widget with `ValueKey(id)`, or the semantics node with
  /// that identifier.
  static NodeRef? findById(WidgetTester tester, String id) {
    final root = tester.binding.rootElement;
    if (root != null) {
      final stack = <Element>[root];
      while (stack.isNotEmpty) {
        final e = stack.removeLast();
        if (_keyId(e.widget) == id) {
          final rect = _globalRect(e.renderObject);
          if (rect != null) return NodeRef(rect, e.renderObject, e);
        }
        final children = <Element>[];
        e.debugVisitOnstageChildren(children.add);
        stack.addAll(children.reversed);
      }
    }
    final snap = SemanticsExtractor.extract(tester);
    for (final n in (snap.json['nodes'] as List).cast<Map<String, dynamic>>()) {
      if (n['id'] == id) return snap._nodes[n['node'] as int];
    }
    return null;
  }
}

class _Visit {
  final Element element;
  final int? parent;
  final bool enabled;
  final bool visible;
  final Widget? parentWidget;
  _Visit(this.element, this.parent, this.enabled, this.visible, [this.parentWidget]);
}

/// The semantics-tree export: what TalkBack/VoiceOver (and so Maestro, and
/// Velocity's device drivers) see.
///
/// Nodes merged into a parent, hidden (scrolled out, clipped) or invisible
/// ones are left out, so off-stage routes, `Opacity(0)` and scrolled-away
/// items never match. `id` is the semantics `identifier` (Flutter 3.19+) or
/// a `ValueKey`; `text` is the field value for text fields (masked when
/// obscured), else the label.
class SemanticsExtractor {
  static HierarchySnapshot extract(WidgetTester tester) {
    final nodes = <Map<String, dynamic>>[];
    final refs = <int, NodeRef>{};
    final view = tester.binding.renderViews.first;
    final dpr = view.flutterView.devicePixelRatio;
    final root = view.owner?.semanticsOwner?.rootSemanticsNode;
    if (root == null) {
      throw StateError('semantics are off: call tester.ensureSemantics() first');
    }

    final owners = _semanticsOwners(view);
    final emitted = <int, Map<String, dynamic>>{};
    final scale = Matrix4.diagonal3Values(1 / dpr, 1 / dpr, 1);

    final stack = <_SemVisit>[_SemVisit(root, null, scale)];
    while (stack.isNotEmpty) {
      final v = stack.removeLast();
      final node = v.node;
      final transform = node.transform == null
          ? v.transform
          : (v.transform.clone()..multiply(node.transform!));
      final isRoot = v.parent == null;
      if (!isRoot && (node.isMergedIntoParent || node.isInvisible || _hidden(node))) {
        continue;
      }
      final rect = MatrixUtils.transformRect(transform, node.rect);
      final data = node.getSemanticsData();
      final isTextField = _flag(data, SemanticsFlag.isTextField);
      final label = data.label.isNotEmpty ? data.label : (data.tooltip.isNotEmpty ? data.tooltip : null);
      final value = data.value.isNotEmpty ? data.value : null;
      final identifier = _identifier(data);
      final json = <String, dynamic>{
        'node': node.id,
        'parent': v.parent,
        'id': identifier,
        'label': label,
        'text': isTextField ? value : (label ?? value),
        'type': isRoot ? 'Root' : _role(data),
        'bounds': _bounds(rect),
        'enabled': !(_flag(data, SemanticsFlag.hasEnabledState) && !_flag(data, SemanticsFlag.isEnabled)),
        'visible': true,
        if (_flag(data, SemanticsFlag.isFocused)) 'focused': true,
        if (_flag(data, SemanticsFlag.hasCheckedState)) 'checked': _flag(data, SemanticsFlag.isChecked),
        // A switch's on/off is its toggled state (S2: `checked:` filters it).
        if (!_flag(data, SemanticsFlag.hasCheckedState) && _flag(data, SemanticsFlag.hasToggledState))
          'checked': _flag(data, SemanticsFlag.isToggled),
        if (_flag(data, SemanticsFlag.isSelected)) 'selected': true,
        if (_flag(data, SemanticsFlag.isObscured)) 'obscured': true,
      };
      nodes.add(json);
      emitted[node.id] = json;
      refs[node.id] = NodeRef(rect, owners[node.id], null);

      final children = <SemanticsNode>[];
      node.visitChildren((c) {
        children.add(c);
        return true;
      });
      for (final c in children.reversed) {
        stack.add(_SemVisit(c, node.id, transform));
      }
    }

    _joinKeys(tester, nodes, emitted, refs);
    return HierarchySnapshot({'format': 'flat', 'nodes': nodes}, refs);
  }

  /// Give `ValueKey`s to the semantics node they belong to (found the way
  /// `SemanticsController.find` does). A key whose widget has no node of its
  /// own (a keyed `Container`) becomes a child node of type `Widget`.
  static void _joinKeys(WidgetTester tester, List<Map<String, dynamic>> nodes,
      Map<int, Map<String, dynamic>> emitted, Map<int, NodeRef> refs) {
    final root = tester.binding.rootElement;
    if (root == null) return;
    var synthetic = -1;
    final stack = <Element>[root];
    while (stack.isNotEmpty) {
      final e = stack.removeLast();
      final children = <Element>[];
      e.debugVisitOnstageChildren(children.add);
      stack.addAll(children.reversed);

      // App keys only: the framework keys its own slots with enums.
      final key = e.widget.key;
      if (key is! ValueKey || key.value is! String) continue;
      final id = key.value as String;
      final ro = e.renderObject;
      // The outermost node the keyed widget's own render subtree produced
      // (a keyed TextField -> its text field node).
      final inside = _firstOwnNode(ro, emitted);
      if (inside != null && emitted[inside.id]!['id'] == null) {
        emitted[inside.id]!['id'] = id;
        continue;
      }
      RenderObject? r = ro;
      SemanticsNode? sem = r?.debugSemantics;
      while (r != null && (sem == null || sem.isMergedIntoParent)) {
        r = r.parent;
        sem = r?.debugSemantics;
      }
      if (sem == null) continue;
      final json = emitted[sem.id];
      if (json == null) continue; // inside a hidden or invisible subtree
      final rect = _globalRect(ro);
      final nodeRect = refs[sem.id]!.rect;
      final sameBox = rect != null &&
          (rect.left - nodeRect.left).abs() <= 1 &&
          (rect.top - nodeRect.top).abs() <= 1 &&
          (rect.width - nodeRect.width).abs() <= 1 &&
          (rect.height - nodeRect.height).abs() <= 1;
      if (json['id'] == null && (identical(r, ro) || sameBox)) {
        json['id'] = id;
        continue;
      }
      if (rect == null || rect.isEmpty) continue;
      final w = e.widget;
      final text = w is Text ? (w.data ?? w.textSpan?.toPlainText()) : null;
      nodes.add({
        'node': synthetic,
        'parent': sem.id,
        'id': id,
        'label': null,
        'text': text,
        'type': 'Widget',
        'bounds': _bounds(rect),
        'enabled': json['enabled'],
        'visible': true,
      });
      refs[synthetic] = NodeRef(rect, ro, e);
      synthetic--;
    }
  }

  static SemanticsNode? _firstOwnNode(RenderObject? ro, Map<int, Map<String, dynamic>> emitted) {
    if (ro == null) return null;
    final stack = <RenderObject>[ro];
    while (stack.isNotEmpty) {
      final r = stack.removeLast();
      final sem = r.debugSemantics;
      if (sem != null && !sem.isMergedIntoParent && emitted.containsKey(sem.id)) return sem;
      final children = <RenderObject>[];
      r.visitChildren(children.add);
      stack.addAll(children.reversed);
    }
    return null;
  }

  /// SemanticsNode id -> the outermost render object that produced it, for
  /// hit tests.
  static Map<int, RenderObject> _semanticsOwners(RenderObject root) {
    final owners = <int, RenderObject>{};
    final stack = <RenderObject>[root];
    while (stack.isNotEmpty) {
      final r = stack.removeLast();
      final sem = r.debugSemantics;
      if (sem != null) owners.putIfAbsent(sem.id, () => r);
      r.visitChildren(stack.add);
    }
    return owners;
  }

  static bool _hidden(SemanticsNode node) => _flag(node.getSemanticsData(), SemanticsFlag.isHidden);

  /// One place for the flag API, which moved to `flagsCollection` after 3.32.
  static bool _flag(SemanticsData data, SemanticsFlag flag) {
    // ignore: deprecated_member_use
    return data.hasFlag(flag);
  }

  /// `SemanticsData.identifier` exists from Flutter 3.19 on.
  static String? _identifier(SemanticsData data) {
    try {
      final id = (data as dynamic).identifier as String?;
      return (id == null || id.isEmpty) ? null : id;
    } catch (_) {
      return null;
    }
  }

  static String _role(SemanticsData data) {
    if (_flag(data, SemanticsFlag.isTextField)) return 'TextField';
    if (_flag(data, SemanticsFlag.hasToggledState)) return 'Switch';
    if (_flag(data, SemanticsFlag.hasCheckedState)) {
      return _flag(data, SemanticsFlag.isInMutuallyExclusiveGroup) ? 'Radio' : 'Checkbox';
    }
    if (_flag(data, SemanticsFlag.isSlider)) return 'Slider';
    if (_flag(data, SemanticsFlag.isLink)) return 'Link';
    if (_flag(data, SemanticsFlag.isButton)) return 'Button';
    if (_flag(data, SemanticsFlag.isImage)) return 'Image';
    if (_flag(data, SemanticsFlag.isHeader)) return 'Header';
    return 'View';
  }
}

class _SemVisit {
  final SemanticsNode node;
  final int? parent;
  final Matrix4 transform;
  _SemVisit(this.node, this.parent, this.transform);
}
