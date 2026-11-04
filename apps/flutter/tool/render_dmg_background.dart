// Export the real desktop starfield as a Retina Finder background.
// Run from apps/flutter: ../../scripts/flutter.sh test tool/render_dmg_background.dart
import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/space.dart';
import 'package:wispnet/src/ui/theme.dart';

void main() {
  testWidgets('export the installer background', (tester) async {
    tester.view.physicalSize = const Size(720, 440);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    await tester.runAsync(() async {
      final fonts = FontLoader('Roboto');
      for (final weight in ['Regular', 'Medium', 'Bold']) {
        fonts.addFont(
          File('assets/fonts/Roboto-$weight.ttf')
              .readAsBytes()
              .then((bytes) => ByteData.sublistView(bytes)),
        );
      }
      await fonts.load();
    });
    final boundary = GlobalKey();
    await tester.pumpWidget(
      MediaQuery(
        data: const MediaQueryData(disableAnimations: true),
        child: Directionality(
          textDirection: TextDirection.ltr,
          child: RepaintBoundary(
            key: boundary,
            child: ColoredBox(
              color: WispColors.background,
              child: WispSpace(
                online: true,
                warp: const AlwaysStoppedAnimation(0),
                child: Stack(
                  children: [
                    const Positioned(
                      top: 44,
                      left: 0,
                      right: 0,
                      child: Text(
                        'Install Wispnet',
                        textAlign: TextAlign.center,
                        style: TextStyle(
                          fontFamily: 'Roboto',
                          fontSize: 28,
                          fontWeight: FontWeight.w500,
                          color: WispColors.text,
                        ),
                      ),
                    ),
                    const Positioned(
                      top: 90,
                      left: 0,
                      right: 0,
                      child: Text(
                        'Drag Wispnet to Applications',
                        textAlign: TextAlign.center,
                        style: TextStyle(
                          fontFamily: 'Roboto',
                          fontSize: 16,
                          color: WispColors.muted,
                        ),
                      ),
                    ),
                    Positioned.fill(
                      child: CustomPaint(painter: _InstallArrow()),
                    ),
                    const Positioned(
                      bottom: 42,
                      left: 0,
                      right: 0,
                      child: Text(
                        'Then open Wispnet from Applications.',
                        textAlign: TextAlign.center,
                        style: TextStyle(
                          fontFamily: 'Roboto',
                          fontSize: 14,
                          color: WispColors.muted,
                        ),
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
    await tester.pump();
    final render =
        boundary.currentContext!.findRenderObject()! as RenderRepaintBoundary;
    await tester.runAsync(() async {
      for (final ratio in [1.0, 2.0]) {
        final image = await render.toImage(pixelRatio: ratio);
        final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
        final suffix = ratio == 1 ? '' : '@2x';
        await File('../../packaging/macos/background$suffix.png')
            .writeAsBytes(bytes!.buffer.asUint8List());
        image.dispose();
      }
    });
  });
}

class _InstallArrow extends CustomPainter {
  @override
  void paint(Canvas canvas, Size size) {
    final paint = Paint()
      ..color = WispColors.cyan.withValues(alpha: .9)
      ..style = PaintingStyle.stroke
      ..strokeWidth = 2.5
      ..strokeCap = StrokeCap.round
      ..strokeJoin = StrokeJoin.round;
    canvas.drawPath(
      Path()
        ..moveTo(278, 232)
        ..cubicTo(326, 211, 394, 211, 442, 232),
      paint,
    );
    canvas.drawPath(
      Path()
        ..moveTo(430, 216)
        ..lineTo(442, 232)
        ..lineTo(422, 234),
      paint,
    );
  }

  @override
  bool shouldRepaint(_InstallArrow oldDelegate) => false;
}
