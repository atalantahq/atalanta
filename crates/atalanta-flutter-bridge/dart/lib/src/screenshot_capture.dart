import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';

/// Captures the current rendered frame as PNG bytes.
class ScreenshotCapture {
  /// The whole view (the root render view, so overlays, dialogs and every
  /// route are in it), at logical size: the image's pixels are the
  /// coordinates of the hierarchy's bounds. Empty when nothing is painted.
  static Future<Uint8List> capture(WidgetTester tester) async {
    final views = tester.binding.renderViews;
    if (views.isEmpty) return Uint8List(0);
    final view = views.first;
    final layer = view.debugLayer;
    if (layer is! OffsetLayer) return Uint8List(0);
    final dpr = tester.view.devicePixelRatio;
    final image = await layer.toImage(view.paintBounds, pixelRatio: 1 / dpr);
    try {
      final byteData = await image.toByteData(format: ui.ImageByteFormat.png);
      return byteData?.buffer.asUint8List() ?? Uint8List(0);
    } finally {
      image.dispose();
    }
  }
}
