import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';

void main() {
  final binding = IntegrationTestWidgetsFlutterBinding.ensureInitialized();
  testWidgets(
    'iOS native identity, pairing, services and responsive directory',
    (tester) async {
      final backend = PlatformBackend();
      final before = await backend.invoke('snapshot');
      expect(before['platform'], 'ios');
      expect(before['peer'], matches(RegExp(r'^[0-9a-f]{64}$')));
      final invitation = await backend.invoke('ownerRequest', {
        'payload': {'op': 'invite_qr'},
      });
      expect(invitation['matrix'], isNotEmpty);
      final preview = await backend.invoke('preview', {
        'code': invitation['code'],
      });
      expect(preview['host'], before['peer']);

      final settings = await backend.invoke('settings');
      var restoreSettings = true;
      if (before['enabled'] != true) {
        try {
          final original = await ServerSocket.bind(
            InternetAddress.loopbackIPv4,
            settings['port'] as int,
          );
          await original.close();
        } on SocketException {
          // Simulators share the Mac's loopback; retain a usable fixture port
          // when the running desktop app owns the original default.
          restoreSettings = false;
        }
      }
      final reservation = await ServerSocket.bind(
        InternetAddress.loopbackIPv4,
        0,
      );
      final registryPort = reservation.port;
      await reservation.close();
      await backend.invoke('saveSettings', {
        'port': registryPort,
        'name': before['name'],
      });
      final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
      server.listen((request) async {
        request.response.headers.contentType = ContentType.json;
        request.response.write(
          jsonEncode({'device': 'ios', 'path': request.uri.path}),
        );
        await request.response.close();
      });
      const service = 'ios-integration';
      var created = false;
      try {
        var owner = await backend.invoke('ownerStatus');
        if ((owner['host']['services'] as List).any(
          (dynamic row) => row['id'] == service,
        )) {
          await backend.invoke('ownerRequest', {
            'payload': {
              'op': 'mutate',
              'operation': {
                'op': 'remove',
                'id': service,
                'expected_revision': owner['host']['revision'],
              },
            },
          });
          owner = await backend.invoke('ownerStatus');
        }
        await backend.invoke('ownerRequest', {
          'payload': {
            'op': 'mutate',
            'operation': {
              'op': 'service',
              'expected_revision': owner['host']['revision'],
              'id': service,
              'kind': 'http',
              'backend': '127.0.0.1:${server.port}',
              'allow': <String>[],
            },
          },
        });
        created = true;
        await backend.invoke('setAccess', {'enabled': true});
        final online = await backend.invoke('snapshot');
        expect(online['enabled'], true);
        final local = (online['hosts'] as Map)[online['peer']] as Map;
        final tile = (local['catalog']['services'] as List).singleWhere(
          (dynamic row) => row['id'] == service,
        ) as Map;
        final opened = await backend.invoke('open', {
          'host': online['peer'],
          'service': service,
          'incarnation': tile['incarnation'],
        });
        final url = Uri.parse(opened['url'] as String);
        expect(url.host, '127.0.0.1');
        final client = HttpClient();
        try {
          final request = await client.getUrl(url.resolve('/probe'));
          final response = await request.close();
          expect(response.statusCode, 200);
          expect(jsonDecode(await utf8.decoder.bind(response).join()), {
            'device': 'ios',
            'path': '/probe',
          });
        } finally {
          client.close(force: true);
        }
        await tester.pumpWidget(WispnetApp(backend: backend));
        for (var i = 0; i < 30; i++) {
          await tester.pump(const Duration(milliseconds: 100));
          await tester.runAsync(
            () => Future<void>.delayed(const Duration(milliseconds: 100)),
          );
          if (find.text('REMOTE SERVICES').evaluate().isNotEmpty) break;
        }
        expect(find.text('REMOTE SERVICES'), findsOneWidget);
        expect(tester.takeException(), isNull);
        await binding.takeScreenshot('ios-directory');
        await tester.tap(find.byTooltip('Settings').first);
        await tester.pump(const Duration(milliseconds: 600));
        expect(tester.takeException(), isNull);
        await binding.takeScreenshot('ios-settings');
        await tester.pumpWidget(const SizedBox.shrink());
        await tester.pump(const Duration(milliseconds: 600));
      } finally {
        if (created) {
          final owner = await backend.invoke('ownerStatus');
          await backend.invoke('ownerRequest', {
            'payload': {
              'op': 'mutate',
              'operation': {
                'op': 'remove',
                'id': service,
                'expected_revision': owner['host']['revision'],
              },
            },
          });
        }
        await backend.invoke('setAccess', {
          'enabled': before['enabled'] == true,
        });
        if (restoreSettings) {
          await backend.invoke('saveSettings', {
            'port': settings['port'],
            'name': before['name'],
          });
        }
        await server.close(force: true);
      }
      final after = await backend.invoke('snapshot');
      expect(after['peer'], before['peer']);
    },
  );
}
