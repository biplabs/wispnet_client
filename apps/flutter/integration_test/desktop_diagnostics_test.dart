import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/desktop_backend.dart';
import 'package:wispnet/src/sheets.dart';

/// Read-only native smoke test against one already paired device.
void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  testWidgets(
    'desktop settings opens shared diagnostics and runs a native probe',
    (tester) async {
      final native = DesktopBackend();
      final before = await native.invoke('snapshot');
      expect(
        before['enabled'],
        true,
        reason: 'Run with the existing node online',
      );
      final hosts = (before['hosts'] as Map).cast<String, dynamic>();
      final remote = hosts.entries
          .where((entry) => (entry.value as Map)['local'] != true)
          .toList();
      expect(
        remote,
        isNotEmpty,
        reason: 'An already paired device is required',
      );
      final selected =
          remote
              .where((entry) => entry.value['name'] == 'hibreak')
              .firstOrNull ??
          remote.first;
      final backend = _OnePeerBackend(native, selected.key);
      final key = GlobalKey();
      Future<void> waitFor(Finder finder, {int seconds = 20}) async {
        final clock = Stopwatch()..start();
        while (finder.evaluate().isEmpty &&
            clock.elapsed < Duration(seconds: seconds)) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        expect(finder, findsWidgets);
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
        await waitFor(find.text('Connections'));
        await tester.ensureVisible(find.text('Connections'));
        await tester.tap(find.text('Connections'));
        await tester.pumpAndSettle();
        await tester.tap(find.text('Connection diagnostics'));
        await waitFor(find.byType(ConnectionDiagnostics));
        await waitFor(find.text('Run again'), seconds: 80);
        expect(backend.tested, [selected.key]);
        expect(backend.result?['peers'], isNotEmpty);
        final peer = (backend.result!['peers'] as List).single as Map;
        expect(peer['host'], selected.key);
        expect(peer['report'], isA<Map>(), reason: peer['error']?.toString());
        expect((peer['report'] as Map)['selected_path'], isA<String>());
        expect(tester.takeException(), isNull);
        await tester.pump();
        final rendered =
            await (key.currentContext!.findRenderObject()!
                    as RenderRepaintBoundary)
                .toImage();
        final data = await rendered.toByteData(format: ui.ImageByteFormat.png);
        final output = Directory('/tmp/wispnet-ui-gaps')
          ..createSync(recursive: true);
        await File('${output.path}/desktop-diagnostics.png')
            .writeAsBytes(data!.buffer.asUint8List());
        rendered.dispose();
        expect((await native.invoke('snapshot'))['peer'], before['peer']);
      } finally {
        await tester.pumpWidget(const SizedBox());
        await native.close();
      }
    },
  );
}

class _OnePeerBackend implements WispnetBackend {
  _OnePeerBackend(this.native, this.peer);
  final DesktopBackend native;
  final String peer;
  final tested = <String>[];
  Map<String, dynamic>? result;
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    final response = await native.invoke(method, arguments);
    if (method == 'snapshot') {
      return {
        ...response,
        'hosts': {
          for (final entry in (response['hosts'] as Map).entries)
            if (entry.key == response['peer'] || entry.key == peer)
              entry.key: entry.value,
        },
      };
    }
    if (method == 'connectionDiagnostics') {
      tested.add(arguments['host'] as String);
      result = response;
    }
    return response;
  }
}
