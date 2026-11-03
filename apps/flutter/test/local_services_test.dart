import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/service_settings.dart';
import 'package:wispnet/ui.dart';

import 'android_file_shares_test.dart' as fixture;
import 'folder_services_test.dart' as folders;

class ServiceListBackend extends folders.FolderBackend {
  final ordinary = <Map<String, dynamic>>[
    {
      'id': 'shell',
      'kind': 'shell',
      'backend': '127.0.0.1:0',
      'incarnation': '4' * 64,
      'allow': [fixture.laptop],
      'enabled': true,
    },
    {
      'id': 'research',
      'kind': 'http',
      'backend': '127.0.0.1:8080',
      'incarnation': '5' * 64,
      'allow': [fixture.other],
      'enabled': true,
    },
    {
      'id': 'database',
      'kind': 'tcp',
      'backend': '127.0.0.1:5432',
      'incarnation': '6' * 64,
      'allow': <String>[],
      'enabled': false,
    },
  ];

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'ownerStatus') {
      final response = await super.invoke(method, arguments);
      (response['host']['services'] as List).insertAll(0, ordinary);
      return response;
    }
    if (method == 'setServiceEnabled' &&
        ordinary.any((service) => service['id'] == arguments['service'])) {
      calls.add((method, Map.of(arguments)));
      ordinary.singleWhere(
        (service) => service['id'] == arguments['service'],
      )['enabled'] = arguments['enabled'];
      revision++;
      return {};
    }
    return super.invoke(method, arguments);
  }
}

