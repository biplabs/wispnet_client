import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/service_settings.dart';

import 'widget_test.dart' as fixture;

class OwnerFeaturesBackend extends fixture.DesktopFake {
  OwnerFeaturesBackend(this.android) {
    if (android) {
      (status['host']['services'] as List).removeWhere(
        (service) => service['kind'] == 'shell',
      );
    }
  }
  final bool android;

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    final result = await super.invoke(method, arguments);
    if (method == 'snapshot') return {...result, 'desktop': !android};
    return result;
  }
}

class DeferredOwnerBackend extends OwnerFeaturesBackend {
  DeferredOwnerBackend(super.android);
  final submitted = Completer<void>();
  final completed = Completer<void>();

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'ownerRequest' &&
        arguments['payload']?['operation']?['op'] == 'approve_invitation') {
      submitted.complete();
      await completed.future;
    }
    return super.invoke(method, arguments);
  }
}

Future<void> tapHeader(WidgetTester tester, String tooltip) async {
  final button = find.byTooltip(tooltip).last;
  await tester.ensureVisible(button);
  await tester.tap(button);
  await tester.pumpAndSettle();
}

Future<void> openInvitation(WidgetTester tester) async {
  final button = find.widgetWithText(OutlinedButton, 'Pair this device');
  await tester.ensureVisible(button);
  await tester.tap(button);
  await tester.pumpAndSettle();
  expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
}

Future<void> mountOwner(
  WidgetTester tester,
  OwnerFeaturesBackend backend, {
  bool disableAnimations = true,
}) => fixture.mount(
  tester,
  backend,
  disableAnimations: disableAnimations,
  size: backend.android ? const Size(390, 844) : const Size(1000, 850),
);

Future<void> openOwnerTools(WidgetTester tester) async {
  await tester.tap(find.byTooltip('Settings'));
  await tester.pumpAndSettle();
  final tools = find.text('Device access and pairing');
  await tester.ensureVisible(tools);
  await tester.tap(tools);
  await tester.pumpAndSettle();
}

