import 'dart:async';
import 'dart:convert';

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

class PermissionBackend extends OwnerFeaturesBackend {
  PermissionBackend(super.android);

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    final operation = arguments['payload']?['operation'];
    if (method == 'ownerRequest' &&
        const ['grant_service', 'revoke_service'].contains(operation?['op'])) {
      if (operation['expected_revision'] != status['host']['revision']) {
        throw const FormatException('Policy changed. Refresh and try again.');
      }
      final service = (status['host']['services'] as List).singleWhere(
        (service) => service['id'] == operation['service'],
      );
      final allow = List<String>.from(service['allow'] as List);
      if (operation['op'] == 'grant_service') {
        allow.add(operation['peer'] as String);
      } else {
        allow.remove(operation['peer']);
      }
      service['allow'] = allow;
      status['host']['revision']++;
    }
    final result = await super.invoke(method, arguments);
    return method == 'ownerStatus'
        ? jsonDecode(jsonEncode(result)) as Map<String, dynamic>
        : result;
  }
}

class PairingFlowBackend extends OwnerFeaturesBackend {
  PairingFlowBackend(super.android) {
    (status['invitations'] as List).clear();
  }
  bool expired = false, failStatus = false, outgoing = false;

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'ownerStatus' && failStatus) {
      throw const FormatException('Status unavailable');
    }
    final result = await super.invoke(method, arguments);
    if (method == 'pair') outgoing = true;
    if (method == 'ownerRequest' &&
        arguments['payload']?['op'] == 'invite_qr' &&
        expired) {
      return {
        ...result,
        'expires_at': DateTime.now().millisecondsSinceEpoch ~/ 1000 - 1,
      };
    }
    if (method == 'snapshot' && outgoing) {
      final hosts = Map<String, dynamic>.from(result['hosts'] as Map);
      final remote = Map<String, dynamic>.from(hosts[fixture.peer] as Map);
      remote['state'] = 'approval_pending';
      remote['status'] = 'Pending approval on this host';
      return {
        ...result,
        'order': {
          'machines': ['e' * 64, fixture.peer],
        },
        'hosts': {
          ...hosts,
          'e' * 64: {
            'name': 'Earlier device',
            'state': 'available',
            'status': 'Available',
          },
          fixture.peer: remote,
        },
      };
    }
    return result;
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
      '$platform receiving a request returns to the list with inline approval first',
      (tester) async {
        final backend = PairingFlowBackend(android);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        await openInvitation(tester);
        backend.failStatus = true;
        await tester.pump(const Duration(seconds: 1));
        await tester.pumpAndSettle();
        expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
        expect(
          find.text('Cannot check pairing status. Retrying…'),
          findsOneWidget,
        );
        backend.failStatus = false;
        (backend.status['invitations'] as List).add({
          'name': 'Incoming device',
          'peer': 'c' * 64,
          'invitation': 'issued-id',
          'state': 'pending',
        });
        await tester.pump(const Duration(seconds: 1));
        await tester.pumpAndSettle();
        expect(find.text('Paired devices').hitTestable(), findsOneWidget);
        expect(find.byKey(const ValueKey('pairing-qr')), findsNothing);
        expect(find.text('c' * 64), findsOneWidget);
        expect(find.byTooltip('Settings for Incoming device'), findsNothing);
        final card = find.byKey(const ValueKey('pairing-request-issued-id'));
        expect(
          tester.getTopLeft(card).dy,
          lessThan(tester.getTopLeft(find.text('j-strix · aaaaaaaa').last).dy),
        );
        expect(find.text('Reject'), findsOneWidget);
        await fixture.screenshot(tester, '$platform-inline-pairing-request');
        await tester.ensureVisible(find.text('Approve'));
        await tester.tap(find.text('Approve'));
        await tester.pumpAndSettle();
        expect(backend.ownerCalls.last['operation'], {
          'op': 'approve_invitation',
          'invitation': 'issued-id',
          'expected_revision': 7,
        });
        expect(card, findsNothing);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform expired invitation stays distinct from a received request',
      (tester) async {
        final backend = PairingFlowBackend(android)..expired = true;
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        await openInvitation(tester);
        await tester.pump(const Duration(seconds: 1));
        await tester.pumpAndSettle();
        expect(
          find.text('Pairing code expired. Generate a new code to try again.'),
          findsOneWidget,
        );
        expect(find.text('Approve'), findsNothing);
        backend.expired = false;
        await tester.tap(find.text('Generate new code'));
        await tester.pumpAndSettle();
        expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform inline rejection removes only the reviewed request',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        final services = jsonEncode(backend.status['host']['services']);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        await tester.ensureVisible(find.text('Reject'));
        await tester.tap(find.text('Reject'));
        await tester.pumpAndSettle();
        expect(backend.ownerCalls.single['operation'], {
          'op': 'reject_invitation',
          'invitation': 'request-id',
          'expected_revision': 7,
        });
        expect(find.text('Reject'), findsNothing);
        expect(jsonEncode(backend.status['host']['services']), services);
        expect(find.text('j-strix · aaaaaaaa').last, findsOneWidget);
        await fixture.close(tester);
      },
    );

    testWidgets(
      '$platform submitting pairing opens the list with the outgoing pending device first',
      (tester) async {
        final backend = PairingFlowBackend(android)..paired = false;
        await mountOwner(tester, backend);
        await tester.tap(find.byKey(const ValueKey('remote-pair-button')));
        await tester.pumpAndSettle();
        await tester.tap(find.text('Pairing Code'));
        await tester.pumpAndSettle();
        await tester.enterText(
          find.widgetWithText(TextField, 'Pairing code'),
          'wispnet1:test',
        );
        await tester.pump();
        await tester.tap(find.widgetWithText(FilledButton, 'Pair'));
        await tester.pumpAndSettle();
        expect(find.text('Paired devices').hitTestable(), findsOneWidget);
        expect(find.text(fixture.peer), findsOneWidget);
        expect(find.byTooltip('Settings for j-strix'), findsNothing);
        expect(find.text('Approve'), findsNothing);
        expect(find.text('Reject'), findsNothing);
        final waiting = find.text('Waiting for approval on the other device');
        expect(waiting, findsOneWidget);
        expect(
          tester.getTopLeft(waiting).dy,
          lessThan(
            tester.getTopLeft(find.text('Earlier device · eeeeeeee').last).dy,
          ),
        );
        await fixture.close(tester);
      },
    );
    testWidgets(
      '$platform per-device permissions preserve other devices and reject stale revisions',
      (tester) async {
        final backend = PermissionBackend(android);
        final service = (backend.status['host']['services'] as List).first;
        final otherPeer = 'b' * 64;
        (service['allow'] as List).add(otherPeer);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        await tester.tap(find.byTooltip('Settings for j-strix'));
        await tester.pumpAndSettle();
        final toggle = find.descendant(
          of: find.byType(ServiceSettings),
          matching: find.text('web'),
        );
        await tester.ensureVisible(toggle);
        await tester.tap(toggle);
        await tester.pumpAndSettle();
        expect(service['allow'], [otherPeer]);
        expect(backend.ownerCalls.single['operation'], {
          'op': 'revoke_service',
          'service': 'web',
          'peer': fixture.peer,
          'expected_revision': 7,
        });
        backend.status['host']['revision']++;
        await tester.tap(toggle);
        await tester.pumpAndSettle();
        expect(
          find.text('Policy changed. Refresh and try again.'),
          findsOneWidget,
        );
        expect(service['allow'], [otherPeer]);
        expect(tester.takeException(), isNull);
        await fixture.close(tester);
      },
    );

    testWidgets('$platform inbound-only approved devices remain manageable', (
      tester,
    ) async {
      final backend = OwnerFeaturesBackend(android)..paired = false;
      await mountOwner(tester, backend);
      await fixture.openPairedDevices(tester);
      expect(find.text('Existing device · aaaaaaaa'), findsOneWidget);
      await tester.ensureVisible(
        find.byTooltip('Settings for Existing device'),
      );
      await tester.tap(find.byTooltip('Settings for Existing device'));
      await tester.pumpAndSettle();
      expect(find.text('Service permissions'), findsOneWidget);
      expect(find.widgetWithText(OutlinedButton, 'Remove'), findsOneWidget);
      await fixture.close(tester);
    });

    testWidgets('$platform owner operations are under advanced settings', (
      tester,
    ) async {
      final backend = OwnerFeaturesBackend(android);
      await mountOwner(tester, backend);
      await tester.tap(find.byTooltip('Settings'));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Advanced'));
      await tester.tap(find.text('Advanced'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Owner operations'));
      await tester.pumpAndSettle();
      expect(find.text('No saved owner operations.'), findsOneWidget);
      expect(find.byType(DropdownButton<String>), findsNothing);
      await fixture.close(tester);
    });
    testWidgets(
      '$platform owner title is specific, Back returns to Settings and X closes the popup',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        expect(find.text('Paired devices').hitTestable(), findsOneWidget);
        expect(find.text('Device and browser settings'), findsNothing);
        expect(find.text('+ Pair another device'), findsOneWidget);
        expect(find.text('Pair this device'), findsOneWidget);
        await tapHeader(tester, 'Back');
        expect(find.text('Settings'), findsOneWidget);
        expect(find.text('Device'), findsOneWidget);
        expect(find.text('Services'), findsOneWidget);
        expect(find.byType(ServiceSettings), findsNothing);

        final tools = find.text('Access & pairing');
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
        await fixture.openPairedDevices(tester);
        await openInvitation(tester);
        await tapHeader(tester, 'Back');
        expect(find.text('Paired devices').hitTestable(), findsOneWidget);
        expect(find.text('j-strix · aaaaaaaa').hitTestable(), findsOneWidget);
        await tapHeader(tester, 'Back');
        expect(find.text('Settings'), findsOneWidget);
        final tools = find.text('Access & pairing');
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

    testWidgets('$platform paired-machine pages go Back to Paired devices', (
      tester,
    ) async {
      final backend = OwnerFeaturesBackend(android);
      await mountOwner(tester, backend);
      await fixture.openPairedDevices(tester);
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
      '$platform removing a machine refreshes the Paired devices list',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
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
        await fixture.openPairedDevices(tester);
        final confirm = find.widgetWithText(FilledButton, 'Approve');
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
      '$platform paired devices consolidates permissions and issues a pairing QR',
      (tester) async {
        final backend = OwnerFeaturesBackend(android);
        await mountOwner(tester, backend);
        await fixture.openPairedDevices(tester);
        expect(find.text('Device access'), findsNothing);
        await tester.tap(find.byTooltip('Settings for j-strix'));
        await tester.pumpAndSettle();
        final settings = tester.widget<ServiceSettings>(
          find.byType(ServiceSettings).last,
        );
        expect(settings.android, android);
        expect(settings.devicePeer, fixture.peer);
        expect(find.text('Service permissions'), findsOneWidget);
        expect(
          find.text('Allow this device to use services on this machine.'),
          findsOneWidget,
        );
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
          await tester.ensureVisible(find.text('Services'));
          await tester.tap(find.text('Services'));
          await tester.pumpAndSettle();
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
        await fixture.openPairedDevices(tester);
        expect(find.text('c' * 64), findsOneWidget);
        expect(backend.ownerCalls, isEmpty);
        final approve = find.widgetWithText(FilledButton, 'Approve');
        await tester.ensureVisible(approve);
        await tester.tap(approve);
        await tester.pumpAndSettle();
        expect(backend.ownerCalls.single, {
          'op': 'mutate',
          'operation': {
            'op': 'approve_invitation',
            'invitation': 'request-id',
            'expected_revision': 7,
          },
        });
        await fixture.close(tester);
      },
    );
  }
}
