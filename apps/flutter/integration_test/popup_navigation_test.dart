import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/desktop_backend.dart';

/// Read-only navigation against the already-online installed desktop node.
/// The adapter rejects writes, service launches, camera and terminal operations.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  testWidgets('native Settings pages distinguish Back from closing a popup', (
    tester,
  ) async {
    final native = DesktopBackend();
    final backend = _ReadOnlyBackend(native);
    final before = await native.invoke('snapshot');
    expect(before['enabled'], true, reason: 'Run with the node already online');
    final ownerBefore = await native.invoke('ownerStatus');
    final key = GlobalKey();

    Future<void> waitFor(Finder finder) async {
      final deadline = DateTime.now().add(const Duration(seconds: 25));
      while (finder.evaluate().isEmpty && DateTime.now().isBefore(deadline)) {
        await tester.pump(const Duration(milliseconds: 100));
        await tester.runAsync(
          () => Future<void>.delayed(const Duration(milliseconds: 100)),
        );
      }
      expect(finder, findsWidgets);
      await tester.pump(const Duration(milliseconds: 300));
    }

    Future<void> capture(String name) async {
      await tester.pump();
      final rendered =
          await (key.currentContext!.findRenderObject()!
                  as RenderRepaintBoundary)
              .toImage();
      final bytes = await rendered.toByteData(format: ui.ImageByteFormat.png);
      final output = Directory('/tmp/wispnet-popup')
        ..createSync(recursive: true);
      await File('${output.path}/$name.png')
          .writeAsBytes(bytes!.buffer.asUint8List());
      rendered.dispose();
    }

    Future<void> ownerPage() async {
      await tester.ensureVisible(find.text('Device access and pairing'));
      await tester.tap(find.text('Device access and pairing'));
      await waitFor(find.text('Pair this device'));
      expect(
        find.text('Device access and pairing').hitTestable(),
        findsOneWidget,
      );
      expect(find.text('Device and browser settings'), findsNothing);
      expect(find.byTooltip('Back').hitTestable(), findsOneWidget);
      expect(find.byTooltip('Close').hitTestable(), findsOneWidget);
    }

    try {
      await tester.pumpWidget(
        RepaintBoundary(
          key: key,
          child: WispnetApp(backend: backend),
        ),
      );
      await waitFor(find.byTooltip('Settings'));
      await tester.tap(find.byTooltip('Settings'));
      await waitFor(find.widgetWithText(TextField, 'Wispnet port'));
      await ownerPage();
      await capture('desktop-owner-navigation');
      await tester.tap(find.byTooltip('Back').last);
      await waitFor(
        find.widgetWithText(TextField, 'Wispnet port').hitTestable(),
      );
      expect(find.text('Pair this device'), findsNothing);
      await ownerPage();
      await tester.tap(find.byTooltip('Close').last);
      await tester.pump(const Duration(milliseconds: 500));
      expect(find.byTooltip('Close'), findsNothing);
      expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);

      await tester.tap(find.byTooltip('Settings'));
      await waitFor(find.text('Local services'));
      await tester.ensureVisible(find.text('Local services'));
      await tester.tap(find.text('Local services'));
      await waitFor(find.byKey(const ValueKey('share-service')));
      await tester.ensureVisible(find.byKey(const ValueKey('share-service')));
      await tester.tap(find.byKey(const ValueKey('share-service')));
      await waitFor(find.widgetWithText(Tab, 'Custom'));
      expect(find.text('Share a service').hitTestable(), findsOneWidget);
      expect(find.byTooltip('Back').hitTestable(), findsOneWidget);
      expect(find.byTooltip('Close').hitTestable(), findsOneWidget);
      await capture('desktop-share-service-navigation');
      await tester.tap(find.byTooltip('Back').last);
      await waitFor(find.byKey(const ValueKey('share-service')).hitTestable());
      expect(find.widgetWithText(Tab, 'Custom'), findsNothing);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pump(const Duration(milliseconds: 500));
      expect(find.byTooltip('Close'), findsNothing);
      expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);

      final after = await native.invoke('snapshot');
      final ownerAfter = await native.invoke('ownerStatus');
      expect(after['peer'], before['peer']);
      expect(after['enabled'], before['enabled']);
      expect(
        (after['hosts'] as Map).keys.toSet(),
        (before['hosts'] as Map).keys.toSet(),
      );
      expect(ownerAfter['host']['services'], ownerBefore['host']['services']);
      expect(backend.rejected, isEmpty);
      expect(tester.takeException(), isNull);
    } finally {
      await tester.pumpWidget(const SizedBox());
      await native.close();
    }
  });
}

class _ReadOnlyBackend implements WispnetBackend {
  _ReadOnlyBackend(this.native);
  final DesktopBackend native;
  final rejected = <String>[];

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    // Leave pending OS links and invitations untouched during this smoke test.
    if (method == 'takeLink' || method == 'takeInvitation') return {};
    if (!{
          'snapshot',
          'refreshHost',
          'settings',
          'ownerStatus',
          'ownerData',
        }.contains(method) ||
        method == 'ownerData' && arguments['op'] != 'operations') {
      rejected.add(method);
      throw StateError('Read-only navigation test rejected $method');
    }
    return native.invoke(method, arguments);
  }
}