void main() {
  setUpAll(() async {
    await (FontLoader('Roboto')
          ..addFont(rootBundle.load('assets/fonts/Roboto-Regular.ttf'))
          ..addFont(rootBundle.load('assets/fonts/Roboto-Medium.ttf'))
          ..addFont(rootBundle.load('assets/fonts/Roboto-Bold.ttf')))
        .load();
    await (FontLoader(
      'MaterialIcons',
    )..addFont(rootBundle.load('fonts/MaterialIcons-Regular.otf'))).load();
  });
  for (final android in [false, true]) {
    final platform = android ? 'Android' : 'desktop';
    testWidgets(
      '$platform owner title is specific, Back returns to Settings and X closes the popup',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await openOwnerTools(tester);
        expect(
          find.text('Paired devices').hitTestable(),
          findsOneWidget,
        );
        expect(find.text('Device and browser settings'), findsNothing);
        expect(find.text('+ Pair another device'), findsOneWidget);
        expect(find.text('Pair this device'), findsOneWidget);
        await tapHeader(tester, 'Back');
        expect(find.text('Settings'), findsOneWidget);
        expect(find.widgetWithText(TextField, 'Device name'), findsOneWidget);
        expect(find.widgetWithText(TextField, 'Wispnet port'), findsOneWidget);
        expect(find.byType(ServiceSettings), findsNothing);

        final tools = find.text('Device access and pairing');
        await tester.ensureVisible(tools);
        await tester.tap(tools);
        await tester.pumpAndSettle();
        await tapHeader(tester, 'Close');
        expect(find.byType(ServiceSettings), findsNothing);
        expect(find.widgetWithText(TextField, 'Device name'), findsNothing);
        expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);
        await tester.tap(find.byTooltip('Settings'));
        await tester.pumpAndSettle();
        expect(find.byTooltip('Back'), findsNothing);
        expect(tester.takeException(), isNull);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform invitation Back follows the popup history and X closes every page',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await openOwnerTools(tester);
        await openInvitation(tester);
        await tapHeader(tester, 'Back');
        expect(
          find.text('Paired devices').hitTestable(),
          findsOneWidget,
        );
        expect(find.text('j-strix · aaaaaaaa').hitTestable(), findsOneWidget);
        await tapHeader(tester, 'Back');
        expect(find.text('Settings'), findsOneWidget);
        final tools = find.text('Device access and pairing');
        await tester.ensureVisible(tools);
        await tester.tap(tools);
        await tester.pumpAndSettle();
        await openInvitation(tester);
        await tapHeader(tester, 'Close');
        expect(find.byKey(const ValueKey('pairing-qr')), findsNothing);
        expect(find.byType(ServiceSettings), findsNothing);
        expect(find.widgetWithText(TextField, 'Device name'), findsNothing);
        expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);
        expect(tester.takeException(), isNull);
        await fixture.close(tester);
      },
    );

    testWidgets('$platform paired-machine pages preserve Manage for Back', (
      tester,
    ) async {
      final backend = OwnerFeaturesBackend(android);
      await mountOwner(tester, backend);
      await tester.tap(
        find.widgetWithText(OutlinedButton, 'Manage · 1 request'),
      );
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('Settings for j-strix'));
      await tester.pumpAndSettle();
      expect(find.textContaining('Device name\nj-strix'), findsOneWidget);
      await tapHeader(tester, 'Back');
      expect(find.text('Paired devices').hitTestable(), findsOneWidget);
      final pair = find.widgetWithText(FilledButton, '+ Pair another device');
      await tester.ensureVisible(pair);
      await tester.tap(pair);
      await tester.pumpAndSettle();
      expect(find.text('Pair a device').hitTestable(), findsOneWidget);
      await tapHeader(tester, 'Back');
      expect(find.text('Paired devices').hitTestable(), findsOneWidget);
      await tester.tap(find.byTooltip('Settings for j-strix'));
      await tester.pumpAndSettle();
      await tapHeader(tester, 'Close');
      expect(find.text('Paired devices'), findsNothing);
      expect(find.textContaining('Device name\nj-strix'), findsNothing);
      expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);
      expect(tester.takeException(), isNull);
      await fixture.close(tester);
    });

    testWidgets(
      '$platform removing a machine refreshes the preserved Manage page',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await tester.tap(
          find.widgetWithText(OutlinedButton, 'Manage · 1 request'),
        );
        await tester.pumpAndSettle();
        await tester.tap(find.byTooltip('Settings for j-strix'));
        await tester.pumpAndSettle();
        await tester.tap(find.widgetWithText(OutlinedButton, 'Remove'));
        await tester.pumpAndSettle();
        final confirm = find.widgetWithText(FilledButton, 'Remove device');
        await tester.ensureVisible(confirm);
        await tester.tap(confirm);
        await tester.pumpAndSettle();
        expect(backend.paired, isFalse);
        final removals = backend.calls
            .where((call) => call.$1 == 'removeHost')
            .toList();
        expect(removals, hasLength(1));
        expect(removals.single.$2, {'host': fixture.peer});
        expect(find.text('Paired devices').hitTestable(), findsOneWidget);
        expect(find.byTooltip('Settings for j-strix'), findsNothing);
        expect(find.text('j-strix · aaaaaaaa'), findsNothing);
        expect(find.textContaining('Device name\nj-strix'), findsNothing);
        expect(
          find.widgetWithText(FilledButton, '+ Pair another device'),
          findsOneWidget,
        );
        await tapHeader(tester, 'Close');
        expect(find.text('Paired devices'), findsNothing);
        expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);
        expect(tester.takeException(), isNull);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform closing a pending approval does not pop home when it completes',
      (tester) async {
        final backend = DeferredOwnerBackend(android);
        await mountOwner(tester, backend);
        await openOwnerTools(tester);
        await tester.tap(find.text('Review 1 pairing request'));
        await tester.pumpAndSettle();
        final approve = find.widgetWithText(OutlinedButton, 'Approve device');
        await tester.ensureVisible(approve);
        await tester.tap(approve);
        await tester.pumpAndSettle();
        final confirm = find.widgetWithText(FilledButton, 'Approve device');
        await tester.ensureVisible(confirm);
        await tester.tap(confirm);
        await tester.pump();
        expect(backend.submitted.isCompleted, isTrue);
        final confirmation = tester.element(confirm);
        final close = find.byTooltip('Close').last;
        await tester.ensureVisible(close);
        await tester.tap(close);
        // Resolve while the dismissed sheets are still mounted for their exit
        // animation. A late success must not pop the underlying app route.
        expect(confirmation.mounted, isTrue);
        backend.completed.complete();
        await tester.pumpAndSettle();
        expect(backend.ownerCalls, hasLength(1));
        expect(find.byType(ServiceSettings), findsNothing);
        expect(find.widgetWithText(TextField, 'Device name'), findsNothing);
        expect(find.byTooltip('Settings').hitTestable(), findsOneWidget);
        await tester.tap(find.byTooltip('Settings'));
        await tester.pumpAndSettle();
        expect(find.text('Settings'), findsOneWidget);
        expect(tester.takeException(), isNull);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform paired devices links to local access and issues a pairing QR',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await openOwnerTools(tester);
        final access = find.widgetWithText(TextButton, 'Device access');
        await tester.ensureVisible(access);
        await tester.tap(access);
        await tester.pumpAndSettle();
        final settings = tester.widget<ServiceSettings>(
          find.byType(ServiceSettings),
        );
        expect(settings.android, android);
        expect(settings.initialSection, 'Devices');
        expect(find.text('Existing device'), findsOneWidget);
        await tapHeader(tester, 'Back');
        await tester.ensureVisible(find.text('Pair this device'));
        await tester.tap(find.text('Pair this device'));
        await tester.pumpAndSettle();
        expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
        expect(
          backend.ownerCalls.where((call) => call['op'] == 'invite_qr'),
          hasLength(1),
        );
        await tester.tap(find.text('Pairing code'));
        await tester.pumpAndSettle();
        expect(find.text('wispnet1:generated-code'), findsOneWidget);
        await fixture.close(tester);
      },
    );

    for (final kind in ['http', 'tcp']) {
      testWidgets(
        '$platform creates a shared $kind service with an explicit port and device grant',
        (tester) async {
          final backend = OwnerFeaturesBackend(android);
          await mountOwner(tester, backend);
          await tester.tap(find.byTooltip('Settings'));
          await tester.pumpAndSettle();
          await tester.ensureVisible(find.text('Local services'));
          await tester.tap(find.text('Local services'));
          await tester.pumpAndSettle();
          await tester.ensureVisible(
            find.byKey(const ValueKey('share-service')),
          );
          await tester.tap(find.byKey(const ValueKey('share-service')));
          await tester.pumpAndSettle();
          expect(find.widgetWithText(Tab, 'Custom'), findsOneWidget);
          await tester.tap(find.widgetWithText(Tab, 'Custom'));
          await tester.pumpAndSettle();
          await tester.enterText(
            find.widgetWithText(TextField, 'Port'),
            '8201',
          );
          await tester.enterText(
            find.widgetWithText(TextField, 'Service ID'),
            'shared-$kind',
          );
          if (kind == 'tcp') {
            await tester.tap(find.byType(DropdownButtonFormField<String>));
            await tester.pumpAndSettle();
            await tester.tap(find.text('TCP').last);
            await tester.pumpAndSettle();
          }
          await tester.ensureVisible(find.text('Existing device'));
          await tester.tap(find.text('Existing device'));
          await tester.ensureVisible(
            find.widgetWithText(FilledButton, 'Save').last,
          );
          await tester.tap(find.widgetWithText(FilledButton, 'Save').last);
          await tester.pumpAndSettle();
          expect(backend.ownerCalls.single, {
            'op': 'mutate',
            'operation': {
              'op': 'service',
              'id': 'shared-$kind',
              'backend': '127.0.0.1:8201',
              'kind': kind,
              'allow': [fixture.peer],
              'expected_revision': 7,
            },
          });
          expect(tester.takeException(), isNull);
          await fixture.close(tester);
        },
      );
    }

    testWidgets(
      '$platform public HTTP access requires the shared explicit confirmation',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await tester.tap(find.byTooltip('Settings for web on hibreak'));
        await tester.pumpAndSettle();
        final enable = find.widgetWithText(
          OutlinedButton,
          'Enable public access',
        );
        await tester.ensureVisible(enable);
        await tester.tap(enable);
        await tester.pumpAndSettle();
        expect(find.text('Expose service to the internet?'), findsOneWidget);
        expect(
          find.textContaining('unprotected to the public internet'),
          findsOneWidget,
        );
        expect(backend.ownerCalls, isEmpty);
        await tester.ensureVisible(find.text('Cancel').last);
        await tester.tap(find.text('Cancel').last);
        await tester.pumpAndSettle();
        expect(backend.ownerCalls, isEmpty);
        await tester.ensureVisible(enable);
        await tester.tap(enable);
        await tester.pumpAndSettle();
        final confirm = find.widgetWithText(
          FilledButton,
          'Enable public access',
        );
        await tester.ensureVisible(confirm);
        await tester.tap(confirm);
        await tester.pumpAndSettle();
        expect(backend.ownerCalls.single, {
          'op': 'configure_public',
          'service': 'web',
          'incarnation': fixture.incarnation,
          'enabled': true,
          'name': 'Desktop',
        });
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform reviews and approves pairing requests through the shared owner page',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await openOwnerTools(tester);
        await tester.tap(find.text('Review 1 pairing request'));
        await tester.pumpAndSettle();
        final approve = find.widgetWithText(OutlinedButton, 'Approve device');
        await tester.ensureVisible(approve);
        await tester.tap(approve);
        await tester.pumpAndSettle();
        expect(backend.ownerCalls, isEmpty);
        final confirm = find.widgetWithText(FilledButton, 'Approve device');
        await tester.ensureVisible(confirm);
        await tester.tap(confirm);
        await tester.pumpAndSettle();
        expect(backend.ownerCalls.single, {
          'op': 'mutate',
          'operation': {
            'op': 'approve_invitation',
            'invitation': 'request-id',
            'services': ['web'],
            'expected_revision': 7,
          },
        });
        await fixture.close(tester);
      },
    );
  }
}
