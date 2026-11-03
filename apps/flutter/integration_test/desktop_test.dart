import 'dart:io';
import 'dart:convert';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/cards.dart';
import 'package:wispnet/src/desktop_backend.dart';
import 'package:wispnet/ui.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  testWidgets(
    'desktop uses shared directory, owner sheets and real browser handoff',
    (tester) async {
      final native = DesktopBackend();
      final backend = _ObservedBackend(native);
      final before = await native.invoke('snapshot');
      expect(
        before['enabled'],
        true,
        reason: 'Run with the existing node online',
      );
      final local = (before['hosts'] as Map)[before['peer']] as Map;
      final services = (local['catalog']['services'] as List)
          .where((s) => s['kind'] == 'http')
          .toList();
      expect(
        services,
        isNotEmpty,
        reason: 'A running HTTP service is required for live handoff',
      );
      final httpServiceId = services.first['id'] as String;
      final key = GlobalKey();
      Future<void> waitFor(Finder finder) async {
        final deadline = DateTime.now().add(const Duration(seconds: 30));
        while (finder.evaluate().isEmpty && DateTime.now().isBefore(deadline)) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        expect(finder, findsWidgets);
      }

      Future<void> capture(String name) async {
        await tester.pump();
        await tester.pump(const Duration(milliseconds: 300));
        final image =
            await (key.currentContext!.findRenderObject()!
                    as RenderRepaintBoundary)
                .toImage();
        final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
        final directory = Directory('/tmp/wispnet-flutter-desktop-review')
          ..createSync(recursive: true);
        await File('${directory.path}/$name.png')
            .writeAsBytes(bytes!.buffer.asUint8List());
        image.dispose();
      }

      try {
        await tester.pumpWidget(
          RepaintBoundary(
            key: key,
            child: WispnetApp(backend: backend),
          ),
        );
        await waitFor(find.text('REMOTE SERVICES'));
        await tester.pump(const Duration(seconds: 3));
        await capture('directory');
        final mouse = await tester.createGesture(
          kind: ui.PointerDeviceKind.mouse,
        );
        await mouse.addPointer(location: Offset.zero);
        await mouse.moveTo(tester.getCenter(find.byType(OnlineButton)));
        await capture('hover-online');
        await mouse.moveTo(tester.getCenter(find.text('Manage').first));
        await capture('hover-manage');
        final serviceTile = find
            .descendant(
              of: find.byKey(const ValueKey('local-sector')),
              matching: find.byType(WispInkWell),
            )
            .first;
        await mouse.moveTo(tester.getCenter(serviceTile));
        await capture('hover-service');
        final machineName = find
            .descendant(
              of: find
                  .byWidgetPredicate(
                    (widget) => widget is MachineCard && widget.showHeader,
                  )
                  .first,
              matching: find.byType(WispInkWell),
            )
            .first;
        await mouse.moveTo(tester.getCenter(machineName));
        await capture('hover-machine-name');
        await mouse.moveTo(Offset.zero);
        await mouse.removePointer();
        await tester.tap(machineName);
        await waitFor(find.textContaining('Device ID\n'));
        expect(find.widgetWithText(TextField, 'Display name'), findsNothing);
        await capture('machine-settings');
        expect(find.widgetWithText(OutlinedButton, 'Refresh'), findsNothing);
        await tester.tap(find.widgetWithText(OutlinedButton, 'Remove'));
        await waitFor(find.text('Remove device?'));
        await capture('remove-machine-confirmation');
        await tester.tap(find.text('Cancel'));
        await tester.pump(const Duration(milliseconds: 500));
        await tester.tap(find.byTooltip('Close').last);
        await tester.pump(const Duration(milliseconds: 500));
        await tester.tap(
          find
              .descendant(
                of: find.byKey(const ValueKey('remote-sector')),
                matching: find.byType(OutlinedButton),
              )
              .first,
        );
        await waitFor(find.text('+ Pair another device'));
        await tester.tap(find.text('+ Pair another device'));
        await waitFor(find.text('Pair a device'));
        expect(find.text('Pairing Code'), findsOneWidget);
        expect(find.text('QR'), findsOneWidget);
        expect(find.text('Import QR image'), findsOneWidget);
        await capture('pairing-qr');
        await tester.tap(find.text('Pairing Code'));
        await tester.pump(const Duration(milliseconds: 300));
        await capture('pairing-code');
        await tester.tap(find.byTooltip('Close').last);
        await tester.pump(const Duration(milliseconds: 500));
        await tester.tap(find.byTooltip('Settings'));
        await waitFor(find.text('Local services'));
        await tester.ensureVisible(find.text('Local services'));
        await tester.tap(find.text('Local services'));
        await waitFor(find.byKey(const ValueKey('share-service')));
        expect(find.text('Pair this device'), findsNothing);
        expect(find.text('Remove service'), findsNothing);
        expect(find.text('Enable public access'), findsNothing);
        await capture('local-services');
        await tester.ensureVisible(find.byKey(const ValueKey('share-service')));
        await tester.tap(find.byKey(const ValueKey('share-service')));
        await waitFor(find.widgetWithText(Tab, 'Custom'));
        await tester.tap(find.widgetWithText(Tab, 'Custom'));
        await waitFor(find.widgetWithText(TextField, 'Service ID'));
        await tester.enterText(
          find.widgetWithText(TextField, 'Service ID'),
          'local_model_api',
        );
        await tester.pump();
        expect(
          tester
              .widget<TextField>(find.widgetWithText(TextField, 'Service ID'))
              .controller!
              .text,
          'local_model_api',
        );
        expect(
          tester
              .widget<TextField>(find.widgetWithText(TextField, 'Port'))
              .controller!
              .text,
          '8080',
        );
        expect(
          tester
              .widget<DropdownButtonFormField<String>>(
                find.byType(DropdownButtonFormField<String>),
              )
              .decoration
              .labelText,
          'Protocol',
        );
        await capture('add-service-form');
        await tester.tap(find.byTooltip('Back').last);
        await tester.pump(const Duration(milliseconds: 500));
        expect(find.byKey(const ValueKey('share-service')), findsOneWidget);
        await tester.tap(find.byTooltip('Back').last);
        await tester.pump(const Duration(milliseconds: 500));
        await tester.ensureVisible(find.text('Device access and pairing'));
        await tester.tap(find.text('Device access and pairing'));
        await waitFor(find.text('Pair this device'));
        expect(
          find.text('Device access and pairing').hitTestable(),
          findsOneWidget,
        );
        expect(find.text('Device and browser settings'), findsNothing);
        await tester.tap(find.text('Pair this device'));
        await waitFor(find.byKey(const ValueKey('pairing-qr')));
        expect(find.text('Copy pairing code'), findsNothing);
        await capture('pair-this-machine-qr');
        await tester.tap(find.text('Pairing code'));
        await waitFor(find.byKey(const ValueKey('pairing-code')));
        expect(find.text('Copy pairing code'), findsOneWidget);
        expect(
          tester
              .widget<SelectableText>(
                find.byKey(const ValueKey('pairing-code')),
              )
              .data,
          startsWith('wispnet1:'),
        );
        await capture('pair-this-machine-code');
        await tester.tap(find.byTooltip('Back').last);
        await tester.pump(const Duration(milliseconds: 500));
        expect(find.text('Pair this device'), findsOneWidget);
        await tester.tap(find.byTooltip('Back').last);
        await tester.pump(const Duration(milliseconds: 500));
        expect(find.widgetWithText(TextField, 'Wispnet port'), findsOneWidget);
        await tester.tap(find.byTooltip('Close').last);
        await tester.pump(const Duration(milliseconds: 500));
        final serviceSettings = find.descendant(
          of: find.byKey(const ValueKey('local-sector')),
          matching: find.byWidgetPredicate(
            (widget) =>
                widget is IconButton &&
                (widget.tooltip?.startsWith(
                      'Settings for $httpServiceId on ',
                    ) ??
                    false),
          ),
        );
        await tester.ensureVisible(serviceSettings);
        await tester.tap(serviceSettings);
        await waitFor(find.text('Remove service'));
        expect(find.byKey(const ValueKey('share-service')), findsNothing);
        await capture('local-service-controls');
        expect(find.textContaining('127.0.0.1:'), findsNothing);
        backend.launched = null;
        await tester.tap(find.byTooltip('Open local service'));
        for (var i = 0; i < 100 && backend.launched == null; i++) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        expect(Uri.parse(backend.launched!).host, endsWith('.localhost'));
        backend.launched = null;
        if (find.byTooltip('Open public service').evaluate().isNotEmpty) {
          final publicLink = find.descendant(
            of: find.byTooltip('Open public service'),
            matching: find.byType(WispInkWell),
          );
          for (
            var i = 0;
            i < 100 && tester.widget<WispInkWell>(publicLink).onTap == null;
            i++
          ) {
            await tester.pump(const Duration(milliseconds: 100));
            await tester.runAsync(
              () => Future<void>.delayed(const Duration(milliseconds: 100)),
            );
          }
          expect(tester.widget<WispInkWell>(publicLink).onTap, isNotNull);
          await tester.tap(publicLink);
          for (var i = 0; i < 100 && backend.launched == null; i++) {
            await tester.pump(const Duration(milliseconds: 100));
            await tester.runAsync(
              () => Future<void>.delayed(const Duration(milliseconds: 100)),
            );
          }
          expect(backend.launched, startsWith('https://'));
          backend.launched = null;
        }

        final remove = find.widgetWithText(OutlinedButton, 'Remove service');
        for (
          var i = 0;
          i < 100 && tester.widget<OutlinedButton>(remove).onPressed == null;
          i++
        ) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        await tester.ensureVisible(find.text('Remove service').first);
        await tester.tap(find.text('Remove service').first);
        await waitFor(find.text('Remove service?'));
        await capture('remove-service-confirmation');
        await tester.tap(find.text('Cancel'));
        await tester.pump(const Duration(milliseconds: 500));
        final disabling = find
            .text('Disable public access')
            .evaluate()
            .isNotEmpty;
        final publicAction = disabling
            ? 'Disable public access'
            : 'Enable public access';
        await tester.ensureVisible(find.text(publicAction).first);
        await tester.tap(find.text(publicAction).first);
        await waitFor(
          find.text(
            disabling
                ? 'Disable public access?'
                : 'Expose service to the internet?',
          ),
        );
        if (!disabling) {
          expect(
            find.textContaining('unprotected to the public internet'),
            findsOneWidget,
          );
        }
        await capture('public-service-confirmation');
        await tester.tap(find.text('Cancel'));
        await tester.pump(const Duration(milliseconds: 500));
        expect(find.text('Device and browser settings'), findsNothing);
        await tester.tap(find.byTooltip('Close').last);
        for (
          var i = 0;
          i < 15 && find.byTooltip('Close').evaluate().isNotEmpty;
          i++
        ) {
          await tester.pump(const Duration(milliseconds: 100));
        }
        await tester.tap(find.byTooltip('Settings'));
        await waitFor(find.widgetWithText(TextField, 'Wispnet port'));
        expect(find.text('Device and browser settings'), findsNothing);
        expect(find.byTooltip('Close'), findsOneWidget);
        await capture('settings');
        await tester.pump(const Duration(seconds: 1));
        expect(
          tester
              .widget<TextField>(find.widgetWithText(TextField, 'Wispnet port'))
              .controller!
              .text,
          isNotEmpty,
        );
        await capture('browser-settings');
        await tester.ensureVisible(
          find.byKey(const ValueKey('settings-address')),
        );
        await tester.tap(find.byKey(const ValueKey('settings-address')));
        for (var i = 0; i < 100 && backend.launched == null; i++) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        expect(backend.launched, endsWith('/registry'));
        expect(Uri.parse(backend.launched!).host, 'wispnet.localhost');
        await tester.runAsync(() async {
          final client = HttpClient();
          try {
            final uri = Uri.parse(backend.launched!);
            // The local registry listens on IPv4; this hostname also resolves to ::1.
            final request = await client.getUrl(uri.replace(host: '127.0.0.1'));
            request.headers.set(HttpHeaders.hostHeader, uri.authority);
            request.headers.set('Sec-Fetch-Site', 'none');
            request.headers.set('Sec-Fetch-Mode', 'navigate');
            request.headers.set('Sec-Fetch-Dest', 'document');
            final response = await request.close();
            expect(response.statusCode, 200);
            expect(response.headers.contentType?.mimeType, 'application/json');
            final json =
                jsonDecode(await utf8.decoder.bind(response).join()) as Map;
            expect(json['api_version'], 1);
            expect(json['services'], isA<List>());
            for (final service in json['services'] as List) {
              expect(service['url'], isA<String>(), reason: service.toString());
            }
          } finally {
            client.close(force: true);
          }
        });
        backend.launched = null;

        Future<void> closeSheet(int remaining) async {
          await tester.tap(find.byTooltip('Close').last);
          for (var i = 0; i < 15; i++) {
            await tester.pump(const Duration(milliseconds: 100));
            if (find.byTooltip('Close').evaluate().length == remaining) break;
          }
          expect(find.byTooltip('Close').evaluate().length, remaining);
        }

        await closeSheet(0);
        await tester.tap(find.byTooltip('Go offline'));
        await waitFor(find.byTooltip('Enter wisp space'));
        expect((await native.invoke('snapshot'))['enabled'], false);
        expect(find.text('hyperjump to wispnet'), findsOneWidget);
        expect(find.byTooltip('Settings'), findsNothing);
        await capture('launch');
        await tester.tap(find.byTooltip('Enter wisp space'));
        await waitFor(find.text('REMOTE SERVICES'));
        expect((await native.invoke('snapshot'))['peer'], before['peer']);
        for (
          var n = 0;
          n < 100 &&
              tester
                      .widget<OnlineButton>(find.byType(OnlineButton))
                      .onPressed ==
                  null;
          n++
        ) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 20)),
          );
        }
        expect(
          tester.widget<OnlineButton>(find.byType(OnlineButton)).onPressed,
          isNotNull,
        );
        tester.binding.handleAppLifecycleStateChanged(
          AppLifecycleState.resumed,
        );
        final localService = find.descendant(
          of: find.byKey(const ValueKey('local-sector')),
          matching: find.text(httpServiceId),
        );
        await tester.ensureVisible(localService);
        await tester.tap(localService);
        for (var i = 0; i < 100 && backend.launched == null; i++) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
        }
        expect(backend.launched, isNotNull, reason: backend.calls.join(', '));
        await tester.runAsync(() async {
          final client = HttpClient();
          try {
            final uri = Uri.parse(backend.launched!);
            final request = await client.getUrl(
              uri.host.endsWith('.localhost')
                  ? uri.replace(host: '127.0.0.1')
                  : uri,
            );
            request.headers.set(HttpHeaders.hostHeader, uri.authority);
            final response = await request.close();
            expect(response.statusCode, lessThan(500));
            await response.drain<void>();
          } finally {
            client.close(force: true);
          }
        });
        final after = await native.invoke('snapshot');
        expect(after['peer'], before['peer']);
        expect(
          (after['hosts'] as Map).keys.toSet(),
          (before['hosts'] as Map).keys.toSet(),
        );
        expect(tester.takeException(), isNull);
      } finally {
        await tester.pumpWidget(const SizedBox());
        if ((await native.invoke('snapshot'))['enabled'] != before['enabled']) {
          await native.invoke('setAccess', {'enabled': before['enabled']});
        }
        await native.close();
      }
    },
  );
}

class _ObservedBackend implements WispnetBackend {
  _ObservedBackend(this.native);
  final DesktopBackend native;
  String? launched;
  final calls = <String>[];
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    calls.add(method);
    final result = await native.invoke(method, arguments);
    if (method == 'launch') launched = arguments['url'] as String;
    if (method == 'launchPublic') launched = result['url'] as String;
    return result;
  }
}
