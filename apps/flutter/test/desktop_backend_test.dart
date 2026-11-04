import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/desktop_backend.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  tearDown(() {
    TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
        .setMockMethodCallHandler(DesktopBackend.shell, null);
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
