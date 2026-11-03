import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/service_settings.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/file_shares.dart';
import 'package:wispnet/src/file_transfer.dart';
import 'package:wispnet/src/local_service_details.dart';
import 'package:wispnet/ui.dart';

import 'android_file_shares_test.dart' as fixture;

class FolderBackend extends fixture.AndroidSharesBackend {
  bool desktop = false, remote = false;
  final legacyExports = <String, String>{};
  String pickedDesktopFolder = '/chosen/inbox';
  final inboxes = <Map<String, dynamic>>[
    {
      'id': 'training-data',
      'folder': 'opaque-training',
      'label': 'Training data',
      'allow': [fixture.laptop],
      'enabled': true,
      'available': true,
      'incarnation': '1' * 64,
      'kind': 'file_inbox',
    },
    {
      'id': 'predictions',
      'folder': 'opaque-predictions',
      'label': 'Predictions',
      'allow': [fixture.other],
      'enabled': true,
      'available': true,
      'incarnation': '2' * 64,
      'kind': 'file_inbox',
    },
  ];
  List<Map<String, dynamic>> restoredTransfers = [];
  final transferStatus = {
    'transfer': '9' * 64,
    'state': 'complete',
    'phase': 'complete',
    'verified_bytes': 1024,
    'size_bytes': 1024,
  };

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    switch (method) {
      case 'folderServicesStatus':
        final response = await super.invoke(method, arguments);
        return {
          ...response,
          'inboxes': inboxes.map(Map<String, dynamic>.of).toList(),
          'file_exports': legacyExports.isNotEmpty
              ? Map.of(legacyExports)
              : {
                  for (final inbox in inboxes)
                    if (inbox['kind'] == 'files') 'incoming': inbox['folder'],
                },
        };
      case 'pickReceivingFolder':
        calls.add((method, Map.of(arguments)));
        return {'folder': 'opaque-receive', 'label': 'Receiving folder'};
      case 'saveInbox':
        calls.add((method, Map.of(arguments)));
        expect(arguments['expected_revision'], revision);
        final existing = inboxes
            .where((inbox) => inbox['id'] == arguments['id'])
            .firstOrNull;
        inboxes.removeWhere((inbox) => inbox['id'] == arguments['id']);
        inboxes.add({
          ...?existing,
          'id': arguments['new_id'] ?? arguments['id'],
          'kind': existing?['kind'] ?? 'file_inbox',
          'folder': arguments['folder'],
          'label': existing?['label'] ?? 'Receiving folder',
          'allow': arguments['allow'],
          'enabled': existing?['enabled'] ?? true,
          'available': true,
          'incarnation': '3' * 64,
        });
        return {'revision': ++revision};
      case 'saveReceiving':
        calls.add((method, Map.of(arguments)));
        expect(arguments['expected_revision'], revision);
        legacyExports
          ..clear()
          ..addAll(Map<String, String>.from(arguments['exports'] as Map));
        final existing = inboxes
            .where((inbox) => inbox['id'] == 'files')
            .firstOrNull;
        inboxes.removeWhere((inbox) => inbox['id'] == 'files');
        if (legacyExports.isNotEmpty) {
          inboxes.add({
            ...?existing,
            'id': 'files',
            'kind': 'files',
            'folder': legacyExports.values.first,
            'label': legacyExports.values.first == 'opaque-receive'
                ? 'Receiving folder'
                : existing?['label'] ?? 'Legacy folder',
            'allow': List.of(arguments['allow'] as List),
            'enabled': existing?['enabled'] ?? true,
            'available': true,
            'incarnation': existing?['incarnation'] ?? 'f' * 64,
          });
        }
        return {'revision': ++revision};
      case 'removeInbox':
        calls.add((method, Map.of(arguments)));
        expect(arguments['expected_revision'], revision);
        inboxes.removeWhere((inbox) => inbox['id'] == arguments['id']);
        return {'revision': ++revision};
      case 'setServiceEnabled':
        if (inboxes.any((inbox) => inbox['id'] == arguments['service'])) {
          calls.add((method, Map.of(arguments)));
          inboxes.singleWhere(
            (inbox) => inbox['id'] == arguments['service'],
          )['enabled'] = arguments['enabled'];
          revision++;
          return {};
        }
        return super.invoke(method, arguments);
      case 'ownerStatus':
        calls.add((method, Map.of(arguments)));
        return {
          'host': {
            'host': fixture.own,
            'revision': revision,
            'services': [
              if (!inboxes.any((inbox) => inbox['id'] == 'files'))
                {
                  'id': 'files',
                  'kind': 'files',
                  'allow': [],
                  'incarnation': 'f' * 64,
                },
              ...inboxes,
              for (final share in shares) {...share, 'kind': 'file_share'},
            ],
            'clients': [
              {'peer': fixture.laptop, 'name': 'My laptop'},
              {'peer': fixture.other, 'name': 'Other device'},
            ],
            'file_exports': legacyExports.isNotEmpty
                ? Map.of(legacyExports)
                : {
                    for (final inbox in inboxes)
                      if (inbox['kind'] == 'files') 'incoming': inbox['folder'],
                  },
            'file_inboxes': {
              for (final inbox in inboxes) inbox['id']: '/srv/${inbox['id']}',
            },
            'file_shares': {
              for (final share in shares) share['id']: '/srv/${share['id']}',
            },
          },
          'invitations': [],
        };
      case 'ownerData':
        calls.add((method, Map.of(arguments)));
        return {'value': []};
      case 'ownerRequest':
        calls.add((method, Map.of(arguments)));
        final operation = (arguments['payload'] as Map)['operation'] as Map;
        expect(operation['expected_revision'], revision);
        if (operation['op'] == 'remove') {
          inboxes.removeWhere((inbox) => inbox['id'] == operation['id']);
          shares.removeWhere((share) => share['id'] == operation['id']);
        } else if (operation['op'] == 'edit_folder') {
          final folder = [
            ...inboxes,
            ...shares,
          ].singleWhere((folder) => folder['id'] == operation['id']);
          folder['id'] = operation['new_id'];
          folder['allow'] = List.of(operation['allow'] as List);
        }
        return {'revision': ++revision};
      case 'pickFolder':
        calls.add((method, Map.of(arguments)));
        return {'path': pickedDesktopFolder};
      case 'snapshot':
        final snapshot = await super.invoke(method, arguments);
        return {
          ...snapshot,
          'desktop': desktop,
          'hosts': {
            if (!remote)
              fixture.own: {
                'name': 'Android phone',
                'local': true,
                'state': 'available',
                'catalog': {
                  'host': fixture.own,
                  'services': [
                    ...inboxes,
                    for (final share in shares)
                      {...share, 'kind': 'file_share'},
                  ],
                },
              },
            if (remote)
              fixture.own: {
                'name': 'Training host',
                'state': 'available',
                'catalog': {
                  'host': fixture.own,
                  'services': [
                    ...inboxes,
                    for (final share in shares)
                      {...share, 'kind': 'file_share'},
                  ],
                },
              },
          },
        };
      case 'fileSend':
      case 'fileStatus':
      case 'fileCancel':
        calls.add((method, Map.of(arguments)));
        return Map.of(transferStatus);
      case 'fileList':
        calls.add((method, Map.of(arguments)));
        return {'transfers': restoredTransfers};
      case 'pickFiles':
        calls.add((method, Map.of(arguments)));
        return {
          'files': [
            {
              'path': 'content://documents/checkpoint',
              'name': 'checkpoint.bin',
              'size_bytes': 1024,
            },
          ],
        };
      default:
        return super.invoke(method, arguments);
    }
  }
}