Future<void> owner(WidgetTester tester, ServiceListBackend backend) =>
    fixture.mount(
      tester,
      ServiceSettings(backend: backend, servicesOnly: true, onChanged: () {}),
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

  testWidgets(
    'Local services lists Shell, HTTP, TCP and folders as uniform existing rows',
    (tester) async {
      final backend = ServiceListBackend();
      backend.ordinary.add(backend.ordinary.removeAt(0));
      await owner(tester, backend);
      expect(
        tester
            .widgetList<WispServiceItem>(find.byType(WispServiceItem))
            .first
            .title,
        'shell',
      );
      for (final id in [
        'shell',
        'research',
        'database',
        'training-data',
        'predictions',
        'checkpoints',
        'datasets',
      ]) {
        final row = find.byKey(ValueKey('local-service-$id'));
        expect(row, findsOneWidget);
        expect(
          find.descendant(of: row, matching: find.byType(WispToggle)),
          findsOneWidget,
        );
        expect(
          find.descendant(of: row, matching: find.byType(WispSettingsButton)),
          findsOneWidget,
        );
        expect(
          find.descendant(of: row, matching: find.byType(WispBadge)),
          findsOneWidget,
        );
      }
      expect(find.byKey(const ValueKey('local-service-files')), findsNothing);
      expect(find.text('Pair this device'), findsNothing);
      expect(find.text('Shell access'), findsNothing);
      expect(find.byType(WispCreateButton), findsOneWidget);
      expect(find.text('Add folder'), findsNothing);
      final lastRow = find.byKey(const ValueKey('local-service-datasets'));
      final create = find.widgetWithText(WispCreateButton, 'Share a service');
      await tester.ensureVisible(create);
      expect(
        tester.getRect(create).top,
        greaterThan(tester.getRect(lastRow).bottom),
      );
      expect(
        find.descendant(of: create, matching: find.byType(OutlinedButton)),
        findsOneWidget,
      );
      expect(
        find.descendant(of: create, matching: find.byIcon(Icons.add)),
        findsOneWidget,
      );
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'each service gear opens its existing settings without mutating grants',
    (tester) async {
      final backend = ServiceListBackend();
      await owner(tester, backend);
      await tester.tap(find.byTooltip('Shell access'));
      await tester.pumpAndSettle();
      expect(find.text('Shell access'), findsOneWidget);
      expect(find.widgetWithText(TextField, 'Port'), findsNothing);
      expect(find.text('Save'), findsOneWidget);
      await tester.tap(find.byTooltip('Close'));
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('Settings for research'));
      await tester.pumpAndSettle();
      expect(find.widgetWithText(TextField, 'Port'), findsOneWidget);
      expect(
        tester
            .widget<TextField>(find.widgetWithText(TextField, 'Service ID'))
            .controller!
            .text,
        'research',
      );
      await tester.tap(find.byTooltip('Close'));
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('Settings for database'));
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<TextField>(find.widgetWithText(TextField, 'Service ID'))
            .controller!
            .text,
        'database',
      );
      expect(backend.calls.where((call) => call.$1 == 'ownerRequest'), isEmpty);
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'ordinary service toggles target only the selected incarnation and keep grants',
    (tester) async {
      final backend = ServiceListBackend();
      await owner(tester, backend);
      await tester.tap(
        find.byKey(const ValueKey('local-service-toggle-shell')),
      );
      await tester.pumpAndSettle();
      await tester.tap(
        find.byKey(const ValueKey('local-service-toggle-database')),
      );
      await tester.pumpAndSettle();
      final toggles = backend.calls
          .where((call) => call.$1 == 'setServiceEnabled')
          .map((call) => call.$2)
          .toList();
      expect(toggles, [
        {'service': 'shell', 'incarnation': '4' * 64, 'enabled': false},
        {'service': 'database', 'incarnation': '6' * 64, 'enabled': true},
      ]);
      expect(backend.ordinary[0]['allow'], [fixture.laptop]);
      expect(backend.ordinary[1]['enabled'], isTrue);
      expect(backend.ordinary[1]['allow'], [fixture.other]);
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'revised Local services fits desktop and phone with creation below the rows',
    (tester) async {
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      tester.view.devicePixelRatio = 1;
      for (final size in [const Size(1000, 800), const Size(360, 780)]) {
        tester.view.physicalSize = size;
        final backend = ServiceListBackend();
        backend.inboxes.removeLast();
        backend.shares.clear();
        await fixture.mount(
          tester,
          Builder(
            builder: (context) => TextButton(
              onPressed: () => showWispSheet<void>(
                context,
                title: 'Local services',
                child: ServiceSettings(
                  backend: backend,
                  servicesOnly: true,
                  onChanged: () {},
                ),
              ),
              child: const Text('Open Local services'),
            ),
          ),
        );
        await tester.tap(find.text('Open Local services'));
        await tester.pumpAndSettle();
        await fixture.capture(
          tester,
          size.width > 500
              ? 'local-services-uniform-desktop'
              : 'local-services-uniform-phone',
        );
        expect(tester.takeException(), isNull);
        expect(find.text('Pair this device'), findsNothing);
        await tester.pumpWidget(const SizedBox());
      }
    },
  );
  testWidgets('creation tabs retain separate drafts and folder permissions', (
    tester,
  ) async {
    final backend = ServiceListBackend();
    await owner(tester, backend);
    final create = find.byKey(const ValueKey('share-service'));
    await tester.ensureVisible(create);
    await tester.tap(create);
    await tester.pumpAndSettle();
    expect(find.text('Add folder'), findsNothing);
    expect(find.widgetWithText(Tab, 'Receive files'), findsOneWidget);
    expect(find.widgetWithText(Tab, 'Share files'), findsOneWidget);
    expect(find.widgetWithText(Tab, 'Custom'), findsOneWidget);
    await tester.enterText(
      find.byKey(const ValueKey('share-name')),
      'incoming-models',
    );
    backend.pickedDesktopFolder = '/srv/incoming-models';
    await tester.tap(find.text('Choose folder'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('My laptop').last);
    await tester.tap(find.text('My laptop').last);
    await tester.ensureVisible(find.widgetWithText(Tab, 'Share files'));
    await tester.tap(find.widgetWithText(Tab, 'Share files'));
    await tester.pumpAndSettle();
    expect(
      tester
          .widget<TextField>(find.byKey(const ValueKey('share-name')))
          .controller!
          .text,
      isEmpty,
    );
    expect(
      tester
          .widget<WispToggleListTile>(
            find.byKey(ValueKey('share-allow-${fixture.laptop}')),
          )
          .value,
      isFalse,
    );
    await tester.enterText(
      find.byKey(const ValueKey('share-name')),
      'model-results',
    );
    backend.pickedDesktopFolder = '/srv/model-results';
    await tester.tap(find.text('Choose folder'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('Other device').last);
    await tester.tap(find.text('Other device').last);
    await tester.ensureVisible(find.widgetWithText(Tab, 'Custom'));
    await tester.tap(find.widgetWithText(Tab, 'Custom'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(TextField, 'Port'), findsOneWidget);
    await tester.enterText(
      find.widgetWithText(TextField, 'Service ID'),
      'model-api',
    );
    await tester.ensureVisible(find.widgetWithText(Tab, 'Receive files'));
    await tester.tap(find.widgetWithText(Tab, 'Receive files'));
    await tester.pumpAndSettle();
    expect(
      tester
          .widget<TextField>(find.byKey(const ValueKey('share-name')))
          .controller!
          .text,
      'incoming-models',
    );
    expect(
      tester
          .widget<WispToggleListTile>(
            find.byKey(ValueKey('share-allow-${fixture.laptop}')),
          )
          .value,
      isTrue,
    );
    await tester.ensureVisible(find.widgetWithText(Tab, 'Share files'));
    await tester.tap(find.widgetWithText(Tab, 'Share files'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('Save'));
    await tester.tap(find.text('Save'));
    await tester.pumpAndSettle();
    final operation = backend.calls
        .where((call) => call.$1 == 'ownerRequest')
        .single
        .$2['payload']['operation'];
    expect(operation, {
      'op': 'file_share',
      'id': 'model-results',
      'root': '/srv/model-results',
      'allow': [fixture.other],
      'expected_revision': 3,
    });
    expect(tester.takeException(), isNull);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('service creation tabs fit desktop and narrow screens', (
    tester,
  ) async {
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    tester.view.devicePixelRatio = 1;
    for (final size in [const Size(1000, 850), const Size(360, 780)]) {
      tester.view.physicalSize = size;
      await owner(tester, ServiceListBackend());
      final create = find.byKey(const ValueKey('share-service'));
      await tester.ensureVisible(create);
      await tester.tap(create);
      await tester.pumpAndSettle();
      for (final (tab, name) in [
        ('Receive files', 'receive'),
        ('Share files', 'share'),
        ('Custom', 'custom'),
      ]) {
        await tester.ensureVisible(find.widgetWithText(Tab, tab));
        await tester.tap(find.widgetWithText(Tab, tab));
        await tester.pumpAndSettle();
        await fixture.capture(
          tester,
          'service-create-$name-${size.width > 500 ? 'desktop' : 'narrow'}',
        );
        expect(tester.takeException(), isNull);
      }
      await tester.pumpWidget(const SizedBox());
    }
  });
  testWidgets(
    'folder settings show usage before editing and retain drafts across refresh',
    (tester) async {
      for (final id in ['training-data', 'checkpoints']) {
        final backend = ServiceListBackend();
        await fixture.mount(
          tester,
          Builder(
            builder: (context) => TextButton(
              onPressed: () => showWispSheet<void>(
                context,
                title: id,
                child: ServiceSettings(
                  backend: backend,
                  servicesOnly: true,
                  serviceId: id,
                  onChanged: () {},
                ),
              ),
              child: const Text('Open configuration'),
            ),
          ),
        );
        await tester.tap(find.text('Open configuration'));
        await tester.pumpAndSettle();
        expect(find.text('Manage folder and access'), findsNothing);
        expect(find.byKey(const ValueKey('share-root')), findsNothing);
        expect(find.text('Remove service'), findsOneWidget);
        expect(find.text('Save'), findsNothing);
        await tester.tap(find.text('Edit'));
        await tester.pumpAndSettle();
        expect(find.text('Remove folder'), findsOneWidget);
        backend.pickedDesktopFolder = '/srv/updated-folder';
        await tester.tap(find.text('Choose folder'));
        await tester.pumpAndSettle();
        await tester.enterText(
          find.byKey(const ValueKey('share-name')),
          '$id-renamed',
        );
        await tester.pump(const Duration(seconds: 4));
        await tester.pumpAndSettle();
        expect(find.text('/srv/updated-folder'), findsOneWidget);
        expect(
          tester
              .widget<TextField>(find.byKey(const ValueKey('share-name')))
              .controller!
              .text,
          '$id-renamed',
        );
        await tester.ensureVisible(find.text('Save'));
        await tester.tap(find.text('Save'));
        await tester.pumpAndSettle();
        final saved = backend.calls
            .where((call) => call.$1 == 'ownerRequest')
            .single
            .$2['payload']['operation'];
        expect(saved['op'], 'edit_folder');
        expect(saved['new_id'], '$id-renamed');
        expect(saved['id'], id);
        expect(saved['root'], '/srv/updated-folder');
        expect(saved['allow'], [fixture.laptop]);
        expect(tester.takeException(), isNull);
        await tester.pumpWidget(const SizedBox());
      }
    },
  );
  for (final id in ['predictions', 'datasets']) {
    testWidgets('desktop folder editor removal confirms only $id', (
      tester,
    ) async {
      final backend = ServiceListBackend();
      await owner(tester, backend);
      final gear = find.byTooltip('Folders and access for $id');
      await tester.ensureVisible(gear);
      await tester.tap(gear);
      await tester.pumpAndSettle();

      Future<void> openRemoval() async {
        final remove = find.widgetWithText(OutlinedButton, 'Remove folder');
        await tester.ensureVisible(remove);
        await tester.tap(remove);
        await tester.pumpAndSettle();
        expect(find.text('Remove folder?'), findsOneWidget);
        expect(find.textContaining('“$id”'), findsOneWidget);
      }

      await openRemoval();
      await tester.ensureVisible(find.text('Keep folder'));
      await tester.tap(find.text('Keep folder'));
      await tester.pumpAndSettle();
      expect(backend.calls.where((call) => call.$1 == 'ownerRequest'), isEmpty);
      expect(find.byKey(const ValueKey('share-name')), findsOneWidget);
      await openRemoval();
      final confirm = find.widgetWithText(FilledButton, 'Remove folder');
      await tester.ensureVisible(confirm);
      await tester.tap(confirm);
      await tester.pumpAndSettle();
      expect(
        backend.calls
            .where((call) => call.$1 == 'ownerRequest')
            .single
            .$2['payload']['operation'],
        {'op': 'remove', 'id': id, 'expected_revision': 3},
      );
      expect(find.byKey(const ValueKey('share-name')), findsNothing);
      expect(
        [
          ...backend.inboxes,
          ...backend.shares,
        ].where((folder) => folder['id'] == id),
        isEmpty,
      );
      expect(backend.inboxes.first['id'], 'training-data');
      expect(backend.inboxes.first['allow'], [fixture.laptop]);
      expect(backend.shares.first['id'], 'checkpoints');
      expect(backend.shares.first['allow'], [fixture.laptop]);
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
    });
  }
  testWidgets('folder picker suggests a name and preserves a custom name', (
    tester,
  ) async {
    final backend = ServiceListBackend();
    await owner(tester, backend);
    final create = find.byKey(const ValueKey('share-service'));
    await tester.ensureVisible(create);
    await tester.tap(create);
    await tester.pumpAndSettle();
    expect(find.widgetWithText(TextField, 'Folder path'), findsNothing);
    final name = find.byKey(const ValueKey('share-name'));
    for (final (path, expected) in [
      ('/srv/Training Checkpoints', 'training-checkpoints'),
      ('/srv/results', 'results'),
    ]) {
      backend.pickedDesktopFolder = path;
      await tester.ensureVisible(find.text('Choose folder'));
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      expect(tester.widget<TextField>(name).controller!.text, expected);
    }
    await tester.enterText(name, 'my-inbox');
    backend.pickedDesktopFolder = '/srv/new-folder';
    await tester.tap(find.text('Choose folder'));
    await tester.pumpAndSettle();
    expect(tester.widget<TextField>(name).controller!.text, 'my-inbox');
    expect(find.text('/srv/new-folder'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });
}
