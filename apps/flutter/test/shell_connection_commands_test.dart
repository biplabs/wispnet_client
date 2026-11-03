import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/cards.dart';

void main() {
  testWidgets('checks installed bridge and existing host alias before setup', (
    tester,
  ) async {
    var checks = 0;
    await tester.pumpWidget(
      MaterialApp(
        home: Builder(
          builder: (context) => Scaffold(
            body: TextButton(
              onPressed: () => showShellCommands(
                context,
                Machine('a' * 64, {'name': 'j-desktop'}),
                onStatus: () async {
                  checks++;
                  return {
                    'installed': true,
                    'alias_configured': true,
                    'command': 'ssh shell-j-desktop.localhost',
                  };
                },
                onSetup: () async =>
                    throw StateError('Should not repeat setup'),
              ),
              child: const Text('Connections'),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('Connections'));
    await tester.pumpAndSettle();
    expect(checks, 1);
    expect(find.textContaining('SSH bridge installed'), findsNothing);
    expect(find.textContaining('--print-config'), findsNothing);
    expect(find.byType(FilledButton), findsNothing);
  });
  testWidgets(
    'SSH setup is explicit and displays the installed short command',
    (tester) async {
      var calls = 0;
      await tester.pumpWidget(
        MaterialApp(
          home: Builder(
            builder: (context) => Scaffold(
              body: TextButton(
                onPressed: () => showShellCommands(
                  context,
                  Machine('a' * 64, {'name': 'j-desktop'}),
                  onSetup: () async {
                    calls++;
                    return 'ssh shell-j-desktop.localhost';
                  },
                ),
                child: const Text('Connections'),
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('Connections'));
      await tester.pumpAndSettle();
      expect(calls, 0);
      expect(find.textContaining('one installation'), findsNothing);
      await tester.ensureVisible(find.text('Add SSH alias'));
      await tester.tap(find.text('Add SSH alias'));
      await tester.pumpAndSettle();
      expect(calls, 1);
      expect(find.text('SSH alias added'), findsNothing);
      expect(find.text('Add SSH alias'), findsNothing);
      expect(find.text('ssh shell-j-desktop.localhost'), findsOneWidget);
    },
  );
}
