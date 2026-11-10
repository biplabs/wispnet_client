import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/sheets.dart';

class LinksBackend implements WispnetBackend {
  LinksBackend(this.url);
  final String url;
  final calls = <String>[];
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    calls.add(method);
    return {'url': url};
  }
}

void main() {
  testWidgets('TCP details open a node forward and expose its local endpoint', (
    tester,
  ) async {
    final backend = LinksBackend('tcp://127.0.0.1:32100');
    final service = {
      'id': 'database',
      'kind': 'tcp',
      'enabled': true,
      'incarnation': 'c' * 64,
    };
    final machine = Machine('a' * 64, {'state': 'available'});
    expect(machine.canOpen(service), isTrue);
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ServiceLinks(
            backend: backend,
            machine: machine,
            service: service,
            canResolve: true,
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Local TCP address'), findsOneWidget);
    expect(find.text('tcp://127.0.0.1:32100'), findsOneWidget);
    expect(backend.calls, ['open']);
  });

  testWidgets('HTTP details include the equivalent WebSocket address', (
    tester,
  ) async {
    final backend = LinksBackend('http://website.device.localhost:7842/');
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: ServiceLinks(
            backend: backend,
            machine: Machine('a' * 64, {}),
            service: {'id': 'website', 'kind': 'http'},
            canResolve: true,
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('WebSocket URL'), findsOneWidget);
    expect(find.text('ws://website.device.localhost:7842/'), findsOneWidget);
  });
}
