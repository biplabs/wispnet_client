import 'dart:io';

import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/desktop_backend.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(DesktopBackend.shell, null);
  });

  test('tray reports the relay offline when access is disabled', () async {
    final directory = await Directory.systemTemp.createTemp(
      'wispnet-tray-test',
    );
    addTearDown(() => directory.delete(recursive: true));
    final helper = await File('${directory.path}/wispnet').writeAsString(
      r'''
#!/bin/sh
read -r request || exit
printf '%s\n' '{"id":1,"result":{}}'
read -r request || exit
printf '%s\n' '{"id":2,"result":{"enabled":false,"relay":"connected"}}'
read -r request || exit
printf '%s\n' '{"id":3,"result":{"enabled":true,"relay":"connected"}}'
read -r request
'''
          .trimLeft(),
    );
    final chmod = await Process.run('chmod', ['700', helper.path]);
    expect(chmod.exitCode, 0);
    final statuses = <Map<Object?, Object?>>[];
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(DesktopBackend.shell, (call) async {
          expect(call.method, 'status');
          statuses.add(call.arguments as Map<Object?, Object?>);
          return null;
        });
    final backend = DesktopBackend(helper: helper.path);
    addTearDown(backend.close);

    await backend.invoke('snapshot');
    expect(statuses.last['online'], false);
    expect(statuses.last['text'], endsWith('Relay: offline'));

    await backend.invoke('snapshot');
    expect(statuses.last['online'], true);
    expect(statuses.last['text'], endsWith('Relay: connected'));
  });

  test(
    'folder picker uses the native desktop window and returns its path',
    () async {
      final calls = <MethodCall>[];
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(DesktopBackend.shell, (call) async {
            calls.add(call);
            return '/home/receiver/Chosen folder';
          });
      final backend = DesktopBackend(helper: '/missing-wispnet');
      expect(
        await backend.invoke('pickFolder', {
          'initialPath': '/home/receiver/Downloads',
        }),
        {'path': '/home/receiver/Chosen folder'},
      );
      expect(calls.single.method, 'pickFolder');
      expect(calls.single.arguments, {
        'initialPath': '/home/receiver/Downloads',
      });
      await backend.close();
    },
  );

  test('cancelled folder picker returns no selection', () async {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(DesktopBackend.shell, (_) async => null);
    final backend = DesktopBackend(helper: '/missing-wispnet');
    expect(await backend.invoke('pickFolder'), isEmpty);
    await backend.close();
  });

  test(
    'native folder errors reach the caller without selecting a path',
    () async {
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
          .setMockMethodCallHandler(DesktopBackend.shell, (_) async {
            throw PlatformException(
              code: 'picker_busy',
              message: 'A folder picker is already open.',
            );
          });
      final backend = DesktopBackend(helper: '/missing-wispnet');
      await expectLater(
        backend.invoke('pickFolder'),
        throwsA(
          isA<PlatformException>().having(
            (error) => error.code,
            'code',
            'picker_busy',
          ),
        ),
      );
      await backend.close();
    },
  );
}