Future<void> chooseMode(WidgetTester tester, String label) async {
  await tester.ensureVisible(find.widgetWithText(Tab, label));
  await tester.tap(find.widgetWithText(Tab, label));
  await tester.pumpAndSettle();
}

Future<void> app(WidgetTester tester, FolderBackend backend) async {
  tester.platformDispatcher.accessibilityFeaturesTestValue =
      const FakeAccessibilityFeatures(disableAnimations: true);
  addTearDown(tester.platformDispatcher.clearAccessibilityFeaturesTestValue);
  await tester.pumpWidget(
    RepaintBoundary(
      key: const ValueKey('android-share-preview'),
      child: MediaQuery(
        data: const MediaQueryData(
          size: Size(800, 600),
          disableAnimations: true,
        ),
        child: WispnetApp(backend: backend),
      ),
    ),
  );
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

  for (final desktop in [false, true]) {
    for (final size in [const Size(360, 780), const Size(1000, 850)]) {
      for (final inbox in [true, false]) {
        testWidgets(
          '${desktop ? 'desktop' : 'Android'} ${inbox ? 'inbox' : 'share'} details use shared actions at ${size.width.toInt()}px',
          (tester) async {
            tester.view.physicalSize = size;
            tester.view.devicePixelRatio = 1;
            addTearDown(tester.view.resetPhysicalSize);
            addTearDown(tester.view.resetDevicePixelRatio);
            final backend = FolderBackend()..desktop = desktop;
            final id = inbox ? 'predictions' : 'datasets';
            final kind = inbox ? 'file_inbox' : 'file_share';
            await app(tester, backend);
            final gear = find.byTooltip('Settings for $id on Android phone');
            await tester.ensureVisible(gear);
            await tester.tap(gear);
            await tester.pumpAndSettle();

            final page = tester.widget<ServiceSettings>(
              find.byType(ServiceSettings),
            );
            expect(page.android, !desktop);
            expect(page.serviceId, id);
            final details = tester.widget<LocalServiceDetails>(
              find.byType(LocalServiceDetails),
            );
            expect(details.id, id);
            expect(details.kind, kind);
            expect(details.host, fixture.own);
            expect(find.widgetWithText(OutlinedButton, 'Edit'), findsOneWidget);
            expect(
              find.widgetWithText(OutlinedButton, 'Remove service'),
              findsOneWidget,
            );
            if (inbox) {
              final command = tester.widget<FileSendCommand>(
                find.byType(FileSendCommand),
              );
              expect(command.host, fixture.own);
              expect(command.service, id);
            } else {
              expect(find.byType(FileSendCommand), findsNothing);
              expect(
                find.textContaining('browse and download'),
                findsOneWidget,
              );
            }
            expect(tester.takeException(), isNull);
            await fixture.capture(
              tester,
              'service-details-${desktop ? 'desktop' : 'android'}-${inbox ? 'inbox' : 'share'}-${size.width.toInt()}',
            );

            await tester.ensureVisible(find.text('Edit'));
            await tester.tap(find.text('Edit'));
            await tester.pumpAndSettle();
            final editor = tester.widget<FileShareEditor>(
              find.byType(FileShareEditor),
            );
            expect(editor.id, id);
            expect(editor.mode, inbox ? 'inbox' : 'share');
            expect(editor.allow, [fixture.other]);
            expect(find.byKey(const ValueKey('folder-mode')), findsNothing);
            expect(
              tester
                  .widget<WispToggleListTile>(
                    find.byKey(ValueKey('share-allow-${fixture.laptop}')),
                  )
                  .value,
              isFalse,
            );
            expect(
              tester
                  .widget<WispToggleListTile>(
                    find.byKey(ValueKey('share-allow-${fixture.other}')),
                  )
                  .value,
              isTrue,
            );
            await tester.ensureVisible(find.text('Other device').last);
            await tester.tap(find.text('Other device').last);
            await tester.ensureVisible(find.text('My laptop').last);
            await tester.tap(find.text('My laptop').last);
            await tester.ensureVisible(find.text('Save'));
            await tester.tap(find.text('Save'));
            await tester.pumpAndSettle();
            if (desktop) {
              final operation = backend.calls
                  .where((call) => call.$1 == 'ownerRequest')
                  .single
                  .$2['payload']['operation'];
              expect(operation, {
                'op': 'edit_folder',
                'id': id,
                'new_id': id,
                'root': '/srv/$id',
                'allow': [fixture.laptop],
                'expected_revision': 3,
              });
            } else {
              final method = inbox ? 'saveInbox' : 'saveShare';
              expect(
                backend.calls.where((call) => call.$1 == method).single.$2,
                {
                  'expected_revision': 3,
                  'id': id,
                  'new_id': id,
                  'folder': inbox ? 'opaque-predictions' : 'opaque-datasets',
                  'allow': [fixture.laptop],
                },
              );
            }
            expect(backend.inboxes.first['allow'], [fixture.laptop]);
            expect(backend.shares.first['allow'], [fixture.laptop]);

            Future<void> openRemoval() async {
              final remove = find.widgetWithText(
                OutlinedButton,
                'Remove service',
              );
              await tester.ensureVisible(remove);
              await tester.tap(remove);
              await tester.pumpAndSettle();
              expect(find.text('Remove service?'), findsOneWidget);
            }

            await openRemoval();
            final callsBeforeCancel = backend.calls
                .where(
                  (call) =>
                      call.$1 == 'ownerRequest' ||
                      call.$1 == 'removeInbox' ||
                      call.$1 == 'removeShare',
                )
                .length;
            await tester.ensureVisible(find.text('Cancel').last);
            await tester.tap(find.text('Cancel').last);
            await tester.pumpAndSettle();
            expect(
              backend.calls
                  .where(
                    (call) =>
                        call.$1 == 'ownerRequest' ||
                        call.$1 == 'removeInbox' ||
                        call.$1 == 'removeShare',
                  )
                  .length,
              callsBeforeCancel,
            );
            expect(find.byType(LocalServiceDetails), findsOneWidget);
            await openRemoval();
            final confirm = find.widgetWithText(FilledButton, 'Remove service');
            await tester.ensureVisible(confirm);
            await tester.tap(confirm);
            await tester.pumpAndSettle();
            if (desktop) {
              expect(
                backend.calls
                    .where((call) => call.$1 == 'ownerRequest')
                    .last
                    .$2['payload']['operation'],
                {'op': 'remove', 'id': id, 'expected_revision': 4},
              );
            } else {
              expect(
                backend.calls
                    .where(
                      (call) =>
                          call.$1 == (inbox ? 'removeInbox' : 'removeShare'),
                    )
                    .single
                    .$2,
                {'expected_revision': 4, 'id': id},
              );
            }
            expect(
              [
                ...backend.inboxes,
                ...backend.shares,
              ].where((folder) => folder['id'] == id),
              isEmpty,
            );
            expect(tester.takeException(), isNull);
            await tester.pumpWidget(const SizedBox());
          },
        );
      }
    }
  }

  testWidgets(
    'Android legacy inbox service removal pauses serving and keeps folder grants',
    (tester) async {
      final backend = FolderBackend();
      backend.inboxes.first['id'] = 'files';
      backend.inboxes.first['kind'] = 'files';
      await app(tester, backend);
      await tester.tap(find.byTooltip('Settings for files on Android phone'));
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<LocalServiceDetails>(find.byType(LocalServiceDetails))
            .kind,
        'files',
      );
      await tester.ensureVisible(find.text('Disable service'));
      await tester.tap(find.text('Disable service'));
      await tester.pumpAndSettle();
      expect(find.text('Disable service?'), findsOneWidget);
      final confirm = find.widgetWithText(FilledButton, 'Disable service');
      await tester.ensureVisible(confirm);
      await tester.tap(confirm);
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'setServiceEnabled').single.$2,
        {'service': 'files', 'incarnation': '1' * 64, 'enabled': false},
      );
      expect(backend.calls.where((call) => call.$1 == 'removeInbox'), isEmpty);
      final inbox = backend.inboxes.singleWhere(
        (inbox) => inbox['id'] == 'files',
      );
      expect(inbox['enabled'], isFalse);
      expect(inbox['folder'], 'opaque-training');
      expect(inbox['allow'], [fixture.laptop]);

      // The common built-in editor stages folder removal until Save.
      await tester.ensureVisible(find.text('Edit'));
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      final removeFolder = find.byTooltip('Remove receiving folder');
      await tester.ensureVisible(removeFolder);
      await tester.tap(removeFolder);
      await tester.pumpAndSettle();
      expect(
        backend.inboxes.where((inbox) => inbox['id'] == 'files'),
        isNotEmpty,
      );
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'saveReceiving').single.$2,
        {
          'expected_revision': 4,
          'exports': <String, String>{},
          'allow': <String>[],
        },
      );
      expect(backend.inboxes.where((inbox) => inbox['id'] == 'files'), isEmpty);
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
    },
  );

  for (final inbox in [true, false]) {
    testWidgets(
      'Android ${inbox ? 'inbox' : 'share'} rename keeps its original capability and grants',
      (tester) async {
        final backend = FolderBackend();
        final id = inbox ? 'predictions' : 'datasets';
        final folders = inbox ? backend.inboxes : backend.shares;
        final original = folders.singleWhere((folder) => folder['id'] == id);
        original['enabled'] = false;
        await app(tester, backend);
        final gear = find.byTooltip('Settings for $id on Android phone');
        await tester.ensureVisible(gear);
        await tester.tap(gear);
        await tester.pumpAndSettle();
        await tester.ensureVisible(find.text('Edit'));
        await tester.tap(find.text('Edit'));
        await tester.pumpAndSettle();
        final name = find.byKey(const ValueKey('share-name'));
        expect(tester.widget<TextField>(name).enabled, isTrue);
        await tester.enterText(name, 'training-data');
        await tester.ensureVisible(find.text('Save'));
        await tester.tap(find.text('Save'));
        await tester.pumpAndSettle();
        expect(
          find.text('This service ID already exists. Choose a different ID.'),
          findsOneWidget,
        );
        expect(
          backend.calls.where(
            (call) => call.$1 == 'saveInbox' || call.$1 == 'saveShare',
          ),
          isEmpty,
        );
        await tester.enterText(name, '$id-renamed');
        await tester.ensureVisible(find.text('Save'));
        await tester.tap(find.text('Save'));
        await tester.pumpAndSettle();
        final method = inbox ? 'saveInbox' : 'saveShare';
        expect(backend.calls.where((call) => call.$1 == method).single.$2, {
          'expected_revision': 3,
          'id': id,
          'new_id': '$id-renamed',
          'folder': inbox ? 'opaque-predictions' : 'opaque-datasets',
          'allow': [fixture.other],
        });
        expect(folders.where((folder) => folder['id'] == id), isEmpty);
        final renamed = folders.singleWhere(
          (folder) => folder['id'] == '$id-renamed',
        );
        expect(renamed['folder'], original['folder']);
        expect(renamed['allow'], original['allow']);
        expect(renamed['enabled'], isFalse);
        expect(find.byType(LocalServiceDetails), findsNothing);
        expect(tester.takeException(), isNull);
        await tester.pumpWidget(const SizedBox());
      },
    );
  }

  testWidgets(
    'Android built-in Files uses the common staged multi-export permission editor',
    (tester) async {
      final backend = FolderBackend();
      backend.inboxes.first['id'] = 'files';
      backend.inboxes.first['kind'] = 'files';
      backend.legacyExports.addAll({
        'incoming': 'opaque-training',
        'archive': 'opaque-archive',
      });
      await fixture.mount(
        tester,
        ServiceSettings(
          backend: backend,
          android: true,
          host: fixture.own,
          servicesOnly: true,
          onChanged: () {},
        ),
      );
      await tester.tap(find.byTooltip('File inbox'));
      await tester.pumpAndSettle();
      expect(find.text('Receiving folder'), findsOneWidget);
      expect(find.text('Training data'), findsOneWidget);
      expect(find.textContaining('opaque-'), findsNothing);
      expect(find.byType(WispPermissionCard), findsOneWidget);
      expect(
        find.text(
          'Choosing a folder replaces the 2 receiving folders currently configured.',
        ),
        findsOneWidget,
      );
      await tester.ensureVisible(find.text('Other device'));
      await tester.tap(find.text('Other device'));
      expect(
        backend.calls.where((call) => call.$1 == 'saveReceiving'),
        isEmpty,
      );
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'saveReceiving').single.$2,
        {
          'expected_revision': 3,
          'exports': {
            'incoming': 'opaque-training',
            'archive': 'opaque-archive',
          },
          'allow': [fixture.laptop, fixture.other],
        },
      );
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'desktop Receive files tab creates a named Inbox with explicit per-folder grants',
    (tester) async {
      final backend = FolderBackend();
      await fixture.mount(
        tester,
        ServiceSettings(backend: backend, servicesOnly: true, onChanged: () {}),
      );
      expect(find.widgetWithText(Tab, 'Receive files'), findsNothing);
      expect(find.text('Add read-only file share'), findsNothing);
      expect(find.text('File inbox'), findsNWidgets(2));
      await tester.tap(find.byKey(const ValueKey('share-service')));
      await tester.pumpAndSettle();
      expect(find.widgetWithText(Tab, 'Receive files'), findsOneWidget);
      await tester.enterText(
        find.byKey(const ValueKey('share-name')),
        'checkpoints-incoming',
      );
      backend.pickedDesktopFolder = '/srv/checkpoints-incoming';
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Other device').last);
      await tester.tap(find.text('Other device').last);
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      final operation = backend.calls
          .where((call) => call.$1 == 'ownerRequest')
          .single
          .$2['payload']['operation'];
      expect(operation, {
        'op': 'file_inbox',
        'id': 'checkpoints-incoming',
        'root': '/srv/checkpoints-incoming',
        'allow': [fixture.other],
        'expected_revision': 3,
      });
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'Android creation tabs keep folder capabilities and grants separate',
    (tester) async {
      final backend = FolderBackend();
      await fixture.mount(
        tester,
        ServiceSettings(
          backend: backend,
          android: true,
          host: fixture.own,
          servicesOnly: true,
          onChanged: () {},
        ),
      );
      await tester.tap(find.byKey(const ValueKey('share-service')));
      await tester.pumpAndSettle();
      await tester.enterText(
        find.byKey(const ValueKey('share-name')),
        'results',
      );
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      expect(find.text('Receiving folder'), findsOneWidget);
      await tester.ensureVisible(find.text('My laptop'));
      await tester.tap(find.text('My laptop'));
      await chooseMode(tester, 'Share files');
      expect(find.text('Receiving folder'), findsNothing);
      expect(find.byKey(const ValueKey('share-selected-folder')), findsNothing);
      expect(
        tester
            .widget<WispToggleListTile>(
              find.byKey(ValueKey('share-allow-${fixture.laptop}')),
            )
            .value,
        isFalse,
      );
      expect(
        tester
            .widget<FilledButton>(find.widgetWithText(FilledButton, 'Save'))
            .onPressed,
        isNull,
      );
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'pickReceivingFolder'),
        hasLength(1),
      );
      expect(
        backend.calls.where((call) => call.$1 == 'pickShareFolder'),
        hasLength(1),
      );
      expect(backend.calls.where((call) => call.$1 == 'saveInbox'), isEmpty);
      expect(
        backend.calls
            .where((call) => call.$1 == 'saveShare')
            .single
            .$2['folder'],
        'opaque-selected-folder',
      );
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'editing an Android inbox keeps its mode and other inbox permissions intact',
    (tester) async {
      final backend = FolderBackend();
      await fixture.mount(
        tester,
        ServiceSettings(
          backend: backend,
          android: true,
          host: fixture.own,
          servicesOnly: true,
          onChanged: () {},
        ),
      );
      await tester.tap(find.byTooltip('Folders and access for predictions'));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('folder-mode')), findsNothing);
      expect(find.text('Allowed devices'), findsOneWidget);
      await tester.ensureVisible(find.text('Other device'));
      await tester.tap(find.text('Other device'));
      await tester.ensureVisible(find.text('My laptop'));
      await tester.tap(find.text('My laptop'));
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      expect(backend.calls.where((call) => call.$1 == 'saveInbox').single.$2, {
        'expected_revision': 3,
        'id': 'predictions',
        'new_id': 'predictions',
        'folder': 'opaque-predictions',
        'allow': [fixture.laptop],
      });
      expect(
        backend.inboxes.singleWhere(
          (inbox) => inbox['id'] == 'training-data',
        )['allow'],
        [fixture.laptop],
      );
      expect(
        backend.calls.where(
          (call) => call.$1 == 'saveShare' || call.$1 == 'saveReceiving',
        ),
        isEmpty,
      );
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets('an explicitly configured legacy inbox remains editable', (
    tester,
  ) async {
    final backend = FolderBackend();
    backend.inboxes.first['id'] = 'files';
    backend.inboxes.first['kind'] = 'files';
    await fixture.mount(
      tester,
      ServiceSettings(
        backend: backend,
        android: true,
        host: fixture.own,
        servicesOnly: true,
        onChanged: () {},
      ),
    );
    await tester.tap(find.byTooltip('File inbox'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('Save'));
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    expect(
      backend.calls.where((call) => call.$1 == 'saveReceiving').single.$2,
      {
        'expected_revision': 3,
        'exports': {'incoming': 'opaque-training'},
        'allow': [fixture.laptop],
      },
    );
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('remote Inbox card sends files to the selected named service', (
    tester,
  ) async {
    final backend = FolderBackend()..remote = true;
    await app(tester, backend);
    expect(find.text('File inbox'), findsNWidgets(2));
    await tester.tap(find.text('predictions'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Choose files'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('Send files'));
    await tester.tap(find.text('Send files'));
    await tester.pumpAndSettle();
    final send = backend.calls.where((call) => call.$1 == 'fileSend').single.$2;
    expect(send['service'], 'predictions');
    expect(send['incarnation'], '2' * 64);
    expect(send['workspace'], '');
    expect(
      backend.calls.any(
        (call) => call.$1 == 'saveInbox' || call.$1 == 'saveShare',
      ),
      isFalse,
    );
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets(
    'desktop drop prompts for an inbox instead of selecting the first folder',
    (tester) async {
      final backend = FolderBackend()
        ..remote = true
        ..desktop = true;
      await app(tester, backend);
      final drop = tester
          .widget<FileDropArea>(find.byType(FileDropArea).first)
          .onFiles(['/source/model.bin']);
      unawaited(drop);
      await tester.pumpAndSettle();
      expect(find.text('Choose inbox on Training host'), findsOneWidget);
      await tester.tap(find.byKey(const ValueKey('choose-inbox-predictions')));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Send files'));
      await tester.tap(find.text('Send files'));
      await tester.pumpAndSettle();
      expect(
        backend.calls
            .where((call) => call.$1 == 'fileSend')
            .single
            .$2['service'],
        'predictions',
      );
      await tester.ensureVisible(find.byTooltip('Close'));
      await tester.tap(find.byTooltip('Close'));
      await tester.pumpAndSettle();
      await drop;
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets('restored and retried uploads retain their named inbox service', (
    tester,
  ) async {
    final backend = FolderBackend();
    backend.restoredTransfers = [
      {
        'transfer': '8' * 64,
        'host': fixture.own,
        'service': 'predictions',
        'incarnation': '2' * 64,
        'workspace': '',
        'local_path': 'content://file',
        'remote_path': 'checkpoint.bin',
        'state': 'failed',
        'size_bytes': 1024,
      },
    ];
    final transfers = DesktopFileTransfers(backend);
    await transfers.restore({
      fixture.own: Machine(fixture.own, {'name': 'Training host'}),
    });
    expect(transfers.jobs.single.service, 'predictions');
    transfers.retry(transfers.jobs.single);
    await tester.pump();
    expect(
      backend.calls.where((call) => call.$1 == 'fileSend').single.$2['service'],
      'predictions',
    );
    transfers.dispose();
  });

  testWidgets('unified named folders and setup fit phone width', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(360, 780);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final backend = FolderBackend();
    await fixture.mount(
      tester,
      ServiceSettings(
        backend: backend,
        android: true,
        host: fixture.own,
        servicesOnly: true,
        onChanged: () {},
      ),
    );
    await fixture.capture(tester, 'unified-folder-services');
    await tester.tap(find.byKey(const ValueKey('share-service')));
    await tester.pumpAndSettle();
    await fixture.capture(tester, 'unified-add-folder');
    expect(tester.takeException(), isNull);
    await tester.pumpWidget(const SizedBox());
    tester.platformDispatcher.accessibilityFeaturesTestValue =
        const FakeAccessibilityFeatures(disableAnimations: true);
    addTearDown(tester.platformDispatcher.clearAccessibilityFeaturesTestValue);
    final remote = FolderBackend()..remote = true;
    await tester.pumpWidget(
      RepaintBoundary(
        key: const ValueKey('android-share-preview'),
        child: MediaQuery(
          data: const MediaQueryData(
            size: Size(360, 780),
            disableAnimations: true,
          ),
          child: WispnetApp(backend: remote),
        ),
      ),
    );
    await tester.pumpAndSettle();
    await fixture.capture(tester, 'unified-remote-folders');
    expect(tester.takeException(), isNull);
    await tester.pumpWidget(const SizedBox());
  });
}
